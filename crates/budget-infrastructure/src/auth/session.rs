//! Postgres-backed server-side session store + cookie policy
//! (`SPEC §9.1`, `BUDGET-AUTH-GATE-1`).
//!
//! Sessions are server-side: the cookie carries only an opaque session id; the
//! session payload (the authenticated `user_id`) lives in Postgres. This is what
//! lets sessions survive scale-to-zero cold starts (`SPEC §9.1`) — the Container
//! App can be evicted and resurrected and the session is still valid because its
//! state is in the database, not in process memory.
//!
//! The store is `tower-sessions-sqlx-store`'s `PostgresStore`, which **owns its
//! own table** (`tower_sessions.session`) and migrates it itself
//! ([`PostgresStore::migrate`]); that is why the app schema migration (m0002)
//! does not define a session table (`SPEC §5`: "the store manages its own
//! table").
//!
//! ## Cookie policy (`SPEC §9.1`)
//!
//! - `Secure` — only sent over HTTPS (Container Apps managed TLS; insecure
//!   ingress disabled).
//! - `HttpOnly` — never readable from JavaScript (XSS cannot exfiltrate it).
//! - `SameSite=Strict` — never sent on cross-site requests (CSRF defense).
//!
//! The cookie name is intentionally generic (`id`) so it does not advertise the
//! framework. Session creation on login, validation, rotation, and destruction
//! on logout are driven by the service layer
//! ([`crate::auth::AuthService`]) through the `tower_sessions::Session` handle the
//! [`SessionManagerLayer`] injects per request.

use std::sync::Arc;

use tower_sessions::cookie::SameSite;
use tower_sessions::cookie::time::Duration;
use tower_sessions::{Expiry, SessionManagerLayer};
use tower_sessions_sqlx_store::PostgresStore;

use budget_domain::auth::AuthError;
use budget_domain::ids::UserId;

/// The session payload key under which the authenticated user id is stored.
///
/// `AuthedUser` reads this key; only the login path writes it. Because the
/// payload lives server-side, this key never appears in the cookie.
pub const SESSION_USER_ID_KEY: &str = "auth.user_id";

/// The session cookie name. Deliberately generic (does not name the framework).
const COOKIE_NAME: &str = "id";

/// Session lifetime. A logged-in session is valid for this long on each refresh;
/// `tower-sessions` rotates the expiry on activity.
const SESSION_TTL_DAYS: i64 = 14;

/// Configuration for the session cookie policy (`SPEC §9.1`).
///
/// Defaults are the secure production policy. `secure` is exposed so a local
/// (HTTP) dev/test harness can run without HTTPS; it MUST be `true` in any
/// deployed environment.
#[derive(Debug, Clone)]
pub struct SessionLayerConfig {
    /// `Secure` cookie attribute (HTTPS-only). MUST be `true` in production.
    pub secure: bool,
}

impl Default for SessionLayerConfig {
    fn default() -> Self {
        Self { secure: true }
    }
}

/// Build the [`SessionManagerLayer`] over a Postgres store, applying the secure
/// cookie policy (`SPEC §9.1`).
///
/// The caller supplies the sqlx [`PgPool`](sqlx::PgPool) (obtained from the
/// `SeaORM` connection via `get_postgres_connection_pool`). This function runs the
/// store's own migration so its session table exists, then returns the layer to
/// mount on the router in the frontend phase.
///
/// # Errors
/// [`AuthError::SessionStore`] if the store's table migration fails.
pub async fn build_session_layer(
    pool: sqlx::PgPool,
    config: &SessionLayerConfig,
) -> Result<SessionManagerLayer<PostgresStore>, AuthError> {
    let store = PostgresStore::new(pool);
    // The store owns + migrates its own table (SPEC §5 / §9.1).
    store
        .migrate()
        .await
        .map_err(|e| AuthError::SessionStore(format!("session-store migrate: {e}")))?;

    let layer = SessionManagerLayer::new(store)
        .with_name(COOKIE_NAME)
        .with_secure(config.secure)
        .with_http_only(true)
        .with_same_site(SameSite::Strict)
        .with_path("/")
        .with_expiry(Expiry::OnInactivity(Duration::days(SESSION_TTL_DAYS)));
    Ok(layer)
}

/// The authenticated-user value persisted in the session payload.
///
/// Stored under [`SESSION_USER_ID_KEY`]. A newtype so the session read/write is
/// strongly typed (rather than threading a bare `Uuid`/string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionUser {
    /// The authenticated user id.
    pub user_id: UserId,
}

impl SessionUser {
    /// Wrap an authenticated [`UserId`] for storage in the session.
    #[must_use]
    pub fn new(user_id: UserId) -> Self {
        Self { user_id }
    }
}

/// Build the [`Arc`]'d Postgres store directly (without the layer), for callers
/// that wire the layer themselves but still want the store migrated.
///
/// # Errors
/// [`AuthError::SessionStore`] if the store's table migration fails.
pub async fn build_migrated_store(pool: sqlx::PgPool) -> Result<Arc<PostgresStore>, AuthError> {
    let store = PostgresStore::new(pool);
    store
        .migrate()
        .await
        .map_err(|e| AuthError::SessionStore(format!("session-store migrate: {e}")))?;
    Ok(Arc::new(store))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::{SESSION_USER_ID_KEY, SessionLayerConfig, SessionUser};
    use budget_domain::ids::UserId;

    #[test]
    fn default_cookie_policy_is_secure() {
        // The production default must be Secure (HTTPS-only). A test/dev harness
        // explicitly opts out; nothing else may.
        assert!(SessionLayerConfig::default().secure);
    }

    #[test]
    fn session_user_round_trips_through_serde() {
        let u = SessionUser::new(UserId::generate());
        let json = serde_json::to_string(&u).expect("serialize");
        let back: SessionUser = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(u, back);
    }

    #[test]
    fn session_user_id_key_is_not_advertised_in_cookie() {
        // The payload key lives server-side; it is a constant, never the cookie
        // name. Guard against accidentally naming the cookie after the payload.
        assert_ne!(SESSION_USER_ID_KEY, super::COOKIE_NAME);
    }
}
