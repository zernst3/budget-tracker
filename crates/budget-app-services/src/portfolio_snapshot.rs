//! Portfolio grounding-snapshot assembly (`docs/AI_FEATURE_DESIGN.md §Phase 3`).
//!
//! The single home for turning a user's [`Position`]s + [`CashBalance`]s + the
//! [`MarketDataProvider`] port into a [`PortfolioSnapshot`] — the locked, citable
//! ground-truth surface the advisor reasons over and `reconcile` checks against.
//! Both the Phase-3 `portfolio_snapshot` server function and the Phase-5
//! `GeneratePortfolioReview` use-case assemble through this one function so the
//! snapshot shape cannot drift between "what the UI shows" and "what reconcile
//! grounds against."
//!
//! ## Concurrency (`ARCH-PARALLEL-INDEPENDENT-1`)
//!
//! Per-ticker `market.quote(..)` calls are independent, so they fan out
//! concurrently via `try_join_all`. A provider error on ANY ticker fails the
//! whole assembly (it is a transport failure, not a per-ticker degrade); a
//! provider that wants a ticker to degrade returns `Ok(None)` instead.
//!
//! ## Manual-price fallback (resolution of a design/locked-type tension)
//!
//! `§Phase 3` describes "a `None`/failed quote falls back to a manual price if
//! the position has one." The locked [`Position`] shape carries no per-position
//! manual *price* field (only `cost_basis`), so in v1 the "manual fallback" is
//! realized by the [`MarketDataProvider`] itself returning a quote tagged
//! [`PriceProvenance::Manual`] — the assembly consumes whatever provenance the
//! provider yields and degrades to `quote: None` only when the provider returns
//! `Ok(None)`. (If a dedicated per-position manual-price field is added later, it
//! slots in here as the fallback before the degrade; the snapshot shape does not
//! change.)
//!
//! ## Money (`BUDGET-MONEY-1`)
//!
//! `market_value = round_to_cents(shares * price)`; `shares` is a `Decimal`
//! COUNT (never `Money`), the price is `Money`. Every sum is exact `Money`
//! arithmetic; no `f64` appears.

use chrono::{DateTime, Utc};
use futures::future::try_join_all;

use budget_domain::ids::UserId;
use budget_domain::money::Money;
use budget_domain::portfolio::{
    CashBalance, MarketDataError, MarketDataProvider, NetWorth, PortfolioSnapshot, Position,
    PriceQuote, PricedPosition, ShareProvenance,
};

/// Resolve quotes for `positions` concurrently and assemble the grounding
/// [`PortfolioSnapshot`] (`ARCH-PARALLEL-INDEPENDENT-1`).
///
/// `now` stamps `captured_at`. A provider error on any ticker propagates (a
/// transport failure); a per-ticker degrade is the provider returning `Ok(None)`.
///
/// # Errors
/// [`MarketDataError`] if the market provider errors on any ticker.
pub async fn assemble_snapshot(
    user_id: UserId,
    positions: Vec<Position>,
    cash_balances: Vec<CashBalance>,
    market: &dyn MarketDataProvider,
    now: DateTime<Utc>,
) -> Result<PortfolioSnapshot, MarketDataError> {
    // Concurrent per-position quote fan-out. Each future resolves the ticker's
    // quote; the position is moved into the future so the result zips back 1:1.
    let priced = try_join_all(positions.into_iter().map(|position| async move {
        let quote = market.quote(&position.ticker).await?;
        Ok::<PricedPosition, MarketDataError>(price_position(position, quote))
    }))
    .await?;

    let buffer_total = sum_reserved(&cash_balances);
    let total_cash = sum_all(&cash_balances);
    let total_invested = sum_market_values(&priced);
    // v1 net worth is assets-only (`§Open Items 3` resolved): liabilities = ZERO,
    // a reserved flag, not an assumption.
    let liabilities = Money::ZERO;
    let total = total_cash + total_invested - liabilities;

    Ok(PortfolioSnapshot {
        user_id,
        positions: priced,
        cash_balances,
        buffer_total,
        net_worth: NetWorth {
            total_cash,
            total_positions: total_invested,
            liabilities,
            total,
        },
        total_invested,
        captured_at: now,
    })
}

/// Build a [`PricedPosition`] from a position and its resolved quote.
///
/// `market_value` is `Some(round_to_cents(shares * price))` exactly when a quote
/// is present, and `None` otherwise (the `None`-iff-`None` invariant the snapshot
/// type documents).
#[must_use]
pub fn price_position(position: Position, quote: Option<PriceQuote>) -> PricedPosition {
    // Phase 7 catch-up has not run here: the share count is the confirmed
    // baseline, so provenance is `Uploaded`. The DRIP wire-in (P7.4) replaces this
    // with `price_position_with_provenance` carrying the accreted shares + label.
    price_position_with_provenance(position, quote, ShareProvenance::Uploaded)
}

/// Build a [`PricedPosition`] from a position, its resolved quote, and an explicit
/// [`ShareProvenance`] label (Phase 7).
///
/// `effective_shares` for the market value comes from the position's `shares`
/// field, which the DRIP catch-up engine has already replaced with the estimated
/// current count (baseline + accretion) before assembly when DRIP is active; the
/// `provenance` carries the label so the UI / AI review present the estimate
/// honestly (`BUDGET-AI-1`).
#[must_use]
pub fn price_position_with_provenance(
    position: Position,
    quote: Option<PriceQuote>,
    provenance: ShareProvenance,
) -> PricedPosition {
    let market_value = quote
        .as_ref()
        .map(|q| Money::from_decimal(position.shares * q.price.as_decimal()).round_to_cents());
    PricedPosition {
        position,
        quote,
        market_value,
        share_provenance: provenance,
    }
}

/// Sum the reserved cash balances (the non-investable buffer).
#[must_use]
fn sum_reserved(balances: &[CashBalance]) -> Money {
    balances
        .iter()
        .filter(|b| b.reserved)
        .map(|b| b.balance)
        .sum()
}

/// Sum all cash balances (reserved and unreserved).
#[must_use]
fn sum_all(balances: &[CashBalance]) -> Money {
    balances.iter().map(|b| b.balance).sum()
}

/// Sum the resolved market values, skipping unresolved (`None`) positions.
#[must_use]
fn sum_market_values(priced: &[PricedPosition]) -> Money {
    priced.iter().filter_map(|p| p.market_value).sum()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use async_trait::async_trait;
    use budget_domain::enums::AccountType;
    use budget_domain::ids::PositionId;
    use budget_domain::portfolio::{PriceProvenance, Ticker};
    use rust_decimal::Decimal;
    use std::collections::HashMap;

    /// A tiny in-test market provider keyed by ticker string.
    struct FakeMarket {
        quotes: HashMap<String, Option<PriceQuote>>,
        error_on: Option<String>,
    }

    #[async_trait]
    impl MarketDataProvider for FakeMarket {
        async fn quote(&self, ticker: &Ticker) -> Result<Option<PriceQuote>, MarketDataError> {
            if self.error_on.as_deref() == Some(ticker.as_str()) {
                return Err(MarketDataError::Api("boom".to_owned()));
            }
            Ok(self.quotes.get(ticker.as_str()).cloned().flatten())
        }
    }

    fn position(ticker: &str, shares: i64) -> Position {
        let now = Utc::now();
        Position {
            id: PositionId::generate(),
            user_id: UserId::generate(),
            ticker: Ticker::try_new(ticker).unwrap(),
            account_label: "Brokerage".to_owned(),
            account_type: AccountType::Investment,
            shares: Decimal::new(shares, 0),
            cost_basis: None,
            drip_enabled: false,
            baseline_as_of: now,
            created_at: now,
            updated_at: now,
        }
    }

    fn quote(price_cents: i64, provenance: PriceProvenance) -> PriceQuote {
        PriceQuote {
            price: Money::from_minor(price_cents),
            provenance,
            as_of: Utc::now(),
        }
    }

    fn balance(label: &str, cents: i64, reserved: bool) -> CashBalance {
        CashBalance {
            account_label: label.to_owned(),
            balance: Money::from_minor(cents),
            reserved,
        }
    }

    #[tokio::test]
    async fn assembles_totals_skipping_unresolved_positions() {
        // AAPL $180 x 10 = $1800 (resolved); NVDA unresolved (None) -> skipped.
        let mut quotes = HashMap::new();
        quotes.insert(
            "AAPL".to_owned(),
            Some(quote(
                18_000,
                PriceProvenance::Market { source: "m".into() },
            )),
        );
        quotes.insert("NVDA".to_owned(), None);
        let market = FakeMarket {
            quotes,
            error_on: None,
        };

        let positions = vec![position("AAPL", 10), position("NVDA", 5)];
        let balances = vec![
            balance("Emergency", 500_000, true), // $5000 reserved
            balance("Checking", 100_000, false), // $1000
        ];

        let snap = assemble_snapshot(UserId::generate(), positions, balances, &market, Utc::now())
            .await
            .unwrap();

        // total_invested skips the unresolved NVDA.
        assert_eq!(snap.total_invested, Money::from_minor(180_000));
        // buffer = only the reserved $5000.
        assert_eq!(snap.buffer_total, Money::from_minor(500_000));
        // total_cash = $5000 + $1000.
        assert_eq!(snap.net_worth.total_cash, Money::from_minor(600_000));
        assert_eq!(snap.net_worth.total_positions, Money::from_minor(180_000));
        assert_eq!(snap.net_worth.liabilities, Money::ZERO);
        // net worth = cash + positions - 0 = $6000 + $1800 = $7800.
        assert_eq!(snap.net_worth.total, Money::from_minor(780_000));

        // market_value is None exactly for the unresolved position.
        let nvda = snap
            .positions
            .iter()
            .find(|p| p.position.ticker.as_str() == "NVDA")
            .unwrap();
        assert_eq!(nvda.market_value, None);
        assert_eq!(nvda.quote, None);
        let aapl = snap
            .positions
            .iter()
            .find(|p| p.position.ticker.as_str() == "AAPL")
            .unwrap();
        assert_eq!(aapl.market_value, Some(Money::from_minor(180_000)));
    }

    #[tokio::test]
    async fn manual_provenance_quote_is_consumed_as_the_fallback() {
        // A Manual-provenance quote is treated like any resolved quote.
        let mut quotes = HashMap::new();
        quotes.insert(
            "MSFT".to_owned(),
            Some(quote(40_000, PriceProvenance::Manual)),
        );
        let market = FakeMarket {
            quotes,
            error_on: None,
        };
        let snap = assemble_snapshot(
            UserId::generate(),
            vec![position("MSFT", 2)],
            vec![],
            &market,
            Utc::now(),
        )
        .await
        .unwrap();
        // 2 x $400 = $800.
        assert_eq!(snap.total_invested, Money::from_minor(80_000));
        let msft = &snap.positions[0];
        assert_eq!(
            msft.quote.as_ref().map(|q| &q.provenance),
            Some(&PriceProvenance::Manual)
        );
    }

    #[tokio::test]
    async fn provider_error_propagates() {
        let market = FakeMarket {
            quotes: HashMap::new(),
            error_on: Some("AAPL".to_owned()),
        };
        let out = assemble_snapshot(
            UserId::generate(),
            vec![position("AAPL", 1)],
            vec![],
            &market,
            Utc::now(),
        )
        .await;
        assert_eq!(out, Err(MarketDataError::Api("boom".to_owned())));
    }

    #[tokio::test]
    async fn empty_positions_assemble_cash_only() {
        let market = FakeMarket {
            quotes: HashMap::new(),
            error_on: None,
        };
        let snap = assemble_snapshot(
            UserId::generate(),
            vec![],
            vec![balance("Checking", 250_000, false)],
            &market,
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(snap.total_invested, Money::ZERO);
        assert_eq!(snap.net_worth.total, Money::from_minor(250_000));
        assert!(snap.positions.is_empty());
    }
}
