//! `NavBar` — the application navigation bar primitive (`RUST-DIOXUS-14`).
//!
//! Exactly one canonical implementation of the nav bar lives here; every page
//! view composes it rather than reimplementing a nav header inline.  The bar
//! renders:
//!
//!   - Brand anchor linking to the Ledger (`/budget`).
//!   - Dioxus [`Link`] elements to each authenticated route: **Ledger** and
//!     **Pending triage**. The active route gets the `.active` CSS class from
//!     the Dioxus router so the current screen is visually highlighted.
//!   - A "Sign out" button that the parent view wires to its own logout handler
//!     (the nav bar has no server-function call itself — it delegates via the
//!     `on_signout: EventHandler<()>` prop so the view owns the navigation-after-
//!     logout).
//!
//! ## Styling
//!
//! All styling is via the global `app.css` stylesheet loaded by `App` (FE3).
//! CSS classes: `.nav-bar`, `.nav-bar__brand`, `.nav-bar__links`,
//! `.nav-bar__link` (active = `.active` added by the router), `.nav-bar__signout`.
//!
//! TODO(visual-polish): mobile hamburger menu, icon integration.

use dioxus::prelude::*;

use crate::Route;

/// Application navigation bar.
///
/// Composed by every authenticated page view. The parent view provides the
/// `on_signout` callback so the view controls its own logout flow (the nav bar
/// has no dependency on any server function, keeping it a pure primitive).
///
/// ### Props
///
/// - `on_signout` — called when the user clicks the "Sign out" button. The
///   parent view should call `logout()` + navigate to `Route::Login {}`.
#[component]
#[must_use]
pub fn NavBar(on_signout: EventHandler<()>) -> Element {
    rsx! {
        nav {
            class: "nav-bar",
            role: "navigation",
            "aria-label": "Main navigation",

            // Brand / home link
            Link {
                to: Route::LedgerView {},
                class: "nav-bar__brand",
                "Budget Tracker"
            }

            // Screen links + sign-out
            div {
                class: "nav-bar__links",

                Link {
                    to: Route::LedgerView {},
                    class: "nav-bar__link",
                    active_class: "active",
                    "Ledger"
                }
                Link {
                    to: Route::PendingView {},
                    class: "nav-bar__link",
                    active_class: "active",
                    "Pending"
                }
                button {
                    class: "nav-bar__signout",
                    r#type: "button",
                    onclick: move |_| on_signout.call(()),
                    "Sign out"
                }
            }
        }
    }
}
