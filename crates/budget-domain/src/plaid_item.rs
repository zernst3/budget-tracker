//! The [`PlaidItem`] aggregate — one linked institution (`SPEC §6`).
//!
//! The Plaid `access_token` is NEVER stored raw: [`PlaidItem::access_token_ref`]
//! holds a Key Vault secret reference only (`BUDGET-PLAID-TOKEN-VAULT-1`), typed
//! as [`AccessTokenRef`] so a raw token cannot be assigned by mistake. The
//! incremental `/transactions/sync` cursor lives here (`SPEC §6`).

use chrono::{DateTime, Utc};

use crate::ids::{PlaidItemId, UserId};
use crate::validated::AccessTokenRef;

/// A linked Plaid institution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaidItem {
    /// Stable identity.
    pub id: PlaidItemId,
    /// Owning user.
    pub user_id: UserId,
    /// Institution display name (e.g. "Bank of America"). Free-form.
    pub institution_name: String,
    /// Key Vault secret reference — NEVER the raw token (`BUDGET-PLAID-TOKEN-VAULT-1`).
    pub access_token_ref: AccessTokenRef,
    /// Plaid cursor for incremental `/transactions/sync` pulls; `None` before the
    /// first sync.
    pub sync_cursor: Option<String>,
    /// When the last sync completed (UTC, `DOMAIN-7`); `None` before the first sync.
    pub last_synced_at: Option<DateTime<Utc>>,
    /// When the item was linked (UTC, `DOMAIN-7`).
    pub created_at: DateTime<Utc>,
}
