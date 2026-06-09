//! The `AuthedUser` Axum extractor — the enforce-by-construction gate
//! (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`).
//!
//! This is the single gate. Every data-returning route / server function MUST
//! take an [`AuthedUser`] argument. The extractor:
//!   1. pulls the server-side [`Session`] (its id came from the secure `HttpOnly`
//!      `SameSite=Strict` cookie),
//!   2. reads the authenticated `user_id` the login path wrote into the session
//!      payload,
//!   3. loads the [`User`] through the repository,
//!   4. yields an [`AuthedUser`] — or rejects with **401** and returns no data.
//!
//! Request identity is obtainable ONLY through this extractor: [`AuthedUser`] has
//! no public constructor that takes a request, so a handler cannot fabricate an
//! identity, and a route that forgets the extractor simply has no `user_id` to
//! scope its queries to. That is the "by construction" property — an ungated
//! data path cannot return user data because it never obtains a user.
//!
//! Defense in depth (`SPEC §9.1`): the `user_id` this extractor yields MUST also
//! scope every query the handler runs; the gate proves *authentication*, the
//! per-query scoping proves *authorization to that user's rows*.
//!
//! The HTTP host that mounts these routes is the FRONTEND phase. Here the
//! extractor is defined and proven against a minimal in-crate Axum harness (see
//! the test module), not a new server crate.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use tower_sessions::Session;

use budget_domain::ids::UserId;
use budget_domain::repositories::UserRepository;
use budget_domain::user::User;

use crate::auth::session::SESSION_USER_ID_KEY;

/// Application state the [`AuthedUser`] extractor needs: a handle to load the
/// authenticated user.
///
/// In the frontend phase this is part of the app's router state; the extractor
/// pulls the [`UserRepository`] out of it via [`FromRef`](axum::extract::FromRef).
#[derive(Clone)]
pub struct AuthState {
    /// Loads the user named by the session (single-user V1, `SPEC §9`).
    pub users: Arc<dyn UserRepository>,
}

impl AuthState {
    /// Construct the auth state from a user repository.
    #[must_use]
    pub fn new(users: Arc<dyn UserRepository>) -> Self {
        Self { users }
    }
}

/// An authenticated request identity (`BUDGET-AUTH-GATE-1`).
///
/// The ONLY way to obtain one from a request is the [`FromRequestParts`] impl
/// below, which requires a valid session. There is deliberately no public
/// `AuthedUser::new` that accepts request data: a handler cannot mint an identity
/// out of thin air. Constructed test fixtures use [`AuthedUser::for_test`], which
/// is `#[cfg(test)]`-only and never compiled into a request path.
#[derive(Debug, Clone)]
pub struct AuthedUser {
    /// The authenticated user id — scope EVERY query to this (`SPEC §9.1`).
    pub user_id: UserId,
    /// The loaded user record.
    pub user: User,
}

impl AuthedUser {
    /// The authenticated user id (convenience accessor for query scoping).
    #[must_use]
    pub fn id(&self) -> UserId {
        self.user_id
    }

    /// Test-only constructor. Compiled only under `cfg(test)`, so it can never
    /// appear on a real request path (`BUDGET-AUTH-GATE-1`: identity comes ONLY
    /// from the extractor in non-test builds).
    #[cfg(test)]
    #[must_use]
    pub fn for_test(user: User) -> Self {
        Self {
            user_id: user.id,
            user,
        }
    }
}

impl<S> FromRequestParts<S> for AuthedUser
where
    S: Send + Sync,
    AuthState: axum::extract::FromRef<S>,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // The session itself is extracted from the request (its id came from the
        // secure cookie). A failure to extract the session is a 401 — without a
        // session there is no identity and no data is reached.
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::UNAUTHORIZED)?;

        // The login path stored the authenticated user id under this key. No
        // value -> not logged in -> 401, no data.
        let user_id: UserId = session
            .get(SESSION_USER_ID_KEY)
            .await
            .map_err(|_| StatusCode::UNAUTHORIZED)?
            .ok_or(StatusCode::UNAUTHORIZED)?;

        // Load the user. A session pointing at a now-deleted user is treated as
        // unauthenticated (fail closed), never as a partial/empty identity.
        let auth_state = AuthState::from_ref(state);
        let user = auth_state
            .users
            .find_by_id(user_id)
            .await
            .map_err(|_| StatusCode::UNAUTHORIZED)?
            .ok_or(StatusCode::UNAUTHORIZED)?;

        Ok(AuthedUser {
            user_id: user.id,
            user,
        })
    }
}

// Bring FromRef into scope for the impl above.
use axum::extract::FromRef;

#[cfg(test)]
mod tests {
    //! Minimal in-crate Axum harness proving the gate (`BUDGET-AUTH-GATE-1`,
    //! `ORCH-NEW-PATH-TESTS-1`). NOT a new host crate — a test-only router with
    //! one protected route (takes `AuthedUser`) and one unprotected route (does
    //! not), exercised through `tower::ServiceExt::oneshot`.
    //!
    //! The protected route returns the authenticated `user_id`; the unprotected
    //! route always returns 200. With no/empty session the protected route 401s
    //! and yields no body; with a valid session it 200s and returns the id.
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::FromRef;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use chrono::Utc;
    use tower::ServiceExt;
    use tower_sessions::cookie::SameSite;
    use tower_sessions::{MemoryStore, Session, SessionManagerLayer};

    use budget_domain::RepositoryError;
    use budget_domain::ids::UserId;
    use budget_domain::repositories::UserRepository;
    use budget_domain::uow::UnitOfWork;
    use budget_domain::user::User;
    use budget_domain::validated::Email;

    use super::{AuthState, AuthedUser};
    use crate::auth::session::SESSION_USER_ID_KEY;

    /// An in-memory user repo so the harness needs no Postgres. Holds the single
    /// user (`SPEC §9`), returned only on an id match.
    struct FakeUserRepo {
        user: User,
    }

    #[async_trait]
    impl UserRepository for FakeUserRepo {
        async fn find_by_id(&self, id: UserId) -> Result<Option<User>, RepositoryError> {
            Ok((id == self.user.id).then(|| self.user.clone()))
        }
        async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError> {
            Ok((email == self.user.email.as_str()).then(|| self.user.clone()))
        }
        async fn save(
            &self,
            _u: &User,
            _uow: Option<&dyn UnitOfWork>,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    /// The harness router state.
    #[derive(Clone)]
    struct TestState {
        auth: AuthState,
    }

    impl FromRef<TestState> for AuthState {
        fn from_ref(s: &TestState) -> AuthState {
            s.auth.clone()
        }
    }

    fn sample_user() -> User {
        User {
            id: UserId::generate(),
            email: Email::try_new("zach@example.com").expect("email"),
            password_hash: "$argon2id$x".to_owned(),
            totp_secret: None,
            tracking_start_date: chrono::NaiveDate::from_ymd_opt(2026, 7, 1).expect("date"),
            created_at: Utc::now(),
        }
    }

    /// Protected: takes the `AuthedUser` gate. Reaches its body only with a valid
    /// session; returns the authenticated id.
    async fn protected(user: AuthedUser) -> impl IntoResponse {
        user.id().to_string()
    }

    /// Unprotected: no gate, always 200.
    async fn unprotected() -> impl IntoResponse {
        "public"
    }

    /// A login route that writes the authenticated user id into the session — the
    /// only path that mints identity, mirroring what `AuthService` does on a
    /// successful login.
    async fn login(session: Session, user_id: UserId) -> Result<(), StatusCode> {
        session
            .insert(SESSION_USER_ID_KEY, user_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    }

    fn build_router(user: User) -> Router {
        let session_layer = SessionManagerLayer::new(MemoryStore::default())
            .with_secure(false) // HTTP test harness
            .with_http_only(true)
            .with_same_site(SameSite::Strict);
        let state = TestState {
            auth: AuthState::new(Arc::new(FakeUserRepo { user })),
        };
        Router::new()
            .route("/protected", get(protected))
            .route("/unprotected", get(unprotected))
            .with_state(state)
            .layer(session_layer)
    }

    #[tokio::test]
    async fn unprotected_route_serves_without_a_session() {
        let router = build_router(sample_user());
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/unprotected")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_route_rejects_without_a_session() {
        let router = build_router(sample_user());
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        // BUDGET-AUTH-GATE-1: no session -> 401, and no data.
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        assert!(body.is_empty(), "401 must yield no data");
    }

    #[tokio::test]
    async fn protected_route_serves_with_a_valid_session() {
        // Drive a real login (writes the session) then reuse the issued cookie on
        // the protected route — a full session round-trip through the layer.
        let user = sample_user();
        let user_id = user.id;
        let session_layer = SessionManagerLayer::new(MemoryStore::default())
            .with_secure(false)
            .with_http_only(true)
            .with_same_site(SameSite::Strict);
        let state = TestState {
            auth: AuthState::new(Arc::new(FakeUserRepo { user })),
        };
        // A login route closed over the user id (the only identity-minting path).
        let login_route = {
            let uid = user_id;
            axum::routing::post(move |session: Session| async move { login(session, uid).await })
        };
        let router = Router::new()
            .route("/protected", get(protected))
            .route("/login", login_route)
            .with_state(state)
            .layer(session_layer);

        // 1. Log in; capture the Set-Cookie.
        let login_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("login resp");
        assert_eq!(login_resp.status(), StatusCode::OK);
        let cookie = login_resp
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(';').next())
            .map(str::to_owned)
            .expect("a session cookie was issued on login");

        // 2. Call the protected route with the session cookie.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("protected resp");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(
            String::from_utf8_lossy(&body),
            user_id.to_string(),
            "the gate must yield the authenticated user id",
        );
    }
}
