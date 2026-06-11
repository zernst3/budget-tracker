//! `SeaORM` entity for `review_runs` (AI Portfolio Insights, migration `m0007`).
//!
//! Schema source: `docs/AI_FEATURE_DESIGN.md §1.E/§1.F` — the authoritative
//! definition for the portfolio-insights feature.
//!
//! Pattern: per ENTITIES-1..6, ENTITIES-12 in `agora-rs/docs/CONVENTIONS.md`.
//!
//! ## Append-only system log (`SQL-AUDIT-COLUMNS-1`)
//!
//! One row per portfolio-review invocation. This is a system audit log, NOT a
//! mutable aggregate: there is `created_at` semantics via `occurred_at` but NO
//! `updated_at` (rows are never updated), NO `created_by`/`modified_by`
//! (machine-written, single-user), and crucially NO reverse `has_many` on
//! `users` (the user aggregate does not own a collection of audit rows). The FK
//! to `users` is `ON DELETE Cascade` so a user's audit trail is removed with the
//! user.
//!
//! ## JSONB payloads
//!
//! `snapshot`, `outcomes`, and `recommendations` are `JSONB` (`Json` on the
//! Model). `outcomes` is the LOCKED index-paired shape
//! `Vec<(usize, ValidationOutcome)>` (`§0.4`); `recommendations` is the model's
//! parsed `Vec<Recommendation>` (`§0.4`-addendum) so the audit row is
//! self-contained. The serde round-trip lives in the Phase-6 `review_runs`
//! mapper, not here (ENTITIES-2: no serde on `Model`).
//!
//! `terminal_state` is the `review_terminal_state` pg-enum, declared below via
//! `DeriveActiveEnum` (`ENTITIES-12`), distinct in name from the domain
//! `ReviewTerminalState` to avoid a collision when both are imported into the
//! mapper (`§0.4` LOCKED).

use sea_orm::entity::prelude::*;

/// The terminal classification of a review run (`review_terminal_state` pg-enum,
/// `ENTITIES-12`). Named `...Entity` to avoid colliding with the domain
/// `ReviewTerminalState` in the mapper (`§0.4` LOCKED).
#[derive(Copy, Clone, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "Enum",
    enum_name = "review_terminal_state"
)]
pub enum ReviewTerminalStateEntity {
    /// ≥1 verifiable recommendation. SUCCESS.
    #[sea_orm(string_value = "completed")]
    Completed,
    /// Valid JSON, zero recs OR zero verifiable. SUCCESS.
    #[sea_orm(string_value = "no_verifiable_insights")]
    NoVerifiableInsights,
    /// Short-circuit before the model call (empty portfolio). SUCCESS.
    #[sea_orm(string_value = "empty_portfolio")]
    EmptyPortfolio,
    /// Parse failure. FAILURE-of-review (run still persisted).
    #[sea_orm(string_value = "malformed_output")]
    MalformedOutput,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "review_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub model_id: String,
    pub prompt_hash: String,
    /// The raw model output (also the home for a parse-failure payload).
    pub raw_output: String,
    /// The grounding `PortfolioSnapshot`, JSONB.
    pub snapshot: Json,
    /// Per-recommendation outcomes, the LOCKED index-paired shape `§0.4`, JSONB.
    pub outcomes: Json,
    /// The model's parsed `Vec<Recommendation>`, JSONB (`§0.4`-addendum).
    pub recommendations: Json,
    pub terminal_state: ReviewTerminalStateEntity,
    /// Prompt token count, if the provider reported it.
    pub prompt_tokens: Option<i64>,
    /// Completion token count, if the provider reported it.
    pub completion_tokens: Option<i64>,
    /// The model's stop/finish reason (truncation / safety-stop audit). Nullable:
    /// `None` on the short-circuit / parse-failure paths (`§0.4`).
    pub finish_reason: Option<String>,
    /// Measured model-call latency in milliseconds.
    pub latency_ms: i64,
    /// When the review occurred (`ARCH-UTC-TIMESTAMPS-1`). The single audit
    /// timestamp — there is no `updated_at` (append-only, `SQL-AUDIT-COLUMNS-1`).
    pub occurred_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::users::Entity",
        from = "Column::UserId",
        to = "super::users::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    User,
}

impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::User.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
