//! The login view — the only unauthenticated affordance (`BUDGET-AUTH-GATE-1`:
//! no public signup, the site exposes only a login).
//!
//! Two sign-in paths (`SPEC §9.1`):
//! - **Password + mandatory TOTP** (the [`login`](crate::services::login) server
//!   function). The fallback factor; fully wired here.
//! - **Passkey / `WebAuthn`** (the day-to-day biometric path: Touch ID / Face ID /
//!   a phone passkey). "Sign in with passkey" resolves the account by the typed
//!   email, runs the browser ceremony
//!   ([`authenticate_passkey_ceremony`](crate::components::webauthn::authenticate_passkey_ceremony)),
//!   and establishes the session through the finish server function.
//!
//! Both establish the same server-side session and navigate to the budget view.
//! On failure each shows a single opaque error (anti-enumeration: the server
//! returns the same 401 for every credential failure / unknown account, so the UI
//! cannot say which factor was wrong or whether the account exists). Visual polish
//! is still minimal — `// TODO(frontend-phase)` marks it.

use dioxus::prelude::*;

use crate::Route;
use crate::components::webauthn::authenticate_passkey_ceremony;
use crate::services::{
    LoginRequest, finish_passkey_authentication, login, start_passkey_authentication,
};

/// Login page.
///
/// TODO(frontend-phase): styling + the TOTP-enroll QR flow. Manual QA required
/// (checker-poor half) for the passkey path against a real authenticator.
#[component]
#[must_use]
pub fn Login() -> Element {
    // Raw form text held in signals (RUST-DIOXUS-3). The domain validates the
    // email shape server-side; the UI does not re-implement validation
    // (RUST-DIOXUS-13).
    let mut email = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut totp_code = use_signal(String::new);
    // None = no attempt yet; Some(true) = in flight; the error string is shown
    // verbatim from the server's opaque rejection.
    let mut error = use_signal(|| Option::<String>::None);
    let mut submitting = use_signal(|| false);

    let nav = use_navigator();

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        // Fire-and-await the login server call (RUST-DIOXUS-6: this is a
        // mutation, driven by spawn, not use_resource which is for displayed
        // data).
        spawn(async move {
            submitting.set(true);
            error.set(None);
            let request = LoginRequest {
                email: email.read().clone(),
                password: password.read().clone(),
                totp_code: totp_code.read().clone(),
            };
            match login(request).await {
                Ok(()) => {
                    // Session established; go to the gated budget view.
                    nav.push(Route::BudgetView {});
                }
                Err(_) => {
                    // Opaque: the server does not reveal which factor failed.
                    error.set(Some("Invalid email, password, or code.".to_owned()));
                }
            }
            submitting.set(false);
        });
    };

    // The passkey (biometric) sign-in path. Uses the typed email to resolve which
    // credentials to challenge (non-discoverable WebAuthn), runs the browser
    // ceremony, then finishes server-side to establish the session.
    let on_passkey = move |_| {
        spawn(async move {
            submitting.set(true);
            error.set(None);
            let typed_email = email.read().clone();
            let result = async {
                // 1. Server issues the challenge for this account's credentials.
                let options = start_passkey_authentication(typed_email)
                    .await
                    .map_err(|_| "No passkey is available for that email.".to_owned())?;
                // 2. The browser runs the OS biometric ceremony.
                let assertion = authenticate_passkey_ceremony(options).await?;
                // 3. The server verifies the assertion and writes the session.
                finish_passkey_authentication(assertion)
                    .await
                    .map_err(|_| "Passkey sign-in failed.".to_owned())?;
                Ok::<(), String>(())
            }
            .await;
            match result {
                Ok(()) => {
                    nav.push(Route::BudgetView {});
                }
                Err(message) => error.set(Some(message)),
            }
            submitting.set(false);
        });
    };

    rsx! {
        main { style: "font-family: sans-serif; max-width: 24rem; margin: 4rem auto; padding: 1rem;",
            h1 { "Budget Tracker" }
            p { "Sign in to continue." }
            form { onsubmit: on_submit,
                label {
                    "Email"
                    input {
                        r#type: "email",
                        name: "email",
                        autocomplete: "username",
                        value: "{email}",
                        oninput: move |e| email.set(e.value()),
                    }
                }
                label {
                    "Password"
                    input {
                        r#type: "password",
                        name: "password",
                        autocomplete: "current-password",
                        value: "{password}",
                        oninput: move |e| password.set(e.value()),
                    }
                }
                label {
                    "Authenticator code"
                    input {
                        r#type: "text",
                        name: "totp",
                        inputmode: "numeric",
                        autocomplete: "one-time-code",
                        value: "{totp_code}",
                        oninput: move |e| totp_code.set(e.value()),
                    }
                }
                button { r#type: "submit", disabled: submitting(),
                    if submitting() { "Signing in…" } else { "Sign in" }
                }
            }
            // Passkey (biometric) sign-in. Uses the email typed above to resolve
            // the account; the OS handles the fingerprint / face prompt.
            // TODO(frontend-phase): style the divider + button.
            p { style: "margin-top: 1rem; color: #666;", "or" }
            button {
                r#type: "button",
                disabled: submitting(),
                onclick: on_passkey,
                "Sign in with passkey"
            }
            if let Some(message) = error() {
                p { style: "color: #b00020;", role: "alert", "{message}" }
            }
        }
    }
}
