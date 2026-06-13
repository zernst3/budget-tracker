//! Authentication domain layer (`BUDGET-AUTH-GATE-1`, `SPEC §9` / `§9.1`).
//!
//! Single-user (`SPEC §9`): there is no public signup and no multi-user code.
//! The single user is provisioned out of band (a seed/CLI), and login is
//! password (Argon2) + mandatory TOTP, with passkeys/WebAuthn for day-to-day
//! biometric login.
//!
//! This module is the **port** layer for auth: pure domain types plus the
//! abstract traits the service layer orchestrates against. It carries **no**
//! crypto, no async runtime, and no framework (`DOMAIN-1`): the concrete Argon2
//! hasher, the `totp-rs` TOTP engine, the `webauthn-rs` ceremony engine, the
//! Postgres session store, and the Azure Key Vault client all live in
//! `budget-infrastructure`. Keeping the ports here lets `budget-app-services`
//! (which stays WASM-clean / runtime-free) wire auth use cases against
//! abstractions, and lets the infra impls be swapped or mocked in tests
//! (`SERVICE-DI-1`).
//!
//! The aggregate modeled here is [`WebauthnCredential`] — one row of the
//! `webauthn_credentials` table (`SPEC §5`): one user, many devices.

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::ids::{UserId, WebauthnCredentialId};

/// A registered passkey / `WebAuthn` authenticator for the single user
/// (`SPEC §5` `webauthn_credentials`; `§9.1`).
///
/// One user owns many credentials (one per device: laptop Touch ID, phone Face
/// ID, a hardware key, …). The [`sign_count`](WebauthnCredential::sign_count) is
/// the authenticator's monotonic signature counter, persisted and checked on
/// each assertion to detect cloned authenticators (a `WebAuthn` anti-replay
/// measure). The `public_key` and `credential_id` are opaque, authenticator-
/// chosen byte strings; they carry no domain validation, so they are stored as
/// raw bytes rather than validated newtypes (`DOMAIN-3` applies to *validated*
/// strings; these are opaque blobs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebauthnCredential {
    /// Stable identity (our surrogate key, `DOMAIN-2`).
    pub id: WebauthnCredentialId,
    /// The owning user (single-user V1, but `user_id`-shaped, `SPEC §5`).
    pub user_id: UserId,
    /// The authenticator-assigned credential id (opaque, globally unique). The
    /// assertion path looks a credential up by this value.
    pub credential_id: Vec<u8>,
    /// The serialized public key / passkey record. This is the
    /// `webauthn-rs` `Passkey` (CBOR/JSON-serialized at the infra boundary); the
    /// domain treats it as an opaque blob it never interprets.
    pub public_key: Vec<u8>,
    /// The authenticator's signature counter at last use (clone-detection).
    pub sign_count: i64,
    /// Authenticator transports hint (e.g. `usb,nfc,internal`), if reported.
    pub transports: Option<String>,
    /// The authenticator AAGUID (model identifier), if reported.
    pub aaguid: Option<String>,
    /// A user-friendly device label (`MacBook Touch ID`), if set.
    pub nickname: Option<String>,
    /// When the credential was registered (UTC, `DOMAIN-7`).
    pub created_at: DateTime<Utc>,
    /// When the credential was last used to authenticate (UTC), if ever.
    pub last_used_at: Option<DateTime<Utc>>,
}

/// The single shared authentication error (`DOMAIN-6` / `RUST-DOMAIN-4`).
///
/// One enum for the auth failure surface. The variants are deliberately coarse
/// on the *credential* side: [`AuthError::InvalidCredentials`] does NOT
/// distinguish "no such user" from "wrong password" from "wrong TOTP code", so
/// the boundary cannot be used as a user-enumeration oracle. Operational
/// failures (a crypto backend error, a Key Vault read failure, a store failure)
/// are distinct variants because they are not user-facing and must be logged /
/// alerted differently — but they never carry secret material (`BUDGET-PLAID-
/// TOKEN-VAULT-1` spirit: secrets never reach logs or telemetry).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    /// The supplied credentials did not authenticate. Intentionally opaque: it
    /// covers unknown user, wrong password, and wrong/expired TOTP code so the
    /// boundary reveals nothing about which factor failed.
    #[error("invalid credentials")]
    InvalidCredentials,

    /// A second factor (TOTP or passkey assertion) is required and was missing
    /// or not yet enrolled.
    #[error("second factor required")]
    SecondFactorRequired,

    /// A password hashing / verification backend error (not a wrong password —
    /// the *engine* failed, e.g. a malformed stored hash). Carries a
    /// non-sensitive description; never the password or hash material.
    #[error("password hashing failure: {0}")]
    PasswordHashing(String),

    /// A TOTP engine error (malformed secret, bad provisioning parameters).
    /// Never carries the shared secret.
    #[error("totp engine failure: {0}")]
    Totp(String),

    /// A `WebAuthn` ceremony error (registration or assertion verification
    /// failed at the protocol level — distinct from a benign wrong-credential).
    #[error("webauthn ceremony failure: {0}")]
    Webauthn(String),

    /// A session-store operation failed (issue / load / rotate / destroy).
    #[error("session store failure: {0}")]
    SessionStore(String),

    /// A secret-vault read failed (e.g. Key Vault unreachable, secret missing,
    /// managed-identity denied). Fails safe: the typed error is surfaced, the
    /// secret value is never logged (`BUDGET-PLAID-TOKEN-VAULT-1`).
    #[error("secret vault failure: {0}")]
    SecretVault(String),

    /// A persistence failure underneath an auth operation (wraps the shared
    /// [`RepositoryError`](crate::error::RepositoryError)).
    #[error(transparent)]
    Repository(#[from] crate::error::RepositoryError),
}

/// Port: a password hasher (`SPEC §9.1`, Argon2id).
///
/// Sync + infallible-shape on the verify side except for engine errors. The
/// concrete Argon2id implementation lives in `budget-infrastructure`; the
/// domain only names the capability. Hashing is intentionally a synchronous,
/// CPU-bound operation — callers in async contexts offload it to a blocking
/// pool at the infra/service boundary, never inside the domain.
pub trait PasswordHasher: Send + Sync {
    /// Hash a plaintext password into a self-describing PHC string (the value
    /// stored in `users.password_hash`).
    ///
    /// # Errors
    /// [`AuthError::PasswordHashing`] if the backend fails to produce a hash.
    fn hash(&self, plaintext: &str) -> Result<String, AuthError>;

    /// Verify a plaintext password against a stored PHC hash.
    ///
    /// Returns `Ok(true)` on a match, `Ok(false)` on a mismatch (a wrong
    /// password is NOT an error — it is a normal negative result). An
    /// [`AuthError::PasswordHashing`] is returned only when the stored hash is
    /// malformed or the engine itself fails.
    ///
    /// # Errors
    /// [`AuthError::PasswordHashing`] if the stored hash cannot be parsed or the
    /// verification backend fails.
    fn verify(&self, plaintext: &str, stored_hash: &str) -> Result<bool, AuthError>;
}

/// A freshly enrolled TOTP secret plus its provisioning URI (`SPEC §9.1`).
///
/// The provisioning URI is the `otpauth://` string an authenticator app scans
/// (rendered as a QR code by the UI in the frontend phase). The `secret` is the
/// Base32 shared secret persisted to `users.totp_secret`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotpEnrollment {
    /// The Base32-encoded shared secret (stored in `users.totp_secret`).
    pub secret: String,
    /// The `otpauth://totp/...` provisioning URI for authenticator apps.
    pub provisioning_uri: String,
}

/// Port: a TOTP (RFC 6238) second-factor engine (`SPEC §9.1`, `AUTH-1/2`).
///
/// Mirrors Agora's mandatory-TOTP pattern. The concrete `totp-rs`-backed engine
/// lives in `budget-infrastructure`.
pub trait TotpService: Send + Sync {
    /// Generate a fresh enrollment (a new random secret + its provisioning URI)
    /// for a user identified by `account_label` (e.g. the email).
    ///
    /// # Errors
    /// [`AuthError::Totp`] if the engine cannot construct a TOTP instance.
    fn enroll(&self, account_label: &str) -> Result<TotpEnrollment, AuthError>;

    /// Verify a 6-digit code against a stored Base32 `secret` at the current
    /// time (with the engine's configured skew window).
    ///
    /// Returns `Ok(true)` on a valid code, `Ok(false)` on an invalid one. An
    /// error is returned only when the stored secret is malformed.
    ///
    /// # Errors
    /// [`AuthError::Totp`] if `secret` cannot be decoded into a TOTP instance.
    fn verify(&self, secret: &str, code: &str) -> Result<bool, AuthError>;

    /// Rebuild the `otpauth://` provisioning URI for an EXISTING Base32 `secret`
    /// (without rotating it), so a logged-in user can re-display their current
    /// second factor as a QR code and add it to an additional authenticator app /
    /// device. Unlike [`enroll`](Self::enroll) this generates no new secret; it
    /// reconstructs the URI for the secret already on the user record.
    ///
    /// # Errors
    /// [`AuthError::Totp`] if `secret` cannot be decoded into a TOTP instance.
    fn provisioning_uri(&self, secret: &str, account_label: &str)
    -> Result<String, AuthError>;
}

/// Port: a secret vault (Azure Key Vault, `BUDGET-PLAID-TOKEN-VAULT-1`).
///
/// Reads and writes secrets by name/reference. Used for DB credentials and
/// (build step 8) the Plaid access token, which is **written** to the vault at
/// exchange time and thereafter stored only as a Key Vault reference in the DB —
/// the raw token never persists in a DB column or a log
/// (`BUDGET-PLAID-TOKEN-VAULT-1`). The concrete managed-identity-authenticated
/// client lives in `budget-infrastructure`. The trait is `async` because a vault
/// operation is a network call; it is declared with `async_trait` so it can be
/// held as a trait object for DI (`SERVICE-DI-1`).
#[async_trait::async_trait]
pub trait SecretVault: Send + Sync {
    /// Read the current value of the secret named `name`.
    ///
    /// Fails safe (`BUDGET-PLAID-TOKEN-VAULT-1`): on any failure it returns the
    /// typed [`AuthError::SecretVault`] and the implementation never logs the
    /// secret value. The returned secret value must likewise never be logged by
    /// the caller.
    ///
    /// # Errors
    /// [`AuthError::SecretVault`] if the vault is unreachable, the identity is
    /// denied, or the secret does not exist.
    async fn get_secret(&self, name: &str) -> Result<String, AuthError>;

    /// Write `value` as the secret named `name`, creating or updating it.
    ///
    /// The Plaid exchange path calls this to store the freshly-exchanged
    /// `access_token` (`SPEC §6`); the DB then only ever holds `name` (the
    /// reference), never `value` (`BUDGET-PLAID-TOKEN-VAULT-1`). Fails safe and
    /// never logs `value`.
    ///
    /// # Errors
    /// [`AuthError::SecretVault`] if the vault is unreachable or the identity is
    /// denied.
    async fn set_secret(&self, name: &str, value: &str) -> Result<(), AuthError>;
}

/// Persistence for the [`WebauthnCredential`] aggregate (`REPO-1` /
/// `ARCH-REPO-PER-AGGREGATE-1`).
///
/// One user, many credentials. The assertion path looks a credential up by its
/// authenticator-assigned `credential_id`; registration saves a new row; each
/// successful assertion bumps `sign_count` + `last_used_at`.
#[async_trait::async_trait]
pub trait WebauthnCredentialRepository: Send + Sync {
    /// All credentials registered for a user (the allow-list an assertion
    /// ceremony is built from).
    ///
    /// # Errors
    /// [`crate::error::RepositoryError`] on any persistence failure.
    async fn list_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Vec<WebauthnCredential>, crate::error::RepositoryError>;

    /// Look up a credential by its authenticator-assigned `credential_id`.
    ///
    /// # Errors
    /// [`crate::error::RepositoryError`] on any persistence failure.
    async fn find_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<WebauthnCredential>, crate::error::RepositoryError>;

    /// Insert or update a credential (registration; `sign_count` /
    /// `last_used_at` bump after a successful assertion).
    ///
    /// # Errors
    /// [`crate::error::RepositoryError`] on any persistence failure.
    async fn save(
        &self,
        credential: &WebauthnCredential,
        uow: Option<&dyn crate::uow::UnitOfWork>,
    ) -> Result<(), crate::error::RepositoryError>;
}
