//! The onboarding / initial-load service — the genesis opening snapshot
//! (`SPEC §4.6`, `§4.3`, `§12` D8; build step 9).
//!
//! ## What onboarding is (and is NOT)
//!
//! Onboarding does **NOT** backfill transaction history (`SPEC §4.6`, decided
//! 2026-06-07). The pre-genesis world is CLOSED and is represented solely by a
//! single opening snapshot: per-category month-to-date summary opening charges
//! plus the correct starting rolling-Other balance and starting buffer-fund
//! balance, taken as of the end of the day before `tracking_start_date` (day 0)
//! so any spend in the lock gap between the last spreadsheet day and day 1 is
//! captured (`BUDGET-CUTOVER-1`). From day 1 forward the app tracks
//! per-transaction; real records accumulate on top of these opening lines.
//!
//! ## The genesis boundary and the cutover agreement (`BUDGET-CUTOVER-1`)
//!
//! The genesis boundary is `users.tracking_start_date` ("day 1"). Onboarding
//! writes **no** transaction dated before it — the opening positions are dated
//! `tracking_start_date` itself (the genesis boundary, valued as of end-of-day-0).
//! This is what keeps the two cutover layers from double-counting: Plaid sync and
//! the rolling 30-day reconcile exclude every transaction dated **strictly
//! before** `tracking_start_date` (`< tracking_start_date`, the clamp in
//! `PlaidSyncService`), while onboarding posts the opening positions **on** the
//! boundary (`== tracking_start_date`). The pre-genesis world is therefore in the
//! snapshot and nowhere else; ingesting it via Plaid is structurally impossible.
//!
//! The opening positions land in the **genesis month** (the month containing
//! `tracking_start_date`). A mid-month day-1 is fully handled by the
//! month-to-date opening charges; a clean month-start day-1 (`SPEC §12`
//! onboarding path) is the cleanest case — Zach typically provides only the prior
//! month's surplus as the starting Other balance and tracks everything else
//! normally.
//!
//! ## D6 Model A consistency (`BUDGET-FUND-EARMARK-1`)
//!
//! Each earmarked dollar is counted exactly once:
//!   - The **starting rolling-Other balance** is posted as one opening line on the
//!     genesis month's rollover ("Other") bucket (`is_rollover = false`, a manual
//!     opening expense/credit), establishing the free-to-spend starting point.
//!   - The **starting buffer-fund balance** is set directly on the fund as a
//!     pre-genesis fact. It is NOT also posted as an Other-bucket expense, because
//!     those buffer dollars were already expensed in the pre-genesis world we do
//!     not track; posting them again as a contribution would double-count against
//!     the starting Other (`BUDGET-FUND-EARMARK-1`: fund balances are not
//!     separately subtracted from free-to-spend). The opening buffer balance is
//!     just the fund's starting state.
//!   - The **per-category opening charges** are ordinary settled manual expenses
//!     that COUNT in budget math (they reduce their category's remaining and thus
//!     the genesis month's net at month-close), exactly like real spend would.
//!
//! Because the opening Other line and the opening charges are real transactions in
//! the genesis month, the FIRST step-4 [`crate::month_lifecycle`] rollover out of
//! the genesis month computes a correct prior-month net over them
//! (`BUDGET-ROLLOVER-INTEGRITY-1`): the clean month-start cutover carries forward
//! coherently.
//!
//! ## Re-runnable / idempotent (`BUDGET-CUTOVER-1`, `BUDGET-IDEMPOTENT-MONTH-INIT-1`)
//!
//! Onboarding is re-runnable: a live test phase, then a clean reset to the real
//! day 1 (`SPEC §12`). Every opening row is keyed by a **deterministic identity**
//! ([`opening_charge_id`] / [`opening_other_id`]) derived from the user, the
//! category/role, and the day-0 kind, so the underlying upsert (ON CONFLICT (pk)
//! DO UPDATE) makes a re-run produce identical state: no duplicated opening
//! charges, no double-counted balances. Re-seeding to a new `tracking_start_date`
//! posts the new boundary's opening positions; the deterministic keys are scoped
//! by the seeded category/fund identity, so a clean reset replaces the test
//! state coherently (a category present in both seeds upserts to the new figure).
//!
//! ## INPUTS are runtime, never hardcoded
//!
//! The actual numbers (per-category month-to-date spend, starting Other, starting
//! buffer balance) are supplied at runtime via [`OnboardingInput`] (the
//! `seed-onboarding` `[[bin]]` reads them from the environment / args, ultimately
//! the reference spreadsheet). This service builds the MECHANISM; it hardcodes no
//! figures.
//!
//! ## Transactionality (`SERVICE-TX-1`, `REPO-10`)
//!
//! The whole snapshot (every opening charge + the opening Other line + the fund
//! balance write) commits in ONE [`budget_domain::uow::UowProvider`] closure, so
//! the opening snapshot is all-or-nothing. The service holds `Arc<dyn _>`
//! dependencies (`SERVICE-DI-1`); no `db.*` lives here.

use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use uuid::Uuid;

use budget_domain::enums::{TransactionSource, TransactionStatus};
use budget_domain::error::DomainError;
use budget_domain::ids::{CategoryId, FundId, MonthId, TransactionId, UserId};
use budget_domain::money::Money;
use budget_domain::repositories::{
    BudgetRepository, FundRepository, MonthRepository, TransactionRepository, UserRepository,
};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowProvider, UowProviderExt};

/// The namespace UUID for deterministic onboarding opening-row identities.
///
/// A fixed, project-local v5 namespace (itself a random v4 minted once and frozen
/// here) so opening-row ids are a stable function of their inputs across runs and
/// machines. Changing this constant would orphan previously-seeded opening rows,
/// so it is frozen (`BUDGET-CUTOVER-1`: re-runnable seeding).
const ONBOARDING_NS: Uuid = Uuid::from_u128(0x4f2b_9c1a_7d3e_4a55_b6c8_1e9f_0a2d_3c44);

/// The deterministic [`TransactionId`] of a category's opening charge
/// (`BUDGET-CUTOVER-1`).
///
/// A v5 (name-based) id over `(user, "opening-charge", category)`: re-running the
/// seed produces the SAME id, so the upsert (ON CONFLICT (pk) DO UPDATE) replaces
/// the row in place instead of duplicating it. The id does NOT include the
/// tracking-start date, so re-seeding the same category to a new day-1 upserts the
/// one opening-charge row to the new figure rather than stacking a second.
#[must_use]
pub fn opening_charge_id(user_id: UserId, category_id: CategoryId) -> TransactionId {
    let name = format!(
        "opening-charge|user={}|category={}",
        user_id.value(),
        category_id.value()
    );
    TransactionId::new(Uuid::new_v5(&ONBOARDING_NS, name.as_bytes()))
}

/// The deterministic [`TransactionId`] of the starting rolling-Other opening line
/// (`BUDGET-CUTOVER-1`, `BUDGET-FUND-EARMARK-1`).
///
/// A v5 (name-based) id over `(user, "opening-other")`: there is exactly one
/// starting-Other line per user, so re-running upserts it in place. It is keyed on
/// the rollover bucket category implicitly (the seed always targets the genesis
/// month's bucket); the deterministic id guarantees a single starting-Other row.
#[must_use]
pub fn opening_other_id(user_id: UserId) -> TransactionId {
    let name = format!("opening-other|user={}", user_id.value());
    TransactionId::new(Uuid::new_v5(&ONBOARDING_NS, name.as_bytes()))
}

/// One category's month-to-date opening charge (`SPEC §4.6`).
///
/// `spend_so_far` is the positive magnitude of that category's running total for
/// the partial first month, as of end-of-day-0. A `Money::ZERO` entry posts no
/// charge (a $0 `flexible_set` like utilities settles normally later); a non-zero
/// entry posts one settled opening expense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CategoryOpeningCharge {
    /// The category this opening charge is booked to.
    pub category_id: CategoryId,
    /// The positive month-to-date spend magnitude before tracking began.
    pub spend_so_far: Money,
}

/// The runtime inputs for an onboarding seed (`SPEC §4.6`, `§12` D8).
///
/// Every figure here is supplied at runtime (from the reference spreadsheet via
/// the `seed-onboarding` `[[bin]]`), never hardcoded. `tracking_start_date` is NOT
/// taken here — it is read from the persisted [`budget_domain::user::User`] so the
/// genesis boundary stays single-source (`BUDGET-CUTOVER-1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingInput {
    /// The single user being onboarded.
    pub user_id: UserId,
    /// Per-category month-to-date opening charges. A category absent from this
    /// list (or present with `Money::ZERO`) gets no opening charge.
    pub category_charges: Vec<CategoryOpeningCharge>,
    /// The starting rolling-Other ("free-to-spend") balance as of end-of-day-0
    /// (`SPEC §4.3`). Signed: a positive surplus or a negative carried deficit.
    /// `Money::ZERO` posts no opening Other line.
    pub starting_other_balance: Money,
    /// The starting buffer-fund balance as of end-of-day-0 (`SPEC §4.9`), with the
    /// fund it applies to. `None` when there is no buffer fund to seed (or it
    /// starts empty and need not be touched).
    pub starting_buffer: Option<BufferOpeningBalance>,
}

/// The starting balance of a buffer fund as of end-of-day-0 (`SPEC §4.9`).
///
/// Set directly on the fund as a pre-genesis fact; NOT also posted as an
/// Other-bucket contribution (those dollars were already expensed in the
/// un-tracked pre-genesis world, `BUDGET-FUND-EARMARK-1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferOpeningBalance {
    /// The buffer fund whose opening balance is being seeded.
    pub fund_id: FundId,
    /// The opening balance magnitude (typically non-negative).
    pub balance: Money,
}

/// A summary of what a seed run materialized, for the caller to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingReport {
    /// The genesis month the opening positions were seeded into.
    pub genesis_month_id: MonthId,
    /// The genesis cutover date (`tracking_start_date`) the positions are dated.
    pub genesis_date: NaiveDate,
    /// How many non-zero per-category opening charges were posted.
    pub opening_charges_posted: usize,
    /// Whether a starting rolling-Other opening line was posted.
    pub other_line_posted: bool,
    /// Whether a starting buffer-fund balance was set.
    pub buffer_seeded: bool,
}

/// The onboarding / initial-load service (`SPEC §4.6`, `§12` D8).
///
/// Holds `Arc<dyn _>` repository + provider dependencies (`SERVICE-DI-1`); all
/// `db.*` lives in the repositories.
pub struct OnboardingService {
    users: Arc<dyn UserRepository>,
    budgets: Arc<dyn BudgetRepository>,
    months: Arc<dyn MonthRepository>,
    transactions: Arc<dyn TransactionRepository>,
    funds: Arc<dyn FundRepository>,
    uow: Arc<dyn UowProvider>,
}

impl OnboardingService {
    /// Wire the service from its dependencies (`SERVICE-DI-1`).
    #[must_use]
    pub fn new(
        users: Arc<dyn UserRepository>,
        budgets: Arc<dyn BudgetRepository>,
        months: Arc<dyn MonthRepository>,
        transactions: Arc<dyn TransactionRepository>,
        funds: Arc<dyn FundRepository>,
        uow: Arc<dyn UowProvider>,
    ) -> Self {
        Self {
            users,
            budgets,
            months,
            transactions,
            funds,
            uow,
        }
    }

    /// Seed the genesis opening snapshot for `input` (`SPEC §4.6`, `BUDGET-CUTOVER-1`).
    ///
    /// Reads `tracking_start_date` from the persisted user (the single-source
    /// genesis boundary), resolves the genesis month (the month containing day 1,
    /// creating it if absent), then in ONE transaction (`SERVICE-TX-1`):
    ///   - posts each non-zero per-category opening charge as a settled manual
    ///     expense dated `tracking_start_date`, keyed by [`opening_charge_id`]
    ///     (idempotent),
    ///   - posts the starting rolling-Other line (if non-zero) on the genesis
    ///     month's rollover bucket, keyed by [`opening_other_id`] (idempotent), and
    ///   - sets the starting buffer-fund balance (if provided) directly on the fund
    ///     (a pre-genesis fact, `BUDGET-FUND-EARMARK-1`).
    ///
    /// Re-runnable: a second run with the same `input` yields identical state; a
    /// re-run with new figures upserts the deterministically-keyed rows to the new
    /// values (`SPEC §12` onboarding path: test phase, then a clean reset).
    ///
    /// # Errors
    /// [`DomainError`] if the user is absent, no budget version is active on the
    /// genesis date, the active budget has no rollover bucket, a referenced fund is
    /// absent, or on any persistence failure.
    pub async fn seed(
        &self,
        input: &OnboardingInput,
        now: DateTime<Utc>,
    ) -> Result<OnboardingReport, DomainError> {
        let user = self.users.find_by_id(input.user_id).await?.ok_or_else(|| {
            DomainError::Invariant(format!("user {} not found for onboarding", input.user_id))
        })?;
        let genesis_date = user.tracking_start_date;
        let g_year = genesis_date.year();
        let g_month = i32::try_from(genesis_date.month()).unwrap_or(1);

        // The budget version active on the genesis date supplies the genesis
        // month's budget_id and the rollover ("Other") bucket (SPEC §4.1).
        let budget = self
            .budgets
            .find_active_for_date(input.user_id, genesis_date)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!(
                    "no budget version active on {genesis_date} for {}",
                    input.user_id
                ))
            })?;
        let bucket = self
            .budgets
            .find_rollover_bucket(budget.id)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!(
                    "budget version {} has no rollover bucket",
                    budget.id
                ))
            })?;

        // Resolve (or create) the genesis month. We create it directly here rather
        // than via MonthLifecycleService so onboarding does not post a rollover
        // from a (nonexistent) prior month — the starting-Other line IS the genesis
        // carryover (BUDGET-CUTOVER-1: no backfill). create_if_absent is ON CONFLICT
        // DO NOTHING, so this is idempotent and safe alongside a later lazy-init.
        let genesis_month = self
            .ensure_genesis_month(input.user_id, budget.id, g_year, g_month, now)
            .await?;
        let month_id = genesis_month.id;

        // Validate referenced funds + build the opening rows BEFORE opening the
        // write tx (pure reads / construction, no write dependency).
        let buffer_to_save = match input.starting_buffer {
            Some(b) if !b.balance.is_zero() => {
                let mut fund = self.funds.find_by_id(b.fund_id).await?.ok_or_else(|| {
                    DomainError::Invariant(format!(
                        "buffer fund {} not found for onboarding",
                        b.fund_id
                    ))
                })?;
                // Pre-genesis fact: SET the balance to the opening figure
                // (idempotent — a re-run sets the same value, never accumulates).
                fund.balance = b.balance;
                Some(fund)
            }
            _ => None,
        };

        // Build the opening rows (pure construction, deterministically keyed).
        let opening_txns = build_opening_transactions(input, month_id, bucket.id, genesis_date, now);
        let other_line_posted = !input.starting_other_balance.is_zero();
        let opening_charges_posted = opening_txns.len() - usize::from(other_line_posted);

        let buffer_seeded = buffer_to_save.is_some();

        // SERVICE-TX-1 / REPO-10: the whole opening snapshot commits atomically.
        // Each opening txn is keyed by a deterministic id, so save() (upsert ON
        // CONFLICT (pk) DO UPDATE) makes a re-run a no-op / coherent replace
        // (BUDGET-CUTOVER-1: re-runnable seeding).
        let transactions = Arc::clone(&self.transactions);
        let funds = Arc::clone(&self.funds);
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    for txn in &opening_txns {
                        transactions.save(txn, Some(uow)).await?;
                    }
                    if let Some(fund) = buffer_to_save.as_ref() {
                        funds.save(fund, Some(uow)).await?;
                    }
                    Ok(())
                })
            })
            .await?;

        Ok(OnboardingReport {
            genesis_month_id: month_id,
            genesis_date,
            opening_charges_posted,
            other_line_posted,
            buffer_seeded,
        })
    }

    /// Resolve (or idempotently create) the genesis month, WITHOUT posting any
    /// rollover (`BUDGET-CUTOVER-1`: no backfill — the genesis month has no prior
    /// month to roll from; the starting-Other opening line is its carryover).
    async fn ensure_genesis_month(
        &self,
        user_id: UserId,
        budget_id: budget_domain::ids::BudgetId,
        year: i32,
        month: i32,
        now: DateTime<Utc>,
    ) -> Result<budget_domain::month::Month, DomainError> {
        if let Some(existing) = self.months.find_by_year_month(user_id, year, month).await? {
            return Ok(existing);
        }
        let new_month = budget_domain::month::Month {
            id: MonthId::generate(),
            user_id,
            budget_id,
            year,
            month,
            status: budget_domain::enums::MonthStatus::Open,
            opened_at: now,
            closed_at: None,
        };
        let resolved = self.months.create_if_absent(&new_month, None).await?;
        Ok(resolved)
    }
}

/// Build the genesis opening rows (pure construction; the caller persists them).
///
/// One settled manual expense per NON-ZERO category opening charge (a `$0` charge
/// is skipped so a `flexible_set` like utilities settles normally later,
/// `SPEC §4.6`), each deterministically keyed by [`opening_charge_id`]; then, when
/// the starting Other balance is non-zero, the single opening rolling-Other line
/// on the rollover `bucket_id`, keyed by [`opening_other_id`]. Every row is dated
/// `genesis_date` (`tracking_start_date`, the genesis boundary — never before it,
/// `BUDGET-CUTOVER-1`) and is `is_rollover = false` / `is_fund_draw = false` so the
/// charges COUNT in budget math (`BUDGET-STATUS-DRIVES-INCLUSION-1`) and the Other
/// line establishes free-to-spend (`BUDGET-FUND-EARMARK-1` / D6 Model A: fund
/// balances are not separately subtracted). The Other line, when present, is the
/// LAST element (so the caller derives the charge count by subtracting it).
#[must_use]
fn build_opening_transactions(
    input: &OnboardingInput,
    month_id: MonthId,
    bucket_id: CategoryId,
    genesis_date: NaiveDate,
    now: DateTime<Utc>,
) -> Vec<Transaction> {
    let mut rows: Vec<Transaction> = Vec::new();

    for charge in &input.category_charges {
        if charge.spend_so_far.is_zero() {
            continue;
        }
        rows.push(Transaction {
            id: opening_charge_id(input.user_id, charge.category_id),
            user_id: input.user_id,
            month_id,
            category_id: Some(charge.category_id),
            account_id: None,
            date: genesis_date,
            // Opening charge = an expense (negative), magnitude = month-to-date
            // spend before tracking began. Settled => it COUNTS in budget math like
            // real spend (BUDGET-STATUS-DRIVES-INCLUSION-1).
            amount: -charge.spend_so_far,
            description: "Opening charge (month-to-date before tracking)".to_owned(),
            source: TransactionSource::Manual,
            plaid_transaction_id: None,
            status: TransactionStatus::Settled,
            income_kind: None,
            is_rollover: false,
            is_fund_draw: false,
            created_at: now,
            updated_at: now,
        });
    }

    // The starting rolling-Other line on the genesis month's rollover bucket.
    // is_rollover=false: this is the onboarding OPENING line, not a system
    // month-to-month rollover (the genesis month has no prior month to roll from).
    if !input.starting_other_balance.is_zero() {
        rows.push(Transaction {
            id: opening_other_id(input.user_id),
            user_id: input.user_id,
            month_id,
            category_id: Some(bucket_id),
            account_id: None,
            date: genesis_date,
            amount: input.starting_other_balance,
            description: "Opening rolling-Other balance".to_owned(),
            source: TransactionSource::Manual,
            plaid_transaction_id: None,
            status: TransactionStatus::Settled,
            income_kind: None,
            is_rollover: false,
            is_fund_draw: false,
            created_at: now,
            updated_at: now,
        });
    }

    rows
}

#[cfg(test)]
mod tests;
