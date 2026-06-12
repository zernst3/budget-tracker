//! Repository traits — one per aggregate (`REPO-1`).
//!
//! Each trait is declared here in the domain crate; the concrete `SeaORM` impls
//! live in `budget-infrastructure`. Every method:
//!   - is `async` (`REPO-3`),
//!   - takes and returns DOMAIN types, never `SeaORM` `Model`s (`REPO-2`),
//!   - returns `Result<_, RepositoryError>` (the single shared error, `DOMAIN-6`),
//!   - threads cross-aggregate transactions via `Option<&dyn UnitOfWork>` on
//!     write paths (`REPO-4`): `None` -> use the pool, `Some` -> enlist in the tx.
//!
//! The method surfaces are sized to what the services will need per `SPEC §4/§5`
//! — including the rollover, settlement, fund, and Plaid-cursor reads that the
//! lazy-init, sync, and large-purchase flows depend on. Read-only methods omit
//! the `UoW` handle (they never participate in a write transaction).

use async_trait::async_trait;
use chrono::NaiveDate;

use crate::account::Account;
use crate::budget::Budget;
use crate::category::Category;
use crate::error::RepositoryError;
use crate::fund::Fund;
use crate::ids::PositionId;
use crate::ids::{
    AccountId, BudgetId, CategoryId, FundId, MonthId, PlaidItemId, RepaymentObligationId,
    TransactionId, UserId,
};
use crate::month::Month;
use crate::paycheck_config::PaycheckConfig;
use crate::plaid_item::PlaidItem;
use crate::portfolio::{CashBalanceSource, Position, PositionSource, ReviewRun};
use crate::portfolio::{DividendEvent, DripApplication, Ticker};
use crate::projections::CategorySpent;
use crate::repayment_obligation::RepaymentObligation;
use crate::transaction::Transaction;
use crate::uow::UnitOfWork;
use crate::user::User;

/// Persistence for the [`User`] aggregate (`REPO-1`).
#[async_trait]
pub trait UserRepository: Send + Sync {
    /// Fetch a user by id.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_by_id(&self, id: UserId) -> Result<Option<User>, RepositoryError>;

    /// Fetch a user by email (the login path, `SPEC §9`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_by_email(&self, email: &str) -> Result<Option<User>, RepositoryError>;

    /// Insert or update a user.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save(&self, user: &User, uow: Option<&dyn UnitOfWork>) -> Result<(), RepositoryError>;
}

/// Persistence for the [`Budget`] aggregate and its [`Category`] children
/// (`REPO-1`). Categories belong to a budget version and have no independent
/// lifecycle, so they are managed through this repository.
#[async_trait]
pub trait BudgetRepository: Send + Sync {
    /// Fetch a budget version by id.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_by_id(&self, id: BudgetId) -> Result<Option<Budget>, RepositoryError>;

    /// Find the budget version active on `date` for a user — the version whose
    /// `[effective_from, effective_to]` range covers `date` (`SPEC §4.1`).
    /// Used when lazy-init creates a month and must resolve its budget version.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_active_for_date(
        &self,
        user_id: UserId,
        date: NaiveDate,
    ) -> Result<Option<Budget>, RepositoryError>;

    /// The current active version (`effective_to IS NULL`) for a user.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_current(&self, user_id: UserId) -> Result<Option<Budget>, RepositoryError>;

    /// All budget versions for a user, newest-effective first.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Budget>, RepositoryError>;

    /// All categories belonging to a budget version, in `sort_order`.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_categories(&self, budget_id: BudgetId) -> Result<Vec<Category>, RepositoryError>;

    /// Fetch a single category by id.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_category(&self, id: CategoryId) -> Result<Option<Category>, RepositoryError>;

    /// The rollover bucket ("Other") for a budget version — the single category
    /// with `is_rollover_bucket = true` (`BUDGET-ROLLOVER-INTEGRITY-1`). The
    /// rollover transaction posts against this category.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_rollover_bucket(
        &self,
        budget_id: BudgetId,
    ) -> Result<Option<Category>, RepositoryError>;

    /// Insert or update a budget version.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save(
        &self,
        budget: &Budget,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;

    /// Insert or update a category (including its sinking-fund `fund_balance`,
    /// `SPEC §4.7`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save_category(
        &self,
        category: &Category,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;
}

/// Persistence for the [`Month`] aggregate (`REPO-1`).
///
/// Sized for lazy-init (`BUDGET-IDEMPOTENT-MONTH-INIT-1`): finding the latest
/// month, looking up a specific `(year, month)`, and an idempotent create.
#[async_trait]
pub trait MonthRepository: Send + Sync {
    /// Fetch a month by id.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_by_id(&self, id: MonthId) -> Result<Option<Month>, RepositoryError>;

    /// Fetch a user's month for a specific `(year, month)` (`UNIQUE(user_id,
    /// year, month)`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_by_year_month(
        &self,
        user_id: UserId,
        year: i32,
        month: i32,
    ) -> Result<Option<Month>, RepositoryError>;

    /// The latest existing month for a user (max `(year, month)`), the anchor for
    /// lazy-init multi-month catch-up.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_latest(&self, user_id: UserId) -> Result<Option<Month>, RepositoryError>;

    /// All months for a user, chronological.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Month>, RepositoryError>;

    /// Idempotently create a month, relying on `UNIQUE(user_id, year, month)` so
    /// two racing lazy-init calls on container wake produce one row
    /// (`BUDGET-IDEMPOTENT-MONTH-INIT-1`). Returns the existing or newly-created
    /// month.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn create_if_absent(
        &self,
        month: &Month,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<Month, RepositoryError>;

    /// Insert or update a month (e.g. transitioning `open` -> `closed`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save(
        &self,
        month: &Month,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;
}

/// Persistence for the [`Transaction`] aggregate (`REPO-1`).
///
/// The widest surface: it backs category-spent and month-net aggregation, the
/// rollover chain, Plaid dedup, and settlement/match (`BUDGET-SETTLE-ON-MATCH-1`).
#[async_trait]
pub trait TransactionRepository: Send + Sync {
    /// Fetch a transaction by id.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_by_id(&self, id: TransactionId) -> Result<Option<Transaction>, RepositoryError>;

    /// All transactions in a month (every status; callers apply
    /// [`crate::predicates::counts_in_budget`] when aggregating).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_for_month(&self, month_id: MonthId) -> Result<Vec<Transaction>, RepositoryError>;

    /// The pending-triage inbox for a user (`SPEC §7`): every transaction with
    /// `status = 'settled'` AND `category_id IS NULL` — settled bank charges that
    /// have not yet been categorized.
    ///
    /// This is the "Pull -> Pending -> triage" inbox, NOT a Plaid `pending`
    /// credit-card charge (`SPEC §4.4`): Plaid `pending` rows carry
    /// `status = 'pending'` and are excluded by the `settled` filter, so they never
    /// surface here. Ordered by date ascending so the oldest awaiting-categorization
    /// charge is first. Scoped to `user_id` (`SPEC §9.1` defense in depth).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_pending_inbox(
        &self,
        user_id: UserId,
    ) -> Result<Vec<Transaction>, RepositoryError>;

    /// All transactions assigned to a category within a month — the input to the
    /// fixed-category spent predicate (`BUDGET-NO-DOUBLE-CHARGE-1`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_for_category_in_month(
        &self,
        month_id: MonthId,
        category_id: CategoryId,
    ) -> Result<Vec<Transaction>, RepositoryError>;

    /// The rollover line item for a month, if posted — the idempotency check for
    /// lazy-init (`BUDGET-ROLLOVER-INTEGRITY-1`): the single row with
    /// `is_rollover = true` in that month.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_rollover_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<Transaction>, RepositoryError>;

    /// Look up a transaction by its Plaid stable id — the dedup path on sync
    /// (`SPEC §6`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_by_plaid_transaction_id(
        &self,
        plaid_transaction_id: &str,
    ) -> Result<Option<Transaction>, RepositoryError>;

    /// All `expected`-status placeholders targeting a month — used by
    /// settle-on-match (`BUDGET-SETTLE-ON-MATCH-1`) and stale-at-close handling
    /// (`SPEC §4.10`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_expected_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Vec<Transaction>, RepositoryError>;

    /// The `expected` placeholder matched to a given real transaction, if any —
    /// the reverse-path lookup for un-match (`BUDGET-SETTLE-ON-MATCH-1`).
    ///
    /// Returns the single placeholder whose `matched_transaction_id` equals
    /// `real_transaction_id` (a real charge matches at most one placeholder). When
    /// the matched real transaction is removed (Plaid `removed`), the engine reads
    /// this to find the placeholder to restore, then clears the link. Backed by
    /// the partial index `ix_transactions_matched_transaction_id` (migration
    /// m0003).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_expected_matched_to(
        &self,
        real_transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, RepositoryError>;

    /// Per-category spent totals for a month, aggregated in a single SQL query
    /// (`REPO-9` / `RUST-SEAORM-PROJECTION-TYPES-1`; `DB-NPLUSONE-1`).
    ///
    /// Returns one [`CategorySpent`] per category that has at least one
    /// budget-counting transaction in the month. The status filter is exactly the
    /// inclusion polarity of [`crate::predicates::counts_in_budget`]
    /// (`BUDGET-STATUS-DRIVES-INCLUSION-1`: settled + expected; pending excluded),
    /// applied in SQL so the whole grouping is one round-trip rather than N
    /// per-category queries (`SQL-DB-NPLUSONE-1`). Uncategorized rows
    /// (`category_id IS NULL`) are excluded — they belong to no category bucket.
    /// The returned signed sums feed [`crate::predicates::fixed_category_spent`]
    /// in the service layer (`BUDGET-NO-DOUBLE-CHARGE-1`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn category_spent_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Vec<CategorySpent>, RepositoryError>;

    /// Insert or update a transaction. Used for manual entry, Plaid `added`,
    /// posting the rollover row, and recording a settlement/match
    /// (`BUDGET-SETTLE-ON-MATCH-1`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save(
        &self,
        transaction: &Transaction,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;

    /// Delete a transaction — the Plaid `removed` path, which also reverses any
    /// settlement/match it had driven (`BUDGET-SETTLE-ON-MATCH-1`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn delete(
        &self,
        id: TransactionId,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;
}

/// Persistence for the [`Fund`] aggregate and its [`RepaymentObligation`]
/// children (`REPO-1`). Obligations are created/closed only alongside fund draws
/// and repayments, so they are managed through this repository.
#[async_trait]
pub trait FundRepository: Send + Sync {
    /// Fetch a fund by id.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_by_id(&self, id: FundId) -> Result<Option<Fund>, RepositoryError>;

    /// All funds for a user (buffer + surplus).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Fund>, RepositoryError>;

    /// Insert or update a fund (including a balance change from a contribution or
    /// draw, `BUDGET-FUND-EARMARK-1`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save(&self, fund: &Fund, uow: Option<&dyn UnitOfWork>) -> Result<(), RepositoryError>;

    /// Fetch a repayment obligation by id.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_obligation(
        &self,
        id: RepaymentObligationId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError>;

    /// All active obligations for a user — the input to the monthly compulsory
    /// installment expense and the buffer-health flag (`SPEC §4.9`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_active_obligations(
        &self,
        user_id: UserId,
    ) -> Result<Vec<RepaymentObligation>, RepositoryError>;

    /// Find the obligation backing a specific buffer-financed purchase
    /// transaction, if any (`SPEC §4.9` D7). The installment posting path looks up
    /// the obligation for a given full-price transaction.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_obligation_for_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError>;

    /// Find the deficit-financing obligation whose `origin_month_id` is `month_id`,
    /// if any, REGARDLESS OF STATUS (`SPEC §12` D9, `BUDGET-DEFICIT-FINANCING-1`).
    ///
    /// At most one `source = 'deficit'` obligation exists per origin month (a
    /// month's deficit is financed at most once). The month-lifecycle rollover path
    /// consumes this to suppress rolling the financed deficit forward in full (the
    /// installments carry it instead). It MUST match `Paid` obligations too: a
    /// single-month financing (`months == 1`) flips the obligation to `Paid` the
    /// instant it is created, yet that month's deficit was still financed and must
    /// stay suppressed — filtering on `Active` would double-count it. Backed by
    /// `ix_repayment_obligations_origin_month_id`.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_deficit_obligation_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError>;

    /// The set of transaction ids that are buffer-financed full-price purchases —
    /// the `transaction_id` of EVERY repayment obligation for the user, active or
    /// paid (`SPEC §4.9` D7).
    ///
    /// These rows post for TRACKING only and must be excluded from the month
    /// expense-remaining sum *permanently* (even after the obligation is paid):
    /// the cash was fronted by the buffer, never out of the month's budget; the
    /// budget effect was the installments (`BUDGET-FUND-EARMARK-1` /
    /// `counts_in_month_expense_remaining`). The month-lifecycle netting (build
    /// step 4) consumes this set to drive that exclusion.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_buffer_financed_transaction_ids(
        &self,
        user_id: UserId,
    ) -> Result<Vec<TransactionId>, RepositoryError>;

    /// Insert or update a repayment obligation (creation on a buffer-financed
    /// purchase; balance decrement on each installment; close on settle).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save_obligation(
        &self,
        obligation: &RepaymentObligation,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;
}

/// Persistence for the [`PlaidItem`] aggregate and its [`Account`] children
/// (`REPO-1`). Carries the cursor read/write that incremental sync depends on
/// (`SPEC §6`).
#[async_trait]
pub trait PlaidItemRepository: Send + Sync {
    /// Fetch a plaid item by id.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_by_id(&self, id: PlaidItemId) -> Result<Option<PlaidItem>, RepositoryError>;

    /// All plaid items for a user.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<PlaidItem>, RepositoryError>;

    /// Read the incremental sync cursor for an item (`SPEC §6`); `None` before
    /// the first sync.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn get_sync_cursor(&self, id: PlaidItemId) -> Result<Option<String>, RepositoryError>;

    /// Persist the sync cursor + `last_synced_at` after a successful pull
    /// (`SPEC §6`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn update_sync_cursor(
        &self,
        id: PlaidItemId,
        cursor: &str,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;

    /// Insert or update a plaid item (e.g. on first link, storing the Key Vault
    /// `access_token_ref`, `BUDGET-PLAID-TOKEN-VAULT-1`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save(
        &self,
        item: &PlaidItem,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;

    /// All accounts for a user.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_accounts(&self, user_id: UserId) -> Result<Vec<Account>, RepositoryError>;

    /// Fetch an account by id.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_account(&self, id: AccountId) -> Result<Option<Account>, RepositoryError>;

    /// Look up an account by its Plaid stable account id — links a synced
    /// transaction to its account (`SPEC §6`).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_account_by_plaid_id(
        &self,
        plaid_account_id: &str,
    ) -> Result<Option<Account>, RepositoryError>;

    /// Insert or update an account.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save_account(
        &self,
        account: &Account,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;
}

/// Persistence for the [`PaycheckConfig`] aggregate (`REPO-1`) — one per user.
#[async_trait]
pub trait PaycheckConfigRepository: Send + Sync {
    /// The user's income configuration (`SPEC §4.8`); `None` before onboarding.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Option<PaycheckConfig>, RepositoryError>;

    /// Insert or update the income configuration.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn save(
        &self,
        config: &PaycheckConfig,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError>;
}

/// Persistence for the [`ReviewRun`] audit aggregate (`REPO-1`,
/// `SQL-AUDIT-COLUMNS-1`).
///
/// `review_runs` is an append-only system log: there is no update or delete
/// surface. The single write is [`ReviewRunRepository::insert`], which takes a
/// `&mut dyn UnitOfWork` directly (not the `Option<&dyn UnitOfWork>` shape used
/// by the editable aggregates) because the portfolio-review use-case always
/// persists the run inside its own explicit transaction
/// (`ARCH-EXPLICIT-TX-1` / `RUST-DOMAIN-7`).
#[async_trait]
pub trait ReviewRunRepository: Send + Sync {
    /// Append one review-run audit row, enlisted in the caller's transaction.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn insert(
        &self,
        run: &ReviewRun,
        uow: &mut dyn UnitOfWork,
    ) -> Result<(), RepositoryError>;

    /// All review runs for a user (newest-first history).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<ReviewRun>, RepositoryError>;
}

/// Persistence for the [`Position`] aggregate (`REPO-1`).
///
/// Extends the read-only [`PositionSource`] port (which the portfolio-review
/// use-case depends on for grounding) with the write surface the manual
/// positions UI needs.
#[async_trait]
pub trait PositionRepository: PositionSource {
    /// Insert a new position.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn insert(&self, position: &Position) -> Result<(), RepositoryError>;

    /// Update an existing position.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn update(&self, position: &Position) -> Result<(), RepositoryError>;

    /// Delete a user's position by id (`user_id`-scoped, `SPEC §9.1` defense in
    /// depth).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn delete(&self, user_id: UserId, id: PositionId) -> Result<(), RepositoryError>;

    /// Reconcile an upload of ONE account's holdings as a PER-ACCOUNT UPSERT, in a
    /// single transaction (`docs/DRIP_REALTIME_DESIGN.md §2.7/§6`,
    /// `ARCH-EXPLICIT-TX-1`, `RUST-SEAORM-INTRA-AGGREGATE-TX-1`).
    ///
    /// Identity is `(user_id, ticker, account_label)`. Scoped to the single
    /// `account_label`: against the existing positions in THAT account only,
    /// - a position present in `uploaded` is UPDATED (new `shares` + `cost_basis`,
    ///   `baseline_as_of = baseline`; **`drip_enabled` is PRESERVED**; the DRIP
    ///   estimate resets because current shares derive from applications with
    ///   `pay_date > baseline_as_of`, §6);
    /// - a position ABSENT from `uploaded` is REMOVED (sold off) — **the removal
    ///   sweep is filtered to `account_label`**, so other accounts are never
    ///   touched;
    /// - a position NEW to the account is INSERTED with `drip_enabled = false`
    ///   (opt-in, §2.7), `account_type`, and `baseline_as_of = baseline`.
    ///
    /// Positions in every OTHER account are left completely untouched. The whole
    /// reconcile runs atomically inside the repository's own transaction
    /// (intra-aggregate atomicity, `RUST-SEAORM-INTRA-AGGREGATE-TX-1`), so a
    /// partial upload can never leave the account half-reconciled.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure (the transaction rolls back).
    async fn upsert_account(
        &self,
        user_id: UserId,
        account_label: &str,
        account_type: crate::enums::AccountType,
        uploaded: &[crate::portfolio::UploadedPosition],
        baseline: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), RepositoryError>;

    /// Set ONLY the `drip_enabled` flag on a user's position (`§2.7`, the inline
    /// DRIP toggle). A targeted single-column update so it touches neither the
    /// `shares` baseline nor `baseline_as_of` (it is per-position config, not a
    /// re-baseline) and therefore SURVIVES uploads. `user_id`-scoped
    /// (`SPEC §9.1` defense in depth).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn set_drip_enabled(
        &self,
        user_id: UserId,
        id: PositionId,
        enabled: bool,
    ) -> Result<(), RepositoryError>;
}

/// Persistence for the [`crate::portfolio::CashBalance`] aggregate (`REPO-1`,
/// `BUDGET-CASH-1`).
///
/// Extends the read-only [`CashBalanceSource`] port with the upsert the manual
/// balances UI needs. A balance is keyed by `(user_id, account_label)`, so the
/// single write is an upsert.
#[async_trait]
pub trait CashBalanceRepository: CashBalanceSource {
    /// Insert or update a cash balance for the user, keyed by account label.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn upsert(&self, balance: &crate::portfolio::CashBalance) -> Result<(), RepositoryError>;
}

/// The `dividend_events` ticker-keyed cache (`REPO-1`, Phase 7 `m0008`).
///
/// A dividend is fetched once per ticker and shared across every position holding
/// it. The cache is keyed `(ticker, pay_date)`; [`upsert_many`] is idempotent on
/// that key, and [`find_since`] returns the cached events with
/// `pay_date > since` (chronological), the catch-up engine's read path.
///
/// [`upsert_many`]: DividendEventCache::upsert_many
/// [`find_since`]: DividendEventCache::find_since
#[async_trait]
pub trait DividendEventCache: Send + Sync {
    /// Cached dividend events for `ticker` with `pay_date` strictly after `since`,
    /// chronological by `pay_date`.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn find_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, RepositoryError>;

    /// Idempotently store dividend events, keyed by `(ticker, pay_date)` (a
    /// re-fetch overwrites `amount_per_share`/`source`/`fetched_at` for an
    /// existing key, never duplicating a row).
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn upsert_many(&self, events: &[DividendEvent]) -> Result<(), RepositoryError>;
}

/// Persistence for the [`DripApplication`] auditable chain (`REPO-1`,
/// `SQL-AUDIT-COLUMNS-1`, Phase 7 `m0008`).
///
/// Append-only: the only write is [`apply_if_absent`], which inserts under the
/// `(position_id, pay_date)` unique guard with `ON CONFLICT DO NOTHING` so a
/// re-entrant catch-up posts nothing extra (`BUDGET-IDEMPOTENT-MONTH-INIT-1`).
/// Reads recompute current shares (`baseline + Σ shares_added`,
/// `BUDGET-ROLLOVER-INTEGRITY-1`).
///
/// [`apply_if_absent`]: DripApplicationRepository::apply_if_absent
#[async_trait]
pub trait DripApplicationRepository: Send + Sync {
    /// Insert one application iff no row exists for its `(position_id, pay_date)`
    /// (the idempotency guard). Returns `true` if a row was inserted, `false` if
    /// the guard suppressed a duplicate. Enlists in `uow` when supplied.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn apply_if_absent(
        &self,
        application: &DripApplication,
        uow: Option<&dyn UnitOfWork>,
    ) -> Result<bool, RepositoryError>;

    /// All applications for a position, chronological by `pay_date` — the
    /// auditable chain a current-shares recompute folds over.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn list_for_position(
        &self,
        position_id: PositionId,
    ) -> Result<Vec<DripApplication>, RepositoryError>;
}
