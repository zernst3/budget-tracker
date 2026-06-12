//! AI Portfolio Insights — server functions + wire DTOs (`RUST-DIOXUS-9`,
//! `BUDGET-AUTH-GATE-1`, `ARCH-API-DTOS-1`, `RUST-DIOXUS-10`).
//!
//! The server-function surface for the manual positions / cash-balances UI and
//! (in later phases) the grounding snapshot + review run. Eight `#[server]`
//! functions, each gated by [`require_authed_user`] FIRST
//! (`BUDGET-AUTH-GATE-1`) and returning serde DTOs only — raw domain
//! discriminants / `UnverifiedReason` codes NEVER cross to the client
//! (`RUST-DIOXUS-10`).
//!
//! ## Phase status (`docs/AI_FEATURE_DESIGN.md`)
//!
//! - **Phase 2:** the six position/balance CRUD functions are live.
//! - **Phase 3:** [`portfolio_snapshot`] goes live (market-data fan-out).
//! - **Phase 6 (this phase):** [`run_review`] gets its body (the real review
//!   pipeline: validate the chosen model against the allow-list, build the
//!   use-case for mock or real `GeminiAdvisor`, persist the audit run, map to
//!   [`ReviewResultDto`] via [`review_run_to_dto`]); [`list_models`] exposes the
//!   model-id allow-list for the dropdown (locked decision #1).
//!
//! ## Money representation (`BUDGET-MONEY-1`)
//!
//! Every monetary DTO field is a decimal STRING (`Money`'s exact `Decimal`
//! rendered via `to_string`), not an `f64`. The server parses the string back
//! through `Money::try_parse` on the write path; no float is ever computed. The
//! view formats the string for display.
//!
//! ## DTO mapper location (judgment call, documented)
//!
//! `docs/AI_FEATURE_DESIGN.md §Phase 2` lists the position/cash DTO mappers under
//! a new `budget-mappers/src/portfolio.rs`. That crate maps `SeaORM Model ↔
//! domain` and depends on `sea-orm` (NOT WASM-clean), while these DTOs must be
//! WASM-clean (they cross to the client) and `budget-ui` does not depend on
//! `budget-mappers`. So — matching the established `services::ledger` /
//! `services::triage` convention, where domain→DTO conversion lives beside the
//! server function — the pure conversion helpers ([`position_to_dto`],
//! [`add_position_dto_to_domain`], [`cash_balance_to_dto`]) live here and are
//! unit-tested directly.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// DTOs (serde, WASM-clean; Money rendered as String — BUDGET-MONEY-1)
// ---------------------------------------------------------------------------

/// A stored position, rendered for the read-only positions table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionDto {
    /// Stable position id (the edit/delete target).
    pub id: Uuid,
    /// The ticker symbol (already validated upstream).
    pub ticker: String,
    /// Human label for the holding's account.
    pub account_label: String,
    /// The account type as a snake_case string (`"investment"` etc.).
    pub account_type: String,
    /// Share count as a decimal string (a COUNT, not money — `BUDGET-MONEY-1`).
    pub shares: String,
    /// Optional cost basis as a decimal string; `None` if unset.
    pub cost_basis: Option<String>,
}

/// The add/edit position form payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddPositionDto {
    /// The ticker symbol (re-validated server-side via `Ticker::try_new`).
    pub ticker: String,
    /// Human label for the holding's account.
    pub account_label: String,
    /// The account type as a snake_case string.
    pub account_type: String,
    /// Share count as a decimal string.
    pub shares: String,
    /// Optional cost basis as a decimal string.
    pub cost_basis: Option<String>,
}

/// A cash balance, for the read/write balances surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CashBalanceDto {
    /// Stable row id; `None` on a fresh upsert payload (server resolves by the
    /// `(user, account_label)` natural key).
    pub id: Option<Uuid>,
    /// Human label for the cash account.
    pub account_label: String,
    /// The balance as a decimal string (`BUDGET-CASH-1`: a stock, never a flow).
    pub balance: String,
    /// `true` => a reserved buffer (sums into the snapshot buffer total).
    pub reserved: bool,
}

/// A priced position for the snapshot table (Phase 3 populates this).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricedPositionDto {
    /// The ticker symbol.
    pub ticker: String,
    /// Human account label.
    pub account_label: String,
    /// Account type, snake_case.
    pub account_type: String,
    /// Share count as a decimal string.
    pub shares: String,
    /// Resolved per-share price as a decimal string; `None` when unresolved.
    pub price: Option<String>,
    /// Price provenance (`"market:finnhub"` / `"manual"`); `None` when unresolved.
    pub provenance: Option<String>,
    /// Quote observation instant (RFC3339); `None` when unresolved.
    pub as_of: Option<String>,
    /// `shares * price` as a decimal string; `None` when unresolved.
    pub market_value: Option<String>,
    /// `% of portfolio` rendered to 1 dp (e.g. `"41.9"`); `None` when unresolved.
    pub pct_of_portfolio: Option<String>,
    /// `true` when the quote is absent/old (degraded position).
    pub is_stale: bool,
    /// `true` when the share count is a DRIP estimate accreted since the last
    /// upload (not the confirmed baseline). Drives the "estimated since last
    /// upload" badge so nothing estimated renders as confirmed (`BUDGET-AI-1`,
    /// §2.5/§8).
    pub shares_estimated: bool,
    /// A pre-rendered HUMAN badge label when the shares are estimated (e.g.
    /// `"estimated · 2 dividends since last upload"`); `None` when the count is
    /// the confirmed `Uploaded` baseline. The raw `ShareProvenance` discriminant
    /// never crosses to the client (`RUST-DIOXUS-10`).
    pub estimated_badge: Option<String>,
}

/// Aggregate net worth, all fields decimal strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetWorthDto {
    /// Sum of all cash balances.
    pub total_cash: String,
    /// Sum of resolved position market values.
    pub total_positions: String,
    /// Liabilities (v1: always `"0"`).
    pub liabilities: String,
    /// `total_cash + total_positions - liabilities`.
    pub total: String,
}

/// The assembled grounding snapshot, rendered for the snapshot view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortfolioSnapshotDto {
    /// Priced holdings.
    pub positions: Vec<PricedPositionDto>,
    /// All cash balances.
    pub cash_balances: Vec<CashBalanceDto>,
    /// Sum of reserved balances (the buffer).
    pub buffer_total: String,
    /// Aggregate net worth.
    pub net_worth: NetWorthDto,
    /// Sum of resolved market values.
    pub total_invested: String,
    /// Snapshot capture instant (RFC3339).
    pub captured_at: String,
}

/// A reconciled claim, with its validation badge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimDto {
    /// Pre-rendered human subject string (`"AAPL market value"` etc.). Raw
    /// discriminants never cross to the client (`RUST-DIOXUS-10`).
    pub subject: String,
    /// The cited figure as a decimal string.
    pub cited_value: String,
    /// Optional cited `% of portfolio` as a decimal string.
    pub cited_percentage: Option<String>,
    /// The reconciliation badge for this claim.
    pub badge: ValidationBadgeDto,
}

/// The reconciliation badge (verified / unverified-with-human-reason).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ValidationBadgeDto {
    /// The claim reconciled.
    Verified,
    /// The claim did not reconcile; `reason` is a human string (rendered at the
    /// server boundary, `RUST-DIOXUS-10`).
    Unverified {
        /// Human-readable reason.
        reason: String,
    },
}

/// A recommendation card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecommendationDto {
    /// Headline.
    pub title: String,
    /// Supporting prose.
    pub rationale: String,
    /// Model self-reported confidence (`"high"`/`"medium"`/`"low"`) — display
    /// only, never reconciled (`§added 2026-06-11`).
    pub confidence: String,
    /// The recommendation's aggregate badge (worst across its claims).
    pub badge: ValidationBadgeDto,
    /// The reconciled claims.
    pub claims: Vec<ClaimDto>,
    /// Optional deterministically-computed tax note (Phase 6).
    pub tax_note: Option<String>,
}

/// The terminal state of a review run, for the result screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReviewTerminalStateDto {
    /// ≥1 verifiable recommendation.
    Completed,
    /// Valid JSON, zero recs / zero verifiable.
    NoVerifiableInsights,
    /// Short-circuit before the model call.
    EmptyPortfolio,
    /// Parse failure.
    MalformedOutput,
}

/// The full review result for the result screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewResultDto {
    /// The persisted run id.
    pub run_id: Uuid,
    /// The classified terminal state.
    pub terminal_state: ReviewTerminalStateDto,
    /// The recommendation cards (empty for empty/malformed runs).
    pub recommendations: Vec<RecommendationDto>,
    /// The standing not-financial-advice disclaimer (`N3`), rendered on every
    /// result. It is a `&'static str` constant ([`PORTFOLIO_REVIEW_DISCLAIMER`]),
    /// not user data, so it is NOT serialized across the wire — it is skipped and
    /// repopulated to the same constant on the client (which keeps the DTO
    /// `DeserializeOwned` for the `#[server]` boundary while honoring the locked
    /// `&'static str` field type).
    #[serde(skip, default = "default_disclaimer")]
    pub disclaimer: &'static str,
}

/// The serde default for [`ReviewResultDto::disclaimer`] — the standing
/// constant, so a deserialized result always carries the disclaimer.
#[must_use]
fn default_disclaimer() -> &'static str {
    PORTFOLIO_REVIEW_DISCLAIMER
}

/// The standing not-financial-advice disclaimer (`N3`) — rendered on every
/// review result.
pub const PORTFOLIO_REVIEW_DISCLAIMER: &str = "These insights are generated by an AI model and reconciled against your own \
     entered data. They are informational only and are NOT financial, investment, \
     or tax advice. Verify every figure and consult a licensed professional before \
     acting.";

// ---------------------------------------------------------------------------
// Pure domain<->DTO conversions (server-only; touch domain types)
// ---------------------------------------------------------------------------

/// Render a domain [`AccountType`](budget_domain::enums::AccountType) to its
/// snake_case wire string.
#[cfg(feature = "server")]
#[must_use]
pub fn account_type_to_string(t: budget_domain::enums::AccountType) -> &'static str {
    use budget_domain::enums::AccountType;
    match t {
        AccountType::Checking => "checking",
        AccountType::Credit => "credit",
        AccountType::Savings => "savings",
        AccountType::Investment => "investment",
        AccountType::Other => "other",
    }
}

/// Parse a snake_case account-type wire string back to the domain enum.
///
/// # Errors
/// Returns a human error string if the value is not a known account type.
#[cfg(feature = "server")]
pub fn account_type_from_string(s: &str) -> Result<budget_domain::enums::AccountType, String> {
    use budget_domain::enums::AccountType;
    match s {
        "checking" => Ok(AccountType::Checking),
        "credit" => Ok(AccountType::Credit),
        "savings" => Ok(AccountType::Savings),
        "investment" => Ok(AccountType::Investment),
        "other" => Ok(AccountType::Other),
        other => Err(format!("unknown account_type '{other}'")),
    }
}

/// Render a domain [`Position`](budget_domain::portfolio::Position) to a
/// [`PositionDto`].
#[cfg(feature = "server")]
#[must_use]
pub fn position_to_dto(p: &budget_domain::portfolio::Position) -> PositionDto {
    PositionDto {
        id: p.id.value(),
        ticker: p.ticker.as_str().to_owned(),
        account_label: p.account_label.clone(),
        account_type: account_type_to_string(p.account_type).to_owned(),
        shares: p.shares.to_string(),
        cost_basis: p.cost_basis.map(|m| m.as_decimal().to_string()),
    }
}

/// Build a domain [`Position`](budget_domain::portfolio::Position) from an
/// [`AddPositionDto`], supplying the persistence context (`id`, `user_id`,
/// timestamps) the form payload does not carry.
///
/// # Errors
/// Returns a human error string when the ticker fails validation, the
/// account-type string is unknown, or `shares`/`cost_basis` fail to parse as a
/// decimal.
#[cfg(feature = "server")]
pub fn add_position_dto_to_domain(
    id: budget_domain::ids::PositionId,
    user_id: budget_domain::ids::UserId,
    input: &AddPositionDto,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<budget_domain::portfolio::Position, String> {
    use budget_domain::money::Money;
    use budget_domain::portfolio::{Position, Ticker};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let ticker = Ticker::try_new(&input.ticker).map_err(|e| e.to_string())?;
    let account_type = account_type_from_string(&input.account_type)?;
    let shares = Decimal::from_str(input.shares.trim())
        .map_err(|e| format!("invalid shares '{}': {e}", input.shares))?;
    let cost_basis = match &input.cost_basis {
        None => None,
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(Money::try_parse("cost_basis", s).map_err(|e| e.to_string())?),
    };
    Ok(Position {
        id,
        user_id,
        ticker,
        account_label: input.account_label.clone(),
        account_type,
        shares,
        cost_basis,
        // A newly-added holding: DRIP off by default (opt-in, §2.7); the baseline
        // as-of is the add instant (BUDGET-CUTOVER-1).
        drip_enabled: false,
        baseline_as_of: now,
        created_at: now,
        updated_at: now,
    })
}

/// Render a domain [`CashBalance`](budget_domain::portfolio::CashBalance) to a
/// [`CashBalanceDto`].
///
/// The domain value is `id`-free; the row id is supplied by the caller (the
/// server fn pulls it from the loaded entity when available).
#[cfg(feature = "server")]
#[must_use]
pub fn cash_balance_to_dto(
    id: Option<Uuid>,
    b: &budget_domain::portfolio::CashBalance,
) -> CashBalanceDto {
    CashBalanceDto {
        id,
        account_label: b.account_label.clone(),
        balance: b.balance.as_decimal().to_string(),
        reserved: b.reserved,
    }
}

/// Build a domain [`CashBalance`](budget_domain::portfolio::CashBalance) from a
/// [`CashBalanceDto`].
///
/// # Errors
/// Returns a human error string when `balance` does not parse as a decimal.
#[cfg(feature = "server")]
pub fn cash_balance_dto_to_domain(
    input: &CashBalanceDto,
) -> Result<budget_domain::portfolio::CashBalance, String> {
    use budget_domain::money::Money;
    use budget_domain::portfolio::CashBalance;

    let balance = Money::try_parse("balance", &input.balance).map_err(|e| e.to_string())?;
    Ok(CashBalance {
        account_label: input.account_label.clone(),
        balance,
        reserved: input.reserved,
    })
}

// ---------------------------------------------------------------------------
// Snapshot domain -> DTO (Phase 3)
// ---------------------------------------------------------------------------

/// A quote older than this is flagged stale (`is_stale`) in the snapshot DTO.
///
/// Not pinned by the design; chosen as 24h so an end-of-day quote shown the next
/// morning is flagged rather than silently presented as current. An absent quote
/// is always stale. (Reconciliation does not key off this flag — a degraded
/// position has `market_value: None`, which `reconcile` turns into
/// `MissingMarketData`; the flag is a UI signal only.)
#[cfg(feature = "server")]
const STALE_AFTER_HOURS: i64 = 24;

/// Render a domain [`PriceProvenance`](budget_domain::portfolio::PriceProvenance)
/// to a wire string (`"market:finnhub"` / `"manual"`).
#[cfg(feature = "server")]
#[must_use]
fn provenance_to_string(p: &budget_domain::portfolio::PriceProvenance) -> String {
    use budget_domain::portfolio::PriceProvenance;
    match p {
        PriceProvenance::Market { source } => format!("market:{source}"),
        PriceProvenance::Manual => "manual".to_owned(),
    }
}

/// Render a [`ShareProvenance`](budget_domain::portfolio::ShareProvenance) to the
/// `(shares_estimated, estimated_badge)` pair for the DTO (`RUST-DIOXUS-10`,
/// §2.5/§8). `Uploaded` → `(false, None)`; `DripEstimated` → `(true, Some(human
/// badge))`. The raw discriminant + the raw `events_applied`/`baseline_as_of`
/// internals never cross to the client; only the human string does.
#[cfg(feature = "server")]
#[must_use]
fn share_provenance_to_badge(
    provenance: &budget_domain::portfolio::ShareProvenance,
) -> (bool, Option<String>) {
    use budget_domain::portfolio::ShareProvenance;
    match provenance {
        ShareProvenance::Uploaded => (false, None),
        ShareProvenance::DripEstimated {
            events_applied,
            baseline_as_of,
        } => {
            let plural = if *events_applied == 1 {
                "dividend"
            } else {
                "dividends"
            };
            let badge = format!(
                "estimated · {events_applied} {plural} reinvested since last upload ({})",
                baseline_as_of.format("%Y-%m-%d")
            );
            (true, Some(badge))
        }
    }
}

/// Render a [`PricedPosition`](budget_domain::portfolio::PricedPosition) to a
/// [`PricedPositionDto`].
///
/// `pct_of_portfolio` is `market_value / total_invested` as a PERCENT (ratio ×
/// 100) rounded to 1 dp; `None` when the position is unresolved or
/// `total_invested` is zero. `is_stale` is true when the quote is absent or older
/// than [`STALE_AFTER_HOURS`]. `shares_estimated`/`estimated_badge` carry the
/// DRIP-provenance label so nothing estimated renders as confirmed
/// (`BUDGET-AI-1`).
#[cfg(feature = "server")]
#[must_use]
fn priced_position_to_dto(
    pp: &budget_domain::portfolio::PricedPosition,
    total_invested: budget_domain::money::Money,
    now: chrono::DateTime<chrono::Utc>,
) -> PricedPositionDto {
    use rust_decimal::Decimal;

    let pct_of_portfolio = match (pp.market_value, total_invested.as_decimal().is_zero()) {
        (Some(mv), false) => {
            let pct = (mv.as_decimal() / total_invested.as_decimal()) * Decimal::from(100);
            Some(pct.round_dp(1).to_string())
        }
        _ => None,
    };

    let is_stale = match &pp.quote {
        None => true,
        Some(q) => (now - q.as_of).num_hours() >= STALE_AFTER_HOURS,
    };

    let (shares_estimated, estimated_badge) = share_provenance_to_badge(&pp.share_provenance);

    PricedPositionDto {
        ticker: pp.position.ticker.as_str().to_owned(),
        account_label: pp.position.account_label.clone(),
        account_type: account_type_to_string(pp.position.account_type).to_owned(),
        shares: pp.position.shares.to_string(),
        price: pp.quote.as_ref().map(|q| q.price.as_decimal().to_string()),
        provenance: pp
            .quote
            .as_ref()
            .map(|q| provenance_to_string(&q.provenance)),
        as_of: pp.quote.as_ref().map(|q| q.as_of.to_rfc3339()),
        market_value: pp.market_value.map(|m| m.as_decimal().to_string()),
        pct_of_portfolio,
        is_stale,
        shares_estimated,
        estimated_badge,
    }
}

/// Render a domain [`PortfolioSnapshot`](budget_domain::portfolio::PortfolioSnapshot)
/// to a [`PortfolioSnapshotDto`].
#[cfg(feature = "server")]
#[must_use]
pub fn snapshot_to_dto(snap: &budget_domain::portfolio::PortfolioSnapshot) -> PortfolioSnapshotDto {
    PortfolioSnapshotDto {
        positions: snap
            .positions
            .iter()
            .map(|pp| priced_position_to_dto(pp, snap.total_invested, snap.captured_at))
            .collect(),
        cash_balances: snap
            .cash_balances
            .iter()
            .map(|b| cash_balance_to_dto(None, b))
            .collect(),
        buffer_total: snap.buffer_total.as_decimal().to_string(),
        net_worth: NetWorthDto {
            total_cash: snap.net_worth.total_cash.as_decimal().to_string(),
            total_positions: snap.net_worth.total_positions.as_decimal().to_string(),
            liabilities: snap.net_worth.liabilities.as_decimal().to_string(),
            total: snap.net_worth.total.as_decimal().to_string(),
        },
        total_invested: snap.total_invested.as_decimal().to_string(),
        captured_at: snap.captured_at.to_rfc3339(),
    }
}

// ---------------------------------------------------------------------------
// review_run domain -> DTO (Phase 6) — RUST-DIOXUS-10 boundary
// ---------------------------------------------------------------------------
//
// The single boundary where the audit `ReviewRun` becomes the client-facing
// `ReviewResultDto`. Raw `ClaimSubject` discriminants and raw `UnverifiedReason`
// codes NEVER cross to the client — they are rendered to HUMAN strings HERE
// (`RUST-DIOXUS-10`, `ARCH-API-DTOS-1`). Lives in `budget-ui` (not
// `budget-mappers`) because the DTO is WASM-clean and `budget-mappers` depends on
// `sea-orm`; this mirrors the documented Phase-2 judgment call for the position /
// cash DTO mappers above.

/// Render a [`ClaimSubject`](budget_domain::portfolio::ClaimSubject) to its human
/// display string (`subject_to_display`, `RUST-DIOXUS-10`). The raw discriminant
/// never crosses to the client.
#[cfg(feature = "server")]
#[must_use]
fn subject_to_display(subject: &budget_domain::portfolio::ClaimSubject) -> String {
    use budget_domain::portfolio::ClaimSubject;
    match subject {
        ClaimSubject::Position { ticker } => format!("{ticker} market value"),
        ClaimSubject::Buffer => "Reserved cash buffer".to_owned(),
        ClaimSubject::NetWorth => "Total net worth".to_owned(),
        ClaimSubject::CostBasisGain { ticker } => format!("{ticker} unrealized gain"),
    }
}

/// Render a [`Confidence`](budget_domain::portfolio::Confidence) to its display
/// string (`confidence_to_display`). Display-only — never reconciled.
#[cfg(feature = "server")]
#[must_use]
fn confidence_to_display(confidence: &budget_domain::portfolio::Confidence) -> String {
    use budget_domain::portfolio::Confidence;
    match confidence {
        Confidence::High => "high".to_owned(),
        Confidence::Medium => "medium".to_owned(),
        Confidence::Low => "low".to_owned(),
    }
}

/// Render an [`UnverifiedReason`](budget_domain::portfolio::UnverifiedReason) to a
/// HUMAN string (`unverified_reason_to_string`, `RUST-DIOXUS-10`). Raw codes never
/// reach the client. Money/ratio figures render through their decimal string.
#[cfg(feature = "server")]
#[must_use]
fn unverified_reason_to_string(reason: &budget_domain::portfolio::UnverifiedReason) -> String {
    use budget_domain::portfolio::UnverifiedReason;
    match reason {
        UnverifiedReason::UnknownTicker(t) => {
            format!("cites {t}, which is not in your portfolio")
        }
        UnverifiedReason::ValueMismatch {
            cited,
            ground_truth,
        } => format!(
            "cited {} but your data shows {}",
            cited.as_decimal(),
            ground_truth.as_decimal()
        ),
        UnverifiedReason::MissingMarketData(t) => {
            format!("no current price for {t}, so this figure could not be checked")
        }
        UnverifiedReason::PercentageMismatch {
            cited,
            ground_truth,
        } => format!(
            "cited {}% of portfolio but your data shows {}%",
            cited * rust_decimal::Decimal::from(100),
            ground_truth * rust_decimal::Decimal::from(100)
        ),
        UnverifiedReason::MalformedClaim(detail) => {
            format!("the claim was malformed: {detail}")
        }
    }
}

/// Render a [`ValidationOutcome`](budget_domain::portfolio::ValidationOutcome) to
/// a [`ValidationBadgeDto`] (`outcome_to_badge`). The unverified reason is
/// human-rendered at this boundary.
#[cfg(feature = "server")]
#[must_use]
fn outcome_to_badge(outcome: &budget_domain::portfolio::ValidationOutcome) -> ValidationBadgeDto {
    use budget_domain::portfolio::ValidationOutcome;
    match outcome {
        ValidationOutcome::Verified => ValidationBadgeDto::Verified,
        ValidationOutcome::Unverified(reason) => ValidationBadgeDto::Unverified {
            reason: unverified_reason_to_string(reason),
        },
    }
}

/// Map the domain [`ReviewTerminalState`](budget_domain::portfolio::ReviewTerminalState)
/// to its DTO (`terminal_state_to_dto`).
#[cfg(feature = "server")]
#[must_use]
fn terminal_state_to_dto(
    state: &budget_domain::portfolio::ReviewTerminalState,
) -> ReviewTerminalStateDto {
    use budget_domain::portfolio::ReviewTerminalState;
    match state {
        ReviewTerminalState::Completed => ReviewTerminalStateDto::Completed,
        ReviewTerminalState::NoVerifiableInsights => ReviewTerminalStateDto::NoVerifiableInsights,
        ReviewTerminalState::EmptyPortfolio => ReviewTerminalStateDto::EmptyPortfolio,
        ReviewTerminalState::MalformedOutput => ReviewTerminalStateDto::MalformedOutput,
    }
}

/// Compute the deterministic tax note (N2) for a recommendation, from the
/// account_type of the positions its claims cite — NEVER from model output.
///
/// Interpretation of N2 (judgment call, documented): with the available
/// [`AccountType`](budget_domain::enums::AccountType) enum (no Roth/IRA variant),
/// the tax-relevant case is a claim about a holding in an `Investment` account:
/// trimming/selling there has capital-gains implications. A recommendation citing
/// at least one `Position` / `CostBasisGain` claim whose underlying position is in
/// an `Investment` account gets the note; everything else gets `None`. Looked up
/// against the persisted snapshot's positions (the same ground truth reconcile
/// used), so it is fully deterministic and never trusts the model.
#[cfg(feature = "server")]
#[must_use]
fn compute_tax_note(
    rec: &budget_domain::portfolio::Recommendation,
    snapshot: &budget_domain::portfolio::PortfolioSnapshot,
) -> Option<String> {
    use budget_domain::enums::AccountType;
    use budget_domain::portfolio::ClaimSubject;

    let cites_investment_holding = rec.claims.iter().any(|claim| {
        let ticker = match &claim.subject {
            ClaimSubject::Position { ticker } | ClaimSubject::CostBasisGain { ticker } => ticker,
            ClaimSubject::Buffer | ClaimSubject::NetWorth => return false,
        };
        snapshot.positions.iter().any(|pp| {
            pp.position.ticker == *ticker
                && matches!(pp.position.account_type, AccountType::Investment)
        })
    });
    cites_investment_holding.then(|| {
        "This recommendation touches a holding in a taxable investment account; \
         selling or trimming may realize a capital gain or loss. Consider the tax \
         impact and consult a professional."
            .to_owned()
    })
}

/// Render one recommendation + its outcome to a [`RecommendationDto`].
///
/// `outcome` is the recommendation's aggregate (worst-across-claims) outcome,
/// driving the card badge. Each claim is re-reconciled against the snapshot for
/// its own per-claim badge (the use-case persisted only the aggregate outcome per
/// rec; the per-claim breakdown is recomputed deterministically here against the
/// same persisted snapshot — never trusting the model).
#[cfg(feature = "server")]
#[must_use]
fn recommendation_to_dto(
    rec: &budget_domain::portfolio::Recommendation,
    aggregate_outcome: &budget_domain::portfolio::ValidationOutcome,
    snapshot: &budget_domain::portfolio::PortfolioSnapshot,
) -> RecommendationDto {
    let per_claim = budget_app_services::reconcile(rec, snapshot).per_claim;
    let claims = rec
        .claims
        .iter()
        .zip(per_claim.iter())
        .map(|(claim, (_subject, outcome))| ClaimDto {
            subject: subject_to_display(&claim.subject),
            cited_value: claim.cited_value.as_decimal().to_string(),
            cited_percentage: claim.cited_percentage.map(|p| p.to_string()),
            badge: outcome_to_badge(outcome),
        })
        .collect();

    RecommendationDto {
        title: rec.title.clone(),
        rationale: rec.rationale.clone(),
        confidence: confidence_to_display(&rec.confidence),
        badge: outcome_to_badge(aggregate_outcome),
        claims,
        tax_note: compute_tax_note(rec, snapshot),
    }
}

/// Map a persisted [`ReviewRun`](budget_domain::portfolio::ReviewRun) to the
/// client-facing [`ReviewResultDto`] (`review_run_to_dto`, `RUST-DIOXUS-10`).
///
/// Zips `recommendations[i]` with `outcomes` by the LOCKED index-paired shape
/// (`§0.4`); renders subjects/reasons/confidence to human strings at this
/// boundary; computes `tax_note` deterministically (N2) from the snapshot, never
/// from model output; always carries the standing [`PORTFOLIO_REVIEW_DISCLAIMER`].
/// `finish_reason` is audit-only and is NOT surfaced.
#[cfg(feature = "server")]
#[must_use]
pub fn review_run_to_dto(run: &budget_domain::portfolio::ReviewRun) -> ReviewResultDto {
    use budget_domain::portfolio::ValidationOutcome;

    // Index the outcomes by their paired index for an O(1) per-rec lookup.
    let recommendations = run
        .recommendations
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let aggregate = run
                .outcomes
                .iter()
                .find(|(idx, _)| *idx == i)
                .map_or(&ValidationOutcome::Verified, |(_, o)| o);
            recommendation_to_dto(rec, aggregate, &run.snapshot)
        })
        .collect();

    ReviewResultDto {
        run_id: run.id.value(),
        terminal_state: terminal_state_to_dto(&run.terminal_state),
        recommendations,
        disclaimer: PORTFOLIO_REVIEW_DISCLAIMER,
    }
}

// ---------------------------------------------------------------------------
// AI model-id allow-list config (Zach's locked decision #1)
// ---------------------------------------------------------------------------

/// The seeded default model-id allow-list when `GEMINI_MODEL_IDS` is unset
/// (Zach's locked decision #1). This is the ONLY place a model id appears as a
/// literal, and only as the seeded-default config string (`ORCH-TRAINING-CUTOFF-1`).
#[cfg(feature = "server")]
const DEFAULT_GEMINI_MODEL_IDS: &str = "gemini-2.5-pro,gemini-2.5-flash";

/// Parse a comma-separated model-id list: trims blanks, drops empties, dedups
/// while keeping first occurrence + order. Pure (no env) so it is unit-tested
/// directly (the env mutation needed to test [`allowed_model_ids`] is forbidden
/// here — `unsafe-code` is denied in this crate).
#[cfg(feature = "server")]
#[must_use]
fn parse_model_ids(raw: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.to_string()))
        .map(ToOwned::to_owned)
        .collect()
}

/// Resolve the allowed model-id list from `GEMINI_MODEL_IDS` (comma-separated),
/// falling back to the seeded default. Delegates parsing to [`parse_model_ids`].
#[cfg(feature = "server")]
#[must_use]
pub fn allowed_model_ids() -> Vec<String> {
    let raw =
        std::env::var("GEMINI_MODEL_IDS").unwrap_or_else(|_| DEFAULT_GEMINI_MODEL_IDS.to_owned());
    parse_model_ids(&raw)
}

// ---------------------------------------------------------------------------
// Server-fn error helper
// ---------------------------------------------------------------------------

/// Map a human error string to an opaque HTTP 500 `ServerFnError`.
#[cfg(feature = "server")]
fn internal_error(message: impl Into<String>) -> ServerFnError {
    ServerFnError::ServerError {
        message: message.into(),
        code: 500,
        details: None,
    }
}

// ---------------------------------------------------------------------------
// Server functions (gate FIRST — BUDGET-AUTH-GATE-1; DTOs only — ARCH-API-DTOS-1)
// ---------------------------------------------------------------------------

/// List the authenticated user's positions (`BUDGET-AUTH-GATE-1`).
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 500 on persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn list_positions() -> Result<Vec<PositionDto>, ServerFnError> {
    use crate::server_state::PortfolioState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = PortfolioState::extract().await?;

    let positions = state
        .position_source
        .positions_for_user(user.id())
        .await
        .map_err(|e| internal_error(e.to_string()))?;
    Ok(positions.iter().map(position_to_dto).collect())
}

/// Add a position for the authenticated user (`BUDGET-AUTH-GATE-1`).
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 422-shaped 500 on a malformed
/// payload; 500 on persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn add_position(input: AddPositionDto) -> Result<PositionDto, ServerFnError> {
    use budget_domain::ids::PositionId;

    use crate::server_state::PortfolioState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = PortfolioState::extract().await?;

    let position = add_position_dto_to_domain(
        PositionId::generate(),
        user.id(),
        &input,
        chrono::Utc::now(),
    )
    .map_err(internal_error)?;

    state
        .position_source
        .insert(&position)
        .await
        .map_err(|e| internal_error(e.to_string()))?;
    Ok(position_to_dto(&position))
}

/// Edit an existing position (`BUDGET-AUTH-GATE-1`). The position's `id` is taken
/// from the path arg; the rest from the form payload. `user_id`-scoped.
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 500 on a malformed payload or
/// persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn edit_position(id: Uuid, input: AddPositionDto) -> Result<PositionDto, ServerFnError> {
    use budget_domain::ids::PositionId;

    use crate::server_state::PortfolioState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = PortfolioState::extract().await?;

    let position =
        add_position_dto_to_domain(PositionId::new(id), user.id(), &input, chrono::Utc::now())
            .map_err(internal_error)?;

    state
        .position_source
        .update(&position)
        .await
        .map_err(|e| internal_error(e.to_string()))?;
    Ok(position_to_dto(&position))
}

/// Delete a position by id (`BUDGET-AUTH-GATE-1`, `user_id`-scoped per
/// `SPEC §9.1`).
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 500 on persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn delete_position(id: Uuid) -> Result<(), ServerFnError> {
    use budget_domain::ids::PositionId;

    use crate::server_state::PortfolioState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = PortfolioState::extract().await?;

    state
        .position_source
        .delete(user.id(), PositionId::new(id))
        .await
        .map_err(|e| internal_error(e.to_string()))?;
    Ok(())
}

/// List the authenticated user's cash balances (`BUDGET-AUTH-GATE-1`).
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 500 on persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn list_cash_balances() -> Result<Vec<CashBalanceDto>, ServerFnError> {
    use crate::server_state::PortfolioState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = PortfolioState::extract().await?;

    let balances = state
        .balance_source
        .balances_for_user(user.id())
        .await
        .map_err(|e| internal_error(e.to_string()))?;
    // The domain CashBalance is id-free; the table view keys off account_label,
    // so id is surfaced as None on the read path (the upsert resolves it by the
    // natural key server-side).
    Ok(balances
        .iter()
        .map(|b| cash_balance_to_dto(None, b))
        .collect())
}

/// Insert or update a cash balance, keyed by account label (`BUDGET-AUTH-GATE-1`).
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 500 on a malformed payload or
/// persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn upsert_cash_balance(input: CashBalanceDto) -> Result<CashBalanceDto, ServerFnError> {
    use crate::server_state::PortfolioState;
    use crate::services::gate::require_authed_user;

    let _user = require_authed_user().await?;
    let state = PortfolioState::extract().await?;

    let balance = cash_balance_dto_to_domain(&input).map_err(internal_error)?;
    state
        .balance_source
        .upsert(&balance)
        .await
        .map_err(|e| internal_error(e.to_string()))?;
    Ok(cash_balance_to_dto(input.id, &balance))
}

/// Assemble the grounding snapshot for the authenticated user (`§Phase 3`,
/// `§Phase 7.4`): load positions + balances concurrently, FIRST run the lazy
/// idempotent DRIP catch-up engine per position (apply any new dividends once
/// each, `BUDGET-IDEMPOTENT-MONTH-INIT-1`), then fan out market quotes via
/// `try_join_all` (`ARCH-PARALLEL-INDEPENDENT-1`) and assemble the snapshot. The
/// priced rows carry the estimated current shares + each position's
/// [`ShareProvenance`](budget_domain::portfolio::ShareProvenance) label so nothing
/// estimated renders as confirmed (`BUDGET-AI-1`).
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 500 on persistence/market/DRIP
/// failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn portfolio_snapshot() -> Result<PortfolioSnapshotDto, ServerFnError> {
    use crate::server_state::PortfolioState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = PortfolioState::extract().await?;

    // Independent reads: positions + balances concurrently
    // (`ARCH-PARALLEL-INDEPENDENT-1`).
    let (positions, balances) = tokio::try_join!(
        state.position_source.positions_for_user(user.id()),
        state.balance_source.balances_for_user(user.id()),
    )
    .map_err(|e| internal_error(e.to_string()))?;

    let snapshot = budget_app_services::assemble_snapshot_with_drip(
        user.id(),
        positions,
        balances,
        state.market.as_ref(),
        state.drip.as_ref(),
        chrono::Utc::now(),
    )
    .await
    .map_err(|e| internal_error(e.to_string()))?;

    Ok(snapshot_to_dto(&snapshot))
}

/// The allowed Gemini model ids for the review dropdown (Zach's locked
/// decision #1). The view renders these as a `<select>`; `run_review` validates
/// the chosen id against this same list.
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 503 if the real path is
/// selected but `GEMINI_MODEL_IDS` resolution yields nothing (a misconfigured
/// prod — never silently empty).
#[allow(clippy::unused_async)]
#[server]
pub async fn list_models() -> Result<Vec<String>, ServerFnError> {
    use crate::services::gate::require_authed_user;

    let _user = require_authed_user().await?;
    let models = allowed_model_ids();
    if models.is_empty() {
        return Err(ServerFnError::ServerError {
            message: "no Gemini model ids configured (GEMINI_MODEL_IDS)".to_owned(),
            code: 503,
            details: None,
        });
    }
    Ok(models)
}

/// Run the AI portfolio review for the authenticated user (`§Phase 6`).
///
/// Gate FIRST (`BUDGET-AUTH-GATE-1`) → extract state → validate the chosen
/// `model_id` against the allow-list (`ORCH-TRAINING-CUTOFF-1`) → build the
/// review use-case for that model (mock or real `GeminiAdvisor`) →
/// `generate_portfolio_review` → `review_run_to_dto`. Returns `Ok` even for
/// `MalformedOutput` / `EmptyPortfolio` (the terminal state communicates the
/// outcome); only a retryable transport failure surfaces as an error.
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 422-shaped 400 if `model_id`
/// is not in the allow-list; 503 if the real path is selected but its
/// prerequisites (`KEY_VAULT_URL` / `GEMINI_MODEL_IDS`) are missing; 500 on a
/// transport / persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn run_review(model_id: String) -> Result<ReviewResultDto, ServerFnError> {
    use crate::server_state::PortfolioState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = PortfolioState::extract().await?;

    // Validate the chosen model id against the allow-list (locked decision #1).
    // A model id NOT in the list is a typed 400 (a tampered/stale client choice),
    // never silently honored.
    let allowed = allowed_model_ids();
    if !allowed.iter().any(|m| m == &model_id) {
        return Err(ServerFnError::ServerError {
            message: format!("model id '{model_id}' is not an allowed model"),
            code: 400,
            details: None,
        });
    }

    // Build the review use-case for the chosen model (mock or real GeminiAdvisor).
    // The real path 503s here if its prerequisites are missing — a misconfigured
    // prod can NEVER silently reach the mock (mirrors PLAID_MODE=mock).
    let service =
        state
            .build_review_service(&model_id)
            .map_err(|e| ServerFnError::ServerError {
                message: e,
                code: 503,
                details: None,
            })?;

    let run = service
        .generate_portfolio_review(user.id(), chrono::Utc::now())
        .await
        .map_err(|e| internal_error(e.to_string()))?;

    Ok(review_run_to_dto(&run))
}

// ---------------------------------------------------------------------------
// Tests — pure conversion helpers (ORCH-NEW-PATH-TESTS-1)
// ---------------------------------------------------------------------------
#[cfg(all(test, feature = "server"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use budget_domain::enums::AccountType;
    use budget_domain::ids::{PositionId, UserId};
    use budget_domain::money::Money;
    use chrono::Utc;

    #[test]
    fn account_type_round_trips_through_string() {
        for t in [
            AccountType::Checking,
            AccountType::Credit,
            AccountType::Savings,
            AccountType::Investment,
            AccountType::Other,
        ] {
            let s = account_type_to_string(t);
            assert_eq!(account_type_from_string(s), Ok(t));
        }
    }

    #[test]
    fn account_type_from_unknown_string_errors() {
        assert!(account_type_from_string("brokerage").is_err());
    }

    #[test]
    fn add_position_dto_to_domain_parses_shares_and_cost_basis() {
        let input = AddPositionDto {
            ticker: "aapl".to_owned(),
            account_label: "Fidelity Roth".to_owned(),
            account_type: "investment".to_owned(),
            shares: "10.5".to_owned(),
            cost_basis: Some("1500.00".to_owned()),
        };
        let id = PositionId::generate();
        let user_id = UserId::generate();
        let now = Utc::now();
        let pos = add_position_dto_to_domain(id, user_id, &input, now).unwrap();

        // ticker is normalised to uppercase by Ticker::try_new.
        assert_eq!(pos.ticker.as_str(), "AAPL");
        assert_eq!(pos.account_type, AccountType::Investment);
        assert_eq!(pos.shares, rust_decimal::Decimal::new(105, 1));
        assert_eq!(pos.cost_basis, Some(Money::from_minor(150_000)));
        assert_eq!(pos.id, id);
        assert_eq!(pos.user_id, user_id);
    }

    #[test]
    fn add_position_dto_blank_cost_basis_is_none() {
        let input = AddPositionDto {
            ticker: "MSFT".to_owned(),
            account_label: "Brokerage".to_owned(),
            account_type: "investment".to_owned(),
            shares: "3".to_owned(),
            cost_basis: Some("   ".to_owned()),
        };
        let pos = add_position_dto_to_domain(
            PositionId::generate(),
            UserId::generate(),
            &input,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(pos.cost_basis, None);
    }

    #[test]
    fn add_position_dto_rejects_bad_ticker_and_shares() {
        let bad_ticker = AddPositionDto {
            ticker: "AA1".to_owned(),
            account_label: "x".to_owned(),
            account_type: "investment".to_owned(),
            shares: "1".to_owned(),
            cost_basis: None,
        };
        assert!(
            add_position_dto_to_domain(
                PositionId::generate(),
                UserId::generate(),
                &bad_ticker,
                Utc::now()
            )
            .is_err()
        );

        let bad_shares = AddPositionDto {
            ticker: "AAPL".to_owned(),
            account_label: "x".to_owned(),
            account_type: "investment".to_owned(),
            shares: "not-a-number".to_owned(),
            cost_basis: None,
        };
        assert!(
            add_position_dto_to_domain(
                PositionId::generate(),
                UserId::generate(),
                &bad_shares,
                Utc::now()
            )
            .is_err()
        );
    }

    #[test]
    fn position_to_dto_renders_fields() {
        let input = AddPositionDto {
            ticker: "NVDA".to_owned(),
            account_label: "Taxable".to_owned(),
            account_type: "investment".to_owned(),
            shares: "5".to_owned(),
            cost_basis: None,
        };
        let pos = add_position_dto_to_domain(
            PositionId::generate(),
            UserId::generate(),
            &input,
            Utc::now(),
        )
        .unwrap();
        let dto = position_to_dto(&pos);
        assert_eq!(dto.ticker, "NVDA");
        assert_eq!(dto.account_type, "investment");
        assert_eq!(dto.shares, "5");
        assert_eq!(dto.cost_basis, None);
        assert_eq!(dto.id, pos.id.value());
    }

    #[test]
    fn cash_balance_dto_round_trips_through_domain() {
        let dto = CashBalanceDto {
            id: None,
            account_label: "Emergency Fund".to_owned(),
            balance: "5000.00".to_owned(),
            reserved: true,
        };
        let domain = cash_balance_dto_to_domain(&dto).unwrap();
        assert_eq!(domain.balance, Money::from_minor(500_000));
        assert!(domain.reserved);

        let back = cash_balance_to_dto(None, &domain);
        assert_eq!(back.account_label, "Emergency Fund");
        assert!(back.reserved);
        // The decimal string is normalised (5000.00 -> "5000.00").
        assert_eq!(back.balance, "5000.00");
    }

    #[test]
    fn cash_balance_dto_rejects_bad_balance() {
        let dto = CashBalanceDto {
            id: None,
            account_label: "x".to_owned(),
            balance: "not-money".to_owned(),
            reserved: false,
        };
        assert!(cash_balance_dto_to_domain(&dto).is_err());
    }

    // -- Phase 3: snapshot DTO mapping ---------------------------------------

    use budget_domain::portfolio::{
        NetWorth, PortfolioSnapshot, Position, PriceProvenance, PriceQuote, PricedPosition,
        ShareProvenance, Ticker,
    };

    fn priced(ticker: &str, shares: i64, mv_cents: Option<i64>, fresh: bool) -> PricedPosition {
        let now = Utc::now();
        let position = Position {
            id: PositionId::generate(),
            user_id: UserId::generate(),
            ticker: Ticker::try_new(ticker).unwrap(),
            account_label: "Brokerage".to_owned(),
            account_type: AccountType::Investment,
            shares: rust_decimal::Decimal::new(shares, 0),
            cost_basis: None,
            drip_enabled: false,
            baseline_as_of: now,
            created_at: now,
            updated_at: now,
        };
        let quote = mv_cents.map(|_| PriceQuote {
            price: Money::from_minor(18_000),
            provenance: PriceProvenance::Market {
                source: "finnhub".to_owned(),
            },
            // Fresh = now; stale = 48h ago (>= STALE_AFTER_HOURS).
            as_of: if fresh {
                now
            } else {
                now - chrono::Duration::hours(48)
            },
        });
        PricedPosition {
            position,
            quote,
            market_value: mv_cents.map(Money::from_minor),
            share_provenance: ShareProvenance::Uploaded,
        }
    }

    fn snapshot(positions: Vec<PricedPosition>, total_invested_cents: i64) -> PortfolioSnapshot {
        PortfolioSnapshot {
            user_id: UserId::generate(),
            positions,
            cash_balances: vec![],
            buffer_total: Money::ZERO,
            net_worth: NetWorth {
                total_cash: Money::ZERO,
                total_positions: Money::from_minor(total_invested_cents),
                liabilities: Money::ZERO,
                total: Money::from_minor(total_invested_cents),
            },
            total_invested: Money::from_minor(total_invested_cents),
            captured_at: Utc::now(),
        }
    }

    #[test]
    fn snapshot_dto_renders_pct_to_one_dp_and_skips_unresolved() {
        // AAPL $1800 of $4300 -> 41.86% -> "41.9"; NVDA unresolved -> None pct.
        let snap = snapshot(
            vec![
                priced("AAPL", 10, Some(180_000), true),
                priced("NVDA", 5, None, true),
            ],
            430_000,
        );
        let dto = snapshot_to_dto(&snap);
        let aapl = dto.positions.iter().find(|p| p.ticker == "AAPL").unwrap();
        assert_eq!(aapl.pct_of_portfolio, Some("41.9".to_owned()));
        assert_eq!(aapl.market_value, Some("1800.00".to_owned()));
        assert_eq!(aapl.provenance, Some("market:finnhub".to_owned()));
        assert!(!aapl.is_stale);

        let nvda = dto.positions.iter().find(|p| p.ticker == "NVDA").unwrap();
        assert_eq!(nvda.pct_of_portfolio, None);
        assert_eq!(nvda.market_value, None);
        assert!(nvda.is_stale, "an unresolved quote is stale");
        assert_eq!(nvda.price, None);
    }

    #[test]
    fn snapshot_dto_flags_old_quote_as_stale() {
        let snap = snapshot(vec![priced("AAPL", 10, Some(180_000), false)], 180_000);
        let dto = snapshot_to_dto(&snap);
        assert!(dto.positions[0].is_stale, "a 48h-old quote is stale");
    }

    #[test]
    fn snapshot_dto_pct_is_none_when_total_invested_zero() {
        // Resolved market_value but total_invested 0 -> no division.
        let snap = snapshot(vec![priced("AAPL", 10, Some(0), true)], 0);
        let dto = snapshot_to_dto(&snap);
        assert_eq!(dto.positions[0].pct_of_portfolio, None);
    }

    // -- Phase 7.4: provenance badge (RUST-DIOXUS-10 boundary) ----------------

    #[test]
    fn uploaded_provenance_has_no_estimated_badge() {
        let (estimated, badge) = share_provenance_to_badge(&ShareProvenance::Uploaded);
        assert!(!estimated);
        assert_eq!(badge, None);
    }

    #[test]
    fn drip_estimated_provenance_renders_a_human_badge() {
        // The raw discriminant + events_applied/baseline never cross; only the
        // human string does (RUST-DIOXUS-10).
        let badge = share_provenance_to_badge(&ShareProvenance::DripEstimated {
            events_applied: 2,
            baseline_as_of: chrono::DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        });
        assert!(badge.0, "shares_estimated is true");
        let label = badge.1.unwrap();
        assert!(label.contains("estimated"));
        assert!(label.contains('2'));
        assert!(label.contains("dividends"), "plural for >1 event");
        assert!(label.contains("2026-01-15"));
    }

    #[test]
    fn drip_estimated_singular_dividend_label() {
        let badge = share_provenance_to_badge(&ShareProvenance::DripEstimated {
            events_applied: 1,
            baseline_as_of: Utc::now(),
        });
        let label = badge.1.unwrap();
        assert!(label.contains("1 dividend "), "singular for one event");
    }

    #[test]
    fn snapshot_dto_carries_estimated_badge_through_to_priced_row() {
        // An end-to-end check that the DRIP label survives snapshot -> DTO.
        let mut pp = priced("AAPL", 10, Some(180_000), true);
        pp.share_provenance = ShareProvenance::DripEstimated {
            events_applied: 3,
            baseline_as_of: Utc::now(),
        };
        let snap = snapshot(vec![pp], 180_000);
        let dto = snapshot_to_dto(&snap);
        assert!(dto.positions[0].shares_estimated);
        assert!(
            dto.positions[0]
                .estimated_badge
                .as_ref()
                .unwrap()
                .contains("3 dividends")
        );
    }

    // -- Phase 6: review_run_to_dto + AI config (ORCH-NEW-PATH-TESTS-1) -------

    use budget_domain::ids::ReviewRunId;
    use budget_domain::portfolio::{
        Claim, ClaimSubject, Confidence, Recommendation, ReviewRun, ReviewTerminalState,
        UnverifiedReason, ValidationOutcome,
    };

    /// A canonical snapshot: AAPL $1800 in an INVESTMENT account (tax-relevant)
    /// + a $5000 reserved buffer. Drives the tax_note + reconcile assertions.
    fn review_snapshot() -> PortfolioSnapshot {
        let mut snap = snapshot(vec![priced("AAPL", 10, Some(180_000), true)], 180_000);
        snap.buffer_total = Money::from_minor(500_000);
        snap.net_worth.total = Money::from_minor(680_000);
        snap
    }

    fn rec(subject: ClaimSubject, cited_cents: i64) -> Recommendation {
        Recommendation {
            title: "Do a thing".to_owned(),
            rationale: "Because reasons".to_owned(),
            confidence: Confidence::Medium,
            claims: vec![Claim {
                subject,
                cited_value: Money::from_minor(cited_cents),
                cited_percentage: None,
            }],
        }
    }

    fn run_with(
        recommendations: Vec<Recommendation>,
        outcomes: Vec<(usize, ValidationOutcome)>,
        terminal_state: ReviewTerminalState,
        raw_output: &str,
    ) -> ReviewRun {
        ReviewRun {
            id: ReviewRunId::generate(),
            user_id: UserId::generate(),
            model_id: "gemini-2.5-pro".to_owned(),
            prompt_hash: "hash".to_owned(),
            raw_output: raw_output.to_owned(),
            snapshot: review_snapshot(),
            recommendations,
            outcomes,
            terminal_state,
            prompt_tokens: Some(1),
            completion_tokens: Some(2),
            finish_reason: Some("STOP".to_owned()),
            latency_ms: 5,
            occurred_at: Utc::now(),
        }
    }

    #[test]
    fn review_run_to_dto_renders_a_verified_completed_run_with_tax_note() {
        // AAPL position claim matching ground truth -> Verified; the holding is in
        // an Investment account -> tax_note present.
        let run = run_with(
            vec![rec(
                ClaimSubject::Position {
                    ticker: Ticker::try_new("AAPL").unwrap(),
                },
                180_000,
            )],
            vec![(0, ValidationOutcome::Verified)],
            ReviewTerminalState::Completed,
            "{\"recommendations\":[]}",
        );
        let dto = review_run_to_dto(&run);
        assert_eq!(dto.terminal_state, ReviewTerminalStateDto::Completed);
        assert_eq!(dto.recommendations.len(), 1);
        let card = &dto.recommendations[0];
        assert_eq!(card.confidence, "medium");
        assert_eq!(card.badge, ValidationBadgeDto::Verified);
        assert_eq!(card.claims[0].subject, "AAPL market value");
        assert_eq!(card.claims[0].badge, ValidationBadgeDto::Verified);
        assert!(
            card.tax_note.is_some(),
            "an Investment-account claim gets a tax note"
        );
        // The standing disclaimer is always present.
        assert_eq!(dto.disclaimer, PORTFOLIO_REVIEW_DISCLAIMER);
    }

    #[test]
    fn review_run_to_dto_renders_an_unverified_claim_with_a_human_reason() {
        // A wrong AAPL figure -> ValueMismatch; the badge carries a HUMAN string,
        // never the raw code (RUST-DIOXUS-10).
        let run = run_with(
            vec![rec(
                ClaimSubject::Position {
                    ticker: Ticker::try_new("AAPL").unwrap(),
                },
                5_000_000, // $50,000 hallucination vs $1800 truth
            )],
            vec![(
                0,
                ValidationOutcome::Unverified(UnverifiedReason::ValueMismatch {
                    cited: Money::from_minor(5_000_000),
                    ground_truth: Money::from_minor(180_000),
                }),
            )],
            ReviewTerminalState::NoVerifiableInsights,
            "{}",
        );
        let dto = review_run_to_dto(&run);
        let card = &dto.recommendations[0];
        match &card.badge {
            ValidationBadgeDto::Unverified { reason } => {
                assert!(reason.contains("50000") || reason.contains("1800"));
            }
            ValidationBadgeDto::Verified => panic!("expected an unverified badge"),
        }
    }

    #[test]
    fn review_run_to_dto_renders_a_malformed_run_with_no_recommendations() {
        let run = run_with(
            vec![],
            vec![],
            ReviewTerminalState::MalformedOutput,
            "not json at all",
        );
        let dto = review_run_to_dto(&run);
        assert_eq!(dto.terminal_state, ReviewTerminalStateDto::MalformedOutput);
        assert!(dto.recommendations.is_empty());
        assert_eq!(dto.disclaimer, PORTFOLIO_REVIEW_DISCLAIMER);
    }

    #[test]
    fn tax_note_absent_for_a_buffer_only_recommendation() {
        // A Buffer claim touches no investment holding -> no tax note.
        let run = run_with(
            vec![rec(ClaimSubject::Buffer, 500_000)],
            vec![(0, ValidationOutcome::Verified)],
            ReviewTerminalState::Completed,
            "{}",
        );
        let dto = review_run_to_dto(&run);
        assert_eq!(dto.recommendations[0].tax_note, None);
    }

    #[test]
    fn subject_to_display_renders_cost_basis_gain() {
        let s = subject_to_display(&ClaimSubject::CostBasisGain {
            ticker: Ticker::try_new("AAPL").unwrap(),
        });
        assert_eq!(s, "AAPL unrealized gain");
    }

    #[test]
    fn parse_model_ids_seeded_default() {
        // The seeded-default string parses to the two locked default models
        // (decision #1). Tests the pure parser (env mutation is forbidden here —
        // `unsafe-code` is denied — so allowed_model_ids()'s env read is exercised
        // only at runtime).
        assert_eq!(
            parse_model_ids(DEFAULT_GEMINI_MODEL_IDS),
            vec!["gemini-2.5-pro", "gemini-2.5-flash"]
        );
    }

    #[test]
    fn parse_model_ids_trims_dedups_and_preserves_order() {
        assert_eq!(parse_model_ids(" a , b ,a, ,c "), vec!["a", "b", "c"]);
        assert!(parse_model_ids("   ").is_empty());
        assert!(parse_model_ids("").is_empty());
    }
}
