//! Month budget view — Phase B4.
//!
//! Renders the current user's budget/spent/remaining summary for a given
//! `(year, month)` using the chorale table library. The screen is **read-only**
//! in B4: editing and transaction categorization are deferred to a later phase.
//!
//! ## Architecture
//!
//! - `ensure_month` fires on mount to trigger the lazy-init
//!   (`BUDGET-IDEMPOTENT-MONTH-INIT-1`). It is idempotent, so repeated renders
//!   (e.g. hot-reload) are safe.
//! - `get_month_view(year, month)` is the reactive data fetch (`RUST-DIOXUS-6`).
//!   Changing `(year, month)` via the navigation buttons re-runs the fetch.
//! - Money amounts arrive as `String` (exact decimal, `BUDGET-MONEY-1`): the
//!   server function serialises `rust_decimal::Decimal` transparently; the UI
//!   displays the pre-formatted currency strings and never interprets them as
//!   numbers.
//! - All columns are text-only. Sort is enabled; filter + selection are off
//!   for the read-only view.

use chorale_core::{Alignment, CellValue, ColumnDef, ColumnId, RowId, TableState};
use chorale_dioxus::{Table, use_table};
use dioxus::prelude::*;

use crate::Route;
use crate::services::{CategoryRowDto, MonthViewDto, ensure_month, get_month_view, logout};

// ---------------------------------------------------------------------------
// Row type for the chorale table
// ---------------------------------------------------------------------------

/// A single row in the month categories table. Holds pre-formatted display
/// strings for every column (money was already formatted server-side).
#[derive(Clone, PartialEq)]
struct CategoryTableRow {
    name: String,
    budgeted: String,
    spent: String,
    remaining: String,
    settle_state: String,
    is_rollover: bool,
}

impl CategoryTableRow {
    fn from_dto(dto: &CategoryRowDto) -> Self {
        Self {
            name: dto.name.clone(),
            budgeted: dto.budgeted.clone(),
            spent: dto.spent.clone(),
            remaining: dto.remaining.clone(),
            settle_state: dto.settle_state.clone(),
            is_rollover: dto.is_rollover,
        }
    }
}

// ---------------------------------------------------------------------------
// Column definitions
// ---------------------------------------------------------------------------

fn category_columns() -> Vec<ColumnDef<CategoryTableRow>> {
    vec![
        ColumnDef::new(ColumnId("name"), "Category", |r: &CategoryTableRow| {
            CellValue::Text(r.name.clone())
        })
        .sortable()
        .initial_width(220.0),
        ColumnDef::new(ColumnId("budgeted"), "Budgeted", |r: &CategoryTableRow| {
            CellValue::Text(r.budgeted.clone())
        })
        .sortable()
        .alignment(Alignment::Right)
        .initial_width(120.0),
        ColumnDef::new(ColumnId("spent"), "Spent", |r: &CategoryTableRow| {
            CellValue::Text(r.spent.clone())
        })
        .sortable()
        .alignment(Alignment::Right)
        .initial_width(120.0),
        ColumnDef::new(
            ColumnId("remaining"),
            "Remaining",
            |r: &CategoryTableRow| CellValue::Text(r.remaining.clone()),
        )
        .sortable()
        .alignment(Alignment::Right)
        .initial_width(120.0),
        ColumnDef::new(ColumnId("settle"), "Status", |r: &CategoryTableRow| {
            CellValue::Text(r.settle_state.clone())
        })
        .initial_width(100.0),
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

/// Current UTC year + month as a `(i32, i32)` pair. Used to initialise the
/// navigation signal on mount. Platform-dependent: on wasm32 we read the
/// browser `Date`; on server/native we use `chrono`.
///
/// NOTE: this is a UI display initialisation only — no money arithmetic is
/// based on this value. It is not a `BUDGET-MONEY-1` concern.
fn current_ym() -> (i32, i32) {
    // Both targets can use chrono because it is a pure dependency.
    use chrono::{Datelike, Utc};
    let now = Utc::now();
    // month() returns 1..=12; i32::try_from is always Ok for values in that range.
    let month = i32::try_from(now.month()).unwrap_or(1);
    (now.year(), month)
}

// ---------------------------------------------------------------------------
// The B4 screen
// ---------------------------------------------------------------------------

/// The month budget view — the primary authenticated screen.
///
/// Displays a chorale table of categories with budget / spent / remaining for
/// the selected month, plus the rolling "Other" balance and previous / next
/// navigation. Read-only for B4; editing is deferred.
///
/// ### Manual QA notes
///
/// 1. On first load, the page fires `ensure_month` (lazy-init). If the DB is
///    reachable and the month record is created, the table fills with rows.
/// 2. Navigating back to a prior month that was never initialised shows the
///    "Month not yet initialised" banner with a zero rolling Other balance.
/// 3. The rolling Other balance is the `is_rollover_bucket` category's spent
///    value (the rollover transaction + any in-month Other entries). A negative
///    value means the prior month carried a deficit into this month.
/// 4. Money values are exact decimal strings ("$1,234.56", "-$42.00") — verify
///    no floats sneak in by inspecting the network response JSON.
/// 5. TODO(visual-polish): add Tailwind colour coding for negative remaining
///    (red) vs positive (green). Currently plain text.
/// 6. TODO(visual-polish): add loading skeleton / spinner while the fetch is
///    in-flight. Currently shows a plain "Loading…" text node.
/// 7. TODO(B5): the passkey registration button + scaffold health probe have
///    been removed from the B4 view. They survive in git history on the B3
///    scaffold commit.
#[component]
#[must_use]
pub fn BudgetView() -> Element {
    // Navigation state: which (year, month) is the user looking at?
    let (init_year, init_month) = current_ym();
    let mut nav_year = use_signal(|| init_year);
    let mut nav_month = use_signal(|| init_month);

    // Fire ensure_month once on mount. The dependency is `(nav_year, nav_month)`
    // so navigating to an un-initialised month also triggers a lazy-init attempt
    // for that month. Idempotent: no harm if the month already exists.
    let ensure = use_resource(move || async move { ensure_month().await });

    // Fetch the view whenever (year, month) changes.
    let view_data = use_resource(move || {
        let year = nav_year();
        let month = nav_month();
        async move { get_month_view(year, month).await }
    });

    // Build the chorale table rows from the DTO categories.
    // We re-derive rows every render from view_data; the chorale hook owns
    // the stable reactive handle.
    let table_rows = use_memo(move || match &*view_data.read() {
        Some(Ok(dto)) => dto
            .categories
            .iter()
            .map(|cat| (RowId::new(), CategoryTableRow::from_dto(cat)))
            .collect::<Vec<_>>(),
        _ => vec![],
    });

    let table = use_table(move || TableState::new(table_rows(), category_columns()));

    let page_nav = use_navigator();

    // Logout handler — same pattern as B3.
    let on_logout = move |_| {
        spawn(async move {
            let _ = logout().await;
            page_nav.push(Route::Login {});
        });
    };

    rsx! {
        main {
            style: "font-family: sans-serif; padding: 1rem; max-width: 960px; margin: 0 auto;",

            // -- Header --
            div {
                style: "display: flex; align-items: center; justify-content: space-between; margin-bottom: 1rem;",
                h1 { style: "margin: 0;", "Budget" }
                button { onclick: on_logout, style: "cursor: pointer;", "Sign out" }
            }

            // -- Month navigation + label --
            {
                let year = nav_year();
                let month = nav_month();
                let (py, pm) = prev_month(year, month);
                let (ny, nm) = next_month(year, month);
                rsx! {
                    div {
                        style: "display: flex; align-items: center; gap: 0.75rem; margin-bottom: 1rem;",
                        button {
                            style: "cursor: pointer; padding: 0.25rem 0.75rem;",
                            onclick: move |_| {
                                nav_year.set(py);
                                nav_month.set(pm);
                            },
                            "< Prev"
                        }
                        span {
                            style: "font-size: 1.2rem; font-weight: 600; min-width: 140px; text-align: center;",
                            {
                                // Derive the label from the current nav state directly
                                // so it updates immediately on click (before the fetch
                                // completes).
                                let label = month_label_ui(year, month);
                                rsx! { "{label}" }
                            }
                        }
                        button {
                            style: "cursor: pointer; padding: 0.25rem 0.75rem;",
                            onclick: move |_| {
                                nav_year.set(ny);
                                nav_month.set(nm);
                            },
                            "Next >"
                        }
                    }
                }
            }

            // -- ensure_month status (debug; silent on success) --
            if let Some(Err(e)) = &*ensure.read() {
                p {
                    style: "color: #c00; font-size: 0.85rem;",
                    role: "alert",
                    "Warning: could not initialise month ({e})"
                }
            }

            // -- Main content: loading / error / data --
            {
                match &*view_data.read() {
                    None => rsx! {
                        p { style: "color: #555;", "Loading…" }
                    },
                    Some(Err(e)) => rsx! {
                        p {
                            style: "color: #c00;",
                            role: "alert",
                            "Error loading budget view: {e}"
                        }
                        Link { to: Route::Login {}, "Return to login" }
                    },
                    Some(Ok(dto)) => {
                        rsx! {
                            MonthContent { dto: dto.clone(), table_handle: table }
                        }
                    },
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sub-component: renders the loaded month data
// ---------------------------------------------------------------------------

#[component]
fn MonthContent(
    dto: MonthViewDto,
    table_handle: chorale_dioxus::UseTableHandle<CategoryTableRow>,
) -> Element {
    rsx! {
        // -- Month not yet initialised banner --
        if !dto.month_exists {
            div {
                style: "background: #fff3cd; border: 1px solid #ffc107; border-radius: 4px; padding: 0.75rem 1rem; margin-bottom: 1rem;",
                role: "status",
                p { style: "margin: 0; font-weight: 600;", "Month not yet initialised" }
                p {
                    style: "margin: 0.25rem 0 0;",
                    "Navigate to the current month to trigger month set-up."
                }
            }
        }

        // -- Rolling Other balance (prominent) --
        div {
            style: "background: #f0f4f8; border-radius: 6px; padding: 0.75rem 1rem; margin-bottom: 1rem; display: flex; align-items: baseline; gap: 0.5rem;",
            span { style: "font-size: 0.9rem; color: #555;", "Rolling Other balance:" }
            span {
                style: "font-size: 1.5rem; font-weight: 700; font-variant-numeric: tabular-nums;",
                "{dto.rolling_other}"
            }
            // TODO(visual-polish): colour the balance green (positive) / red
            // (negative) based on the first char of the string.
        }

        // -- Month status badge --
        if dto.month_exists {
            div {
                style: "margin-bottom: 0.75rem;",
                span {
                    style: if dto.is_open {
                        "background: #d1fae5; color: #065f46; border-radius: 999px; padding: 0.2rem 0.65rem; font-size: 0.8rem; font-weight: 600;"
                    } else {
                        "background: #e5e7eb; color: #374151; border-radius: 999px; padding: 0.2rem 0.65rem; font-size: 0.8rem; font-weight: 600;"
                    },
                    if dto.is_open { "Open" } else { "Closed" }
                }
            }
        }

        // -- Zero-state (month exists but no categories) --
        if dto.month_exists && dto.categories.is_empty() {
            p { style: "color: #555;", "No categories found for this month." }
        }

        // -- Categories table (chorale) --
        if !dto.categories.is_empty() {
            div {
                style: "margin-top: 0.5rem;",
                Table { handle: table_handle, sort_enabled: true }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UI-only label helper (duplicate of the server-side helper for client use)
// ---------------------------------------------------------------------------

/// UI-side month label ("July 2026"). Duplicates the server-side helper so the
/// navigation label updates immediately on button click without waiting for the
/// fetch response.
fn month_label_ui(year: i32, month: i32) -> String {
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
