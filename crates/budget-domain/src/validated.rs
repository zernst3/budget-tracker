//! Validated newtype strings (`DOMAIN-3`).
//!
//! Strings that carry validation semantics are wrapped in newtypes with a
//! fallible [`try_new`] constructor. Anywhere the type appears, the value has
//! already passed validation — the type is the proof. Free-form content
//! (transaction descriptions, budget names) stays as `String`.
//!
//! Applied here to: [`Email`] (the one login identity), [`AccessTokenRef`] (the
//! Key Vault secret reference, `BUDGET-PLAID-TOKEN-VAULT-1` — validated to make
//! it impossible to accidentally store a raw token where a reference belongs).

use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// A validated email address (the single-user login identity, `SPEC §9`).
///
/// Validation is intentionally minimal: non-empty, contains exactly the shape
/// `local@domain` with a dot in the domain. Full RFC 5322 validation is not the
/// point — the point is that an `Email`-typed value is never an obviously-broken
/// string, and the check lives in exactly one place.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Email(String);

impl Email {
    const MAX_LEN: usize = 254;

    /// Construct a validated [`Email`].
    ///
    /// # Errors
    /// - [`ValidationError::Empty`] if `raw` is blank.
    /// - [`ValidationError::TooLong`] if `raw` exceeds 254 characters.
    /// - [`ValidationError::Format`] if `raw` is not `local@domain.tld`-shaped.
    pub fn try_new(raw: &str) -> Result<Self, ValidationError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ValidationError::Empty { field: "email" });
        }
        if trimmed.len() > Self::MAX_LEN {
            return Err(ValidationError::TooLong {
                field: "email",
                max: Self::MAX_LEN,
                actual: trimmed.len(),
            });
        }
        let parts: Vec<&str> = trimmed.split('@').collect();
        let valid = parts.len() == 2
            && !parts[0].is_empty()
            && parts[1].contains('.')
            && !parts[1].starts_with('.')
            && !parts[1].ends_with('.');
        if !valid {
            return Err(ValidationError::Format {
                field: "email",
                reason: "expected local@domain.tld".to_string(),
            });
        }
        Ok(Email(trimmed.to_string()))
    }

    /// The underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the owned [`String`] (for the persistence boundary).
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for Email {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A reference to a secret in Azure Key Vault — NEVER a raw secret value
/// (`BUDGET-PLAID-TOKEN-VAULT-1`).
///
/// Wrapping the reference in its own type makes it a typed concept the compiler
/// keeps distinct from a raw Plaid `access_token` string: a function that takes
/// an `AccessTokenRef` cannot be handed a raw token by mistake, and the
/// resolution step that fetches the real token from the vault is the only place
/// a raw token ever exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccessTokenRef(String);

impl AccessTokenRef {
    const MAX_LEN: usize = 512;

    /// Construct a validated [`AccessTokenRef`].
    ///
    /// # Errors
    /// - [`ValidationError::Empty`] if `raw` is blank.
    /// - [`ValidationError::TooLong`] if `raw` exceeds 512 characters.
    pub fn try_new(raw: &str) -> Result<Self, ValidationError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ValidationError::Empty {
                field: "access_token_ref",
            });
        }
        if trimmed.len() > Self::MAX_LEN {
            return Err(ValidationError::TooLong {
                field: "access_token_ref",
                max: Self::MAX_LEN,
                actual: trimmed.len(),
            });
        }
        Ok(AccessTokenRef(trimmed.to_string()))
    }

    /// The underlying reference string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the owned [`String`] (for the persistence boundary).
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for AccessTokenRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_email() {
        // The lint config denies unwrap/expect/panic even in tests, so we compare
        // the whole Result (mapped to the inner str) instead of unwrapping.
        assert_eq!(
            Email::try_new("  zach@example.com ").map(|e| e.as_str().to_string()),
            Ok("zach@example.com".to_string())
        );
    }

    #[test]
    fn rejects_empty_email() {
        assert_eq!(
            Email::try_new("   "),
            Err(ValidationError::Empty { field: "email" })
        );
    }

    #[test]
    fn rejects_email_without_at() {
        assert!(matches!(
            Email::try_new("zach.example.com"),
            Err(ValidationError::Format { field: "email", .. })
        ));
    }

    #[test]
    fn rejects_email_without_domain_dot() {
        assert!(matches!(
            Email::try_new("zach@localhost"),
            Err(ValidationError::Format { field: "email", .. })
        ));
    }

    #[test]
    fn access_token_ref_rejects_empty() {
        assert_eq!(
            AccessTokenRef::try_new(""),
            Err(ValidationError::Empty {
                field: "access_token_ref"
            })
        );
    }

    #[test]
    fn access_token_ref_round_trips() {
        assert_eq!(
            AccessTokenRef::try_new("kv://plaid/boa-item-1").map(|r| r.as_str().to_string()),
            Ok("kv://plaid/boa-item-1".to_string())
        );
    }
}
