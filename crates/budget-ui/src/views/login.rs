//! The login view — the only unauthenticated affordance (`BUDGET-AUTH-GATE-1`:
//! no public signup, the site exposes only a login).
//!
//! Phase B0 scaffold: a non-functional placeholder form. The real handler
//! (password + mandatory TOTP, passkeys) is a later phase; when built, the form
//! constructs the domain newtypes through their fallible constructors and
//! renders whatever validation error they return (`RUST-DIOXUS-13`) — it never
//! re-implements validation rules here. A `// TODO` marks the visual polish and
//! the live wiring; see the MANUAL-QA notes in the build worklog.

use dioxus::prelude::*;

use crate::Route;

/// Login page (placeholder).
///
/// TODO(frontend-phase): wire the submit handler to the `AuthService` login
/// server function (password + TOTP), add passkey/WebAuthn, and style. Manual QA
/// required (checker-poor half).
#[component]
#[must_use]
pub fn Login() -> Element {
    rsx! {
        main { style: "font-family: sans-serif; max-width: 24rem; margin: 4rem auto; padding: 1rem;",
            h1 { "Budget Tracker" }
            p { "Sign in to continue." }
            // Placeholder form: no submit logic yet (Phase B0 scaffold).
            form {
                onsubmit: move |evt| {
                    // Prevent a real navigation; the live handler lands in a later phase.
                    evt.prevent_default();
                },
                label {
                    "Email"
                    input { r#type: "email", name: "email", autocomplete: "username" }
                }
                label {
                    "Password"
                    input {
                        r#type: "password",
                        name: "password",
                        autocomplete: "current-password",
                    }
                }
                label {
                    "Authenticator code"
                    input { r#type: "text", name: "totp", inputmode: "numeric", autocomplete: "one-time-code" }
                }
                button { r#type: "submit", "Sign in" }
            }
            p {
                // The budget view is gated by auth in a later phase; this link
                // exists only so the scaffold's second route is reachable.
                Link { to: Route::BudgetView {}, "(scaffold) go to budget view" }
            }
        }
    }
}
