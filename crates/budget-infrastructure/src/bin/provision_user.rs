//! `provision-user` — the out-of-band single-user seed (`SPEC §9`,
//! `BUDGET-AUTH-GATE-1`).
//!
//! There is NO public signup. The single user (Zach) is provisioned out of band
//! by this admin CLI: it hashes the password with Argon2id, enrolls a mandatory
//! TOTP secret, and writes the one `users` row. It then prints the TOTP
//! provisioning URI so it can be added to an authenticator app (the only place
//! the secret is surfaced).
//!
//! This is a `[[bin]]` under the EXISTING `budget-infrastructure` crate (it owns
//! the Postgres repositories, the Argon2 hasher, and the TOTP engine), NOT a new
//! crate — auth was built entirely within the established crate topology
//! (`ROUTE-1`: no new crate / module-topology boundary).
//!
//! Usage (DB creds + secrets are supplied out of band, never committed):
//!
//! ```text
//! DATABASE_URL=postgres://... \
//! PROVISION_EMAIL=zach@example.com \
//! PROVISION_PASSWORD='...' \
//! PROVISION_TRACKING_START=2026-07-01 \
//! cargo run -p budget-infrastructure --bin provision-user
//! ```
//!
//! Idempotent on re-run: provisioning an email that already exists UPDATES that
//! user's password + TOTP (the `users` upsert keys on the primary key, and the
//! email lookup finds the existing row), rather than creating a duplicate.

// Binary edge: anyhow is permitted here (RUST-DOMAIN-4 reserves it for app
// edges). The library code this calls returns typed errors.
#![allow(clippy::expect_used)]

use std::env;

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, Utc};

use budget_domain::auth::{PasswordHasher, TotpService};
use budget_domain::ids::UserId;
use budget_domain::repositories::UserRepository;
use budget_domain::user::User;
use budget_domain::validated::Email;

use budget_infrastructure::auth::{Argon2idHasher, Rfc6238TotpService};
use budget_infrastructure::{PostgresUserRepository, run_pending_migrations};

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = env::var("DATABASE_URL")
        .context("DATABASE_URL must be set (the Neon connection string)")?;
    let email_raw = env::var("PROVISION_EMAIL").context("PROVISION_EMAIL must be set")?;
    let password = env::var("PROVISION_PASSWORD").context("PROVISION_PASSWORD must be set")?;
    let tracking_start_raw = env::var("PROVISION_TRACKING_START")
        .context("PROVISION_TRACKING_START must be set (YYYY-MM-DD; the genesis cutover, D8)")?;

    if password.len() < 12 {
        bail!("PROVISION_PASSWORD must be at least 12 characters");
    }

    let email =
        Email::try_new(&email_raw).map_err(|e| anyhow::anyhow!("invalid PROVISION_EMAIL: {e}"))?;
    let tracking_start_date: NaiveDate = tracking_start_raw
        .parse()
        .context("PROVISION_TRACKING_START must be a YYYY-MM-DD date")?;

    let db = sea_orm::Database::connect(&database_url)
        .await
        .context("connecting to the database")?;
    // Ensure the schema (incl. the users + webauthn_credentials tables) exists.
    run_pending_migrations(&db)
        .await
        .context("applying pending migrations")?;

    let users = PostgresUserRepository::new(db);
    let hasher = Argon2idHasher::new();
    let totp = Rfc6238TotpService::new();

    // Upsert by email: reuse the existing user id if one is already provisioned
    // (idempotent re-run resets credentials rather than duplicating).
    let existing = users
        .find_by_email(email.as_str())
        .await
        .context("looking up an existing user")?;
    let user_id = existing.as_ref().map_or_else(UserId::generate, |u| u.id);
    let created_at = existing.as_ref().map_or_else(Utc::now, |u| u.created_at);

    let password_hash = hasher
        .hash(&password)
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?;
    let enrollment = totp
        .enroll(email.as_str())
        .map_err(|e| anyhow::anyhow!("enrolling TOTP: {e}"))?;

    let user = User {
        id: user_id,
        email,
        password_hash,
        totp_secret: Some(enrollment.secret),
        tracking_start_date,
        created_at,
    };
    users
        .save(&user, None)
        .await
        .context("saving the provisioned user")?;

    let action = if existing.is_some() {
        "updated"
    } else {
        "created"
    };
    println!("user {action}: {user_id}");
    println!("email: {email_raw}");
    println!("tracking_start_date: {tracking_start_date}");
    println!();
    println!("Add this TOTP to your authenticator app (scan or paste the URI):");
    println!("{}", enrollment.provisioning_uri);
    println!();
    println!(
        "Passkeys (biometric login) are registered later from the logged-in UI; \
         TOTP above is the mandatory fallback factor."
    );

    Ok(())
}
