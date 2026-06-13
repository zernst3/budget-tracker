//! Account / second-factor management server functions
//! (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`, `RUST-DIOXUS-9`).
//!
//! [`totp_enrollment`] re-derives the signed-in user's CURRENT TOTP second
//! factor and renders its `otpauth://` provisioning URI as an inline SVG QR code,
//! so the user can scan it into an additional authenticator app / device. It does
//! **not** rotate the secret — adding a device leaves existing devices working
//! (the rotation path is the out-of-band `enroll_totp` admin flow, not this
//! screen).
//!
//! Gated like every data server function: [`require_authed_user`] runs first, so
//! an unauthenticated caller 401s and the secret is never reached. The QR is
//! rendered server-side (the `qrcode` crate is a native-only dep, used inside the
//! `#[server]` body) and the SVG string crosses the wire already built.

use dioxus::prelude::*;

/// The signed-in user's current TOTP enrollment, shaped for the account screen.
///
/// `qr_svg` is a complete, self-contained `<svg>…</svg>` document for the
/// provisioning URI (rendered server-side); the view injects it directly.
/// `secret` is the Base32 shared secret for manual entry (authenticator apps that
/// cannot scan). `provisioning_uri` is the raw `otpauth://` string (the same data
/// the QR encodes) for copy/paste.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TotpEnrollmentDto {
    /// The raw `otpauth://totp/...` provisioning URI (what the QR encodes).
    pub provisioning_uri: String,
    /// The Base32 shared secret, for manual entry into an authenticator app.
    pub secret: String,
    /// A complete inline `<svg>` QR code for `provisioning_uri`.
    pub qr_svg: String,
}

/// Re-derive the authenticated user's current TOTP second factor and render it as
/// a scannable QR code (`SPEC §9.1`).
///
/// Used by the `/account` screen to add another authenticator device for the
/// EXISTING secret (no rotation). Gated: unauthenticated callers 401 and reach no
/// secret.
///
/// # Errors
///
/// - `401` (via the gate) when there is no valid authenticated session.
/// - `500` on a genuine server fault (no enrolled secret, a TOTP/QR engine
///   failure, or a persistence error) — the failure detail is not leaked.
#[server]
pub async fn totp_enrollment() -> Result<TotpEnrollmentDto, ServerFnError> {
    use dioxus::fullstack::FullstackContext;
    use dioxus::fullstack::axum::Extension;
    use qrcode::QrCode;
    use qrcode::render::svg;

    use crate::server_state::AppState;
    use crate::services::gate::require_authed_user;

    // GATE FIRST — no secret is read before this returns Ok.
    let user = require_authed_user().await?;

    // The AuthService re-derives the URI for the user's stored secret (it does not
    // rotate it). AppState carries the service; its absence is a wiring fault.
    let Extension(state) = FullstackContext::extract::<Extension<AppState>, _>()
        .await
        .map_err(|_| ServerFnError::new("server state unavailable"))?;

    let enrollment = state
        .auth
        .current_totp_provisioning(user.id())
        .await
        .map_err(|_| ServerFnError::new("could not load second factor"))?;

    // Render the provisioning URI to a self-contained SVG QR (server-side).
    let qr_svg = QrCode::new(enrollment.provisioning_uri.as_bytes())
        .map_err(|_| ServerFnError::new("could not encode TOTP QR"))?
        .render::<svg::Color>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .build();

    Ok(TotpEnrollmentDto {
        provisioning_uri: enrollment.provisioning_uri,
        secret: enrollment.secret,
        qr_svg,
    })
}
