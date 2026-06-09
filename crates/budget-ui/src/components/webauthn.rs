//! The client-side `WebAuthn` ceremony bridge (`SPEC §9.1`, `RUST-DIOXUS-15`).
//!
//! A web app cannot read a fingerprint sensor directly: it calls the browser's
//! `navigator.credentials` API, which has the OS mediate the biometric (Touch ID /
//! Face ID / a phone passkey) and return a public-key assertion. That API lives in
//! JavaScript and traffics in `ArrayBuffer`s, neither of which Rust/wasm can call
//! ergonomically through `web-sys` without a large amount of glue. The idiomatic
//! Dioxus path (and the one `RUST-DIOXUS-15` codifies) is [`dioxus::document::eval`]:
//! run a small JS snippet, hand it the challenge, await the browser ceremony, and
//! receive the result back via `dioxus.send(value)`.
//!
//! ## The protocol with the server functions
//!
//! 1. A server function ([`start_passkey_registration`] / [`start_passkey_authentication`])
//!    returns the `WebAuthn` options as JSON (the webauthn-rs serialization, whose
//!    binary fields — `challenge`, `user.id`, credential ids — are base64url
//!    strings).
//! 2. This bridge feeds that JSON to the JS snippet, which converts the base64url
//!    fields to `ArrayBuffer`s, calls `navigator.credentials.create` / `.get`,
//!    then serializes the browser's `PublicKeyCredential` back into the JSON shape
//!    webauthn-rs's `RegisterPublicKeyCredential` / `PublicKeyCredential` expects
//!    (base64url again) and posts it back with `dioxus.send`.
//! 3. The returned JSON is handed to the finish server function
//!    ([`finish_passkey_registration`] / [`finish_passkey_authentication`]), which
//!    verifies it against the stashed ceremony state.
//!
//! ## Why `document::eval` and not `web-sys`
//!
//! The `navigator.credentials` ceremony is fundamentally a `Promise`-returning
//! browser API operating on `ArrayBuffer`s. Driving it through `web-sys` would
//! mean hand-marshalling every `PublicKeyCredentialCreationOptions` field and the
//! `AuthenticatorAttestationResponse` back out, byte by byte — far more surface
//! than the few dozen lines of JSON↔base64url glue here, and `RUST-DIOXUS-15`
//! exists precisely so this kind of one-shot browser interop goes through the eval
//! bridge. No secret or token is ever touched: the challenge is public ceremony
//! material and the response is a public-key assertion.
//!
//! Every code path returns a `Result<serde_json::Value, String>`; the `Err`
//! carries a short, non-sensitive reason (e.g. the user cancelled the OS prompt)
//! suitable for a single inline message.
//!
//! [`start_passkey_registration`]: crate::services::start_passkey_registration
//! [`start_passkey_authentication`]: crate::services::start_passkey_authentication
//! [`finish_passkey_registration`]: crate::services::finish_passkey_registration
//! [`finish_passkey_authentication`]: crate::services::finish_passkey_authentication

use dioxus::prelude::*;

/// The shared JS helpers: base64url <-> `ArrayBuffer`, injected ahead of each
/// ceremony so the snippet that follows can call them. `b64urlToBuf` decodes a
/// base64url string to a `Uint8Array`'s buffer; `bufToB64url` encodes an
/// `ArrayBuffer` back to base64url (no padding), matching the webauthn-rs JSON
/// representation on both directions.
const B64URL_HELPERS: &str = r"
function b64urlToBuf(s) {
    const pad = s.length % 4 === 0 ? '' : '='.repeat(4 - (s.length % 4));
    const b64 = (s.replace(/-/g, '+').replace(/_/g, '/')) + pad;
    const bin = atob(b64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) { bytes[i] = bin.charCodeAt(i); }
    return bytes.buffer;
}
function bufToB64url(buf) {
    const bytes = new Uint8Array(buf);
    let bin = '';
    for (let i = 0; i < bytes.length; i++) { bin += String.fromCharCode(bytes[i]); }
    return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}
";

/// Run the browser registration ceremony (`navigator.credentials.create`).
///
/// `options` is the JSON returned by [`start_passkey_registration`]. On success
/// the returned `Value` is the `RegisterPublicKeyCredential` JSON to hand to
/// [`finish_passkey_registration`].
///
/// # Errors
/// A short reason string if the browser lacks `WebAuthn`, the user cancels the OS
/// prompt, or the eval bridge fails.
///
/// [`start_passkey_registration`]: crate::services::start_passkey_registration
/// [`finish_passkey_registration`]: crate::services::finish_passkey_registration
pub async fn register_passkey_ceremony(
    options: serde_json::Value,
) -> Result<serde_json::Value, String> {
    // The snippet:
    //   1. awaits the options JSON from Rust (dioxus.recv),
    //   2. rebuilds the BufferSource fields the API requires,
    //   3. runs navigator.credentials.create,
    //   4. serializes the attestation response back to the webauthn-rs JSON shape,
    //   5. posts it (or an {error} envelope) back via dioxus.send (RUST-DIOXUS-15).
    let script = format!(
        "{B64URL_HELPERS}
        (async () => {{
            try {{
                if (!window.PublicKeyCredential || !navigator.credentials) {{
                    dioxus.send({{ error: 'This browser does not support passkeys.' }});
                    return;
                }}
                const options = await dioxus.recv();
                const pk = options.publicKey;
                pk.challenge = b64urlToBuf(pk.challenge);
                pk.user.id = b64urlToBuf(pk.user.id);
                if (Array.isArray(pk.excludeCredentials)) {{
                    pk.excludeCredentials = pk.excludeCredentials.map(c => ({{ ...c, id: b64urlToBuf(c.id) }}));
                }}
                const cred = await navigator.credentials.create({{ publicKey: pk }});
                const r = cred.response;
                const out = {{
                    id: cred.id,
                    rawId: bufToB64url(cred.rawId),
                    type: cred.type,
                    extensions: (cred.getClientExtensionResults ? cred.getClientExtensionResults() : {{}}),
                    response: {{
                        attestationObject: bufToB64url(r.attestationObject),
                        clientDataJSON: bufToB64url(r.clientDataJSON),
                    }},
                }};
                dioxus.send({{ ok: out }});
            }} catch (e) {{
                dioxus.send({{ error: (e && e.message) ? e.message : 'Passkey registration was cancelled.' }});
            }}
        }})();
        "
    );
    run_ceremony(&script, options).await
}

/// Run the browser authentication ceremony (`navigator.credentials.get`).
///
/// `options` is the JSON returned by [`start_passkey_authentication`]. On success
/// the returned `Value` is the `PublicKeyCredential` assertion JSON to hand to
/// [`finish_passkey_authentication`].
///
/// # Errors
/// A short reason string if the browser lacks `WebAuthn`, the user cancels the OS
/// prompt, or the eval bridge fails.
///
/// [`start_passkey_authentication`]: crate::services::start_passkey_authentication
/// [`finish_passkey_authentication`]: crate::services::finish_passkey_authentication
pub async fn authenticate_passkey_ceremony(
    options: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let script = format!(
        "{B64URL_HELPERS}
        (async () => {{
            try {{
                if (!window.PublicKeyCredential || !navigator.credentials) {{
                    dioxus.send({{ error: 'This browser does not support passkeys.' }});
                    return;
                }}
                const options = await dioxus.recv();
                const pk = options.publicKey;
                pk.challenge = b64urlToBuf(pk.challenge);
                if (Array.isArray(pk.allowCredentials)) {{
                    pk.allowCredentials = pk.allowCredentials.map(c => ({{ ...c, id: b64urlToBuf(c.id) }}));
                }}
                const cred = await navigator.credentials.get({{ publicKey: pk }});
                const r = cred.response;
                const out = {{
                    id: cred.id,
                    rawId: bufToB64url(cred.rawId),
                    type: cred.type,
                    extensions: (cred.getClientExtensionResults ? cred.getClientExtensionResults() : {{}}),
                    response: {{
                        authenticatorData: bufToB64url(r.authenticatorData),
                        clientDataJSON: bufToB64url(r.clientDataJSON),
                        signature: bufToB64url(r.signature),
                        userHandle: (r.userHandle ? bufToB64url(r.userHandle) : null),
                    }},
                }};
                dioxus.send({{ ok: out }});
            }} catch (e) {{
                dioxus.send({{ error: (e && e.message) ? e.message : 'Passkey sign-in was cancelled.' }});
            }}
        }})();
        "
    );
    run_ceremony(&script, options).await
}

/// The shared eval driver: send the options into the snippet, await the
/// `{ ok | error }` envelope it posts back (`RUST-DIOXUS-15`: the snippet ends in
/// `dioxus.send`), and unwrap it into a `Result`.
async fn run_ceremony(
    script: &str,
    options: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut eval = document::eval(script);
    // Hand the options to the snippet's `await dioxus.recv()`.
    eval.send(options)
        .map_err(|e| format!("could not start the passkey ceremony: {e}"))?;
    // Await the single envelope the snippet posts back.
    let envelope: serde_json::Value = eval
        .recv()
        .await
        .map_err(|e| format!("the passkey ceremony did not complete: {e}"))?;

    if let Some(ok) = envelope.get("ok") {
        Ok(ok.clone())
    } else {
        let reason = envelope
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("The passkey ceremony failed.")
            .to_owned();
        Err(reason)
    }
}
