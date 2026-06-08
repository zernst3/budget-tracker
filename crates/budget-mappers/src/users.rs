//! Mapper: `budget-entities::users::Model` ↔ `budget-domain::user::User`.
//!
//! Conversions:
//!   - `email: String` → `Email::try_new` (fallible — `MapperError` on invalid stored value)
//!   - `tracking_start_date: Date` (SeaORM `NaiveDate`) → `NaiveDate` (same type; no conversion)
//!   - `created_at: DateTimeWithTimeZone` → `DateTime<Utc>` via `.with_timezone(&Utc)` (`DOMAIN-7`)
//!   - `id / (no fk)`: `Uuid` → `UserId` via `From<Uuid>` (`DOMAIN-2`)

use chrono::Utc;
use sea_orm::ActiveValue::Set;

use budget_domain::user::User;
use budget_domain::validated::Email;
use budget_domain::ids::UserId;

use budget_entities::users;

use crate::MapperError;

/// Translate a `users` [`users::Model`] into a domain [`User`].
///
/// # Errors
/// Returns [`MapperError::InvalidStoredValue`] if the stored `email` fails
/// [`Email::try_new`] (e.g. blank email in the DB — indicates data corruption).
pub fn model_to_domain(m: users::Model) -> Result<User, MapperError> {
    let email = Email::try_new(&m.email).map_err(|e| MapperError::InvalidStoredValue {
        field: "email",
        reason: e.to_string(),
    })?;

    Ok(User {
        id: UserId::new(m.id),
        email,
        password_hash: m.password_hash,
        totp_secret: m.totp_secret,
        tracking_start_date: m.tracking_start_date,
        created_at: m.created_at.with_timezone(&Utc),
    })
}

/// Translate a domain [`User`] into a `users` [`users::ActiveModel`] for insert or update.
///
/// Total — every domain value is already valid by construction.
#[must_use]
pub fn domain_to_active_model(v: &User) -> users::ActiveModel {
    users::ActiveModel {
        id: Set(v.id.value()),
        email: Set(v.email.as_str().to_owned()),
        password_hash: Set(v.password_hash.clone()),
        totp_secret: Set(v.totp_secret.clone()),
        tracking_start_date: Set(v.tracking_start_date),
        created_at: Set(v.created_at.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone};
    use uuid::Uuid;

    fn sample_model() -> users::Model {
        users::Model {
            id: Uuid::new_v4(),
            email: "zach@example.com".to_owned(),
            password_hash: "$argon2id$...".to_owned(),
            totp_secret: None,
            tracking_start_date: NaiveDate::from_ymd_opt(2026, 7, 1)
                .unwrap_or(NaiveDate::MIN),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .unwrap()
                .into(),
        }
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let m = sample_model();
        let expected_id = m.id;
        let expected_email = m.email.clone();
        let expected_hash = m.password_hash.clone();
        let expected_date = m.tracking_start_date;

        let domain = model_to_domain(m);
        assert!(
            domain.is_ok(),
            "expected Ok, got: {:?}",
            domain.err()
        );
        let u = domain.unwrap_or_else(|_| unreachable!());

        assert_eq!(u.id.value(), expected_id);
        assert_eq!(u.email.as_str(), expected_email);
        assert_eq!(u.password_hash, expected_hash);
        assert_eq!(u.tracking_start_date, expected_date);
        assert!(u.totp_secret.is_none());
    }

    #[test]
    fn invalid_email_returns_mapper_error() {
        let mut m = sample_model();
        m.email = "not-an-email".to_owned();
        assert!(
            matches!(
                model_to_domain(m),
                Err(MapperError::InvalidStoredValue { field: "email", .. })
            ),
            "expected MapperError::InvalidStoredValue for bad email"
        );
    }

    #[test]
    fn active_model_round_trip() {
        let m = sample_model();
        let domain = model_to_domain(m).unwrap_or_else(|_| unreachable!());
        let am = domain_to_active_model(&domain);
        // The Set variants should carry the values we stored.
        assert_eq!(am.id, Set(domain.id.value()));
        assert_eq!(am.email, Set(domain.email.as_str().to_owned()));
    }
}
