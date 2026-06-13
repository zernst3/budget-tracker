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

    fn provisioning_uri(
        &self,
        secret: &str,
        account_label: &str,
    ) -> Result<String, AuthError> {
        // Rebuild the same TOTP the secret already represents (issuer + the
        // standard parameters) and emit its otpauth:// URI. No new secret is
        // minted, so adding another authenticator device does not invalidate the
        // existing ones.
        Ok(Self::build(secret, account_label)?.get_url())
    }
}

/// A LOCAL-DEVELOPMENT-ONLY [`TotpService`] that accepts ANY code as valid.
///
/// Reached **only** by the explicit `TOTP_BYPASS=dev` opt-in wired in
/// `budget-ui`'s `server_state` (mirroring the `AI_MODE=mock` / `PLAID_MODE=mock`
/// switches). It exists so a developer running locally can sign in without an
/// enrolled authenticator app; it MUST NEVER be selected in production, where it
/// would render the mandatory second factor inert (`SPEC §9.1`).
///
/// It delegates [`enroll`](TotpService::enroll) and
/// [`provisioning_uri`](TotpService::provisioning_uri) to the real engine so the
/// QR-enrollment screen still produces a genuine secret + URI (a developer can
/// scan it to test the real flow); only [`verify`](TotpService::verify) is
/// short-circuited to `Ok(true)`.
#[derive(Debug, Default, Clone)]
pub struct BypassTotpService {
    inner: Rfc6238TotpService,
}

impl BypassTotpService {
    /// Construct the bypass engine (wrapping the real engine for enroll/URI).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Rfc6238TotpService::new(),
        }
    }
}

impl TotpService for BypassTotpService {
    fn enroll(&self, account_label: &str) -> Result<TotpEnrollment, AuthError> {
        self.inner.enroll(account_label)
    }

    fn verify(&self, _secret: &str, _code: &str) -> Result<bool, AuthError> {
        // DEV BYPASS: every code is accepted. Never reachable in production
        // (gated by the exact `TOTP_BYPASS=dev` opt-in at the application edge).
        Ok(true)
    }

    fn provisioning_uri(
        &self,
        secret: &str,
        account_label: &str,
    ) -> Result<String, AuthError> {
        self.inner.provisioning_uri(secret, account_label)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::{BypassTotpService, Rfc6238TotpService};
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
    fn provisioning_uri_rebuilds_for_an_existing_secret_without_rotating() {
        // Re-deriving the URI for a stored secret must yield the SAME secret
        // embedded (no rotation) and a well-formed otpauth URI, so a user can add
        // another authenticator device for their CURRENT second factor.
        let svc = Rfc6238TotpService::new();
        let enrollment = svc.enroll("zach@example.com").expect("enroll");
        let uri = svc
            .provisioning_uri(&enrollment.secret, "zach@example.com")
            .expect("uri");
        assert!(uri.starts_with("otpauth://totp/"));
        assert!(
            uri.contains(&format!("secret={}", enrollment.secret)),
            "the re-derived URI must carry the unchanged secret, got: {uri}",
        );
    }

    #[test]
    fn bypass_accepts_any_code_but_still_enrolls_a_real_secret() {
        // DEV BYPASS: verify is short-circuited to Ok(true) for any code, while
        // enroll/provisioning_uri still delegate to the real engine (so the QR
        // screen produces a genuine secret a developer can scan to test for real).
        let bypass = BypassTotpService::new();
        let enrollment = bypass.enroll("dev@example.com").expect("enroll");
        assert!(enrollment.provisioning_uri.starts_with("otpauth://totp/"));
        assert!(
            Secret::Encoded(enrollment.secret.clone()).to_bytes().is_ok(),
            "the bypass still mints a valid base32 secret",
        );
        assert_eq!(bypass.verify(&enrollment.secret, "000000"), Ok(true));
        assert_eq!(bypass.verify("anything", "literally-anything"), Ok(true));
    }

    #[test]
    fn malformed_secret_is_engine_error() {
        let svc = Rfc6238TotpService::new();
        let err = svc.verify("!!!not-base32!!!", "123456").unwrap_err();
        assert!(matches!(err, AuthError::Totp(_)));
    }

    // ---- Adversarial / RFC-vector security tests (ORCH-NEW-PATH-TESTS-1) ------

    /// The RFC 6238 reference secret is the ASCII string "12345678901234567890"
    /// (20 bytes). Base32-encoded for `totp-rs`.
    const RFC_SECRET_ASCII: &[u8] = b"12345678901234567890";

    /// Build a TOTP with the EXACT parameters the production service uses
    /// (SHA1 / 6 digits / 30s step / skew 1), independently of the (private)
    /// `Rfc6238TotpService::build`, over a caller-supplied raw secret.
    fn rfc_totp(secret: Vec<u8>) -> TOTP {
        TOTP::new(
            Algorithm::SHA1,
            6,
            1,
            30,
            secret,
            Some("Budget Tracker".to_owned()),
            "rfc@example.com".to_owned(),
        )
        .expect("build rfc totp")
    }

    #[test]
    fn rfc6238_vectors_truncated_to_six_digits_verify() {
        // RFC 6238 Appendix B publishes 8-digit TOTPs for SHA1 at fixed unix
        // times. A 6-digit code is the low 6 digits of the same 8-digit value
        // (the truncation is `binary mod 10^digits`). We compute the 8-digit RFC
        // values with an 8-digit TOTP and assert our 6-digit TOTP yields the
        // matching low six digits AND that `check` accepts them at that time.
        // This proves our engine implements RFC 6238 truncation, not a lookalike.
        let totp6 = rfc_totp(RFC_SECRET_ASCII.to_vec());
        let totp8 = TOTP::new(
            Algorithm::SHA1,
            8,
            1,
            30,
            RFC_SECRET_ASCII.to_vec(),
            Some("Budget Tracker".to_owned()),
            "rfc@example.com".to_owned(),
        )
        .expect("build 8-digit");

        // (unix time, RFC 6238 Appendix B expected SHA1 8-digit TOTP)
        let vectors: [(u64, &str); 3] = [
            (59, "94287082"),
            (1_111_111_109, "07081804"),
            (1_111_111_111, "14050471"),
        ];
        for (time, expected8) in vectors {
            assert_eq!(
                totp8.generate(time),
                expected8,
                "RFC 6238 8-digit vector mismatch at t={time}",
            );
            let expected6 = &expected8[2..]; // low six digits
            assert_eq!(
                totp6.generate(time),
                expected6,
                "6-digit truncation mismatch at t={time}",
            );
            assert!(
                totp6.check(expected6, time),
                "the RFC-derived 6-digit code must verify at its own time",
            );
        }
    }

    #[test]
    fn code_from_a_far_off_time_does_not_verify() {
        // A code generated for one time window must not verify in a window far
        // outside the skew tolerance (an "expired" code).
        let totp = rfc_totp(RFC_SECRET_ASCII.to_vec());
        let t0 = 1_111_111_109u64;
        let old_code = totp.generate(t0);
        // 10 minutes later is 20 steps away — well beyond the ±1-step skew.
        assert!(
            !totp.check(&old_code, t0 + 600),
            "a code 10 minutes stale must be rejected (expired)",
        );
    }

    #[test]
    fn skew_window_edges_are_exactly_plus_or_minus_one_step() {
        // The service is configured with ±1 step of skew. A code from the
        // immediately adjacent windows (±30s) must verify; a code two steps away
        // (±60s) must NOT. This pins the tolerance to exactly one step.
        let totp = rfc_totp(RFC_SECRET_ASCII.to_vec());
        let now = 1_700_000_000u64; // an arbitrary fixed instant
        let prev = totp.generate(now - 30);
        let next = totp.generate(now + 30);
        let two_back = totp.generate(now - 60);
        let two_fwd = totp.generate(now + 60);

        assert!(
            totp.check(&prev, now),
            "the previous window must be in skew"
        );
        assert!(totp.check(&next, now), "the next window must be in skew");
        // Two steps away is outside ±1; reject (unless a digit collision, which we
        // guard against below by also asserting they differ from the in-window
        // codes when the underlying step value differs).
        if two_back != prev && two_back != totp.generate(now) {
            assert!(
                !totp.check(&two_back, now),
                "a code two steps in the past must be rejected",
            );
        }
        if two_fwd != next && two_fwd != totp.generate(now) {
            assert!(
                !totp.check(&two_fwd, now),
                "a code two steps in the future must be rejected",
            );
        }
    }

    #[test]
    fn service_verifies_a_code_it_generated_and_rejects_neighbours_secret() {
        // End-to-end through the service: a code derived from the enrolled secret
        // verifies; the same code against a DIFFERENT secret does not.
        let svc = Rfc6238TotpService::new();
        let a = svc.enroll("a@example.com").expect("enroll a");
        let b = svc.enroll("b@example.com").expect("enroll b");
        assert_ne!(a.secret, b.secret, "fresh enrollments differ");

        let a_bytes = Secret::Encoded(a.secret.clone())
            .to_bytes()
            .expect("decode");
        let code = rfc_totp(a_bytes).generate_current().expect("gen");
        assert_eq!(svc.verify(&a.secret, &code), Ok(true));
        // A code minted for A's secret must not verify against B's secret.
        assert_eq!(
            svc.verify(&b.secret, &code),
            Ok(false),
            "a code is bound to its own secret",
        );
    }

    #[test]
    fn non_numeric_and_wrong_length_codes_are_rejected_cleanly() {
        // Garbage codes are a normal negative result (Ok(false)), never a panic or
        // an engine error: the secret is valid, only the code is wrong.
        let svc = Rfc6238TotpService::new();
        let e = svc.enroll("z@example.com").expect("enroll");
        for code in ["", "abcdef", "12345", "1234567", "      "] {
            assert_eq!(
                svc.verify(&e.secret, code),
                Ok(false),
                "malformed code {code:?} must be a clean Ok(false)",
            );
        }
    }
}
