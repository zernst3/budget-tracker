//! Unit tests for [`PlaidSyncService`] (`SPEC §6`, build step 8).
//!
//! All collaborators are DB-free in-memory fakes; the [`PlaidApi`] +
//! [`PlaidSyncEngine`] are mocked, so NO live Plaid call runs (`SPEC §6`). The
//! load-bearing assertions:
//!   - `create_link_token` requests Transactions(+Accounts) only — the Transfer
//!     product is never enabled (`BUDGET-PLAID-TOKEN-VAULT-1`).
//!   - `exchange_and_link` writes the raw access token to the vault and persists
//!     ONLY the Key Vault reference; the raw token NEVER reaches the persisted
//!     `PlaidItem` (`BUDGET-PLAID-TOKEN-VAULT-1`).
//!   - `sync_user` fetches the token from the vault by reference at call time and
//!     passes the user's `tracking_start_date` to the engine (`BUDGET-CUTOVER-1`).

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};

use budget_domain::RepositoryError;
use budget_domain::account::Account;
use budget_domain::auth::{AuthError, SecretVault};
use budget_domain::ids::{AccountId, PlaidItemId, UserId};
use budget_domain::plaid_api::{
    AccessTokenExchange, LinkToken, LinkTokenRequest, PlaidApi, PlaidError, PlaidProduct,
    PlaidSyncEngine, PlaidSyncPage, SyncSummary,
};
use budget_domain::plaid_item::PlaidItem;
use budget_domain::repositories::{PlaidItemRepository, UserRepository};
use budget_domain::uow::{UnitOfWork, UowFuture, UowProvider};
use budget_domain::user::User;
use budget_domain::validated::Email;

use super::*;

// ---------------------------------------------------------------------------
// UoW fakes (mirror the fund-service test style)
// ---------------------------------------------------------------------------

struct FakeUow;
impl UnitOfWork for FakeUow {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

type BoxedUowClosure<'a> =
    Box<dyn for<'u> FnOnce(&'u dyn UnitOfWork) -> UowFuture<'u, Box<dyn Any + Send>> + Send + 'a>;

struct FakeUowProvider;

#[async_trait]
impl UowProvider for FakeUowProvider {
    async fn run_boxed(
        &self,
        f: BoxedUowClosure<'_>,
    ) -> Result<Box<dyn Any + Send>, RepositoryError> {
        let uow = FakeUow;
        let handle: &dyn UnitOfWork = &uow;
        f(handle).await
    }
}

fn poisoned<T>(_e: std::sync::PoisonError<T>) -> RepositoryError {
    RepositoryError::Database("test mutex poisoned".to_owned())
}

// ---------------------------------------------------------------------------
// Mock PlaidApi — records what was requested; never hits the network
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockPlaidApi {
    last_link_request: Mutex<Option<LinkTokenRequest>>,
    exchange_item_id: String,
    exchange_access_token: String,
}

#[async_trait]
impl PlaidApi for MockPlaidApi {
    async fn create_link_token(&self, request: &LinkTokenRequest) -> Result<LinkToken, PlaidError> {
        request.assert_no_money_movement()?;
        *self.last_link_request.lock().map_err(map_poison)? = Some(request.clone());
        Ok(LinkToken("link-sandbox-abc".to_owned()))
    }

    async fn exchange_public_token(
        &self,
        _public_token: &str,
    ) -> Result<AccessTokenExchange, PlaidError> {
        Ok(AccessTokenExchange {
            access_token: self.exchange_access_token.clone(),
            plaid_item_id: self.exchange_item_id.clone(),
        })
    }

    async fn transactions_sync(
        &self,
        _access_token: &str,
        _cursor: Option<&str>,
    ) -> Result<PlaidSyncPage, PlaidError> {
        Ok(PlaidSyncPage {
            added: vec![],
            modified: vec![],
            removed: vec![],
            accounts: vec![],
            next_cursor: "cursor-1".to_owned(),
            has_more: false,
        })
    }
}

fn map_poison<T>(_e: std::sync::PoisonError<T>) -> PlaidError {
    PlaidError::Api("test mutex poisoned".to_owned())
}

// ---------------------------------------------------------------------------
// Mock engine — records the tracking_start_date it was handed
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockEngine {
    seen_tracking_start: Mutex<Vec<NaiveDate>>,
    seen_token_nonempty: Mutex<Vec<bool>>,
}

#[async_trait]
impl PlaidSyncEngine for MockEngine {
    async fn sync_item(
        &self,
        _item_id: PlaidItemId,
        _user_id: UserId,
        access_token: &str,
        tracking_start_date: NaiveDate,
        _today: NaiveDate,
    ) -> Result<SyncSummary, PlaidError> {
        self.seen_tracking_start
            .lock()
            .map_err(map_poison)?
            .push(tracking_start_date);
        self.seen_token_nonempty
            .lock()
            .map_err(map_poison)?
            .push(!access_token.is_empty());
        Ok(SyncSummary {
            added: 2,
            ..SyncSummary::default()
        })
    }
}

// ---------------------------------------------------------------------------
// Fake vault — records writes; reads back what was written
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeVault {
    store: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl SecretVault for FakeVault {
    async fn get_secret(&self, name: &str) -> Result<String, AuthError> {
        self.store
            .lock()
            .map_err(|_| AuthError::SecretVault("poisoned".to_owned()))?
            .get(name)
            .cloned()
            .ok_or_else(|| AuthError::SecretVault("missing".to_owned()))
    }

    async fn set_secret(&self, name: &str, value: &str) -> Result<(), AuthError> {
        self.store
            .lock()
            .map_err(|_| AuthError::SecretVault("poisoned".to_owned()))?
            .insert(name.to_owned(), value.to_owned());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fake PlaidItemRepository — captures the saved item(s)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakePlaidItemRepo {
    items: Mutex<Vec<PlaidItem>>,
}

#[async_trait]
impl PlaidItemRepository for FakePlaidItemRepo {
    async fn find_by_id(&self, id: PlaidItemId) -> Result<Option<PlaidItem>, RepositoryError> {
        Ok(self
            .items
            .lock()
            .map_err(poisoned)?
            .iter()
            .find(|i| i.id == id)
            .cloned())
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<PlaidItem>, RepositoryError> {
        Ok(self
            .items
            .lock()
            .map_err(poisoned)?
            .iter()
            .filter(|i| i.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn get_sync_cursor(&self, _id: PlaidItemId) -> Result<Option<String>, RepositoryError> {
        Ok(None)
    }

    async fn update_sync_cursor(
        &self,
        _id: PlaidItemId,
        _cursor: &str,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn save(
        &self,
        item: &PlaidItem,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        self.items.lock().map_err(poisoned)?.push(item.clone());
        Ok(())
    }

    async fn list_accounts(&self, _user_id: UserId) -> Result<Vec<Account>, RepositoryError> {
        Ok(vec![])
    }

    async fn find_account(&self, _id: AccountId) -> Result<Option<Account>, RepositoryError> {
        Ok(None)
    }

    async fn find_account_by_plaid_id(
        &self,
        _plaid_account_id: &str,
    ) -> Result<Option<Account>, RepositoryError> {
        Ok(None)
    }

    async fn save_account(
        &self,
        _account: &Account,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fake UserRepository
// ---------------------------------------------------------------------------

struct FakeUserRepo {
    user: User,
}

#[async_trait]
impl UserRepository for FakeUserRepo {
    async fn find_by_id(&self, id: UserId) -> Result<Option<User>, RepositoryError> {
        Ok((self.user.id == id).then(|| self.user.clone()))
    }

    async fn find_by_email(&self, _email: &str) -> Result<Option<User>, RepositoryError> {
        Ok(None)
    }

    async fn save(
        &self,
        _user: &User,
        _uow: Option<&dyn UnitOfWork>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn sample_user(user_id: UserId, tracking_start: NaiveDate) -> User {
    User {
        id: user_id,
        email: Email::try_new("zach@example.com").unwrap(),
        password_hash: "hash".to_owned(),
        totp_secret: None,
        tracking_start_date: tracking_start,
        created_at: Utc::now(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_link_token_requests_transactions_only_never_transfer() {
    let plaid = Arc::new(MockPlaidApi::default());
    let engine = Arc::new(MockEngine::default());
    let vault = Arc::new(FakeVault::default());
    let items = Arc::new(FakePlaidItemRepo::default());
    let user_id = UserId::generate();
    let users = Arc::new(FakeUserRepo {
        user: sample_user(user_id, NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()),
    });
    let uow = Arc::new(FakeUowProvider);

    let service = PlaidSyncService::new(
        Arc::<MockPlaidApi>::clone(&plaid),
        engine,
        vault,
        items,
        users,
        uow,
    );

    let token = service.create_link_token(user_id).await.unwrap();
    assert_eq!(token.0, "link-sandbox-abc");

    // The request must be Transactions(+Accounts) only; Transfer never present.
    let recorded = plaid.last_link_request.lock().unwrap().clone().unwrap();
    assert!(
        recorded.assert_no_money_movement().is_ok(),
        "the link request must carry no money-movement product"
    );
    assert!(recorded.products.contains(&PlaidProduct::Transactions));
    assert!(recorded.products.contains(&PlaidProduct::Accounts));
    assert!(!recorded.products.contains(&PlaidProduct::Transfer));
}

#[tokio::test]
async fn exchange_and_link_stores_token_in_vault_and_only_a_reference_in_db() {
    let raw_token = "access-sandbox-SECRET-must-not-persist";
    let plaid = Arc::new(MockPlaidApi {
        exchange_item_id: "item-xyz".to_owned(),
        exchange_access_token: raw_token.to_owned(),
        ..MockPlaidApi::default()
    });
    let engine = Arc::new(MockEngine::default());
    let vault = Arc::new(FakeVault::default());
    let items = Arc::new(FakePlaidItemRepo::default());
    let user_id = UserId::generate();
    let users = Arc::new(FakeUserRepo {
        user: sample_user(user_id, NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()),
    });
    let uow = Arc::new(FakeUowProvider);

    let service = PlaidSyncService::new(
        plaid,
        engine,
        Arc::<FakeVault>::clone(&vault),
        Arc::<FakePlaidItemRepo>::clone(&items),
        users,
        uow,
    );

    let item = service
        .exchange_and_link(user_id, "public-token-123", "Bank of America")
        .await
        .unwrap();

    // The vault holds the RAW token under the derived name.
    let secret_name = item.access_token_ref.as_str().to_owned();
    let stored = vault.store.lock().unwrap();
    assert_eq!(
        stored.get(&secret_name).map(String::as_str),
        Some(raw_token),
        "the raw token must be written to the vault"
    );

    // The persisted item carries ONLY the reference — NEVER the raw token
    // (BUDGET-PLAID-TOKEN-VAULT-1).
    let persisted = items.items.lock().unwrap();
    assert_eq!(persisted.len(), 1);
    let stored_ref = persisted[0].access_token_ref.as_str();
    assert_eq!(stored_ref, secret_name);
    assert_ne!(
        stored_ref, raw_token,
        "the DB reference must NOT be the raw token"
    );
    assert!(
        !stored_ref.contains(raw_token),
        "the raw token must never appear in the stored reference"
    );
    // The reference is the derived, item-id-keyed secret name.
    assert_eq!(stored_ref, "plaid-access-token-item-xyz");
    assert_eq!(persisted[0].institution_name, "Bank of America");
    assert!(persisted[0].sync_cursor.is_none());
}

#[tokio::test]
async fn sync_user_fetches_token_from_vault_and_passes_tracking_start_to_engine() {
    let tracking_start = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
    let plaid = Arc::new(MockPlaidApi::default());
    let engine = Arc::new(MockEngine::default());
    let vault = Arc::new(FakeVault::default());
    let items = Arc::new(FakePlaidItemRepo::default());
    let user_id = UserId::generate();
    let users = Arc::new(FakeUserRepo {
        user: sample_user(user_id, tracking_start),
    });
    let uow = Arc::new(FakeUowProvider);

    // Pre-seed: one linked item + its token in the vault.
    let secret_name = "plaid-access-token-item-1";
    vault
        .set_secret(secret_name, "access-token-live")
        .await
        .unwrap();
    items
        .save(
            &PlaidItem {
                id: PlaidItemId::generate(),
                user_id,
                institution_name: "BoA".to_owned(),
                access_token_ref: budget_domain::validated::AccessTokenRef::try_new(secret_name)
                    .unwrap(),
                sync_cursor: None,
                last_synced_at: None,
                created_at: Utc::now(),
            },
            None,
        )
        .await
        .unwrap();

    let service = PlaidSyncService::new(
        plaid,
        Arc::<MockEngine>::clone(&engine),
        vault,
        items,
        users,
        uow,
    );

    let today = NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
    let summary = service.sync_user(user_id, today).await.unwrap();
    assert_eq!(summary.added, 2, "the engine's per-item summary aggregates");

    // The engine was handed the user's tracking_start_date (BUDGET-CUTOVER-1)
    // and a non-empty token read from the vault (BUDGET-PLAID-TOKEN-VAULT-1).
    assert_eq!(
        engine.seen_tracking_start.lock().unwrap().as_slice(),
        &[tracking_start]
    );
    assert_eq!(
        engine.seen_token_nonempty.lock().unwrap().as_slice(),
        &[true]
    );
}

#[tokio::test]
async fn sync_user_with_no_items_is_a_noop() {
    let plaid = Arc::new(MockPlaidApi::default());
    let engine = Arc::new(MockEngine::default());
    let vault = Arc::new(FakeVault::default());
    let items = Arc::new(FakePlaidItemRepo::default());
    let user_id = UserId::generate();
    let users = Arc::new(FakeUserRepo {
        user: sample_user(user_id, NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()),
    });
    let uow = Arc::new(FakeUowProvider);

    let service = PlaidSyncService::new(plaid, engine, vault, items, users, uow);
    let summary = service
        .sync_user(user_id, NaiveDate::from_ymd_opt(2026, 6, 8).unwrap())
        .await
        .unwrap();
    assert_eq!(summary, SyncSummary::default());
}
