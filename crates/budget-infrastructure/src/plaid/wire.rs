//! Plaid `/transactions/sync` wire DTOs (`SPEC §6`).
//!
//! These are the serde structs the byte-level Plaid JSON response deserializes
//! into, plus the `From` conversions into the domain [`PlaidSyncPage`] /
//! [`PlaidTransaction`] / [`PlaidAccount`] DTOs. They are deliberately shared
//! between two adapters:
//!
//! - [`HttpPlaidApi`](super::http_client::HttpPlaidApi) — the real reqwest client
//!   deserializes the live Plaid response through them.
//! - [`MockPlaidApi`](super::mock_client::MockPlaidApi) — the local-dev mock
//!   deserializes its *fixture* JSON through the **same** DTOs, so the mock
//!   exercises the real byte-level JSON -> DTO contract, not just the Rust type
//!   (`STAGE-1` fidelity requirement).
//!
//! Because the fixtures are stored as real Plaid `/transactions/sync` response
//! JSON and walk through this one deserialization path, a drift between the
//! fixture schema and the real Plaid schema would fail the mock's own
//! round-trip test — keeping the mock honest to the contract.
//!
//! Amounts stay in Plaid's native convention (`amount > 0` = outflow); the sign
//! flip to the internal convention happens exactly once downstream at the mapper
//! boundary (`BUDGET-PLAID-SIGN-1`). These DTOs never interpret sign.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use budget_domain::plaid_api::{PlaidAccount, PlaidSyncPage, PlaidTransaction};

/// The full `/transactions/sync` response envelope.
///
/// `accounts` defaults to empty because Plaid only includes it on the first
/// page of a fresh sync; later pages may omit it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TransactionsSyncResponse {
    pub(crate) added: Vec<WireTransaction>,
    pub(crate) modified: Vec<WireTransaction>,
    pub(crate) removed: Vec<WireRemoved>,
    #[serde(default)]
    pub(crate) accounts: Vec<WireAccount>,
    pub(crate) next_cursor: String,
    pub(crate) has_more: bool,
}

impl From<TransactionsSyncResponse> for PlaidSyncPage {
    fn from(resp: TransactionsSyncResponse) -> Self {
        PlaidSyncPage {
            added: resp.added.into_iter().map(Into::into).collect(),
            modified: resp.modified.into_iter().map(Into::into).collect(),
            removed: resp.removed.into_iter().map(|r| r.transaction_id).collect(),
            accounts: resp.accounts.into_iter().map(Into::into).collect(),
            next_cursor: resp.next_cursor,
            has_more: resp.has_more,
        }
    }
}

/// A single transaction row as Plaid reports it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireTransaction {
    pub(crate) transaction_id: String,
    pub(crate) account_id: String,
    pub(crate) amount: Decimal,
    pub(crate) date: NaiveDate,
    pub(crate) name: String,
    pub(crate) pending: bool,
    #[serde(default)]
    pub(crate) pending_transaction_id: Option<String>,
    /// Plaid `personal_finance_category` object (`SPEC §4.11`, D10).
    ///
    /// Tolerant of absence: Plaid only includes this field when the
    /// `transactions` product includes enhanced categorization.  `#[serde(default)]`
    /// maps both a missing key and an explicit JSON `null` to `None`, so the DTO
    /// never fails to deserialize if the field is absent or null.
    #[serde(default)]
    pub(crate) personal_finance_category: Option<WirePersonalFinanceCategory>,
}

/// The `personal_finance_category` sub-object that Plaid may include on a
/// transaction.  We capture only `detailed` (e.g.
/// `LOAN_PAYMENTS_CREDIT_CARD_PAYMENT`, `TRANSFER_OUT`, `TRANSFER_IN`) because
/// that is the field that drives the transfer triage AUTO-SUGGEST
/// (`SPEC §4.11`, `BUDGET-TRANSFER-EXCLUDE-1`).  The other fields
/// (`primary`, `confidence_level`) are intentionally ignored.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WirePersonalFinanceCategory {
    /// Detailed Plaid category code.  `None` when the key is absent or null.
    #[serde(default)]
    pub(crate) detailed: Option<String>,
    // `primary` and `confidence_level` are present in the Plaid schema but
    // unused by this application; serde silently ignores unknown fields by
    // default, so no explicit skip annotation is needed.
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
            // Flatten the nested `personal_finance_category.detailed` into the
            // single `plaid_category` field the domain DTO carries.  Tolerant of
            // any combination of absent object / null object / absent detailed
            // field / null detailed value — all resolve to `None`.
            plaid_category: w.personal_finance_category.and_then(|pfc| pfc.detailed),
        }
    }
}

/// A removed-transaction entry (only its id is meaningful).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireRemoved {
    pub(crate) transaction_id: String,
}

/// A Plaid account row (for naming the linked accounts).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WireAccount {
    pub(crate) account_id: String,
    pub(crate) name: String,
    #[serde(rename = "type", default)]
    pub(crate) account_type: String,
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
