//! The expected-expense match service (`SPEC §4.10` / `§12`,
//! `BUDGET-SETTLE-ON-MATCH-1`, build step A1).
//!
//! An **expected expense** is a manually-entered `status='expected'` placeholder
//! that RESERVES budget in its target month before the real charge lands
//! (`SPEC §4.10`). When the real charge arrives (Plaid or manual), the user
//! **matches** it to the placeholder; the placeholder is then settled by the real
//! transaction — never both, so the pair counts exactly once
//! (`BUDGET-NO-DOUBLE-CHARGE-1`).
//!
//! This service owns the **forward** match path. It persists the link as
//! `transactions.matched_transaction_id` on the placeholder row
//! (migration m0003): once set, the placeholder is a *matched placeholder*
//! ([`budget_domain::transaction::Transaction::is_matched_placeholder`]) and is
//! excluded from budget math — the real transaction counts instead.
//!
//! The **reverse** path (Plaid `removed` un-matches the placeholder) lives in the
//! Plaid sync engine (`budget-infrastructure`), where the removal is observed: it
//! reads `matched_transaction_id` via
//! [`budget_domain::repositories::TransactionRepository::find_expected_matched_to`],
//! restores the placeholder, and clears the link before deleting the real row.
//!
//! Holds `Arc<dyn _>` collaborators (`SERVICE-DI-1`), contains no `db.*`
//! (`ARCH-STRICT-LAYERING-1`), and writes through the [`UowProvider`] closure
//! (`SERVICE-TX-1`).

use std::sync::Arc;

use chrono::{DateTime, Utc};

use budget_domain::enums::TransactionStatus;
use budget_domain::error::DomainError;
use budget_domain::ids::TransactionId;
use budget_domain::repositories::TransactionRepository;
use budget_domain::uow::{UnitOfWork, UowProvider, UowProviderExt};

/// Orchestrates the expected-expense match-and-replace use case (`SPEC §4.10`).
pub struct ExpectedExpenseService {
    transactions: Arc<dyn TransactionRepository>,
    uow: Arc<dyn UowProvider>,
}

impl ExpectedExpenseService {
    /// Wire the service from its collaborators (`SERVICE-DI-1`).
    #[must_use]
    pub fn new(transactions: Arc<dyn TransactionRepository>, uow: Arc<dyn UowProvider>) -> Self {
        Self { transactions, uow }
    }

    /// Match an `expected` placeholder to the real transaction that settles it,
    /// persisting the link on the placeholder row (`BUDGET-SETTLE-ON-MATCH-1`).
    ///
    /// After this, the placeholder is a *matched placeholder* and drops out of
    /// budget math (the real transaction counts instead), so the placeholder/real
    /// pair counts exactly once (`BUDGET-NO-DOUBLE-CHARGE-1`).
    ///
    /// Validates the match before writing:
    ///   - the placeholder must exist and be `status='expected'`,
    ///   - the placeholder must not already be matched (idempotent re-match to the
    ///     SAME real txn is a no-op; matching to a DIFFERENT one is rejected),
    ///   - the real transaction must exist,
    ///   - a placeholder is never matched to itself.
    ///
    /// # Errors
    /// [`DomainError::Invariant`] if any precondition fails;
    /// [`DomainError::Repository`] on any persistence failure.
    pub async fn match_expected(
        &self,
        placeholder_id: TransactionId,
        real_transaction_id: TransactionId,
        now: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        if placeholder_id == real_transaction_id {
            return Err(DomainError::Invariant(
                "a placeholder cannot be matched to itself".to_owned(),
            ));
        }

        let mut placeholder = self
            .transactions
            .find_by_id(placeholder_id)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!("expected placeholder {placeholder_id} not found"))
            })?;

        if !matches!(placeholder.status, TransactionStatus::Expected) {
            return Err(DomainError::Invariant(format!(
                "transaction {placeholder_id} is not an expected placeholder (status={:?})",
                placeholder.status
            )));
        }

        // Idempotent: re-matching to the same real txn is a no-op; re-matching to a
        // different one is rejected (un-match first).
        match placeholder.matched_transaction_id {
            Some(existing) if existing == real_transaction_id => return Ok(()),
            Some(existing) => {
                return Err(DomainError::Invariant(format!(
                    "placeholder {placeholder_id} is already matched to {existing}; \
                     un-match before re-matching"
                )));
            }
            None => {}
        }

        // The real transaction must exist (the charge we are settling against).
        if self
            .transactions
            .find_by_id(real_transaction_id)
            .await?
            .is_none()
        {
            return Err(DomainError::Invariant(format!(
                "real transaction {real_transaction_id} not found"
            )));
        }

        placeholder.matched_transaction_id = Some(real_transaction_id);
        placeholder.updated_at = now;

        let transactions = Arc::clone(&self.transactions);
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    transactions.save(&placeholder, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(())
    }

    /// Un-match the placeholder linked to a given real transaction, restoring it
    /// and clearing the link (`BUDGET-SETTLE-ON-MATCH-1`).
    ///
    /// This is the inverse of [`match_expected`]. It looks the placeholder up by
    /// the stored link (`find_expected_matched_to`), clears
    /// `matched_transaction_id`, and saves — restoring the placeholder to an active
    /// (budget-reserving) `expected` state. Returns the restored placeholder's id,
    /// or `None` if no placeholder was matched to that real transaction (idempotent
    /// no-op).
    ///
    /// The Plaid `removed` reverse path in the sync engine implements the same
    /// restore inline (it must run inside the same unit of work as the row delete);
    /// this method is the service-layer entry for a manual un-match.
    ///
    /// # Errors
    /// [`DomainError::Repository`] on any persistence failure.
    pub async fn unmatch_by_real_transaction(
        &self,
        real_transaction_id: TransactionId,
        now: DateTime<Utc>,
    ) -> Result<Option<TransactionId>, DomainError> {
        let Some(mut placeholder) = self
            .transactions
            .find_expected_matched_to(real_transaction_id)
            .await?
        else {
            return Ok(None);
        };
        let restored_id = placeholder.id;
        placeholder.matched_transaction_id = None;
        placeholder.updated_at = now;

        let transactions = Arc::clone(&self.transactions);
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    transactions.save(&placeholder, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(Some(restored_id))
    }
}

#[cfg(test)]
mod tests;
