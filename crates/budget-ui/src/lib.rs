//! `budget-ui` — the Dioxus 0.7 fullstack application crate.
//!
//! This crate is the single shared `App` for both the native `server` target
//! (SSR host + server-function handlers) and the `web` wasm32 target (the
//! hydrating client bundle), per `RUST-DIOXUS-16`. The `budget-server` binary
//! crate mounts the Axum router that serves all three concerns from one process
//! (`RUST-DIOXUS-11`); this crate owns the UI tree, the router, and the server
//! functions.
//!
//! ## Organization (`RUST-DIOXUS-1`)
//!
//! Source is split by file role, not by feature:
//! - [`views`] — page-level components, one per route.
//! - [`components`] — reusable primitives composed by views.
//! - [`services`] — server-function wrappers (`RUST-DIOXUS-9`): the client-side
//!   call and the server-side handler are generated from one definition; the
//!   server body runs in the app-services layer (gated to the `server` feature).
//!
//! The central [`Route`] enum lives here at the crate root.

#![warn(missing_docs)]

pub mod components;
pub mod services;
pub mod views;

use dioxus::prelude::*;

use views::{BudgetView, Login};

/// The application route table.
///
/// A central `Routable` enum (`RUST-DIOXUS-1`). Page-level views compose
/// primitives; primitives never compose views. Routes are placeholders for
/// Phase B0 (the scaffold): `Login` is the public entry (no public signup,
/// `BUDGET-AUTH-GATE-1` — only a login), and `BudgetView` is the future
/// authenticated transactions screen (`SPEC §7`).
#[derive(Routable, Clone, PartialEq)]
pub enum Route {
    /// The login page — the only unauthenticated affordance.
    #[route("/")]
    Login {},
    /// The authenticated budget / transactions view (placeholder).
    #[route("/budget")]
    BudgetView {},
}

/// The root application component, shared by the server (SSR) and web (hydrate)
/// targets. Mounts the router; cross-tree context providers (`AuthContext`,
/// etc., `RUST-DIOXUS-4`) attach here as the app grows past the scaffold.
#[component]
#[must_use]
pub fn App() -> Element {
    rsx! {
        Router::<Route> {}
    }
}
