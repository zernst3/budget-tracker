//! The Portfolio Insights view (`docs/AI_FEATURE_DESIGN.md §Phase 2`/`§Phase 6`,
//! `RUST-DIOXUS-1`).
//!
//! Phase 2 rendered a read-only view of the user's manually-entered holdings and
//! cash balances. Phase 6 adds the AI review surface:
//!
//! - a model-id **dropdown** sourced from the gated [`list_models`] server fn
//!   (the configurable allow-list, locked decision #1);
//! - a read-only **chorale [`Table`]** of the priced positions from the grounding
//!   snapshot (`RUST-DIOXUS-6`: data via `use_resource`);
//! - a **"Run Review"** button, disabled while a run is in flight (debounce
//!   against double-submit);
//! - **insight cards** per recommendation, each with its validation badge, the
//!   per-claim badges, the model's confidence indicator, and any deterministic
//!   tax note; and
//! - the standing **not-financial-advice disclaimer** ([`PORTFOLIO_REVIEW_DISCLAIMER`]),
//!   always rendered on a result.
//!
//! Every validation badge / reason / subject string is rendered SERVER-SIDE
//! (`RUST-DIOXUS-10`); this view only displays the human strings the DTO carries.
//!
//! ## Money representation (`BUDGET-MONEY-1`)
//!
//! The DTOs carry exact decimal STRINGS; this view renders them verbatim and does
//! NO float math. The only display-side arithmetic is the reserved-buffer
//! subtotal, summed exactly via `rust_decimal::Decimal` (never `f64`).
//!
//! [`PORTFOLIO_REVIEW_DISCLAIMER`]: crate::services::portfolio_review::PORTFOLIO_REVIEW_DISCLAIMER

use dioxus::prelude::*;
use rust_decimal::Decimal;
use std::str::FromStr;

use chorale_core::{Alignment, CellValue, ColumnDef, ColumnId, RowId, TableState};
use chorale_dioxus::{Table, UseTableHandle, use_table};

use crate::Route;
use crate::components::NavBar;
use crate::services::logout;
use crate::services::portfolio_review::{
    CashBalanceDto, PositionDto, PricedPositionDto, RecommendationDto, ReviewResultDto,
    ReviewTerminalStateDto, ValidationBadgeDto, list_cash_balances, list_models, list_positions,
    portfolio_snapshot, run_review,
};

/// The Portfolio Insights page (Phase 2 holdings/cash + Phase 6 AI review).
#[component]
#[must_use]
pub fn PortfolioReviewView() -> Element {
    let positions = use_resource(move || async move { list_positions().await });
    let balances = use_resource(move || async move { list_cash_balances().await });
    let snapshot = use_resource(move || async move { portfolio_snapshot().await });
    let models = use_resource(move || async move { list_models().await });

    let page_nav = use_navigator();
    let on_signout = move |()| {
        spawn(async move {
            let _ = logout().await;
            page_nav.push(Route::Login {});
        });
    };

    rsx! {
        NavBar { on_signout }
        main { class: "portfolio-review",
            h1 { "Portfolio Insights" }

            section { class: "portfolio-positions",
                h2 { "Holdings" }
                {match &*positions.read() {
                    None => rsx! { p { class: "loading-text", "Loading holdings…" } },
                    Some(Err(_)) => rsx! { p { class: "error-text", "Could not load holdings." } },
                    Some(Ok(rows)) => rsx! { PositionsTable { rows: rows.clone() } },
                }}
            }

            section { class: "portfolio-cash",
                h2 { "Cash balances" }
                {match &*balances.read() {
                    None => rsx! { p { class: "loading-text", "Loading cash balances…" } },
                    Some(Err(_)) => rsx! { p { class: "error-text", "Could not load cash balances." } },
                    Some(Ok(rows)) => rsx! { CashBalancesPanel { rows: rows.clone() } },
                }}
            }

            section { class: "portfolio-snapshot",
                h2 { "Priced snapshot" }
                {match &*snapshot.read() {
                    None => rsx! { p { class: "loading-text", "Loading market snapshot…" } },
                    Some(Err(_)) => rsx! { p { class: "error-text", "Could not load the market snapshot." } },
                    Some(Ok(snap)) => rsx! { PricedPositionsTable { rows: snap.positions.clone() } },
                }}
            }

            ReviewPanel { models: models.read().clone() }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 6: the AI review panel (model dropdown + Run Review + insight cards)
// ---------------------------------------------------------------------------

/// The AI review panel: the model dropdown, the Run Review button (disabled
/// while a run is in flight), and the rendered result.
#[component]
fn ReviewPanel(models: Option<Result<Vec<String>, ServerFnError>>) -> Element {
    // The chosen model id (the dropdown selection). Seeded to the first allowed
    // model once the list resolves.
    let mut selected_model = use_signal(String::new);
    // Whether a run is in flight (debounces the button against double-submit).
    let mut running = use_signal(|| false);
    // The latest review result (or an error string for display).
    let mut result = use_signal(|| None::<Result<ReviewResultDto, String>>);

    // Seed the selection to the first allowed model when the list first resolves
    // and nothing is selected yet.
    let model_list: Vec<String> = match &models {
        Some(Ok(list)) => list.clone(),
        _ => Vec::new(),
    };
    if selected_model.read().is_empty()
        && let Some(first) = model_list.first()
    {
        selected_model.set(first.clone());
    }

    let on_run = move |_| {
        if *running.read() {
            return; // already running — debounce.
        }
        let model = selected_model.read().clone();
        if model.is_empty() {
            return;
        }
        running.set(true);
        spawn(async move {
            let outcome = run_review(model).await.map_err(|e| e.to_string());
            result.set(Some(outcome));
            running.set(false);
        });
    };

    rsx! {
        section { class: "portfolio-ai-review",
            h2 { "AI review" }

            {match &models {
                None => rsx! { p { class: "loading-text", "Loading available models…" } },
                Some(Err(_)) => rsx! {
                    p { class: "error-text",
                        "AI review is not available (no models configured)." }
                },
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "error-text", "No review models are configured." }
                },
                Some(Ok(list)) => {
                    let list = list.clone();
                    rsx! {
                        div { class: "ai-review-controls",
                            label { r#for: "model-select", "Model: " }
                            select {
                                id: "model-select",
                                value: "{selected_model}",
                                disabled: *running.read(),
                                onchange: move |e| selected_model.set(e.value()),
                                for m in list {
                                    option { key: "{m}", value: "{m}", "{m}" }
                                }
                            }
                            button {
                                class: "run-review-button",
                                disabled: *running.read() || selected_model.read().is_empty(),
                                onclick: on_run,
                                {if *running.read() { "Running…" } else { "Run Review" }}
                            }
                        }
                    }
                }
            }}

            {match &*result.read() {
                None => rsx! {},
                Some(Err(msg)) => rsx! {
                    p { class: "error-text", "Review failed: {msg}" }
                },
                Some(Ok(review)) => rsx! { ReviewResult { review: review.clone() } },
            }}
        }
    }
}

/// Render a completed review result: the terminal-state banner, the insight
/// cards, and the always-present standing disclaimer.
#[component]
fn ReviewResult(review: ReviewResultDto) -> Element {
    rsx! {
        div { class: "review-result",
            TerminalStateBanner { state: review.terminal_state.clone() }

            if review.recommendations.is_empty() {
                p { class: "empty-text",
                    "No verifiable recommendations were produced for this run." }
            } else {
                div { class: "insight-cards",
                    for (i, rec) in review.recommendations.iter().enumerate() {
                        InsightCard { key: "{i}", rec: rec.clone() }
                    }
                }
            }

            // The standing not-financial-advice disclaimer (N3) — always rendered.
            p { class: "review-disclaimer", "{review.disclaimer}" }
        }
    }
}

/// A human banner for the run's terminal state.
#[component]
fn TerminalStateBanner(state: ReviewTerminalStateDto) -> Element {
    let (class, text) = match state {
        ReviewTerminalStateDto::Completed => ("banner-ok", "Review complete."),
        ReviewTerminalStateDto::NoVerifiableInsights => (
            "banner-warn",
            "The model produced no verifiable insights for your portfolio.",
        ),
        ReviewTerminalStateDto::EmptyPortfolio => (
            "banner-info",
            "Your portfolio is empty — add holdings or cash balances to get a review.",
        ),
        ReviewTerminalStateDto::MalformedOutput => (
            "banner-error",
            "The model returned output that could not be parsed; nothing was shown.",
        ),
    };
    rsx! {
        p { class: "review-banner {class}", "{text}" }
    }
}

/// One recommendation card: title, confidence, aggregate badge, per-claim badges,
/// and any deterministic tax note.
#[component]
fn InsightCard(rec: RecommendationDto) -> Element {
    rsx! {
        article { class: "insight-card",
            header { class: "insight-card-header",
                h3 { "{rec.title}" }
                ValidationBadge { badge: rec.badge.clone() }
                span { class: "confidence-indicator confidence-{rec.confidence}",
                    "confidence: {rec.confidence}" }
            }
            p { class: "insight-rationale", "{rec.rationale}" }

            if !rec.claims.is_empty() {
                ul { class: "insight-claims",
                    for (i, claim) in rec.claims.iter().enumerate() {
                        li { key: "{i}", class: "insight-claim",
                            span { class: "claim-subject", "{claim.subject}" }
                            span { class: "claim-value", " — {claim.cited_value}" }
                            if let Some(pct) = &claim.cited_percentage {
                                span { class: "claim-pct", " ({pct})" }
                            }
                            ValidationBadge { badge: claim.badge.clone() }
                        }
                    }
                }
            }

            if let Some(note) = &rec.tax_note {
                p { class: "insight-tax-note", "{note}" }
            }
        }
    }
}

/// A verified / unverified validation badge (human reason rendered server-side).
#[component]
fn ValidationBadge(badge: ValidationBadgeDto) -> Element {
    match badge {
        ValidationBadgeDto::Verified => rsx! {
            span { class: "badge badge-verified", "verified" }
        },
        ValidationBadgeDto::Unverified { reason } => rsx! {
            span { class: "badge badge-unverified", title: "{reason}", "unverified: {reason}" }
        },
    }
}

// ---------------------------------------------------------------------------
// Phase 6: read-only chorale Table of the priced snapshot positions
// ---------------------------------------------------------------------------

/// A flattened priced-position row for the chorale table.
#[derive(Debug, Clone, PartialEq)]
struct PricedRow {
    ticker: String,
    account: String,
    shares: String,
    price: String,
    market_value: String,
    pct: String,
    stale: bool,
    /// The server-rendered "estimated since last upload" badge text, or empty for
    /// a confirmed (`Uploaded`) baseline (`BUDGET-AI-1`, §2.5/§8). Display-only;
    /// the raw provenance never crossed the wire (`RUST-DIOXUS-10`).
    estimated_badge: String,
}

impl PricedRow {
    fn from_dto(p: &PricedPositionDto) -> Self {
        let dash = || "—".to_owned();
        Self {
            ticker: p.ticker.clone(),
            account: p.account_label.clone(),
            shares: p.shares.clone(),
            price: p.price.clone().unwrap_or_else(dash),
            market_value: p.market_value.clone().unwrap_or_else(dash),
            pct: p
                .pct_of_portfolio
                .clone()
                .map_or_else(dash, |v| format!("{v}%")),
            stale: p.is_stale,
            // The server already rendered the human badge (or None for a confirmed
            // baseline); render it verbatim or empty.
            estimated_badge: p.estimated_badge.clone().unwrap_or_default(),
        }
    }
}

/// The chorale column set for the priced-positions table (read-only; sortable).
fn priced_columns() -> Vec<ColumnDef<PricedRow>> {
    vec![
        ColumnDef::new(ColumnId("ticker"), "Ticker", |r: &PricedRow| {
            CellValue::Text(r.ticker.clone())
        })
        .sortable()
        .initial_width(100.0),
        ColumnDef::new(ColumnId("account"), "Account", |r: &PricedRow| {
            CellValue::Text(r.account.clone())
        })
        .initial_width(160.0),
        ColumnDef::new(ColumnId("shares"), "Shares", |r: &PricedRow| {
            CellValue::Text(r.shares.clone())
        })
        .alignment(Alignment::Right)
        .initial_width(100.0),
        ColumnDef::new(ColumnId("price"), "Price", |r: &PricedRow| {
            CellValue::Text(r.price.clone())
        })
        .alignment(Alignment::Right)
        .initial_width(110.0),
        ColumnDef::new(ColumnId("market_value"), "Market value", |r: &PricedRow| {
            CellValue::Text(r.market_value.clone())
        })
        .alignment(Alignment::Right)
        .sortable()
        .initial_width(140.0),
        ColumnDef::new(ColumnId("pct"), "% of portfolio", |r: &PricedRow| {
            CellValue::Text(r.pct.clone())
        })
        .alignment(Alignment::Right)
        .initial_width(120.0),
        ColumnDef::new(ColumnId("stale"), "Stale?", |r: &PricedRow| {
            CellValue::Text(if r.stale {
                "stale".to_owned()
            } else {
                String::new()
            })
        })
        .initial_width(90.0),
        // The DRIP "estimated since last upload" badge (§2.5/§8). Empty for a
        // confirmed baseline; the server rendered the human text (RUST-DIOXUS-10).
        ColumnDef::new(ColumnId("estimated"), "Share count", |r: &PricedRow| {
            CellValue::Text(if r.estimated_badge.is_empty() {
                "confirmed".to_owned()
            } else {
                r.estimated_badge.clone()
            })
        })
        .initial_width(280.0),
    ]
}

/// Read-only chorale [`Table`] over the snapshot's priced positions.
#[component]
fn PricedPositionsTable(rows: Vec<PricedPositionDto>) -> Element {
    if rows.is_empty() {
        return rsx! { p { class: "empty-text", "No priced holdings yet." } };
    }
    // chorale rows are `(RowId, TRow)`; RowId identity is generated once here so
    // it is stable across renders (the rows are captured into `use_table`'s
    // run-once init closure).
    let priced: Vec<(RowId, PricedRow)> = rows
        .iter()
        .map(|p| (RowId::new(), PricedRow::from_dto(p)))
        .collect();
    let n = priced.len();
    let table: UseTableHandle<PricedRow> = use_table(move || {
        let mut s = TableState::new(priced.clone(), priced_columns());
        s.page_size = n.max(1);
        s
    });
    rsx! {
        Table { handle: table, sort_enabled: true }
    }
}

// ---------------------------------------------------------------------------
// Phase 2: read-only holdings table + cash panel (unchanged)
// ---------------------------------------------------------------------------

/// The read-only holdings table.
#[component]
fn PositionsTable(rows: Vec<PositionDto>) -> Element {
    if rows.is_empty() {
        return rsx! { p { class: "empty-text", "No holdings entered yet." } };
    }
    rsx! {
        table { class: "positions-table",
            thead {
                tr {
                    th { "Ticker" }
                    th { "Account" }
                    th { "Type" }
                    th { "Shares" }
                    th { "Cost basis" }
                }
            }
            tbody {
                for p in rows {
                    tr { key: "{p.id}",
                        td { "{p.ticker}" }
                        td { "{p.account_label}" }
                        td { "{p.account_type}" }
                        td { "{p.shares}" }
                        td { {p.cost_basis.clone().unwrap_or_else(|| "—".to_owned())} }
                    }
                }
            }
        }
    }
}

/// The read-only cash-balances list + the reserved-buffer subtotal.
#[component]
fn CashBalancesPanel(rows: Vec<CashBalanceDto>) -> Element {
    let buffer_total = reserved_buffer_total(&rows);
    rsx! {
        if rows.is_empty() {
            p { class: "empty-text", "No cash balances entered yet." }
        } else {
            table { class: "cash-balances-table",
                thead {
                    tr {
                        th { "Account" }
                        th { "Balance" }
                        th { "Reserved" }
                    }
                }
                tbody {
                    for b in &rows {
                        tr { key: "{b.account_label}",
                            td { "{b.account_label}" }
                            td { "{b.balance}" }
                            td { {if b.reserved { "yes" } else { "no" }} }
                        }
                    }
                }
            }
            p { class: "buffer-total",
                strong { "Reserved buffer total: " }
                "{buffer_total}"
            }
        }
    }
}

/// Sum the reserved balances exactly (`rust_decimal`, no `f64` —
/// `BUDGET-MONEY-1`). A balance whose string fails to parse is skipped (it should
/// not happen: the server rendered it from an exact `Money`).
#[must_use]
fn reserved_buffer_total(rows: &[CashBalanceDto]) -> String {
    let total: Decimal = rows
        .iter()
        .filter(|b| b.reserved)
        .filter_map(|b| Decimal::from_str(b.balance.trim()).ok())
        .sum();
    total.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balance(label: &str, amount: &str, reserved: bool) -> CashBalanceDto {
        CashBalanceDto {
            id: None,
            account_label: label.to_owned(),
            balance: amount.to_owned(),
            reserved,
        }
    }

    #[test]
    fn buffer_total_sums_only_reserved_balances() {
        let rows = vec![
            balance("Emergency", "5000.00", true),
            balance("Checking", "1200.50", false),
            balance("Reserve 2", "300.25", true),
        ];
        // Only the two reserved sum: 5000.00 + 300.25 = 5300.25.
        assert_eq!(reserved_buffer_total(&rows), "5300.25");
    }

    #[test]
    fn buffer_total_is_zero_with_no_reserved() {
        let rows = vec![balance("Checking", "1200.50", false)];
        assert_eq!(reserved_buffer_total(&rows), "0");
    }

    fn priced_dto(ticker: &str, mv: Option<&str>, stale: bool) -> PricedPositionDto {
        PricedPositionDto {
            ticker: ticker.to_owned(),
            account_label: "Brokerage".to_owned(),
            account_type: "investment".to_owned(),
            shares: "10".to_owned(),
            price: mv.map(|_| "180.00".to_owned()),
            provenance: mv.map(|_| "market:finnhub".to_owned()),
            as_of: mv.map(|_| "2026-06-11T12:00:00Z".to_owned()),
            market_value: mv.map(ToOwned::to_owned),
            pct_of_portfolio: mv.map(|_| "41.9".to_owned()),
            is_stale: stale,
            shares_estimated: false,
            estimated_badge: None,
        }
    }

    #[test]
    fn priced_row_renders_dashes_for_unresolved_and_pct_suffix() {
        let resolved = PricedRow::from_dto(&priced_dto("AAPL", Some("1800.00"), false));
        assert_eq!(resolved.market_value, "1800.00");
        assert_eq!(resolved.pct, "41.9%");
        assert!(!resolved.stale);
        // A confirmed baseline carries no estimated badge.
        assert_eq!(resolved.estimated_badge, "");

        let unresolved = PricedRow::from_dto(&priced_dto("NVDA", None, true));
        assert_eq!(unresolved.price, "—");
        assert_eq!(unresolved.market_value, "—");
        assert_eq!(unresolved.pct, "—");
        assert!(unresolved.stale);
    }

    #[test]
    fn priced_row_carries_estimated_badge_verbatim() {
        let mut dto = priced_dto("AAPL", Some("1800.00"), false);
        dto.shares_estimated = true;
        dto.estimated_badge =
            Some("estimated · 2 dividends reinvested since last upload".to_owned());
        let row = PricedRow::from_dto(&dto);
        assert_eq!(
            row.estimated_badge,
            "estimated · 2 dividends reinvested since last upload"
        );
    }
}
