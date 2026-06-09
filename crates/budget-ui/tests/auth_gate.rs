//! Integration test for the server-function `AuthedUser` gate
//! (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`, `ORCH-NEW-PATH-TESTS-1`).
//!
//! Server-only (`#[cfg(feature = "server")]`): the gate
//! (`budget_ui::services::gate::require_authed_user`) and the `AppState` it reads
//! only exist on the native server target. This test drives the REAL gate inside
//! a `FullstackContext` scope (the same context a server-function handler runs
//! in), proving by construction that:
//!   - an UNAUTHENTICATED call (no session id in the session) is rejected with a
//!     401 and reaches no user data; and
//!   - an AUTHENTICATED call (the login path wrote the `user_id`) yields the
//!     authenticated user, scoped to that id.
//!
//! It exercises the same task-local `FullstackContext` plumbing the dioxus
//! server-function handler uses (`FullstackContext::new(parts).scope(fut)`), so
//! the gate is verified against the actual extraction path, not a reimplementation.
#![cfg(feature = "server")]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use dioxus::fullstack::FullstackContext;
use http::Request;
use tower_sessions::{MemoryStore, Session, session::Id, session_store::SessionStore};

use budget_domain::RepositoryError;
use budget_domain::auth::{WebauthnCredential, WebauthnCredentialRepository};
use budget_domain::ids::UserId;
use budget_domain::repositories::UserRepository;
use budget_domain::uow::UnitOfWork;
use budget_domain::user::User;
use budget_domain::validated::Email;

use budget_app_services::AuthService;
use budget_infrastructure::WebauthnService;
use budget_infrastructure::auth::{Argon2idHasher, Rfc6238TotpService};
use budget_ui::server_state::AppState;
use budget_ui::services::gate::{SESSION_USER_ID_KEY, require_authed_user};

/// An in-memory user repository so the test needs no Postgres. Holds the single
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
    async fn save(&self, _u: &User, _uow: Option<&dyn UnitOfWork>) -> Result<(), RepositoryError> {
        Ok(())
    }
}

/// A repository that holds NO matching user — to prove a session pointing at a
/// deleted user fails closed.
struct EmptyUserRepo;

#[async_trait]
impl UserRepository for EmptyUserRepo {
    async fn find_by_id(&self, _id: UserId) -> Result<Option<User>, RepositoryError> {
        Ok(None)
    }
    async fn find_by_email(&self, _email: &str) -> Result<Option<User>, RepositoryError> {
        Ok(None)
    }
    async fn save(&self, _u: &User, _uow: Option<&dyn UnitOfWork>) -> Result<(), RepositoryError> {
        Ok(())
    }
}

/// A no-op credentials repository: the gate + login-failure paths under test
/// never touch passkeys, so an empty implementation suffices for `AuthService`.
struct NoopCredentials;

#[async_trait]
impl WebauthnCredentialRepository for NoopCredentials {
    async fn list_for_user(
        &self,
        _user_id: UserId,
    ) -> Result<Vec<WebauthnCredential>, RepositoryError> {
        Ok(Vec::new())
    }
    async fn find_by_credential_id(
        &self,
        _credential_id: &[u8],
    ) -> Result<Option<WebauthnCredential>, RepositoryError> {
        Ok(None)
    }
    async fn save(
        &self,
        _credential: &WebauthnCredential,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
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

fn app_state(users: Arc<dyn UserRepository>) -> AppState {
    let credentials = Arc::new(NoopCredentials);
    let passwords = Arc::new(Argon2idHasher::new());
    let totp = Arc::new(Rfc6238TotpService::new());
    let auth = Arc::new(AuthService::new(
        users.clone(),
        credentials,
        passwords,
        totp,
    ));
    let webauthn = Arc::new(
        WebauthnService::new("localhost", "http://localhost:8080", "Budget Tracker")
            .expect("valid rp params build the webauthn engine"),
    );
    AppState::new(users, auth, webauthn)
}

/// Build a request `Parts` carrying the `AppState` extension and a `Session`
/// extension, then run `require_authed_user` inside the matching
/// `FullstackContext` scope — exactly the plumbing a real server function uses.
async fn run_gate_with(
    state: AppState,
    session: Session,
) -> Result<budget_ui::services::gate::AuthedServerUser, dioxus::prelude::ServerFnError> {
    let mut request = Request::builder()
        .method("GET")
        .uri("/api/data")
        .body(())
        .unwrap();
    // Insert the state as a BARE value (Axum's `Extension<T>` extractor reads
    // `T` from extensions, exactly as the `.layer(Extension(state))` in
    // budget-server installs it).
    request.extensions_mut().insert(state);
    request.extensions_mut().insert(session);
    let (parts, ()) = request.into_parts();

    FullstackContext::new(parts)
        .scope(async move { require_authed_user().await })
        .await
}

/// Create a fresh, empty session backed by an in-memory store (no id written).
fn fresh_session(store: &Arc<MemoryStore>) -> Session {
    Session::new(None, store.clone(), None)
}

#[tokio::test]
async fn unauthenticated_call_to_a_gated_path_is_rejected_401() {
    // BUDGET-AUTH-GATE-1: a session with NO authenticated user id must 401 and
    // reach no data.
    let store = Arc::new(MemoryStore::default());
    let state = app_state(Arc::new(FakeUserRepo {
        user: sample_user(),
    }));
    let session = fresh_session(&store);

    let result = run_gate_with(state, session).await;

    let err = result.expect_err("an empty session must be rejected");
    let dioxus::prelude::ServerFnError::ServerError { code, .. } = err else {
        panic!("the gate must reject with a ServerError(401), got {err:?}");
    };
    assert_eq!(
        code, 401,
        "the gate must reject an unauthenticated call with 401"
    );
}

#[tokio::test]
async fn authenticated_call_yields_the_scoped_user() {
    // BUDGET-AUTH-GATE-1: with the login-written user id in the session, the gate
    // yields the authenticated user, scoped to that id.
    let store = Arc::new(MemoryStore::default());
    let user = sample_user();
    let user_id = user.id;
    let state = app_state(Arc::new(FakeUserRepo { user }));

    // Mirror what a real request carries: a session whose in-memory payload
    // holds the login-written user id (the SessionManagerLayer hydrates this from
    // the store on a real request; here we seed it directly).
    let session = fresh_session(&store);
    session.insert(SESSION_USER_ID_KEY, user_id).await.unwrap();

    let authed = run_gate_with(state, session)
        .await
        .expect("a valid session must authenticate");
    assert_eq!(
        authed.id(),
        user_id,
        "the gate must yield the authenticated user id",
    );
    assert_eq!(authed.user().id, user_id);
}

#[tokio::test]
async fn session_for_a_deleted_user_fails_closed_401() {
    // Defense in depth: a session whose user no longer exists must 401, never a
    // partial identity.
    let store = Arc::new(MemoryStore::default());
    let state = app_state(Arc::new(EmptyUserRepo));

    let session = fresh_session(&store);
    session
        .insert(SESSION_USER_ID_KEY, UserId::generate())
        .await
        .unwrap();
    session.save().await.unwrap();

    let result = run_gate_with(state, session).await;
    let err = result.expect_err("a deleted-user session must fail closed");
    let dioxus::prelude::ServerFnError::ServerError { code, .. } = err else {
        panic!("expected a ServerError(401), got {err:?}");
    };
    assert_eq!(code, 401, "a session for a non-existent user must 401");
}

// Touch `Id` and `SessionStore` so the imports that document the store-backed
// session model are exercised even though the helpers above hide them.
#[tokio::test]
async fn session_store_round_trips_the_payload() {
    let store = MemoryStore::default();
    let session = Session::new(None, Arc::new(store.clone()), None);
    session.insert("k", 1u8).await.unwrap();
    session.save().await.unwrap();
    let id: Id = session.id().expect("a saved session has an id");
    let record = store.load(&id).await.unwrap();
    assert!(record.is_some(), "the saved session must be loadable by id");
}
