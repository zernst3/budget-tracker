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
}
