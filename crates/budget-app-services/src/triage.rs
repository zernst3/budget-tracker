//! Pending-triage use case (`SPEC §7`, BACKEND-3).
//!
//! The "Pull -> Pending -> triage" intake flow. A manual Plaid Pull
//! ([`crate::plaid_sync::PlaidSyncService::sync_user`]) lands new SETTLED bank
//! charges; the ones that are not yet categorized (`status = 'settled'` AND
//! `category_id IS NULL`) form the triage inbox ([`TriageService::pending_inbox`]).
//! Plaid `pending` credit-card charges (`status = 'pending'`, `SPEC §4.4`) carry a
//! different status and never enter the inbox.
//!
//! ## Atomic triage (`SERVICE-TX-1`, `BUDGET-NO-DOUBLE-CHARGE-1`)
//!
//! [`TriageService::triage`] applies, for ONE pending transaction, in ONE unit of
//! work:
//!   1. set its **category**,
//!   2. set its optional **comment**, and
//!   3. apply exactly ONE [`Treatment`] — the three `SPEC §4.9` settlement paths:
//!      - [`Treatment::PayFromSavings`] — a fund DRAW from a savings/surplus fund
//!        (the money was pre-saved; `BUDGET-NO-DOUBLE-CHARGE-1` — not re-charged),
//!      - [`Treatment::SpreadOverMonths`] — buffer-financed: the full price is
//!        tracked now (zero net month impact) and a `repayment_obligation` (D7)
//!        amortizes it over N compulsory installments,
//!      - [`Treatment::PayDirectly`] — a normal in-month expense (the DEFAULT and
//!        most common case): the row simply becomes a counting expense once it has
//!        a category.
//!
//! Because the category/comment edit and the treatment commit together, a triage
//! either fully applies or not at all. After it succeeds the row has a
//! `category_id`, so it LEAVES the inbox (the next [`Self::pending_inbox`] read no
//! longer returns it) and appears in the month ledger on its transaction date.
//!
//! The money math is NOT re-implemented here: treatments (a) and (b) delegate to
//! [`crate::fund::FundService::prepare_existing_fund_draw`] /
//! [`crate::fund::FundService::prepare_existing_buffer_finance`], which are the same
//! fund-draw / obligation arithmetic the large-purchase create-path uses. Treatment
//! (c) needs no fund and is a plain category + comment edit (the row's own amount is
//! the in-month expense).

use std::sync::Arc;

use chrono::{DateTime, Utc};

use budget_domain::error::DomainError;
use budget_domain::ids::{CategoryId, FundId, TransactionId, UserId};
use budget_domain::repositories::TransactionRepository;
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowProvider, UowProviderExt};

use crate::fund::FundService;

/// One pending-inbox row (`SPEC §7`): a settled, not-yet-categorized bank charge
/// awaiting triage. Carries exactly the fields the triage UI needs to present the
/// choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTransaction {
    /// Stable transaction id — the triage target.
    pub id: TransactionId,
    /// Transaction date (the day it settled).
    pub date: chrono::NaiveDate,
    /// Signed amount (`Money`; negative = expense). The treatment math uses its
    /// magnitude.
    pub amount: budget_domain::money::Money,
    /// Plaid / merchant description (read-only).
    pub description: String,
    /// The account the charge is on, if linked (`None` for a manual/unlinked row).
    pub account_id: Option<budget_domain::ids::AccountId>,
}

impl From<&Transaction> for PendingTransaction {
    fn from(t: &Transaction) -> Self {
        Self {
            id: t.id,
            date: t.date,
            amount: t.amount,
            description: t.description.clone(),
            account_id: t.account_id,
        }
    }
}

/// The treatment applied to a pending transaction at triage (`SPEC §7` / `§4.9`):
/// exactly one of the three settlement paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Treatment {
    /// (a) Pay from a savings accrual — a fund DRAW from a savings/surplus fund.
    /// The pre-saved money covers it, so the charge is NOT re-charged against the
    /// month (`BUDGET-NO-DOUBLE-CHARGE-1`).
    PayFromSavings {
        /// The savings/surplus fund to draw down.
        fund_id: FundId,
    },
    /// (b) Spread over the next few months — buffer-financed (D7). The full price
    /// is tracked now with zero net month impact; a `repayment_obligation`
    /// amortizes it over `months` compulsory installments that flow into the buffer.
    SpreadOverMonths {
        /// The buffer fund fronting the cash (`compulsory_repayment = true`).
        fund_id: FundId,
        /// Number of compulsory monthly installments.
        months: i32,
    },
    /// (c) Pay directly through the budget — a normal in-month expense. The DEFAULT
    /// and most common case: the row's own amount is the expense; no fund involved.
    PayDirectly,
}

/// The input to one atomic triage action (`SPEC §7`).
#[derive(Debug, Clone)]
pub struct TriageInput {
    /// The pending transaction being triaged.
    pub transaction_id: TransactionId,
    /// The category to assign (required — categorizing is what removes the row from
    /// the inbox).
    pub category_id: CategoryId,
    /// An optional free-text comment (`transactions.comment`, `SPEC §5`/`§7`).
    pub comment: Option<String>,
    /// The single treatment to apply.
    pub treatment: Treatment,
}

/// The outcome of a successful triage (`SPEC §7`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageOutcome {
    /// The triaged transaction id.
    pub transaction_id: TransactionId,
    /// The obligation created when the treatment was
    /// [`Treatment::SpreadOverMonths`]; `None` for the other treatments.
    pub obligation_id: Option<budget_domain::ids::RepaymentObligationId>,
}

/// Pending-triage use case (`SPEC §7`).
///
/// Holds `Arc<dyn _>` collaborators (`SERVICE-DI-1`); no `db.*` lives here
/// (`ARCH-STRICT-LAYERING-1`). The atomic triage runs in one [`UowProvider`]
/// closure (`SERVICE-TX-1`). The fund treatments reuse [`FundService`]'s money math
/// rather than re-deriving it.
pub struct TriageService {
    transactions: Arc<dyn TransactionRepository>,
    funds: Arc<FundService>,
    uow: Arc<dyn UowProvider>,
}

impl TriageService {
    /// Wire the service from its collaborators (`SERVICE-DI-1`).
    #[must_use]
    pub fn new(
        transactions: Arc<dyn TransactionRepository>,
        funds: Arc<FundService>,
        uow: Arc<dyn UowProvider>,
    ) -> Self {
        Self {
            transactions,
            funds,
            uow,
        }
    }

    /// The triage inbox for `user_id` (`SPEC §7`): every settled, not-yet-
    /// categorized transaction (`status = 'settled'` AND `category_id IS NULL`),
    /// oldest first.
    ///
    /// Plaid `pending` charges (`SPEC §4.4`) carry `status = 'pending'` and so are
    /// excluded by construction — they never appear here.
    ///
    /// # Errors
    /// [`DomainError`] on any persistence failure.
    pub async fn pending_inbox(
        &self,
        user_id: UserId,
    ) -> Result<Vec<PendingTransaction>, DomainError> {
        let rows = self.transactions.list_pending_inbox(user_id).await?;
        Ok(rows.iter().map(PendingTransaction::from).collect())
    }

    /// Atomically triage one pending transaction (`SPEC §7`): set category + comment
    /// and apply exactly one [`Treatment`], all in ONE unit of work
    /// (`SERVICE-TX-1`).
    ///
    /// The category assignment is what removes the row from the inbox: after this
    /// returns `Ok`, the transaction has a `category_id` and the next
    /// [`Self::pending_inbox`] read no longer returns it. The treatment decides the
    /// budget bookkeeping (`SPEC §4.9`); the three paths each count the money
    /// EXACTLY ONCE (`BUDGET-NO-DOUBLE-CHARGE-1` / `BUDGET-FUND-EARMARK-1`).
    ///
    /// # Errors
    /// [`DomainError::Invariant`] if the transaction is absent or is not a settled
    /// inbox row (already categorized, or not settled — guarding against a
    /// double-triage or triaging a Plaid `pending` charge); the treatment's own
    /// errors (missing/wrong-kind fund, non-positive month count); or any
    /// persistence failure.
    pub async fn triage(
        &self,
        input: TriageInput,
        now: DateTime<Utc>,
    ) -> Result<TriageOutcome, DomainError> {
        // Load + validate the row is a real inbox row BEFORE any write: it must be
        // settled and uncategorized. This rejects double-triage (already has a
        // category) and triaging a Plaid `pending` charge (SPEC §4.4).
        let mut txn = self
            .transactions
            .find_by_id(input.transaction_id)
            .await?
            .ok_or_else(|| {
                DomainError::Invariant(format!("transaction {} not found", input.transaction_id))
            })?;
        if txn.status != budget_domain::enums::TransactionStatus::Settled {
            return Err(DomainError::IllegalState(
                "only a settled transaction can be triaged (a Plaid `pending` charge is excluded, SPEC §4.4)"
                    .to_owned(),
            ));
        }
        if txn.category_id.is_some() {
            return Err(DomainError::IllegalState(
                "transaction is already categorized and has left the triage inbox".to_owned(),
            ));
        }

        // Apply the category + comment edit (common to every treatment).
        txn.category_id = Some(input.category_id);
        txn.comment = input.comment.clone();
        txn.updated_at = now;
        let txn_id = txn.id;

        // Resolve the treatment's money math OUTSIDE the unit of work (the
        // DomainError-fallible fund loads + validation live in FundService, which is
        // the single home of the money logic — no duplication here). What survives
        // into the closure is a flat set of entities to persist, so the closure only
        // does repo `.save()` calls (one transaction, SERVICE-TX-1).
        let plan = match input.treatment {
            Treatment::PayDirectly => {
                // The default: the row's own amount is the in-month expense.
                // is_fund_draw stays false so it COUNTS once it has a category.
                TriagePlan {
                    fund: None,
                    obligation: None,
                }
            }
            Treatment::PayFromSavings { fund_id } => {
                // (a) fund draw — FundService applies the same draw math as a surplus
                // draw / sinking payout (mutates txn -> is_fund_draw=true, returns the
                // balance-decremented fund).
                let fund = self
                    .funds
                    .prepare_existing_fund_draw(fund_id, &mut txn, now)
                    .await?;
                TriagePlan {
                    fund: Some(fund),
                    obligation: None,
                }
            }
            Treatment::SpreadOverMonths { fund_id, months } => {
                // (b) buffer-financed — FundService applies the same
                // obligation/installment math as a buffer-financed large purchase
                // (draws the buffer, builds the obligation; txn stays the tracking
                // row).
                let (fund, obligation) = self
                    .funds
                    .prepare_existing_buffer_finance(fund_id, &mut txn, months, now)
                    .await?;
                TriagePlan {
                    fund: Some(fund),
                    obligation: Some(obligation),
                }
            }
        };

        let obligation_id = plan.obligation.as_ref().map(|o| o.id);

        let transactions = Arc::clone(&self.transactions);
        let funds_repo = self.funds.fund_repo();
        // ONE unit of work: the category/comment edit and the treatment's writes
        // commit together or not at all (SERVICE-TX-1). The transaction is saved
        // FIRST so an obligation's transaction_id FK is satisfiable.
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    transactions.save(&txn, Some(uow)).await?;
                    if let Some(fund) = plan.fund {
                        funds_repo.save(&fund, Some(uow)).await?;
                    }
                    if let Some(obligation) = plan.obligation {
                        funds_repo.save_obligation(&obligation, Some(uow)).await?;
                    }
                    Ok::<(), budget_domain::RepositoryError>(())
                })
            })
            .await?;

        Ok(TriageOutcome {
            transaction_id: txn_id,
            obligation_id,
        })
    }
}

/// The flat set of entities one triage commits, resolved before the unit of work
/// so the closure does only repo writes (`SERVICE-TX-1`).
struct TriagePlan {
    /// The balance-changed fund (treatments a/b); `None` for pay-directly.
    fund: Option<budget_domain::fund::Fund>,
    /// The created repayment obligation (treatment b); `None` otherwise.
    obligation: Option<budget_domain::repayment_obligation::RepaymentObligation>,
}

#[cfg(test)]
mod tests;
