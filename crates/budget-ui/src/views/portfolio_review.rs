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
    AccountUploadDto, CashBalanceDto, PositionDto, PricedPositionDto, RecommendationDto,
    ReviewResultDto, ReviewTerminalStateDto, UploadedPositionDto, ValidationBadgeDto,
    list_cash_balances, list_models, list_positions, portfolio_snapshot, run_review,
    set_drip_enabled, upload_account_positions,
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
                    Some(Ok(rows)) => rsx! { PositionsByAccount { rows: rows.clone(), positions } },
                }}
            }

            // Lean per-account upload affordance (§2.7/§6): paste an account's
            // holdings and reconcile that account only.
            section { class: "portfolio-upload",
                h2 { "Upload an account" }
                UploadAccountForm { positions }
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
// Phase 7.4: holdings GROUPED BY ACCOUNT, each row a DRIP toggle (§2.7)
// ---------------------------------------------------------------------------

/// Group the positions by `account_label`, preserving stable order (accounts in
/// first-seen order; rows within an account in load order). Pure so it is
/// unit-tested directly (`ORCH-NEW-PATH-TESTS-1`).
#[must_use]
fn group_by_account(rows: &[PositionDto]) -> Vec<(String, Vec<PositionDto>)> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<PositionDto>> =
        std::collections::HashMap::new();
    for p in rows {
        if !groups.contains_key(&p.account_label) {
            order.push(p.account_label.clone());
        }
        groups
            .entry(p.account_label.clone())
            .or_default()
            .push(p.clone());
    }
    order
        .into_iter()
        .map(|label| {
            let rows = groups.remove(&label).unwrap_or_default();
            (label, rows)
        })
        .collect()
}

/// The holdings, GROUPED BY ACCOUNT. Each row carries a DRIP checkbox that
/// toggles `drip_enabled` inline (`set_drip_enabled`) and persists; on success the
/// `positions` resource is restarted so the table reflects the new state
/// (`RUST-DIOXUS-6`/`RUST-DIOXUS-8`: keyed rows).
#[component]
fn PositionsByAccount(
    rows: Vec<PositionDto>,
    positions: Resource<Result<Vec<PositionDto>, ServerFnError>>,
) -> Element {
    if rows.is_empty() {
        return rsx! { p { class: "empty-text", "No holdings entered yet." } };
    }
    let grouped = group_by_account(&rows);
    rsx! {
        div { class: "positions-by-account",
            for (account , account_rows) in grouped {
                section { key: "{account}", class: "account-group",
                    h3 { class: "account-group-header", "{account}" }
                    table { class: "positions-table",
                        thead {
                            tr {
                                th { "Ticker" }
                                th { "Type" }
                                th { "Shares" }
                                th { "Cost basis" }
                                th { "DRIP" }
                            }
                        }
                        tbody {
                            for p in account_rows {
                                PositionRow { key: "{p.id}", position: p.clone(), positions }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One holdings row with an inline DRIP checkbox (`§2.7`). Toggling calls the
/// gated `set_drip_enabled` server fn; on success the parent `positions` resource
/// is restarted so the persisted state re-renders. The checkbox is disabled while
/// the toggle is in flight (debounce).
#[component]
fn PositionRow(
    position: PositionDto,
    positions: Resource<Result<Vec<PositionDto>, ServerFnError>>,
) -> Element {
    let mut saving = use_signal(|| false);
    let id = position.id;
    let on_toggle = move |evt: Event<FormData>| {
        if *saving.read() {
            return;
        }
        let enabled = evt.checked();
        saving.set(true);
        spawn(async move {
            let _ = set_drip_enabled(id, enabled).await;
            // Re-read so the table reflects the persisted state (survives uploads).
            positions.restart();
            saving.set(false);
        });
    };
    rsx! {
        tr {
            td { "{position.ticker}" }
            td { "{position.account_type}" }
            td { "{position.shares}" }
            td { {position.cost_basis.clone().unwrap_or_else(|| "—".to_owned())} }
            td {
                input {
                    r#type: "checkbox",
                    checked: position.drip_enabled,
                    disabled: *saving.read(),
                    onchange: on_toggle,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 7.4: the lean per-account upload affordance (§2.7/§6)
// ---------------------------------------------------------------------------

/// Parse pasted CSV-ish upload text into `UploadedPositionDto`s. Each non-blank
/// line is `TICKER, SHARES[, COST_BASIS]`. Pure so it is unit-tested directly
/// (`ORCH-NEW-PATH-TESTS-1`); validation of ticker/shares happens server-side via
/// the domain newtypes (`RUST-DIOXUS-13`).
#[must_use]
fn parse_upload_text(text: &str) -> Vec<UploadedPositionDto> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let mut cols = line.split(',').map(str::trim);
            let ticker = cols.next().filter(|t| !t.is_empty())?.to_owned();
            let shares = cols.next().filter(|s| !s.is_empty())?.to_owned();
            let cost_basis = cols.next().map(ToOwned::to_owned).filter(|s| !s.is_empty());
            Some(UploadedPositionDto {
                ticker,
                shares,
                cost_basis,
            })
        })
        .collect()
}

/// A lean upload form: pick an account label + type, paste the holdings, submit a
/// PER-ACCOUNT upsert (`upload_account_positions`). On success the `positions`
/// resource is restarted so the grouped table reflects the reconcile.
#[component]
fn UploadAccountForm(positions: Resource<Result<Vec<PositionDto>, ServerFnError>>) -> Element {
    let mut account_label = use_signal(String::new);
    let mut account_type = use_signal(|| "investment".to_owned());
    let mut paste = use_signal(String::new);
    let mut submitting = use_signal(|| false);
    let mut message = use_signal(|| None::<Result<String, String>>);

    let on_submit = move |_| {
        if *submitting.read() {
            return;
        }
        let label = account_label.read().trim().to_owned();
        if label.is_empty() {
            message.set(Some(Err("Enter an account label first.".to_owned())));
            return;
        }
        let parsed = parse_upload_text(&paste.read());
        let payload = AccountUploadDto {
            account_label: label,
            account_type: account_type.read().clone(),
            positions: parsed,
        };
        submitting.set(true);
        spawn(async move {
            match upload_account_positions(payload).await {
                Ok(rows) => {
                    message.set(Some(Ok(format!(
                        "Uploaded — {} holding(s) now in the portfolio.",
                        rows.len()
                    ))));
                    positions.restart();
                }
                Err(e) => message.set(Some(Err(e.to_string()))),
            }
            submitting.set(false);
        });
    };

    rsx! {
        div { class: "upload-account-form",
            p { class: "upload-help",
                "Paste one holding per line as " code { "TICKER, SHARES" }
                " (optional third column for cost basis). The upload replaces the chosen account's holdings only; DRIP toggles on surviving positions are kept."
            }
            div { class: "upload-controls",
                label { r#for: "upload-account-label", "Account: " }
                input {
                    id: "upload-account-label",
                    r#type: "text",
                    placeholder: "Brokerage",
                    value: "{account_label}",
                    disabled: *submitting.read(),
                    oninput: move |e| account_label.set(e.value()),
                }
                label { r#for: "upload-account-type", "Type: " }
                select {
                    id: "upload-account-type",
                    value: "{account_type}",
                    disabled: *submitting.read(),
                    onchange: move |e| account_type.set(e.value()),
                    option { value: "investment", "investment" }
                    option { value: "savings", "savings" }
                    option { value: "checking", "checking" }
                    option { value: "other", "other" }
                }
            }
            textarea {
                class: "upload-textarea",
                rows: "5",
                placeholder: "AAPL, 10\nMSFT, 5, 1500.00",
                value: "{paste}",
                disabled: *submitting.read(),
                oninput: move |e| paste.set(e.value()),
            }
            button {
                class: "upload-submit-button",
                disabled: *submitting.read(),
                onclick: on_submit,
                {if *submitting.read() { "Uploading…" } else { "Upload account" }}
            }
            {match &*message.read() {
                None => rsx! {},
                Some(Ok(ok)) => rsx! { p { class: "upload-ok", "{ok}" } },
                Some(Err(err)) => rsx! { p { class: "error-text", "Upload failed: {err}" } },
            }}
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

    // -- Phase 7.4: account grouping + upload parsing -------------------------

    fn position_dto(ticker: &str, account: &str, drip: bool) -> PositionDto {
        PositionDto {
            id: uuid::Uuid::new_v4(),
            ticker: ticker.to_owned(),
            account_label: account.to_owned(),
            account_type: "investment".to_owned(),
            shares: "10".to_owned(),
            cost_basis: None,
            drip_enabled: drip,
        }
    }

    #[test]
    fn group_by_account_groups_and_preserves_first_seen_order() {
        let rows = vec![
            position_dto("AAPL", "Brokerage", false),
            position_dto("VTI", "Roth", true),
            position_dto("MSFT", "Brokerage", false),
        ];
        let grouped = group_by_account(&rows);
        assert_eq!(grouped.len(), 2);
        // Brokerage seen first, then Roth.
        assert_eq!(grouped[0].0, "Brokerage");
        assert_eq!(grouped[0].1.len(), 2, "AAPL + MSFT under Brokerage");
        assert_eq!(grouped[1].0, "Roth");
        assert_eq!(grouped[1].1.len(), 1);
        assert!(grouped[1].1[0].drip_enabled, "Roth VTI drip flag carried");
    }

    #[test]
    fn parse_upload_text_reads_ticker_shares_and_optional_cost_basis() {
        let text = "AAPL, 10\nMSFT, 5, 1500.00\n\n  NVDA ,3 \n";
        let parsed = parse_upload_text(text);
        assert_eq!(parsed.len(), 3, "blank lines skipped");
        assert_eq!(parsed[0].ticker, "AAPL");
        assert_eq!(parsed[0].shares, "10");
        assert_eq!(parsed[0].cost_basis, None);
        assert_eq!(parsed[1].cost_basis, Some("1500.00".to_owned()));
        // Whitespace trimmed per column.
        assert_eq!(parsed[2].ticker, "NVDA");
        assert_eq!(parsed[2].shares, "3");
    }

    #[test]
    fn parse_upload_text_skips_lines_missing_shares() {
        // A ticker with no shares column is not a valid holding line.
        let parsed = parse_upload_text("AAPL\nMSFT, 5");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].ticker, "MSFT");
    }
}
