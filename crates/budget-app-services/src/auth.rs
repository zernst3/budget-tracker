//! Authentication orchestration (`BUDGET-AUTH-GATE-1`, `SPEC §9` / `§9.1`).
//!
//! The use-case layer for auth. It wires the domain auth ports together —
//! [`PasswordHasher`], [`TotpService`], [`UserRepository`],
//! [`WebauthnCredentialRepository`] — held as `Arc<dyn _>` (`SERVICE-DI-1`); it
//! contains **no** crypto, no HTTP, and no async runtime, so the crate stays
//! WASM-clean for the Dioxus server functions that will call it in the frontend
//! phase.
//!
//! What this service decides (the policy); what it does NOT do (the mechanism):
//!   - **Login** = password (Argon2) AND mandatory TOTP, both verified here
//!     (`SPEC §9.1`). On success it returns the authenticated [`UserId`]; the
//!     CALLER (the server function / route handler) then writes that id into the
//!     session via the `tower_sessions::Session` handle (the cookie + session
//!     store are the infra/HTTP boundary, not this layer). Session create on
//!     login, validate, rotate, and destroy on logout are therefore driven from
//!     the handler using the session handle; this service supplies the identity
//!     decision that authorizes the create.
//!   - **TOTP enrollment** generates a secret + provisioning URI and persists the
//!     secret on the user (mandatory second factor, out-of-band provisioning —
//!     there is NO public signup, `SPEC §9`).
//!
//! Single-user (`SPEC §9`): there is no user lookup that returns multiple users
//! and no registration endpoint. The single user is provisioned out of band (the
//! `provision-user` seed bin); this service only authenticates that user.
//!
//! Anti-enumeration: every credential-rejection path returns the SAME opaque
//! [`AuthError::InvalidCredentials`] (unknown email, wrong password, wrong/missing
//! TOTP), so the boundary cannot be probed to discover which factor failed or
//! whether an account exists.

use std::sync::Arc;

use budget_domain::auth::{
    AuthError, PasswordHasher, TotpEnrollment, TotpService, WebauthnCredential,
    WebauthnCredentialRepository,
};
use budget_domain::ids::UserId;
use budget_domain::repositories::UserRepository;

/// Authentication use cases (`BUDGET-AUTH-GATE-1`).
///
/// Holds its collaborators as trait objects (`SERVICE-DI-1`): the infra impls
/// ([`budget_infrastructure::Argon2idHasher`], [`Rfc6238TotpService`], the
/// Postgres repositories) are injected at the application edge; tests inject
/// fakes.
#[derive(Clone)]
pub struct AuthService {
    users: Arc<dyn UserRepository>,
    credentials: Arc<dyn WebauthnCredentialRepository>,
    passwords: Arc<dyn PasswordHasher>,
    totp: Arc<dyn TotpService>,
}

impl AuthService {
    /// Wire the auth service from its collaborators (`SERVICE-DI-1`).
    #[must_use]
    pub fn new(
        users: Arc<dyn UserRepository>,
        credentials: Arc<dyn WebauthnCredentialRepository>,
        passwords: Arc<dyn PasswordHasher>,
        totp: Arc<dyn TotpService>,
    ) -> Self {
        Self {
            users,
            credentials,
            passwords,
            totp,
        }
    }

    /// Verify a password + TOTP login (`SPEC §9.1`, `BUDGET-AUTH-GATE-1`).
    ///
    /// Returns the authenticated [`UserId`] on success. The CALLER then writes
    /// this id into the session (the cookie/session-store mechanism is the infra
    /// boundary); without that, no session exists and the gate
    /// ([`budget_infrastructure::AuthedUser`]) rejects subsequent requests.
    ///
    /// Both factors are mandatory: the password is verified with Argon2 and the
    /// TOTP code with the RFC 6238 engine. ANY failure — unknown email, wrong
    /// password, a user with no enrolled TOTP, or a wrong code — returns the same
    /// opaque [`AuthError::InvalidCredentials`] (anti-enumeration). To keep the
    /// timing of "unknown email" indistinguishable from "wrong password", a
    /// rejected lookup still runs a password verification against a throwaway
    /// hash before returning.
    ///
    /// # Errors
    /// - [`AuthError::InvalidCredentials`] on any authentication failure.
    /// - [`AuthError::PasswordHashing`] / [`AuthError::Totp`] only on a genuine
    ///   engine fault (e.g. a corrupt stored hash/secret), never on a normal
    ///   wrong-credential.
    /// - [`AuthError::Repository`] on a persistence failure.
    pub async fn verify_login(
        &self,
        email: &str,
        password: &str,
        totp_code: &str,
    ) -> Result<UserId, AuthError> {
        let user = self.users.find_by_email(email).await?;

        let Some(user) = user else {
            // Unknown email: do equivalent work (a verify against a fixed dummy
            // hash) so the response timing does not leak account existence, then
            // reject opaquely.
            let _ = self.passwords.verify(password, DUMMY_ARGON2_HASH);
            return Err(AuthError::InvalidCredentials);
        };

        // Factor 1: password (Argon2). A wrong password is Ok(false) -> opaque
        // rejection; an engine error (corrupt stored hash) propagates as-is.
        if !self.passwords.verify(password, &user.password_hash)? {
            return Err(AuthError::InvalidCredentials);
        }

        // Factor 2: mandatory TOTP (SPEC §9.1). A user without an enrolled secret
        // cannot complete login (the second factor is not optional).
        let Some(secret) = user.totp_secret.as_deref() else {
            return Err(AuthError::InvalidCredentials);
        };
        if !self.totp.verify(secret, totp_code)? {
            return Err(AuthError::InvalidCredentials);
        }

        Ok(user.id)
    }

    /// Enroll (or re-enroll) the user's mandatory TOTP second factor.
    ///
    /// Generates a fresh secret + `otpauth://` provisioning URI, persists the
    /// secret on the user record, and returns the enrollment so the UI can render
    /// the QR code (frontend phase). Out-of-band only — there is no public signup
    /// (`SPEC §9`).
    ///
    /// # Errors
    /// - [`AuthError::InvalidCredentials`] if the user does not exist.
    /// - [`AuthError::Totp`] on a TOTP engine fault.
    /// - [`AuthError::Repository`] on a persistence failure.
    pub async fn enroll_totp(&self, user_id: UserId) -> Result<TotpEnrollment, AuthError> {
        let mut user = self
            .users
            .find_by_id(user_id)
            .await?
            .ok_or(AuthError::InvalidCredentials)?;
        let enrollment = self.totp.enroll(user.email.as_str())?;
        user.totp_secret = Some(enrollment.secret.clone());
        self.users.save(&user, None).await?;
        Ok(enrollment)
    }

    /// Hash a new password and persist it on the user record.
    ///
    /// Used by the provisioning path and any future password-change flow. There
    /// is no public signup; this never creates a user, only sets the hash on an
    /// existing one (`SPEC §9`).
    ///
    /// # Errors
    /// - [`AuthError::InvalidCredentials`] if the user does not exist.
    /// - [`AuthError::PasswordHashing`] on a hashing-engine fault.
    /// - [`AuthError::Repository`] on a persistence failure.
    pub async fn set_password(&self, user_id: UserId, new_password: &str) -> Result<(), AuthError> {
        let mut user = self
            .users
            .find_by_id(user_id)
            .await?
            .ok_or(AuthError::InvalidCredentials)?;
        user.password_hash = self.passwords.hash(new_password)?;
        self.users.save(&user, None).await?;
        Ok(())
    }

    /// List the registered passkeys for a user (the authentication ceremony's
    /// allow-list; also the "manage your devices" read).
    ///
    /// # Errors
    /// [`AuthError::Repository`] on a persistence failure.
    pub async fn list_credentials(
        &self,
        user_id: UserId,
    ) -> Result<Vec<WebauthnCredential>, AuthError> {
        Ok(self.credentials.list_for_user(user_id).await?)
    }

    /// Persist a newly registered passkey credential.
    ///
    /// The `WebAuthn` ceremony itself runs in the infra
    /// [`budget_infrastructure::WebauthnService`] (it needs the concrete engine);
    /// this method stores the resulting domain credential.
    ///
    /// # Errors
    /// [`AuthError::Repository`] on a persistence failure.
    pub async fn save_credential(&self, credential: &WebauthnCredential) -> Result<(), AuthError> {
        self.credentials.save(credential, None).await?;
        Ok(())
    }

    /// Look up the user a credential belongs to, after a finished passkey
    /// assertion, and persist the authenticator's advanced signature counter
    /// (clone detection, `WebauthnCredential::sign_count`).
    ///
    /// A counter that did not advance past the stored value is treated as a
    /// possible clone and rejected (`SPEC §9.1` `WebAuthn` anti-replay).
    ///
    /// # Errors
    /// - [`AuthError::InvalidCredentials`] if the credential is unknown or the
    ///   counter regressed (possible clone).
    /// - [`AuthError::Repository`] on a persistence failure.
    pub async fn complete_passkey_assertion(
        &self,
        credential_id: &[u8],
        new_sign_count: i64,
    ) -> Result<UserId, AuthError> {
        let mut credential = self
            .credentials
            .find_by_credential_id(credential_id)
            .await?
            .ok_or(AuthError::InvalidCredentials)?;

        // Clone detection: the authenticator's counter must strictly advance
        // (unless the authenticator does not maintain one, i.e. both are 0).
        if new_sign_count != 0 && new_sign_count <= credential.sign_count {
            return Err(AuthError::InvalidCredentials);
        }

        let user_id = credential.user_id;
        credential.sign_count = new_sign_count;
        credential.last_used_at = Some(now());
        self.credentials.save(&credential, None).await?;
        Ok(user_id)
    }
}

/// The current instant (indirection so the timestamp source is one place).
fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// A fixed, well-formed Argon2id hash used only to equalize the timing of an
/// unknown-email rejection with a wrong-password rejection (anti-enumeration).
/// It is a hash of a random throwaway value; nothing authenticates against it.
const DUMMY_ARGON2_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$\
c29tZXNhbHRzb21lc2FsdA$cGxhY2Vob2xkZXJoYXNocGxhY2Vob2xkZXJoYXNo";

#[cfg(test)]
mod tests;
