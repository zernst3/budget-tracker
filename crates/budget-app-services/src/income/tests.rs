//! Unit tests for the income engine (`SPEC §4.8`, `D5`) — step 6.
//!
//! Covers:
//!   - the BUILT semimonthly per-paycheck path (`2 × amount`, every month),
//!   - the BUILT hourly/variable degradation (blank amount -> `Money::ZERO`),
//!   - the STUBBED paths failing loudly-but-safely as
//!     [`ServiceError::UnsupportedIncome`] (no `panic!`/`todo!()`),
//!   - config-driven resolution off a persisted [`PaycheckConfig`] (read via the
//!     repository), and
//!   - surplus routing (default + per-transaction override) reusing the step-5
//!     [`FundService::contribute`] plumbing.
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

use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{FundKind, IncomeMode, PaycheckType, SurplusRouting};
use budget_domain::fund::Fund;
use budget_domain::ids::{
    BudgetId, CategoryId, FundId, MonthId, PaycheckConfigId, RepaymentObligationId, TransactionId,
    UserId,
};
use budget_domain::money::Money;
use budget_domain::paycheck_config::PaycheckConfig;
use budget_domain::repayment_obligation::RepaymentObligation;
use budget_domain::repositories::{
    BudgetRepository, FundRepository, PaycheckConfigRepository, TransactionRepository,
};
use budget_domain::transaction::Transaction;
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};
use budget_domain::{CategorySpent, MonthNet, RepositoryError};

use super::*;
use crate::fund::FundService;

// ---------------------------------------------------------------------------
// Config builders
// ---------------------------------------------------------------------------

fn cfg(mode: IncomeMode, ptype: PaycheckType, amount: Option<Money>) -> PaycheckConfig {
    PaycheckConfig {
        id: PaycheckConfigId::generate(),
        user_id: UserId::generate(),
        income_mode: mode,
        paycheck_type: ptype,
        amount,
        anchor_date: NaiveDate::from_ymd_opt(2026, 6, 1).expect("valid date"),
        surplus_routing: SurplusRouting::Buffer,
        smoothing_buffer: Money::ZERO,
    }
}

// ---------------------------------------------------------------------------
// resolve_per_month / from_config — BUILT paths
// ---------------------------------------------------------------------------

#[test]
fn semimonthly_per_paycheck_is_two_times_amount() {
    // SPEC §4.8 "Zach's own situation": semimonthly = always 2 paychecks/month.
    let config = cfg(
        IncomeMode::PerPaycheck,
        PaycheckType::Semimonthly,
        Some(Money::from_major(2_000)),
    );
    let exp = ConfigDrivenIncomeExpectation::from_config(&config).expect("built path");
    let user = config.user_id;
    // Same figure every month of the year (no anchor/cadence arithmetic).
    for month in 1..=12 {
        assert_eq!(
            exp.expected_income(user, 2026, month),
            Money::from_major(4_000),
            "semimonthly expectation must be 2 x amount in month {month}"
        );
    }
}

#[test]
fn blank_amount_degrades_to_zero_for_every_cadence_and_mode() {
    // SPEC §4.8: a blank amount is the hourly/variable degradation -> ZERO
    // expectation -> net = actual - 0 = actual flows straight into Other. This is
    // buildable for ANY nominal cadence/mode, so it is checked before the matrix.
    let modes = [IncomeMode::PerPaycheck, IncomeMode::Smoothed];
    let cadences = [
        PaycheckType::Semimonthly,
        PaycheckType::Biweekly,
        PaycheckType::Weekly,
        PaycheckType::Hourly,
    ];
    for mode in modes {
        for cadence in cadences {
            let config = cfg(mode, cadence, None);
            let exp = ConfigDrivenIncomeExpectation::from_config(&config)
                .expect("blank-amount degradation is built for every cadence/mode");
            assert_eq!(
                exp.expected_income(config.user_id, 2026, 6),
                Money::ZERO,
                "blank amount must degrade to ZERO ({mode:?}, {cadence:?})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// resolve_per_month — STUBBED paths fail loudly-but-safely (no panic/todo)
// ---------------------------------------------------------------------------

#[test]
fn smoothed_mode_is_unsupported_for_every_cadence() {
    // SPEC §4.8: the smoothed 12-month-average mode + its buffer are stubbed.
    for cadence in [
        PaycheckType::Semimonthly,
        PaycheckType::Biweekly,
        PaycheckType::Weekly,
    ] {
        let config = cfg(
            IncomeMode::Smoothed,
            cadence,
            Some(Money::from_major(2_000)),
        );
        let err = ConfigDrivenIncomeExpectation::from_config(&config)
            .expect_err("smoothed mode is stubbed");
        assert!(
            matches!(
                err,
                ServiceError::UnsupportedIncome {
                    mode: IncomeMode::Smoothed,
                    ..
                }
            ),
            "smoothed must surface UnsupportedIncome, got {err:?}"
        );
    }
}

#[test]
fn biweekly_and_weekly_per_paycheck_are_unsupported() {
    // SPEC §4.8: anchor-date per-month paycheck counting is stubbed.
    for cadence in [PaycheckType::Biweekly, PaycheckType::Weekly] {
        let config = cfg(
            IncomeMode::PerPaycheck,
            cadence,
            Some(Money::from_major(1_500)),
        );
        let err = ConfigDrivenIncomeExpectation::from_config(&config)
            .expect_err("biweekly/weekly resolution is stubbed");
        match err {
            ServiceError::UnsupportedIncome {
                mode: IncomeMode::PerPaycheck,
                cadence: c,
                ..
            } => assert_eq!(c, cadence),
            other => panic!("expected UnsupportedIncome for {cadence:?}, got {other:?}"),
        }
    }
}

#[test]
fn hourly_with_a_fixed_amount_is_a_loud_config_error() {
    // SPEC §4.8: hourly is the blank-amount path; a fixed amount on hourly is
    // contradictory and is surfaced loudly rather than guessed.
    let config = cfg(
        IncomeMode::PerPaycheck,
        PaycheckType::Hourly,
        Some(Money::from_major(1_000)),
    );
    let err =
        ConfigDrivenIncomeExpectation::from_config(&config).expect_err("hourly+amount is invalid");
    assert!(matches!(
        err,
        ServiceError::UnsupportedIncome {
            cadence: PaycheckType::Hourly,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// load() — config read via the repository (db.* stays in repositories)
// ---------------------------------------------------------------------------

struct FakePaycheckConfigRepo {
    store: Mutex<Vec<PaycheckConfig>>,
}

impl FakePaycheckConfigRepo {
    fn with(configs: Vec<PaycheckConfig>) -> Self {
        Self {
            store: Mutex::new(configs),
        }
    }
}

#[async_trait]
impl PaycheckConfigRepository for FakePaycheckConfigRepo {
    async fn find_for_user(
        &self,
        user_id: UserId,
    ) -> Result<Option<PaycheckConfig>, RepositoryError> {
        let store = self
            .store
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".to_owned()))?;
        Ok(store.iter().find(|c| c.user_id == user_id).cloned())
    }

    async fn save(
        &self,
        config: &PaycheckConfig,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".to_owned()))?;
        if let Some(slot) = store.iter_mut().find(|c| c.user_id == config.user_id) {
            *slot = config.clone();
        } else {
            store.push(config.clone());
        }
        Ok(())
    }
}

#[tokio::test]
async fn load_reads_config_and_drives_expectation_off_persisted_cadence() {
    // SPEC §4.8 (point 1): expected_income is DRIVEN off the persisted config,
    // not a hardcoded constructor.
    let config = cfg(
        IncomeMode::PerPaycheck,
        PaycheckType::Semimonthly,
        Some(Money::from_minor(245_000)), // $2,450 per paycheck
    );
    let user = config.user_id;
    let repo = FakePaycheckConfigRepo::with(vec![config]);

    let exp = ConfigDrivenIncomeExpectation::load(&repo, user)
        .await
        .expect("semimonthly config loads");
    assert_eq!(
        exp.expected_income(user, 2026, 6),
        Money::from_minor(490_000),
        "2 x $2,450 = $4,900"
    );
}

#[tokio::test]
async fn load_missing_config_is_a_domain_error() {
    let repo = FakePaycheckConfigRepo::with(vec![]);
    let err = ConfigDrivenIncomeExpectation::load(&repo, UserId::generate())
        .await
        .expect_err("no config -> error");
    assert!(matches!(err, ServiceError::Domain(_)));
}

#[tokio::test]
async fn load_surfaces_unsupported_for_stubbed_config() {
    let config = cfg(
        IncomeMode::Smoothed,
        PaycheckType::Semimonthly,
        Some(Money::from_major(2_000)),
    );
    let user = config.user_id;
    let repo = FakePaycheckConfigRepo::with(vec![config]);
    let err = ConfigDrivenIncomeExpectation::load(&repo, user)
        .await
        .expect_err("smoothed is stubbed");
    assert!(matches!(err, ServiceError::UnsupportedIncome { .. }));
}

#[test]
fn expectation_is_zero_for_a_mismatched_user() {
    // Defensive depth (SPEC §9 single-user): a call routed for a different user
    // returns ZERO, never another user's figure.
    let config = cfg(
        IncomeMode::PerPaycheck,
        PaycheckType::Semimonthly,
        Some(Money::from_major(2_000)),
    );
    let exp = ConfigDrivenIncomeExpectation::from_config(&config).expect("built");
    assert_eq!(
        exp.expected_income(UserId::generate(), 2026, 6),
        Money::ZERO
    );
}

// ---------------------------------------------------------------------------
// Surplus routing — reuses the step-5 FundService::contribute plumbing
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

struct FakeFundRepo {
    funds: Mutex<Vec<Fund>>,
}

#[async_trait]
impl FundRepository for FakeFundRepo {
    async fn find_by_id(&self, id: FundId) -> Result<Option<Fund>, RepositoryError> {
        Ok(self
            .funds
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|f| f.id == id)
            .cloned())
    }
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<Fund>, RepositoryError> {
        Ok(self
            .funds
            .lock()
            .map_err(poisoned)?
            .iter()
            .filter(|f| f.user_id == user_id)
            .cloned()
            .collect())
    }
    async fn save(
        &self,
        fund: &Fund,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        let mut funds = self.funds.lock().map_err(poisoned)?;
        if let Some(slot) = funds.iter_mut().find(|f| f.id == fund.id) {
            *slot = fund.clone();
        } else {
            funds.push(fund.clone());
        }
        Ok(())
    }
    async fn find_obligation(
        &self,
        _id: RepaymentObligationId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        Ok(None)
    }
    async fn list_active_obligations(
        &self,
        _user_id: UserId,
    ) -> Result<Vec<RepaymentObligation>, RepositoryError> {
        Ok(vec![])
    }
    async fn find_obligation_for_transaction(
        &self,
        _transaction_id: TransactionId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        Ok(None)
    }
    async fn find_deficit_obligation_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Option<RepaymentObligation>, RepositoryError> {
        Ok(None)
    }
    async fn list_buffer_financed_transaction_ids(
        &self,
        _user_id: UserId,
    ) -> Result<Vec<TransactionId>, RepositoryError> {
        Ok(vec![])
    }
    async fn save_obligation(
        &self,
        _obligation: &RepaymentObligation,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

struct FakeTxnRepo {
    saved: Mutex<Vec<Transaction>>,
}

#[async_trait]
impl TransactionRepository for FakeTxnRepo {
    async fn find_by_id(&self, _id: TransactionId) -> Result<Option<Transaction>, RepositoryError> {
        Ok(None)
    }
    async fn list_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        Ok(vec![])
    }
    async fn list_for_category_in_month(
        &self,
        _month_id: MonthId,
        _category_id: CategoryId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        Ok(vec![])
    }
    async fn find_rollover_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        Ok(None)
    }
    async fn find_by_plaid_transaction_id(
        &self,
        _plaid_transaction_id: &str,
    ) -> Result<Option<Transaction>, RepositoryError> {
        Ok(None)
    }
    async fn list_expected_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Vec<Transaction>, RepositoryError> {
        Ok(vec![])
    }
    async fn find_expected_matched_to(
        &self,
        _real_transaction_id: TransactionId,
    ) -> Result<Option<Transaction>, RepositoryError> {
        Ok(None)
    }
    async fn category_spent_for_month(
        &self,
        _month_id: MonthId,
    ) -> Result<Vec<CategorySpent>, RepositoryError> {
        Ok(vec![])
    }
    async fn month_net(&self, month_id: MonthId) -> Result<MonthNet, RepositoryError> {
        Ok(MonthNet {
            month_id,
            net: Money::ZERO,
        })
    }
    async fn save(
        &self,
        transaction: &Transaction,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        self.saved
            .lock()
            .map_err(poisoned)?
            .push(transaction.clone());
        Ok(())
    }
    async fn delete(
        &self,
        _id: TransactionId,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

struct FakeBudgetRepo;

#[async_trait]
impl BudgetRepository for FakeBudgetRepo {
    async fn find_by_id(&self, _id: BudgetId) -> Result<Option<Budget>, RepositoryError> {
        Ok(None)
    }
    async fn find_active_for_date(
        &self,
        _user_id: UserId,
        _date: NaiveDate,
    ) -> Result<Option<Budget>, RepositoryError> {
        Ok(None)
    }
    async fn find_current(&self, _user_id: UserId) -> Result<Option<Budget>, RepositoryError> {
        Ok(None)
    }
    async fn list_for_user(&self, _user_id: UserId) -> Result<Vec<Budget>, RepositoryError> {
        Ok(vec![])
    }
    async fn list_categories(
        &self,
        _budget_id: BudgetId,
    ) -> Result<Vec<Category>, RepositoryError> {
        Ok(vec![])
    }
    async fn find_category(&self, _id: CategoryId) -> Result<Option<Category>, RepositoryError> {
        Ok(None)
    }
    async fn find_rollover_bucket(
        &self,
        _budget_id: BudgetId,
    ) -> Result<Option<Category>, RepositoryError> {
        Ok(None)
    }
    async fn save(
        &self,
        _budget: &Budget,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
    async fn save_category(
        &self,
        _category: &Category,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

fn buffer_fund(user_id: UserId) -> Fund {
    Fund {
        id: FundId::generate(),
        user_id,
        name: "Buffer".to_owned(),
        kind: FundKind::Buffer,
        balance: Money::ZERO,
        target_balance: Some(Money::from_major(5_000)),
        compulsory_repayment: true,
        created_at: Utc::now(),
    }
}

fn build_router(funds: Arc<FakeFundRepo>, txns: Arc<FakeTxnRepo>) -> IncomeSurplusRouter {
    let budgets = Arc::new(FakeBudgetRepo);
    let uow = Arc::new(FakeUowProvider);
    let fund_service = Arc::new(FundService::new(funds, txns, budgets, uow));
    IncomeSurplusRouter::new(fund_service)
}

#[test]
fn effective_routing_prefers_the_override() {
    assert_eq!(
        IncomeSurplusRouter::effective_routing(SurplusRouting::Buffer, None),
        SurplusRouting::Buffer
    );
    assert_eq!(
        IncomeSurplusRouter::effective_routing(
            SurplusRouting::Buffer,
            Some(SurplusRouting::ThisMonth)
        ),
        SurplusRouting::ThisMonth,
        "the per-transaction override wins over the config default"
    );
}

#[tokio::test]
async fn route_this_month_is_a_noop() {
    // SPEC §4.8: this_month leaves the surplus raising Other (the D5 formula
    // already does it) — no fund contribution, no transaction posted.
    let user = UserId::generate();
    let funds = Arc::new(FakeFundRepo {
        funds: Mutex::new(vec![]),
    });
    let txns = Arc::new(FakeTxnRepo {
        saved: Mutex::new(vec![]),
    });
    let router = build_router(Arc::clone(&funds), Arc::clone(&txns));

    router
        .route_surplus(
            SurplusRouting::ThisMonth,
            Money::from_major(300),
            None,
            MonthId::generate(),
            CategoryId::generate(),
            NaiveDate::from_ymd_opt(2026, 6, 15).expect("date"),
            Utc::now(),
        )
        .await
        .expect("this_month route succeeds");
    let _ = user;
    assert!(
        txns.saved.lock().expect("lock").is_empty(),
        "this_month must post no transaction"
    );
}

#[tokio::test]
async fn route_buffer_contributes_into_the_fund() {
    // SPEC §4.8 + BUDGET-FUND-EARMARK-1: a buffer route reuses
    // FundService::contribute — the surplus becomes a counted Other-bucket
    // expense raising the fund balance by the same amount.
    let user = UserId::generate();
    let fund = buffer_fund(user);
    let fund_id = fund.id;
    let funds = Arc::new(FakeFundRepo {
        funds: Mutex::new(vec![fund]),
    });
    let txns = Arc::new(FakeTxnRepo {
        saved: Mutex::new(vec![]),
    });
    let router = build_router(Arc::clone(&funds), Arc::clone(&txns));

    let earmark = CategoryId::generate();
    router
        .route_surplus(
            SurplusRouting::Buffer,
            Money::from_major(300),
            Some(fund_id),
            MonthId::generate(),
            earmark,
            NaiveDate::from_ymd_opt(2026, 6, 15).expect("date"),
            Utc::now(),
        )
        .await
        .expect("buffer route succeeds");

    // Fund balance rose by the surplus.
    let stored = funds.find_by_id(fund_id).await.expect("ok").expect("fund");
    assert_eq!(stored.balance, Money::from_major(300));

    // A counted Other-bucket expense (negative, is_fund_draw=false) was posted.
    let saved = txns.saved.lock().expect("lock");
    assert_eq!(saved.len(), 1);
    let posted = &saved[0];
    assert_eq!(posted.amount, Money::from_major(-300));
    assert_eq!(posted.category_id, Some(earmark));
    assert!(
        !posted.is_fund_draw,
        "a contribution must COUNT (not a draw)"
    );
}

#[tokio::test]
async fn route_buffer_without_a_target_fund_errors() {
    let funds = Arc::new(FakeFundRepo {
        funds: Mutex::new(vec![]),
    });
    let txns = Arc::new(FakeTxnRepo {
        saved: Mutex::new(vec![]),
    });
    let router = build_router(funds, txns);
    let err = router
        .route_surplus(
            SurplusRouting::Buffer,
            Money::from_major(300),
            None, // missing target fund
            MonthId::generate(),
            CategoryId::generate(),
            NaiveDate::from_ymd_opt(2026, 6, 15).expect("date"),
            Utc::now(),
        )
        .await
        .expect_err("buffer route needs a target fund");
    assert!(matches!(err, ServiceError::Domain(_)));
}

#[tokio::test]
async fn route_non_positive_surplus_errors() {
    let funds = Arc::new(FakeFundRepo {
        funds: Mutex::new(vec![]),
    });
    let txns = Arc::new(FakeTxnRepo {
        saved: Mutex::new(vec![]),
    });
    let router = build_router(funds, txns);
    let err = router
        .route_surplus(
            SurplusRouting::ThisMonth,
            Money::ZERO,
            None,
            MonthId::generate(),
            CategoryId::generate(),
            NaiveDate::from_ymd_opt(2026, 6, 15).expect("date"),
            Utc::now(),
        )
        .await
        .expect_err("zero surplus is invalid");
    assert!(matches!(err, ServiceError::Domain(_)));
}
