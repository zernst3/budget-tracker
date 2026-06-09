//! Server-function wrappers (`RUST-DIOXUS-9`).
//!
//! Each `#[server]` function compiles to a client-side call (serialized over the
//! wire) AND a server-side handler from one definition; the server body runs in
//! this process and may call the app-services layer directly. A separately
//! maintained REST/RPC client crate is forbidden (`RUST-DIOXUS-9`, `D1`).
//!
//! Phase B0 ships a single trivial example: [`health`]. It returns a health
//! string and touches nothing dangerous (no DB, no auth, no Plaid). Data-bearing
//! server functions land in later phases and MUST take the `AuthedUser`
//! extractor before any handler logic (`BUDGET-AUTH-GATE-1`); `health` is the
//! deliberate exception (an unauthenticated liveness probe that returns no user
//! data).
//!
//! Phase B4 adds the `ensure_month` lazy-init server function.

pub mod auth;
mod health;
pub mod ledger;
pub mod ledger_edit;
pub mod month_view;
pub mod passkey;
pub mod triage;

// The AuthedUser gate (`BUDGET-AUTH-GATE-1`) is server-only: it extracts the
// session + server state and loads the user. Its types are referenced only from
// `#[server]` bodies, which the macro strips on the wasm client target.
#[cfg(feature = "server")]
pub mod gate;

pub use auth::{LoginRequest, current_user, login, logout};
pub use health::health;
pub use ledger::{
    DayLedgerDto, EnvelopeCategoryDto, EnvelopeSummaryDto, LedgerTransactionDto, MonthLedgerDto,
    get_envelope_summary, get_month_ledger,
};
pub use ledger_edit::{InlineEditRequest, InlineEditResult, update_transaction_inline};
pub use month_view::ensure_month;
pub use passkey::{
    finish_passkey_authentication, finish_passkey_registration, start_passkey_authentication,
    start_passkey_registration,
};
pub use triage::{
    FundDto, PendingRowDto, PullResultDto, TreatmentDto, TriageRequestDto, TriageResultDto,
    get_pending_inbox, list_funds, pull, triage_transaction,
};
