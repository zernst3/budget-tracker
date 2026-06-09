//! Mapper: `budget-entities::webauthn_credentials::Model` ↔
//! `budget-domain::auth::WebauthnCredential`.
//!
//! Conversions:
//!   - `id` / `user_id`: `Uuid` → newtype IDs via `From<Uuid>` (`DOMAIN-2`)
//!   - timestamps: `DateTimeWithTimeZone` → `DateTime<Utc>` via
//!     `.with_timezone(&Utc)` (`DOMAIN-7`)
//!   - `credential_id` / `public_key`: `Vec<u8>` (opaque blobs, no validation)
//!
//! `model_to_domain` is total: the `webauthn_credentials` row carries no
//! validated newtype, so there is nothing that can fail at the boundary.

use chrono::Utc;
use sea_orm::ActiveValue::Set;

use budget_domain::auth::WebauthnCredential;
use budget_domain::ids::{UserId, WebauthnCredentialId};

use budget_entities::webauthn_credentials;

/// Translate a `webauthn_credentials` [`webauthn_credentials::Model`] into a
/// domain [`WebauthnCredential`]. Total (no validated fields).
#[must_use]
pub fn model_to_domain(m: webauthn_credentials::Model) -> WebauthnCredential {
    WebauthnCredential {
        id: WebauthnCredentialId::new(m.id),
        user_id: UserId::new(m.user_id),
        credential_id: m.credential_id,
        public_key: m.public_key,
        sign_count: m.sign_count,
        transports: m.transports,
        aaguid: m.aaguid,
        nickname: m.nickname,
        created_at: m.created_at.with_timezone(&Utc),
        last_used_at: m.last_used_at.map(|t| t.with_timezone(&Utc)),
    }
}

/// Translate a domain [`WebauthnCredential`] into an `ActiveModel` for insert or
/// update. Total — every domain value is already valid by construction.
#[must_use]
pub fn domain_to_active_model(v: &WebauthnCredential) -> webauthn_credentials::ActiveModel {
    webauthn_credentials::ActiveModel {
        id: Set(v.id.value()),
        user_id: Set(v.user_id.value()),
        credential_id: Set(v.credential_id.clone()),
        public_key: Set(v.public_key.clone()),
        sign_count: Set(v.sign_count),
        transports: Set(v.transports.clone()),
        aaguid: Set(v.aaguid.clone()),
        nickname: Set(v.nickname.clone()),
        created_at: Set(v.created_at.into()),
        last_used_at: Set(v.last_used_at.map(Into::into)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn sample_model() -> webauthn_credentials::Model {
        webauthn_credentials::Model {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            credential_id: vec![1, 2, 3, 4],
            public_key: vec![9, 8, 7],
            sign_count: 42,
            transports: Some("internal".to_owned()),
            aaguid: None,
            nickname: Some("MacBook Touch ID".to_owned()),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap().into(),
            last_used_at: Some(Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap().into()),
        }
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let m = sample_model();
        let expected_id = m.id;
        let expected_user = m.user_id;
        let expected_cred = m.credential_id.clone();
        let expected_count = m.sign_count;

        let d = model_to_domain(m);
        assert_eq!(d.id.value(), expected_id);
        assert_eq!(d.user_id.value(), expected_user);
        assert_eq!(d.credential_id, expected_cred);
        assert_eq!(d.sign_count, expected_count);
        assert_eq!(d.nickname.as_deref(), Some("MacBook Touch ID"));
        assert!(d.last_used_at.is_some());
    }

    #[test]
    fn active_model_round_trip() {
        let d = model_to_domain(sample_model());
        let am = domain_to_active_model(&d);
        assert_eq!(am.id, Set(d.id.value()));
        assert_eq!(am.credential_id, Set(d.credential_id.clone()));
        assert_eq!(am.sign_count, Set(d.sign_count));
    }
}
