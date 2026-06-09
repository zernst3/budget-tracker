//! Pull / Pending-inbox / atomic-triage server functions — the BACKEND-3 write
//! side of transaction intake (`SPEC §7`, `§6`, `§4.4`, `§4.9`,
//! `BUDGET-AUTH-GATE-1`, `RUST-DIOXUS-9`).
//!
//! Three gated server functions, each extracting the authenticated user FIRST
//! (`BUDGET-AUTH-GATE-1`) and scoping every operation to that user (`SPEC §9.1`):
//!
//!   - [`pull`] — manually trigger the Plaid cursor sync (`SPEC §6`/§7's "Pull"
//!     button). New SETTLED transactions become visible; Plaid `pending` charges
//!     are excluded (`SPEC §4.4`) by the engine and so never reach the inbox.
//!   - [`get_pending_inbox`] — the triage inbox (`SPEC §7`): every settled,
//!     not-yet-categorized transaction (`status = 'settled'` AND
//!     `category_id IS NULL`). Plaid `pending` rows carry `status = 'pending'` and
//!     are excluded by construction.
//!   - [`triage_transaction`] — apply, atomically, category + comment + exactly one
//!     treatment (the three `SPEC §4.9` paths) to one pending transaction. After it
//!     succeeds the row has a category and leaves the inbox.
//!
//! ## Money representation (`BUDGET-MONEY-1`)
//!
//! Every monetary field on these DTOs is [`budget_domain::Money`] (Decimal-backed,
//! serde-transparent, exact). No float is computed or stored here.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use budget_domain::Money;

// ---------------------------------------------------------------------------
// DTOs (WASM-clean — compile on both targets)
// ---------------------------------------------------------------------------

/// The outcome of a manual Pull (`SPEC §6`/§7): how many rows the cursor sync
/// added / modified / removed, plus the resulting inbox size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullResultDto {
    /// New transactions ingested by this pull.
    pub added: u32,
    /// Existing transactions updated in place (e.g. pending -> settled).
    pub modified: u32,
    /// Transactions removed by Plaid.
    pub removed: u32,
    /// The number of rows now awaiting triage (settled + uncategorized).
    pub pending_inbox_size: u32,
}

/// One row in the triage inbox (`SPEC §7`): a settled, not-yet-categorized bank
/// charge. Read-only here; the user assigns category + comment + treatment via
/// [`triage_transaction`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRowDto {
    /// Stable transaction id — the triage target.
    pub id: String,
    /// Transaction date (ISO `YYYY-MM-DD`).
    pub date: String,
    /// Signed amount (`Money`; negative = expense).
    pub amount: Money,
    /// Plaid / merchant description (read-only).
    pub description: String,
    /// The linked account id, if any (`None` for a manual/unlinked row).
    pub account_id: Option<String>,
}

/// One selectable fund for the triage treatment pickers (`SPEC §4.9`). Surfaced
/// by [`list_funds`] so the UI can offer a fund target for the two fund-backed
/// treatments and label each one's kind + current balance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundDto {
    /// Stable fund id — the treatment target.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Fund kind as a lowercase label (`"buffer"` / `"surplus"`). `SpreadOverMonths`
    /// (buffer-financing) requires a `"buffer"` fund; the UI uses this to gate which
    /// funds it offers for that treatment.
    pub kind: String,
    /// `true` for the buffer (`compulsory_repayment = true`) pool — the only kind
    /// valid for `SpreadOverMonths`.
    pub is_buffer: bool,
    /// Current balance (`Money`; signed, normally positive).
    pub balance: Money,
}

/// The treatment to apply at triage (`SPEC §7` / `§4.9`): exactly one of three
/// paths. The wire shape carries the discriminant plus the path-specific
/// parameters; the server fn maps it to the app-services
/// [`budget_app_services::Treatment`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreatmentDto {
    /// (a) Pay from a savings/surplus fund accrual — a fund draw. Carries the fund
    /// id.
    PayFromSavings {
        /// The savings/surplus fund to draw down.
        fund_id: String,
    },
    /// (b) Spread over the next few months — buffer-financed (D7). Carries the
    /// buffer fund id and the number of compulsory installments.
    SpreadOverMonths {
        /// The buffer fund fronting the cash.
        fund_id: String,
        /// Number of compulsory monthly installments (`>= 1`).
        months: i32,
    },
    /// (c) Pay directly through the budget — a normal in-month expense (the
    /// DEFAULT).
    PayDirectly,
}

/// The input to one atomic triage (`SPEC §7`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageRequestDto {
    /// The pending transaction to triage.
    pub transaction_id: String,
    /// The category to assign (required — categorizing removes the row from the
    /// inbox).
    pub category_id: String,
    /// An optional free-text comment (`transactions.comment`).
    pub comment: Option<String>,
    /// The single treatment to apply.
    pub treatment: TreatmentDto,
}

/// The result of a successful triage (`SPEC §7`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageResultDto {
    /// The triaged transaction id (now categorized, out of the inbox).
    pub transaction_id: String,
    /// The repayment obligation id created for a `SpreadOverMonths` treatment;
    /// `None` for the other treatments.
    pub obligation_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Server functions (native only; the `#[server]` macro strips the body on wasm)
// ---------------------------------------------------------------------------

/// Manually trigger the Plaid cursor sync (`SPEC §6`/§7 "Pull"), gated by session
/// auth (`BUDGET-AUTH-GATE-1`).
///
/// Pulls everything new since the last cursor for the authenticated user. New
/// SETTLED transactions become visible (and, if uncategorized, enter the triage
/// inbox); Plaid `pending` charges are excluded from budget math (`SPEC §4.4`) and
/// never reach the inbox. `today` is resolved in the home timezone (D2) so the
/// rolling reconcile window is deterministic.
///
/// # Errors
///
/// `ServerFnError` (HTTP 401) when there is no valid session; HTTP 503 when Plaid
/// is not configured (no credentials / vault); HTTP 500 on any Plaid or
/// persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn pull() -> Result<PullResultDto, dioxus::prelude::ServerFnError> {
    use crate::server_state::TriageState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = TriageState::extract().await?;

    let Some(plaid) = state.plaid.clone() else {
        return Err(dioxus::prelude::ServerFnError::ServerError {
            message: "bank sync is not configured on this deployment".to_owned(),
            code: 503,
            details: None,
        });
    };

    // Today in the home timezone (D2, ARCH-UTC-TIMESTAMPS-1): the reconcile-window
    // lower bound is a calendar date, resolved in America/New_York.
    let today = home_today();

    let summary = plaid
        .sync_user(user.id(), today)
        .await
        .map_err(|e| plaid_error(&e))?;

    // The resulting inbox size, so the UI can badge the Pending tab in one round
    // trip.
    let inbox = state
        .triage
        .pending_inbox(user.id())
        .await
        .map_err(|e| domain_error(&e))?;

    Ok(PullResultDto {
        added: u32::try_from(summary.added).unwrap_or(u32::MAX),
        modified: u32::try_from(summary.modified).unwrap_or(u32::MAX),
        removed: u32::try_from(summary.removed).unwrap_or(u32::MAX),
        pending_inbox_size: u32::try_from(inbox.len()).unwrap_or(u32::MAX),
    })
}

/// Fetch the triage inbox for the authenticated user (`SPEC §7`), gated by session
/// auth (`BUDGET-AUTH-GATE-1`).
///
/// Returns every settled, not-yet-categorized transaction (`status = 'settled'`
/// AND `category_id IS NULL`), oldest first. Plaid `pending` charges (`SPEC §4.4`)
/// carry `status = 'pending'` and are excluded by construction.
///
/// # Errors
///
/// `ServerFnError` (HTTP 401) when there is no valid session; HTTP 500 on any
/// persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn get_pending_inbox() -> Result<Vec<PendingRowDto>, dioxus::prelude::ServerFnError> {
    use crate::server_state::TriageState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = TriageState::extract().await?;

    let rows = state
        .triage
        .pending_inbox(user.id())
        .await
        .map_err(|e| domain_error(&e))?;

    Ok(rows
        .into_iter()
        .map(|p| PendingRowDto {
            id: p.id.to_string(),
            date: p.date.to_string(),
            amount: p.amount,
            description: p.description,
            account_id: p.account_id.map(|a| a.to_string()),
        })
        .collect())
}

/// List the authenticated user's funds (`SPEC §4.9`), gated by session auth
/// (`BUDGET-AUTH-GATE-1`).
///
/// The triage UI offers these as the fund target for the two fund-backed
/// treatments: `PayFromSavings` accepts any fund; `SpreadOverMonths`
/// (buffer-financing) requires a `buffer` fund (`is_buffer = true`). Returning all
/// funds with their kind lets the UI gate the picker. No money math runs here —
/// the `Money` balance crosses the wire as an exact Decimal (`BUDGET-MONEY-1`).
///
/// # Errors
///
/// `ServerFnError` (HTTP 401) when there is no valid session; HTTP 500 on any
/// persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn list_funds() -> Result<Vec<FundDto>, dioxus::prelude::ServerFnError> {
    use budget_domain::FundKind;

    use crate::server_state::TriageState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = TriageState::extract().await?;

    let funds = state
        .triage
        .list_funds(user.id())
        .await
        .map_err(|e| domain_error(&e))?;

    Ok(funds
        .into_iter()
        .map(|f| FundDto {
            id: f.id.to_string(),
            name: f.name,
            kind: match f.kind {
                FundKind::Buffer => "buffer".to_owned(),
                FundKind::Surplus => "surplus".to_owned(),
            },
            is_buffer: matches!(f.kind, FundKind::Buffer),
            balance: f.balance,
        })
        .collect())
}

/// Atomically triage one pending transaction (`SPEC §7`), gated by session auth
/// (`BUDGET-AUTH-GATE-1`).
///
/// Sets category + comment and applies exactly one treatment in ONE unit of work
/// (`SERVICE-TX-1`). The treatment is one of the three `SPEC §4.9` paths; each
/// counts the money exactly once (`BUDGET-NO-DOUBLE-CHARGE-1`). After success the
/// row has a category and leaves the inbox.
///
/// # Errors
///
/// `ServerFnError` (HTTP 401) when there is no valid session; HTTP 400 on a
/// malformed id / illegal treatment (e.g. a non-buffer fund for `SpreadOverMonths`,
/// or triaging an already-categorized or Plaid `pending` row); HTTP 500 on any
/// persistence failure.
#[allow(clippy::unused_async)]
#[server]
pub async fn triage_transaction(
    request: TriageRequestDto,
) -> Result<TriageResultDto, dioxus::prelude::ServerFnError> {
    use budget_app_services::{Treatment, TriageInput};
    use budget_domain::ids::{CategoryId, FundId, TransactionId};

    use crate::server_state::TriageState;
    use crate::services::gate::require_authed_user;

    let user = require_authed_user().await?;
    let state = TriageState::extract().await?;

    // Parse the wire ids into domain newtypes (a malformed id is a 400, never a
    // data reach).
    let transaction_id = TransactionId::new(parse_uuid(&request.transaction_id, "transaction_id")?);
    let category_id = CategoryId::new(parse_uuid(&request.category_id, "category_id")?);
    let treatment = match request.treatment {
        TreatmentDto::PayDirectly => Treatment::PayDirectly,
        TreatmentDto::PayFromSavings { fund_id } => Treatment::PayFromSavings {
            fund_id: FundId::new(parse_uuid(&fund_id, "fund_id")?),
        },
        TreatmentDto::SpreadOverMonths { fund_id, months } => {
            if months < 1 {
                return Err(bad_request("months must be at least 1"));
            }
            Treatment::SpreadOverMonths {
                fund_id: FundId::new(parse_uuid(&fund_id, "fund_id")?),
                months,
            }
        }
    };

    let input = TriageInput {
        transaction_id,
        category_id,
        comment: request.comment,
        treatment,
    };

    // Defense in depth (SPEC §9.1): confirm the transaction belongs to the
    // authenticated user before mutating it. The triage service loads the row;
    // verify ownership here against the inbox scope so a forged id for another
    // user's row cannot be triaged. (Single-user today, but the gate is unconditional.)
    let owned = state
        .triage
        .pending_inbox(user.id())
        .await
        .map_err(|e| domain_error(&e))?
        .into_iter()
        .any(|p| p.id == transaction_id);
    if !owned {
        return Err(bad_request(
            "transaction is not in your pending triage inbox",
        ));
    }

    let outcome = state
        .triage
        .triage(input, chrono::Utc::now())
        .await
        .map_err(|e| domain_error(&e))?;

    Ok(TriageResultDto {
        transaction_id: outcome.transaction_id.to_string(),
        obligation_id: outcome.obligation_id.map(|o| o.to_string()),
    })
}

// ---------------------------------------------------------------------------
// Helpers (server-only)
// ---------------------------------------------------------------------------

/// Today's calendar date in the home timezone (`America/New_York`, D2,
/// `ARCH-UTC-TIMESTAMPS-1`). The reconcile-window lower bound is a calendar date,
/// so it is resolved in the home zone, not UTC.
#[cfg(feature = "server")]
fn home_today() -> chrono::NaiveDate {
    use chrono::TimeZone;
    chrono_tz::America::New_York
        .from_utc_datetime(&chrono::Utc::now().naive_utc())
        .date_naive()
}

/// Parse a wire UUID string, mapping a malformed value to an opaque HTTP 400.
#[cfg(feature = "server")]
fn parse_uuid(raw: &str, field: &str) -> Result<uuid::Uuid, dioxus::prelude::ServerFnError> {
    uuid::Uuid::parse_str(raw).map_err(|_| bad_request(&format!("malformed {field}")))
}

/// Build an opaque HTTP 400 `ServerFnError` (client error — bad input).
#[cfg(feature = "server")]
fn bad_request(message: &str) -> dioxus::prelude::ServerFnError {
    dioxus::prelude::ServerFnError::ServerError {
        message: message.to_owned(),
        code: 400,
        details: None,
    }
}

/// Map a domain error to an HTTP 500 `ServerFnError`. The message carries the
/// error text for server logs; it reveals no user data and no secret.
#[cfg(feature = "server")]
fn domain_error(e: &budget_domain::DomainError) -> dioxus::prelude::ServerFnError {
    dioxus::prelude::ServerFnError::ServerError {
        message: e.to_string(),
        code: 500,
        details: None,
    }
}

/// Map a Plaid error to an HTTP 500 `ServerFnError` (server logs only; no secret
/// material is in a `PlaidError` variant by construction).
#[cfg(feature = "server")]
fn plaid_error(e: &budget_domain::plaid_api::PlaidError) -> dioxus::prelude::ServerFnError {
    dioxus::prelude::ServerFnError::ServerError {
        message: e.to_string(),
        code: 500,
        details: None,
    }
}
