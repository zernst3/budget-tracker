//! Dividend-data adapters for DRIP & real-time tracking
//! (`docs/DRIP_REALTIME_DESIGN.md §5/§7`).
//!
//! The [`DividendSource`](budget_domain::portfolio::DividendSource) port resolves
//! a ticker's [`DividendEvent`](budget_domain::portfolio::DividendEvent)s after a
//! cutoff date. The catch-up engine fans these out concurrently per distinct
//! ticker (`ARCH-PARALLEL-INDEPENDENT-1`) and caches results in `dividend_events`.
//!
//! ## Tiers (the chain, §6/§7)
//!
//! - **`mock.rs`:** [`MockDividendSource`] — fixture-configured, no network. What
//!   every mock-only catch-up test grounds against.
//! - **`manual.rs`:** [`ManualDividendSource`] — a configurable
//!   ticker→`Vec<DividendEvent>` map (the ultimate fallback; the user confirms a
//!   `$/share`). Mirrors `ManualPriceSource` on the market-data side.
//! - **`tiingo.rs`:** [`TiingoDividendSource`] — the primary free chain tier (key
//!   from the vault). The dividend JSON is parsed by a pure function unit-tested
//!   against captured payloads.
//! - **`yahoo.rs`:** [`YahooDividendSource`] — the keyless v8 `events=div`
//!   fallback. Pure parser, captured-payload tested.
//! - **`chain.rs`:** [`ChainDividendSource`] — Tiingo → Yahoo → manual, the same
//!   resilience shape as `ChainMarketDataProvider`.
//!
//! ## Verification boundary (`ORCH-TRAINING-CUTOFF-1`)
//!
//! The Tiingo/Yahoo wire shapes + tiers are best-effort behind the port; the live
//! smoke test (real key + network) is the operator's. Every parser here is unit
//! tested against a CAPTURED/SAMPLE payload, never a live call.

pub mod chain;
pub mod manual;
pub mod mock;
pub mod tiingo;
pub mod yahoo;

pub use chain::ChainDividendSource;
pub use manual::ManualDividendSource;
pub use mock::MockDividendSource;
pub use tiingo::{TIINGO_API_KEY_SECRET, TiingoDividendSource};
pub use yahoo::YahooDividendSource;
