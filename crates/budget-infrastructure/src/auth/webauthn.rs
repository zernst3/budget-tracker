//! `WebAuthn` / passkey ceremony engine (`SPEC §9.1`, `BUDGET-AUTH-GATE-1`).
//!
//! Concrete wrapper over `webauthn-rs` 0.5. Passkeys are the day-to-day
//! biometric login (Touch ID / Face ID); TOTP is the fallback (`SPEC §9.1`). A
//! web app cannot read a fingerprint sensor directly: `WebAuthn` has the OS mediate
//! the biometric and return a public-key assertion, which this engine verifies.
//!
//! Two ceremonies, each a start/finish pair:
//!   - **Registration** binds a new authenticator to the user. `start` issues a
//!     challenge; the browser creates a credential; `finish` verifies it and
//!     yields a [`Passkey`] we persist (serialized) as a
//!     [`budget_domain::auth::WebauthnCredential`].
//!   - **Authentication** proves possession of a registered authenticator.
//!     `start` issues a challenge against the user's registered credentials;
//!     `finish` verifies the assertion and reports the authenticator's new
//!     signature counter (clone detection, `WebauthnCredential::sign_count`).
//!
//! The ceremony *state* (`PasskeyRegistration` / `PasskeyAuthentication`) MUST be
//! held server-side between start and finish; in the frontend phase it lives in
//! the session. Here the engine returns it to the caller; this crate does not own
//! the HTTP host.

use std::time::Duration;

use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Url,
};
use webauthn_rs::{Webauthn, WebauthnBuilder};

use budget_domain::auth::{AuthError, WebauthnCredential};
use budget_domain::ids::{UserId, WebauthnCredentialId};

/// The session-bound ceremony timeout: the start/finish pair must complete
/// within it (one minute).
const CEREMONY_TIMEOUT: Duration = Duration::from_mins(1);

/// The product of a finished registration ceremony: the serialized passkey to
/// persist plus the raw credential id and starting counter.
#[derive(Debug, Clone)]
pub struct RegisteredPasskey {
    /// The authenticator-assigned credential id (raw bytes).
    pub credential_id: Vec<u8>,
    /// The JSON-serialized [`Passkey`] record (opaque blob for the domain).
    pub public_key: Vec<u8>,
    /// The authenticator's initial signature counter.
    pub sign_count: i64,
}

/// `webauthn-rs`-backed passkey ceremony engine.
pub struct WebauthnService {
    inner: Webauthn,
}

impl WebauthnService {
    /// Build the engine for a relying party.
    ///
    /// `rp_id` is the effective domain (e.g. `budget.example.com`); `rp_origin`
    /// is the full HTTPS origin the browser will report (e.g.
    /// `https://budget.example.com`). These are fixed per deployment and come
    /// from configuration in the frontend phase.
    ///
    /// # Errors
    /// [`AuthError::Webauthn`] if `rp_origin` is not a valid URL or the builder
    /// rejects the relying-party parameters.
    pub fn new(rp_id: &str, rp_origin: &str, rp_name: &str) -> Result<Self, AuthError> {
        let origin = Url::parse(rp_origin)
            .map_err(|e| AuthError::Webauthn(format!("bad rp_origin: {e}")))?;
        let inner = WebauthnBuilder::new(rp_id, &origin)
            .map_err(|e| AuthError::Webauthn(e.to_string()))?
            .rp_name(rp_name)
            // The session-bound ceremony timeout; the start/finish pair must
            // complete within it.
            .timeout(CEREMONY_TIMEOUT)
            .build()
            .map_err(|e| AuthError::Webauthn(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Begin registering a new authenticator for `user_id`.
    ///
    /// `existing` is the user's already-registered credentials, excluded so the
    /// same authenticator is not enrolled twice. Returns the challenge to send to
    /// the browser plus the server-side ceremony state to persist.
    ///
    /// # Errors
    /// [`AuthError::Webauthn`] if the ceremony cannot be started.
    pub fn start_registration(
        &self,
        user_id: UserId,
        user_name: &str,
        user_display_name: &str,
        existing: &[WebauthnCredential],
    ) -> Result<(CreationChallengeResponse, PasskeyRegistration), AuthError> {
        let exclude = if existing.is_empty() {
            None
        } else {
            Some(
                existing
                    .iter()
                    .map(|c| c.credential_id.clone().into())
                    .collect(),
            )
        };
        self.inner
            .start_passkey_registration(user_id.value(), user_name, user_display_name, exclude)
            .map_err(|e| AuthError::Webauthn(e.to_string()))
    }

    /// Finish a registration ceremony, verifying the browser's credential against
    /// the persisted ceremony state.
    ///
    /// # Errors
    /// [`AuthError::Webauthn`] if the credential fails verification.
    pub fn finish_registration(
        &self,
        response: &RegisterPublicKeyCredential,
        state: &PasskeyRegistration,
    ) -> Result<RegisteredPasskey, AuthError> {
        let passkey = self
            .inner
            .finish_passkey_registration(response, state)
            .map_err(|e| AuthError::Webauthn(e.to_string()))?;
        Self::serialize_passkey(&passkey)
    }

    /// Begin authenticating `existing` registered credentials.
    ///
    /// Returns the challenge to send to the browser plus the server-side ceremony
    /// state to persist.
    ///
    /// # Errors
    /// [`AuthError::Webauthn`] if no credentials are registered or the ceremony
    /// cannot be started.
    pub fn start_authentication(
        &self,
        existing: &[WebauthnCredential],
    ) -> Result<(RequestChallengeResponse, PasskeyAuthentication), AuthError> {
        if existing.is_empty() {
            return Err(AuthError::Webauthn(
                "no registered credentials for user".to_owned(),
            ));
        }
        let passkeys = existing
            .iter()
            .map(|c| Self::deserialize_passkey(&c.public_key))
            .collect::<Result<Vec<_>, _>>()?;
        self.inner
            .start_passkey_authentication(&passkeys)
            .map_err(|e| AuthError::Webauthn(e.to_string()))
    }

    /// Finish an authentication ceremony, verifying the browser's assertion.
    ///
    /// Returns the raw credential id that authenticated and the authenticator's
    /// new signature counter (which the caller persists to detect clones). A
    /// counter that did not advance is reported as-is; the caller (the service
    /// layer) decides clone-rejection policy.
    ///
    /// # Errors
    /// [`AuthError::Webauthn`] if the assertion fails verification.
    pub fn finish_authentication(
        &self,
        response: &PublicKeyCredential,
        state: &PasskeyAuthentication,
    ) -> Result<AuthenticationOutcome, AuthError> {
        let result = self
            .inner
            .finish_passkey_authentication(response, state)
            .map_err(|e| AuthError::Webauthn(e.to_string()))?;
        // The authenticator counter is a u32, so widening to i64 is infallible.
        let new_count = i64::from(result.counter());
        Ok(AuthenticationOutcome {
            credential_id: result.cred_id().as_ref().to_vec(),
            new_sign_count: new_count,
            user_verified: result.user_verified(),
        })
    }

    /// Build a domain [`WebauthnCredential`] from a finished registration.
    ///
    /// Constructs the surrogate id + timestamps; the caller persists it via the
    /// [`budget_domain::auth::WebauthnCredentialRepository`].
    #[must_use]
    pub fn to_domain_credential(
        registered: &RegisteredPasskey,
        user_id: UserId,
        nickname: Option<String>,
    ) -> WebauthnCredential {
        WebauthnCredential {
            id: WebauthnCredentialId::generate(),
            user_id,
            credential_id: registered.credential_id.clone(),
            public_key: registered.public_key.clone(),
            sign_count: registered.sign_count,
            transports: None,
            aaguid: None,
            nickname,
            created_at: chrono::Utc::now(),
            last_used_at: None,
        }
    }

    fn serialize_passkey(passkey: &Passkey) -> Result<RegisteredPasskey, AuthError> {
        let public_key = serde_json::to_vec(passkey)
            .map_err(|e| AuthError::Webauthn(format!("passkey serialize: {e}")))?;
        Ok(RegisteredPasskey {
            credential_id: passkey.cred_id().as_ref().to_vec(),
            public_key,
            // A freshly registered passkey starts at counter 0 unless the
            // authenticator reports otherwise; the stored Passkey is the source
            // of truth on subsequent auths, so 0 is the safe initial value.
            sign_count: 0,
        })
    }

    fn deserialize_passkey(public_key: &[u8]) -> Result<Passkey, AuthError> {
        serde_json::from_slice(public_key)
            .map_err(|e| AuthError::Webauthn(format!("passkey deserialize: {e}")))
    }
}

/// The product of a finished authentication ceremony.
#[derive(Debug, Clone)]
pub struct AuthenticationOutcome {
    /// The raw credential id that authenticated (matches a stored credential).
    pub credential_id: Vec<u8>,
    /// The authenticator's new signature counter (persist for clone detection).
    pub new_sign_count: i64,
    /// Whether user verification (biometric / PIN) was performed.
    pub user_verified: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::WebauthnService;

    #[test]
    fn builds_for_valid_rp() {
        let svc = WebauthnService::new("localhost", "http://localhost:8080", "Budget Tracker");
        assert!(svc.is_ok(), "valid rp params must build: {:?}", svc.err());
    }

    #[test]
    fn rejects_invalid_origin() {
        let svc = WebauthnService::new("localhost", "not a url", "Budget Tracker");
        assert!(svc.is_err(), "an invalid origin must be rejected");
    }

    #[test]
    fn start_authentication_with_no_credentials_errors() {
        let svc = WebauthnService::new("localhost", "http://localhost:8080", "Budget Tracker")
            .expect("build");
        let err = svc.start_authentication(&[]).unwrap_err();
        assert!(
            matches!(err, budget_domain::auth::AuthError::Webauthn(_)),
            "no-credentials authentication must surface a Webauthn error",
        );
    }
}
