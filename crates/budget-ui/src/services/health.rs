//! The trivial example server function for the Phase B0 scaffold.

use dioxus::prelude::*;

/// Liveness probe server function (`RUST-DIOXUS-9`).
///
/// The deliberate, documented exception to the `AuthedUser` gate
/// (`BUDGET-AUTH-GATE-1`): an UNAUTHENTICATED probe that returns a fixed health
/// string and reaches no user data — no DB query, no auth, no Plaid, no secret.
/// Every data-returning server function added later takes the `AuthedUser`
/// extractor before any handler logic.
///
/// The client-side half (the wasm bundle) issues the serialized call; the
/// server-side body runs in this process and returns the literal string.
///
/// # Errors
///
/// Returns [`ServerFnError`] only if the server-function transport itself fails
/// (network/serialization). The handler body is infallible.
// The `#[server]` macro requires an `async fn` signature (it generates the async
// client call + server handler from it); this trivial probe body has no `.await`,
// so `clippy::unused_async` fires on the server target. The async is mandated by
// the framework contract, not incidental — narrow allow against that named cause.
#[allow(clippy::unused_async)]
#[server]
pub async fn health() -> Result<String, ServerFnError> {
    Ok("budget-tracker: ok".to_owned())
}
