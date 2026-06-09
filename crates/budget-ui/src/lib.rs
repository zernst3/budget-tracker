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
//!
//! ## PWA (Phase B3)
//!
//! The app is an installable PWA: a manifest (`/manifest.json`) declares the app
//! name, icons, and standalone display mode so it installs to the phone home
//! screen. A thin service worker (`/service-worker.js`) caches the app shell
//! (HTML, manifest, static assets) for faster cold starts on repeat visits.
//! **NOT offline-first** — the budget app is server-backed and requires the DB.
//! The service worker uses network-first for the shell and cache-first for
//! versioned assets; API responses are never cached.

#![warn(missing_docs)]

pub mod components;
pub mod services;
pub mod views;

// The server-side application state (repositories + AuthService) the gated
// server functions read. Server-only (`#[cfg(feature = "server")]`): it pulls
// the app-services + infrastructure stack, which must never enter the wasm32
// client graph (`RUST-DIOXUS-16`). The `budget-server` host builds it and mounts
// it as an Axum `Extension` layer.
#[cfg(feature = "server")]
pub mod server_state;

use dioxus::prelude::*;

use views::{LedgerView, Login};

/// The application route table.
///
/// A central `Routable` enum (`RUST-DIOXUS-1`). Page-level views compose
/// primitives; primitives never compose views. `Login` is the public entry (no
/// public signup, `BUDGET-AUTH-GATE-1` — only a login), and `LedgerView` is the
/// authenticated month-ledger screen (`SPEC §7`).
#[derive(Routable, Clone, PartialEq)]
pub enum Route {
    /// The login page — the only unauthenticated affordance.
    #[route("/")]
    Login {},
    /// The authenticated month-ledger view (`SPEC §7`).
    #[route("/budget")]
    LedgerView {},
}

/// The root application component, shared by the server (SSR) and web (hydrate)
/// targets. Mounts the router; cross-tree context providers (`AuthContext`,
/// etc., `RUST-DIOXUS-4`) attach here as the app grows past the scaffold.
///
/// **PWA metadata** (Phase B3): wires the manifest link and service-worker
/// registration into the document head. The manifest allows the app to install
/// to the phone home screen; the service worker caches the app shell for faster
/// cold starts on repeat visits. See the crate-level docs for the thin service
/// worker implementation (app-shell caching only, NOT offline-first).
#[component]
#[must_use]
pub fn App() -> Element {
    // Register the service worker (client-side only, wasm32 target).
    // This runs once on app load and is a no-op if the worker is already registered.
    #[cfg(target_arch = "wasm32")]
    {
        use dioxus::prelude::use_effect;

        // Use a captured unit closure to fire once on mount.
        use_effect(|| {
            // Safe to call on wasm32; the promise is unresolved on the server
            // and never awaited there, so this compiles but has no runtime effect.
            // `register` returns a `Promise`; we fire-and-forget it.
            // The `let _ =` suppresses the unused-value lint.
            if let Some(w) = web_sys::window() {
                let _promise = w
                    .navigator()
                    .service_worker()
                    .register("/service-worker.js");
            }
        });
    }

    rsx! {
        // Document head metadata for the PWA (manifest link, theme color, icons).
        // These render in the HTML <head> on SSR and are preserved on hydration.
        document::Link { rel: "manifest", href: "/manifest.json" }
        document::Meta { name: "theme-color", content: "#1f2937" }
        document::Meta { name: "apple-mobile-web-app-capable", content: "yes" }
        document::Meta { name: "apple-mobile-web-app-status-bar-style", content: "default" }
        document::Meta { name: "apple-mobile-web-app-title", content: "Budget" }

        // Router mounts the page views.
        Router::<Route> {}
    }
}
