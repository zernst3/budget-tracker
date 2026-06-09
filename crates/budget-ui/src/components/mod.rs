//! Reusable primitive components (`RUST-DIOXUS-1` / `RUST-DIOXUS-14`): Button,
//! Modal, Input, and similar. Each primitive has exactly one canonical
//! implementation here; views compose them and never reimplement a primitive
//! inline.
//!
//! Phase B0 ships no primitives yet (the scaffold views use raw elements). They
//! land as the UI grows; this module is the single home for them so design-system
//! decisions live in one place.
