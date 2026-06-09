//! The MONTH LEDGER screen (`SPEC §7`) — FRONTEND-1.
//!
//! The main authenticated view. Replaces the retired B4 read-only category-row
//! month view. Structure (`SPEC §7`, DECIDED 2026-06-09):
//!
//!   - **Month selector** (prev / current / next) at the top.
//!   - A **collapsible envelope-summary header** (`get_envelope_summary`): each
//!     category's budgeted / spent / remaining + the rolling-Other balance
//!     (`SPEC §4.3`), currency-formatted from the `rust_decimal`-backed DTO.
//!   - A **chorale parent `Table` with ONE ROW PER DAY** (`get_month_ledger`):
//!     date + the day's total expense. `detail_renderer` (a chorale `Callback`
//!     per the integration cheat-sheet) expands a day into a child `Table`
//!     (`inline: true`) of that day's transactions, **grouped by category**
//!     (`s.grouping = [ColumnId("category")]`) with `AggregatorKind::Sum` on the
//!     amount column (the per-category daily subtotal).
//!   - **Inline editing** on the child transaction rows, exactly two fields
//!     (`SPEC §7`): `category` (an `EditorKind::Select` dropdown over the user's
//!     category names) and `comment` (`EditorKind::Text`). Everything else
//!     (amount, date, description) omits `.editor()` and stays read-only.
//!     `on_commit_edit` resolves the edited field by `edit.column_id` and calls
//!     the `update_transaction_inline` BACKEND-4 server function.
//!
//! ## Money representation (`BUDGET-MONEY-1`)
//!
//! Every monetary field on the DTOs is [`budget_domain::Money`] (Decimal-backed,
//! exact). The ONLY float in this module is the `Decimal -> f64` conversion at
//! the chorale `CellValue::Float` accessor boundary (for display + chorale's
//! `Sum` aggregation); the `Money`/`Decimal` value is the source of truth and is
//! never mutated through the float. Currency formatting for the envelope header
//! is done from the exact `Decimal` (`fmt_currency`), not from an f64.
//!
//! ## chorale API (cited to `docs/planning/chorale-integration-cheatsheet.md`)
//!
//! - master/detail: `detail_renderer: Callback<DayRow, Element>` (cheat-sheet §1,
//!   `Callback` NOT `EventHandler`); child `Table { inline: true }` (§1).
//! - grouping + aggregation: `s.grouping = vec![ColumnId("category")]` (§2) +
//!   `.aggregator(AggregatorKind::Sum)` on the amount column (§2).
//! - in-cell editing: `category` -> `.editor(EditorKind::Select { options })`,
//!   `comment` -> `.editor(EditorKind::Text)`; one `on_commit_edit:
//!   EventHandler<CommittedEdit<TxnRow>>` matching on `edit.column_id` +
//!   `handle.update_row` (§3).

use std::collections::HashMap;

use chorale_core::{
    AggregatorKind, Alignment, CellValue, ColumnDef, ColumnId, CommittedEdit, CurrencyCode,
    EditorKind, RenderKind, RowId, TableState,
};
use chorale_dioxus::{Table, UseTableHandle, use_table};
use dioxus::prelude::*;
use rust_decimal::prelude::ToPrimitive;

use crate::Route;
use crate::components::NavBar;
use crate::services::{
    DayLedgerDto, EnvelopeCategoryDto, EnvelopeSummaryDto, InlineEditRequest, LedgerTransactionDto,
    MonthLedgerDto, ensure_month, get_envelope_summary, get_month_ledger, logout,
    update_transaction_inline,
};

// ---------------------------------------------------------------------------
// Row types for the chorale tables
// ---------------------------------------------------------------------------

/// One PARENT row: a single day in the month ledger (`SPEC §7`).
///
/// `total_expense` is the day's signed expense total as an `f64` ONLY for the
/// chorale currency cell; the exact `Money` lives server-side and is not
/// recomputed here. `date_iso` (`YYYY-MM-DD`) drives the date cell.
#[derive(Clone, PartialEq)]
struct DayRow {
    date: chrono::NaiveDate,
    /// The day's transactions, carried so the detail renderer can build the
    /// child table without an extra fetch (the ledger fetch already returned
    /// them).
    transactions: Vec<LedgerTransactionDto>,
    /// Day expense total as f64 — chorale `CellValue::Float` boundary only.
    total_expense_f64: f64,
}

/// One CHILD row: a single transaction in a day's drill-down (`SPEC §7`).
///
/// `category` + `comment` are the two editable fields. `txn_id` is the stable
/// server id used by the inline-edit server function; it is NOT a chorale column
/// (read-only display columns + the two editors are the only visible cells).
#[derive(Clone, PartialEq)]
struct TxnRow {
    /// Server transaction id (the inline-edit target). Not displayed.
    txn_id: String,
    date: chrono::NaiveDate,
    description: String,
    /// Current category NAME (the `Select` editor's value is a name); empty
    /// string when uncategorized.
    category: String,
    comment: String,
    /// Amount as f64 — chorale `CellValue::Float` boundary only.
    amount_f64: f64,
}

// ---------------------------------------------------------------------------
// Decimal -> f64 at the chorale accessor boundary (BUDGET-MONEY-1)
// ---------------------------------------------------------------------------

/// Convert a `Money` to `f64` for a chorale `CellValue::Float`.
///
/// This is the SINGLE sanctioned `Decimal -> f64` site (`BUDGET-MONEY-1`): the
/// f64 exists only so chorale can render the currency cell and `Sum`-aggregate a
/// category subtotal. The exact `Money`/`Decimal` remains the source of truth on
/// the server; nothing in the budget math is ever read back from this f64.
fn money_to_cell_f64(m: budget_domain::Money) -> f64 {
    m.as_decimal().to_f64().unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Currency formatting for the envelope header (from exact Decimal, no float)
// ---------------------------------------------------------------------------

/// Format a `Money` as a US-currency string (e.g. `$1,234.56`, `-$42.00`).
///
/// Operates on the exact `Decimal` (round to cents, then group the integer part)
/// — NO float is involved, so this is `BUDGET-MONEY-1`-clean even though it is a
/// display helper. Negative values render with a leading `-` before the `$`.
pub(crate) fn fmt_currency(m: budget_domain::Money) -> String {
    let d = m.as_decimal().round_dp(2);
    let negative = d.is_sign_negative() && !d.is_zero();
    let abs = d.abs();
    // Decimal Display gives a fixed-point string; split into integer/fraction.
    let s = format!("{abs:.2}");
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s.as_str(), "00"));

    // Group the integer part with thousands separators.
    let bytes = int_part.as_bytes();
    let mut grouped = String::with_capacity(int_part.len() + int_part.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(*b as char);
    }

    if negative {
        format!("-${grouped}.{frac_part}")
    } else {
        format!("${grouped}.{frac_part}")
    }
}

// ---------------------------------------------------------------------------
// Parent (day) columns
// ---------------------------------------------------------------------------

fn day_columns() -> Vec<ColumnDef<DayRow>> {
    vec![
        ColumnDef::new(ColumnId("date"), "Date", |d: &DayRow| {
            CellValue::Date(d.date)
        })
        .render_kind(RenderKind::Date)
        .sortable()
        .initial_width(160.0),
        ColumnDef::new(ColumnId("total"), "Day total", |d: &DayRow| {
            CellValue::Float(d.total_expense_f64)
        })
        .alignment(Alignment::Right)
        .render_kind(RenderKind::Currency(CurrencyCode::USD))
        .sortable()
        .initial_width(160.0),
    ]
}

// ---------------------------------------------------------------------------
// Child (transaction) columns — category + comment editable, rest read-only
// ---------------------------------------------------------------------------

/// Build the child transaction columns. `category_options` is the set of
/// category NAMES the `Select` editor offers (cheat-sheet §3).
fn txn_columns(category_options: Vec<String>) -> Vec<ColumnDef<TxnRow>> {
    vec![
        // READ-ONLY: date.
        ColumnDef::new(ColumnId("date"), "Date", |t: &TxnRow| {
            CellValue::Date(t.date)
        })
        .render_kind(RenderKind::Date)
        .initial_width(120.0),
        // READ-ONLY: merchant / Plaid description.
        ColumnDef::new(ColumnId("description"), "Description", |t: &TxnRow| {
            CellValue::Text(t.description.clone())
        })
        .initial_width(240.0),
        // EDITABLE: category — native dropdown over the user's category names
        // (cheat-sheet §3, EditorKind::Select). Grouped on; carries no aggregator
        // (it is the group key).
        ColumnDef::new(ColumnId("category"), "Category", |t: &TxnRow| {
            CellValue::Text(t.category.clone())
        })
        .editor(EditorKind::Select {
            options: category_options,
        })
        .initial_width(160.0),
        // EDITABLE: free-text comment (cheat-sheet §3, EditorKind::Text).
        ColumnDef::new(ColumnId("comment"), "Comment", |t: &TxnRow| {
            CellValue::Text(t.comment.clone())
        })
        .editor(EditorKind::Text)
        .initial_width(220.0),
        // READ-ONLY + AGGREGATED: amount. No .editor() => read-only; the
        // Sum aggregator produces the per-category daily subtotal in each group
        // header (cheat-sheet §2).
        ColumnDef::new(ColumnId("amount"), "Amount", |t: &TxnRow| {
            CellValue::Float(t.amount_f64)
        })
        .alignment(Alignment::Right)
        .render_kind(RenderKind::Currency(CurrencyCode::USD))
        .aggregator(AggregatorKind::Sum)
        .initial_width(140.0),
    ]
}

// ---------------------------------------------------------------------------
// Navigation helpers
// ---------------------------------------------------------------------------

fn prev_month(year: i32, month: i32) -> (i32, i32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn next_month(year: i32, month: i32) -> (i32, i32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

/// Current year + month, for initialising the nav signal on mount. Display-only
/// (no money math), so not a `BUDGET-MONEY-1` concern.
fn current_ym() -> (i32, i32) {
    use chrono::{Datelike, Utc};
    let now = Utc::now();
    let month = i32::try_from(now.month()).unwrap_or(1);
    (now.year(), month)
}

/// Month label ("July 2026").
fn month_label(year: i32, month: i32) -> String {
    let name = match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "Unknown",
    };
    format!("{name} {year}")
}

/// Parse an ISO `YYYY-MM-DD` date for a chorale `CellValue::Date`. A malformed
/// value (should never happen — the server emits ISO) falls back to the epoch so
/// the row still renders rather than panicking.
fn parse_iso_date(s: &str) -> chrono::NaiveDate {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .unwrap_or_else(|_| chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Row builders
// ---------------------------------------------------------------------------

/// Build the parent day rows from the month-ledger DTO. `RowId`s are generated
/// fresh here (the data-fetch boundary) so they stay stable for the render's
/// lifetime (cheat-sheet §0: `RowId` identity must be stable across renders;
/// `use_table`'s init closure runs once).
fn build_day_rows(dto: &MonthLedgerDto) -> Vec<(RowId, DayRow)> {
    dto.days
        .iter()
        .map(|d: &DayLedgerDto| {
            (
                RowId::new(),
                DayRow {
                    date: parse_iso_date(&d.date),
                    transactions: d.transactions.clone(),
                    total_expense_f64: money_to_cell_f64(d.total_expense),
                },
            )
        })
        .collect()
}

/// Build the child transaction rows for one day, plus the `RowId -> txn_id`
/// side-map the commit handler uses to resolve the server id (chorale `RowId`
/// has no from-UUID constructor, so we map it alongside).
fn build_txn_rows(
    transactions: &[LedgerTransactionDto],
) -> (Vec<(RowId, TxnRow)>, HashMap<RowId, String>) {
    let mut rows = Vec::with_capacity(transactions.len());
    let mut id_map = HashMap::with_capacity(transactions.len());
    for t in transactions {
        let row_id = RowId::new();
        id_map.insert(row_id, t.id.clone());
        rows.push((
            row_id,
            TxnRow {
                txn_id: t.id.clone(),
                date: parse_iso_date(&t.date),
                description: t.description.clone(),
                category: t.category_name.clone().unwrap_or_default(),
                comment: t.comment.clone().unwrap_or_default(),
                amount_f64: money_to_cell_f64(t.amount),
            },
        ));
    }
    (rows, id_map)
}

// ---------------------------------------------------------------------------
// The ledger screen
// ---------------------------------------------------------------------------

/// The month ledger — the primary authenticated screen (`SPEC §7`).
///
/// ### Manual QA notes (what Zach should see + click)
///
/// 1. On load the page fires `ensure_month` (lazy-init, idempotent) then fetches
///    the envelope summary + the daily ledger for the current month.
/// 2. **Envelope header**: a collapsible panel ("Envelope summary" with a
///    show/hide toggle) listing every category with Budgeted / Spent / Remaining
///    and, prominently, the **Rolling Other balance** (`SPEC §4.3`). Values are
///    `$1,234.56` / `-$42.00` exact-decimal currency. Click the toggle to
///    collapse/expand. (TODO visual-polish: colour negative Remaining red.)
/// 3. **Ledger**: a chorale table, ONE ROW PER DAY (Date + Day total). Click a
///    day's chevron (left column) to expand it.
/// 4. **Drill-down**: the expanded day shows a child table of that day's
///    transactions GROUPED BY CATEGORY; each category group header carries the
///    per-category daily subtotal (the amount `Sum`).
/// 5. **Inline edit — category**: click a transaction's Category cell -> a
///    native dropdown of your category names appears; pick one -> it commits
///    immediately (calls `update_transaction_inline`); the next render reflects
///    the move. Re-fetch (prev/next + back) to see the envelope spent shift.
/// 6. **Inline edit — comment**: click a Comment cell -> a text input; type +
///    Enter (or blur) commits. Esc cancels.
/// 7. **Read-only**: Date / Description / Amount cells are NOT editable (no
///    editor opens on click).
/// 8. **Month nav**: `< Prev` / `Next >` move the month; the label updates
///    immediately, the tables refetch.
/// 9. Money: inspect the network JSON — every amount is an exact decimal string,
///    no float (`BUDGET-MONEY-1`). The only f64 is inside chorale's cell render.
#[component]
#[must_use]
pub fn LedgerView() -> Element {
    let (init_year, init_month) = current_ym();
    let mut nav_year = use_signal(|| init_year);
    let mut nav_month = use_signal(|| init_month);
    let mut summary_open = use_signal(|| true);

    // Lazy-init the month (idempotent). Re-runs when (year, month) changes so an
    // un-initialised month also triggers a set-up attempt.
    let ensure = use_resource(move || async move {
        let _ = (nav_year(), nav_month());
        ensure_month().await
    });

    // Reactive fetches: envelope summary + daily ledger for the selected month.
    let summary = use_resource(move || {
        let (y, m) = (nav_year(), nav_month());
        async move { get_envelope_summary(y, m).await }
    });
    let ledger = use_resource(move || {
        let (y, m) = (nav_year(), nav_month());
        async move { get_month_ledger(y, m).await }
    });

    let page_nav = use_navigator();
    let on_signout = move |()| {
        spawn(async move {
            let _ = logout().await;
            page_nav.push(Route::Login {});
        });
    };

    let (year, month) = (nav_year(), nav_month());
    let (py, pm) = prev_month(year, month);
    let (ny, nm) = next_month(year, month);
    let label = month_label(year, month);

    rsx! {
        div { class: "app-shell",
            // Shared nav bar (RUST-DIOXUS-14 — NavBar is the canonical primitive)
            NavBar { on_signout }

            main { class: "page-content",
                h1 { class: "page-title", "Ledger" }

                // -- Month navigation --
                div { class: "month-nav",
                    button {
                        class: "month-nav__btn",
                        onclick: move |_| { nav_year.set(py); nav_month.set(pm); },
                        "< Prev"
                    }
                    span { class: "month-nav__label", "{label}" }
                    button {
                        class: "month-nav__btn",
                        onclick: move |_| { nav_year.set(ny); nav_month.set(nm); },
                        "Next >"
                    }
                }

                // -- ensure_month error (silent on success) --
                if let Some(Err(e)) = &*ensure.read() {
                    p { class: "text-error", role: "alert",
                        "Warning: could not initialise month ({e})" }
                }

                // -- Envelope summary header (collapsible) --
                {
                    match &*summary.read() {
                        None => rsx! { p { class: "loading-text", "Loading summary…" } },
                        Some(Err(e)) => rsx! {
                            p { class: "text-error", role: "alert", "Error loading summary: {e}" }
                        },
                        Some(Ok(dto)) => rsx! {
                            EnvelopeSummary { dto: dto.clone(), open: summary_open, on_toggle: move |()| {
                                let cur = summary_open();
                                summary_open.set(!cur);
                            } }
                        },
                    }
                }

                // -- Daily ledger --
                {
                    match &*ledger.read() {
                        None => rsx! { p { class: "loading-text", "Loading ledger…" } },
                        Some(Err(e)) => rsx! {
                            p { class: "text-error", role: "alert", "Error loading ledger: {e}" }
                            Link { to: Route::Login {}, "Return to login" }
                        },
                        Some(Ok(dto)) => rsx! {
                            LedgerTable { dto: dto.clone(), summary: summary.read().clone() }
                        },
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope-summary sub-component
// ---------------------------------------------------------------------------

#[component]
fn EnvelopeSummary(
    dto: EnvelopeSummaryDto,
    open: Signal<bool>,
    on_toggle: EventHandler<()>,
) -> Element {
    let is_open = open();
    // The rolling-Other balance is itself a negative number when in deficit — make it
    // visually distinct using the amount CSS classes.
    let rolling_class = if dto.rolling_other.as_decimal().is_sign_negative()
        && !dto.rolling_other.as_decimal().is_zero()
    {
        "envelope-rolling-value amount--negative"
    } else {
        "envelope-rolling-value"
    };
    rsx! {
        div { class: "envelope-header",
            // -- Collapse toggle bar + rolling Other --
            div {
                class: "envelope-toggle",
                onclick: move |_| on_toggle.call(()),
                div { class: "envelope-toggle__left",
                    span { class: "envelope-toggle__title", "Envelope summary" }
                    span { class: "envelope-toggle__hint",
                        if is_open { "(click to collapse)" } else { "(click to expand)" }
                    }
                }
                div { class: "envelope-toggle__right",
                    span { class: "envelope-rolling-label", "Rolling Other:" }
                    span { class: "{rolling_class}", "{fmt_currency(dto.rolling_other)}" }
                }
            }

            // -- Per-category rows --
            if is_open {
                if !dto.month_exists {
                    div { class: "banner--warn",
                        "Month not yet initialised. Navigate to the current month to trigger set-up."
                    }
                } else if dto.categories.is_empty() {
                    div { style: "padding: 0.75rem 1rem;", class: "text-muted",
                        "No categories for this month."
                    }
                } else {
                    table { class: "envelope-table",
                        thead {
                            tr {
                                th { "Category" }
                                th { "Budgeted" }
                                th { "Spent" }
                                th { "Remaining" }
                            }
                        }
                        tbody {
                            for cat in dto.categories.iter() {
                                EnvelopeRow { cat: cat.clone() }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn EnvelopeRow(cat: EnvelopeCategoryDto) -> Element {
    // Remaining < 0 = over-budget (red); > 0 = under-budget (green).
    // TODO(visual-polish): add a filled progress bar within each row.
    let remaining_class =
        if cat.remaining.as_decimal().is_sign_negative() && !cat.remaining.as_decimal().is_zero() {
            "amount amount--negative"
        } else {
            "amount amount--positive"
        };
    let row_class = if cat.is_rollover {
        "envelope-row--rollover"
    } else {
        ""
    };
    rsx! {
        tr { class: "{row_class}",
            td { "{cat.name}" }
            td { class: "amount", "{fmt_currency(cat.budgeted)}" }
            td { class: "amount", "{fmt_currency(cat.spent)}" }
            td { class: "{remaining_class}", "{fmt_currency(cat.remaining)}" }
        }
    }
}

// ---------------------------------------------------------------------------
// Parent ledger table (day rows + detail_renderer mounting the child table)
// ---------------------------------------------------------------------------

#[component]
fn LedgerTable(
    dto: MonthLedgerDto,
    /// The summary fetch result, used to derive the category-name options for
    /// the inline Select editor (the child tables share one option list).
    summary: Option<Result<EnvelopeSummaryDto, ServerFnError>>,
) -> Element {
    // Category-name options for the inline Select editor (cheat-sheet §3) AND the
    // name -> id resolution the commit handler needs (the Select editor commits a
    // category NAME; the server fn `update_transaction_inline` takes a category
    // id). Both derive from the envelope summary; empty until it loads (the
    // editor then offers no options, harmless).
    let (category_options, name_to_id): (Vec<String>, HashMap<String, String>) = match &summary {
        Some(Ok(s)) => (
            s.categories.iter().map(|c| c.name.clone()).collect(),
            s.categories
                .iter()
                .map(|c| (c.name.clone(), c.id.clone()))
                .collect(),
        ),
        _ => (vec![], HashMap::new()),
    };

    // Stable parent rows + the category options, captured into the table init.
    let day_rows = build_day_rows(&dto);
    let n_days = day_rows.len();
    let opts_for_detail = category_options.clone();
    let name_to_id_for_detail = name_to_id.clone();

    let table: UseTableHandle<DayRow> = use_table(move || {
        let mut s = TableState::new(day_rows.clone(), day_columns());
        // Never paginate the day list (a month is <= 31 days).
        s.page_size = n_days.max(1);
        s
    });

    if dto.days.is_empty() {
        return rsx! {
            p { class: "text-muted", style: "margin-top: 0.5rem;",
                "No transactions yet for this month." }
        };
    }

    rsx! {
        Table {
            handle: table,
            sort_enabled: true,
            // Master/detail (cheat-sheet §1): Callback receiving the parent
            // DayRow by value; mounts the child transaction table.
            detail_renderer: Callback::new(move |day: DayRow| {
                let opts = opts_for_detail.clone();
                let name_to_id = name_to_id_for_detail.clone();
                rsx! {
                    DayDetail {
                        transactions: day.transactions.clone(),
                        category_options: opts,
                        name_to_id,
                    }
                }
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Child detail table (one day's transactions, grouped by category)
// ---------------------------------------------------------------------------

#[component]
fn DayDetail(
    transactions: Vec<LedgerTransactionDto>,
    category_options: Vec<String>,
    /// Category NAME -> id map, resolving the `Select` editor's committed name
    /// to the category id the inline-edit server function expects.
    name_to_id: HashMap<String, String>,
) -> Element {
    let (txn_rows, id_map) = build_txn_rows(&transactions);
    let n = txn_rows.len();
    let opts = category_options.clone();

    let table: UseTableHandle<TxnRow> = use_table(move || {
        let mut s = TableState::new(txn_rows.clone(), txn_columns(opts.clone()));
        s.page_size = n.max(1);
        // Grouping ON: group the child table by category (cheat-sheet §2). The
        // amount column's Sum aggregator yields the per-category subtotal.
        s.grouping = vec![ColumnId("category")];
        s
    });

    // RowId -> txn_id resolution for the commit handler (chorale RowId has no
    // from-UUID constructor, so we keep the side-map built alongside the rows).
    let id_map = use_signal(|| id_map);
    // Category NAME -> id, for resolving the Select editor's committed name.
    let name_to_id = use_signal(|| name_to_id);

    let on_commit: EventHandler<CommittedEdit<TxnRow>> =
        EventHandler::new(move |edit: CommittedEdit<TxnRow>| {
            // Resolve the current row + the server id.
            let current = table
                .signal()
                .read()
                .rows
                .iter()
                .find(|(id, _)| *id == edit.row_id)
                .map(|(_, r)| r.clone());
            let Some(mut row) = current else {
                return;
            };
            let txn_id = id_map.read().get(&edit.row_id).cloned().unwrap_or_default();

            // Apply the edit locally (optimistic) per column, then persist.
            // (cheat-sheet §3: match edit.column_id, mutate, update_row.)
            let mut req = InlineEditRequest {
                transaction_id: txn_id,
                category_id: None,
                comment: None,
            };
            match edit.column_id {
                ColumnId("category") => {
                    // The Select editor commits a category NAME; resolve it to the
                    // category id the server fn expects. An unknown name (should
                    // not happen — options come from the same summary) is dropped
                    // rather than sent as a non-id.
                    let Some(cat_id) = name_to_id.read().get(&edit.value).cloned() else {
                        return;
                    };
                    row.category.clone_from(&edit.value);
                    req.category_id = Some(cat_id);
                }
                ColumnId("comment") => {
                    row.comment.clone_from(&edit.value);
                    req.comment = Some(edit.value.clone());
                }
                _ => return,
            }
            table.update_row(edit.row_id, row);

            spawn(async move {
                let _ = update_transaction_inline(req).await;
            });
        });

    rsx! {
        div { class: "day-detail",
            Table {
                handle: table,
                inline: true,
                on_commit_edit: on_commit,
            }
        }
    }
}
