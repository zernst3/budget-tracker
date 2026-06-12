//! Mock-only catch-up engine tests (`docs/DRIP_REALTIME_DESIGN.md §Phase 7.3`,
//! `ORCH-NEW-PATH-TESTS-1`).
//!
//! Covers: idempotency / re-entrancy (apply twice → no change), chronological
//! compounding, DRIP-on vs DRIP-off, buffer + floor correctness, and suppression
//! of `pay_date <= baseline_as_of`. Everything runs against in-memory port mocks;
//! no DB, no network.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;

use budget_domain::RepositoryError;
use budget_domain::enums::AccountType;
use budget_domain::ids::{PositionId, UserId};
use budget_domain::money::Money;
use budget_domain::portfolio::{
    CashBalance, CashBalanceSource, DividendEvent, DividendSource, DividendSourceError,
    DividendSourceKind, DripApplication, Position, ShareProvenance, Ticker,
};
use budget_domain::repositories::{
    CashBalanceRepository, DividendEventCache, DripApplicationRepository,
};

use super::config::DripConfig;
use super::{DripCatchUpService, PayDatePriceSource, compute_accretion};
use crate::error::ServiceError;

// ===========================================================================
// In-memory port mocks
// ===========================================================================

/// A dividend source returning a fixed per-ticker event list, filtered by `since`.
#[derive(Default)]
struct FixtureDividends {
    events: HashMap<String, Vec<DividendEvent>>,
}

#[async_trait]
impl DividendSource for FixtureDividends {
    async fn dividends_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, DividendSourceError> {
        Ok(self
            .events
            .get(ticker.as_str())
            .into_iter()
            .flatten()
            .filter(|e| e.pay_date > since)
            .cloned()
            .collect())
    }
}

/// An in-memory dividend cache keyed by `(ticker, pay_date)`.
#[derive(Default)]
struct MemCache {
    rows: Mutex<HashMap<(String, NaiveDate), DividendEvent>>,
}

#[async_trait]
impl DividendEventCache for MemCache {
    async fn find_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, RepositoryError> {
        let mut out: Vec<DividendEvent> = self
            .rows
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".into()))?
            .values()
            .filter(|e| e.ticker.as_str() == ticker.as_str() && e.pay_date > since)
            .cloned()
            .collect();
        out.sort_by_key(|e| e.pay_date);
        Ok(out)
    }

    async fn upsert_many(&self, events: &[DividendEvent]) -> Result<(), RepositoryError> {
        let mut map = self
            .rows
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".into()))?;
        for e in events {
            map.insert((e.ticker.as_str().to_owned(), e.pay_date), e.clone());
        }
        Ok(())
    }
}

/// A pay-date price source: a fixed price for every date, or `None` for held dates.
struct FixedPrice {
    price: Option<Money>,
    /// Dates that explicitly resolve to `None` (held), overriding `price`.
    held: Vec<NaiveDate>,
}

impl FixedPrice {
    fn at(cents: i64) -> Self {
        Self {
            price: Some(Money::from_minor(cents)),
            held: vec![],
        }
    }
    fn at_with_hold(cents: i64, held: Vec<NaiveDate>) -> Self {
        Self {
            price: Some(Money::from_minor(cents)),
            held,
        }
    }
}

#[async_trait]
impl PayDatePriceSource for FixedPrice {
    async fn price_on(
        &self,
        _ticker: &Ticker,
        pay_date: NaiveDate,
    ) -> Result<Option<Money>, ServiceError> {
        if self.held.contains(&pay_date) {
            return Ok(None);
        }
        Ok(self.price)
    }
}

/// An in-memory drip-applications store enforcing the `(position_id, pay_date)`
/// unique guard (apply-once), exactly like the real ON CONFLICT DO NOTHING.
#[derive(Default)]
struct MemApplications {
    rows: Mutex<Vec<DripApplication>>,
}

#[async_trait]
impl DripApplicationRepository for MemApplications {
    async fn apply_if_absent(
        &self,
        application: &DripApplication,
        _uow: Option<&dyn budget_domain::uow::UnitOfWork>,
    ) -> Result<bool, RepositoryError> {
        let mut rows = self
            .rows
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".into()))?;
        let exists = rows.iter().any(|r| {
            r.position_id == application.position_id && r.pay_date == application.pay_date
        });
        if exists {
            return Ok(false);
        }
        rows.push(application.clone());
        Ok(true)
    }

    async fn list_for_position(
        &self,
        position_id: PositionId,
    ) -> Result<Vec<DripApplication>, RepositoryError> {
        let mut out: Vec<DripApplication> = self
            .rows
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".into()))?
            .iter()
            .filter(|r| r.position_id == position_id)
            .cloned()
            .collect();
        out.sort_by_key(|r| r.pay_date);
        Ok(out)
    }
}

/// An in-memory cash-balance store keyed by `account_label`.
#[derive(Default)]
struct MemBalances {
    rows: Mutex<HashMap<String, CashBalance>>,
}

#[async_trait]
impl CashBalanceSource for MemBalances {
    async fn balances_for_user(
        &self,
        _user_id: UserId,
    ) -> Result<Vec<CashBalance>, RepositoryError> {
        Ok(self
            .rows
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".into()))?
            .values()
            .cloned()
            .collect())
    }
}

#[async_trait]
impl CashBalanceRepository for MemBalances {
    async fn upsert(&self, balance: &CashBalance) -> Result<(), RepositoryError> {
        self.rows
            .lock()
            .map_err(|_| RepositoryError::Database("poisoned".into()))?
            .insert(balance.account_label.clone(), balance.clone());
        Ok(())
    }
}

// ===========================================================================
// Fixtures
// ===========================================================================

fn ticker(s: &str) -> Ticker {
    Ticker::try_new(s).unwrap()
}

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn baseline(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
}

fn event(sym: &str, pay: NaiveDate, cents: i64) -> DividendEvent {
    DividendEvent {
        ticker: ticker(sym),
        ex_date: pay - chrono::Duration::days(7),
        pay_date: pay,
        amount_per_share: Money::from_minor(cents),
        source: DividendSourceKind::Mock,
    }
}

fn position(shares: i64, drip: bool, baseline_as_of: DateTime<Utc>) -> Position {
    Position {
        id: PositionId::generate(),
        user_id: UserId::generate(),
        ticker: ticker("AAPL"),
        account_label: "Brokerage".to_owned(),
        account_type: AccountType::Investment,
        shares: Decimal::from(shares),
        cost_basis: None,
        drip_enabled: drip,
        baseline_as_of,
        created_at: baseline_as_of,
        updated_at: baseline_as_of,
    }
}

/// Build a service over the given dividend fixture + price + config, sharing the
/// application store so a test can assert idempotency across runs.
fn service(
    events: Vec<DividendEvent>,
    price: FixedPrice,
    config: DripConfig,
    apps: Arc<MemApplications>,
    balances: Arc<MemBalances>,
) -> DripCatchUpService {
    let mut map: HashMap<String, Vec<DividendEvent>> = HashMap::new();
    for e in events {
        map.entry(e.ticker.as_str().to_owned()).or_default().push(e);
    }
    DripCatchUpService::new(
        Arc::new(FixtureDividends { events: map }),
        Arc::new(MemCache::default()),
        Arc::new(price),
        apps,
        balances,
        config,
    )
}

// ===========================================================================
// Pure §3 math tests (buffer + floor + compounding)
// ===========================================================================

#[test]
fn buffer_and_floor_scope_to_accretion_only() {
    // baseline 30 shares; one $0.78/share dividend; price $20.
    // raw_new = (0.78 × 30) / 20 = 23.4 / 20 = 1.17 shares.
    // × buffer_factor 0.90 = 1.053 → floor 3dp = 1.053.
    let cfg = DripConfig::default();
    let ev = event("AAPL", date(2026, 5, 15), 78);
    let computed = compute_accretion(
        Decimal::from(30),
        true,
        &[(ev, Money::from_minor(2000))],
        cfg,
    );
    assert_eq!(computed.len(), 1);
    assert_eq!(computed[0].shares_added, Decimal::new(1053, 3));
    assert!(computed[0].cash_added.is_zero(), "DRIP-on accrues no cash");
}

#[test]
fn floor_truncates_does_not_round_up() {
    // Construct a raw value whose 4th-dp would round UP but must floor DOWN.
    // amount $1.00/share, 10 shares, price $7 → raw_new = (1×10)/7 = 1.428571...
    // × 0.90 = 1.2857142... → floor 3dp = 1.285 (a round would give 1.286).
    let cfg = DripConfig::default();
    let ev = event("AAPL", date(2026, 5, 15), 100);
    let computed = compute_accretion(
        Decimal::from(10),
        true,
        &[(ev, Money::from_minor(700))],
        cfg,
    );
    assert_eq!(
        computed[0].shares_added,
        Decimal::new(1285, 3),
        "floor truncates the 4th dp down (1.285), never rounds up to 1.286"
    );
}

#[test]
fn chronological_compounding_uses_prior_accretion() {
    // Two dividends; the second compounds on the first's accretion.
    // baseline 100; price $10 each; amount $1/share each.
    // e1: raw = (1×100)/10 = 10; ×0.9 = 9.000 → +9.000. held now 109.
    // e2: raw = (1×109)/10 = 10.9; ×0.9 = 9.81 → +9.810. total accreted 18.810.
    let cfg = DripConfig::default();
    let e1 = event("AAPL", date(2026, 2, 15), 100);
    let e2 = event("AAPL", date(2026, 5, 15), 100);
    let priced = vec![
        (e2.clone(), Money::from_minor(1000)), // deliberately out of order
        (e1.clone(), Money::from_minor(1000)),
    ];
    let computed = compute_accretion(Decimal::from(100), true, &priced, cfg);
    // Sorted chronological: e1 first.
    assert_eq!(computed[0].pay_date, date(2026, 2, 15));
    assert_eq!(computed[0].shares_added, Decimal::new(9000, 3));
    assert_eq!(computed[1].pay_date, date(2026, 5, 15));
    assert_eq!(computed[1].shares_added, Decimal::new(9810, 3));
}

#[test]
fn drip_off_yields_exact_cash_no_buffer() {
    // DRIP off: cash = amount × shares_held = 0.78 × 30 = 23.40 exactly, no buffer.
    let cfg = DripConfig::default();
    let ev = event("AAPL", date(2026, 5, 15), 78);
    let computed = compute_accretion(
        Decimal::from(30),
        false,
        &[(ev, Money::from_minor(2000))],
        cfg,
    );
    assert_eq!(computed[0].shares_added, Decimal::ZERO);
    assert_eq!(computed[0].cash_added, Money::from_minor(2340));
}

// ===========================================================================
// Service tests (idempotency, suppression, DRIP-on/off end-to-end)
// ===========================================================================

#[tokio::test]
async fn idempotent_reentrancy_applies_once() {
    // Two runs over the same dividend post exactly one application.
    let apps = Arc::new(MemApplications::default());
    let balances = Arc::new(MemBalances::default());
    let pos = position(100, true, baseline(2026, 1, 1));
    let svc = service(
        vec![event("AAPL", date(2026, 5, 15), 100)],
        FixedPrice::at(1000),
        DripConfig::default(),
        Arc::clone(&apps),
        Arc::clone(&balances),
    );

    let first = svc.catch_up_position(&pos).await.unwrap();
    assert_eq!(first.newly_applied, 1);
    // current = 100 + (1×100)/10 ×0.9 = 100 + 9.000 = 109.000.
    assert_eq!(first.current_shares, Decimal::new(109_000, 3));

    let second = svc.catch_up_position(&pos).await.unwrap();
    assert_eq!(second.newly_applied, 0, "re-run posts nothing extra");
    assert_eq!(
        second.current_shares, first.current_shares,
        "current shares unchanged on re-entry"
    );
    assert_eq!(apps.list_for_position(pos.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn suppresses_dividends_on_or_before_baseline() {
    // A dividend whose pay-date == baseline date is suppressed (already uploaded).
    let apps = Arc::new(MemApplications::default());
    let balances = Arc::new(MemBalances::default());
    let pos = position(100, true, baseline(2026, 5, 15));
    let svc = service(
        vec![
            event("AAPL", date(2026, 5, 15), 100), // == baseline → suppressed
            event("AAPL", date(2026, 4, 1), 100),  // < baseline → suppressed
        ],
        FixedPrice::at(1000),
        DripConfig::default(),
        Arc::clone(&apps),
        Arc::clone(&balances),
    );
    let result = svc.catch_up_position(&pos).await.unwrap();
    assert_eq!(result.newly_applied, 0);
    assert_eq!(result.current_shares, Decimal::from(100));
    assert_eq!(result.provenance, ShareProvenance::Uploaded);
}

#[tokio::test]
async fn drip_on_estimated_provenance_and_share_growth() {
    let apps = Arc::new(MemApplications::default());
    let balances = Arc::new(MemBalances::default());
    let pos = position(30, true, baseline(2026, 1, 1));
    let svc = service(
        vec![event("AAPL", date(2026, 5, 15), 78)],
        FixedPrice::at(2000),
        DripConfig::default(),
        Arc::clone(&apps),
        Arc::clone(&balances),
    );
    let result = svc.catch_up_position(&pos).await.unwrap();
    // 30 + 1.053 = 31.053.
    assert_eq!(result.current_shares, Decimal::new(31_053, 3));
    assert_eq!(
        result.provenance,
        ShareProvenance::DripEstimated {
            events_applied: 1,
            baseline_as_of: baseline(2026, 1, 1),
        }
    );
    // No cash credited on the DRIP-on path.
    assert!(
        balances
            .balances_for_user(pos.user_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn drip_off_credits_account_cash_not_shares() {
    let apps = Arc::new(MemApplications::default());
    let balances = Arc::new(MemBalances::default());
    let pos = position(30, false, baseline(2026, 1, 1));
    let svc = service(
        vec![event("AAPL", date(2026, 5, 15), 78)],
        FixedPrice::at(2000),
        DripConfig::default(),
        Arc::clone(&apps),
        Arc::clone(&balances),
    );
    let result = svc.catch_up_position(&pos).await.unwrap();
    // Shares unchanged; provenance Uploaded (no share-accreting events).
    assert_eq!(result.current_shares, Decimal::from(30));
    assert_eq!(result.provenance, ShareProvenance::Uploaded);
    // Cash = 0.78 × 30 = $23.40 credited to the Brokerage account.
    let bal = balances.balances_for_user(pos.user_id).await.unwrap();
    assert_eq!(bal.len(), 1);
    assert_eq!(bal[0].account_label, "Brokerage");
    assert_eq!(bal[0].balance, Money::from_minor(2340));
}

#[tokio::test]
async fn drip_off_cash_is_not_double_credited_on_reentry() {
    let apps = Arc::new(MemApplications::default());
    let balances = Arc::new(MemBalances::default());
    let pos = position(30, false, baseline(2026, 1, 1));
    let svc = service(
        vec![event("AAPL", date(2026, 5, 15), 78)],
        FixedPrice::at(2000),
        DripConfig::default(),
        Arc::clone(&apps),
        Arc::clone(&balances),
    );
    svc.catch_up_position(&pos).await.unwrap();
    svc.catch_up_position(&pos).await.unwrap(); // re-run
    let bal = balances.balances_for_user(pos.user_id).await.unwrap();
    // Still exactly one dividend's worth of cash (the guard suppressed the re-credit).
    assert_eq!(bal[0].balance, Money::from_minor(2340));
}

#[tokio::test]
async fn event_with_no_pay_date_price_is_held() {
    // The May dividend's price is unavailable → the event is held (not applied).
    let apps = Arc::new(MemApplications::default());
    let balances = Arc::new(MemBalances::default());
    let pos = position(100, true, baseline(2026, 1, 1));
    let svc = service(
        vec![event("AAPL", date(2026, 5, 15), 100)],
        FixedPrice::at_with_hold(1000, vec![date(2026, 5, 15)]),
        DripConfig::default(),
        Arc::clone(&apps),
        Arc::clone(&balances),
    );
    let result = svc.catch_up_position(&pos).await.unwrap();
    assert_eq!(result.newly_applied, 0, "held event posts nothing");
    assert_eq!(result.current_shares, Decimal::from(100));
}

#[tokio::test]
async fn two_runs_apply_a_newly_appearing_later_dividend() {
    // Run 1 sees one dividend; run 2 sees a second (later) one — only the new one
    // posts, compounding on the first.
    let apps = Arc::new(MemApplications::default());
    let balances = Arc::new(MemBalances::default());
    let pos = position(100, true, baseline(2026, 1, 1));

    let svc1 = service(
        vec![event("AAPL", date(2026, 2, 15), 100)],
        FixedPrice::at(1000),
        DripConfig::default(),
        Arc::clone(&apps),
        Arc::clone(&balances),
    );
    let r1 = svc1.catch_up_position(&pos).await.unwrap();
    assert_eq!(r1.newly_applied, 1);
    assert_eq!(r1.current_shares, Decimal::new(109_000, 3)); // +9.000

    let svc2 = service(
        vec![
            event("AAPL", date(2026, 2, 15), 100),
            event("AAPL", date(2026, 5, 15), 100),
        ],
        FixedPrice::at(1000),
        DripConfig::default(),
        Arc::clone(&apps),
        Arc::clone(&balances),
    );
    let r2 = svc2.catch_up_position(&pos).await.unwrap();
    assert_eq!(r2.newly_applied, 1, "only the new dividend posts");
    // e2 compounds on 109: raw = (1×109)/10 = 10.9; ×0.9 = 9.81 → 109 + 9.81 = 118.81.
    assert_eq!(r2.current_shares, Decimal::new(118_810, 3));
}
