//! `budget-ui` — the Dioxus 0.7 fullstack application crate.
//!
//! This crate is the single shared `App` for both the native server target
//! (SSR host + server-function handlers) and the `web` wasm32 target (the
//! hydrating client bundle), per `RUST-DIOXUS-16` / `PORT-FULLSTACK-1`. The same
//! crate ALSO produces the native `budget-server` entry binary (`src/main.rs`)
//! that mounts the Axum router serving all three concerns from one process
//! (`RUST-DIOXUS-11`), plus the admin CLIs. Per-target code is gated by
//! `cfg(target_arch = "wasm32")` / `cfg(not(target_arch = "wasm32"))`, not a
//! Cargo feature, so a single `dx serve` builds BOTH targets and the server-only
//! deps stay out of the wasm graph.
//!
//! ## Organization (`RUST-DIOXUS-1`)
//!
//! Source is split by file role, not by feature:
//! - [`views`] — page-level components, one per route.
//! - [`components`] — reusable primitives composed by views.
//! - [`services`] — server-function wrappers (`RUST-DIOXUS-9`): the client-side
//!   call and the server-side handler are generated from one definition; the
//!   server body runs in the app-services layer (the `#[server]` macro gates it
//!   on `cfg(feature = "server")`; the heavy server deps it reaches live in the
//!   not-wasm32 target block so the wasm graph stays clean, PORT-FULLSTACK-1).
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
// server functions read. Server-only (`#[cfg(feature = "server")]`, matching the
// gating the `#[server]` macro emits): it pulls the app-services + infrastructure
// stack, which must never enter the wasm32 client graph (`RUST-DIOXUS-16`). The
// heavy deps it uses live in the not-wasm32 target block, so the wasm graph stays
// clean regardless; the feature gate just strips the server bodies on the client.
// The `budget-server` host builds it and mounts it as an Axum `Extension` layer.
#[cfg(feature = "server")]
pub mod server_state;

// The native Axum host wiring (connections -> migrations -> state -> router).
// Server-only (`#[cfg(feature = "server")]`): it pulls the Axum + Dioxus server
// runtime + the persistence stack, none of which may enter the wasm32 client
// graph. The `budget-server` entry binary (`src/main.rs`) calls
// [`server::build_router`]; integration tests can too.
#[cfg(feature = "server")]
pub mod server;

use dioxus::prelude::*;

use views::{AccountView, LedgerView, Login, PendingView, PortfolioReviewView};

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
    /// The authenticated Pending triage inbox + Pull (`SPEC §7`): the transaction
    /// intake screen where newly-pulled settled transactions get category +
    /// comment + one of the three `SPEC §4.9` treatments.
    #[route("/pending")]
    PendingView {},
    /// The authenticated AI Portfolio Insights screen
    /// (`docs/AI_FEATURE_DESIGN.md §Phase 2`): read-only holdings + cash
    /// balances + the reserved-buffer subtotal. Priced snapshot + review insights
    /// land in later phases.
    #[route("/portfolio")]
    PortfolioReviewView {},
    /// The authenticated account & security screen (`SPEC §9.1`): displays the
    /// current TOTP second factor as a QR code so the user can add another
    /// authenticator device.
    #[route("/account")]
    AccountView {},
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
        // Favicon + iOS home-screen icon (served as compiled-in static routes).
        document::Link { rel: "icon", r#type: "image/png", href: "/icons/favicon-32x32.png" }
        document::Link { rel: "apple-touch-icon", href: "/icons/icon-192x192.png" }
        // Global stylesheet (FE3): consistent spacing, nav, currency, group-header.
        // Served by the Axum host as a compiled-in static route (/app.css).
        document::Link { rel: "stylesheet", href: "/app.css" }
        document::Meta { name: "theme-color", content: "#1f2937" }
        // The standard installability hint (the modern replacement for the
        // deprecated `apple-mobile-web-app-capable`, which is kept alongside it
        // only for older iOS Safari).
        document::Meta { name: "mobile-web-app-capable", content: "yes" }
        document::Meta { name: "apple-mobile-web-app-capable", content: "yes" }
        document::Meta { name: "apple-mobile-web-app-status-bar-style", content: "default" }
        document::Meta { name: "apple-mobile-web-app-title", content: "Budget" }

        // Router mounts the page views.
        Router::<Route> {}
    }
}
