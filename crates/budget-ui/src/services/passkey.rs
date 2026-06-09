//! Passkey / `WebAuthn` server functions (`SPEC §9.1`, `BUDGET-AUTH-GATE-1`,
//! `RUST-DIOXUS-9`).
//!
//! Four server functions, two ceremonies, each a start/finish pair. The browser's
//! `WebAuthn` API (`navigator.credentials.create` / `.get`) runs the actual
//! ceremony on the client (see [`crate::components::webauthn`]); these functions
//! own the server half: issue the challenge, stash the in-progress ceremony state
//! in the SERVER-SIDE session (never the client), and verify the browser's
//! response through the infrastructure [`WebauthnService`](budget_infrastructure::WebauthnService).
//!
//! ## Registration (add a passkey to the account) — GATED
//!
//! [`start_passkey_registration`] requires an authenticated session (you must be
//! logged in with password + TOTP to enroll a new authenticator), so it goes
//! through the [`require_authed_user`](super::gate::require_authed_user) gate.
//! [`finish_passkey_registration`] verifies the browser's new credential and
//! persists it against the authenticated user.
//!
//! ## Authentication (sign in with a passkey) — UNGATED login path
//!
//! [`start_passkey_authentication`] and [`finish_passkey_authentication`] are a
//! LOGIN path, so they are deliberately ungated (a logged-out user runs them to
//! sign in). The user supplies their email — the same identifier the password
//! login uses — so the server can resolve which credentials to challenge
//! (non-discoverable `WebAuthn`; this avoids needing resident keys or a
//! "find-the-sole-user" repository method, keeping the domain trait surface
//! unchanged). On a verified assertion the finish function writes the
//! authenticated `user_id` into the session exactly as the password login does.
//!
//! ## Anti-enumeration
//!
//! The authentication start path returns the SAME opaque `401` whether the email
//! is unknown or simply has no registered passkeys, mirroring the password login's
//! `AuthError::InvalidCredentials` polarity — the boundary reveals nothing about
//! which accounts exist or which have passkeys.
//!
//! ## Ceremony-state stash
//!
//! The in-progress `PasskeyRegistration` / `PasskeyAuthentication` state is held
//! in the server-side session between start and finish (the
//! `danger-allow-state-serialisation` feature serializes it; the "danger" is only
//! about storing it CLIENT-side, which we never do — the Postgres-backed session
//! store is the documented safe place). The challenge that travels to the browser
//! carries only public ceremony material.

use dioxus::prelude::*;

/// The session key under which an in-progress passkey REGISTRATION ceremony state
/// is stashed between the start and finish server functions.
#[cfg(feature = "server")]
pub(crate) const SESSION_REG_STATE_KEY: &str = "auth.passkey_reg_state";

/// The session key under which an in-progress passkey AUTHENTICATION ceremony
/// state (plus the resolved candidate `user_id`) is stashed between start and
/// finish.
#[cfg(feature = "server")]
pub(crate) const SESSION_AUTH_STATE_KEY: &str = "auth.passkey_auth_state";

/// Begin registering a new passkey for the signed-in user (GATED, `SPEC §9.1`).
///
/// Returns the `navigator.credentials.create` options as JSON for the browser to
/// run the ceremony with. The opaque ceremony state is stashed in the session;
/// the client never sees it.
///
/// # Errors
/// - `401` (via the gate) when there is no authenticated session.
/// - `500` on a webauthn-engine or session-store fault (no detail leaked).
#[server]
pub async fn start_passkey_registration() -> Result<serde_json::Value, ServerFnError> {
    use dioxus::fullstack::FullstackContext;
    use dioxus::fullstack::axum::Extension;
    use tower_sessions::Session;

    use crate::server_state::AppState;
    use crate::services::gate::require_authed_user;

    // GATE FIRST: only an authenticated user may add a passkey to the account.
    let authed = require_authed_user().await?;
    let user = authed.user();

    let Extension(state) = FullstackContext::extract::<Extension<AppState>, _>()
        .await
        .map_err(|_| ServerFnError::new("server state unavailable"))?;
    let session = FullstackContext::extract::<Session, _>()
        .await
        .map_err(|_| ServerFnError::new("session unavailable"))?;

    // Existing credentials are excluded so the same authenticator is not enrolled
    // twice (the engine builds the excludeCredentials list).
    let existing = state
        .auth
        .list_credentials(authed.id())
        .await
        .map_err(|_| ServerFnError::new("passkey registration failed"))?;

    let email = user.email.as_str();
    let (challenge, ceremony_state) = state
        .webauthn
        .start_registration_json(authed.id(), email, email, &existing)
        .map_err(|_| ServerFnError::new("passkey registration failed"))?;

    // Stash the opaque ceremony state server-side until the finish call.
    session
        .insert(SESSION_REG_STATE_KEY, ceremony_state)
        .await
        .map_err(|_| ServerFnError::new("session write failed"))?;

    Ok(challenge)
}

/// Finish registering a new passkey: verify the browser's credential and persist
/// it for the signed-in user (GATED, `SPEC §9.1`).
///
/// `credential` is the JSON the browser's `navigator.credentials.create` produced.
///
/// # Errors
/// - `401` (via the gate) when there is no authenticated session.
/// - `400` if there is no pending registration ceremony (mismatched start/finish).
/// - `500` on a verification or persistence fault (opaque).
#[server]
pub async fn finish_passkey_registration(
    credential: serde_json::Value,
) -> Result<(), ServerFnError> {
    use dioxus::fullstack::FullstackContext;
    use dioxus::fullstack::axum::Extension;
    use tower_sessions::Session;

    use budget_infrastructure::WebauthnService;

    use crate::server_state::AppState;
    use crate::services::gate::require_authed_user;

    let authed = require_authed_user().await?;

    let Extension(state) = FullstackContext::extract::<Extension<AppState>, _>()
        .await
        .map_err(|_| ServerFnError::new("server state unavailable"))?;
    let session = FullstackContext::extract::<Session, _>()
        .await
        .map_err(|_| ServerFnError::new("session unavailable"))?;

    // Pull (and clear) the stashed ceremony state. Its absence means there is no
    // ceremony in flight — reject as a bad request rather than verifying against
    // nothing.
    let ceremony_state: serde_json::Value = session
        .remove(SESSION_REG_STATE_KEY)
        .await
        .map_err(|_| ServerFnError::new("session read failed"))?
        .ok_or_else(|| ServerFnError::ServerError {
            message: "no pending registration".to_owned(),
            code: 400,
            details: None,
        })?;

    let registered = state
        .webauthn
        .finish_registration_json(&credential, &ceremony_state)
        .map_err(|_| ServerFnError::new("passkey registration failed"))?;

    // Persist the new credential against the authenticated user. The nickname is
    // left unset here; a "name this device" affordance is a later polish item.
    let domain_credential = WebauthnService::to_domain_credential(&registered, authed.id(), None);
    state
        .auth
        .save_credential(&domain_credential)
        .await
        .map_err(|_| ServerFnError::new("passkey registration failed"))?;

    Ok(())
}

/// Begin signing in with a passkey for the account identified by `email` (UNGATED
/// login path, `SPEC §9.1`).
///
/// Returns the `navigator.credentials.get` options as JSON. The opaque ceremony
/// state is stashed in the session for the finish call.
///
/// Anti-enumeration: an unknown email or an email with no registered passkeys both
/// return the same opaque `401`.
///
/// # Errors
/// - `401` (opaque) if the email is unknown or has no registered passkeys.
/// - `500` on a webauthn-engine or session-store fault.
#[server]
pub async fn start_passkey_authentication(
    email: String,
) -> Result<serde_json::Value, ServerFnError> {
    use dioxus::fullstack::FullstackContext;
    use dioxus::fullstack::axum::Extension;
    use tower_sessions::Session;

    use crate::server_state::AppState;
    use crate::services::gate::unauthorized;

    let Extension(state) = FullstackContext::extract::<Extension<AppState>, _>()
        .await
        .map_err(|_| ServerFnError::new("server state unavailable"))?;
    let session = FullstackContext::extract::<Session, _>()
        .await
        .map_err(|_| ServerFnError::new("session unavailable"))?;

    // Resolve the account by email (the same identifier as password login). An
    // unknown email collapses to the opaque 401 (anti-enumeration); a genuine
    // store fault is a 500.
    let user = state
        .users
        .find_by_email(&email)
        .await
        .map_err(|_| ServerFnError::new("authentication failed"))?
        .ok_or_else(unauthorized)?;

    let credentials = state
        .auth
        .list_credentials(user.id)
        .await
        .map_err(|_| ServerFnError::new("authentication failed"))?;

    // No registered passkeys -> the same opaque 401 (do not reveal that the
    // account exists but has not enrolled a passkey).
    let Ok((challenge, ceremony_state)) = state.webauthn.start_authentication_json(&credentials)
    else {
        return Err(unauthorized());
    };

    session
        .insert(SESSION_AUTH_STATE_KEY, ceremony_state)
        .await
        .map_err(|_| ServerFnError::new("session write failed"))?;

    Ok(challenge)
}

/// Finish signing in with a passkey: verify the browser's assertion and establish
/// the session (UNGATED login path, `SPEC §9.1`, `BUDGET-AUTH-GATE-1`).
///
/// `assertion` is the JSON the browser's `navigator.credentials.get` produced. On
/// a verified assertion the authenticated `user_id` is written into the session
/// (and the session id rotated) exactly as the password login does, so every
/// subsequent gated call resolves through the same gate.
///
/// # Errors
/// - `401` (opaque) if there is no pending ceremony, the assertion fails
///   verification, or the authenticator's signature counter regressed (possible
///   clone).
/// - `500` on a session-store fault.
#[server]
pub async fn finish_passkey_authentication(
    assertion: serde_json::Value,
) -> Result<(), ServerFnError> {
    use dioxus::fullstack::FullstackContext;
    use dioxus::fullstack::axum::Extension;
    use tower_sessions::Session;

    use budget_domain::auth::AuthError;

    use crate::server_state::AppState;
    use crate::services::gate::{SESSION_USER_ID_KEY, unauthorized};

    let Extension(state) = FullstackContext::extract::<Extension<AppState>, _>()
        .await
        .map_err(|_| ServerFnError::new("server state unavailable"))?;
    let session = FullstackContext::extract::<Session, _>()
        .await
        .map_err(|_| ServerFnError::new("session unavailable"))?;

    // Pull (and clear) the stashed ceremony state. Absence -> no ceremony in
    // flight -> opaque 401.
    let ceremony_state: serde_json::Value = session
        .remove(SESSION_AUTH_STATE_KEY)
        .await
        .map_err(|_| ServerFnError::new("session read failed"))?
        .ok_or_else(unauthorized)?;

    // Verify the assertion. Any verification failure is the opaque 401.
    let Ok(outcome) = state
        .webauthn
        .finish_authentication_json(&assertion, &ceremony_state)
    else {
        return Err(unauthorized());
    };

    // Resolve the credential -> user and persist the advanced signature counter
    // (clone detection). A counter regression is rejected as a possible clone.
    let user_id = match state
        .auth
        .complete_passkey_assertion(&outcome.credential_id, outcome.new_sign_count)
        .await
    {
        Ok(id) => id,
        Err(AuthError::InvalidCredentials) => return Err(unauthorized()),
        Err(_) => return Err(ServerFnError::new("authentication failed")),
    };

    // Establish the session: write the authenticated id, then rotate the session
    // id to prevent fixation — identical to the password login path.
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
