//! Newtype IDs for every aggregate (`DOMAIN-2`).
//!
//! Every entity ID is a newtype wrapper around [`uuid::Uuid`]. The compiler
//! refuses to let a [`UserId`] be passed where a [`BudgetId`] is expected, even
//! though both are `Uuid` underneath — moving a whole class of
//! wrong-ID-to-wrong-function bug from runtime to compile time. Bare `Uuid`
//! appears only at the persistence and server boundaries.
//!
//! Each newtype offers:
//!   - `new(Uuid)` — wrap an existing id (e.g. one read from the DB),
//!   - `generate()` — mint a fresh v4 id,
//!   - `value()` — unwrap to the bare `Uuid` for the persistence boundary,
//!   - serde + the usual derives so the type threads through DTOs cleanly.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Defines a `Uuid`-backed newtype id with the standard constructors + derives.
macro_rules! uuid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Wrap an existing [`Uuid`] (e.g. one read from persistence).
            #[must_use]
            pub const fn new(id: Uuid) -> Self {
                Self(id)
            }

            /// Mint a fresh random (v4) id.
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::new_v4())
            }

            /// The underlying [`Uuid`], for the persistence / server boundary.
            #[must_use]
            pub const fn value(&self) -> Uuid {
                self.0
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Uuid {
                id.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

uuid_newtype!(
    /// Identifies a [`crate::user::User`].
    UserId
);
uuid_newtype!(
    /// Identifies a [`crate::budget::Budget`] version.
    BudgetId
);
uuid_newtype!(
    /// Identifies a [`crate::category::Category`].
    CategoryId
);
uuid_newtype!(
    /// Stable lineage id shared by a category across budget versions (`D3`).
    CategoryKey
);
uuid_newtype!(
    /// Identifies an [`crate::account::Account`].
    AccountId
);
uuid_newtype!(
    /// Identifies a [`crate::plaid_item::PlaidItem`].
    PlaidItemId
);
uuid_newtype!(
    /// Identifies a [`crate::month::Month`].
    MonthId
);
uuid_newtype!(
    /// Identifies a [`crate::transaction::Transaction`].
    TransactionId
);
uuid_newtype!(
    /// Identifies a [`crate::fund::Fund`].
    FundId
);
uuid_newtype!(
    /// Identifies a [`crate::repayment_obligation::RepaymentObligation`].
    RepaymentObligationId
);
uuid_newtype!(
    /// Identifies a [`crate::paycheck_config::PaycheckConfig`].
    PaycheckConfigId
);
uuid_newtype!(
    /// Identifies a [`crate::auth::WebauthnCredential`] (a passkey / device,
    /// `SPEC §5` / `§9.1`, `BUDGET-AUTH-GATE-1`).
    WebauthnCredentialId
);
uuid_newtype!(
    /// Identifies a [`crate::portfolio::Position`].
    PositionId
);
uuid_newtype!(
    /// Identifies a [`crate::portfolio::ReviewRun`] (the audit row for one
    /// portfolio-review invocation).
    ReviewRunId
);
uuid_newtype!(
    /// Identifies a cached [`crate::portfolio::DividendEvent`] row in the
    /// `dividend_events` table (Phase 7 `m0008`). The domain `DividendEvent` value
    /// is identity-free; this id is the persistence PK supplied at the mapper.
    DividendEventId
);
uuid_newtype!(
    /// Identifies a [`crate::portfolio::DripApplication`] (one row in a position's
    /// auditable DRIP accretion chain, Phase 7 `m0008`,
    /// `BUDGET-ROLLOVER-INTEGRITY-1`).
    DripApplicationId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_uuid() {
        let raw = Uuid::new_v4();
        let id = UserId::new(raw);
        assert_eq!(id.value(), raw);
        assert_eq!(Uuid::from(id), raw);
        assert_eq!(UserId::from(raw), id);
    }

    #[test]
    fn generate_produces_distinct_ids() {
        assert_ne!(BudgetId::generate(), BudgetId::generate());
    }
}
