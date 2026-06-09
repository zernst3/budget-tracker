//! RFC 6238 TOTP second factor (`SPEC §9.1`, `AUTH-1/2`, `BUDGET-AUTH-GATE-1`).
//!
//! Concrete implementation of the domain [`TotpService`] port, mirroring Agora's
//! mandatory-TOTP pattern (`AUTH-1/2`). Backed by `totp-rs` with the standard
//! authenticator-app parameters: SHA1, 6 digits, 30-second step, ±1 step of skew
//! (so a code valid in the adjacent window is still accepted, covering clock
//! drift and the moment a step rolls over). The shared secret is a Base32 string
//! persisted to `users.totp_secret`; the provisioning URI is the `otpauth://`
//! string the UI renders as a QR code in the frontend phase.

use totp_rs::{Algorithm, Secret, TOTP};

use budget_domain::auth::{AuthError, TotpEnrollment, TotpService};

/// The TOTP issuer label embedded in the provisioning URI (shown in the
/// authenticator app next to the account).
const ISSUER: &str = "Budget Tracker";

/// SHA1 / 6 digits / 30s step are the de-facto authenticator-app standard
/// (Google Authenticator, Authy, 1Password all assume them).
const DIGITS: usize = 6;
const STEP_SECONDS: u64 = 30;
/// ±1 step (30s either side) tolerates clock drift and step-boundary timing.
const SKEW_STEPS: u8 = 1;

/// `totp-rs`-backed [`TotpService`].
#[derive(Debug, Default, Clone)]
pub struct Rfc6238TotpService;

impl Rfc6238TotpService {
    /// Construct the service with the standard authenticator-app parameters.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Build a [`TOTP`] instance from a Base32-encoded secret + account label.
    ///
    /// Shared by [`enroll`](Self::enroll) (a fresh secret) and
    /// [`verify`](TotpService::verify) (a stored secret).
    fn build(secret_base32: &str, account_label: &str) -> Result<TOTP, AuthError> {
        let secret = Secret::Encoded(secret_base32.to_owned())
            .to_bytes()
            .map_err(|e| AuthError::Totp(format!("invalid base32 secret: {e:?}")))?;
        TOTP::new(
            Algorithm::SHA1,
            DIGITS,
            SKEW_STEPS,
            STEP_SECONDS,
            secret,
            Some(ISSUER.to_owned()),
            account_label.to_owned(),
        )
        .map_err(|e| AuthError::Totp(e.to_string()))
    }
}

impl TotpService for Rfc6238TotpService {
    fn enroll(&self, account_label: &str) -> Result<TotpEnrollment, AuthError> {
        // A fresh random secret of the default (RFC-compliant) length.
        let secret_base32 = Secret::generate_secret().to_encoded().to_string();
        let totp = Self::build(&secret_base32, account_label)?;
        Ok(TotpEnrollment {
            secret: secret_base32,
            provisioning_uri: totp.get_url(),
        })
    }

    fn verify(&self, secret: &str, code: &str) -> Result<bool, AuthError> {
        let totp = Self::build(secret, "verify")?;
        // check_current only errors if the system clock is before the Unix epoch,
        // which is not a wrong-code condition; surface it as an engine error.
        totp.check_current(code)
            .map_err(|e| AuthError::Totp(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::Rfc6238TotpService;
    use budget_domain::auth::{AuthError, TotpService};
    use totp_rs::{Algorithm, Secret, TOTP};

    #[test]
    fn enroll_produces_otpauth_uri_and_base32_secret() {
        let svc = Rfc6238TotpService::new();
        let enrollment = svc.enroll("zach@example.com").expect("enroll");
        assert!(
            enrollment.provisioning_uri.starts_with("otpauth://totp/"),
            "expected an otpauth provisioning URI, got: {}",
            enrollment.provisioning_uri,
        );
        assert!(
            enrollment.provisioning_uri.contains("Budget%20Tracker")
                || enrollment.provisioning_uri.contains("Budget Tracker"),
            "issuer must be embedded in the URI",
        );
        // The secret must be decodable Base32 (round-trips through Secret).
        assert!(
            Secret::Encoded(enrollment.secret.clone())
                .to_bytes()
                .is_ok(),
            "enrolled secret must be valid base32",
        );
    }

    #[test]
    fn current_code_for_enrolled_secret_verifies() {
        let svc = Rfc6238TotpService::new();
        let enrollment = svc.enroll("zach@example.com").expect("enroll");
        // Independently derive the current code from the same secret, then verify
        // through the service (non-tautological: the generator is a separate TOTP).
        let bytes = Secret::Encoded(enrollment.secret.clone())
            .to_bytes()
            .expect("decode");
        let generator = TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            bytes,
            Some("Budget Tracker".to_owned()),
            "zach@example.com".to_owned(),
        )
        .expect("build generator");
        let code = generator.generate_current().expect("generate");
        assert_eq!(svc.verify(&enrollment.secret, &code), Ok(true));
    }

    #[test]
    fn wrong_code_does_not_verify() {
        let svc = Rfc6238TotpService::new();
        let enrollment = svc.enroll("zach@example.com").expect("enroll");
        assert_eq!(svc.verify(&enrollment.secret, "000000"), Ok(false));
    }

    #[test]
    fn malformed_secret_is_engine_error() {
        let svc = Rfc6238TotpService::new();
        let err = svc.verify("!!!not-base32!!!", "123456").unwrap_err();
        assert!(matches!(err, AuthError::Totp(_)));
    }
}
