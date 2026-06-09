//! Unit tests for the expected-expense match service (`SPEC §4.10` / `§12`,
//! `BUDGET-SETTLE-ON-MATCH-1`, build step A1).
//!
//! DB-free in-memory fakes (a `Mutex`-wrapped `Vec<Transaction>` upserting by id,
//! mirroring the production ON CONFLICT (pk) DO UPDATE so a re-match is
//! idempotent). The tests assert the two halves of the match-link contract:
//!   - **forward**: matching a real txn to an `expected` placeholder persists the
//!     link on the placeholder, and the linked pair counts EXACTLY ONCE in the
//!     month net (the placeholder drops out via `is_matched_placeholder`, the real
//!     txn counts) — never double, never zero;
//!   - **reverse**: `unmatch_by_real_transaction` uses the STORED link to find and
//!     restore the placeholder, clearing the link so it reserves budget again;
//!   - the match-rule guards (not-a-placeholder, already-matched, self-match,
//!     missing rows) are rejected loudly.
//!
//! ### Lint suppressions (test-only)
//!
//! The workspace denies `unwrap_used`, `expect_used`, and `panic` in production
//! code; tests intentionally use them, suppressed for this module only.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::any::Any;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};

use budget_domain::RepositoryError;
use budget_domain::enums::{TransactionSource, TransactionStatus};
use budget_domain::error::DomainError;
use budget_domain::ids::{CategoryId, MonthId, TransactionId, UserId};
use budget_domain::money::Money;
use budget_domain::projections::{CategorySpent, MonthNet};
use budget_domain::repositories::TransactionRepository;
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};

use super::*;

// ---------------------------------------------------------------------------
// Fakes (DB-free, upsert-by-id)
// ---------------------------------------------------------------------------

struct FakeUow;
impl UnitOfWork for FakeUow {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

type BoxedUowClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

struct FakeUowProvider;

#[async_trait]
impl UowProvider for FakeUowProvider {
    async fn run_boxed(
        &self,
        f: BoxedUowClosure<'_>,
    ) -> Result<Box<dyn Any + Send>, RepositoryError> {
        let uow = FakeUow;
        let handle: &dyn UnitOfWork = &uow;
        f(handle).await
    }
}

fn poisoned<T>(_e: std::sync::PoisonError<T>) -> RepositoryError {
    RepositoryError::Database("test mutex poisoned".to_owned())
}

#[derive(Default)]
struct MemTxnRepo {
    txns: Mutex<Vec<Transaction>>,
}

#[async_trait]
impl TransactionRepository for MemTxnRepo {
    async fn find_by_id(&self, id: TransactionId) -> Result<Option<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter().find(|t| t.id == id).cloned())
    }

    async fn list_for_month(&self, month_id: MonthId) -> Result<Vec<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .filter(|t| t.month_id == month_id)
            .cloned()
            .collect())
    }

    async fn list_for_category_in_month(
        &self,
        month_id: MonthId,
        category_id: CategoryId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .filter(|t| t.month_id == month_id && t.category_id == Some(category_id))
            .cloned()
            .collect())
    }

    async fn find_rollover_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|t| t.month_id == month_id && t.is_rollover)
            .cloned())
    }

    async fn find_by_plaid_transaction_id(
        &self,
        plaid_id: &str,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|t| t.plaid_transaction_id.as_deref() == Some(plaid_id))
            .cloned())
    }

    async fn list_expected_for_month(
        &self,
        month_id: MonthId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .filter(|t| t.month_id == month_id && t.status == TransactionStatus::Expected)
            .cloned()
            .collect())
    }

    async fn find_expected_matched_to(
        &self,
        real_transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        Ok(g.iter()
            .find(|t| t.matched_transaction_id == Some(real_transaction_id))
            .cloned())
    }

    async fn category_spent_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Vec<CategorySpent>, RepositoryError> {
        Ok(Vec::new())
    }

    /// Independent net oracle: settled + expected count, pending excluded, and a
    /// MATCHED expected placeholder is excluded (it links to a real txn that
    /// counts instead) — re-derived here, NOT via the production predicate, so the
    /// "counts exactly once" assertion is non-tautological
    /// (`BUDGET-SETTLE-ON-MATCH-1` / `BUDGET-NO-DOUBLE-CHARGE-1`).
    async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError> {
        let g = self.txns.lock().map_err(poisoned)?;
        let net: Money = g
            .iter()
            .filter(|t| {
                t.month_id == month_id
                    && matches!(
                        t.status,
                        TransactionStatus::Settled | TransactionStatus::Expected
                    )
                    && !(t.status == TransactionStatus::Expected
                        && t.matched_transaction_id.is_some())
            })
            .map(|t| t.amount)
            .sum();
        Ok(MonthNet { month_id, net })
    }

    async fn save(
        &self,
        transaction: &Transaction,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut g = self.txns.lock().map_err(poisoned)?;
        if let Some(slot) = g.iter_mut().find(|t| t.id == transaction.id) {
            *slot = transaction.clone();
        } else {
            g.push(transaction.clone());
        }
        Ok(())
    }

    async fn delete(
        &self,
        id: TransactionId,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut g = self.txns.lock().map_err(poisoned)?;
        g.retain(|t| t.id != id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Harness + builders
// ---------------------------------------------------------------------------

struct Harness {
    service: ExpectedExpenseService,
    repo: Arc<MemTxnRepo>,
    month_id: MonthId,
}

fn harness() -> Harness {
    let repo = Arc::new(MemTxnRepo::default());
    let service = ExpectedExpenseService::new(
        Arc::clone(&repo) as Arc<dyn TransactionRepository>,
        Arc::new(FakeUowProvider) as Arc<dyn UowProvider>,
    );
    Harness {
        service,
        repo,
        month_id: MonthId::generate(),
    }
}

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap_or(NaiveDate::MIN)
}

fn base(month_id: MonthId, amount: Money, status: TransactionStatus) -> Transaction {
    let now = Utc::now();
    Transaction {
        id: TransactionId::generate(),
        user_id: UserId::generate(),
        month_id,
        category_id: None,
        account_id: None,
        date: ymd(2026, 6, 15),
        amount,
        description: "t".to_owned(),
        source: TransactionSource::Manual,
        plaid_transaction_id: None,
        status,
        income_kind: None,
        is_rollover: false,
        is_fund_draw: false,
        matched_transaction_id: None,
        created_at: now,
        updated_at: now,
    }
}

async fn insert(repo: &MemTxnRepo, t: Transaction) -> TransactionId {
    let id = t.id;
    repo.save(&t, None).await.unwrap();
    id
}

// ---------------------------------------------------------------------------
// Forward match
// ---------------------------------------------------------------------------

#[tokio::test]
async fn match_persists_link_on_placeholder() {
    let h = harness();
    // -$800 AirBnB placeholder + the real $800 charge.
    let placeholder_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-800),
            TransactionStatus::Expected,
        ),
    )
    .await;
    let real_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-800),
            TransactionStatus::Settled,
        ),
    )
    .await;

    h.service
        .match_expected(placeholder_id, real_id, Utc::now())
        .await
        .unwrap();

    let placeholder = h.repo.find_by_id(placeholder_id).await.unwrap().unwrap();
    assert_eq!(
        placeholder.matched_transaction_id,
        Some(real_id),
        "the link must be persisted on the placeholder row"
    );
    assert!(
        placeholder.is_matched_placeholder(),
        "a linked expected row is a matched placeholder"
    );
}

#[tokio::test]
async fn matched_pair_counts_exactly_once_not_double() {
    let h = harness();
    // Both are the same -$800; if BOTH counted the net would be -$1600 (double),
    // if NEITHER it would be $0. The matched pair must net to exactly -$800.
    let placeholder_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-800),
            TransactionStatus::Expected,
        ),
    )
    .await;
    let real_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-800),
            TransactionStatus::Settled,
        ),
    )
    .await;

    // Before matching: placeholder (expected, counts) + real (settled, counts)
    // would double to -$1600.
    let before = h.repo.month_net(h.month_id).await.unwrap();
    assert_eq!(
        before.net,
        Money::from_major(-1600),
        "unmatched = double-counted"
    );

    h.service
        .match_expected(placeholder_id, real_id, Utc::now())
        .await
        .unwrap();

    let after = h.repo.month_net(h.month_id).await.unwrap();
    assert_eq!(
        after.net,
        Money::from_major(-800),
        "matched pair must count EXACTLY ONCE (BUDGET-SETTLE-ON-MATCH-1)"
    );
}

#[tokio::test]
async fn rematch_to_same_real_txn_is_idempotent() {
    let h = harness();
    let placeholder_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-50),
            TransactionStatus::Expected,
        ),
    )
    .await;
    let real_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-50),
            TransactionStatus::Settled,
        ),
    )
    .await;

    h.service
        .match_expected(placeholder_id, real_id, Utc::now())
        .await
        .unwrap();
    // Re-matching to the SAME real txn is a no-op (not an error).
    h.service
        .match_expected(placeholder_id, real_id, Utc::now())
        .await
        .unwrap();

    let net = h.repo.month_net(h.month_id).await.unwrap();
    assert_eq!(
        net.net,
        Money::from_major(-50),
        "still counted exactly once"
    );
}

#[tokio::test]
async fn rematch_to_different_real_txn_is_rejected() {
    let h = harness();
    let placeholder_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-50),
            TransactionStatus::Expected,
        ),
    )
    .await;
    let real_a = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-50),
            TransactionStatus::Settled,
        ),
    )
    .await;
    let real_b = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-50),
            TransactionStatus::Settled,
        ),
    )
    .await;

    h.service
        .match_expected(placeholder_id, real_a, Utc::now())
        .await
        .unwrap();
    let err = h
        .service
        .match_expected(placeholder_id, real_b, Utc::now())
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::Invariant(_)),
        "already-matched must reject"
    );
}

#[tokio::test]
async fn matching_a_non_placeholder_is_rejected() {
    let h = harness();
    let settled_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-50),
            TransactionStatus::Settled,
        ),
    )
    .await;
    let real_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-50),
            TransactionStatus::Settled,
        ),
    )
    .await;
    let err = h
        .service
        .match_expected(settled_id, real_id, Utc::now())
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::Invariant(_)),
        "non-placeholder must reject"
    );
}

#[tokio::test]
async fn matching_to_self_is_rejected() {
    let h = harness();
    let placeholder_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-50),
            TransactionStatus::Expected,
        ),
    )
    .await;
    let err = h
        .service
        .match_expected(placeholder_id, placeholder_id, Utc::now())
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::Invariant(_)),
        "self-match must reject"
    );
}

#[tokio::test]
async fn matching_a_missing_real_txn_is_rejected() {
    let h = harness();
    let placeholder_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-50),
            TransactionStatus::Expected,
        ),
    )
    .await;
    let missing_real = TransactionId::generate();
    let err = h
        .service
        .match_expected(placeholder_id, missing_real, Utc::now())
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::Invariant(_)),
        "missing real txn must reject"
    );
}

// ---------------------------------------------------------------------------
// Reverse path (un-match via the stored link)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unmatch_uses_stored_link_to_restore_placeholder_and_clears_it() {
    let h = harness();
    let placeholder_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-800),
            TransactionStatus::Expected,
        ),
    )
    .await;
    let real_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-800),
            TransactionStatus::Settled,
        ),
    )
    .await;
    h.service
        .match_expected(placeholder_id, real_id, Utc::now())
        .await
        .unwrap();

    // Reverse: the service finds the placeholder via the STORED link alone (only
    // the real txn id is passed in), restores it, and clears the link.
    let restored = h
        .service
        .unmatch_by_real_transaction(real_id, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        restored,
        Some(placeholder_id),
        "the reverse path must resolve the placeholder from the stored link"
    );

    let placeholder = h.repo.find_by_id(placeholder_id).await.unwrap().unwrap();
    assert_eq!(
        placeholder.matched_transaction_id, None,
        "the link must be cleared on un-match"
    );
    assert!(
        !placeholder.is_matched_placeholder(),
        "the restored placeholder reserves budget again"
    );

    // With the placeholder restored AND the real txn still present, the net is
    // back to the double figure — confirming the placeholder counts again.
    let net = h.repo.month_net(h.month_id).await.unwrap();
    assert_eq!(
        net.net,
        Money::from_major(-1600),
        "placeholder reserves budget again"
    );
}

#[tokio::test]
async fn unmatch_with_no_matched_placeholder_is_noop() {
    let h = harness();
    // A real txn nothing was ever matched to.
    let real_id = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-10),
            TransactionStatus::Settled,
        ),
    )
    .await;
    let restored = h
        .service
        .unmatch_by_real_transaction(real_id, Utc::now())
        .await
        .unwrap();
    assert_eq!(restored, None, "nothing matched -> idempotent no-op");
}

#[tokio::test]
async fn match_link_targets_exactly_one_placeholder() {
    // Two distinct placeholders + two distinct real charges; each match links its
    // own pair, and the reverse path restores ONLY the one keyed by the stored
    // link — never the other placeholder.
    let h = harness();
    let p1 = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-100),
            TransactionStatus::Expected,
        ),
    )
    .await;
    let p2 = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-200),
            TransactionStatus::Expected,
        ),
    )
    .await;
    let r1 = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-100),
            TransactionStatus::Settled,
        ),
    )
    .await;
    let r2 = insert(
        &h.repo,
        base(
            h.month_id,
            Money::from_major(-200),
            TransactionStatus::Settled,
        ),
    )
    .await;

    h.service.match_expected(p1, r1, Utc::now()).await.unwrap();
    h.service.match_expected(p2, r2, Utc::now()).await.unwrap();

    // find_expected_matched_to keys exactly one placeholder per real txn.
    assert_eq!(
        h.repo
            .find_expected_matched_to(r1)
            .await
            .unwrap()
            .map(|t| t.id),
        Some(p1)
    );
    assert_eq!(
        h.repo
            .find_expected_matched_to(r2)
            .await
            .unwrap()
            .map(|t| t.id),
        Some(p2)
    );

    // Un-matching r1 restores ONLY p1; p2 stays matched.
    h.service
        .unmatch_by_real_transaction(r1, Utc::now())
        .await
        .unwrap();
    assert!(
        !h.repo
            .find_by_id(p1)
            .await
            .unwrap()
            .unwrap()
            .is_matched_placeholder()
    );
    assert!(
        h.repo
            .find_by_id(p2)
            .await
            .unwrap()
            .unwrap()
            .is_matched_placeholder()
    );
}
