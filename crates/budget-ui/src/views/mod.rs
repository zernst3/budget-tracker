//! Page-level views (`RUST-DIOXUS-1`): one component per route. Views compose
//! primitives from [`crate::components`]; primitives never compose views.

mod budget;
mod login;

pub use budget::BudgetView;
pub use login::Login;
