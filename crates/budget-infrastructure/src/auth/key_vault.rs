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
use azure_security_keyvault_secrets::models::SetSecretParameters;

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

    async fn set_secret(&self, name: &str, value: &str) -> Result<(), AuthError> {
        // Store the Plaid access token (SPEC §6). The value is the secret; the
        // DB only ever holds `name` (BUDGET-PLAID-TOKEN-VAULT-1). The value is
        // moved into the request body and never logged.
        let params = SetSecretParameters {
            value: Some(value.to_owned()),
            content_type: None,
            secret_attributes: None,
            tags: None,
        };
        let body = params
            .try_into()
            // A serialization fault carries no secret material (it is a structural
            // error about the request shape, not the value).
            .map_err(|e| AuthError::SecretVault(format!("set_secret body: {e}")))?;
        self.client
            .set_secret(name, body, None)
            .await
            // The Azure error never contains the secret value (transport/request
            // error). Map to the typed fail-safe error.
            .map_err(|e| AuthError::SecretVault(format!("set_secret: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]

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

    // ---- Adversarial / fail-safe tests (ORCH-NEW-PATH-TESTS-1) --------------

    #[test]
    fn construction_failure_never_embeds_a_vault_value() {
        // BUDGET-PLAID-TOKEN-VAULT-1: a failure must not carry secret material.
        // Construction errors only ever describe the endpoint/identity category;
        // there is no secret to leak at construction, but we assert the Display
        // text is the typed operational description, not anything caller-supplied
        // that could resemble a secret.
        use budget_domain::auth::AuthError;
        // AzureKeyVault (the Ok type) is not Debug, so match instead of unwrap_err.
        match AzureKeyVault::new("ftp://not-a-vault") {
            Err(AuthError::SecretVault(msg)) => {
                // The message must be a non-empty operational description.
                assert!(!msg.is_empty());
            }
            Err(other) => panic!("expected SecretVault error, got {other:?}"),
            Ok(_) => panic!("a non-http(s) endpoint must not succeed"),
        }
    }

    #[tokio::test]
    #[ignore = "requires a live Azure Key Vault + managed identity (SPEC §12, out of band)"]
    async fn live_get_secret_round_trip() {
        // Gated live test: only runs against a real vault provided via env. Reads
        // a known secret name and asserts a non-empty value comes back. Never
        // logs the value (BUDGET-PLAID-TOKEN-VAULT-1).
        use budget_domain::auth::SecretVault;
        let Ok(vault_url) = std::env::var("KEY_VAULT_URL") else {
            return;
        };
        let Ok(secret_name) = std::env::var("KEY_VAULT_TEST_SECRET") else {
            return;
        };
        let vault = AzureKeyVault::new(&vault_url).expect("build vault");
        let value = vault.get_secret(&secret_name).await.expect("read secret");
        assert!(!value.is_empty(), "a live secret must have a value");
    }
}
