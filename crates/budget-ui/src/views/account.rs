//! The account / security view (`/account`) — second-factor management
//! (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`, `RUST-DIOXUS-1`).
//!
//! An authenticated screen that displays the user's CURRENT TOTP second factor as
//! a scannable QR code plus the Base32 secret for manual entry, so they can add
//! the authenticator to another app or device. It re-derives the existing secret
//! (the [`totp_enrollment`](crate::services::totp_enrollment) server function does
//! not rotate it), so enrolling a new device leaves existing devices working.
//!
//! The QR is rendered to SVG server-side and injected here via
//! `dangerous_inner_html` — the value is a server-built `<svg>` document derived
//! from the user's own provisioning URI (no user-supplied markup), so there is no
//! injection surface.

use dioxus::prelude::*;

use crate::Route;
use crate::components::NavBar;
use crate::services::{logout, totp_enrollment};

/// Account & security page: add an authenticator device by scanning the QR for
/// the current TOTP second factor.
#[component]
#[must_use]
pub fn AccountView() -> Element {
    // The current second factor (QR + secret + URI). use_resource because this is
    // displayed data, not a mutation (RUST-DIOXUS-6).
    let enrollment = use_resource(move || async move { totp_enrollment().await });

    let page_nav = use_navigator();
    let on_signout = move |()| {
        spawn(async move {
            let _ = logout().await;
            page_nav.push(Route::Login {});
        });
    };

    rsx! {
        div { class: "app-shell",
            NavBar { on_signout }

            main { class: "page-content",
                h1 { class: "page-title", "Account & security" }

                section { class: "account-card",
                    h2 { class: "account-card__title", "Authenticator app" }
                    p { class: "account-card__lead",
                        "Scan this QR code with an authenticator app "
                        "(1Password, Google Authenticator, Authy, …) to add a new "
                        "device for your sign-in code. Your existing devices keep "
                        "working — this shows the same secret, it does not reset it."
                    }

                    match &*enrollment.read() {
                        None => rsx! {
                            p { class: "loading-text", "Loading your authenticator…" }
                        },
                        Some(Err(_)) => rsx! {
                            p { class: "account-card__error", role: "alert",
                                "Could not load your second factor. "
                            }
                            Link { to: Route::Login {}, "Return to login" }
                        },
                        Some(Ok(data)) => rsx! {
                            // The server-rendered SVG QR (trusted: built from the
                            // user's own provisioning URI, no user markup).
                            div {
                                class: "account-qr",
                                dangerous_inner_html: "{data.qr_svg}",
                            }

                            p { class: "account-card__hint",
                                "Can't scan? Enter this key manually:"
                            }
                            code { class: "account-secret", "{data.secret}" }
                        },
                    }
                }
            }
        }
    }
}
