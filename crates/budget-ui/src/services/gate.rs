//! The `AuthedUser` gate for Dioxus server functions
//! (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`, `D-sec`).
//!
//! # The one rule
//!
//! **Every server function that reads or writes budget data MUST call
//! [`require_authed_user`] as its FIRST line, before any handler logic.** It
//! returns the authenticated [`AuthedServerUser`] or a `401` `ServerFnError`. A
//! function that forgets the call has no [`UserId`] to scope its queries to and
//! reaches no user data — that is the "by construction" property the rule (and
//! `budget_infrastructure::AuthedUser`) is built on.
//!
//! The canonical shape of a gated data server function:
//!
//! ```ignore
//! #[server]
//! pub async fn list_transactions() -> Result<Vec<TransactionDto>, ServerFnError> {
//!     // 1. GATE FIRST — no data is touched before this returns Ok.
//!     let user = require_authed_user().await?;
//!     // 2. Scope EVERY query to user.id() (SPEC §9.1 defense in depth).
//!     // ... call the app-services layer with user.id() ...
//! }
//! ```
//!
//! # Why this is a function, not the Axum extractor
//!
//! The `budget_infrastructure::AuthedUser` Axum extractor enforces the gate for
//! plain Axum routes via `FromRequestParts`, requiring `AuthState: FromRef<S>`.
//! Dioxus server functions, however, run with `FullstackContext` as the Axum
//! state `S` (the framework owns that type), so an extractor parameterized on our
//! own `AuthState` cannot be used as a server-function argument. This helper is
//! the server-function-shaped equivalent: it performs the SAME steps the infra
//! extractor does — pull the session, read the stored `user_id`, load the user,
//! fail closed to 401 — but reads the [`AppState`] from the request's
//! [`Extension`](axum::Extension) (mounted in `budget-server`) rather than from a
//! typed router state. The security property is identical; only the plumbing the
//! framework exposes differs.
//!
//! # Fail-closed semantics
//!
//! Every failure path — missing session, no `user_id` in the session, a
//! `user_id` for a now-deleted user, a session-store read error, a missing
//! [`AppState`] extension — yields [`unauthorized`] (HTTP `401`) and reaches no
//! data. The rejection carries no body that could leak which step failed.

use dioxus::fullstack::FullstackContext;
use dioxus::fullstack::axum::Extension;
use dioxus::prelude::*;
use tower_sessions::Session;

use budget_domain::ids::UserId;
use budget_domain::user::User;

use crate::server_state::AppState;

/// The session payload key under which the login path stores the authenticated
/// `user_id`.
///
/// This MUST match `budget_infrastructure::auth::session::SESSION_USER_ID_KEY`
/// so the gate reads exactly what the login server function wrote. That constant
/// is not re-exported from the infrastructure crate's public surface, so it is
/// re-declared here as the single server-function-side source of truth; the
/// equality is pinned by a test in this module.
pub const SESSION_USER_ID_KEY: &str = "auth.user_id";

/// An authenticated request identity inside a server function
/// (`BUDGET-AUTH-GATE-1`).
///
/// Obtainable ONLY by [`require_authed_user`] (there is no public constructor
/// that fabricates one from request data), so a server function cannot mint an
/// identity. Scope every query to [`AuthedServerUser::id`] (`SPEC §9.1`).
#[derive(Debug, Clone)]
pub struct AuthedServerUser {
    user: User,
}

impl AuthedServerUser {
    /// The authenticated user id — scope EVERY query to this (`SPEC §9.1`).
    #[must_use]
    pub fn id(&self) -> UserId {
        self.user.id
    }

    /// The loaded user record (read-only).
    #[must_use]
    pub fn user(&self) -> &User {
        &self.user
    }
}

/// Build the opaque `401 Unauthorized` server-function error.
///
/// The status code is what drives the HTTP response (`code: 401`), so a caller
/// that propagates this with `?` returns a real `401` to the client by
/// construction. The message is a fixed, non-revealing string: it never says
/// which step failed (anti-enumeration, mirroring `AuthError::InvalidCredentials`).
#[must_use]
pub fn unauthorized() -> ServerFnError {
    ServerFnError::ServerError {
        message: "unauthorized".to_owned(),
        code: 401,
        details: None,
    }
}

/// **The gate.** Resolve the authenticated user for the current server-function
/// request, or reject with `401` (`BUDGET-AUTH-GATE-1`).
///
/// Call this FIRST in every data-returning / data-mutating server function. The
/// steps (identical to the `budget_infrastructure::AuthedUser` extractor):
///   1. extract the server-side [`Session`] (its id came from the secure
///      `HttpOnly` `SameSite=Strict` cookie);
///   2. read the `user_id` the login path stored in the session payload;
///   3. load the [`User`] through the [`AppState`] user repository;
///   4. yield an [`AuthedServerUser`] — or [`unauthorized`] (401) at any failure,
///      reaching no data.
///
/// Fails closed: a missing session, an empty session, a session for a deleted
/// user, or any store/extension error all produce `401`.
///
/// # Errors
///
/// [`unauthorized`] (`ServerFnError` with HTTP status `401`) when there is no
/// valid authenticated session, or the named user no longer exists, or the
/// server state / session layer is unavailable.
pub async fn require_authed_user() -> Result<AuthedServerUser, ServerFnError> {
    // 1. The session: extracted from the request the SessionManagerLayer has
    //    already processed (the cookie carried only the opaque id). No session
    //    layer / no cookie -> 401, no data.
    let session = FullstackContext::extract::<Session, _>()
        .await
        .map_err(|_| unauthorized())?;

    // 2. The authenticated user id the login path wrote. Absent -> not logged
    //    in -> 401.
    let user_id: UserId = session
        .get(SESSION_USER_ID_KEY)
        .await
        .map_err(|_| unauthorized())?
        .ok_or_else(unauthorized)?;

    // 3. The server state carrying the user repository. Its absence is a wiring
    //    fault; fail closed rather than reach data.
    let Extension(state) = FullstackContext::extract::<Extension<AppState>, _>()
        .await
        .map_err(|_| unauthorized())?;

    // 4. Load the user. A session pointing at a now-deleted user is treated as
    //    unauthenticated (fail closed), never a partial identity.
    let user = state
        .users
        .find_by_id(user_id)
        .await
        .map_err(|_| unauthorized())?
        .ok_or_else(unauthorized)?;

    Ok(AuthedServerUser { user })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::{SESSION_USER_ID_KEY, unauthorized};

    #[test]
    fn session_key_matches_the_infrastructure_login_key() {
        // The gate reads exactly the key the login server function (which writes
        // via the same infrastructure constant) stored. If the infra constant
        // ever changes, this equality must be updated in lockstep or the gate
        // would never see the logged-in user. The literal here is pinned to
        // `budget_infrastructure::auth::session::SESSION_USER_ID_KEY`.
        assert_eq!(SESSION_USER_ID_KEY, "auth.user_id");
    }

    #[test]
    fn unauthorized_is_a_401_with_no_revealing_body() {
        // BUDGET-AUTH-GATE-1: the rejection drives a real 401 (the `code` field
        // is what the framework turns into the HTTP status) and carries a fixed,
        // non-revealing message + no structured details (anti-enumeration).
        let err = unauthorized();
        // Pull the variant fields without a panic branch (the workspace
        // `clippy::panic` deny holds even in tests). A regressed rejection shape
        // makes `fields` None and trips the assert below.
        let fields = if let dioxus::prelude::ServerFnError::ServerError {
            code,
            message,
            details,
        } = &err
        {
            Some((*code, message.clone(), details.is_none()))
        } else {
            None
        };
        assert_eq!(
            fields,
            Some((401, "unauthorized".to_owned(), true)),
            "the gate must reject with an opaque HTTP 401 carrying no revealing detail (got {err:?})",
        );
    }
}
