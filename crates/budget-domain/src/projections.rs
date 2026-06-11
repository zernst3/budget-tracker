//! Read-side projection types for computed/aggregate read surfaces (`REPO-9`,
//! `RUST-SEAORM-PROJECTION-TYPES-1`).
//!
//! A projection is a typed view that does NOT correspond to a single stored
//! aggregate: it is the shape of a computed read (a conditional aggregate, a
//! grouped sum) returned by a repository read method. These exist so the budget
//! math (category spent, month net) is computed in ONE SQL query rather than by
//! fetching every transaction and folding in Rust (`SQL-DB-NPLUSONE-1` /
//! `DB-NPLUSONE-1`).
//!
//! ## Where the `SeaORM` result-mapping derive lives (convention reconciliation)
//!
//! `RUST-SEAORM-PROJECTION-TYPES-1` says a projection "lives in the domain crate
//! and derives the `SeaORM` result-mapping trait." That is in direct tension with
//! `DOMAIN-1`/`RUST-DOMAIN-1`, which forbids ANY ORM dependency in this crate
//! (the crate's `Cargo.toml` carries no `sea-orm` dep and must compile cleanly to
//! WASM). The two rules cannot both be taken literally.
//!
//! Resolution (pending Zach's ratification — see the run report): the projection
//! STRUCT lives here as a pure domain type, satisfying both "lives in the domain
//! crate" and `RUST-SEAORM-RAW-SQL-ESCAPE-1`'s harder invariant that "only typed
//! domain types cross the trait boundary." The `FromQueryResult` derive itself
//! lives on a private infra-local row struct in `budget-infrastructure`, which
//! maps into these domain projections. `DOMAIN-1` (a structural, lint-and-
//! Cargo-enforced invariant) wins over the literal "derives the trait here"
//! clause; the spirit of the projection rule (single typed query, named for the
//! view, domain type at the boundary) is fully preserved.
//!
//! The inclusion polarity used by these aggregates
//! (`BUDGET-STATUS-DRIVES-INCLUSION-1`: settled + expected count, pending does
//! not) is pushed into the SQL `WHERE` in the infra impl; the canonical source of
//! that polarity remains [`crate::predicates::counts_in_budget`], and the SQL
//! comment in the impl cross-references it so the two cannot silently diverge.

use crate::ids::{CategoryId, MonthId};
use crate::money::Money;

/// The spent-to-date total for one category within one month
/// (`BUDGET-NO-DOUBLE-CHARGE-1` input, `SPEC §4.5`).
///
/// `spent` is the signed sum (`Money`, `BUDGET-MONEY-1`) of that category's
/// budget-counting transactions in the month — i.e. only the statuses for which
/// [`crate::predicates::counts_in_budget`] is `true` (settled + expected;
/// pending excluded). It is the raw transaction sum; the fixed-category
/// settled-vs-placeholder choice (`BUDGET-NO-DOUBLE-CHARGE-1`) is applied on top
/// by [`crate::predicates::fixed_category_spent`] in the service layer, which
/// this projection feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CategorySpent {
    /// The category whose spend this row reports.
    pub category_id: CategoryId,
    /// Signed sum of the category's budget-counting transactions in the month.
    pub spent: Money,
}

/// The net position of one month: the signed sum of every budget-counting
/// transaction in it (`SPEC §4.3` rolling-Other input,
/// `BUDGET-STATUS-DRIVES-INCLUSION-1`).
///
/// `net` is the signed `Money` total (income positive, expenses negative) of all
/// transactions in the month whose status counts toward budget math.
///
/// NOTE: the production rolling-Other / rollover computation is the single
/// authoritative path
/// [`budget_app_services::MonthLifecycleService::month_net_for`], which folds the
/// full Rust predicate (`counts_in_month_expense_remaining`, including the
/// `is_fund_draw` and income exclusions). The parallel `TransactionRepository::
/// month_net` SQL aggregate that once returned this type was DELETED to remove a
/// drift-prone second net formula (`DRIFT_REPORT` MUST-FIX #2 / SHOULD-FIX #5,
/// `SPIRIT-ROBUSTNESS-1`). This struct survives only as a convenient net-oracle
/// shape for test fakes; no production code returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonthNet {
    /// The month this net belongs to.
    pub month_id: MonthId,
    /// Signed sum of all budget-counting transactions in the month.
    pub net: Money,
}
