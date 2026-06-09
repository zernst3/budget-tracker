//! Reusable primitive components (`RUST-DIOXUS-1` / `RUST-DIOXUS-14`): Button,
//! Modal, Input, and similar. Each primitive has exactly one canonical
//! implementation here; views compose them and never reimplement a primitive
//! inline.
//!
//! Phase B0 ships no primitives yet (the scaffold views use raw elements). They
//! land as the UI grows; this module is the single home for them so design-system
//! decisions live in one place.
//!
//! [`webauthn`] is not a visual primitive but the client-side `WebAuthn` ceremony
//! bridge (`navigator.credentials` via `document::eval`, `RUST-DIOXUS-15`): the
//! reusable, view-agnostic glue the login + budget views call to run the passkey
//! register / authenticate ceremonies in the browser.

pub mod webauthn;
