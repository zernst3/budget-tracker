//! The budget / transactions view (`SPEC §7`) — the future main screen.
//!
//! Phase B0 scaffold: a minimal chorale [`Table`] over placeholder rows, proving
//! the `chorale-dioxus` git dependency compiles and renders, plus a call to the
//! [`health`](crate::services::health) server function through [`use_resource`]
//! (`RUST-DIOXUS-6`) proving the server-function plumbing works end-to-end. The
//! real grouped / aggregated / in-cell-editing transactions grid (`SPEC §7`,
//! chorale v0.2.0) lands in step 10, gated behind the `AuthedUser` server-side
//! (`BUDGET-AUTH-GATE-1`) — this scaffold is unauthenticated and shows no real
//! financial data.

use chorale_core::{Alignment, CellValue, ColumnDef, ColumnId, RenderKind, RowId, TableState};
use chorale_dioxus::{Table, use_table};
use dioxus::prelude::*;

use crate::services::health;

/// A placeholder transactions row for the scaffold. The real domain row
/// (mapped from a server-function DTO) replaces this in step 10; money stays a
/// `rust_decimal::Decimal`-backed value end-to-end (`BUDGET-MONEY-1`), never a
/// float.
#[derive(Clone, PartialEq)]
struct PlaceholderTxn {
    date: String,
    description: String,
    category: String,
}

fn placeholder_rows() -> Vec<(RowId, PlaceholderTxn)> {
    [
        ("2026-07-01", "Opening balance", "Other"),
        ("2026-07-02", "Groceries", "Food"),
        ("2026-07-03", "Coffee", "Food"),
    ]
    .into_iter()
    .map(|(d, desc, cat)| {
        (
            RowId::new(),
            PlaceholderTxn {
                date: d.to_owned(),
                description: desc.to_owned(),
                category: cat.to_owned(),
            },
        )
    })
    .collect()
}

fn placeholder_columns() -> Vec<ColumnDef<PlaceholderTxn>> {
    vec![
        ColumnDef::new(ColumnId("date"), "Date", |t: &PlaceholderTxn| {
            CellValue::Text(t.date.clone())
        })
        .sortable()
        .initial_width(120.0),
        ColumnDef::new(
            ColumnId("description"),
            "Description",
            |t: &PlaceholderTxn| CellValue::Text(t.description.clone()),
        )
        .sortable()
        .initial_width(260.0),
        ColumnDef::new(ColumnId("category"), "Category", |t: &PlaceholderTxn| {
            CellValue::Text(t.category.clone())
        })
        .sortable()
        .initial_width(160.0)
        .alignment(Alignment::Left)
        .render_kind(RenderKind::Text),
    ]
}

/// Budget / transactions view (scaffold).
///
/// TODO(frontend-phase, step 10): replace the placeholder rows with real
/// transactions from an `AuthedUser`-gated server function, add the
/// grouped/aggregated/in-cell-editing chorale surface (`SPEC §7`), and style.
/// Manual QA required (checker-poor half).
#[component]
#[must_use]
pub fn BudgetView() -> Element {
    // RUST-DIOXUS-6: async data the UI displays goes through use_resource, not an
    // effect-plus-fetch. The scaffold probe confirms the server function round-trips.
    let health_probe = use_resource(health);
    let table = use_table(|| TableState::new(placeholder_rows(), placeholder_columns()));

    rsx! {
        main { style: "font-family: sans-serif; padding: 1rem; max-width: 900px; margin: 0 auto;",
            h1 { "Budget (scaffold)" }
            p {
                "Server health: "
                match &*health_probe.read() {
                    Some(Ok(status)) => rsx! { span { "{status}" } },
                    Some(Err(_)) => rsx! { span { "unavailable" } },
                    None => rsx! { span { "checking…" } },
                }
            }
            Table { handle: table, sort_enabled: true }
        }
    }
}
