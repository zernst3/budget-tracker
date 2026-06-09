//! `seed-local-demo` — the STAGE-1 local-testing seed (`STAGE-1`).
//!
//! Populates a LOCAL Postgres with a realistic demo budget so every screen has
//! content before a single real transaction is pulled. Requires ONLY
//! `DATABASE_URL` and `SEED_EMAIL`; NO Plaid credentials, NO Azure, NO
//! real Key Vault.
//!
//! **What it creates** (all scoped to the provisioned user):
//!
//! 1. **Budget version** — "Local Demo Budget", effective from the current
//!    month's first day, no `effective_to` (current active version).
//! 2. **Categories** (7 + the mandatory rollover "Other" bucket):
//!    - Rent          — Fixed / `TrueSet`     / $2 500/mo   sort 1
//!    - Groceries     — Discretionary        / $600/mo    sort 2
//!    - Dining        — Discretionary        / $300/mo    sort 3
//!    - Transport     — Discretionary        / $150/mo    sort 4
//!    - Utilities     — Fixed / `FlexibleSet` / $130/mo (2 bills) sort 5
//!    - Subscriptions — Fixed / `TrueSet`     / $80/mo    sort 6
//!    - Misc          — Discretionary        / $200/mo    sort 7
//!    - **Other**     — rollover bucket      / $0         sort 8
//! 3. **Month** — the current month, `open`, referencing the budget above.
//! 4. **Funds**:
//!    - "Emergency Buffer"  — Buffer fund, `compulsory_repayment = true`,
//!      balance $5 000, target $6 000. Exercises the buffer treatment.
//!    - "Vacation Savings"  — Surplus fund, `compulsory_repayment = false`,
//!      balance $1 200, no target. Exercises the surplus treatment.
//! 5. **Pre-settled transactions** (two, so the month ledger is not empty before
//!    the first Pull). Both dated the 1st of the current month, both categorized:
//!    - "Rent" $-2 500 → Rent category (the big fixed charge).
//!    - "Whole Foods" $-84.30 → Groceries category.
//! 6. **Fake `PlaidItem`** — a linked institution row whose `access_token_ref` is
//!    set to the deterministic `MOCK_ACCESS_TOKEN` that [`MockPlaidApi`] and
//!    [`InMemorySecretVault`] both expect. Pull works under `PLAID_MODE=mock`
//!    without any exchange ceremony.
//! 7. **Fake Accounts** — `"BoA Checking"` and `"BoA Credit Card"`, each carrying
//!    the mock Plaid account id the fixture pages reference
//!    (`mock-account-checking` / `mock-account-credit`).
//!
//! ## Idempotent / re-runnable
//!
//! Every row uses a **deterministic id** derived from the user id via
//! `uuid::Uuid::new_v5` (namespace `BUDGET_DEMO_NS`). Re-running against the
//! same user upserts to the same state (ON CONFLICT (pk) DO UPDATE).
//!
//! ## Tracking start date
//!
//! Read from the provisioned user (set by `provision-user`). All demo rows
//! are dated on or after that boundary.
//!
//! ## NOT required
//!
//! - Real Plaid credentials (`PLAID_CLIENT_ID` / `PLAID_SECRET` / `KEY_VAULT_URL`)
//! - Azure Key Vault
//! - `PLAID_MODE=mock` (server-runtime var; this seed just plants the right rows)
//!
//! ## Usage
//!
//! ```text
//! DATABASE_URL=postgres://user:pass@localhost:5432/budget_local \
//! SEED_EMAIL=zach@example.com \
//! cargo run -p budget-server --bin seed-local-demo
//! ```

// Binary edge: anyhow is permitted here (RUST-DOMAIN-4 reserves it for app
// edges). The library code this calls returns typed errors.
#![allow(clippy::expect_used)]

use std::env;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate, Utc};

use budget_domain::account::Account;
use budget_domain::budget::Budget;
use budget_domain::category::Category;
use budget_domain::enums::{
    AccountType, Cadence, CategoryGrp, FundKind, MonthStatus, SettleType, TransactionSource,
    TransactionStatus,
};
use budget_domain::fund::Fund;
use budget_domain::ids::{
    AccountId, BudgetId, CategoryId, CategoryKey, FundId, MonthId, PlaidItemId, TransactionId,
};
use budget_domain::money::Money;
use budget_domain::month::Month;
use budget_domain::plaid_item::PlaidItem;
use budget_domain::repositories::{
    BudgetRepository, FundRepository, MonthRepository, PlaidItemRepository, TransactionRepository,
    UserRepository,
};
use budget_domain::transaction::Transaction;
use budget_domain::validated::AccessTokenRef;
use budget_infrastructure::{
    MOCK_ACCESS_TOKEN, PostgresBudgetRepository, PostgresFundRepository, PostgresMonthRepository,
    PostgresPlaidItemRepository, PostgresTransactionRepository, PostgresUserRepository,
    run_pending_migrations,
};

/// UUID v5 namespace for all deterministic demo-seed ids (fixed namespace uuid).
const BUDGET_DEMO_NS: uuid::Uuid = uuid::Uuid::from_bytes([
    0xbd, 0x9e, 0x74, 0x2a, 0x05, 0x1c, 0x4e, 0x3f, 0xa1, 0xd2, 0xc8, 0x3b, 0x66, 0x72, 0x1f, 0x90,
]);

/// Deterministic id: `uuid_v5(BUDGET_DEMO_NS, "{user_id}:{label}")`.
fn demo_id(user_id: uuid::Uuid, label: &str) -> uuid::Uuid {
    let key = format!("{user_id}:{label}");
    uuid::Uuid::new_v5(&BUDGET_DEMO_NS, key.as_bytes())
}

/// The first day of the given `(year, month)`.
fn month_start(year: i32, month: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, 1)
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(year, 1, 1).expect("date arithmetic"))
}

// ---------------------------------------------------------------------------
// Builder helpers — keep main() under the too_many_lines limit.
// ---------------------------------------------------------------------------

fn build_budget(
    user_id: budget_domain::ids::UserId,
    budget_id: BudgetId,
    effective_from: NaiveDate,
) -> Budget {
    Budget {
        id: budget_id,
        user_id,
        name: "Local Demo Budget".to_owned(),
        effective_from,
        effective_to: None,
        created_at: Utc::now(),
    }
}

/// Build the 8 demo categories (7 expense buckets + the rollover "Other").
#[allow(clippy::too_many_lines)]
fn build_categories(
    user_id: budget_domain::ids::UserId,
    budget_id: BudgetId,
    uid: uuid::Uuid,
) -> Vec<Category> {
    let make = |label: &str,
                name: &str,
                amount_minor: i64,
                grp: CategoryGrp,
                settle_type: Option<SettleType>,
                expected_bills: Option<i32>,
                is_rollover_bucket: bool,
                sort_order: i32|
     -> Category {
        let id = CategoryId::new(demo_id(uid, label));
        Category {
            id,
            budget_id,
            category_key: CategoryKey::new(id.value()),
            name: name.to_owned(),
            amount: Money::from_minor(amount_minor),
            grp,
            settle_type,
            expected_bills,
            is_rollover_bucket,
            cadence: Cadence::Monthly,
            period_months: None,
            fund_balance: Money::ZERO,
            next_due_date: None,
            sort_order,
        }
    };

    // Suppress the unused field warning from the compiler about `user_id` not
    // being used directly in this helper — it is used by callers to scope other
    // rows; the categories themselves carry budget_id (the user scoping chain).
    let _ = user_id;

    vec![
        // Rollover "Other" bucket — exactly one per budget (BUDGET-ROLLOVER-INTEGRITY-1).
        make(
            "cat-other",
            "Other",
            0,
            CategoryGrp::Discretionary,
            None,
            None,
            true,
            8,
        ),
        // Fixed
        make(
            "cat-rent",
            "Rent",
            250_000,
            CategoryGrp::Fixed,
            Some(SettleType::TrueSet),
            None,
            false,
            1,
        ),
        make(
            "cat-utilities",
            "Utilities",
            13_000,
            CategoryGrp::Fixed,
            Some(SettleType::FlexibleSet),
            Some(2),
            false,
            5,
        ),
        make(
            "cat-subscriptions",
            "Subscriptions",
            8_000,
            CategoryGrp::Fixed,
            Some(SettleType::TrueSet),
            None,
            false,
            6,
        ),
        // Discretionary
        make(
            "cat-groceries",
            "Groceries",
            60_000,
            CategoryGrp::Discretionary,
            None,
            None,
            false,
            2,
        ),
        make(
            "cat-dining",
            "Dining",
            30_000,
            CategoryGrp::Discretionary,
            None,
            None,
            false,
            3,
        ),
        make(
            "cat-transport",
            "Transport",
            15_000,
            CategoryGrp::Discretionary,
            None,
            None,
            false,
            4,
        ),
        make(
            "cat-misc",
            "Misc",
            20_000,
            CategoryGrp::Discretionary,
            None,
            None,
            false,
            7,
        ),
    ]
}

fn build_month(
    user_id: budget_domain::ids::UserId,
    budget_id: BudgetId,
    month_id: MonthId,
    year: i32,
    month: i32,
) -> Month {
    Month {
        id: month_id,
        user_id,
        budget_id,
        year,
        month,
        status: MonthStatus::Open,
        opened_at: Utc::now(),
        closed_at: None,
    }
}

fn build_funds(user_id: budget_domain::ids::UserId, uid: uuid::Uuid) -> [Fund; 2] {
    let now = Utc::now();
    [
        Fund {
            id: FundId::new(demo_id(uid, "fund-buffer")),
            user_id,
            name: "Emergency Buffer".to_owned(),
            kind: FundKind::Buffer,
            balance: Money::from_minor(500_000),
            target_balance: Some(Money::from_minor(600_000)),
            compulsory_repayment: true,
            created_at: now,
        },
        Fund {
            id: FundId::new(demo_id(uid, "fund-vacation")),
            user_id,
            name: "Vacation Savings".to_owned(),
            kind: FundKind::Surplus,
            balance: Money::from_minor(120_000),
            target_balance: None,
            compulsory_repayment: false,
            created_at: now,
        },
    ]
}

/// Build the two pre-settled demo transactions.
fn build_transactions(
    user_id: budget_domain::ids::UserId,
    uid: uuid::Uuid,
    month_id: MonthId,
    account_checking_id: AccountId,
    content_date: NaiveDate,
) -> [Transaction; 2] {
    let now = Utc::now();
    let cat_rent_id = CategoryId::new(demo_id(uid, "cat-rent"));
    let cat_groceries_id = CategoryId::new(demo_id(uid, "cat-groceries"));

    [
        Transaction {
            id: TransactionId::new(demo_id(uid, "txn-rent-demo")),
            user_id,
            month_id,
            category_id: Some(cat_rent_id),
            account_id: Some(account_checking_id),
            date: content_date,
            amount: Money::from_minor(-250_000),
            description: format!("Rent – {}", content_date.format("%B")),
            source: TransactionSource::Manual,
            plaid_transaction_id: None,
            status: TransactionStatus::Settled,
            income_kind: None,
            is_rollover: false,
            is_fund_draw: false,
            matched_transaction_id: None,
            comment: None,
            created_at: now,
            updated_at: now,
        },
        Transaction {
            id: TransactionId::new(demo_id(uid, "txn-grocery-demo")),
            user_id,
            month_id,
            category_id: Some(cat_groceries_id),
            account_id: Some(account_checking_id),
            date: content_date,
            amount: Money::from_minor(-8_430),
            description: "Whole Foods".to_owned(),
            source: TransactionSource::Manual,
            plaid_transaction_id: None,
            status: TransactionStatus::Settled,
            income_kind: None,
            is_rollover: false,
            is_fund_draw: false,
            matched_transaction_id: None,
            comment: Some("Demo pre-seeded grocery trip".to_owned()),
            created_at: now,
            updated_at: now,
        },
    ]
}

fn build_plaid_item(
    user_id: budget_domain::ids::UserId,
    plaid_item_id: PlaidItemId,
) -> Result<PlaidItem> {
    let access_token_ref = AccessTokenRef::try_new(MOCK_ACCESS_TOKEN)
        .context("constructing the mock access token ref")?;
    Ok(PlaidItem {
        id: plaid_item_id,
        user_id,
        institution_name: "Mock Bank (local dev)".to_owned(),
        access_token_ref,
        sync_cursor: None,
        last_synced_at: None,
        created_at: Utc::now(),
    })
}

fn build_accounts(
    user_id: budget_domain::ids::UserId,
    uid: uuid::Uuid,
    plaid_item_id: PlaidItemId,
) -> [Account; 2] {
    [
        Account {
            id: AccountId::new(demo_id(uid, "account-checking")),
            user_id,
            name: "BoA Checking".to_owned(),
            account_type: AccountType::Checking,
            plaid_account_id: Some("mock-account-checking".to_owned()),
            plaid_item_id: Some(plaid_item_id),
        },
        Account {
            id: AccountId::new(demo_id(uid, "account-credit")),
            user_id,
            name: "BoA Credit Card".to_owned(),
            account_type: AccountType::Credit,
            plaid_account_id: Some("mock-account-credit".to_owned()),
            plaid_item_id: Some(plaid_item_id),
        },
    ]
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = env::var("DATABASE_URL")
        .context("DATABASE_URL must be set (the local Postgres connection string)")?;
    let email =
        env::var("SEED_EMAIL").context("SEED_EMAIL must be set (the provisioned user email)")?;

    // --- Apply pending migrations (idempotent: no-op if already applied). ----
    let migrate_conn = sea_orm::Database::connect(&database_url)
        .await
        .context("connecting to the database for migrations")?;
    run_pending_migrations(&migrate_conn)
        .await
        .context("applying pending migrations")?;
    drop(migrate_conn);

    // --- Load the provisioned user. ------------------------------------------
    let user_db = sea_orm::Database::connect(&database_url)
        .await
        .context("connecting to the database (user lookup)")?;
    let user = PostgresUserRepository::new(user_db)
        .find_by_email(&email)
        .await
        .context("looking up the provisioned user")?
        .with_context(|| format!("no user with email {email}; run provision-user first"))?;

    let user_id = user.id;
    let uid = user_id.value();

    // --- Date context. -------------------------------------------------------
    let today = Utc::now().date_naive();
    let current_month_start = month_start(today.year(), today.month());
    let content_date = current_month_start.max(user.tracking_start_date);

    // --- Build domain objects. -----------------------------------------------
    let budget_id = BudgetId::new(demo_id(uid, "budget-v1"));
    let budget = build_budget(user_id, budget_id, content_date);
    let categories = build_categories(user_id, budget_id, uid);

    let month_id = MonthId::new(demo_id(uid, "month-current"));
    // `today.month()` returns u32 in 1..=12; i32::try_from is infallible for
    // values this small, so we unwrap with a panic-in-test fallback message.
    let month_num = i32::try_from(today.month())
        .context("month number out of i32 range (should never happen)")?;
    let month = build_month(user_id, budget_id, month_id, today.year(), month_num);

    let funds = build_funds(user_id, uid);

    let plaid_item_id = PlaidItemId::new(demo_id(uid, "plaid-item-mock"));
    let plaid_item = build_plaid_item(user_id, plaid_item_id)?;
    let accounts = build_accounts(user_id, uid, plaid_item_id);

    let account_checking_id = accounts[0].id;
    let transactions =
        build_transactions(user_id, uid, month_id, account_checking_id, content_date);

    // --- Connection factory (DatabaseConnection is not Clone under SeaORM
    // `mock` feature; each repo gets its own handle, same pattern as
    // seed_onboarding and AppState::from_connections). -----------------------
    let connect = || async {
        sea_orm::Database::connect(&database_url)
            .await
            .context("connecting to the database")
    };

    // Budget + categories.
    let budget_repo: Arc<dyn BudgetRepository> =
        Arc::new(PostgresBudgetRepository::new(connect().await?));
    budget_repo
        .save(&budget, None)
        .await
        .context("saving demo budget")?;
    for cat in &categories {
        budget_repo
            .save_category(cat, None)
            .await
            .with_context(|| format!("saving category {}", cat.name))?;
    }

    // Month (idempotent create).
    let month_repo: Arc<dyn MonthRepository> =
        Arc::new(PostgresMonthRepository::new(connect().await?));
    month_repo
        .create_if_absent(&month, None)
        .await
        .context("creating demo month")?;

    // Funds.
    let fund_repo: Arc<dyn FundRepository> =
        Arc::new(PostgresFundRepository::new(connect().await?));
    for fund in &funds {
        fund_repo
            .save(fund, None)
            .await
            .with_context(|| format!("saving fund {}", fund.name))?;
    }

    // PlaidItem + accounts (item must exist before accounts due to FK).
    let plaid_repo: Arc<dyn PlaidItemRepository> =
        Arc::new(PostgresPlaidItemRepository::new(connect().await?));
    plaid_repo
        .save(&plaid_item, None)
        .await
        .context("saving fake plaid item")?;
    for account in &accounts {
        plaid_repo
            .save_account(account, None)
            .await
            .with_context(|| format!("saving account {}", account.name))?;
    }

    // Pre-settled transactions (month must exist first).
    let txn_repo: Arc<dyn TransactionRepository> =
        Arc::new(PostgresTransactionRepository::new(connect().await?));
    for txn in &transactions {
        txn_repo
            .save(txn, None)
            .await
            .with_context(|| format!("saving demo transaction '{}'", txn.description))?;
    }

    // --- Summary. ------------------------------------------------------------
    println!("seed-local-demo applied (idempotent / re-runnable):");
    println!("  user:                 {user_id} ({email})");
    println!("  tracking_start_date:  {}", user.tracking_start_date);
    println!("  content_date:         {content_date}");
    println!("  budget:               {} ({budget_id})", budget.name);
    println!("  categories:           {} rows", categories.len());
    println!(
        "  month:                {}-{month_num:02} (id {month_id})",
        today.year()
    );
    println!(
        "  funds:                Emergency Buffer ($5000, target $6000) + Vacation Savings ($1200)"
    );
    println!("  plaid_item:           {plaid_item_id} (mock, no real Plaid)");
    println!(
        "  accounts:             BoA Checking (mock-account-checking) + BoA Credit Card (mock-account-credit)"
    );
    println!("  pre-seeded txns:      Rent $-2500 (Rent) + Whole Foods $-84.30 (Groceries)");
    println!();
    println!("Run the server with PLAID_MODE=mock to use the mock Plaid integration.");
    println!("Pull 1 → 4 transactions (1 pending grocery/gas/refund/restaurant).");
    println!("Pull 2 → pending restaurant settles + subscription added.");
    println!("Pull 3 → gas removed + coffee added. Then steady state.");

    Ok(())
}
