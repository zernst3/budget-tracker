//! `seed-onboarding` — the re-runnable initial-load seed (`SPEC §4.6`, `§12` D8;
//! `BUDGET-CUTOVER-1`, build step 9).
//!
//! Posts the genesis OPENING SNAPSHOT for the single user: per-category
//! month-to-date summary opening charges plus the correct starting rolling-Other
//! balance and starting buffer-fund balance, all dated the user's
//! `tracking_start_date` (the genesis boundary). NO transaction history is
//! backfilled; nothing is dated before the boundary (`BUDGET-CUTOVER-1`).
//!
//! This is a `[[bin]]` under the EXISTING `budget-infrastructure` crate (it owns
//! the Postgres repositories + the `SeaOrmUowProvider`), mirroring the
//! `provision-user` seed — NOT a new crate (`ROUTE-1`). The orchestration itself
//! lives in [`budget_app_services::OnboardingService`]; this binary only wires the
//! concrete repos to it and reads the runtime inputs.
//!
//! ## Inputs are runtime, never hardcoded
//!
//! `tracking_start_date` is read from the persisted user (run `provision-user`
//! first to set it). The figures (per-category month-to-date spend, the starting
//! Other balance, the starting buffer balance) come from a JSON config file whose
//! path is given by `SEED_CONFIG`, ultimately transcribed from Zach's reference
//! spreadsheet. The binary hardcodes no figures.
//!
//! Config JSON shape (category ids + fund id are the DB UUIDs):
//!
//! ```json
//! {
//!   "category_charges": [
//!     { "category_id": "11111111-1111-1111-1111-111111111111", "spend_so_far": "300.00" },
//!     { "category_id": "22222222-2222-2222-2222-222222222222", "spend_so_far": "0" }
//!   ],
//!   "starting_other_balance": "212.00",
//!   "starting_buffer": {
//!     "fund_id": "33333333-3333-3333-3333-333333333333",
//!     "balance": "5000.00"
//!   }
//! }
//! ```
//!
//! `starting_buffer` may be omitted/null. A `spend_so_far` of `"0"` posts no
//! charge (a $0 `flexible_set` like utilities settles normally later, `SPEC §4.6`).
//!
//! Usage (DB creds + the user email supplied out of band):
//!
//! ```text
//! DATABASE_URL=postgres://... \
//! SEED_EMAIL=zach@example.com \
//! SEED_CONFIG=/path/to/opening-snapshot.json \
//! cargo run -p budget-infrastructure --bin seed-onboarding
//! ```
//!
//! Re-runnable: running twice yields identical state (deterministic opening-row
//! ids, ON CONFLICT (pk) DO UPDATE upserts), and re-running with revised figures
//! after a test phase performs a clean coherent reset (`SPEC §12` onboarding path).

// Binary edge: anyhow is permitted here (RUST-DOMAIN-4 reserves it for app edges).
#![allow(clippy::expect_used)]

use std::env;
use std::sync::Arc;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use uuid::Uuid;

use budget_app_services::{
    BufferOpeningBalance, CategoryOpeningCharge, OnboardingInput, OnboardingService,
};
use budget_domain::ids::{CategoryId, FundId};
use budget_domain::money::Money;
use budget_domain::repositories::{
    BudgetRepository, FundRepository, MonthRepository, TransactionRepository, UserRepository,
};
use budget_domain::uow::UowProvider;
use chrono::Utc;

use budget_infrastructure::{
    PostgresBudgetRepository, PostgresFundRepository, PostgresMonthRepository,
    PostgresTransactionRepository, PostgresUserRepository, SeaOrmUowProvider,
    run_pending_migrations,
};

/// The JSON config file shape (`SEED_CONFIG`). Money is parsed from a decimal
/// string so no float ever touches a monetary value (`BUDGET-MONEY-1`).
#[derive(Debug, Deserialize)]
struct SeedConfig {
    category_charges: Vec<ChargeEntry>,
    starting_other_balance: String,
    #[serde(default)]
    starting_buffer: Option<BufferEntry>,
}

#[derive(Debug, Deserialize)]
struct ChargeEntry {
    category_id: Uuid,
    spend_so_far: String,
}

#[derive(Debug, Deserialize)]
struct BufferEntry {
    fund_id: Uuid,
    balance: String,
}

fn parse_money(raw: &str, field: &str) -> Result<Money> {
    let dec: Decimal = raw
        .parse()
        .with_context(|| format!("{field} must be a decimal string (got {raw:?})"))?;
    Ok(Money::from_decimal(dec))
}

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = env::var("DATABASE_URL")
        .context("DATABASE_URL must be set (the Neon connection string)")?;
    let email = env::var("SEED_EMAIL").context("SEED_EMAIL must be set (the provisioned user)")?;
    let config_path = env::var("SEED_CONFIG")
        .context("SEED_CONFIG must be set (path to the opening-snapshot JSON)")?;

    let config_raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading SEED_CONFIG file {config_path}"))?;
    let config: SeedConfig =
        serde_json::from_str(&config_raw).context("parsing the SEED_CONFIG JSON")?;

    // SeaORM's `DatabaseConnection` is `Clone` only when the `mock` dev-feature is
    // active; the production build (and the CI clippy `--all-features` build) does
    // NOT have it Clone. So each component opens its own connection rather than
    // cloning one. This is safe across the unit-of-work: when a write enlists in
    // `Some(uow)`, it routes through the provider's transaction handle, not its own
    // connection (see `crate::conn::with_conn`), so all writes inside the seed's
    // `uow.run` closure commit atomically on the provider's connection (SERVICE-TX-1).
    let migrate_conn = sea_orm::Database::connect(&database_url)
        .await
        .context("connecting to the database")?;
    run_pending_migrations(&migrate_conn)
        .await
        .context("applying pending migrations")?;

    let user = PostgresUserRepository::new(
        sea_orm::Database::connect(&database_url)
            .await
            .context("connecting to the database")?,
    )
    .find_by_email(&email)
    .await
    .context("looking up the seed user")?
    .with_context(|| format!("no user with email {email}; run provision-user first"))?;

    // Translate the runtime config into the typed service input. tracking_start_date
    // is NOT taken from config — it is read from the user inside the service (the
    // single-source genesis boundary, BUDGET-CUTOVER-1).
    let mut category_charges = Vec::with_capacity(config.category_charges.len());
    for entry in &config.category_charges {
        category_charges.push(CategoryOpeningCharge {
            category_id: CategoryId::new(entry.category_id),
            spend_so_far: parse_money(&entry.spend_so_far, "category_charges[].spend_so_far")?,
        });
    }
    let starting_other_balance =
        parse_money(&config.starting_other_balance, "starting_other_balance")?;
    let starting_buffer = match &config.starting_buffer {
        Some(b) => Some(BufferOpeningBalance {
            fund_id: FundId::new(b.fund_id),
            balance: parse_money(&b.balance, "starting_buffer.balance")?,
        }),
        None => None,
    };

    let input = OnboardingInput {
        user_id: user.id,
        category_charges,
        starting_other_balance,
        starting_buffer,
    };

    // A fresh connection per component (DatabaseConnection is not Clone under the
    // mock feature; see above). All writes commit on the provider's connection via
    // the unit of work.
    let connect = || async {
        sea_orm::Database::connect(&database_url)
            .await
            .context("connecting to the database")
    };
    let service = OnboardingService::new(
        Arc::new(PostgresUserRepository::new(connect().await?)) as Arc<dyn UserRepository>,
        Arc::new(PostgresBudgetRepository::new(connect().await?)) as Arc<dyn BudgetRepository>,
        Arc::new(PostgresMonthRepository::new(connect().await?)) as Arc<dyn MonthRepository>,
        Arc::new(PostgresTransactionRepository::new(connect().await?))
            as Arc<dyn TransactionRepository>,
        Arc::new(PostgresFundRepository::new(connect().await?)) as Arc<dyn FundRepository>,
        Arc::new(SeaOrmUowProvider::new(connect().await?)) as Arc<dyn UowProvider>,
    );

    let report = service
        .seed(&input, Utc::now())
        .await
        .context("seeding the onboarding opening snapshot")?;

    println!("onboarding seed applied (re-runnable / idempotent):");
    println!("  user:                  {} ({email})", user.id);
    println!("  genesis date (day 1):  {}", report.genesis_date);
    println!("  genesis month id:      {}", report.genesis_month_id);
    println!(
        "  opening charges posted: {}",
        report.opening_charges_posted
    );
    println!("  starting Other line:   {}", report.other_line_posted);
    println!("  buffer balance seeded: {}", report.buffer_seeded);
    println!();
    println!(
        "No history was backfilled; all opening positions are dated the genesis \
         boundary (BUDGET-CUTOVER-1). Plaid sync excludes anything dated before it, \
         so the two layers never double-count."
    );

    Ok(())
}
