//! Mapper: `budget-entities::accounts::Model` ↔ `budget-domain::account::Account`.
//!
//! Conversions:
//!   - `id / user_id / plaid_item_id`: `Uuid` / `Option<Uuid>` → typed IDs (`DOMAIN-2`)
//!   - `type` (entity `AccountType`) → domain `AccountType` (1:1 variant names)
//!   - `plaid_account_id`: `Option<String>` — pass through
//!   - No timestamp columns; no `DOMAIN-7` conversion needed.
//!
//! Total — no validated newtypes on `Account`.

use sea_orm::ActiveValue::Set;

use budget_domain::account::Account;
use budget_domain::enums::AccountType;
use budget_domain::ids::{AccountId, PlaidItemId, UserId};

use budget_entities::accounts;

use crate::MapperError;

// ---------------------------------------------------------------------------
// Entity enum → domain enum
// ---------------------------------------------------------------------------

fn account_type_to_domain(e: accounts::AccountType) -> AccountType {
    match e {
        accounts::AccountType::Checking => AccountType::Checking,
        accounts::AccountType::Credit => AccountType::Credit,
        accounts::AccountType::Savings => AccountType::Savings,
        accounts::AccountType::Investment => AccountType::Investment,
        accounts::AccountType::Other => AccountType::Other,
    }
}

fn account_type_to_entity(d: AccountType) -> accounts::AccountType {
    match d {
        AccountType::Checking => accounts::AccountType::Checking,
        AccountType::Credit => accounts::AccountType::Credit,
        AccountType::Savings => accounts::AccountType::Savings,
        AccountType::Investment => accounts::AccountType::Investment,
        AccountType::Other => accounts::AccountType::Other,
    }
}

// ---------------------------------------------------------------------------
// Public mapper functions
// ---------------------------------------------------------------------------

/// Translate an `accounts` [`accounts::Model`] into a domain [`Account`].
///
/// Total — no validated newtypes on `Account`.
pub fn model_to_domain(m: accounts::Model) -> Result<Account, MapperError> {
    Ok(Account {
        id: AccountId::new(m.id),
        user_id: UserId::new(m.user_id),
        name: m.name,
        account_type: account_type_to_domain(m.r#type),
        plaid_account_id: m.plaid_account_id,
        plaid_item_id: m.plaid_item_id.map(PlaidItemId::new),
    })
}

/// Translate a domain [`Account`] into an `accounts` [`accounts::ActiveModel`].
#[must_use]
pub fn domain_to_active_model(v: &Account) -> accounts::ActiveModel {
    accounts::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        name: Set(v.name.clone()),
        r#type: Set(account_type_to_entity(v.account_type)),
        plaid_account_id: Set(v.plaid_account_id.clone()),
        plaid_item_id: Set(v.plaid_item_id.map(|id| id.value())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn sample_model(account_type: accounts::AccountType) -> accounts::Model {
        accounts::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            name: "BoA Checking".to_owned(),
            r#type: account_type,
            plaid_account_id: Some("plaid-acct-123".to_owned()),
            plaid_item_id: Some(Uuid::new_v4()),
        }
    }

    #[test]
    fn checking_account_round_trips() {
        let m = sample_model(accounts::AccountType::Checking);
        let expected_id = m.id;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert_eq!(domain.id.value(), expected_id);
        assert_eq!(domain.account_type, AccountType::Checking);
        assert!(domain.plaid_account_id.is_some());
        assert!(domain.plaid_item_id.is_some());
    }

    #[test]
    fn manually_tracked_account_no_plaid() {
        let mut m = sample_model(accounts::AccountType::Credit);
        m.plaid_account_id = None;
        m.plaid_item_id = None;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        assert!(domain.plaid_account_id.is_none());
        assert!(domain.plaid_item_id.is_none());
    }

    #[test]
    fn all_account_types_map_cleanly() {
        for (entity_type, expected) in [
            (accounts::AccountType::Checking, AccountType::Checking),
            (accounts::AccountType::Credit, AccountType::Credit),
            (accounts::AccountType::Savings, AccountType::Savings),
            (accounts::AccountType::Investment, AccountType::Investment),
            (accounts::AccountType::Other, AccountType::Other),
        ] {
            let m = sample_model(entity_type);
            let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
            assert_eq!(domain.account_type, expected);
        }
    }

    #[test]
    fn active_model_plaid_item_id_preserved() {
        let m = sample_model(accounts::AccountType::Checking);
        let expected_plaid_item_id = m.plaid_item_id;
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        assert_eq!(am.plaid_item_id, Set(expected_plaid_item_id));
    }
}
