//! Market-data adapters for AI Portfolio Insights
//! (`docs/AI_FEATURE_DESIGN.md §Phase 3`).
//!
//! The [`MarketDataProvider`](budget_domain::portfolio::MarketDataProvider) port
//! resolves a per-ticker [`PriceQuote`](budget_domain::portfolio::PriceQuote).
//! The use-case fans these out concurrently (`ARCH-PARALLEL-INDEPENDENT-1`) and
//! falls back to a position's manual price when a quote is absent.
//!
//! ## Phase status
//!
//! - **`mock.rs` (Phase 3, here):** [`MockMarketDataProvider`] — a
//!   fixture-configured provider returning canned quotes / `None` / errors per
//!   ticker. Fully usable today under (Phase 6) `AI_MODE=mock`; it is what every
//!   mock-only test below the UI grounds against.
//! - **Real HTTP adapter (Open Item, deferred to a confirmed provider):** the
//!   market-data provider choice (Finnhub / Twelve Data / Alpha Vantage, plus the
//!   multi-source enrichment Zach signed off) is an Open Item; the real-path
//!   wiring returns `Err` until confirmed (`§Open Items 2`).
//! - **Real fallback chain (Phase 6):** [`ChainMarketDataProvider`] composes
//!   Finnhub (real-time, key from vault — [`FinnhubMarketData`]) → Stooq (keyless
//!   CSV — [`StooqMarketData`]) → a manual price tier ([`ManualPriceSource`]) →
//!   degrade to `None`. The chain swallows per-tier errors (it is the resilience
//!   layer), so the feature runs end-to-end with NO API key (Stooq + manual);
//!   the Finnhub key only upgrades to real-time quotes (Zach's resolved
//!   decision #2).

pub mod chain;
pub mod finnhub;
pub mod mock;
pub mod stooq;

pub use chain::{ChainMarketDataProvider, ManualPriceSource};
pub use finnhub::{FINNHUB_API_KEY_SECRET, FinnhubMarketData};
pub use mock::{MockMarketDataProvider, MockQuote};
pub use stooq::StooqMarketData;
