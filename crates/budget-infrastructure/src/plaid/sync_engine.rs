//! The cursor-sync + rolling-reconcile engine (`SPEC §6`).
//!
//! Implements the domain [`PlaidSyncEngine`] port. It owns the mechanics that
//! need both the mappers crate (the single sign-flip site, `BUDGET-PLAID-SIGN-1`)
//! and the repositories, which is why it lives in `budget-infrastructure` rather
//! than `budget-app-services` (the latter stays WASM-clean / domain-only).
//!
//! ## What `sync_item` does, in order
//!
//! 1. Loop `/transactions/sync` from the stored cursor until `has_more = false`.
//! 2. For each page, in ONE unit-of-work transaction (`SERVICE-TX-1`):
//!    - **added**: skip any row dated before `tracking_start_date`
//!      (`BUDGET-CUTOVER-1`); dedup by `plaid_transaction_id` (UNIQUE); insert
//!      uncategorized (`category_id = NULL`) with status from Plaid's `pending`
//!      flag (`SPEC §4.4`).
//!    - **modified**: update in place; the pending->settled transition arrives
//!      here. If the settled row references a now-superseded pending row via
//!      `pending_transaction_id`, the pending row is removed so the same charge
//!      isn't counted twice.
//!    - **removed**: delete the row. Deleting a settled row dated to a fixed
//!      category automatically reverses that category's settlement, because
//!      settlement is predicate-based (`fixed_category_spent`,
//!      `BUDGET-SETTLE-ON-MATCH-1`) — once no settled rows remain the placeholder
//!      stands back in. No stored match-link exists to un-do (see the build
//!      report's ROUTE-1 note on expected-expense un-match).
//!    - persist the cursor.
//! 3. After the cursor loop, run the rolling 30-day reconcile, clamped to
//!    `max(today - 30d, tracking_start_date)` (`SPEC §6`, `BUDGET-CUTOVER-1`).
//!
//! Idempotent: re-running never duplicates (dedup by `plaid_transaction_id`) and
//! never double-applies (cursor advances; modifications are upserts).
//!
//! ## Shape
//!
//! The per-row apply helpers live on an `Arc`-shareable [`SyncApplier`] (it holds
//! only the repository handles), so they can be moved into the `'static`
//! unit-of-work closures by cloning the `Arc`. [`SeaOrmPlaidSyncEngine`] holds
//! that applier plus the [`PlaidApi`] + [`UowProvider`] it drives.

use async_trait::async_trait;
use chrono::{Datelike, Duration, NaiveDate, Utc};

use budget_domain::enums::TransactionStatus;
use budget_domain::ids::{AccountId, MonthId, PlaidItemId, TransactionId, UserId};
use budget_domain::plaid_api::{
    PlaidApi, PlaidError, PlaidSyncEngine, PlaidTransaction, SyncSummary,
};
use budget_domain::repositories::{MonthRepository, PlaidItemRepository, TransactionRepository};
use budget_domain::uow::{UnitOfWork, UowProvider, UowProviderExt};

use budget_mappers::transactions as txn_mapper;

use std::sync::Arc;

/// `SeaORM`/repository-backed [`PlaidSyncEngine`].
pub struct SeaOrmPlaidSyncEngine {
    plaid: Arc<dyn PlaidApi>,
    applier: Arc<SyncApplier>,
    uow: Arc<dyn UowProvider>,
}

/// The repository-backed per-row apply mechanics, `Arc`-shareable so they move
/// into the `'static` unit-of-work closures.
struct SyncApplier {
    transactions: Arc<dyn TransactionRepository>,
    months: Arc<dyn MonthRepository>,
    plaid_items: Arc<dyn PlaidItemRepository>,
}

impl SeaOrmPlaidSyncEngine {
    /// Wire the engine from its collaborators (`SERVICE-DI-1`).
    #[must_use]
    pub fn new(
        plaid: Arc<dyn PlaidApi>,
        transactions: Arc<dyn TransactionRepository>,
        months: Arc<dyn MonthRepository>,
        plaid_items: Arc<dyn PlaidItemRepository>,
        uow: Arc<dyn UowProvider>,
    ) -> Self {
        Self {
            plaid,
            applier: Arc::new(SyncApplier {
                transactions,
                months,
                plaid_items,
            }),
            uow,
        }
    }

    /// The rolling 30-day reconcile lower bound (`SPEC §6`, `BUDGET-CUTOVER-1`):
    /// `max(today - 30d, tracking_start_date)`. The clamp IS the cutover guard —
    /// a reconcile never reaches a row dated before day 1.
    fn reconcile_lower_bound(today: NaiveDate, tracking_start_date: NaiveDate) -> NaiveDate {
        let thirty_days_ago = today - Duration::days(30);
        thirty_days_ago.max(tracking_start_date)
    }
}

impl SyncApplier {
    /// The status a Plaid row maps to (`SPEC §4.4`): pending = excluded from
    /// budget math, settled = included.
    const fn status_for(pending: bool) -> TransactionStatus {
        if pending {
            TransactionStatus::Pending
        } else {
            TransactionStatus::Settled
        }
    }

    /// Resolve the month a Plaid transaction belongs to. Plaid `date` is a
    /// calendar date, so month-membership is the date's own `(year, month)` — no
    /// timezone conversion is needed for a date-typed field (`SPEC §6`; D2 governs
    /// instants, not calendar dates). Returns `None` if the month row does not
    /// exist (the caller ran lazy month-init first, so this is an edge / skip).
    async fn resolve_month(
        &self,
        user_id: UserId,
        date: NaiveDate,
    ) -> Result<Option<MonthId>, PlaidError> {
        let year = date.year();
        let month = i32::try_from(date.month()).unwrap_or(1);
        let found = self.months.find_by_year_month(user_id, year, month).await?;
        Ok(found.map(|m| m.id))
    }

    /// Resolve the domain account id for a Plaid account id, if linked. A missing
    /// account is non-fatal: the transaction is still ingested with
    /// `account_id = None` (naming is best-effort, `SPEC §6`).
    async fn resolve_account(
        &self,
        plaid_account_id: &str,
    ) -> Result<Option<AccountId>, PlaidError> {
        let acct = self
            .plaid_items
            .find_account_by_plaid_id(plaid_account_id)
            .await?;
        Ok(acct.map(|a| a.id))
    }

    /// Apply one Plaid `added` row. Cutover-guarded, deduped, idempotent.
    async fn apply_added(
        &self,
        user_id: UserId,
        dto: &PlaidTransaction,
        tracking_start_date: NaiveDate,
        uow: &dyn UnitOfWork,
    ) -> Result<AddedOutcome, PlaidError> {
        // BUDGET-CUTOVER-1: never ingest a transaction dated before day 1.
        if dto.date < tracking_start_date {
            return Ok(AddedOutcome::SkippedPreGenesis);
        }
        // Dedup by the UNIQUE plaid_transaction_id (SPEC §5/§6).
        if self
            .transactions
            .find_by_plaid_transaction_id(&dto.transaction_id)
            .await?
            .is_some()
        {
            return Ok(AddedOutcome::AlreadyPresent);
        }
        let Some(month_id) = self.resolve_month(user_id, dto.date).await? else {
            return Err(PlaidError::Mapping(format!(
                "no month row for transaction dated {}",
                dto.date
            )));
        };
        let account_id = self.resolve_account(&dto.account_id).await?;
        let status = Self::status_for(dto.pending);
        // BUDGET-PLAID-SIGN-1: the flip happens once, in the mapper.
        let txn = txn_mapper::plaid_dto_to_domain(
            dto,
            TransactionId::generate(),
            user_id,
            month_id,
            account_id,
            status,
            Utc::now(),
        )
        .map_err(|e| PlaidError::Mapping(e.to_string()))?;
        self.transactions.save(&txn, Some(uow)).await?;
        Ok(AddedOutcome::Inserted)
    }

    /// Apply one Plaid `modified` row (update in place, incl. pending->settled).
    ///
    /// Cutover-guarded. Upsert keyed on `plaid_transaction_id` (idempotent). The
    /// user's manual category assignment is preserved across the update. The
    /// pending->settled transition: when the settled version references a DISTINCT
    /// pending row via `pending_transaction_id`, that pending row is removed so the
    /// charge counts once.
    async fn apply_modified(
        &self,
        user_id: UserId,
        dto: &PlaidTransaction,
        tracking_start_date: NaiveDate,
        uow: &dyn UnitOfWork,
    ) -> Result<bool, PlaidError> {
        if dto.date < tracking_start_date {
            return Ok(false);
        }
        let Some(month_id) = self.resolve_month(user_id, dto.date).await? else {
            return Err(PlaidError::Mapping(format!(
                "no month row for modified transaction dated {}",
                dto.date
            )));
        };
        let account_id = self.resolve_account(&dto.account_id).await?;
        let status = Self::status_for(dto.pending);

        let existing = self
            .transactions
            .find_by_plaid_transaction_id(&dto.transaction_id)
            .await?;
        let (id, category_id) = existing
            .as_ref()
            .map_or((TransactionId::generate(), None), |e| (e.id, e.category_id));

        let mut txn = txn_mapper::plaid_dto_to_domain(
            dto,
            id,
            user_id,
            month_id,
            account_id,
            status,
            Utc::now(),
        )
        .map_err(|e| PlaidError::Mapping(e.to_string()))?;
        // Keep the user's category assignment (a Plaid modify must not wipe it).
        txn.category_id = category_id;
        self.transactions.save(&txn, Some(uow)).await?;

        // pending->settled: drop the superseded pending row if it is a DISTINCT
        // row (Plaid sometimes settles under a NEW transaction_id, linking back
        // via pending_transaction_id).
        self.drop_superseded_pending(dto, uow).await?;
        Ok(true)
    }

    /// On a pending->settled transition, delete the superseded pending row (a
    /// DISTINCT row Plaid settled under a new id, linked via
    /// `pending_transaction_id`) so the charge counts exactly once
    /// (`BUDGET-NO-DOUBLE-CHARGE-1`). A no-op when there is no distinct pending
    /// predecessor.
    async fn drop_superseded_pending(
        &self,
        settled: &PlaidTransaction,
        uow: &dyn UnitOfWork,
    ) -> Result<(), PlaidError> {
        let Some(pending_id) = settled
            .pending_transaction_id
            .as_ref()
            .filter(|p| *p != &settled.transaction_id)
        else {
            return Ok(());
        };
        let Some(superseded) = self
            .transactions
            .find_by_plaid_transaction_id(pending_id)
            .await?
        else {
            return Ok(());
        };
        self.transactions.delete(superseded.id, Some(uow)).await?;
        Ok(())
    }

    /// Apply one Plaid `removed` id (delete; settlement reverses via the
    /// predicate, `BUDGET-SETTLE-ON-MATCH-1`). Idempotent: a re-removed id no-ops.
    async fn apply_removed(
        &self,
        plaid_transaction_id: &str,
        uow: &dyn UnitOfWork,
    ) -> Result<bool, PlaidError> {
        let Some(existing) = self
            .transactions
            .find_by_plaid_transaction_id(plaid_transaction_id)
            .await?
        else {
            return Ok(false);
        };
        // Deleting the row reverses any predicate-based settlement it had caused:
        // a fixed category's spent = settled ? sum(rows) : placeholder
        // (BUDGET-NO-DOUBLE-CHARGE-1 / BUDGET-SETTLE-ON-MATCH-1). Once no settled
        // rows remain, the placeholder stands back in automatically.
        self.transactions.delete(existing.id, Some(uow)).await?;
        Ok(true)
    }
}

/// Outcome of attempting to apply a Plaid `added` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddedOutcome {
    /// A new row was inserted.
    Inserted,
    /// Skipped because it predates `tracking_start_date` (`BUDGET-CUTOVER-1`).
    SkippedPreGenesis,
    /// Already present (deduped by `plaid_transaction_id`).
    AlreadyPresent,
}

/// Carry a [`PlaidError`] across the unit-of-work closure boundary (typed on
/// [`budget_domain::error::RepositoryError`]) without losing it. Non-repository
/// Plaid failures wrap into [`budget_domain::error::RepositoryError::Database`]
/// so the transaction rolls back and the cause surfaces.
fn plaid_err_to_repo(e: PlaidError) -> budget_domain::error::RepositoryError {
    match e {
        PlaidError::Repository(r) => r,
        other => budget_domain::error::RepositoryError::Database(other.to_string()),
    }
}

#[async_trait]
impl PlaidSyncEngine for SeaOrmPlaidSyncEngine {
    async fn sync_item(
        &self,
        item_id: PlaidItemId,
        user_id: UserId,
        access_token: &str,
        tracking_start_date: NaiveDate,
        today: NaiveDate,
    ) -> Result<SyncSummary, PlaidError> {
        let mut summary = SyncSummary::default();

        // ---- 1. Cursor sync loop ------------------------------------------
        let mut cursor = self.applier.plaid_items.get_sync_cursor(item_id).await?;
        loop {
            let page = self
                .plaid
                .transactions_sync(access_token, cursor.as_deref())
                .await?;
            let has_more = page.has_more;
            let next_cursor = page.next_cursor.clone();
            cursor = Some(next_cursor.clone());

            // Apply the whole page atomically (SERVICE-TX-1): added/modified/
            // removed + the cursor advance commit together.
            let applier = Arc::clone(&self.applier);
            let page_summary = self
                .uow
                .run(move |uow: &dyn UnitOfWork| {
                    Box::pin(async move {
                        let mut s = SyncSummary::default();
                        for dto in &page.added {
                            match applier
                                .apply_added(user_id, dto, tracking_start_date, uow)
                                .await
                                .map_err(plaid_err_to_repo)?
                            {
                                AddedOutcome::Inserted => s.added += 1,
                                AddedOutcome::SkippedPreGenesis => s.skipped_pre_genesis += 1,
                                AddedOutcome::AlreadyPresent => {}
                            }
                        }
                        for dto in &page.modified {
                            if applier
                                .apply_modified(user_id, dto, tracking_start_date, uow)
                                .await
                                .map_err(plaid_err_to_repo)?
                            {
                                s.modified += 1;
                            }
                        }
                        for removed_id in &page.removed {
                            if applier
                                .apply_removed(removed_id, uow)
                                .await
                                .map_err(plaid_err_to_repo)?
                            {
                                s.removed += 1;
                            }
                        }
                        applier
                            .plaid_items
                            .update_sync_cursor(item_id, &next_cursor, Some(uow))
                            .await?;
                        Ok(s)
                    })
                })
                .await?;
            summary.merge(page_summary);
            if !has_more {
                break;
            }
        }

        // ---- 2. Rolling 30-day reconcile (SPEC §6, BUDGET-CUTOVER-1) -------
        // Re-pull a fresh full sync (cursor = None) and re-apply only rows in
        // [lower_bound, today] as `modified` upserts, catching amount/category/
        // pending drift the incremental stream missed. The lower-bound clamp is
        // the cutover guard.
        let lower_bound = Self::reconcile_lower_bound(today, tracking_start_date);
        let reconcile_page = self.plaid.transactions_sync(access_token, None).await?;
        let in_window: Vec<PlaidTransaction> = reconcile_page
            .added
            .into_iter()
            .chain(reconcile_page.modified)
            .filter(|t| t.date >= lower_bound && t.date <= today)
            .collect();
        if !in_window.is_empty() {
            let count = in_window.len();
            let applier = Arc::clone(&self.applier);
            self.uow
                .run(move |uow: &dyn UnitOfWork| {
                    Box::pin(async move {
                        for dto in &in_window {
                            applier
                                .apply_modified(user_id, dto, tracking_start_date, uow)
                                .await
                                .map_err(plaid_err_to_repo)?;
                        }
                        Ok(())
                    })
                })
                .await?;
            summary.reconciled += count;
        }

        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_lower_bound_clamps_to_tracking_start() {
        // tracking_start within the last 30 days -> clamp to it (cutover guard).
        let today = NaiveDate::from_ymd_opt(2026, 6, 8).unwrap_or(NaiveDate::MIN);
        let tsd = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap_or(NaiveDate::MIN);
        let bound = SeaOrmPlaidSyncEngine::reconcile_lower_bound(today, tsd);
        assert_eq!(bound, tsd, "clamp to tracking_start when it is more recent");
    }

    #[test]
    fn reconcile_lower_bound_is_thirty_days_when_tracking_start_is_old() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 8).unwrap_or(NaiveDate::MIN);
        let tsd = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap_or(NaiveDate::MIN);
        let bound = SeaOrmPlaidSyncEngine::reconcile_lower_bound(today, tsd);
        assert_eq!(
            bound,
            NaiveDate::from_ymd_opt(2026, 5, 9).unwrap_or(NaiveDate::MIN),
            "30 days back when tracking_start is older than that"
        );
    }

    #[test]
    fn status_maps_from_pending_flag() {
        assert_eq!(SyncApplier::status_for(true), TransactionStatus::Pending);
        assert_eq!(SyncApplier::status_for(false), TransactionStatus::Settled);
    }
}
