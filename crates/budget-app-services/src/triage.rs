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
    /// `BUDGET-TRANSFER-EXCLUDE-1` / `SPEC §4.11` D10 — the transfer AUTO-SUGGEST
    /// flag. `true` when this row's captured `plaid_category` (Plaid's
    /// `personal_finance_category.detailed`) indicates a credit-card payment or an
    /// account-to-account transfer
    /// ([`budget_domain::plaid_category_suggests_transfer`]). When `true`, the triage
    /// UI pre-selects the [`Treatment::Transfer`] treatment for the user to confirm.
    ///
    /// SUGGESTION ONLY: it is computed (read-only) from `plaid_category`, NOT a stored
    /// fact, and triage NEVER auto-sets `is_transfer` from it — the user confirms or
    /// overrides, so a Plaid mis-tag can never silently mutate budget math. A manual /
    /// unlinked row, or a Plaid row with no transfer-indicating category, is `false`.
    pub suggested_transfer: bool,
}

impl From<&Transaction> for PendingTransaction {
    fn from(t: &Transaction) -> Self {
        Self {
            id: t.id,
            date: t.date,
            amount: t.amount,
            description: t.description.clone(),
            account_id: t.account_id,
            // SPEC §4.11 D10: compute the suggestion from the captured Plaid category
            // (None -> false). This is the ONLY place the suggestion is derived; it
            // never writes is_transfer (the user confirms at triage).
            suggested_transfer: t
                .plaid_category
                .as_deref()
                .is_some_and(budget_domain::plaid_category_suggests_transfer),
        }
    }
}

/// The treatment applied to a pending transaction at triage (`SPEC §7` / `§4.9` /
/// `§4.11`): exactly one of the four settlement paths.
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
    /// (d) Transfer / card payment — NOT an expense (`BUDGET-TRANSFER-EXCLUDE-1`,
    /// `SPEC §4.11` D10). The row is an internal account movement (a credit-card
    /// payment, a checking↔savings transfer), not spending or income. It sets
    /// `is_transfer = true` and is then TRACKED but EXCLUDED from budget math on
    /// BOTH legs (the funding-account outflow AND the destination-account inflow) via
    /// the single [`counts_in_month_expense_remaining`] predicate and its SQL-aggregate
    /// mirrors. It requires NO category (the `is_transfer` flag, not a category, is
    /// what removes the row from the inbox) and touches NO fund or obligation (it is
    /// not a fund draw or a financing). Auto-suggested from
    /// [`PendingTransaction::suggested_transfer`] but NEVER auto-applied — the user
    /// confirms.
    ///
    /// [`counts_in_month_expense_remaining`]: budget_domain::predicates::counts_in_month_expense_remaining
    Transfer,
}

/// The input to one atomic triage action (`SPEC §7` / `§4.11`).
#[derive(Debug, Clone)]
pub struct TriageInput {
    /// The pending transaction being triaged.
    pub transaction_id: TransactionId,
    /// The category to assign. REQUIRED for the three expense treatments
    /// ([`Treatment::PayDirectly`], [`Treatment::PayFromSavings`],
    /// [`Treatment::SpreadOverMonths`]) — categorizing is what removes the row from
    /// the inbox for those. It is `None` for [`Treatment::Transfer`], which removes
    /// the row from the inbox by setting `is_transfer = true` instead of by
    /// categorizing (`SPEC §4.11` D10); a category supplied alongside Transfer is
    /// ignored.
    pub category_id: Option<CategoryId>,
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

    /// All funds for `user_id` (`SPEC §4.9`): the buffer pool and any surplus
    /// funds. The triage UI offers these as the targets for the two fund-backed
    /// treatments — `PayFromSavings` (draw any fund) and `SpreadOverMonths` (which
    /// requires a `Buffer` fund). The service does not pre-filter by kind here: the
    /// treatment-time validation in [`crate::fund::FundService`] enforces the
    /// kind rule, and surfacing all funds lets the UI label each one's kind.
    ///
    /// # Errors
    /// [`DomainError`] on any persistence failure.
    pub async fn list_funds(
        &self,
        user_id: UserId,
    ) -> Result<Vec<budget_domain::Fund>, DomainError> {
        Ok(self.funds.fund_repo().list_for_user(user_id).await?)
    }

    /// Atomically triage one pending transaction (`SPEC §7` / `§4.11`): apply exactly
    /// one [`Treatment`] (plus the optional comment), all in ONE unit of work
    /// (`SERVICE-TX-1`).
    ///
    /// What removes the row from the inbox depends on the treatment:
    ///   - the three EXPENSE treatments ([`Treatment::PayDirectly`],
    ///     [`Treatment::PayFromSavings`], [`Treatment::SpreadOverMonths`]) assign the
    ///     required `category_id`; after this returns `Ok` the row has a category and
    ///     the next [`Self::pending_inbox`] read no longer returns it. Each path
    ///     counts the money EXACTLY ONCE (`BUDGET-NO-DOUBLE-CHARGE-1` /
    ///     `BUDGET-FUND-EARMARK-1`).
    ///   - [`Treatment::Transfer`] (`SPEC §4.11` D10) sets `is_transfer = true` and
    ///     assigns NO category; the inbox predicate (`status='settled' AND category_id
    ///     IS NULL AND is_transfer = false`) drops it via the flag. It touches NO fund
    ///     or obligation and is EXCLUDED from budget math on both legs via
    ///     [`counts_in_month_expense_remaining`] (`BUDGET-TRANSFER-EXCLUDE-1`). It is
    ///     never auto-applied — the caller is acting on the user's confirmation of the
    ///     [`PendingTransaction::suggested_transfer`] suggestion.
    ///
    /// # Errors
    /// [`DomainError::IllegalState`] if the transaction is not a settled inbox row
    /// (already categorized, already a transfer, or not settled — guarding against a
    /// double-triage or triaging a Plaid `pending` charge);
    /// [`DomainError::Invariant`] if the transaction is absent or an expense treatment
    /// is missing its required `category_id`; the treatment's own errors
    /// (missing/wrong-kind fund, non-positive month count); or any persistence
    /// failure.
    ///
    /// [`counts_in_month_expense_remaining`]: budget_domain::predicates::counts_in_month_expense_remaining
    pub async fn triage(
        &self,
        input: TriageInput,
        now: DateTime<Utc>,
    ) -> Result<TriageOutcome, DomainError> {
        // Load + validate the row is a real inbox row BEFORE any write: it must be
        // settled, uncategorized, and not already a transfer. This rejects
        // double-triage (already has a category OR already a transfer) and triaging a
        // Plaid `pending` charge (SPEC §4.4).
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
        // BUDGET-TRANSFER-EXCLUDE-1: a row already flagged as a transfer has left the
        // inbox via is_transfer=true; re-triaging it is rejected like a re-triage of a
        // categorized row (symmetry with the category guard above).
        if txn.is_transfer {
            return Err(DomainError::IllegalState(
                "transaction is already a transfer and has left the triage inbox".to_owned(),
            ));
        }

        // The Transfer treatment (SPEC §4.11 D10) is structurally different from the
        // three expense treatments: it sets is_transfer=true, assigns NO category, and
        // touches NO fund/obligation. Handle it on its own path so the category-
        // required expense flow stays untouched.
        if input.treatment == Treatment::Transfer {
            return self.triage_transfer(txn, input.comment, now).await;
        }

        // Expense treatments require a category — categorizing is what removes the row
        // from the inbox for these three paths.
        let category_id = input.category_id.ok_or_else(|| {
            DomainError::Invariant(
                "a category is required for every triage treatment except Transfer".to_owned(),
            )
        })?;

        // Apply the category + comment edit (common to the three expense treatments).
        txn.category_id = Some(category_id);
        txn.comment = input.comment.clone();
        txn.updated_at = now;
        let txn_id = txn.id;

        // Resolve the treatment's money math OUTSIDE the unit of work (the
        // DomainError-fallible fund loads + validation live in FundService, which is
        // the single home of the money logic — no duplication here). What survives
        // into the closure is a flat set of entities to persist, so the closure only
        // does repo `.save()` calls (one transaction, SERVICE-TX-1).
        let plan = match input.treatment {
            Treatment::Transfer => {
                // Unreachable: handled above on its own path before category resolution.
                // Kept as an explicit arm so the match stays exhaustive without a
                // catch-all that could silently swallow a future treatment.
                unreachable!("Transfer is handled before category resolution")
            }
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

    /// Apply the [`Treatment::Transfer`] path (`SPEC §4.11` D10,
    /// `BUDGET-TRANSFER-EXCLUDE-1`): flag the row `is_transfer = true`, assign NO
    /// category, touch NO fund or obligation, and persist in ONE unit of work
    /// (`SERVICE-TX-1`).
    ///
    /// The caller has already loaded + validated `txn` is a settled, uncategorized,
    /// not-already-transfer inbox row. After this commits, the row satisfies
    /// `is_transfer = true`, so the pending-inbox predicate (`status='settled' AND
    /// category_id IS NULL AND is_transfer = false`) no longer returns it — the row
    /// leaves the inbox via the flag, not a category. The row is then EXCLUDED from
    /// budget math on both legs by [`counts_in_month_expense_remaining`] and its SQL
    /// mirrors.
    ///
    /// [`counts_in_month_expense_remaining`]: budget_domain::predicates::counts_in_month_expense_remaining
    async fn triage_transfer(
        &self,
        mut txn: Transaction,
        comment: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<TriageOutcome, DomainError> {
        // The ONLY mutations: set the transfer flag and the optional comment.
        // category_id stays None (a transfer needs no category); is_fund_draw stays
        // whatever it was (default false) — a transfer is NOT a fund draw, and we do
        // not touch funds/obligations.
        txn.is_transfer = true;
        txn.comment = comment;
        txn.updated_at = now;
        let txn_id = txn.id;

        let transactions = Arc::clone(&self.transactions);
        // ONE unit of work: a single transaction save (SERVICE-TX-1). No fund or
        // obligation write — a transfer is not financing.
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    transactions.save(&txn, Some(uow)).await?;
                    Ok::<(), budget_domain::RepositoryError>(())
                })
            })
            .await?;

        Ok(TriageOutcome {
            transaction_id: txn_id,
            // A transfer creates no repayment obligation.
            obligation_id: None,
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
