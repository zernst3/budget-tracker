//! Reusable primitive components (`RUST-DIOXUS-1` / `RUST-DIOXUS-14`): Button,
//! Modal, Input, and similar. Each primitive has exactly one canonical
//! implementation here; views compose them and never reimplement a primitive
//! inline.
//!
//! ## Primitives
//!
//! - [`nav`] — the application [`NavBar`]: brand + authenticated route links +
//!   sign-out button. Every authenticated page view composes this rather than
//!   reimplementing a nav header inline (`RUST-DIOXUS-14`). Styled via
//!   `/app.css` (FE3).
//!
//! [`webauthn`] is not a visual primitive but the client-side `WebAuthn` ceremony
//! bridge (`navigator.credentials` via `document::eval`, `RUST-DIOXUS-15`): the
//! reusable, view-agnostic glue the login + budget views call to run the passkey
//! register / authenticate ceremonies in the browser.

pub mod nav;
pub mod webauthn;

pub use nav::NavBar;
