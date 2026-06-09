//! `AuthService` policy tests (`BUDGET-AUTH-GATE-1`, `ORCH-NEW-PATH-TESTS-1`).
//!
//! Fakes implement the domain auth ports so the policy is exercised without any
//! crypto / DB / runtime. The fakes are intentionally simple oracles: the
//! password hasher treats the hash as `"hash:" + plaintext`; the TOTP engine
//! accepts a fixed code. The assertions target the SERVICE policy — both factors
//! mandatory, opaque rejections, clone detection — not the crypto (which the
//! infra tests cover).
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;

use budget_domain::RepositoryError;
use budget_domain::auth::{
    AuthError, PasswordHasher, TotpEnrollment, TotpService, WebauthnCredential,
    WebauthnCredentialRepository,
};
use budget_domain::ids::{UserId, WebauthnCredentialId};
use budget_domain::repositories::UserRepository;
use budget_domain::uow::UnitOfWork;
use budget_domain::user::User;
use budget_domain::validated::Email;

use super::AuthService;
use std::sync::Arc;

/// An in-memory single-user repo. Holds at most one user, updated on `save`.
struct FakeUsers {
    user: Mutex<Option<User>>,
}

impl FakeUsers {
    fn with(user: User) -> Arc<Self> {
        Arc::new(Self {
            user: Mutex::new(Some(user)),
        })
    }
    fn empty() -> Arc<Self> {
        Arc::new(Self {
            user: Mutex::new(None),
        })
    }
}

#[async_trait]
impl UserRepository for FakeUsers {
    async fn find_by_id(&self, id: UserId) -> Result<Option<User>, RepositoryError> {
        let guard = self.user.lock().unwrap();
        Ok(guard.clone().filter(|u| u.id == id))
    }
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError> {
        let guard = self.user.lock().unwrap();
        Ok(guard.clone().filter(|u| u.email.as_str() == email))
    }
    async fn save(
        &self,
        user: &User,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        *self.user.lock().unwrap() = Some(user.clone());
        Ok(())
    }
}

/// A fake password hasher: hash = `"hash:" + plaintext`; verify is exact match.
struct FakePasswords;

impl PasswordHasher for FakePasswords {
    fn hash(&self, plaintext: &str) -> Result<String, AuthError> {
        Ok(format!("hash:{plaintext}"))
    }
    fn verify(&self, plaintext: &str, stored_hash: &str) -> Result<bool, AuthError> {
        Ok(stored_hash == format!("hash:{plaintext}"))
    }
}

/// A fake TOTP engine: a fixed secret + a fixed accepted code.
struct FakeTotp;

const ACCEPTED_CODE: &str = "123456";
const FAKE_SECRET: &str = "JBSWY3DPEHPK3PXP";

impl TotpService for FakeTotp {
    fn enroll(&self, _account_label: &str) -> Result<TotpEnrollment, AuthError> {
        Ok(TotpEnrollment {
            secret: FAKE_SECRET.to_owned(),
            provisioning_uri: "otpauth://totp/Budget?secret=JBSWY3DPEHPK3PXP".to_owned(),
        })
    }
    fn verify(&self, secret: &str, code: &str) -> Result<bool, AuthError> {
        Ok(secret == FAKE_SECRET && code == ACCEPTED_CODE)
    }
}

/// An in-memory credential repo.
struct FakeCredentials {
    rows: Mutex<Vec<WebauthnCredential>>,
}

impl FakeCredentials {
    fn empty() -> Arc<Self> {
        Arc::new(Self {
            rows: Mutex::new(Vec::new()),
        })
    }
    fn with(rows: Vec<WebauthnCredential>) -> Arc<Self> {
        Arc::new(Self {
            rows: Mutex::new(rows),
        })
    }
}

#[async_trait]
impl WebauthnCredentialRepository for FakeCredentials {
    async fn list_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Vec<WebauthnCredential>, RepositoryError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.user_id == user_id)
            .cloned()
            .collect())
    }
    async fn find_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<WebauthnCredential>, RepositoryError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.credential_id == credential_id)
            .cloned())
    }
    async fn save(
        &self,
        credential: &WebauthnCredential,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut rows = self.rows.lock().unwrap();
        if let Some(slot) = rows.iter_mut().find(|c| c.id == credential.id) {
            *slot = credential.clone();
        } else {
            rows.push(credential.clone());
        }
        Ok(())
    }
}

fn enrolled_user() -> User {
    User {
        id: UserId::generate(),
        email: Email::try_new("zach@example.com").unwrap(),
        password_hash: "hash:correct-password".to_owned(),
        totp_secret: Some(FAKE_SECRET.to_owned()),
        tracking_start_date: chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
        created_at: Utc::now(),
    }
}

fn service_for(user: User) -> (AuthService, UserId) {
    let id = user.id;
    let svc = AuthService::new(
        FakeUsers::with(user),
        FakeCredentials::empty(),
        Arc::new(FakePasswords),
        Arc::new(FakeTotp),
    );
    (svc, id)
}

#[tokio::test]
async fn correct_password_and_totp_authenticates() {
    let (svc, id) = service_for(enrolled_user());
    let result = svc
        .verify_login("zach@example.com", "correct-password", ACCEPTED_CODE)
        .await;
    assert_eq!(result, Ok(id), "valid both-factor login must authenticate");
}

#[tokio::test]
async fn wrong_password_is_rejected_opaquely() {
    let (svc, _id) = service_for(enrolled_user());
    let result = svc
        .verify_login("zach@example.com", "WRONG", ACCEPTED_CODE)
        .await;
    assert_eq!(result, Err(AuthError::InvalidCredentials));
}

#[tokio::test]
async fn correct_password_but_wrong_totp_is_rejected() {
    // The second factor is MANDATORY (SPEC §9.1): a correct password alone fails.
    let (svc, _id) = service_for(enrolled_user());
    let result = svc
        .verify_login("zach@example.com", "correct-password", "000000")
        .await;
    assert_eq!(result, Err(AuthError::InvalidCredentials));
}

#[tokio::test]
async fn user_without_enrolled_totp_cannot_log_in() {
    // TOTP is mandatory; a user with no secret cannot complete login even with
    // the right password.
    let mut user = enrolled_user();
    user.totp_secret = None;
    let (svc, _id) = service_for(user);
    let result = svc
        .verify_login("zach@example.com", "correct-password", ACCEPTED_CODE)
        .await;
    assert_eq!(result, Err(AuthError::InvalidCredentials));
}

#[tokio::test]
async fn unknown_email_is_rejected_opaquely() {
    let svc = AuthService::new(
        FakeUsers::empty(),
        FakeCredentials::empty(),
        Arc::new(FakePasswords),
        Arc::new(FakeTotp),
    );
    let result = svc
        .verify_login("ghost@example.com", "whatever", ACCEPTED_CODE)
        .await;
    // Same opaque error as a wrong password — no user-enumeration oracle.
    assert_eq!(result, Err(AuthError::InvalidCredentials));
}

#[tokio::test]
async fn enroll_totp_persists_secret_and_returns_uri() {
    let mut user = enrolled_user();
    user.totp_secret = None; // not yet enrolled
    let (svc, id) = service_for(user);
    let enrollment = svc.enroll_totp(id).await.expect("enroll");
    assert!(enrollment.provisioning_uri.starts_with("otpauth://"));
    // After enrollment the user can complete a both-factor login.
    let result = svc
        .verify_login("zach@example.com", "correct-password", ACCEPTED_CODE)
        .await;
    assert_eq!(result, Ok(id));
}

#[tokio::test]
async fn set_password_changes_the_verified_password() {
    let (svc, id) = service_for(enrolled_user());
    svc.set_password(id, "new-password").await.expect("set");
    // Old password no longer authenticates...
    assert_eq!(
        svc.verify_login("zach@example.com", "correct-password", ACCEPTED_CODE)
            .await,
        Err(AuthError::InvalidCredentials),
    );
    // ...the new one does.
    assert_eq!(
        svc.verify_login("zach@example.com", "new-password", ACCEPTED_CODE)
            .await,
        Ok(id),
    );
}

fn credential_for(user_id: UserId, cred_id: Vec<u8>, sign_count: i64) -> WebauthnCredential {
    WebauthnCredential {
        id: WebauthnCredentialId::generate(),
        user_id,
        credential_id: cred_id,
        public_key: vec![1, 2, 3],
        sign_count,
        transports: None,
        aaguid: None,
        nickname: None,
        created_at: Utc::now(),
        last_used_at: None,
    }
}

#[tokio::test]
async fn passkey_assertion_advances_counter_and_yields_user() {
    let user_id = UserId::generate();
    let creds = FakeCredentials::with(vec![credential_for(user_id, vec![9, 9], 5)]);
    let svc = AuthService::new(
        FakeUsers::empty(),
        creds.clone(),
        Arc::new(FakePasswords),
        Arc::new(FakeTotp),
    );
    // A strictly advancing counter is accepted.
    let result = svc.complete_passkey_assertion(&[9, 9], 6).await;
    assert_eq!(result, Ok(user_id));
    // The advanced counter + last_used_at were persisted.
    let stored = creds.find_by_credential_id(&[9, 9]).await.unwrap().unwrap();
    assert_eq!(stored.sign_count, 6);
    assert!(stored.last_used_at.is_some());
}

#[tokio::test]
async fn passkey_assertion_with_regressed_counter_is_rejected() {
    // A counter that does not advance signals a possible clone (SPEC §9.1).
    let user_id = UserId::generate();
    let creds = FakeCredentials::with(vec![credential_for(user_id, vec![7], 10)]);
    let svc = AuthService::new(
        FakeUsers::empty(),
        creds,
        Arc::new(FakePasswords),
        Arc::new(FakeTotp),
    );
    assert_eq!(
        svc.complete_passkey_assertion(&[7], 10).await,
        Err(AuthError::InvalidCredentials),
        "a non-advancing counter must be rejected as a possible clone",
    );
}

#[tokio::test]
async fn passkey_assertion_with_unknown_credential_is_rejected() {
    let svc = AuthService::new(
        FakeUsers::empty(),
        FakeCredentials::empty(),
        Arc::new(FakePasswords),
        Arc::new(FakeTotp),
    );
    assert_eq!(
        svc.complete_passkey_assertion(&[0, 0, 0], 1).await,
        Err(AuthError::InvalidCredentials),
    );
}

// ---- User-scoping defense in depth (BUDGET-AUTH-GATE-1, SPEC §9.1) ----------

#[tokio::test]
async fn list_credentials_is_scoped_to_the_authenticated_user() {
    // BUDGET-AUTH-GATE-1: "every query is additionally scoped to the
    // authenticated user_id." Even in single-user V1 the read path must not
    // return another user's rows. We seed credentials for TWO users and assert
    // each user sees only their own.
    let me = UserId::generate();
    let other = UserId::generate();
    assert_ne!(me, other);

    let creds = FakeCredentials::with(vec![
        credential_for(me, vec![1, 1], 0),
        credential_for(me, vec![2, 2], 0),
        credential_for(other, vec![9, 9], 0),
    ]);
    let svc = AuthService::new(
        FakeUsers::empty(),
        creds,
        Arc::new(FakePasswords),
        Arc::new(FakeTotp),
    );

    let mine = svc.list_credentials(me).await.expect("list");
    assert_eq!(mine.len(), 2, "must see exactly my two credentials");
    assert!(
        mine.iter().all(|c| c.user_id == me),
        "no other user's credential may appear in my list",
    );
    assert!(
        mine.iter().all(|c| c.credential_id != vec![9, 9]),
        "the other user's credential must be excluded",
    );

    // And the other user sees only theirs.
    let theirs = svc.list_credentials(other).await.expect("list other");
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].user_id, other);
}

#[tokio::test]
async fn passkey_assertion_yields_the_owning_users_id_for_scoping() {
    // The assertion path returns the credential OWNER's user_id, which the gate
    // then scopes queries to. A credential owned by `owner` must yield exactly
    // `owner`, never a different/ambient identity.
    let owner = UserId::generate();
    let creds = FakeCredentials::with(vec![credential_for(owner, vec![4, 2], 7)]);
    let svc = AuthService::new(
        FakeUsers::empty(),
        creds,
        Arc::new(FakePasswords),
        Arc::new(FakeTotp),
    );
    let yielded = svc
        .complete_passkey_assertion(&[4, 2], 8)
        .await
        .expect("assertion");
    assert_eq!(
        yielded, owner,
        "the assertion must scope to the credential's owning user",
    );
}

#[tokio::test]
async fn enroll_and_set_password_on_unknown_user_do_not_mutate_anything() {
    // No public signup (SPEC §9): operating on a user id the repo doesn't hold
    // must reject, never create a user as a side effect.
    let svc = AuthService::new(
        FakeUsers::empty(),
        FakeCredentials::empty(),
        Arc::new(FakePasswords),
        Arc::new(FakeTotp),
    );
    let ghost = UserId::generate();
    assert_eq!(
        svc.enroll_totp(ghost).await.err(),
        Some(AuthError::InvalidCredentials),
    );
    assert_eq!(
        svc.set_password(ghost, "x").await.err(),
        Some(AuthError::InvalidCredentials),
    );
}
