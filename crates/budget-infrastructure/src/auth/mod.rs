//! Concrete authentication adapters (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`).
//!
//! Implementations of the domain auth ports (`budget_domain::auth`): the
//! Argon2id password hasher, the RFC 6238 TOTP engine (`AUTH-1/2`), the
//! `webauthn-rs` passkey ceremony engine, the Postgres-backed session store +
//! cookie policy, the [`AuthedUser`](extractor::AuthedUser) Axum extractor that
//! enforces the gate by construction, and the Azure Key Vault secret-vault
//! client (`BUDGET-PLAID-TOKEN-VAULT-1`).
//!
//! The HTTP host and the Dioxus UI are the FRONTEND phase; this module builds and
//! tests the gate + the auth primitives in-crate (a minimal Axum test harness),
//! not a new server crate.

pub mod extractor;
pub mod key_vault;
pub mod password;
pub mod session;
pub mod totp;
pub mod webauthn;

pub use extractor::{AuthState, AuthedUser};
pub use key_vault::AzureKeyVault;
pub use password::Argon2idHasher;
pub use session::{SessionLayerConfig, build_session_layer};
pub use totp::Rfc6238TotpService;
pub use webauthn::WebauthnService;
