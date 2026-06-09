//! Authentication server functions: login, logout, and the current-user probe
//! (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`, `D1`, `RUST-DIOXUS-9`).
//!
//! These are the only server functions that establish or tear down a session.
//! [`login`] verifies password + mandatory TOTP through the server-side
//! [`AuthService`](budget_app_services::AuthService) and, on success, writes the
//! authenticated `user_id` into the server-side session (the cookie the
//! `SessionManagerLayer` issues carries only the opaque session id). [`logout`]
//! destroys the session. [`current_user`] is a minimal GATED data server
//! function demonstrating the [`require_authed_user`](super::gate::require_authed_user)
//! pattern every future data path follows.
//!
//! There is no signup server function: the single user is provisioned out of
//! band (the `provision-user` CLI), `SPEC §9`.
//!
//! The login/logout/session-write mechanics are server-only; the `#[server]`
//! macro generates the client-side call so the login view can invoke them over
//! the wire.

use dioxus::prelude::*;

/// A redacted login request. The fields are sent to the server; they are never
/// stored client-side beyond the in-flight form state, and the server never
/// echoes them back.
///
/// Validation of the email shape happens in the domain
/// ([`Email::try_new`](budget_domain::validated::Email)) on the server side; the
/// UI does not re-implement it (`RUST-DIOXUS-13`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoginRequest {
    /// The user's email (the login identifier, `SPEC §9`).
    pub email: String,
    /// The plaintext password (verified with Argon2 server-side; never logged).
    pub password: String,
    /// The 6-digit TOTP code (mandatory second factor, `SPEC §9.1`).
    pub totp_code: String,
}

/// Establish a session with password + mandatory TOTP (`SPEC §9.1`,
/// `BUDGET-AUTH-GATE-1`).
///
/// On success the authenticated `user_id` is written into the server-side
/// session; the client receives the secure `HttpOnly` `SameSite=Strict` session
/// cookie via the response. Subsequent gated server functions then resolve that
/// session through [`require_authed_user`](super::gate::require_authed_user).
///
/// Anti-enumeration: every credential failure (unknown email, wrong password,
/// missing/wrong TOTP) returns the SAME opaque `401`, so the boundary reveals
/// nothing about which factor failed or whether the account exists — mirroring
/// [`AuthError::InvalidCredentials`](budget_domain::auth::AuthError).
///
/// # Errors
///
/// - `401` (opaque) on any authentication failure.
/// - `500` only on a genuine server fault (session-store write, a corrupt stored
///   hash/secret) — never to distinguish a wrong credential.
#[server]
pub async fn login(request: LoginRequest) -> Result<(), ServerFnError> {
    use dioxus::fullstack::FullstackContext;
    use dioxus::fullstack::axum::Extension;
    use tower_sessions::Session;

    use budget_domain::auth::AuthError;

    use crate::server_state::AppState;
    use crate::services::gate::{SESSION_USER_ID_KEY, unauthorized};

    // The server state (the AuthService) and the per-request session handle.
    let Extension(state) = FullstackContext::extract::<Extension<AppState>, _>()
        .await
        .map_err(|_| ServerFnError::new("server state unavailable"))?;
    let session = FullstackContext::extract::<Session, _>()
        .await
        .map_err(|_| ServerFnError::new("session unavailable"))?;

    // Verify BOTH factors (password Argon2 + mandatory TOTP). Any credential
    // failure collapses to the opaque 401; only a genuine engine/persistence
    // fault surfaces as 500.
    let user_id = match state
        .auth
        .verify_login(&request.email, &request.password, &request.totp_code)
        .await
    {
        Ok(id) => id,
        Err(AuthError::InvalidCredentials | AuthError::SecondFactorRequired) => {
            return Err(unauthorized());
        }
        // Operational faults (corrupt stored hash/secret, repository error):
        // 500, no detail leaked. The credential outcome is NOT revealed.
        Err(_) => return Err(ServerFnError::new("login failed")),
    };

    // Establish the session: write the authenticated id, then rotate the session
    // id to prevent fixation (a pre-login id cannot be reused post-auth).
    session
        .insert(SESSION_USER_ID_KEY, user_id)
        .await
        .map_err(|_| ServerFnError::new("session write failed"))?;
    session
        .cycle_id()
        .await
        .map_err(|_| ServerFnError::new("session rotation failed"))?;

    Ok(())
}

/// Destroy the current session (`BUDGET-AUTH-GATE-1`).
///
/// Idempotent: logging out without a session is a no-op success. After this the
/// session cookie no longer authenticates any request (the server-side session
/// is gone), so the gate 401s every subsequent gated call.
///
/// # Errors
///
/// `500` only if the session store fails to delete the session.
#[server]
pub async fn logout() -> Result<(), ServerFnError> {
    use dioxus::fullstack::FullstackContext;
    use tower_sessions::Session;

    let session = FullstackContext::extract::<Session, _>()
        .await
        .map_err(|_| ServerFnError::new("session unavailable"))?;
    session
        .delete()
        .await
        .map_err(|_| ServerFnError::new("logout failed"))?;
    Ok(())
}

/// The authenticated user's email — a minimal GATED data server function
/// (`BUDGET-AUTH-GATE-1`).
///
/// This is the reference example for every future data path: it calls
/// [`require_authed_user`](super::gate::require_authed_user) FIRST and returns
/// only data scoped to that authenticated user. Called unauthenticated, it 401s
/// and reaches no data — proving the gate is wired, not just defined.
///
/// # Errors
///
/// `401` (via the gate) when there is no valid authenticated session.
#[server]
pub async fn current_user() -> Result<String, ServerFnError> {
    use crate::services::gate::require_authed_user;

    // GATE FIRST — no data is read before this returns Ok.
    let user = require_authed_user().await?;
    // Data scoped to the authenticated user (SPEC §9.1 defense in depth).
    Ok(user.user().email.as_str().to_owned())
}
