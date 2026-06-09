//! The reqwest-based Plaid HTTP client (`SPEC §6`).
//!
//! Implements the domain [`PlaidApi`] port against the real Plaid REST API.
//! Three endpoints: `/link/token/create`, `/item/public_token/exchange`, and
//! `/transactions/sync`.
//!
//! ## Security invariants
//!
//! - **Transactions(+Accounts) only; Transfer NEVER enabled** (`SPEC §6`): every
//!   link-token request is run through
//!   [`LinkTokenRequest::assert_no_money_movement`] before a byte goes to Plaid,
//!   so the resulting `access_token` is physically unable to move money.
//! - **The `access_token` is never logged** (`BUDGET-PLAID-TOKEN-VAULT-1`). It is
//!   placed in the JSON body and dropped; no `tracing` line, error, or telemetry
//!   carries it. Plaid's own client-id/secret live only in [`PlaidCredentials`]
//!   and are likewise never logged.

use async_trait::async_trait;
use chrono::NaiveDate;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use budget_domain::plaid_api::{
    AccessTokenExchange, LinkToken, LinkTokenRequest, PlaidAccount, PlaidApi, PlaidError,
    PlaidSyncPage, PlaidTransaction,
};

/// Which Plaid environment to target (`SPEC §6`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaidEnvironment {
    /// `https://sandbox.plaid.com` — the free test tier (no real bank).
    Sandbox,
    /// `https://production.plaid.com` — real banks (needs Plaid approval).
    Production,
}

impl PlaidEnvironment {
    /// The base URL for this environment.
    #[must_use]
    pub const fn base_url(self) -> &'static str {
        match self {
            PlaidEnvironment::Sandbox => "https://sandbox.plaid.com",
            PlaidEnvironment::Production => "https://production.plaid.com",
        }
    }
}

/// Plaid API credentials (`SPEC §6`). The `secret` is sensitive and is never
/// logged; it is read from Key Vault at startup, not hard-coded.
#[derive(Clone)]
pub struct PlaidCredentials {
    /// The Plaid `client_id`.
    pub client_id: String,
    /// The Plaid `secret` (per-environment). Sensitive.
    pub secret: String,
}

/// The reqwest-backed [`PlaidApi`] client.
pub struct HttpPlaidApi {
    http: Client,
    credentials: PlaidCredentials,
    environment: PlaidEnvironment,
}

impl HttpPlaidApi {
    /// Build the client for `environment` with `credentials`.
    #[must_use]
    pub fn new(http: Client, credentials: PlaidCredentials, environment: PlaidEnvironment) -> Self {
        Self {
            http,
            credentials,
            environment,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.environment.base_url())
    }

    /// POST a JSON body to a Plaid path and deserialize the response.
    ///
    /// The Plaid `client_id` + `secret` are injected at the wire layer
    /// (`PlaidAuthEnvelope`) and never logged; the response error path carries
    /// only the HTTP status / Plaid error category (no token material).
    async fn post<B: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: B,
    ) -> Result<R, PlaidError> {
        let envelope = PlaidAuthEnvelope {
            client_id: &self.credentials.client_id,
            secret: &self.credentials.secret,
            body,
        };
        let response = self
            .http
            .post(self.url(path))
            .json(&envelope)
            .send()
            .await
            .map_err(|e| PlaidError::Api(format!("{path}: transport: {}", redact(&e))))?;

        let status = response.status();
        if !status.is_success() {
            // Read the Plaid error_code/error_type if present (non-sensitive).
            let detail = response.json::<PlaidApiError>().await.map_or_else(
                |_| "unparseable error body".to_owned(),
                |e| format!("{}/{}", e.error_type, e.error_code),
            );
            return Err(PlaidError::Api(format!("{path}: {status}: {detail}")));
        }
        response
            .json::<R>()
            .await
            .map_err(|e| PlaidError::Api(format!("{path}: decode: {}", redact(&e))))
    }
}

/// Strip any chance of a secret leaking via a reqwest error's URL/body echo.
/// reqwest errors can include the request URL; tokens live in the body, never the
/// URL, but we defensively keep the description to the error kind only.
fn redact(e: &reqwest::Error) -> String {
    // Describe the failure category without echoing any request body.
    if e.is_timeout() {
        "timeout".to_owned()
    } else if e.is_connect() {
        "connect".to_owned()
    } else if e.is_decode() {
        "decode".to_owned()
    } else {
        "request".to_owned()
    }
}

// ---------------------------------------------------------------------------
// Wire envelopes (request)
// ---------------------------------------------------------------------------

/// Every Plaid request carries `client_id` + `secret` alongside the call body.
#[derive(Serialize)]
struct PlaidAuthEnvelope<'a, B: Serialize> {
    client_id: &'a str,
    secret: &'a str,
    #[serde(flatten)]
    body: B,
}

#[derive(Serialize)]
struct LinkTokenCreateBody<'a> {
    user: LinkUser<'a>,
    client_name: &'a str,
    products: Vec<&'a str>,
    country_codes: Vec<&'a str>,
    language: &'a str,
}

#[derive(Serialize)]
struct LinkUser<'a> {
    client_user_id: &'a str,
}

#[derive(Serialize)]
struct PublicTokenExchangeBody<'a> {
    public_token: &'a str,
}

#[derive(Serialize)]
struct TransactionsSyncBody<'a> {
    access_token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Wire envelopes (response)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LinkTokenCreateResponse {
    link_token: String,
}

#[derive(Deserialize)]
struct PublicTokenExchangeResponse {
    access_token: String,
    item_id: String,
}

#[derive(Deserialize)]
struct TransactionsSyncResponse {
    added: Vec<WireTransaction>,
    modified: Vec<WireTransaction>,
    removed: Vec<WireRemoved>,
    #[serde(default)]
    accounts: Vec<WireAccount>,
    next_cursor: String,
    has_more: bool,
}

#[derive(Deserialize)]
struct WireTransaction {
    transaction_id: String,
    account_id: String,
    amount: Decimal,
    date: NaiveDate,
    name: String,
    pending: bool,
    #[serde(default)]
    pending_transaction_id: Option<String>,
}

impl From<WireTransaction> for PlaidTransaction {
    fn from(w: WireTransaction) -> Self {
        PlaidTransaction {
            transaction_id: w.transaction_id,
            account_id: w.account_id,
            amount: w.amount,
            date: w.date,
            name: w.name,
            pending: w.pending,
            pending_transaction_id: w.pending_transaction_id,
        }
    }
}

#[derive(Deserialize)]
struct WireRemoved {
    transaction_id: String,
}

#[derive(Deserialize)]
struct WireAccount {
    account_id: String,
    name: String,
    #[serde(rename = "type", default)]
    account_type: String,
}

impl From<WireAccount> for PlaidAccount {
    fn from(w: WireAccount) -> Self {
        PlaidAccount {
            account_id: w.account_id,
            name: w.name,
            account_type: w.account_type,
        }
    }
}

#[derive(Deserialize)]
struct PlaidApiError {
    #[serde(default)]
    error_type: String,
    #[serde(default)]
    error_code: String,
}

// ---------------------------------------------------------------------------
// Port impl
// ---------------------------------------------------------------------------

#[async_trait]
impl PlaidApi for HttpPlaidApi {
    async fn create_link_token(&self, request: &LinkTokenRequest) -> Result<LinkToken, PlaidError> {
        // Refuse any money-movement product BEFORE we ever call Plaid (SPEC §6).
        request.assert_no_money_movement()?;

        let products: Vec<&str> = request.products.iter().map(|p| p.as_plaid_str()).collect();
        let body = LinkTokenCreateBody {
            user: LinkUser {
                client_user_id: &request.user_id.value().to_string(),
            },
            client_name: "Budget Tracker",
            products,
            country_codes: vec!["US"],
            language: "en",
        };
        let resp: LinkTokenCreateResponse = self.post("/link/token/create", body).await?;
        Ok(LinkToken(resp.link_token))
    }

    async fn exchange_public_token(
        &self,
        public_token: &str,
    ) -> Result<AccessTokenExchange, PlaidError> {
        let body = PublicTokenExchangeBody { public_token };
        let resp: PublicTokenExchangeResponse =
            self.post("/item/public_token/exchange", body).await?;
        // The access_token is secret: returned to the caller to write to the vault,
        // never logged here (BUDGET-PLAID-TOKEN-VAULT-1).
        Ok(AccessTokenExchange {
            access_token: resp.access_token,
            plaid_item_id: resp.item_id,
        })
    }

    async fn transactions_sync(
        &self,
        access_token: &str,
        cursor: Option<&str>,
    ) -> Result<PlaidSyncPage, PlaidError> {
        let body = TransactionsSyncBody {
            access_token,
            cursor,
        };
        let resp: TransactionsSyncResponse = self.post("/transactions/sync", body).await?;
        Ok(PlaidSyncPage {
            added: resp.added.into_iter().map(Into::into).collect(),
            modified: resp.modified.into_iter().map(Into::into).collect(),
            removed: resp.removed.into_iter().map(|r| r.transaction_id).collect(),
            accounts: resp.accounts.into_iter().map(Into::into).collect(),
            next_cursor: resp.next_cursor,
            has_more: resp.has_more,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use budget_domain::ids::UserId;
    use budget_domain::plaid_api::PlaidProduct;
    use uuid::Uuid;

    fn client() -> HttpPlaidApi {
        HttpPlaidApi::new(
            Client::new(),
            PlaidCredentials {
                client_id: "test-client".to_owned(),
                secret: "test-secret".to_owned(),
            },
            PlaidEnvironment::Sandbox,
        )
    }

    #[test]
    fn base_url_per_environment() {
        assert_eq!(
            PlaidEnvironment::Sandbox.base_url(),
            "https://sandbox.plaid.com"
        );
        assert_eq!(
            PlaidEnvironment::Production.base_url(),
            "https://production.plaid.com"
        );
    }

    #[tokio::test]
    async fn create_link_token_refuses_money_movement_before_calling_plaid() {
        // A Transfer product must be refused locally — no network call (SPEC §6).
        let api = client();
        let request = LinkTokenRequest {
            user_id: UserId::new(Uuid::new_v4()),
            products: vec![PlaidProduct::Transactions, PlaidProduct::Transfer],
        };
        let result = api.create_link_token(&request).await;
        assert!(
            matches!(result, Err(PlaidError::MoneyMovementProductRequested(_))),
            "Transfer must be refused before any Plaid call"
        );
    }

    #[test]
    fn transactions_only_request_passes_the_guard() {
        let request = LinkTokenRequest::transactions_only(UserId::new(Uuid::new_v4()));
        assert!(request.assert_no_money_movement().is_ok());
        // And it requests exactly Transactions + Accounts, nothing that moves money.
        assert!(request.products.iter().all(|p| !p.moves_money()));
        assert!(request.products.contains(&PlaidProduct::Transactions));
        assert!(request.products.contains(&PlaidProduct::Accounts));
    }
}
