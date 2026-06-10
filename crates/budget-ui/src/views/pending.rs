//! The PENDING TRIAGE screen + Pull button (`SPEC §7`) — FRONTEND-2.
//!
//! The transaction-intake screen. Newly-pulled SETTLED transactions land here
//! (`status = 'settled'` AND `category_id IS NULL`); the user assigns each one a
//! category, an optional comment, and exactly ONE treatment, then submits. A
//! successful submit triages the row atomically (`SERVICE-TX-1`) and the row
//! leaves the inbox.
//!
//! ## Structure (`SPEC §7`)
//!
//!   - A **"Pull" button** (top): calls the BACKEND-3 [`pull`] server function
//!     (the Plaid cursor sync, `SPEC §6`), then refreshes the inbox. It reports
//!     how many rows were added / modified / removed and the resulting inbox size.
//!     When Plaid is not configured on the deployment the server returns 503 and
//!     the button surfaces a clear message (the inbox still works over existing
//!     rows).
//!   - A **form-per-row triage list** of the Pending inbox ([`get_pending_inbox`]).
//!     Each row shows date / amount / description (read-only) and three inputs:
//!       * **Category** — a plain Dioxus `<select>` over the user's category names
//!         (resolved to a category id on submit). Required.
//!       * **Comment** — a free-text `<input>` (optional).
//!       * **Treatment** — a `<select>` choosing one of the three `SPEC §4.9`
//!         paths, with treatment-specific sub-inputs revealed conditionally:
//!           - **Pay directly** (DEFAULT) — a normal in-month expense; no fund.
//!           - **Pay from savings** — a fund DRAW; reveals a fund `<select>`
//!             (any fund).
//!           - **Spread over N months** — buffer-financed (D7); reveals a *buffer*
//!             fund `<select>` + a months `<input type=number min=1>`.
//!
//! ### Why a form-per-row, not a chorale editable table (documented decision)
//!
//! The prompt allows either a chorale editable `Table` or a plain Dioxus form.
//! Triage needs a treatment selector whose sub-fields (fund picker, months) appear
//! conditionally on the chosen treatment, plus a per-row submit. That conditional,
//! multi-field, validated-on-submit shape maps cleanly onto plain Dioxus `<select>`
//! / `<input>` elements; squeezing it into chorale's per-cell `EditorKind` (one
//! committed value per cell, no cross-cell conditional reveal) would fight the API.
//! The chorale in-table `EditorKind::Select` IS used on the ledger screen
//! (`views::ledger`) where the two editable fields are independent single-value
//! cells — the right tool there. Here the form is the cleaner fit.
//!
//! ## Treatment -> BACKEND-3 path mapping (`SPEC §4.9`)
//!
//! The UI [`TreatmentChoice`] maps to the wire [`TreatmentDto`] the
//! [`triage_transaction`] server function consumes, which the
//! `budget_app_services::TriageService` maps to its `Treatment` enum:
//!
//! | UI choice              | `TreatmentDto`                         | §4.9 path |
//! |------------------------|----------------------------------------|-----------|
//! | Pay directly (default) | `PayDirectly`                          | (c) normal in-month expense — counts once |
//! | Pay from savings       | `PayFromSavings { fund_id }`           | (a) fund DRAW (`is_fund_draw = true`, excluded) |
//! | Spread over N months   | `SpreadOverMonths { fund_id, months }` | (b) buffer-financed -> `repayment_obligation` (D7) |
//!
//! ## Money representation (`BUDGET-MONEY-1`)
//!
//! Every amount on the DTOs is [`budget_domain::Money`] (Decimal-backed, exact).
//! This screen does NO money math; it only formats balances/amounts for display
//! via [`crate::views::ledger::fmt_currency`] (which operates on the exact
//! `Decimal`, no float). No `f64` appears in this module.

use std::collections::HashMap;

use dioxus::prelude::*;

use crate::Route;
use crate::components::NavBar;
use crate::services::{
    EnvelopeCategoryDto, FundDto, PendingRowDto, TreatmentDto, TriageRequestDto,
    get_envelope_summary, get_pending_inbox, list_funds, logout, pull, triage_transaction,
};
use crate::views::ledger::fmt_currency;

// ---------------------------------------------------------------------------
// Per-row draft state
// ---------------------------------------------------------------------------

/// Which of the three `SPEC §4.9` treatments the user has chosen for a row. The
/// fund id + months live separately in the draft so switching treatment does not
/// lose a partially-filled value.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum TreatmentChoice {
    /// (c) Pay directly through the budget — the DEFAULT / most common case.
    #[default]
    PayDirectly,
    /// (a) Pay from a savings/surplus fund accrual — a fund draw.
    PayFromSavings,
    /// (b) Spread over the next few months — buffer-financed (D7).
    SpreadOverMonths,
    /// (d) Transfer / card payment — NOT an expense (`SPEC §4.11` D10). Sets
    /// `is_transfer = true`; requires no category and no fund.
    Transfer,
}

impl TreatmentChoice {
    /// Parse the `<select>` value.
    fn from_value(v: &str) -> Self {
        match v {
            "savings" => Self::PayFromSavings,
            "spread" => Self::SpreadOverMonths,
            "transfer" => Self::Transfer,
            _ => Self::PayDirectly,
        }
    }

    /// The `<select>` option value for this choice.
    fn as_value(self) -> &'static str {
        match self {
            Self::PayDirectly => "direct",
            Self::PayFromSavings => "savings",
            Self::SpreadOverMonths => "spread",
            Self::Transfer => "transfer",
        }
    }
}

/// The editable draft for one pending row, held in the per-screen draft map keyed
/// by transaction id. All fields start empty/default; submit validates them.
#[derive(Clone, Default)]
struct RowDraft {
    /// Selected category id (empty until the user picks one — required on submit).
    category_id: String,
    /// Free-text comment (optional).
    comment: String,
    /// The chosen treatment.
    treatment: TreatmentChoice,
    /// Selected fund id (used by `PayFromSavings` + `SpreadOverMonths`).
    fund_id: String,
    /// Months for `SpreadOverMonths` (string-held; parsed + validated on submit).
    months: String,
    /// A submit-time error for this row, shown inline (`None` = no error).
    error: Option<String>,
    /// `true` while this row's submit is in flight (disables its button).
    submitting: bool,
}

// ---------------------------------------------------------------------------
// The Pending triage screen
// ---------------------------------------------------------------------------

/// The Pending triage inbox + Pull button (`SPEC §7`).
///
/// ### Manual QA notes (what Zach should see + click)
///
/// 1. On load the page fetches the Pending inbox, the user's funds, and the
///    category list (the current month's envelope summary supplies category
///    id+name). All three are `AuthedUser`-gated server calls.
/// 2. **Pull button** (top right): click it -> it calls the Plaid cursor sync,
///    then refetches the inbox. A status line reports `added / modified / removed`
///    and the new inbox size. If bank sync is not configured on the deployment
///    (no Plaid creds / vault) the server returns 503 and the status line says so
///    plainly; the inbox below still works over any existing rows.
/// 3. **Inbox**: one card per pending (settled, uncategorized) transaction showing
///    Date / Amount / Description (read-only). An empty inbox shows "Nothing to
///    triage."
/// 4. **Category** (`<select>`, required): the user's category names. Submitting
///    without a category shows an inline "Pick a category" error.
/// 5. **Comment** (`<input>`, optional): free text stored on `transactions.comment`.
/// 6. **Treatment** (`<select>`): "Pay directly" (default) / "Pay from savings" /
///    "Spread over N months".
///    - Pay directly: no extra fields.
///    - Pay from savings: a fund `<select>` (all funds; balance shown) appears.
///    - Spread over N months: a *buffer-only* fund `<select>` + a months
///      `<input type=number min=1>` appear. Submitting with months < 1 or no fund
///      shows an inline error.
/// 7. **Triage**: click "Triage" on a row -> it calls `triage_transaction`
///    atomically. On success the row disappears from the inbox (it now has a
///    category and is in the ledger). On failure the row shows an inline error and
///    stays.
/// 8. **Nav**: "Back to ledger" returns to the month ledger; "Sign out" ends the
///    session.
/// 9. Money: inspect the network JSON — every amount/balance is an exact decimal
///    string, no float (`BUDGET-MONEY-1`).
///
/// TODO(visual-polish): card spacing, colour-coding amounts, a per-row success
/// toast. Marked for the manual-QA pass.
#[component]
#[must_use]
pub fn PendingView() -> Element {
    // The drafts map: transaction id -> the row's editable state. Persists across
    // re-renders so a partially-filled row keeps its inputs while others submit.
    let drafts = use_signal(HashMap::<String, RowDraft>::new);

    // A monotonically-incrementing refresh token: bumping it re-runs the inbox
    // fetch (after a Pull or a successful triage).
    let mut refresh = use_signal(|| 0_u32);

    // Pull status line (None = no pull yet).
    let pull_status = use_signal(|| Option::<Result<String, String>>::None);

    // Fetches: inbox (re-runs on refresh), funds, categories.
    let inbox = use_resource(move || {
        let _ = refresh();
        async move { get_pending_inbox().await }
    });
    let funds = use_resource(move || async move { list_funds().await });
    let categories = use_resource(move || async move {
        let (y, m) = current_ym();
        get_envelope_summary(y, m).await
    });

    let page_nav = use_navigator();
    let on_signout = move |()| {
        spawn(async move {
            let _ = logout().await;
            page_nav.push(Route::Login {});
        });
    };

    // The Pull handler: sync, set a status line, then refresh the inbox.
    let mut pull_status_w = pull_status;
    let on_pull = move |_| {
        spawn(async move {
            pull_status_w.set(Some(Ok("Pulling…".to_owned())));
            match pull().await {
                Ok(r) => {
                    pull_status_w.set(Some(Ok(format!(
                        "Pulled: {} added, {} modified, {} removed. {} awaiting triage.",
                        r.added, r.modified, r.removed, r.pending_inbox_size
                    ))));
                    refresh.set(refresh() + 1);
                }
                Err(e) => {
                    pull_status_w.set(Some(Err(format!("Pull failed: {e}"))));
                }
            }
        });
    };

    // Derive the category + fund option lists once per render.
    let category_opts: Vec<EnvelopeCategoryDto> = match &*categories.read() {
        Some(Ok(s)) => s.categories.clone(),
        _ => vec![],
    };
    let fund_opts: Vec<FundDto> = match &*funds.read() {
        Some(Ok(f)) => f.clone(),
        _ => vec![],
    };

    rsx! {
        div { class: "app-shell",
            // Shared nav bar (RUST-DIOXUS-14 — NavBar is the canonical primitive)
            NavBar { on_signout }

            main { class: "page-content",
                h1 { class: "page-title", "Pending triage" }

                // -- Pull button + status --
                div { class: "pull-bar",
                    button {
                        class: "pull-btn",
                        onclick: on_pull,
                        "Pull from bank"
                    }
                    {
                        match &*pull_status.read() {
                            None => rsx! {
                                span { class: "pull-status",
                                    "Pull ingests new settled transactions into the inbox." }
                            },
                            Some(Ok(msg)) => rsx! {
                                span { class: "pull-status pull-status--ok", role: "status", "{msg}" }
                            },
                            Some(Err(msg)) => rsx! {
                                span { class: "pull-status pull-status--err", role: "alert", "{msg}" }
                            },
                        }
                    }
                }

                // -- The inbox --
                {
                    match &*inbox.read() {
                        None => rsx! { p { class: "loading-text", "Loading inbox…" } },
                        Some(Err(e)) => rsx! {
                            p { class: "text-error", role: "alert", "Error loading inbox: {e}" }
                            Link { to: Route::Login {}, "Return to login" }
                        },
                        Some(Ok(rows)) if rows.is_empty() => rsx! {
                            p { class: "text-muted",
                                "Nothing to triage. Pull to ingest new transactions." }
                        },
                        Some(Ok(rows)) => rsx! {
                            div { class: "inbox",
                                for row in rows.iter() {
                                    PendingRowCard {
                                        key: "{row.id}",
                                        row: row.clone(),
                                        categories: category_opts.clone(),
                                        funds: fund_opts.clone(),
                                        drafts,
                                        refresh,
                                    }
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// One pending-row triage card
// ---------------------------------------------------------------------------

#[component]
fn PendingRowCard(
    row: PendingRowDto,
    categories: Vec<EnvelopeCategoryDto>,
    funds: Vec<FundDto>,
    /// The shared draft map (keyed by transaction id).
    drafts: Signal<HashMap<String, RowDraft>>,
    /// The inbox refresh token; bumped after a successful triage so the row drops
    /// out of the refetched inbox.
    refresh: Signal<u32>,
) -> Element {
    let txn_id = row.id.clone();

    // Pull this row's current draft (default if not yet touched). SPEC §4.11 D10:
    // when Plaid's category suggests a transfer AND the user has not yet touched this
    // row, PRE-SELECT the Transfer treatment for them to confirm. This is a UI
    // suggestion only — the server never auto-sets is_transfer; the user confirms (or
    // switches the treatment) and the actual mutation happens on Triage.
    let draft = drafts
        .read()
        .get(&txn_id)
        .cloned()
        .unwrap_or_else(|| RowDraft {
            treatment: if row.suggested_transfer {
                TreatmentChoice::Transfer
            } else {
                TreatmentChoice::PayDirectly
            },
            ..Default::default()
        });
    let treatment = draft.treatment;

    // -- Input handlers (each mutates this row's draft entry in place) --
    let mut drafts_w = drafts;
    let id_cat = txn_id.clone();
    let on_category = move |e: FormEvent| {
        let mut map = drafts_w.write();
        map.entry(id_cat.clone()).or_default().category_id = e.value();
    };
    let id_com = txn_id.clone();
    let on_comment = move |e: FormEvent| {
        let mut map = drafts_w.write();
        map.entry(id_com.clone()).or_default().comment = e.value();
    };
    let id_treat = txn_id.clone();
    let on_treatment = move |e: FormEvent| {
        let mut map = drafts_w.write();
        map.entry(id_treat.clone()).or_default().treatment =
            TreatmentChoice::from_value(&e.value());
    };
    let id_fund = txn_id.clone();
    let on_fund = move |e: FormEvent| {
        let mut map = drafts_w.write();
        map.entry(id_fund.clone()).or_default().fund_id = e.value();
    };
    let id_months = txn_id.clone();
    let on_months = move |e: FormEvent| {
        let mut map = drafts_w.write();
        map.entry(id_months.clone()).or_default().months = e.value();
    };

    // -- Submit: validate the draft -> map to TreatmentDto -> triage_transaction --
    let id_submit = txn_id.clone();
    let mut refresh_w = refresh;
    let on_submit = move |_| {
        let id = id_submit.clone();
        spawn(async move {
            // Read + validate the current draft.
            let current = drafts_w.read().get(&id).cloned().unwrap_or_default();
            let Some((category_id, treatment_dto, comment)) = validate_draft(&current) else {
                // validate_draft set the error already.
                let mut map = drafts_w.write();
                let d = map.entry(id.clone()).or_default();
                if d.error.is_none() {
                    d.error = Some("Please complete the row.".to_owned());
                }
                return;
            };

            // Mark in flight + clear any prior error.
            {
                let mut map = drafts_w.write();
                let d = map.entry(id.clone()).or_default();
                d.submitting = true;
                d.error = None;
            }

            let req = TriageRequestDto {
                transaction_id: id.clone(),
                category_id,
                comment,
                treatment: treatment_dto,
            };
            match triage_transaction(req).await {
                Ok(_) => {
                    // Drop the draft and refresh the inbox (the row leaves it).
                    drafts_w.write().remove(&id);
                    refresh_w.set(refresh_w() + 1);
                }
                Err(e) => {
                    let mut map = drafts_w.write();
                    let d = map.entry(id.clone()).or_default();
                    d.submitting = false;
                    d.error = Some(format!("Triage failed: {e}"));
                }
            }
        });
    };

    // The amount on a pending row is an expense (negative). Apply the negative-amount
    // class so it is visually distinct (red).
    let amount_class = if row.amount.is_negative() {
        "inbox-card__amount amount--negative"
    } else {
        "inbox-card__amount"
    };

    rsx! {
        div { class: "inbox-card",
            // -- Read-only header: date / amount / description --
            div { class: "inbox-card__header",
                span { class: "inbox-card__date", "{row.date}" }
                span { class: "{amount_class}", "{fmt_currency(row.amount)}" }
                span { class: "inbox-card__desc", "{row.description}" }
            }

            // -- Editable controls --
            div { class: "inbox-card__fields",

                // Category — required for the expense treatments, hidden for Transfer
                // (SPEC §4.11 D10: a transfer carries no category; hiding the
                // dead-end affordance per PROC-HIDE-DEAD-END-1).
                if treatment != TreatmentChoice::Transfer {
                    label { class: "field-label",
                        "Category"
                        select {
                            style: "min-width: 150px;",
                            value: "{draft.category_id}",
                            onchange: on_category,
                            option { value: "", "— choose —" }
                            for cat in categories.iter() {
                                option { value: "{cat.id}", "{cat.name}" }
                            }
                        }
                    }
                }

                // Comment (optional).
                label { class: "field-label",
                    "Comment"
                    input {
                        style: "min-width: 180px;",
                        r#type: "text",
                        value: "{draft.comment}",
                        oninput: on_comment,
                        placeholder: "optional note",
                    }
                }

                // Treatment.
                label { class: "field-label",
                    "Treatment"
                    select {
                        style: "min-width: 170px;",
                        value: "{treatment.as_value()}",
                        onchange: on_treatment,
                        option { value: "direct", "Pay directly (default)" }
                        option { value: "savings", "Pay from savings" }
                        option { value: "spread", "Spread over months" }
                        option { value: "transfer", "Transfer / card payment (not an expense)" }
                    }
                    // SPEC §4.11 D10 / BUDGET-TRANSFER-EXCLUDE-1: show a hint when
                    // Plaid's category indicates a card payment or account transfer. The
                    // hint appears only when the treatment is currently Transfer AND the
                    // row carries a server-computed suggestion — it confirms WHY this
                    // treatment is pre-selected without forcing the user to accept it.
                    // Never auto-submitted; the user still clicks "Triage" to confirm.
                    if treatment == TreatmentChoice::Transfer && row.suggested_transfer {
                        span { class: "transfer-hint",
                            "Plaid: looks like a card payment" }
                    }
                }

                // Treatment-specific sub-fields.
                if treatment == TreatmentChoice::PayFromSavings {
                    label { class: "field-label",
                        "Fund"
                        select {
                            style: "min-width: 170px;",
                            value: "{draft.fund_id}",
                            onchange: on_fund.clone(),
                            option { value: "", "— choose fund —" }
                            for f in funds.iter() {
                                option { value: "{f.id}",
                                    "{f.name} ({f.kind}, {fmt_currency(f.balance)})" }
                            }
                        }
                    }
                }
                if treatment == TreatmentChoice::SpreadOverMonths {
                    label { class: "field-label",
                        "Buffer fund"
                        select {
                            style: "min-width: 170px;",
                            value: "{draft.fund_id}",
                            onchange: on_fund,
                            option { value: "", "— choose buffer —" }
                            // Only buffer funds are valid for buffer-financing (D7);
                            // the server re-validates, but we hide the dead-end choice.
                            for f in funds.iter().filter(|f| f.is_buffer) {
                                option { value: "{f.id}",
                                    "{f.name} ({fmt_currency(f.balance)})" }
                            }
                        }
                    }
                    label { class: "field-label",
                        "Months"
                        input {
                            style: "width: 80px;",
                            r#type: "number",
                            min: "1",
                            value: "{draft.months}",
                            oninput: on_months,
                            placeholder: "e.g. 3",
                        }
                    }
                }

                // Submit.
                button {
                    class: "triage-btn",
                    disabled: draft.submitting,
                    onclick: on_submit,
                    if draft.submitting { "Triaging…" } else { "Triage" }
                }
            }

            // -- Inline error --
            if let Some(err) = &draft.error {
                p { class: "inline-error", role: "alert", "{err}" }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Validation: draft -> (category_id, TreatmentDto, comment) or set the error
// ---------------------------------------------------------------------------

/// Validate a row draft and, on success, return the pieces of the triage request.
///
/// On failure it returns `None` and writes the inline `error` is the CALLER's job
/// (this is a pure function of the draft); the caller reads the returned `None` and
/// surfaces the message it already set, OR a generic fallback. We keep validation
/// pure (no `Signal` access) so it is trivially testable.
///
/// Rules (`SPEC §4.9` / `§4.11` / the BACKEND-3 contract):
///   - category is required for the three EXPENSE treatments (it is what removes the
///     row from the inbox), and NOT required for `Transfer` (which removes the row
///     via `is_transfer = true`, `SPEC §4.11` D10);
///   - `PayFromSavings` needs a fund;
///   - `SpreadOverMonths` needs a (buffer) fund AND `months >= 1`;
///   - `PayDirectly` needs nothing extra;
///   - `Transfer` needs nothing extra (no category, no fund).
///
/// The empty-string comment normalizes to `None` (no note), non-empty to `Some`. The
/// returned category is `Some(id)` for an expense treatment and `None` for `Transfer`.
fn validate_draft(draft: &RowDraft) -> Option<(Option<String>, TreatmentDto, Option<String>)> {
    let comment = if draft.comment.trim().is_empty() {
        None
    } else {
        Some(draft.comment.clone())
    };
    let treatment = match draft.treatment {
        TreatmentChoice::PayDirectly => TreatmentDto::PayDirectly,
        TreatmentChoice::PayFromSavings => {
            if draft.fund_id.is_empty() {
                return None;
            }
            TreatmentDto::PayFromSavings {
                fund_id: draft.fund_id.clone(),
            }
        }
        TreatmentChoice::SpreadOverMonths => {
            if draft.fund_id.is_empty() {
                return None;
            }
            let months: i32 = draft.months.trim().parse().ok()?;
            if months < 1 {
                return None;
            }
            TreatmentDto::SpreadOverMonths {
                fund_id: draft.fund_id.clone(),
                months,
            }
        }
        // SPEC §4.11 D10: Transfer requires neither a category nor a fund.
        TreatmentChoice::Transfer => TreatmentDto::Transfer,
    };
    // Category is required for the expense treatments only.
    let category_id = if matches!(treatment, TreatmentDto::Transfer) {
        None
    } else {
        if draft.category_id.is_empty() {
            return None;
        }
        Some(draft.category_id.clone())
    };
    Some((category_id, treatment, comment))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Current year + month (home zone is not load-bearing here — the category list is
/// budget-version scoped, and the active version is the same across the current
/// month regardless of a one-day zone skew). Display-only, no money math.
fn current_ym() -> (i32, i32) {
    use chrono::{Datelike, Utc};
    let now = Utc::now();
    let month = i32::try_from(now.month()).unwrap_or(1);
    (now.year(), month)
}

// ---------------------------------------------------------------------------
// Tests — the pure validation core (no Signal / DB plumbing)
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    // Tests assert on resolved values; `expect` on a known-valid draft is the
    // established test pattern in this crate (see `services::ledger` tests).
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{RowDraft, TreatmentChoice, TreatmentDto, validate_draft};

    #[test]
    fn category_is_required() {
        let d = RowDraft::default();
        assert!(validate_draft(&d).is_none(), "no category -> invalid");
    }

    #[test]
    fn pay_directly_needs_only_a_category() {
        let d = RowDraft {
            category_id: "cat-1".to_owned(),
            ..Default::default()
        };
        let (cat, treatment, comment) = validate_draft(&d).expect("valid");
        assert_eq!(cat.as_deref(), Some("cat-1"));
        assert_eq!(treatment, TreatmentDto::PayDirectly);
        assert_eq!(comment, None);
    }

    #[test]
    fn transfer_needs_no_category_or_fund() {
        // SPEC §4.11 D10: the Transfer treatment validates with NO category and NO
        // fund — it removes the row via is_transfer=true, not a category. The
        // returned category is None.
        let d = RowDraft {
            treatment: TreatmentChoice::Transfer,
            ..Default::default()
        };
        let (cat, treatment, _) = validate_draft(&d).expect("transfer with no category is valid");
        assert_eq!(cat, None, "Transfer carries no category");
        assert_eq!(treatment, TreatmentDto::Transfer);
    }

    #[test]
    fn comment_normalizes_blank_to_none() {
        let d = RowDraft {
            category_id: "cat-1".to_owned(),
            comment: "   ".to_owned(),
            ..Default::default()
        };
        let (_, _, comment) = validate_draft(&d).expect("valid");
        assert_eq!(comment, None);
        let d2 = RowDraft {
            category_id: "cat-1".to_owned(),
            comment: "groceries run".to_owned(),
            ..Default::default()
        };
        let (_, _, comment2) = validate_draft(&d2).expect("valid");
        assert_eq!(comment2.as_deref(), Some("groceries run"));
    }

    #[test]
    fn pay_from_savings_requires_a_fund() {
        let no_fund = RowDraft {
            category_id: "cat-1".to_owned(),
            treatment: TreatmentChoice::PayFromSavings,
            ..Default::default()
        };
        assert!(validate_draft(&no_fund).is_none());

        let ok = RowDraft {
            category_id: "cat-1".to_owned(),
            treatment: TreatmentChoice::PayFromSavings,
            fund_id: "fund-1".to_owned(),
            ..Default::default()
        };
        let (_, treatment, _) = validate_draft(&ok).expect("valid");
        assert_eq!(
            treatment,
            TreatmentDto::PayFromSavings {
                fund_id: "fund-1".to_owned()
            }
        );
    }

    #[test]
    fn spread_requires_fund_and_positive_months() {
        let base = RowDraft {
            category_id: "cat-1".to_owned(),
            treatment: TreatmentChoice::SpreadOverMonths,
            fund_id: "buf-1".to_owned(),
            ..Default::default()
        };

        // Missing months.
        assert!(validate_draft(&base).is_none(), "no months -> invalid");

        // Zero months.
        let zero = RowDraft {
            months: "0".to_owned(),
            ..base.clone()
        };
        assert!(validate_draft(&zero).is_none(), "months < 1 -> invalid");

        // Non-numeric months.
        let junk = RowDraft {
            months: "soon".to_owned(),
            ..base.clone()
        };
        assert!(validate_draft(&junk).is_none(), "non-numeric -> invalid");

        // No fund.
        let no_fund = RowDraft {
            fund_id: String::new(),
            months: "3".to_owned(),
            ..base.clone()
        };
        assert!(validate_draft(&no_fund).is_none(), "no fund -> invalid");

        // Valid.
        let ok = RowDraft {
            months: "3".to_owned(),
            ..base
        };
        let (_, treatment, _) = validate_draft(&ok).expect("valid");
        assert_eq!(
            treatment,
            TreatmentDto::SpreadOverMonths {
                fund_id: "buf-1".to_owned(),
                months: 3
            }
        );
    }
}
