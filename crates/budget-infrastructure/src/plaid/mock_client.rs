//! A local-dev mock of the Plaid [`PlaidApi`] port (`STAGE-1` local testing).
//!
//! This adapter lets Zach run the whole app LOCALLY with NO real Plaid / Neon /
//! Azure — fake bank data flows through the exact same domain ports, mapper, and
//! sync engine the production path uses, so the UI (Pull -> Pending -> triage,
//! the month ledger, fund math) can be shaken out before any deploy.
//!
//! ## OFF by default — opt-in only (`STAGE-1` safety)
//!
//! This type is selected ONLY by an explicit opt-in (`PLAID_MODE=mock`) in
//! [`crate`'s wiring](budget-ui's `server_state`). With the env var unset or set
//! to anything else, the real [`HttpPlaidApi`](super::HttpPlaidApi) + real Azure
//! Key Vault remain the default/production path. A misconfigured prod can never
//! silently fall through to this mock.
//!
//! ## Real-contract fidelity
//!
//! The fixture pages are stored as **actual Plaid `/transactions/sync` response
//! JSON** (matching Plaid's documented schema) and are deserialized through the
//! **same serde DTOs** the live [`HttpPlaidApi`](super::HttpPlaidApi) uses
//! ([`super::wire::TransactionsSyncResponse`]). This exercises the real
//! byte-level JSON -> DTO contract, not merely the Rust type. A test asserts the
//! fixtures deserialize and that the cursor walk yields the expected
//! added/modified/removed across pages.
//!
//! ## Cursor semantics (`SPEC §6`)
//!
//! Plaid `/transactions/sync` is cursor-based. The mock honors it faithfully:
//!
//! | incoming cursor                  | page served            | `next_cursor`              |
//! |----------------------------------|------------------------|----------------------------|
//! | `None` (first pull)              | settled + 1 pending    | `mock-cursor-after-page-1` |
//! | `mock-cursor-after-page-1`       | pending->settled + add | `mock-cursor-after-page-2` |
//! | `mock-cursor-after-page-2`       | a `removed` + an add   | `mock-cursor-after-page-3` |
//! | `mock-cursor-after-page-3` / any | empty steady-state     | `mock-cursor-after-page-3` |
//!
//! Each page returns `has_more = false`, so the sync engine's inner loop
//! terminates after one page; the cursor is persisted, and the NEXT manual Pull
//! advances to the next fixture. Repeated pulls past the end serve the empty
//! steady-state page (idempotent — the UI shows "nothing new").
//!
//! ## Realism that exercises the budget code
//!
//! - **Settled** grocery/gas/coffee/subscription/refund transactions.
//! - At least one Plaid **`pending`** transaction (must end up EXCLUDED from the
//!   budget, `SPEC §4.4`).
//! - A later page that **MODIFIES** the pending txn to settled
//!   (pending->settled, `pending_transaction_id` links them, `SPEC §6`).
//! - A **`removed`** example (`SPEC §6`, settlement reversal).
//! - Plaid's **sign convention** (positive amount = outflow/expense; the mapper
//!   flips it once, `BUDGET-PLAID-SIGN-1`) — including one negative-amount
//!   refund (an inflow).
//! - Real-looking `account_id`(s), `merchant_name`/`name`, ISO `date`s in a
//!   recent window, and `iso_currency_code`.

use async_trait::async_trait;

use budget_domain::plaid_api::{
    AccessTokenExchange, LinkToken, LinkTokenRequest, PlaidApi, PlaidError, PlaidSyncPage,
};

use super::wire::TransactionsSyncResponse;

/// The fixture cursors, kept as constants so the walk + the test agree on the
/// exact strings (a typo would otherwise silently serve the wrong page).
const CURSOR_AFTER_PAGE_1: &str = "mock-cursor-after-page-1";
const CURSOR_AFTER_PAGE_2: &str = "mock-cursor-after-page-2";

/// The deterministic mock access token. Any vault read in mock mode resolves to
/// this (the [`InMemorySecretVault`](super::InMemorySecretVault) hands it back);
/// it is a fake string and moves no money.
pub const MOCK_ACCESS_TOKEN: &str = "mock-access-token-local-dev";

/// The fixture pages, embedded at compile time so the mock needs no filesystem
/// access at runtime (works inside a container or a bare `dx serve`).
const PAGE_1_JSON: &str = include_str!("fixtures/sync_page_1.json");
const PAGE_2_JSON: &str = include_str!("fixtures/sync_page_2.json");
const PAGE_3_JSON: &str = include_str!("fixtures/sync_page_3.json");
const PAGE_EMPTY_JSON: &str = include_str!("fixtures/sync_page_empty.json");

/// A local-dev mock of the Plaid [`PlaidApi`] port.
///
/// Stateless: the cursor the caller passes selects the page (Plaid's own
/// contract), so multiple [`MockPlaidApi`] instances behave identically and no
/// interior mutability is needed.
#[derive(Debug, Default, Clone, Copy)]
pub struct MockPlaidApi;

impl MockPlaidApi {
    /// Build the mock. No configuration: the fixtures + cursor walk are fixed.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Select the fixture JSON for an incoming cursor (Plaid cursor semantics).
    fn fixture_for_cursor(cursor: Option<&str>) -> &'static str {
        match cursor {
            None => PAGE_1_JSON,
            Some(CURSOR_AFTER_PAGE_1) => PAGE_2_JSON,
            Some(CURSOR_AFTER_PAGE_2) => PAGE_3_JSON,
            // The terminal cursor and any unknown cursor serve the empty
            // steady-state page so repeated pulls are safe + idempotent.
            Some(_) => PAGE_EMPTY_JSON,
        }
    }

    /// Deserialize a fixture through the SAME wire DTOs the live client uses,
    /// then convert to the domain [`PlaidSyncPage`]. A malformed fixture is a
    /// programming error in this crate, surfaced as a mapping error (it can only
    /// trip if a fixture file is edited into invalid JSON).
    fn page_for_cursor(cursor: Option<&str>) -> Result<PlaidSyncPage, PlaidError> {
        let json = Self::fixture_for_cursor(cursor);
        let wire: TransactionsSyncResponse = serde_json::from_str(json)
            .map_err(|e| PlaidError::Mapping(format!("mock fixture decode: {e}")))?;
        Ok(wire.into())
    }
}

#[async_trait]
impl PlaidApi for MockPlaidApi {
    async fn create_link_token(&self, request: &LinkTokenRequest) -> Result<LinkToken, PlaidError> {
        // Honor the same money-movement guard the real client asserts (SPEC §6):
        // even the mock refuses a Transfer-scoped request.
        request.assert_no_money_movement()?;
        // A deterministic fake link token. The frontend Link widget would open
        // with this; in local dev the exchange is short-circuited to the fake
        // access token below.
        Ok(LinkToken("link-mock-local-dev".to_owned()))
    }

    async fn exchange_public_token(
        &self,
        _public_token: &str,
    ) -> Result<AccessTokenExchange, PlaidError> {
        // A deterministic fake access token + a stable mock item id. The token is
        // a fake string; it moves no money and never reaches a real Plaid call.
        Ok(AccessTokenExchange {
            access_token: MOCK_ACCESS_TOKEN.to_owned(),
            plaid_item_id: "mock-item-local-dev".to_owned(),
        })
    }

    async fn transactions_sync(
        &self,
        _access_token: &str,
        cursor: Option<&str>,
    ) -> Result<PlaidSyncPage, PlaidError> {
        // Serve the fixture page for this cursor; the access token is ignored
        // (any non-empty token is accepted, so the mock vault's token works).
        Self::page_for_cursor(cursor)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]

    use super::*;
    use budget_domain::ids::UserId;
    use budget_domain::plaid_api::PlaidProduct;
    use rust_decimal::Decimal;
    use uuid::Uuid;

    /// The fixtures must deserialize through the real wire DTOs (byte-level
    /// contract), and the cursor walk must yield the expected pages.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // comprehensive fixture walk; splitting would lose narrative
    async fn cursor_walk_yields_expected_pages_through_real_wire_dtos() {
        let api = MockPlaidApi::new();

        // --- Page 1: cursor None -> settled + one pending, two accounts. ------
        let p1 = api
            .transactions_sync(MOCK_ACCESS_TOKEN, None)
            .await
            .unwrap();
        assert_eq!(p1.added.len(), 4, "page 1 adds four transactions");
        assert!(p1.modified.is_empty());
        assert!(p1.removed.is_empty());
        assert_eq!(p1.accounts.len(), 2, "page 1 carries both accounts");
        assert_eq!(p1.next_cursor, CURSOR_AFTER_PAGE_1);
        assert!(
            !p1.has_more,
            "each pull serves a single self-contained page"
        );

        // Exactly one pending transaction (the restaurant) — it must be EXCLUDED
        // from the budget downstream (SPEC §4.4).
        let pending: Vec<_> = p1.added.iter().filter(|t| t.pending).collect();
        assert_eq!(pending.len(), 1, "one pending txn on page 1");
        assert_eq!(
            pending[0].transaction_id,
            "mock-txn-0004-restaurant-pending"
        );

        // Plaid sign convention preserved: the grocery outflow is positive, the
        // refund is negative (an inflow). The mapper flips this once downstream
        // (BUDGET-PLAID-SIGN-1); the port DTO keeps Plaid's native sign.
        let grocery = p1
            .added
            .iter()
            .find(|t| t.transaction_id == "mock-txn-0001-grocery")
            .unwrap();
        assert_eq!(grocery.amount, Decimal::new(8430, 2), "84.30 outflow > 0");
        let refund = p1
            .added
            .iter()
            .find(|t| t.transaction_id == "mock-txn-0003-refund")
            .unwrap();
        assert_eq!(refund.amount, Decimal::new(-2299, 2), "-22.99 inflow < 0");

        // --- Page 2: pending -> settled via `modified`, plus a new add. -------
        let p2 = api
            .transactions_sync(MOCK_ACCESS_TOKEN, Some(&p1.next_cursor))
            .await
            .unwrap();
        assert_eq!(p2.added.len(), 1, "page 2 adds the subscription");
        assert_eq!(
            p2.modified.len(),
            1,
            "page 2 settles the pending restaurant"
        );
        let settled = &p2.modified[0];
        assert!(!settled.pending, "the modified row is now settled");
        assert_eq!(
            settled.pending_transaction_id.as_deref(),
            Some("mock-txn-0004-restaurant-pending"),
            "the settled row links back to the original pending id (pending->settled)"
        );
        assert_eq!(p2.next_cursor, CURSOR_AFTER_PAGE_2);

        // --- Page 3: a `removed` example, the coffee add, and the D10 credit-
        //     card-payment scenario (SPEC §4.11 / BUDGET-TRANSFER-EXCLUDE-1).
        let p3 = api
            .transactions_sync(MOCK_ACCESS_TOKEN, Some(&p2.next_cursor))
            .await
            .unwrap();
        assert_eq!(p3.removed.len(), 1, "page 3 removes one transaction");
        assert_eq!(p3.removed[0], "mock-txn-0002-gas");
        assert_eq!(
            p3.added.len(),
            3,
            "page 3 adds coffee + both legs of the credit-card payment (D10)"
        );
        assert_eq!(p3.next_cursor, "mock-cursor-after-page-3");

        // D10 credit-card-payment legs: both carry plaid_category that drives
        // suggested_transfer=true in the triage inbox (BUDGET-TRANSFER-EXCLUDE-1).
        // - checking outflow: positive amount (Plaid outflow convention), non-pending,
        //   plaid_category = "LOAN_PAYMENTS_CREDIT_CARD_PAYMENT".
        // - card-side payment credit: negative amount (Plaid inflow convention),
        //   non-pending, same plaid_category.
        let cc_payment_checking = p3
            .added
            .iter()
            .find(|t| t.transaction_id == "mock-txn-0007-cc-payment-checking")
            .unwrap();
        assert_eq!(
            cc_payment_checking.amount,
            Decimal::new(50_000, 2),
            "checking card-payment outflow: positive Plaid amount (outflow convention)"
        );
        assert!(!cc_payment_checking.pending, "checking leg is settled");
        assert_eq!(
            cc_payment_checking.plaid_category.as_deref(),
            Some("LOAN_PAYMENTS_CREDIT_CARD_PAYMENT"),
            "checking leg plaid_category must match the D10 trigger value"
        );

        let cc_payment_credit = p3
            .added
            .iter()
            .find(|t| t.transaction_id == "mock-txn-0008-cc-payment-credit")
            .unwrap();
        assert_eq!(
            cc_payment_credit.amount,
            Decimal::new(-50_000, 2),
            "card-side payment credit: negative Plaid amount (inflow convention)"
        );
        assert!(
            !cc_payment_credit.pending,
            "card-payment credit leg is settled"
        );
        assert_eq!(
            cc_payment_credit.plaid_category.as_deref(),
            Some("LOAN_PAYMENTS_CREDIT_CARD_PAYMENT"),
            "card-side credit plaid_category must match the D10 trigger value"
        );

        // --- Terminal: any cursor past the end -> empty steady-state. ---------
        let p4 = api
            .transactions_sync(MOCK_ACCESS_TOKEN, Some(&p3.next_cursor))
            .await
            .unwrap();
        assert!(p4.added.is_empty() && p4.modified.is_empty() && p4.removed.is_empty());
        assert_eq!(
            p4.next_cursor, "mock-cursor-after-page-3",
            "terminal cursor is stable (idempotent repeated pulls)"
        );
    }

    #[tokio::test]
    async fn link_and_exchange_are_deterministic() {
        let api = MockPlaidApi::new();
        let user = UserId::new(Uuid::new_v4());

        let token = api
            .create_link_token(&LinkTokenRequest::transactions_only(user))
            .await
            .unwrap();
        assert_eq!(token.0, "link-mock-local-dev");

        let exchange = api.exchange_public_token("public-mock").await.unwrap();
        assert_eq!(exchange.access_token, MOCK_ACCESS_TOKEN);
        assert_eq!(exchange.plaid_item_id, "mock-item-local-dev");
    }

    #[tokio::test]
    async fn mock_refuses_money_movement_like_the_real_client() {
        let api = MockPlaidApi::new();
        let request = LinkTokenRequest {
            user_id: UserId::new(Uuid::new_v4()),
            products: vec![PlaidProduct::Transactions, PlaidProduct::Transfer],
        };
        let result = api.create_link_token(&request).await;
        assert!(
            matches!(result, Err(PlaidError::MoneyMovementProductRequested(_))),
            "the mock must refuse Transfer just like HttpPlaidApi"
        );
    }
}
