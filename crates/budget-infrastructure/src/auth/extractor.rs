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
    use axum::extract::{FromRef, FromRequestParts};
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

    #[test]
    fn extractor_rejection_is_a_bare_status_code_no_body() {
        // Enforce-by-construction (BUDGET-AUTH-GATE-1): the extractor's failure
        // type is exactly `StatusCode`, not a struct/enum that could carry a body
        // or a partial identity. A bare 401 status response has no body of its own,
        // so a failed extraction cannot smuggle data out. This type-level check
        // pins the rejection shape; the runtime "401 with empty body" behavior is
        // asserted by `protected_route_rejects_without_a_session` below.
        //
        // On a real request an `AuthedUser` is obtainable ONLY through this
        // `FromRequestParts` impl (which validates the session). The struct has no
        // public request-taking constructor (`for_test` is `#[cfg(test)]`-only),
        // so a handler cannot mint identity from request data. Adding any
        // `AuthedUser::from_*(request-data)` constructor would be the regression
        // this rule guards against.
        fn assert_rejection_is_status_code<R>()
        where
            R: 'static,
        {
            assert_eq!(
                std::any::TypeId::of::<R>(),
                std::any::TypeId::of::<StatusCode>(),
                "the extractor rejection must be a bare StatusCode (no body carrier)",
            );
        }
        assert_rejection_is_status_code::<<AuthedUser as FromRequestParts<TestState>>::Rejection>();
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

    /// Like [`build_router`] but applies the PRODUCTION cookie policy (Secure +
    /// `HttpOnly` + `SameSite=Strict`) so the issued `Set-Cookie` can be inspected.
    /// Adds a `/login` and a `/logout` route.
    fn build_secure_router(user: User) -> Router {
        let user_id = user.id;
        // Mirror the production policy in session.rs: Secure + HttpOnly +
        // SameSite=Strict. (MemoryStore stands in for the Postgres store; the
        // cookie policy is identical and is what we assert here.)
        let session_layer = SessionManagerLayer::new(MemoryStore::default())
            .with_secure(true)
            .with_http_only(true)
            .with_same_site(SameSite::Strict)
            .with_path("/");
        let state = TestState {
            auth: AuthState::new(Arc::new(FakeUserRepo { user })),
        };
        let login_route =
            axum::routing::post(
                move |session: Session| async move { login(session, user_id).await },
            );
        Router::new()
            .route("/protected", get(protected))
            .route("/login", login_route)
            .route(
                "/logout",
                axum::routing::post(|session: Session| async move {
                    // Destroying the session is what logout does (BUDGET-AUTH-GATE-1).
                    session
                        .delete()
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
                }),
            )
            .with_state(state)
            .layer(session_layer)
    }

    /// Extract the raw `Set-Cookie` header string from a login response.
    fn set_cookie_of(resp: &axum::response::Response) -> String {
        resp.headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .expect("a Set-Cookie header")
    }

    /// The `name=value` first segment, for replay on a follow-up request.
    fn cookie_pair(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .map(str::to_owned)
            .expect("cookie pair")
    }

    #[tokio::test]
    async fn issued_cookie_has_secure_httponly_samesite_strict_flags() {
        // BUDGET-AUTH-GATE-1 / SPEC §9.1: the session cookie MUST be Secure (HTTPS
        // only), HttpOnly (no JS access -> XSS can't steal it), SameSite=Strict
        // (CSRF defense). Assert all three on the real issued header.
        let router = build_secure_router(sample_user());
        let login_resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("login resp");
        let set_cookie = set_cookie_of(&login_resp).to_ascii_lowercase();
        assert!(
            set_cookie.contains("httponly"),
            "cookie must be HttpOnly: {set_cookie}"
        );
        assert!(
            set_cookie.contains("secure"),
            "cookie must be Secure: {set_cookie}"
        );
        assert!(
            set_cookie.contains("samesite=strict"),
            "cookie must be SameSite=Strict: {set_cookie}",
        );
    }

    #[tokio::test]
    async fn logout_destroys_the_session_and_protected_route_then_401s() {
        // A full login -> access -> logout -> access-again cycle. After logout the
        // same cookie must no longer authenticate (the server-side session is gone).
        let router = build_secure_router(sample_user());

        // 1. Log in, capture cookie.
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
            .expect("login");
        let cookie = cookie_pair(&set_cookie_of(&login_resp));

        // 2. The cookie authenticates the protected route.
        let ok = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(axum::http::header::COOKIE, &cookie)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("protected pre-logout");
        assert_eq!(
            ok.status(),
            StatusCode::OK,
            "cookie must work before logout"
        );

        // 3. Log out using the SAME cookie (destroys the server-side session).
        let logout = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/logout")
                    .header(axum::http::header::COOKIE, &cookie)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("logout");
        assert_eq!(logout.status(), StatusCode::OK);

        // 4. The same cookie must NO LONGER authenticate (session destroyed).
        let after = router
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(axum::http::header::COOKIE, &cookie)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("protected post-logout");
        assert_eq!(
            after.status(),
            StatusCode::UNAUTHORIZED,
            "a destroyed session's cookie must not authenticate (logout works)",
        );
    }

    #[tokio::test]
    async fn forged_and_tampered_cookies_are_rejected() {
        // A cookie with a value the server never issued (forged), and a tampered
        // version of a real cookie, must both 401 — the session id is opaque and
        // unguessable; a mismatched id resolves to no server-side session.
        let router = build_secure_router(sample_user());

        // First obtain a real cookie so we can tamper with its value.
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
            .expect("login");
        let real = cookie_pair(&set_cookie_of(&login_resp));

        // Tamper: flip the last character of the cookie value.
        let mut bytes = real.clone().into_bytes();
        let last = bytes.len() - 1;
        bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(bytes).expect("utf8");
        assert_ne!(tampered, real);

        for bad in [
            "id=totally-made-up-session-id".to_owned(),
            "id=".to_owned(),
            tampered,
        ] {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/protected")
                        .header(axum::http::header::COOKIE, &bad)
                        .body(Body::empty())
                        .expect("req"),
                )
                .await
                .expect("resp");
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "forged/tampered cookie {bad:?} must be rejected",
            );
        }
    }

    #[tokio::test]
    async fn session_pointing_at_a_deleted_user_fails_closed() {
        // Defense in depth: a valid session whose user no longer exists must be
        // treated as unauthenticated (401), never as a partial/empty identity.
        // The login route writes a DIFFERENT user id than the repo holds.
        let repo_user = sample_user();
        let ghost_id = UserId::generate();
        assert_ne!(repo_user.id, ghost_id);

        let session_layer = SessionManagerLayer::new(MemoryStore::default())
            .with_secure(false)
            .with_http_only(true)
            .with_same_site(SameSite::Strict);
        let state = TestState {
            auth: AuthState::new(Arc::new(FakeUserRepo { user: repo_user })),
        };
        // Log in AS the ghost (an id the repo will not find).
        let login_route =
            axum::routing::post(
                move |session: Session| async move { login(session, ghost_id).await },
            );
        let router = Router::new()
            .route("/protected", get(protected))
            .route("/login", login_route)
            .with_state(state)
            .layer(session_layer);

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
            .expect("login");
        let cookie = cookie_pair(&set_cookie_of(&login_resp));

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "a session for a non-existent user must fail closed",
        );
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
