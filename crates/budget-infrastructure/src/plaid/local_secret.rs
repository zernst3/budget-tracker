//! A local-dev, in-memory [`SecretVault`] (`STAGE-1` local testing).
//!
//! This is the local-secret-store half of the `PLAID_MODE=mock` opt-in: it lets
//! the Pull path (which fetches the Plaid access token from the vault BEFORE
//! `transactions_sync`) resolve WITHOUT Azure Key Vault, so the whole app runs
//! locally with no Azure dependency.
//!
//! ## OFF by default — opt-in only (`STAGE-1` safety)
//!
//! Selected ONLY alongside [`MockPlaidApi`](super::MockPlaidApi) under
//! `PLAID_MODE=mock` (budget-ui's `server_state`). With the env var unset/other,
//! the real [`AzureKeyVault`](crate::auth::AzureKeyVault) remains the
//! default/production path. A misconfigured prod can never silently use this
//! in-memory store.
//!
//! ## Behavior
//!
//! - [`get_secret`](SecretVault::get_secret): returns whatever was previously
//!   written for `name`; if `name` was never written (the common case — the
//!   `PlaidItem` row's `access_token_ref` was seeded directly, not via an
//!   `exchange_and_link` run), it returns the deterministic
//!   [`MOCK_ACCESS_TOKEN`](super::MOCK_ACCESS_TOKEN). The mock Plaid client
//!   ignores the token value, so any non-empty token unblocks the Pull.
//! - [`set_secret`](SecretVault::set_secret): records the write in memory so a
//!   subsequent `get_secret(name)` reads it back (mirrors the real vault for a
//!   local `exchange_and_link` flow).
//!
//! It never touches the network and never logs a secret value
//! (`BUDGET-PLAID-TOKEN-VAULT-1` spirit), though every value here is a fake.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use budget_domain::auth::{AuthError, SecretVault};

use super::MOCK_ACCESS_TOKEN;

/// In-memory [`SecretVault`] for local mock-mode runs.
///
/// Reads fall back to the deterministic [`MOCK_ACCESS_TOKEN`] so a seeded
/// `PlaidItem` (whose `access_token_ref` was never written through this store)
/// still resolves on Pull.
#[derive(Debug, Default)]
pub struct InMemorySecretVault {
    store: Mutex<HashMap<String, String>>,
}

impl InMemorySecretVault {
    /// Build an empty in-memory vault. Reads of never-written names fall back to
    /// the deterministic mock access token.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl SecretVault for InMemorySecretVault {
    async fn get_secret(&self, name: &str) -> Result<String, AuthError> {
        let store = self
            .store
            .lock()
            .map_err(|_| AuthError::SecretVault("in-memory vault mutex poisoned".to_owned()))?;
        // A previously-written value wins; otherwise hand back the deterministic
        // mock token so a seeded PlaidItem (no exchange run) still pulls.
        Ok(store
            .get(name)
            .cloned()
            .unwrap_or_else(|| MOCK_ACCESS_TOKEN.to_owned()))
    }

    async fn set_secret(&self, name: &str, value: &str) -> Result<(), AuthError> {
        self.store
            .lock()
            .map_err(|_| AuthError::SecretVault("in-memory vault mutex poisoned".to_owned()))?
            .insert(name.to_owned(), value.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn unknown_name_falls_back_to_mock_token() {
        let vault = InMemorySecretVault::new();
        let token = vault
            .get_secret("plaid-access-token-some-unseen-item")
            .await
            .unwrap();
        assert_eq!(
            token, MOCK_ACCESS_TOKEN,
            "a never-written name resolves to the deterministic mock token"
        );
    }

    #[tokio::test]
    async fn written_value_reads_back() {
        let vault = InMemorySecretVault::new();
        vault
            .set_secret("plaid-access-token-mock-item", "written-token")
            .await
            .unwrap();
        let token = vault
            .get_secret("plaid-access-token-mock-item")
            .await
            .unwrap();
        assert_eq!(
            token, "written-token",
            "a written value wins over the fallback"
        );
    }
}
