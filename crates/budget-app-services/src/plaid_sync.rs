//! The Plaid sync service (`SPEC §6`, build step 8).
//!
//! The use-case entry point for bank auto-pull. It orchestrates against three
//! domain ports — [`PlaidApi`] (raw Plaid HTTP), [`PlaidSyncEngine`] (the
//! cursor-sync/reconcile mechanics, implemented in `budget-infrastructure`), and
//! [`SecretVault`] (Key Vault) — plus the [`PlaidItemRepository`] and
//! [`UserRepository`]. It holds `Arc<dyn _>` collaborators (`SERVICE-DI-1`) and
//! contains no `db.*` (`ARCH-STRICT-LAYERING-1`); cross-aggregate writes route
//! through the [`UowProvider`] closure (`SERVICE-TX-1`).
//!
//! ## The three flows (`SPEC §6`)
//!
//! 1. **`create_link_token`** — open the Plaid Link widget. The request is
//!    Transactions(+Accounts) only; the Transfer product is never requested, so
//!    the eventual `access_token` is physically unable to move money
//!    (`BUDGET-PLAID-TOKEN-VAULT-1`).
//! 2. **`exchange_and_link`** — exchange the short-lived `public_token` for the
//!    long-lived `access_token`, write the token to Key Vault, and persist the
//!    [`PlaidItem`] with ONLY the Key Vault reference (`access_token_ref`). The
//!    raw token never touches the DB or a log (`BUDGET-PLAID-TOKEN-VAULT-1`).
//! 3. **`sync_user`** — for each linked item, fetch the token from the vault at
//!    call time and delegate the cursor sync + rolling 30-day reconcile to the
//!    engine. The genesis cutover guard (`BUDGET-CUTOVER-1`) is honored inside the
//!    engine, keyed on the user's `tracking_start_date`.

use std::sync::Arc;

use chrono::{NaiveDate, Utc};

use budget_domain::auth::SecretVault;
use budget_domain::ids::UserId;
use budget_domain::plaid_api::{
    AccessTokenExchange, LinkToken, LinkTokenRequest, PlaidApi, PlaidError, PlaidSyncEngine,
    SyncSummary,
};
use budget_domain::plaid_item::PlaidItem;
use budget_domain::repositories::{PlaidItemRepository, UserRepository};
use budget_domain::uow::{UnitOfWork, UowProvider, UowProviderExt};
use budget_domain::validated::AccessTokenRef;

/// Orchestrates the Plaid link + sync use cases (`SPEC §6`).
pub struct PlaidSyncService {
    plaid: Arc<dyn PlaidApi>,
    engine: Arc<dyn PlaidSyncEngine>,
    vault: Arc<dyn SecretVault>,
    plaid_items: Arc<dyn PlaidItemRepository>,
    users: Arc<dyn UserRepository>,
    uow: Arc<dyn UowProvider>,
}

impl PlaidSyncService {
    /// Wire the service from its collaborators (`SERVICE-DI-1`).
    #[must_use]
    pub fn new(
        plaid: Arc<dyn PlaidApi>,
        engine: Arc<dyn PlaidSyncEngine>,
        vault: Arc<dyn SecretVault>,
        plaid_items: Arc<dyn PlaidItemRepository>,
        users: Arc<dyn UserRepository>,
        uow: Arc<dyn UowProvider>,
    ) -> Self {
        Self {
            plaid,
            engine,
            vault,
            plaid_items,
            users,
            uow,
        }
    }

    /// The Key Vault secret name for a linked item's access token. Derived from
    /// the Plaid `item_id` so it is stable + unique per link; this name (NOT the
    /// token) is what the DB stores as `access_token_ref`
    /// (`BUDGET-PLAID-TOKEN-VAULT-1`).
    #[must_use]
    fn access_token_secret_name(plaid_item_id: &str) -> String {
        format!("plaid-access-token-{plaid_item_id}")
    }

    /// Create a Plaid Link token for the frontend widget (`SPEC §6`).
    ///
    /// Requests the Transactions(+Accounts) product only; the money-movement
    /// guard refuses anything else before any Plaid call
    /// (`BUDGET-PLAID-TOKEN-VAULT-1`).
    ///
    /// # Errors
    /// [`PlaidError`] on any Plaid/transport failure (or the money-movement
    /// guard, which cannot trip for `transactions_only`).
    pub async fn create_link_token(&self, user_id: UserId) -> Result<LinkToken, PlaidError> {
        let request = LinkTokenRequest::transactions_only(user_id);
        self.plaid.create_link_token(&request).await
    }

    /// Exchange a `public_token` and persist the link (`SPEC §6`).
    ///
    /// Steps:
    /// 1. Exchange the `public_token` for the `access_token`.
    /// 2. Write the `access_token` to Key Vault under a derived secret name.
    /// 3. Persist the [`PlaidItem`] with ONLY the Key Vault reference — never the
    ///    raw token (`BUDGET-PLAID-TOKEN-VAULT-1`).
    ///
    /// The raw token is dropped after the vault write; it never reaches the DB or
    /// a log.
    ///
    /// # Errors
    /// [`PlaidError::Api`] on the exchange, [`PlaidError::SecretVault`] on the
    /// vault write, [`PlaidError::Mapping`] if the derived reference is invalid,
    /// or [`PlaidError::Repository`] on the DB write.
    pub async fn exchange_and_link(
        &self,
        user_id: UserId,
        public_token: &str,
        institution_name: &str,
    ) -> Result<PlaidItem, PlaidError> {
        // 1. Exchange (the access_token is secret from here on).
        let AccessTokenExchange {
            access_token,
            plaid_item_id,
        } = self.plaid.exchange_public_token(public_token).await?;

        // 2. Write the token to the vault. The DB will only hold the NAME.
        let secret_name = Self::access_token_secret_name(&plaid_item_id);
        self.vault
            .set_secret(&secret_name, &access_token)
            .await
            .map_err(|e| PlaidError::SecretVault(e.to_string()))?;
        // Drop the plaintext token immediately; it must not flow further
        // (BUDGET-PLAID-TOKEN-VAULT-1).
        drop(access_token);

        // 3. Persist the item with ONLY the reference.
        let access_token_ref = AccessTokenRef::try_new(&secret_name)
            .map_err(|e| PlaidError::Mapping(e.to_string()))?;
        let now = Utc::now();
        let item = PlaidItem {
            id: budget_domain::ids::PlaidItemId::generate(),
            user_id,
            institution_name: institution_name.to_owned(),
            access_token_ref,
            sync_cursor: None,
            last_synced_at: None,
            created_at: now,
        };

        let saved = item.clone();
        let plaid_items = Arc::clone(&self.plaid_items);
        self.uow
            .run(move |uow: &dyn UnitOfWork| {
                Box::pin(async move {
                    plaid_items.save(&saved, Some(uow)).await?;
                    Ok(())
                })
            })
            .await?;
        Ok(item)
    }

    /// Run a full sync for every linked item of `user_id` (`SPEC §6`).
    ///
    /// For each item: fetch the access token from the vault at call time
    /// (`BUDGET-PLAID-TOKEN-VAULT-1`), then delegate the cursor sync + rolling
    /// 30-day reconcile to the engine, which honors the genesis cutover guard
    /// (`BUDGET-CUTOVER-1`) keyed on the user's `tracking_start_date`.
    ///
    /// `today` is supplied by the caller (resolved in the home timezone at the
    /// HTTP edge, D2) so the reconcile window clamp is deterministic + testable.
    ///
    /// # Errors
    /// [`PlaidError`] on any vault, Plaid, mapping, or persistence failure. A
    /// missing user surfaces as [`PlaidError::Mapping`].
    pub async fn sync_user(
        &self,
        user_id: UserId,
        today: NaiveDate,
    ) -> Result<SyncSummary, PlaidError> {
        let user = self
            .users
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| PlaidError::Mapping("user not found".to_owned()))?;
        let tracking_start_date = user.tracking_start_date;

        let items = self.plaid_items.list_for_user(user_id).await?;
        let mut summary = SyncSummary::default();
        for item in items {
            // Fetch the token from the vault by reference at call time; never
            // persisted raw (BUDGET-PLAID-TOKEN-VAULT-1).
            let access_token = self
                .vault
                .get_secret(item.access_token_ref.as_str())
                .await
                .map_err(|e| PlaidError::SecretVault(e.to_string()))?;
            let item_summary = self
                .engine
                .sync_item(item.id, user_id, &access_token, tracking_start_date, today)
                .await?;
            drop(access_token);
            summary.merge(item_summary);
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests;
