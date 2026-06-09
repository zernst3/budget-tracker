//! Argon2id password hashing (`SPEC §9.1`, `BUDGET-AUTH-GATE-1`).
//!
//! Concrete implementation of the domain [`PasswordHasher`] port. Argon2id is
//! the memory-hard, side-channel-resistant default recommended by OWASP and the
//! Argon2 RFC for password storage. The produced hash is a self-describing PHC
//! string (`$argon2id$v=19$m=...,t=...,p=...$salt$hash`) stored verbatim in
//! `users.password_hash`; verification re-derives the parameters from the stored
//! string, so a future parameter bump does not invalidate existing hashes.
//!
//! Hashing is CPU-bound and deliberately slow; callers in an async context
//! offload it to a blocking pool at the service boundary
//! ([`crate::auth::AuthService`]), never block the reactor inside the domain.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier, SaltString};

use budget_domain::auth::{AuthError, PasswordHasher};

/// Argon2id-backed [`PasswordHasher`].
///
/// Uses the `argon2` crate defaults (Argon2id, v19, the crate's OWASP-aligned
/// memory/time/parallelism cost parameters). Stateless: a fresh per-hash salt is
/// drawn from the OS CSPRNG ([`OsRng`]).
#[derive(Debug, Default, Clone)]
pub struct Argon2idHasher {
    argon2: Argon2<'static>,
}

impl Argon2idHasher {
    /// Construct the hasher with the recommended Argon2id default parameters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            argon2: Argon2::default(),
        }
    }
}

impl PasswordHasher for Argon2idHasher {
    fn hash(&self, plaintext: &str) -> Result<String, AuthError> {
        let salt = SaltString::generate(&mut OsRng);
        self.argon2
            .hash_password(plaintext.as_bytes(), &salt)
            // map_err carries only the engine's own (non-secret) error string;
            // never the plaintext or the resulting hash (BUDGET-PLAID-TOKEN-VAULT-1
            // spirit: no secret material in errors/logs).
            .map(|h| h.to_string())
            .map_err(|e| AuthError::PasswordHashing(e.to_string()))
    }

    fn verify(&self, plaintext: &str, stored_hash: &str) -> Result<bool, AuthError> {
        // A malformed stored hash is an engine error (PasswordHashing), distinct
        // from a wrong password (Ok(false)).
        let parsed = PasswordHash::new(stored_hash)
            .map_err(|e| AuthError::PasswordHashing(e.to_string()))?;
        match self.argon2.verify_password(plaintext.as_bytes(), &parsed) {
            Ok(()) => Ok(true),
            Err(argon2::password_hash::Error::Password) => Ok(false),
            Err(e) => Err(AuthError::PasswordHashing(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]

    use super::Argon2idHasher;
    use budget_domain::auth::{AuthError, PasswordHasher};

    #[test]
    fn hash_then_verify_roundtrips() {
        let h = Argon2idHasher::new();
        let hash = h.hash("correct horse battery staple").expect("hash");
        // The stored hash is a PHC argon2id string, never the plaintext.
        assert!(
            hash.starts_with("$argon2id$"),
            "expected argon2id PHC string"
        );
        assert!(
            !hash.contains("correct horse"),
            "plaintext leaked into hash"
        );
        assert!(
            h.verify("correct horse battery staple", &hash)
                .expect("verify"),
            "correct password must verify",
        );
    }

    #[test]
    fn wrong_password_is_false_not_error() {
        let h = Argon2idHasher::new();
        let hash = h.hash("s3cret").expect("hash");
        // A wrong password is a normal negative result, NOT an engine error.
        assert_eq!(h.verify("wrong", &hash), Ok(false));
    }

    #[test]
    fn distinct_salts_produce_distinct_hashes() {
        let h = Argon2idHasher::new();
        let a = h.hash("same").expect("hash a");
        let b = h.hash("same").expect("hash b");
        assert_ne!(a, b, "per-hash random salt must make hashes differ");
        // Both still verify against the same plaintext.
        assert_eq!(h.verify("same", &a), Ok(true));
        assert_eq!(h.verify("same", &b), Ok(true));
    }

    #[test]
    fn malformed_stored_hash_is_engine_error() {
        let h = Argon2idHasher::new();
        let err = h.verify("whatever", "not-a-phc-string").unwrap_err();
        assert!(matches!(err, AuthError::PasswordHashing(_)));
    }

    // ---- Adversarial / independent security tests (ORCH-NEW-PATH-TESTS-1) ----

    #[test]
    fn empty_password_hashes_and_only_empty_verifies() {
        // An empty password is still a value the engine handles deterministically:
        // it hashes, the hash is not the plaintext, and only the empty string
        // verifies against it (a non-empty guess must be Ok(false), not an error).
        let h = Argon2idHasher::new();
        let hash = h.hash("").expect("hash empty");
        assert!(hash.starts_with("$argon2id$"));
        assert_eq!(h.verify("", &hash), Ok(true), "empty must verify");
        assert_eq!(h.verify("x", &hash), Ok(false), "non-empty must not verify");
    }

    #[test]
    fn empty_guess_against_real_password_is_false() {
        // The inverse: an empty guess must NOT authenticate a real password.
        let h = Argon2idHasher::new();
        let hash = h.hash("a-real-password").expect("hash");
        assert_eq!(h.verify("", &hash), Ok(false));
    }

    #[test]
    fn hash_never_contains_plaintext() {
        // The PHC string carries only algorithm params + salt + derived hash;
        // the plaintext must never appear, even as a substring, for several inputs.
        let h = Argon2idHasher::new();
        for plaintext in ["hunter2", "P@ssw0rd!longer-secret-value", "café-ünïçødé"] {
            let hash = h.hash(plaintext).expect("hash");
            assert!(
                !hash.contains(plaintext),
                "plaintext {plaintext:?} leaked into the stored hash",
            );
        }
    }

    #[test]
    fn argon2_parameters_are_non_trivial() {
        // The default Argon2id cost must be memory-hard, not a degenerate config.
        // Parse the PHC string and assert m (memory KiB), t (iterations), and p
        // (parallelism) meet a meaningful floor. OWASP's minimum for Argon2id is
        // m>=19456 (19 MiB), t>=2, p>=1; the argon2 crate default meets this.
        let h = Argon2idHasher::new();
        let hash = h.hash("params-check").expect("hash");
        let parsed = argon2::password_hash::PasswordHash::new(&hash).expect("parse phc");
        let params = argon2::Params::try_from(&parsed).expect("params");
        assert!(
            params.m_cost() >= 19_456,
            "memory cost too low: {} KiB (want >= 19456)",
            params.m_cost(),
        );
        assert!(
            params.t_cost() >= 2,
            "iteration count too low: {}",
            params.t_cost(),
        );
        assert!(params.p_cost() >= 1, "parallelism must be >= 1");
        // The algorithm + version must be Argon2id v19 (not argon2i / argon2d).
        assert_eq!(parsed.algorithm.as_str(), "argon2id");
        assert_eq!(parsed.version, Some(0x13), "must be Argon2 v19 (0x13)");
    }

    #[test]
    fn verifying_a_hash_made_for_a_different_password_is_false_not_error() {
        // Two distinct passwords; the hash of one must not verify the other, and
        // the mismatch is a clean Ok(false) (not leaked as an engine error).
        let h = Argon2idHasher::new();
        let hash_a = h.hash("alpha").expect("hash a");
        assert_eq!(h.verify("bravo", &hash_a), Ok(false));
        // Case sensitivity: a case variant must not verify.
        assert_eq!(h.verify("Alpha", &hash_a), Ok(false));
        // Trailing whitespace must not verify (no silent trimming).
        assert_eq!(h.verify("alpha ", &hash_a), Ok(false));
    }

    #[test]
    fn corrupt_stored_hash_never_authenticates() {
        // A truncated / corrupt stored hash must NEVER authenticate. Depending on
        // where the corruption lands it either fails to parse (typed engine error)
        // or parses but the derived hash mismatches (Ok(false)); the load-bearing
        // security invariant is that it is never Ok(true) for the right password.
        let h = Argon2idHasher::new();
        let full = h.hash("secret").expect("hash");
        // Several truncation points, plus a single-byte flip in the hash segment.
        let mut flipped = full.clone().into_bytes();
        let last = flipped.len() - 1;
        flipped[last] ^= 0x01;
        let flipped = String::from_utf8(flipped).expect("utf8");

        for corrupt in [
            &full[..full.len() / 2],
            &full[..full.len() - 3],
            flipped.as_str(),
            "not-a-phc-string",
            "$argon2id$",
        ] {
            match h.verify("secret", corrupt) {
                Ok(true) => panic!("corrupt hash {corrupt:?} authenticated the password!"),
                // A clean rejection (Ok(false)) or a typed engine error are both
                // acceptable: the invariant is only that it never authenticates.
                Ok(false) | Err(AuthError::PasswordHashing(_)) => {}
                Err(other) => panic!("unexpected error variant for {corrupt:?}: {other:?}"),
            }
        }
    }
}
