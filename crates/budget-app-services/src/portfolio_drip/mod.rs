//! `DripCatchUpService` — the idempotent, lazy DRIP accretion engine
//! (`docs/DRIP_REALTIME_DESIGN.md §3/§6`).
//!
//! Mirrors `BUDGET-IDEMPOTENT-MONTH-INIT-1`: on snapshot assembly / app open, for
//! each position it applies every unprocessed `(position, dividend pay-date)`
//! exactly once, in chronological order, under the `(position_id, pay_date)`
//! unique guard with `ON CONFLICT DO NOTHING`. Re-entrant: two opens, or a
//! same-day re-open, post nothing extra.
//!
//! ## The estimation math (`§3`, exact decimals — `BUDGET-MONEY-1`)
//!
//! For a DRIP-enabled position, events with `pay_date > baseline_as_of` compound
//! in chronological order:
//!
//! ```text
//! shares_held_at(e) = baseline_shares + Σ shares_added(eᵢ) for eᵢ.pay_date < e.pay_date
//! raw_new(e)        = (amount_per_share × shares_held_at(e)) / price_used(e)
//! shares_added(e)   = floor( raw_new(e) × (1 - DRIP_BUFFER), DRIP_SHARE_DP )   // conservative
//! current_shares    = baseline_shares + Σ shares_added(e)
//! ```
//!
//! The conservative buffer is scoped to the ACCRETED shares only — the baseline is
//! never haircut (§2.2). DRIP **off**: `cash_added(e) = amount_per_share ×
//! shares_held_at(e)` (exact, no buffer — real cash, not an estimate) increases the
//! position's account `CashBalance` (`BUDGET-CASH-1`); no shares accrue.
//!
//! ## Auditable chain, never a mutated scalar (`BUDGET-ROLLOVER-INTEGRITY-1`)
//!
//! Current shares are `baseline + Σ drip_applications` — always recomputable from
//! the persisted chain, never a stored running number. A dividend whose
//! `price_used` is unavailable is HELD (not applied) rather than applied against a
//! bad price (§3 last paragraph).
//!
//! This module is built + tested entirely against mocks (`§Phase 7.3`); the live
//! Tiingo/Yahoo smoke test is the operator's.

pub mod config;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use futures::future::try_join_all;
use rust_decimal::{Decimal, RoundingStrategy};

use budget_domain::ids::DripApplicationId;
use budget_domain::money::Money;
use budget_domain::portfolio::{
    CashBalance, DividendEvent, DividendSource, DripApplication, Position, ShareProvenance, Ticker,
};
use budget_domain::repositories::{
    CashBalanceRepository, DividendEventCache, DripApplicationRepository,
};

use crate::error::ServiceError;
use config::DripConfig;

/// The catch-up outcome for a single position: the estimated current share count,
/// its provenance label, and how many NEW applications were posted this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DripCatchUpResult {
    /// The estimated current share count = baseline + Σ applied accretion (§3).
    pub current_shares: Decimal,
    /// `Uploaded` if no accretion applies, else `DripEstimated { events_applied }`.
    pub provenance: ShareProvenance,
    /// How many applications were newly inserted this run (idempotency telemetry).
    pub newly_applied: u32,
}

/// One computed §3 application before persistence — the pure-math output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedApplication {
    /// The dividend pay-date (the idempotency key half).
    pub pay_date: NaiveDate,
    /// The dividend amount per share.
    pub amount_per_share: Money,
    /// The per-share price used on the pay-date.
    pub price_used: Money,
    /// Shares accreted (DRIP on); `Decimal::ZERO` when DRIP off.
    pub shares_added: Decimal,
    /// Cash accreted (DRIP off); `Money::ZERO` when DRIP on.
    pub cash_added: Money,
}

/// Compute the §3 DRIP accretion for one position's dividend events, in
/// chronological order, compounding (pure — no I/O, exact decimals).
///
/// `events` MUST already be filtered to `pay_date > baseline_as_of` and carry a
/// resolved `price_used` (events with no price are dropped by the caller before
/// this call — a missing price holds the event, §3). `drip_enabled` selects the
/// shares-vs-cash branch. Returns one [`ComputedApplication`] per event, in pay
/// order.
#[must_use]
pub fn compute_accretion(
    baseline_shares: Decimal,
    drip_enabled: bool,
    events: &[(DividendEvent, Money)],
    config: DripConfig,
) -> Vec<ComputedApplication> {
    // Sort chronologically so compounding sees earlier accretion first.
    let mut ordered: Vec<&(DividendEvent, Money)> = events.iter().collect();
    ordered.sort_by_key(|(e, _)| e.pay_date);

    let buffer_factor = Decimal::ONE - config.buffer;
    let mut accreted = Decimal::ZERO; // Σ shares_added so far (compounds).
    let mut out = Vec::with_capacity(ordered.len());

    for (event, price_used) in ordered {
        let shares_held_at = baseline_shares + accreted;
        let dividend_cash_decimal = event.amount_per_share.as_decimal() * shares_held_at;

        if drip_enabled {
            // raw_new = (amount_per_share × shares_held_at) / price_used.
            let price = price_used.as_decimal();
            // price_used is guaranteed non-zero by the caller (a zero/None price
            // holds the event); guard defensively to avoid a div-by-zero panic.
            let shares_added = if price.is_zero() {
                Decimal::ZERO
            } else {
                let raw_new = dividend_cash_decimal / price;
                // floor(raw_new × buffer_factor, DRIP_SHARE_DP). Accreted shares
                // are positive, so ToZero == floor (conservative, §2.2/§2.3).
                (raw_new * buffer_factor)
                    .round_dp_with_strategy(config.share_dp, RoundingStrategy::ToZero)
            };
            accreted += shares_added;
            out.push(ComputedApplication {
                pay_date: event.pay_date,
                amount_per_share: event.amount_per_share,
                price_used: *price_used,
                shares_added,
                cash_added: Money::ZERO,
            });
        } else {
            // DRIP off: the dividend is real cash (no buffer, no rounding to share
            // dp — it is exact Money to the account CashBalance, §3/BUDGET-CASH-1).
            out.push(ComputedApplication {
                pay_date: event.pay_date,
                amount_per_share: event.amount_per_share,
                price_used: *price_used,
                shares_added: Decimal::ZERO,
                cash_added: Money::from_decimal(dividend_cash_decimal).round_to_cents(),
            });
        }
    }
    out
}

/// Resolves the per-share price on a dividend pay-date (`§3`: `e.price_used`).
///
/// A dedicated port because the existing [`MarketDataProvider`] resolves only the
/// CURRENT quote, not a historical one. `Ok(None)` => no price for that date (the
/// event is HELD, not applied against a bad price). Mock-backed in P7.3; the P7.4
/// wiring supplies a concrete resolver (which may approximate with the current
/// quote, flagged).
///
/// [`MarketDataProvider`]: budget_domain::portfolio::MarketDataProvider
#[async_trait::async_trait]
pub trait PayDatePriceSource: Send + Sync {
    /// The per-share price for `ticker` on `pay_date`; `None` => unavailable.
    ///
    /// # Errors
    /// [`ServiceError`] only on an unexpected transport failure; a simple "no
    /// price for this date" is `Ok(None)`.
    async fn price_on(
        &self,
        ticker: &Ticker,
        pay_date: NaiveDate,
    ) -> Result<Option<Money>, ServiceError>;
}

/// The lazy, idempotent DRIP catch-up engine (`§6`). Holds only ports
/// (`ARCH-STRICT-LAYERING-1`).
pub struct DripCatchUpService {
    dividends: Arc<dyn DividendSource>,
    cache: Arc<dyn DividendEventCache>,
    prices: Arc<dyn PayDatePriceSource>,
    applications: Arc<dyn DripApplicationRepository>,
    balances: Arc<dyn CashBalanceRepository>,
    config: DripConfig,
}

impl DripCatchUpService {
    /// Assemble the engine from its ports + config.
    #[must_use]
    pub fn new(
        dividends: Arc<dyn DividendSource>,
        cache: Arc<dyn DividendEventCache>,
        prices: Arc<dyn PayDatePriceSource>,
        applications: Arc<dyn DripApplicationRepository>,
        balances: Arc<dyn CashBalanceRepository>,
        config: DripConfig,
    ) -> Self {
        Self {
            dividends,
            cache,
            prices,
            applications,
            balances,
            config,
        }
    }

    /// Run catch-up for a single position, returning its estimated current share
    /// count + provenance (`§6`). Idempotent and re-entrant.
    ///
    /// Steps: fetch (cache-through) dividends since `baseline_as_of`; resolve each
    /// pay-date price (drop events with no price — held); compute §3 accretion in
    /// chronological order; `apply_if_absent` each under the unique guard; for a
    /// newly-applied DRIP-off event, add its cash to the account `CashBalance`.
    ///
    /// # Errors
    /// [`ServiceError`] on any port failure.
    pub async fn catch_up_position(
        &self,
        position: &Position,
    ) -> Result<DripCatchUpResult, ServiceError> {
        let since = position.baseline_as_of.date_naive();

        // 1. Cache-through dividend fetch: read cache, fetch the source, refresh
        // the cache, then read the union from cache (so the source's results are
        // available even on the first call this run).
        let fetched = self
            .dividends
            .dividends_since(&position.ticker, since)
            .await
            .map_err(|e| ServiceError::AdvisorTransport(e.to_string()))?;
        if !fetched.is_empty() {
            self.cache.upsert_many(&fetched).await?;
        }
        let events = self.cache.find_since(&position.ticker, since).await?;

        // 2. Resolve a pay-date price per event; hold events with no price (§3).
        let priced = self.resolve_prices(&events).await?;

        // 3. §3 accretion, chronological + compounding (pure).
        let computed =
            compute_accretion(position.shares, position.drip_enabled, &priced, self.config);

        // 4. Persist each application under the idempotency guard; cash-side
        // effects only for a NEWLY applied DRIP-off event.
        let now = Utc::now();
        let mut newly_applied = 0_u32;
        let mut accreted = Decimal::ZERO;
        for app in &computed {
            accreted += app.shares_added;
            let record = DripApplication {
                id: DripApplicationId::generate(),
                user_id: position.user_id,
                position_id: position.id,
                ticker: position.ticker.clone(),
                pay_date: app.pay_date,
                amount_per_share: app.amount_per_share,
                price_used: app.price_used,
                shares_added: app.shares_added,
                cash_added: app.cash_added,
                drip_on_at_apply: position.drip_enabled,
                applied_at: now,
            };
            let inserted = self.applications.apply_if_absent(&record, None).await?;
            if inserted {
                newly_applied += 1;
                if !position.drip_enabled && !app.cash_added.is_zero() {
                    self.credit_cash(position, app.cash_added).await?;
                }
            }
        }

        // 5. Recompute current shares from the AUTHORITATIVE chain (never the
        // in-loop accumulator) — baseline + Σ shares_added over persisted rows
        // (BUDGET-ROLLOVER-INTEGRITY-1). This also reflects rows applied by a
        // prior run.
        let current_shares = self.current_shares(position).await?;
        let events_applied = self.count_accreting_since(position).await?;
        let provenance = if events_applied == 0 {
            ShareProvenance::Uploaded
        } else {
            ShareProvenance::DripEstimated {
                events_applied,
                baseline_as_of: position.baseline_as_of,
            }
        };
        // `accreted` is retained only as a local cross-check of the loop; the
        // returned figure is the chain recompute.
        debug_assert!(
            position.drip_enabled || accreted.is_zero(),
            "DRIP-off positions accrete no shares"
        );

        Ok(DripCatchUpResult {
            current_shares,
            provenance,
            newly_applied,
        })
    }

    /// Resolve a pay-date price per event, dropping (holding) events with no price.
    async fn resolve_prices(
        &self,
        events: &[DividendEvent],
    ) -> Result<Vec<(DividendEvent, Money)>, ServiceError> {
        // Independent per-event price lookups fan out concurrently
        // (ARCH-PARALLEL-INDEPENDENT-1).
        let resolved = try_join_all(events.iter().cloned().map(|event| async move {
            let price = self.prices.price_on(&event.ticker, event.pay_date).await?;
            Ok::<_, ServiceError>(price.map(|p| (event, p)))
        }))
        .await?;
        Ok(resolved.into_iter().flatten().collect())
    }

    /// Add `amount` to the position's account `CashBalance` (DRIP-off path,
    /// `BUDGET-CASH-1`). Reads the current balance for that account label and
    /// upserts the sum; an absent balance starts from zero.
    async fn credit_cash(&self, position: &Position, amount: Money) -> Result<(), ServiceError> {
        let existing = self
            .balances
            .balances_for_user(position.user_id)
            .await?
            .into_iter()
            .find(|b| b.account_label == position.account_label);
        let (current, reserved) =
            existing.map_or((Money::ZERO, false), |b| (b.balance, b.reserved));
        let updated = CashBalance {
            account_label: position.account_label.clone(),
            balance: current + amount,
            reserved,
        };
        self.balances.upsert(&updated).await?;
        Ok(())
    }

    /// Current shares = baseline + Σ shares_added over the persisted chain for
    /// applications with `pay_date > baseline_as_of` (`BUDGET-ROLLOVER-INTEGRITY-1`).
    async fn current_shares(&self, position: &Position) -> Result<Decimal, ServiceError> {
        let since = position.baseline_as_of.date_naive();
        let accreted: Decimal = self
            .applications
            .list_for_position(position.id)
            .await?
            .into_iter()
            .filter(|a| a.pay_date > since)
            .map(|a| a.shares_added)
            .sum();
        Ok(position.shares + accreted)
    }

    /// Count the applications contributing to the current estimate (those with
    /// `pay_date > baseline_as_of`), for the `DripEstimated { events_applied }`
    /// label. Counts both share- and cash-side rows that post-date the baseline so
    /// the label reflects "events since last upload"; the share count itself only
    /// moves for DRIP-on rows.
    async fn count_accreting_since(&self, position: &Position) -> Result<u32, ServiceError> {
        let since = position.baseline_as_of.date_naive();
        let count = self
            .applications
            .list_for_position(position.id)
            .await?
            .into_iter()
            .filter(|a| a.pay_date > since && a.drip_on_at_apply)
            .count();
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }
}

/// Helper for callers that need the provenance for a position whose accretion has
/// already been applied (e.g. snapshot assembly building a `PricedPosition`).
///
/// `events_applied == 0` → `Uploaded`; else `DripEstimated`.
#[must_use]
pub fn provenance_for(events_applied: u32, baseline_as_of: DateTime<Utc>) -> ShareProvenance {
    if events_applied == 0 {
        ShareProvenance::Uploaded
    } else {
        ShareProvenance::DripEstimated {
            events_applied,
            baseline_as_of,
        }
    }
}

/// A map of `ticker -> Vec<DividendEvent>` used by tests / wiring to pre-seed a
/// mock dividend source. Public so the P7.4 wiring can reuse it.
pub type DividendFixture = HashMap<String, Vec<DividendEvent>>;
