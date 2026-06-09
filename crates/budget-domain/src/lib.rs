//! Pure business logic for the budget tracker.
//!
//! Hexagonal core (`DOMAIN-1`): no framework, no async runtime, no ORM, no DB.
//! This crate holds:
//!   - the single [`money::Money`] type (`BUDGET-MONEY-1`) used for ALL money,
//!   - newtype IDs for every aggregate (`DOMAIN-2`, [`ids`]),
//!   - validated newtype strings (`DOMAIN-3`, [`validated`]),
//!   - typed enums mirroring the Postgres `pgEnum`s (`ENTITIES-12`, [`enums`]),
//!   - the shared error enums (`DOMAIN-4`/`DOMAIN-6`, [`error`]),
//!   - one domain struct per aggregate (`DOMAIN-7`),
//!   - one repository trait per aggregate (`REPO-1`/`REPO-2`/`REPO-3`, [`repositories`]),
//!   - the unit-of-work + provider traits for cross-aggregate transactions
//!     (`REPO-4`/`REPO-10`, [`uow`]),
//!   - the pure budget predicates ([`predicates`]):
//!     `BUDGET-STATUS-DRIVES-INCLUSION-1` and `BUDGET-NO-DOUBLE-CHARGE-1`.
//!
//! Module organisation follows `DOMAIN-1` (one crate, modules by concern). The
//! aggregate structs live one-per-module; the cross-cutting concerns
//! (`money`, `ids`, `enums`, `error`, `predicates`, `uow`, `repositories`) each
//! get their own module and re-export through this root.

pub mod account;
pub mod auth;
pub mod budget;
pub mod category;
pub mod enums;
pub mod error;
pub mod fund;
pub mod ids;
pub mod money;
pub mod month;
pub mod paycheck_config;
pub mod plaid_api;
pub mod plaid_item;
pub mod predicates;
pub mod projections;
pub mod repayment_obligation;
pub mod repositories;
pub mod transaction;
pub mod uow;
pub mod user;
pub mod validated;

// Flat re-exports of the most-used types so callers can `use budget_domain::Money`
// etc. without the module path. Aggregate structs, IDs, enums, errors, the Money
// type, the predicates, and the repository + UoW traits are all surfaced here.
pub use account::Account;
pub use auth::{
    AuthError, PasswordHasher, SecretVault, TotpEnrollment, TotpService, WebauthnCredential,
    WebauthnCredentialRepository,
};
pub use budget::Budget;
pub use category::Category;
pub use enums::{
    AccountType, Cadence, CategoryGrp, FundKind, IncomeKind, IncomeMode, MonthStatus,
    ObligationSource, ObligationStatus, PaycheckType, SettleType, SurplusRouting,
    TransactionSource, TransactionStatus,
};
pub use error::{DomainError, RepositoryError, ValidationError};
pub use fund::Fund;
pub use ids::{
    AccountId, BudgetId, CategoryId, CategoryKey, FundId, MonthId, PaycheckConfigId, PlaidItemId,
    RepaymentObligationId, TransactionId, UserId, WebauthnCredentialId,
};
pub use money::Money;
pub use month::Month;
pub use paycheck_config::PaycheckConfig;
pub use plaid_api::{
    AccessTokenExchange, LinkToken, LinkTokenRequest, PlaidAccount, PlaidApi, PlaidError,
    PlaidProduct, PlaidSyncEngine, PlaidSyncPage, PlaidTransaction, SyncSummary,
};
pub use plaid_item::PlaidItem;
pub use predicates::{
    FixedSettlement, counts_in_budget, counts_in_month_expense_remaining, fixed_category_spent,
};
pub use projections::{CategorySpent, MonthNet};
pub use repayment_obligation::RepaymentObligation;
pub use repositories::{
    BudgetRepository, FundRepository, MonthRepository, PaycheckConfigRepository,
    PlaidItemRepository, TransactionRepository, UserRepository,
};
pub use transaction::Transaction;
pub use uow::{UnitOfWork, UowFuture, UowProvider, UowProviderExt};
pub use user::User;
pub use validated::{AccessTokenRef, Email};
