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

mod health;

pub use health::health;
