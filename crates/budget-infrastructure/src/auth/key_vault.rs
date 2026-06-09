//! Azure Key Vault secret-vault client (`BUDGET-PLAID-TOKEN-VAULT-1`).
//!
//! Concrete implementation of the domain [`SecretVault`] port. Reads a secret by
//! name from Azure Key Vault using **managed-identity** authentication (the
//! Container App's user-assigned identity; the Terraform Key Vault + identity
//! wiring is stubbed in build step 1). Used for the DB connection string and,
//! in build step 8, the Plaid access token — which is stored ONLY as a Key Vault
//! reference, never raw in the DB (`BUDGET-PLAID-TOKEN-VAULT-1`).
//!
//! ## Fail-safe + no secret logging
//!
//! Every failure path maps to the typed [`AuthError::SecretVault`] carrying only
//! a non-sensitive description (the Azure error category / message). The secret
//! VALUE is never written to a log, an error, or telemetry — neither here nor by
//! callers (`BUDGET-PLAID-TOKEN-VAULT-1`). A missing secret, a denied identity,
//! or an unreachable vault all surface the same typed error rather than panicking
//! or returning a partial/empty value.

use std::sync::Arc;

use azure_core::credentials::TokenCredential;
use azure_identity::ManagedIdentityCredential;
use azure_security_keyvault_secrets::SecretClient;

use async_trait::async_trait;
use budget_domain::auth::{AuthError, SecretVault};

/// Azure Key Vault-backed [`SecretVault`].
///
/// Holds a configured [`SecretClient`] bound to a single vault endpoint and a
/// managed-identity credential.
pub struct AzureKeyVault {
    client: SecretClient,
}

impl AzureKeyVault {
    /// Build the client against `vault_url` (e.g.
    /// `https://budget-portal-kv-prod.vault.azure.net/`) using the ambient
    /// **system-assigned managed identity**.
    ///
    /// # Errors
    /// [`AuthError::SecretVault`] if the managed-identity credential cannot be
    /// constructed or the client cannot bind to the endpoint.
    pub fn new(vault_url: &str) -> Result<Self, AuthError> {
        let credential = ManagedIdentityCredential::new(None)
            .map_err(|e| AuthError::SecretVault(format!("managed identity: {e}")))?;
        Self::with_credential(vault_url, credential)
    }

    /// Build the client against `vault_url` with an explicit credential. Lets a
    /// user-assigned identity (or, in tests, a fake credential) be supplied.
    ///
    /// # Errors
    /// [`AuthError::SecretVault`] if the client cannot bind to the endpoint.
    pub fn with_credential(
        vault_url: &str,
        credential: Arc<dyn TokenCredential>,
    ) -> Result<Self, AuthError> {
        let client = SecretClient::new(vault_url, credential, None)
            .map_err(|e| AuthError::SecretVault(format!("client init: {e}")))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl SecretVault for AzureKeyVault {
    async fn get_secret(&self, name: &str) -> Result<String, AuthError> {
        let response = self
            .client
            .get_secret(name, None)
            .await
            // The Azure error message never contains the secret value (it is the
            // request/transport error). Map to the typed fail-safe error.
            .map_err(|e| AuthError::SecretVault(format!("get_secret: {e}")))?;
        let secret = response
            .into_model()
            .map_err(|e| AuthError::SecretVault(format!("read body: {e}")))?;
        secret
            .value
            // A secret with no value is an operational fault, not an empty secret;
            // fail safe rather than handing back an empty string.
            .ok_or_else(|| AuthError::SecretVault("secret has no value".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::AzureKeyVault;

    // A live Key Vault read needs an Azure identity + a real vault, neither of
    // which exists in CI (SPEC §12: Zach provisions Azure out of band). These
    // tests assert the *construction* + fail-safe surface without a network call.

    #[test]
    fn rejects_non_http_endpoint() {
        // SecretClient::new rejects a non-http(s) endpoint; the error maps to the
        // typed SecretVault variant (fail-safe construction).
        let result = AzureKeyVault::new("ftp://not-a-vault");
        // Either the managed-identity credential or the endpoint validation
        // fails; both surface as the typed error, never a panic.
        assert!(result.is_err(), "a non-http(s) endpoint must be rejected");
    }
}
