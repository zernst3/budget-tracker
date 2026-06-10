//! Plaid integration ports + DTOs (`SPEC §6`).
//!
//! Two abstract ports, both WASM-clean (no `SeaORM`, no `reqwest`), so the
//! application services can orchestrate against them and tests can substitute
//! mocked implementations — NO live Plaid call ever runs in a unit test
//! (`SPEC §6`; live creds are a deploy-time step).
//!
//! - [`PlaidApi`] — the raw Plaid HTTP surface: create a Link token, exchange a
//!   `public_token` for an `access_token`, and pull `/transactions/sync`
//!   (cursor-based) pages. It speaks **raw Plaid data only**: amounts are in
//!   Plaid's native convention (positive = outflow). The sign flip to the
//!   internal convention happens exactly once, downstream, at the mapper
//!   boundary (`BUDGET-PLAID-SIGN-1`) — this port never interprets sign.
//! - [`PlaidSyncEngine`] — the higher-level sync mechanics that translate Plaid
//!   pages into domain transactions (via the mapper, the single flip site),
//!   apply `added / modified / removed`, run the rolling 30-day reconcile, and
//!   honor the genesis cutover guard (`BUDGET-CUTOVER-1`). Its concrete impl
//!   lives in `budget-infrastructure` (it needs the mapper + repositories);
//!   `PlaidSyncService` in `budget-app-services` orchestrates against this port.
//!
//! ## Product scoping (`BUDGET-PLAID-TOKEN-VAULT-1`, `SPEC §6`)
//!
//! Link tokens are requested for the **Transactions** product only (+ Accounts
//! for naming). The **Transfer** product is NEVER enabled, so the resulting
//! `access_token` physically cannot move money. [`LinkTokenRequest`] carries the
//! requested products and [`LinkTokenRequest::assert_no_money_movement`] is the
//! invariant guard the HTTP client asserts before it ever calls Plaid.

use async_trait::async_trait;
use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::error::RepositoryError;
use crate::ids::{PlaidItemId, UserId};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failure talking to Plaid or applying a sync (`SPEC §6`).
///
/// Carries only non-sensitive descriptions: the Plaid `access_token`, the
/// `public_token`, and any secret material NEVER appear in an error
/// (`BUDGET-PLAID-TOKEN-VAULT-1`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlaidError {
    /// The Plaid HTTP call failed (transport, auth, or a Plaid-side error code).
    /// The description is the Plaid error category / message, never a token.
    #[error("plaid api failure: {0}")]
    Api(String),

    /// A money-movement product (e.g. Transfer) was requested. This is a
    /// programming error the client refuses to send (`SPEC §6`: the token must
    /// be physically unable to move money).
    #[error("money-movement product requested ({0}); only Transactions + Accounts are allowed")]
    MoneyMovementProductRequested(String),

    /// A persistence failure while applying a sync (wraps [`RepositoryError`]).
    #[error(transparent)]
    Repository(#[from] RepositoryError),

    /// The vault read/write of the access token failed. Non-sensitive only.
    #[error("secret vault failure: {0}")]
    SecretVault(String),

    /// A Plaid response could not be mapped into a domain value (e.g. a
    /// malformed amount or date). Never carries token material.
    #[error("plaid response mapping failure: {0}")]
    Mapping(String),
}

// ---------------------------------------------------------------------------
// Link + token DTOs
// ---------------------------------------------------------------------------

/// The Plaid products an `access_token` may be scoped to (`SPEC §6`).
///
/// Only [`PlaidProduct::Transactions`] and [`PlaidProduct::Accounts`] are ever
/// allowed. [`PlaidProduct::Transfer`] exists in this enum ONLY so the
/// money-movement guard can name it and refuse it — it is never requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaidProduct {
    /// Transaction history — the one product the budget tracker reads.
    Transactions,
    /// Account metadata (for naming the linked accounts).
    Accounts,
    /// Money movement. NEVER requested; present only for the refusal guard.
    Transfer,
}

impl PlaidProduct {
    /// The Plaid API string for this product.
    #[must_use]
    pub const fn as_plaid_str(self) -> &'static str {
        match self {
            PlaidProduct::Transactions => "transactions",
            PlaidProduct::Accounts => "accounts",
            PlaidProduct::Transfer => "transfer",
        }
    }

    /// Whether this product can move money. Only [`PlaidProduct::Transfer`] can.
    #[must_use]
    pub const fn moves_money(self) -> bool {
        matches!(self, PlaidProduct::Transfer)
    }
}

/// A request to create a Plaid Link token (`SPEC §6`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTokenRequest {
    /// The owning user (Plaid `client_user_id`).
    pub user_id: UserId,
    /// The products to scope the eventual `access_token` to. MUST be exactly
    /// Transactions (+ Accounts); the money-movement guard refuses anything else.
    pub products: Vec<PlaidProduct>,
}

impl LinkTokenRequest {
    /// Build the read-only Transactions(+Accounts) request — the only shape the
    /// app ever uses (`SPEC §6`).
    #[must_use]
    pub fn transactions_only(user_id: UserId) -> Self {
        Self {
            user_id,
            products: vec![PlaidProduct::Transactions, PlaidProduct::Accounts],
        }
    }

    /// Assert no money-movement product is present (`SPEC §6`,
    /// `BUDGET-PLAID-TOKEN-VAULT-1`): the resulting token must be physically
    /// unable to move money. Called by the HTTP client before any Plaid call.
    ///
    /// # Errors
    /// [`PlaidError::MoneyMovementProductRequested`] if any requested product
    /// can move money (e.g. Transfer).
    pub fn assert_no_money_movement(&self) -> Result<(), PlaidError> {
        if let Some(p) = self.products.iter().find(|p| p.moves_money()) {
            return Err(PlaidError::MoneyMovementProductRequested(
                p.as_plaid_str().to_owned(),
            ));
        }
        Ok(())
    }
}

/// The opaque Link token returned to the frontend widget (`SPEC §6`). Short-lived;
/// never persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkToken(pub String);

/// The result of exchanging a `public_token` for an `access_token` (`SPEC §6`).
///
/// The `access_token` is the long-lived secret; it is written ONLY to Key Vault
/// and the DB stores only the reference (`BUDGET-PLAID-TOKEN-VAULT-1`). It is
/// never logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessTokenExchange {
    /// The Plaid `access_token` — secret. Caller must store it to the vault and
    /// drop the plaintext immediately; NEVER write it to the DB or a log.
    pub access_token: String,
    /// The Plaid `item_id` (a stable, non-secret identifier for the link).
    pub plaid_item_id: String,
}

// ---------------------------------------------------------------------------
// Sync DTOs (raw Plaid; sign NOT yet flipped)
// ---------------------------------------------------------------------------

/// A single transaction as Plaid reports it (`/transactions/sync`).
///
/// Amounts are in **Plaid's native convention**: `amount > 0` = outflow (debit),
/// `amount < 0` = inflow (credit/refund). The flip to the internal signed
/// convention (negative = expense) happens once downstream at the mapper boundary
/// (`BUDGET-PLAID-SIGN-1`); this DTO is never sign-interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaidTransaction {
    /// Plaid stable transaction id (dedup key; UNIQUE in the DB, `SPEC §5`).
    pub transaction_id: String,
    /// Plaid account id this transaction belongs to.
    pub account_id: String,
    /// Native Plaid amount (positive = outflow). NUMERIC — never `f64`
    /// (`BUDGET-MONEY-1`).
    pub amount: Decimal,
    /// The authorized / posted date (`SPEC §6` cutover guard keys on this).
    pub date: NaiveDate,
    /// Merchant / payee description.
    pub name: String,
    /// Whether Plaid currently considers this transaction pending (`SPEC §4.4`:
    /// pending is EXCLUDED from budget math until settled).
    pub pending: bool,
    /// When a pending transaction settles, the settled version arrives via
    /// `modified` carrying the original pending row's id here, so the two are
    /// linked (`SPEC §6` pending->settled transition).
    pub pending_transaction_id: Option<String>,
    /// Plaid `personal_finance_category.detailed` string, if provided
    /// (`SPEC §4.11`, D10, `BUDGET-TRANSFER-EXCLUDE-1`).
    ///
    /// Examples: `LOAN_PAYMENTS_CREDIT_CARD_PAYMENT`, `TRANSFER_OUT`,
    /// `TRANSFER_IN`. Captured at ingest and stored in
    /// `transactions.plaid_category` to drive the triage Transfer AUTO-SUGGEST.
    /// `None` when Plaid did not supply the field (older API responses, some
    /// account types). Never used for budget math — the flag
    /// `transactions.is_transfer` (set explicitly at triage) is the authoritative
    /// exclusion signal.
    pub plaid_category: Option<String>,
}

/// A Plaid account as reported alongside a sync (for naming, `SPEC §6`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaidAccount {
    /// Plaid stable account id.
    pub account_id: String,
    /// Human-readable account name.
    pub name: String,
    /// Plaid account type (`depository`, `credit`, ...). Best-effort mapping to
    /// the domain [`crate::enums::AccountType`].
    pub account_type: String,
}

/// One page of a cursor-based `/transactions/sync` pull (`SPEC §6`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaidSyncPage {
    /// Brand-new transactions since the cursor.
    pub added: Vec<PlaidTransaction>,
    /// Updated transactions (incl. the pending->settled transition).
    pub modified: Vec<PlaidTransaction>,
    /// Ids of transactions Plaid has removed/reversed.
    pub removed: Vec<String>,
    /// Accounts referenced by this page (for naming).
    pub accounts: Vec<PlaidAccount>,
    /// The cursor to persist + pass on the next pull.
    pub next_cursor: String,
    /// Whether more pages remain (loop until `false`).
    pub has_more: bool,
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// The raw Plaid HTTP surface (`SPEC §6`). Concrete impl in
/// `budget-infrastructure`; mocked in tests (no live calls, `SPEC §6`).
#[async_trait]
pub trait PlaidApi: Send + Sync {
    /// Create a Link token for the frontend widget. The request is asserted to
    /// contain no money-movement product before the call
    /// (`LinkTokenRequest::assert_no_money_movement`, `SPEC §6`).
    ///
    /// # Errors
    /// [`PlaidError::MoneyMovementProductRequested`] if the guard trips;
    /// [`PlaidError::Api`] on any Plaid/transport failure.
    async fn create_link_token(&self, request: &LinkTokenRequest) -> Result<LinkToken, PlaidError>;

    /// Exchange a short-lived `public_token` for a long-lived `access_token`
    /// (`SPEC §6`). The returned `access_token` is secret — the caller writes it
    /// to Key Vault and never to the DB/log (`BUDGET-PLAID-TOKEN-VAULT-1`).
    ///
    /// # Errors
    /// [`PlaidError::Api`] on any Plaid/transport failure.
    async fn exchange_public_token(
        &self,
        public_token: &str,
    ) -> Result<AccessTokenExchange, PlaidError>;

    /// Pull one `/transactions/sync` page for `access_token` from `cursor`
    /// (`None` = from the beginning). Cursor-based, NOT date-range (`SPEC §6`).
    ///
    /// `access_token` is passed by value-reference and never logged.
    ///
    /// # Errors
    /// [`PlaidError::Api`] on any Plaid/transport failure.
    async fn transactions_sync(
        &self,
        access_token: &str,
        cursor: Option<&str>,
    ) -> Result<PlaidSyncPage, PlaidError>;
}

/// The summary of one item's sync pull (`SPEC §6`) — what `PlaidSyncService`
/// surfaces to its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncSummary {
    /// New transactions ingested (after the cutover guard, `BUDGET-CUTOVER-1`).
    pub added: usize,
    /// Transactions updated in place (incl. pending->settled).
    pub modified: usize,
    /// Transactions removed (settlement reversed where applicable,
    /// `BUDGET-SETTLE-ON-MATCH-1`).
    pub removed: usize,
    /// Transactions skipped because they predate `tracking_start_date`
    /// (`BUDGET-CUTOVER-1`).
    pub skipped_pre_genesis: usize,
    /// Transactions re-reconciled in the rolling 30-day window (`SPEC §6`).
    pub reconciled: usize,
}

impl SyncSummary {
    /// Merge another page's counts into this summary.
    pub fn merge(&mut self, other: SyncSummary) {
        self.added += other.added;
        self.modified += other.modified;
        self.removed += other.removed;
        self.skipped_pre_genesis += other.skipped_pre_genesis;
        self.reconciled += other.reconciled;
    }
}

/// The higher-level sync mechanics (`SPEC §6`). Translates Plaid pages into
/// domain transactions through the mapper (the single sign-flip site,
/// `BUDGET-PLAID-SIGN-1`), applies `added / modified / removed`, runs the rolling
/// 30-day reconcile, and honors the genesis cutover guard (`BUDGET-CUTOVER-1`).
///
/// Concrete impl in `budget-infrastructure` (it needs the mapper + repositories
/// + the unit-of-work); `PlaidSyncService` in `budget-app-services` orchestrates
/// against this object-safe port (`SERVICE-DI-1`).
#[async_trait]
pub trait PlaidSyncEngine: Send + Sync {
    /// Run a full cursor sync for one linked item: loop `/transactions/sync`
    /// pages from the stored cursor, apply each page (cutover-guarded, deduped,
    /// idempotent), persist the cursor, then run the rolling 30-day reconcile.
    /// All writes for a page are committed atomically via the unit-of-work
    /// (`SERVICE-TX-1`).
    ///
    /// `access_token` is fetched from the vault by the caller and passed in; it
    /// is never logged here (`BUDGET-PLAID-TOKEN-VAULT-1`).
    ///
    /// # Errors
    /// [`PlaidError`] on any Plaid, mapping, vault, or persistence failure.
    async fn sync_item(
        &self,
        item_id: PlaidItemId,
        user_id: UserId,
        access_token: &str,
        tracking_start_date: NaiveDate,
        today: NaiveDate,
    ) -> Result<SyncSummary, PlaidError>;
}
