//! The Portfolio Insights view — Phase 2 read-only positions + cash/buffer
//! display (`docs/AI_FEATURE_DESIGN.md §Phase 2`, `RUST-DIOXUS-1`).
//!
//! Phase 2 renders a read-only view of the user's manually-entered holdings and
//! cash balances, plus the reserved-buffer subtotal. It uses `use_resource`
//! (`RUST-DIOXUS-6`: for displayed data) over the gated [`list_positions`] /
//! [`list_cash_balances`] server functions.
//!
//! The market-priced snapshot (per-position market value, total invested, full
//! net worth) and the "Run Review" affordance + insight cards arrive in Phase 3 /
//! Phase 6 respectively; this phase deliberately shows only what is computable
//! from the stored data (positions, balances, and the buffer subtotal = the sum
//! of reserved balances). A plain Dioxus table is used here (read-only, no
//! per-cell editing); the Phase-6 review screen adopts the chorale `Table`.
//!
//! ## Money representation (`BUDGET-MONEY-1`)
//!
//! The DTOs carry exact decimal STRINGS; this view renders them verbatim and does
//! NO float math. The only display-side arithmetic is the buffer subtotal, summed
//! exactly via `rust_decimal::Decimal` (never `f64`).

use dioxus::prelude::*;
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::Route;
use crate::components::NavBar;
use crate::services::logout;
use crate::services::portfolio_review::{
    CashBalanceDto, PositionDto, list_cash_balances, list_positions,
};

/// The Portfolio Insights page (Phase 2: read-only positions + cash/buffer).
#[component]
#[must_use]
pub fn PortfolioReviewView() -> Element {
    let positions = use_resource(move || async move { list_positions().await });
    let balances = use_resource(move || async move { list_cash_balances().await });

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
        }
    }
}

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
}
