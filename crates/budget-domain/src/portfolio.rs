//! AI Portfolio Insights — the bounded-context domain module (`RUST-DOMAIN-1`).
//!
//! Single module for the whole portfolio-review context: the value types the
//! grounding snapshot is built from, the model-output types (`Recommendation`,
//! `Claim`, `ClaimSubject`), the reconciliation outcome types, the persisted
//! audit aggregate (`ReviewRun`), the four ports the use-case orchestrates
//! against, and the two port error enums.
//!
//! ## WASM-clean dependency isolation (`docs/AI_FEATURE_DESIGN.md §0.3`)
//!
//! This module is part of the hexagonal core (`DOMAIN-1`): it compiles to WASM
//! and MUST NOT depend on `reqwest`, on `serde_json::Value`, on `SeaORM`, or on
//! any Gemini wire type. Only `serde` derives are permitted (already a domain
//! dep). The Gemini wire structs live in `budget-infrastructure/src/advisor/wire.rs`;
//! the domain [`AdvisorError`]/[`MarketDataError`] carry only `String` payloads —
//! never an HTTP status, a Gemini error object, or an API key.
//!
//! ## The one-way-door surface (`ORCH-ONE-WAY-DOOR-1`)
//!
//! [`InvestmentAdvisor::recommend`] takes a `&PortfolioSnapshot` (the locked,
//! citable ground-truth surface) and returns an [`AdvisorOutput`]. Adding a new
//! citable figure later is a coordinated change: a [`ClaimSubject`] variant + the
//! exhaustive `reconcile` arm + the wire schema + fixtures + tests. The closed
//! [`ClaimSubject`] enum (no `_` arm anywhere downstream) is the mechanical
//! enforcement of that coupling (`BUDGET-AI-1`).

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::enums::AccountType;
use crate::error::{RepositoryError, ValidationError};
use crate::ids::{DripApplicationId, PositionId, ReviewRunId, UserId};
use crate::money::Money;

// ===========================================================================
// Ticker (validated newtype, DOMAIN-3)
// ===========================================================================

/// A validated stock ticker symbol (uppercase, 1–10 chars of `[A-Z.]`).
///
/// Anywhere a `Ticker`-typed value appears it has already passed validation —
/// the type is the proof. [`Ticker::try_new`] trims and uppercases before
/// validating, so `"aapl"` constructs `Ok("AAPL")`. Accepts dotted classes like
/// `"BRK.A"`; rejects empty, over-length, digits, and embedded spaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ticker(String);

impl Ticker {
    const MAX_LEN: usize = 10;

    /// Construct a validated [`Ticker`] (uppercase, 1–10 of `[A-Z.]`).
    ///
    /// Trims surrounding whitespace and uppercases before validating, so
    /// `"  aapl "` → `Ok("AAPL")`.
    ///
    /// # Errors
    /// - [`ValidationError::Empty`] if `raw` is blank after trimming.
    /// - [`ValidationError::TooLong`] if the symbol exceeds 10 characters.
    /// - [`ValidationError::Format`] if any character is not `A-Z` or `.`.
    pub fn try_new(raw: &str) -> Result<Self, ValidationError> {
        let normalized = raw.trim().to_uppercase();
        if normalized.is_empty() {
            return Err(ValidationError::Empty { field: "ticker" });
        }
        if normalized.chars().count() > Self::MAX_LEN {
            return Err(ValidationError::TooLong {
                field: "ticker",
                max: Self::MAX_LEN,
                actual: normalized.chars().count(),
            });
        }
        if !normalized
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '.')
        {
            return Err(ValidationError::Format {
                field: "ticker",
                reason: "expected 1-10 uppercase letters or '.' (no digits or spaces)".to_string(),
            });
        }
        Ok(Ticker(normalized))
    }

    /// The underlying symbol string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the owned [`String`] (for the persistence boundary).
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for Ticker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ===========================================================================
// Price provenance + quote
// ===========================================================================

/// Where a [`PriceQuote`] came from — a real market feed, or a user-entered
/// manual price (the coverage fallback).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PriceProvenance {
    /// A live market quote from a named source (e.g. `"finnhub"`).
    Market {
        /// The provider that produced the quote.
        source: String,
    },
    /// A user-entered manual price (no live feed).
    Manual,
}

/// A point-in-time price for one ticker, carrying its provenance and the instant
/// it was observed (`ARCH-UTC-TIMESTAMPS-1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceQuote {
    /// The per-share price.
    pub price: Money,
    /// Where the price came from (live feed vs. manual).
    pub provenance: PriceProvenance,
    /// When the price was observed, UTC-anchored (`ARCH-UTC-TIMESTAMPS-1`).
    pub as_of: DateTime<Utc>,
}

// ===========================================================================
// Dividend events + share provenance (Phase 7 — DRIP & real-time tracking)
// ===========================================================================

/// Where a [`DividendEvent`] came from — one of the chain tiers (Tiingo → Yahoo →
/// manual), mirroring [`PriceProvenance`] for the dividend side.
///
/// Carried on the event so the cache row records provenance and the UI/audit can
/// distinguish a fetched dividend from a user-confirmed manual one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DividendSourceKind {
    /// A dividend fetched from Tiingo (the primary free EOD chain tier).
    Tiingo,
    /// A dividend fetched from Yahoo's keyless v8 `events=div` endpoint.
    Yahoo,
    /// A user-entered manual dividend (the ultimate fallback tier).
    Manual,
    /// A mock/test-fixture dividend (no real feed).
    Mock,
}

impl DividendSourceKind {
    /// The lowercase wire/storage label for this source (`"tiingo"`, `"yahoo"`,
    /// `"manual"`, `"mock"`) — the value persisted in `dividend_events.source`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            DividendSourceKind::Tiingo => "tiingo",
            DividendSourceKind::Yahoo => "yahoo",
            DividendSourceKind::Manual => "manual",
            DividendSourceKind::Mock => "mock",
        }
    }

    /// Parse a stored/wire source label back into the kind.
    ///
    /// # Errors
    /// [`ValidationError::Format`] if `raw` is not one of the four known labels.
    pub fn try_from_str(raw: &str) -> Result<Self, ValidationError> {
        match raw {
            "tiingo" => Ok(DividendSourceKind::Tiingo),
            "yahoo" => Ok(DividendSourceKind::Yahoo),
            "manual" => Ok(DividendSourceKind::Manual),
            "mock" => Ok(DividendSourceKind::Mock),
            other => Err(ValidationError::Format {
                field: "dividend_source",
                reason: format!("unknown dividend source '{other}'"),
            }),
        }
    }
}

/// One dividend payment for a ticker: the ex-date, the pay-date, and the cash
/// amount per share, plus where it was sourced from.
///
/// `amount_per_share` is exact [`Money`] (`BUDGET-MONEY-1`). The DRIP catch-up
/// engine applies events with `pay_date > baseline_as_of` in chronological order
/// (§3). Identity in the `dividend_events` cache is `(ticker, pay_date)` so a
/// ticker's dividends are fetched once and shared across positions holding it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DividendEvent {
    /// The ticker this dividend was paid on.
    pub ticker: Ticker,
    /// The ex-dividend date (the trade-date cutoff for eligibility).
    pub ex_date: NaiveDate,
    /// The pay-date — when the dividend actually pays (the DRIP-apply key).
    pub pay_date: NaiveDate,
    /// The cash amount per share (exact `Money`, `BUDGET-MONEY-1`).
    pub amount_per_share: Money,
    /// Which chain tier produced this event.
    pub source: DividendSourceKind,
}

/// The provenance of a [`PricedPosition`]'s share count — confirmed (`Uploaded`)
/// vs. a DRIP estimate accreted since the last upload (`DripEstimated`).
///
/// Surfaced on the snapshot / DTO / UI and reflected in the AI review so nothing
/// estimated is presented as confirmed truth (`BUDGET-AI-1`, §2.5/§8). An upload
/// re-baselines the position and restores `Uploaded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareProvenance {
    /// The share count is the confirmed upload baseline (no DRIP accretion since
    /// `baseline_as_of`).
    Uploaded,
    /// The share count includes `events_applied` DRIP estimates accreted since the
    /// confirmed `baseline_as_of` (a LABELED estimate).
    DripEstimated {
        /// How many dividend events have been DRIP-applied since the baseline.
        events_applied: u32,
        /// The confirmed-baseline as-of instant the estimate accreted from.
        baseline_as_of: DateTime<Utc>,
    },
}

/// One row in a position's auditable DRIP accretion chain (`drip_applications`,
/// Phase 7 `m0008`, `BUDGET-ROLLOVER-INTEGRITY-1`).
///
/// Append-only system-log semantics (`SQL-AUDIT-COLUMNS-1`): never mutated. The
/// current share count of a DRIP position is `baseline_shares + Σ shares_added`
/// over the applications with `pay_date > baseline_as_of` — recomputable, never a
/// stored mutable scalar. Idempotency is enforced by the DB unique
/// `(position_id, pay_date)` with `ON CONFLICT DO NOTHING` (§6).
///
/// Exactly one of `shares_added` / `cash_added` is meaningful per row:
/// `drip_on_at_apply == true` → `shares_added` is the accreted shares and
/// `cash_added` is zero; `false` → `cash_added` is the dividend cash (to the
/// account `CashBalance`, `BUDGET-CASH-1`) and `shares_added` is zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DripApplication {
    /// Stable identity for this audit row.
    pub id: DripApplicationId,
    /// Owning user (`SPEC §9.1` defense in depth).
    pub user_id: UserId,
    /// The position this application accreted (FK → positions, Cascade).
    pub position_id: PositionId,
    /// The ticker (denormalized for audit readability).
    pub ticker: Ticker,
    /// The dividend pay-date — the idempotency key half (`(position_id, pay_date)`).
    pub pay_date: NaiveDate,
    /// The dividend amount per share applied (exact `Money`, `BUDGET-MONEY-1`).
    pub amount_per_share: Money,
    /// The per-share price used on the pay-date to value the reinvestment (§3).
    pub price_used: Money,
    /// Shares added by this application — a COUNT (`Decimal`, `BUDGET-MONEY-1`);
    /// `0` when DRIP was off at apply time (then `cash_added` carries the value).
    pub shares_added: Decimal,
    /// Cash added to the account `CashBalance` when DRIP was off (`BUDGET-CASH-1`);
    /// `Money::ZERO` when DRIP was on (then `shares_added` carries the value).
    pub cash_added: Money,
    /// Whether DRIP was enabled on the position at the instant this row was applied.
    pub drip_on_at_apply: bool,
    /// When this application was computed/written, UTC-anchored
    /// (`ARCH-UTC-TIMESTAMPS-1`). The single audit timestamp (append-only).
    pub applied_at: DateTime<Utc>,
}

// ===========================================================================
// Position
// ===========================================================================

/// A holding: a count of `shares` of one `ticker` in a labelled account.
///
/// `shares` is a COUNT (a [`Decimal`]), never [`Money`] (`BUDGET-MONEY-1`). The
/// `created_at`/`updated_at` audit timestamps are carried so the mapper is total
/// against the entity; they are NOT used by `reconcile` (reconciliation keys off
/// `ticker` + market value only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Stable identity for this holding.
    pub id: PositionId,
    /// Owning user (`SPEC §9.1` defense in depth).
    pub user_id: UserId,
    /// The validated stock symbol.
    pub ticker: Ticker,
    /// A human label for the holding's account ("Fidelity Roth", "Brokerage").
    pub account_label: String,
    /// The account's tax/category type (reuses [`crate::enums::AccountType`]).
    pub account_type: AccountType,
    /// Number of shares held — the CONFIRMED baseline COUNT, not money
    /// (`BUDGET-MONEY-1`). Immutable between uploads; current shares are derived
    /// (`baseline + Σ drip_applications`, `BUDGET-ROLLOVER-INTEGRITY-1`).
    pub shares: Decimal,
    /// Optional cost basis (what the holding was acquired for).
    pub cost_basis: Option<Money>,
    /// The per-position, per-account DRIP toggle (Phase 7, `m0008`). When `true` a
    /// dividend that pays after `baseline_as_of` reinvests into accreted shares
    /// (a labeled estimate); when `false` the dividend becomes account cash
    /// (`BUDGET-CASH-1`). PERSISTS across uploads for surviving positions
    /// (§2.7/§6). Default `false` (DRIP is opt-in).
    pub drip_enabled: bool,
    /// The as-of date of the current confirmed `shares` baseline, set on upload
    /// (Phase 7, `m0008`, `BUDGET-CUTOVER-1`). DRIP accretion applies only to
    /// dividend events with `pay_date > baseline_as_of`.
    pub baseline_as_of: DateTime<Utc>,
    /// Row-create instant, UTC-anchored (`ARCH-UTC-TIMESTAMPS-1`).
    pub created_at: DateTime<Utc>,
    /// Row-update instant, UTC-anchored (`ARCH-UTC-TIMESTAMPS-1`).
    pub updated_at: DateTime<Utc>,
}

/// One incoming holding in a per-account upload upsert (`docs/DRIP_REALTIME_DESIGN.md
/// §2.7/§6`).
///
/// An upload is scoped to ONE account and carries the confirmed share count (and
/// optional cost basis) per ticker IN THAT ACCOUNT. The repository reconciles
/// these against the existing positions WHERE `account_label = the uploaded
/// account` (identity `(user_id, ticker, account_label)`): surviving positions
/// are re-baselined (preserving `drip_enabled`), absent ones removed, new ones
/// inserted with `drip_enabled = false`. Positions in OTHER accounts are never
/// touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedPosition {
    /// The validated stock symbol.
    pub ticker: Ticker,
    /// The confirmed share count (the new baseline) — a COUNT, not money
    /// (`BUDGET-MONEY-1`).
    pub shares: Decimal,
    /// Optional cost basis for the holding.
    pub cost_basis: Option<Money>,
}

// ===========================================================================
// CashBalance
// ===========================================================================

/// A cash balance in a labelled account.
///
/// `balance` is a BALANCE (a stock), never a flow (`BUDGET-CASH-1`). `reserved`
/// marks a non-investable reserve (an emergency buffer); reserved balances sum
/// into the snapshot's `buffer_total`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashBalance {
    /// A human label for the cash account.
    pub account_label: String,
    /// The cash balance — a stock, never a flow (`BUDGET-CASH-1`).
    pub balance: Money,
    /// `true` => a buffer / non-investable reserve (sums into `buffer_total`).
    pub reserved: bool,
}

// ===========================================================================
// NetWorth
// ===========================================================================

/// Aggregate net worth at snapshot time.
///
/// v1 is assets-only: `liabilities` is reserved at [`Money::ZERO`] (a flag, not
/// an assumption — adding a real subtraction later is not a snapshot-shape
/// change). `total == total_cash + total_positions - liabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetWorth {
    /// Sum of all cash balances (reserved and unreserved).
    pub total_cash: Money,
    /// Sum of all resolved position market values.
    pub total_positions: Money,
    /// Liabilities — v1: always [`Money::ZERO`] (reserved flag).
    pub liabilities: Money,
    /// `total_cash + total_positions - liabilities`.
    pub total: Money,
}

// ===========================================================================
// PricedPosition + PortfolioSnapshot
// ===========================================================================

/// A [`Position`] with its resolved price and computed market value.
///
/// `quote == None` means the quote failed / went stale and there was no manual
/// fallback; `market_value` is `None` exactly when `quote` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricedPosition {
    /// The underlying holding. Its `shares` field is the CONFIRMED baseline; the
    /// estimated current share count (baseline + DRIP accretion) is reflected in
    /// `market_value` and labeled by `share_provenance`.
    pub position: Position,
    /// The resolved quote; `None` => failed/stale and no manual fallback.
    pub quote: Option<PriceQuote>,
    /// `current_shares * price` rounded to cents (current shares = baseline + DRIP
    /// accretion, §3); `None` iff `quote` is `None`.
    pub market_value: Option<Money>,
    /// Whether the share count is confirmed (`Uploaded`) or a DRIP estimate
    /// (`DripEstimated`) accreted since the last upload (Phase 7, §2.5/§8). The
    /// AI review and UI surface this label so nothing estimated reads as confirmed
    /// (`BUDGET-AI-1`).
    pub share_provenance: ShareProvenance,
}

/// The grounding snapshot the advisor reasons over — the locked, citable
/// ground-truth surface (`ORCH-ONE-WAY-DOOR-1`).
///
/// Every figure a [`Claim`] may cite is reconcilable against exactly one field
/// here: a `Position` claim against the matching `positions[i].market_value`, a
/// `Buffer` claim against `buffer_total`, a `NetWorth` claim against
/// `net_worth.total`. Adding a new citable figure is a coordinated change across
/// the whole context (see the module header).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    /// Owning user.
    pub user_id: UserId,
    /// Priced holdings (one per [`Position`]).
    pub positions: Vec<PricedPosition>,
    /// All cash balances (reserved and unreserved).
    pub cash_balances: Vec<CashBalance>,
    /// Sum of reserved balances (the non-investable buffer).
    pub buffer_total: Money,
    /// Aggregate net worth.
    pub net_worth: NetWorth,
    /// Sum of resolved market values (skips unresolved positions).
    pub total_invested: Money,
    /// Snapshot capture instant, UTC-anchored (`ARCH-UTC-TIMESTAMPS-1`).
    pub captured_at: DateTime<Utc>,
}

// ===========================================================================
// Recommendation / Claim / ClaimSubject
// ===========================================================================

/// The model's SELF-REPORTED confidence in a [`Recommendation`].
///
/// A DISPLAY signal only: it drives the "low-confidence flagged" guardrail in the
/// UI. It is NOT reconciled against ground truth and is explicitly OUTSIDE
/// `BUDGET-AI-1` (which governs only the tuple-reconciliation of [`Claim`]s). Rides
/// inside `review_runs.recommendations` JSONB — no dedicated column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confidence {
    /// The model reports high confidence.
    High,
    /// The model reports medium confidence.
    Medium,
    /// The model reports low confidence (drives the low-confidence display flag).
    Low,
}

/// One model-produced recommendation: a title, a rationale, the model's
/// self-reported confidence, and the verifiable numeric claims it makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recommendation {
    /// Short headline for the recommendation card.
    pub title: String,
    /// The supporting prose.
    pub rationale: String,
    /// The model's SELF-REPORTED confidence — a DISPLAY signal only, NOT
    /// reconciled, NOT part of `BUDGET-AI-1`.
    pub confidence: Confidence,
    /// The verifiable numeric claims this recommendation makes.
    pub claims: Vec<Claim>,
}

/// A single verifiable numeric claim a [`Recommendation`] makes about the
/// portfolio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// What the claim is about (a position, the buffer, or net worth).
    pub subject: ClaimSubject,
    /// The figure the model cited.
    pub cited_value: Money,
    /// Optional cited "% of portfolio" for a `Position` subject, as a RATIO
    /// (e.g. `0.4` for 40%), NOT a 0–100 percentage. `Buffer`/`NetWorth` MUST
    /// carry `None` (enforced in `reconcile`). The UI formatter multiplies by 100
    /// for display.
    pub cited_percentage: Option<Decimal>,
}

/// What a [`Claim`] is about. CLOSED enum — adding a variant forces a new
/// `reconcile` arm (`BUDGET-AI-1`). NEVER defeat this with a `_ =>` wildcard.
///
/// Four variants. The `reconcile` arm for [`ClaimSubject::CostBasisGain`] is built
/// with Phase 5 (it does not exist yet): ground truth = that position's
/// `market_value - cost_basis`; ticker not found → `UnknownTicker`; `cost_basis`
/// is `None` → `Unverified(MissingMarketData(ticker))` (the unrealized gain cannot
/// be computed when cost basis is absent — same "a required figure is missing"
/// semantics as a missing quote, so the reason is reused rather than adding a
/// dedicated variant; revisit if a `MissingCostBasis` reason reads better in the UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimSubject {
    /// A claim about one holding's market value (and optionally its % share).
    Position {
        /// The holding the claim is about.
        ticker: Ticker,
    },
    /// A claim about the reserved cash buffer total.
    Buffer,
    /// A claim about total net worth.
    NetWorth,
    /// A claim about one holding's unrealized gain (`market_value - cost_basis`).
    CostBasisGain {
        /// The holding the unrealized-gain claim is about.
        ticker: Ticker,
    },
}

// ===========================================================================
// Outcomes
// ===========================================================================

/// The reconciliation outcome for a single claim (or, aggregated, a whole
/// recommendation — the worst across its claims).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationOutcome {
    /// The cited figure reconciles against ground truth.
    Verified,
    /// The cited figure could not be verified, for the carried reason.
    Unverified(UnverifiedReason),
}

/// Why a claim failed reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnverifiedReason {
    /// A `Position` claim named a ticker not present in the snapshot.
    UnknownTicker(String),
    /// A cited monetary figure did not match ground truth.
    ValueMismatch {
        /// The figure the model cited.
        cited: Money,
        /// The reconciled ground-truth figure.
        ground_truth: Money,
    },
    /// The position exists but had no resolved market value (stale/failed quote,
    /// no manual fallback). Carries the ticker symbol.
    MissingMarketData(String),
    /// A cited "% of portfolio" ratio did not match ground truth.
    PercentageMismatch {
        /// The ratio the model cited.
        cited: Decimal,
        /// The reconciled ground-truth ratio.
        ground_truth: Decimal,
    },
    /// The claim was structurally malformed (e.g. a `Buffer`/`NetWorth` claim
    /// carrying a `cited_percentage`). Carries a human description.
    MalformedClaim(String),
}

// ===========================================================================
// ReviewRun + ReviewTerminalState
// ===========================================================================

/// The persisted audit row for one portfolio-review invocation
/// (`SQL-AUDIT-COLUMNS-1`).
///
/// Self-contained: it carries both the model's parsed `recommendations` and the
/// per-recommendation `outcomes` (indexed: `outcomes[i]` indexes into
/// `recommendations[i]`, the LOCKED paired shape per `§0.4`), so the Phase-6
/// mapper can render cards without re-parsing `raw_output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRun {
    /// Stable identity for this audit row.
    pub id: ReviewRunId,
    /// Owning user.
    pub user_id: UserId,
    /// The advisor model id that produced the output.
    pub model_id: String,
    /// A hash of the rendered prompt (reproducibility / dedup).
    pub prompt_hash: String,
    /// The raw model output (also the home for a parse-failure payload).
    pub raw_output: String,
    /// The grounding snapshot the review reconciled against.
    pub snapshot: PortfolioSnapshot,
    /// The model's parsed recommendations (empty for `EmptyPortfolio` or a
    /// zero-rec `NoVerifiableInsights`).
    pub recommendations: Vec<Recommendation>,
    /// Per-recommendation reconciliation outcomes, paired with their index
    /// (`outcomes[i].0` indexes into `recommendations`). LOCKED shape, `§0.4`.
    pub outcomes: Vec<(usize, ValidationOutcome)>,
    /// The classified terminal state of the run.
    pub terminal_state: ReviewTerminalState,
    /// Prompt token count, if the provider reported it.
    pub prompt_tokens: Option<i64>,
    /// Completion token count, if the provider reported it.
    pub completion_tokens: Option<i64>,
    /// The model's stop/finish reason, surfaced for audit (truncation /
    /// safety-stop detection). `None` on the short-circuit / parse-failure paths
    /// where the model produced no candidate. Written from
    /// [`AdvisorOutput::finish_reason`].
    pub finish_reason: Option<String>,
    /// Measured latency around the model call, in milliseconds.
    pub latency_ms: i64,
    /// When the review occurred, UTC-anchored (`ARCH-UTC-TIMESTAMPS-1`).
    pub occurred_at: DateTime<Utc>,
}

/// The terminal classification of a review run.
///
/// A stale/failed quote is NOT a terminal state — it degrades a position to
/// `quote: None`, making any citing claim `Unverified(MissingMarketData)` by
/// construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewTerminalState {
    /// ≥1 verifiable recommendation. SUCCESS.
    Completed,
    /// Valid JSON, zero recs OR zero verifiable recs. SUCCESS.
    NoVerifiableInsights,
    /// Short-circuit before the model call (no positions and no cash). SUCCESS.
    EmptyPortfolio,
    /// Parse failure. FAILURE-of-review (the run is still persisted with the
    /// raw output for audit).
    MalformedOutput,
}

// ===========================================================================
// Ports
// ===========================================================================

/// The investment-advisor port — the single boundary the use-case calls to turn
/// a grounding snapshot into recommendations (`ORCH-ONE-WAY-DOOR-1`).
///
/// Object-safe (`Send + Sync`, `#[async_trait]`) so the use-case can hold
/// `Arc<dyn InvestmentAdvisor>`. Returning [`AdvisorOutput`] (not a bare
/// `Vec<Recommendation>`) keeps the audit counters and the raw output on the port
/// boundary, and gives the parse-failure path its raw-string home.
#[async_trait]
pub trait InvestmentAdvisor: Send + Sync {
    /// Produce recommendations grounded in `snapshot`.
    ///
    /// # Errors
    /// [`AdvisorError::Api`]/[`RateLimited`]/[`Unavailable`]/[`Parse`]/[`SecretVault`]
    /// on the respective failures.
    ///
    /// [`RateLimited`]: AdvisorError::RateLimited
    /// [`Unavailable`]: AdvisorError::Unavailable
    /// [`Parse`]: AdvisorError::Parse
    /// [`SecretVault`]: AdvisorError::SecretVault
    async fn recommend(&self, snapshot: &PortfolioSnapshot) -> Result<AdvisorOutput, AdvisorError>;

    /// The model id this advisor is configured to use — recorded on the audit
    /// row.
    fn model_id(&self) -> &str;
}

/// Everything an advisor call yields: the parsed recommendations plus the audit
/// metadata the use-case persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisorOutput {
    /// The parsed recommendations.
    pub recommendations: Vec<Recommendation>,
    /// The raw model output (also persisted to `review_runs.raw_output`).
    pub raw_output: String,
    /// A hash of the rendered prompt (reproducibility / dedup).
    pub prompt_hash: String,
    /// Prompt token count, if the provider reported it.
    pub prompt_tokens: Option<i64>,
    /// Completion token count, if the provider reported it.
    pub completion_tokens: Option<i64>,
    /// The model's stop/finish reason (truncation / safety-stop detection),
    /// mapped through from the Gemini wire's `finish_reason`. `None` on the mock
    /// and the parse-failure paths (the `Parse` error path produces an
    /// [`AdvisorError`], not an `AdvisorOutput`). Persisted to
    /// `review_runs.finish_reason`.
    pub finish_reason: Option<String>,
}

/// The market-data port — resolves a per-ticker quote.
///
/// Object-safe (`Send + Sync`, `#[async_trait]`). `Ok(None)` => no quote for that
/// ticker (the caller falls back to a manual price or degrades the position to
/// `quote: None`).
#[async_trait]
pub trait MarketDataProvider: Send + Sync {
    /// Resolve a quote for `ticker`. `Ok(None)` => no quote (caller degrades).
    ///
    /// # Errors
    /// [`MarketDataError::Api`]/[`RateLimited`]/[`SecretVault`] on the respective
    /// failures.
    ///
    /// [`RateLimited`]: MarketDataError::RateLimited
    /// [`SecretVault`]: MarketDataError::SecretVault
    async fn quote(&self, ticker: &Ticker) -> Result<Option<PriceQuote>, MarketDataError>;
}

/// The dividend-data port — resolves a ticker's dividend events after a cutoff
/// date (Phase 7, §5). Object-safe (`Send + Sync`, `#[async_trait]`) so the
/// catch-up engine can hold `Arc<dyn DividendSource>`.
///
/// `Ok(vec![])` => no dividends for that ticker after `since` (not an error). The
/// concrete adapters (`Tiingo`/`Yahoo`/`Manual`/`Mock`, composed by a chain)
/// live in `budget-infrastructure`, mirroring the [`MarketDataProvider`] shape.
#[async_trait]
pub trait DividendSource: Send + Sync {
    /// Dividend events for `ticker` whose `pay_date` is strictly after `since`.
    ///
    /// `Ok(vec![])` means none. Implementations may cache aggressively (dividends
    /// are quarterly), so this is safe to call on every snapshot assembly.
    ///
    /// # Errors
    /// [`DividendSourceError::Api`]/[`RateLimited`]/[`SecretVault`] on the
    /// respective failures.
    ///
    /// [`RateLimited`]: DividendSourceError::RateLimited
    /// [`SecretVault`]: DividendSourceError::SecretVault
    async fn dividends_since(
        &self,
        ticker: &Ticker,
        since: NaiveDate,
    ) -> Result<Vec<DividendEvent>, DividendSourceError>;
}

/// Read port for a user's positions (the `Position` read side; the write side is
/// [`crate::repositories::PositionRepository`]).
#[async_trait]
pub trait PositionSource: Send + Sync {
    /// All positions for a user.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn positions_for_user(&self, user_id: UserId) -> Result<Vec<Position>, RepositoryError>;
}

/// Read port for a user's cash balances (the `CashBalance` read side; the write
/// side is [`crate::repositories::CashBalanceRepository`]).
#[async_trait]
pub trait CashBalanceSource: Send + Sync {
    /// All cash balances for a user.
    ///
    /// # Errors
    /// [`RepositoryError`] on any persistence failure.
    async fn balances_for_user(&self, user_id: UserId)
    -> Result<Vec<CashBalance>, RepositoryError>;
}

// ===========================================================================
// Port error enums (NO secret material — §0.3)
// ===========================================================================

/// Failures from the [`InvestmentAdvisor`] port.
///
/// Carries only `String` payloads — never an HTTP status, a Gemini error object,
/// or the API key (`§0.3`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdvisorError {
    /// The advisor API returned a failure.
    #[error("advisor api failure: {0}")]
    Api(String),
    /// The advisor API rate-limited the request.
    #[error("advisor rate limited: {0}")]
    RateLimited(String),
    /// The advisor service was unavailable.
    #[error("advisor unavailable: {0}")]
    Unavailable(String),
    /// The advisor output could not be parsed (carries the raw output).
    #[error("advisor output parse failure: {0}")]
    Parse(String),
    /// Resolving the API secret from the vault failed.
    #[error("secret vault failure: {0}")]
    SecretVault(String),
}

/// Failures from the [`MarketDataProvider`] port.
///
/// Carries only `String` payloads — never an HTTP status or the API key (`§0.3`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarketDataError {
    /// The market-data API returned a failure.
    #[error("market data api failure: {0}")]
    Api(String),
    /// The market-data API rate-limited the request.
    #[error("market data rate limited: {0}")]
    RateLimited(String),
    /// Resolving the API secret from the vault failed.
    #[error("secret vault failure: {0}")]
    SecretVault(String),
}

/// Failures from the [`DividendSource`] port (Phase 7).
///
/// Carries only `String` payloads — never an HTTP status or the API key (`§0.3`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DividendSourceError {
    /// The dividend API returned a failure.
    #[error("dividend api failure: {0}")]
    Api(String),
    /// The dividend API rate-limited the request.
    #[error("dividend rate limited: {0}")]
    RateLimited(String),
    /// Resolving the API secret from the vault failed.
    #[error("secret vault failure: {0}")]
    SecretVault(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that maps a `Ticker` result to its inner string, so assertions
    /// compare whole `Result`s without unwrapping (`unwrap`/`expect`/`panic` are
    /// lint-denied even in tests).
    fn ticker_str(raw: &str) -> Result<String, ValidationError> {
        Ticker::try_new(raw).map(|t| t.as_str().to_string())
    }

    #[test]
    fn normalises_lowercase_to_upper() {
        assert_eq!(ticker_str("  aapl "), Ok("AAPL".to_string()));
    }

    #[test]
    fn accepts_dotted_class_ticker() {
        assert_eq!(ticker_str("brk.a"), Ok("BRK.A".to_string()));
    }

    #[test]
    fn accepts_single_letter_ticker() {
        assert_eq!(ticker_str("f"), Ok("F".to_string()));
    }

    #[test]
    fn rejects_empty_ticker() {
        assert_eq!(
            Ticker::try_new("   "),
            Err(ValidationError::Empty { field: "ticker" })
        );
    }

    #[test]
    fn rejects_too_long_ticker() {
        assert!(matches!(
            Ticker::try_new("ABCDEFGHIJK"), // 11 chars
            Err(ValidationError::TooLong {
                field: "ticker",
                max: 10,
                actual: 11
            })
        ));
    }

    #[test]
    fn rejects_digits() {
        assert!(matches!(
            Ticker::try_new("AA1"),
            Err(ValidationError::Format {
                field: "ticker",
                ..
            })
        ));
    }

    #[test]
    fn rejects_embedded_space() {
        assert!(matches!(
            Ticker::try_new("AA PL"),
            Err(ValidationError::Format {
                field: "ticker",
                ..
            })
        ));
    }

    #[test]
    fn round_trips_through_display_and_into_string() {
        let normalized = Ticker::try_new("aapl").map(|t| t.to_string());
        assert_eq!(normalized, Ok("AAPL".to_string()));
        let owned = Ticker::try_new("brk.a").map(Ticker::into_string);
        assert_eq!(owned, Ok("BRK.A".to_string()));
    }

    #[test]
    fn confidence_serde_round_trips_each_variant() {
        for variant in [Confidence::High, Confidence::Medium, Confidence::Low] {
            let encoded = serde_json::to_string(&variant);
            let decoded = encoded
                .as_deref()
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<Confidence>(s).map_err(|e| e.to_string()));
            assert_eq!(decoded, Ok(variant));
        }
    }

    #[test]
    fn cost_basis_gain_subject_serde_round_trips() {
        // The fourth ClaimSubject variant survives a JSON round-trip with its
        // validated Ticker payload intact (`ORCH-NEW-PATH-TESTS-1`).
        let subject = Ticker::try_new("aapl").map(|ticker| ClaimSubject::CostBasisGain { ticker });
        let round_tripped = subject.as_ref().ok().and_then(|s| {
            serde_json::to_string(s)
                .ok()
                .and_then(|json| serde_json::from_str::<ClaimSubject>(&json).ok())
        });
        assert_eq!(round_tripped, subject.ok());
    }

    #[test]
    fn dividend_source_kind_round_trips_label() {
        for kind in [
            DividendSourceKind::Tiingo,
            DividendSourceKind::Yahoo,
            DividendSourceKind::Manual,
            DividendSourceKind::Mock,
        ] {
            assert_eq!(DividendSourceKind::try_from_str(kind.as_str()), Ok(kind));
        }
    }

    #[test]
    fn dividend_source_kind_rejects_unknown_label() {
        assert!(matches!(
            DividendSourceKind::try_from_str("alphavantage"),
            Err(ValidationError::Format {
                field: "dividend_source",
                ..
            })
        ));
    }

    #[test]
    fn dividend_event_serde_round_trips() {
        let event = Ticker::try_new("AAPL").ok().and_then(|ticker| {
            let ex_date = NaiveDate::from_ymd_opt(2026, 5, 8)?;
            let pay_date = NaiveDate::from_ymd_opt(2026, 5, 15)?;
            Some(DividendEvent {
                ticker,
                ex_date,
                pay_date,
                amount_per_share: Money::from_minor(25),
                source: DividendSourceKind::Tiingo,
            })
        });
        let round_tripped = event.as_ref().and_then(|e| {
            serde_json::to_string(e)
                .ok()
                .and_then(|json| serde_json::from_str::<DividendEvent>(&json).ok())
        });
        assert_eq!(round_tripped, event);
    }

    #[test]
    fn share_provenance_serde_round_trips_each_variant() {
        let baseline = Utc::now();
        for provenance in [
            ShareProvenance::Uploaded,
            ShareProvenance::DripEstimated {
                events_applied: 3,
                baseline_as_of: baseline,
            },
        ] {
            let decoded = serde_json::to_string(&provenance)
                .ok()
                .and_then(|s| serde_json::from_str::<ShareProvenance>(&s).ok());
            assert_eq!(decoded, Some(provenance));
        }
    }

    #[test]
    fn advisor_output_finish_reason_is_optional() {
        // The added finish_reason field is `None` on the mock/short-circuit path.
        let output = AdvisorOutput {
            recommendations: Vec::new(),
            raw_output: String::new(),
            prompt_hash: String::new(),
            prompt_tokens: None,
            completion_tokens: None,
            finish_reason: None,
        };
        assert_eq!(output.finish_reason, None);
    }
}
