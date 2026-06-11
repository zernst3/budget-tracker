//! `SeaORM`-backed [`ReviewRunRepository`] — the append-only audit log for AI
//! Portfolio Insights (`REPO-1`, `SQL-AUDIT-COLUMNS-1`,
//! `docs/AI_FEATURE_DESIGN.md §0.4`, `§Phase 6`).
//!
//! `review_runs` has no update or delete surface: the only write is
//! [`insert`](ReviewRunRepository::insert), which the portfolio-review use-case
//! always calls inside its own explicit transaction (`ARCH-EXPLICIT-TX-1` /
//! `RUST-DOMAIN-7`). The trait hands the handle as `&mut dyn UnitOfWork` (not the
//! `Option<&dyn UnitOfWork>` shape the editable aggregates use), so this insert
//! always enlists in that transaction — there is no pool-fallback path.
//!
//! Reads ([`list_for_user`](ReviewRunRepository::list_for_user)) are newest-first
//! history; each row maps through [`budget_mappers::review_runs`], whose JSONB
//! decode is fallible (a corrupt payload surfaces as a [`RepositoryError`] via
//! [`map_read`], not a panic).

use async_trait::async_trait;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

use budget_domain::RepositoryError;
use budget_domain::ids::UserId;
use budget_domain::portfolio::ReviewRun;
use budget_domain::repositories::ReviewRunRepository;
use budget_domain::uow::UnitOfWork;

use budget_entities::review_runs;
use budget_mappers::review_runs as review_runs_mapper;

use crate::conn::with_conn;
use crate::error::map_db_err;
use crate::repositories::map_read;

/// `SeaORM`-backed [`ReviewRunRepository`] (append-only audit log).
pub struct PostgresReviewRunRepository {
    db: DatabaseConnection,
}

impl PostgresReviewRunRepository {
    /// Build the repository over a connection pool.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl ReviewRunRepository for PostgresReviewRunRepository {
    async fn insert(
        &self,
        run: &ReviewRun,
        uow: &mut dyn UnitOfWork,
    ) -> Result<(), RepositoryError> {
        // The audit row's JSONB payloads are serialized here; a serialize failure
        // is data corruption surfaced as a RepositoryError, never a panic.
        let active = review_runs_mapper::domain_to_active_model(run).map_err(map_read)?;
        // The trait hands a `&mut dyn UnitOfWork`; `with_conn` wants
        // `Option<&dyn UnitOfWork>`. Reborrow as a shared reference — `with_conn`
        // downcasts it to the concrete `SeaOrmUow` (whose tx lives behind an
        // `Arc<Mutex<..>>`, so no `&mut` is needed for the actual write).
        let handle: &dyn UnitOfWork = &*uow;
        with_conn(&self.db, Some(handle), move |conn| {
            Box::pin(async move {
                review_runs::Entity::insert(active)
                    .exec(conn)
                    .await
                    .map_err(map_db_err)?;
                Ok(())
            })
        })
        .await
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<ReviewRun>, RepositoryError> {
        let rows = review_runs::Entity::find()
            .filter(review_runs::Column::UserId.eq(user_id.value()))
            // Newest-first history (matches the `(user_id, occurred_at DESC)`
            // history index from m0007).
            .order_by_desc(review_runs::Column::OccurredAt)
            .all(&self.db)
            .await
            .map_err(map_db_err)?;
        rows.into_iter()
            .map(|m| review_runs_mapper::model_to_domain(m).map_err(map_read))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use sea_orm::{DatabaseBackend, MockDatabase};

    use budget_domain::ids::ReviewRunId;
    use budget_domain::money::Money;
    use budget_domain::portfolio::{NetWorth, PortfolioSnapshot, ReviewTerminalState};
    use chrono::Utc;
    use uuid::Uuid;

    fn empty_snapshot(user_id: Uuid) -> PortfolioSnapshot {
        PortfolioSnapshot {
            user_id: UserId::new(user_id),
            positions: vec![],
            cash_balances: vec![],
            buffer_total: Money::ZERO,
            net_worth: NetWorth {
                total_cash: Money::ZERO,
                total_positions: Money::ZERO,
                liabilities: Money::ZERO,
                total: Money::ZERO,
            },
            total_invested: Money::ZERO,
            captured_at: Utc::now(),
        }
    }

    /// `list_for_user` maps a stored row through the JSONB-decoding mapper. A
    /// `MockDatabase` row with the real JSONB payloads round-trips to the domain
    /// `ReviewRun` (`ORCH-NEW-PATH-TESTS-1`).
    #[tokio::test]
    async fn list_for_user_maps_rows_to_domain() {
        let user_id = Uuid::new_v4();
        let run = ReviewRun {
            id: ReviewRunId::new(Uuid::new_v4()),
            user_id: UserId::new(user_id),
            model_id: "gemini-2.5-pro".to_owned(),
            prompt_hash: "deadbeef".to_owned(),
            raw_output: "{\"recommendations\":[]}".to_owned(),
            snapshot: empty_snapshot(user_id),
            recommendations: vec![],
            outcomes: vec![],
            terminal_state: ReviewTerminalState::EmptyPortfolio,
            prompt_tokens: None,
            completion_tokens: None,
            finish_reason: None,
            latency_ms: 0,
            occurred_at: Utc::now(),
        };
        // Build the stored Model from the domain run via the real mapper so the
        // JSONB shapes match exactly.
        let am = review_runs_mapper::domain_to_active_model(&run).unwrap();
        let get_json = |v: sea_orm::ActiveValue<sea_orm::JsonValue>| match v {
            sea_orm::ActiveValue::Set(j) => j,
            _ => panic!("expected Set"),
        };
        let model = review_runs::Model {
            id: run.id.value(),
            user_id,
            model_id: run.model_id.clone(),
            prompt_hash: run.prompt_hash.clone(),
            raw_output: run.raw_output.clone(),
            snapshot: get_json(am.snapshot),
            outcomes: get_json(am.outcomes),
            recommendations: get_json(am.recommendations),
            terminal_state: review_runs_mapper::terminal_state_to_entity(
                run.terminal_state.clone(),
            ),
            prompt_tokens: None,
            completion_tokens: None,
            finish_reason: None,
            latency_ms: 0,
            occurred_at: run.occurred_at.into(),
        };

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results([vec![model]])
            .into_connection();
        let repo = PostgresReviewRunRepository::new(db);
        let out = repo.list_for_user(UserId::new(user_id)).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], run);
    }
}
