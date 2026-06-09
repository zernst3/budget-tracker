//! `ensure_month` server function — lazy-init the budget month record
//! (`BUDGET-IDEMPOTENT-MONTH-INIT-1`).
//!
//! Fires `MonthLifecycleService::ensure_current_month` on page load so the
//! lazy-init runs before the view fetch. Idempotent: if the month already
//! exists it is a fast no-op.

use dioxus::prelude::*;

/// Ensure the current month is initialised (lazy-init, `BUDGET-IDEMPOTENT-MONTH-INIT-1`).
///
/// Call this once on page load before any budget queries. It is idempotent: if the
/// month already exists it is a fast no-op. Gated by `require_authed_user`.
///
/// # Errors
///
/// `ServerFnError` if the session is absent (401) or any persistence call fails.
#[allow(clippy::unused_async)]
#[server]
pub async fn ensure_month() -> Result<(), dioxus::prelude::ServerFnError> {
    use chrono::Utc;

    use crate::server_state::MonthViewState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = MonthViewState::extract().await?;
    state
        .lifecycle
        .ensure_current_month(user.id(), Utc::now())
        .await
        .map_err(|e| dioxus::prelude::ServerFnError::ServerError {
            message: e.to_string(),
            code: 500,
            details: None,
        })?;
    Ok(())
}
