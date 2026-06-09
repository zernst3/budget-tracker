//! Page-level views (`RUST-DIOXUS-1`): one component per route. Views compose
//! primitives from [`crate::components`]; primitives never compose views.

mod ledger;
mod login;

pub use ledger::LedgerView;
pub use login::Login;
