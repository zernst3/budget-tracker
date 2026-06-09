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
use crate::ids::{
    AccountId, BudgetId, CategoryId, FundId, MonthId, PlaidItemId, RepaymentObligationId,
    TransactionId, UserId,
};
use crate::month::Month;
use crate::paycheck_config::PaycheckConfig;
use crate::plaid_item::PlaidItem;
use crate::projections::{CategorySpent, MonthNet};
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

    /// The net position of a month — the signed sum of every budget-counting
    /// transaction in it, computed in a single SQL aggregate (`REPO-9` /
    /// `RUST-SEAORM-PROJECTION-TYPES-1`; `DB-NPLUSONE-1`).
    ///
    /// The same inclusion polarity applies (`BUDGET-STATUS-DRIVES-INCLUSION-1`).
    /// This is the rolling-Other input (`SPEC §4.3`, build step 4). Returns a
    /// [`MonthNet`] with a zero `net` when the month has no counting
    /// transactions (rather than `None`), so callers always get a usable figure.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError>;

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
