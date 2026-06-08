//! Mapper: `budget-entities::plaid_items::Model` ↔ `budget-domain::plaid_item::PlaidItem`.
//!
//! Conversions:
//!   - `id / user_id`: `Uuid` → typed IDs (`DOMAIN-2`)
//!   - `access_token_ref`: `String` → `AccessTokenRef::try_new` (fallible — `BUDGET-PLAID-TOKEN-VAULT-1`)
//!   - `last_synced_at / created_at`: `Option<DateTimeWithTimeZone>` / `DateTimeWithTimeZone`
//!     → `Option<DateTime<Utc>>` / `DateTime<Utc>` via `.with_timezone(&Utc)` (`DOMAIN-7`)

use chrono::Utc;
use sea_orm::ActiveValue::Set;

use budget_domain::ids::{PlaidItemId, UserId};
use budget_domain::plaid_item::PlaidItem;
use budget_domain::validated::AccessTokenRef;

use budget_entities::plaid_items;

use crate::MapperError;

/// Translate a `plaid_items` [`plaid_items::Model`] into a domain [`PlaidItem`].
///
/// # Errors
/// Returns [`MapperError::InvalidStoredValue`] if the stored `access_token_ref`
/// fails [`AccessTokenRef::try_new`] (blank value indicates data corruption).
pub fn model_to_domain(m: plaid_items::Model) -> Result<PlaidItem, MapperError> {
    let access_token_ref = AccessTokenRef::try_new(&m.access_token_ref).map_err(|e| {
        MapperError::InvalidStoredValue {
            field: "access_token_ref",
            reason: e.to_string(),
        }
    })?;

    Ok(PlaidItem {
        id: PlaidItemId::new(m.id),
        user_id: UserId::new(m.user_id),
        institution_name: m.institution_name,
        access_token_ref,
        sync_cursor: m.sync_cursor,
        last_synced_at: m.last_synced_at.map(|dt| dt.with_timezone(&Utc)),
        created_at: m.created_at.with_timezone(&Utc),
    })
}

/// Translate a domain [`PlaidItem`] into a `plaid_items` [`plaid_items::ActiveModel`].
#[must_use]
pub fn domain_to_active_model(v: &PlaidItem) -> plaid_items::ActiveModel {
    plaid_items::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        institution_name: Set(v.institution_name.clone()),
        access_token_ref: Set(v.access_token_ref.as_str().to_owned()),
        sync_cursor: Set(v.sync_cursor.clone()),
        last_synced_at: Set(v.last_synced_at.map(Into::into)),
        created_at: Set(v.created_at.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn sample_model() -> plaid_items::Model {
        plaid_items::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            institution_name: "Bank of America".to_owned(),
            access_token_ref: "kv://plaid/boa-item-1".to_owned(),
            sync_cursor: Some("cursor-abc".to_owned()),
            last_synced_at: None,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap().into(),
        }
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let m = sample_model();
        let expected_id = m.id;
        let expected_ref = m.access_token_ref.clone();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.id.value(), expected_id);
        assert_eq!(domain.access_token_ref.as_str(), expected_ref);
        assert_eq!(domain.sync_cursor, Some("cursor-abc".to_owned()));
        assert!(domain.last_synced_at.is_none());
    }

    #[test]
    fn blank_access_token_ref_returns_error() {
        let mut m = sample_model();
        m.access_token_ref = "   ".to_owned();
        assert!(
            matches!(
                model_to_domain(m),
                Err(MapperError::InvalidStoredValue {
                    field: "access_token_ref",
                    ..
                })
            ),
            "expected MapperError::InvalidStoredValue for blank access_token_ref"
        );
    }

    #[test]
    fn active_model_preserves_token_ref() {
        let m = sample_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        assert_eq!(
            am.access_token_ref,
            Set(domain.access_token_ref.as_str().to_owned())
        );
    }
}
