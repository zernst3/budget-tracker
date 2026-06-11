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
//! - **Phase 2 (this phase):** the six position/balance CRUD functions are live.
//! - **Phase 3:** [`portfolio_snapshot`] goes live (market-data fan-out).
//! - **Phase 6:** [`run_review`] gets its body (the real review pipeline).
//!   Until then it is a `501` stub so a caller gets a clear "not implemented yet"
//!   rather than a panic.
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

/// Render a [`PricedPosition`](budget_domain::portfolio::PricedPosition) to a
/// [`PricedPositionDto`].
///
/// `pct_of_portfolio` is `market_value / total_invested` as a PERCENT (ratio ×
/// 100) rounded to 1 dp; `None` when the position is unresolved or
/// `total_invested` is zero. `is_stale` is true when the quote is absent or older
/// than [`STALE_AFTER_HOURS`].
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

/// Assemble the grounding snapshot for the authenticated user (`§Phase 3`):
/// load positions + balances concurrently, fan out market quotes via
/// `try_join_all` (`ARCH-PARALLEL-INDEPENDENT-1`), and assemble the snapshot.
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 500 on persistence/market
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

    let snapshot = budget_app_services::assemble_snapshot(
        user.id(),
        positions,
        balances,
        state.market.as_ref(),
        chrono::Utc::now(),
    )
    .await
    .map_err(|e| internal_error(e.to_string()))?;

    Ok(snapshot_to_dto(&snapshot))
}

/// Run the AI portfolio review for the authenticated user (Phase 6).
///
/// # Errors
/// `ServerFnError` (401) without a valid session; 501 until Phase 6 wires the
/// review pipeline.
#[allow(clippy::unused_async)]
#[server]
pub async fn run_review() -> Result<ReviewResultDto, ServerFnError> {
    // Phase 6 wires the body (require_authed_user -> PortfolioState::extract ->
    // GeneratePortfolioReview -> review_run_to_dto). Until then it is a clear 501.
    Err(ServerFnError::ServerError {
        message: "run_review is not implemented until Phase 6".to_owned(),
        code: 501,
        details: None,
    })
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
        NetWorth, PortfolioSnapshot, Position, PriceProvenance, PriceQuote, PricedPosition, Ticker,
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
}
