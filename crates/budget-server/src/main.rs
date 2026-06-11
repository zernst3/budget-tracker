//! `budget-server` — the native Axum fullstack host for the `budget-ui` app.
//!
//! One process serves three concerns (`RUST-DIOXUS-11`): it server-renders the
//! initial HTML, serves the hydrating wasm client bundle + static assets, and
//! mounts the server-function endpoints declared in `budget_ui::services`
//! (`RUST-DIOXUS-9`). The router is built explicitly here so the server-function
//! plumbing AND the auth gate's wiring are visible at the entrypoint.
//!
//! ## Auth wiring (`BUDGET-AUTH-GATE-1`, `SPEC §9.1`, Phase B1)
//!
//! Two layers are mounted around the Dioxus application so they apply to the
//! server-function routes (the server functions extract from the request the
//! layers populate):
//!   1. the [`SessionManagerLayer`] over the **Postgres-backed** session store —
//!      secure `HttpOnly` `SameSite=Strict` cookies that survive scale-to-zero
//!      (`build_session_layer`); and
//!   2. an [`Extension`] of [`AppState`](budget_ui::server_state::AppState) — the
//!      user repository + `AuthService` that the [`require_authed_user`] gate and
//!      the login server function read.
//!
//! With those two layers present, `budget_ui::services::gate::require_authed_user`
//! resolves the authenticated user (or 401s), and every data server function that
//! calls it FIRST is gated by construction.
//!
//! The bind address is supplied by the `dx` CLI / the deploy environment via
//! `dioxus-cli-config`, falling back to localhost for a bare `cargo run`.

// The fullstack integration's documented surface uses fallible setup at the app
// edge; anyhow is the binary-edge error type (RUST-DOMAIN-4).
use anyhow::Context;
use axum::Extension;
use axum::response::IntoResponse;
use axum::routing::get;
use dioxus_server::{DioxusRouterExt, ServeConfig};
use sea_orm::{ConnectOptions, Database};

use budget_infrastructure::auth::{SessionLayerConfig, build_session_layer};
use budget_infrastructure::run_pending_migrations;
use budget_ui::server_state::{AppState, MonthViewState, PortfolioState, TriageState};

// The entrypoint is a linear wiring sequence (connections -> migrations -> state
// -> router -> serve); splitting it would scatter the one-shot startup wiring
// across helpers without making it clearer.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Non-verbose logging (SPEC §8: stay under the Log Analytics free tier). The
    // RUST_LOG env var still overrides this default at runtime.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // The Neon/Postgres connection string is supplied out of band (never
    // committed). CI is expected red until the secret exists (SPEC §12).
    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL must be set (the Neon Postgres connection string)")?;

    // A connection string helper that keeps the pool modest for a single-user
    // scale-to-zero container.
    let connect = || {
        let mut opts = ConnectOptions::new(database_url.clone());
        opts.max_connections(5);
        Database::connect(opts)
    };

    // Primary connection: migrations, the user repository, and the session pool.
    let db = connect().await.context("connecting to the database")?;
    // Secondary connection for the webauthn-credential repository. A separate
    // connection (rather than a clone) keeps the wiring valid even under the
    // SeaORM `mock` feature, where `DatabaseConnection` is not `Clone`.
    let credentials_db = connect()
        .await
        .context("connecting to the database (credentials)")?;

    // Connections for the B4 month-view server functions. Each component gets
    // its own pool handle (same SeaORM `mock`-feature reason as above).
    let months_db = connect()
        .await
        .context("connecting to the database (months)")?;
    let budgets_db = connect()
        .await
        .context("connecting to the database (budgets)")?;
    let txns_db = connect()
        .await
        .context("connecting to the database (transactions)")?;
    let funds_db = connect()
        .await
        .context("connecting to the database (funds)")?;
    let uow_db = connect()
        .await
        .context("connecting to the database (uow)")?;

    // Connections for the BACKEND-3 Pull / Pending / triage server functions. Each
    // repository + unit-of-work gets its own pool handle (the SeaORM `mock`-feature
    // `Clone` caveat, as above).
    let triage_txns_db = connect()
        .await
        .context("connecting to the database (triage transactions)")?;
    let triage_funds_db = connect()
        .await
        .context("connecting to the database (triage funds)")?;
    let triage_fund_budgets_db = connect()
        .await
        .context("connecting to the database (triage fund budgets)")?;
    let triage_fund_uow_db = connect()
        .await
        .context("connecting to the database (triage fund uow)")?;
    let triage_uow_db = connect()
        .await
        .context("connecting to the database (triage uow)")?;
    let triage_plaid_items_db = connect()
        .await
        .context("connecting to the database (triage plaid items)")?;
    let triage_plaid_months_db = connect()
        .await
        .context("connecting to the database (triage plaid months)")?;
    let triage_plaid_users_db = connect()
        .await
        .context("connecting to the database (triage plaid users)")?;
    let triage_plaid_txns_db = connect()
        .await
        .context("connecting to the database (triage plaid transactions)")?;
    let triage_plaid_engine_uow_db = connect()
        .await
        .context("connecting to the database (triage plaid engine uow)")?;
    let triage_plaid_sync_uow_db = connect()
        .await
        .context("connecting to the database (triage plaid sync uow)")?;

    // Connections for the AI Portfolio Insights server functions (positions,
    // cash balances, the review-run audit log, and the review unit of work). Each
    // gets its own pool handle (same SeaORM `mock`-feature `Clone` reason).
    let portfolio_positions_db = connect()
        .await
        .context("connecting to the database (portfolio positions)")?;
    let portfolio_balances_db = connect()
        .await
        .context("connecting to the database (portfolio balances)")?;
    let portfolio_review_runs_db = connect()
        .await
        .context("connecting to the database (portfolio review runs)")?;
    let portfolio_review_uow_db = connect()
        .await
        .context("connecting to the database (portfolio review uow)")?;

    // Apply pending schema migrations before serving any traffic (idempotent).
    run_pending_migrations(&db)
        .await
        .context("applying pending migrations")?;

    // The session store needs the raw sqlx pool (the tower-sessions store owns +
    // migrates its own table). Pull it out of the SeaORM connection.
    let pool = db.get_postgres_connection_pool().clone();

    // The session layer: Postgres-backed store, secure cookie policy (Secure +
    // HttpOnly + SameSite=Strict). `SECURE_COOKIES=false` opts out of the Secure
    // flag for a local HTTP dev run ONLY; production (HTTPS ingress) keeps the
    // default `true`.
    let secure_cookies = std::env::var("SECURE_COOKIES").map_or(true, |v| v != "false");
    let session_layer = build_session_layer(
        pool,
        &SessionLayerConfig {
            secure: secure_cookies,
        },
    )
    .await
    .context("building the session layer")?;

    // The server-side state the gated server functions read. Building it includes
    // the `webauthn-rs` engine, whose relying-party origin must be a valid URL
    // (`WEBAUTHN_RP_ORIGIN`); a bad value fails startup loudly rather than
    // half-wiring passkeys.
    let state = AppState::from_connections(db, credentials_db)
        .map_err(|e| anyhow::anyhow!("building the webauthn engine: {e}"))?;

    // Month-view state: repos + lifecycle service for the B4 server functions.
    let month_view_state =
        MonthViewState::from_connections(months_db, budgets_db, txns_db, funds_db, uow_db);

    // Triage state: the Pull (Plaid sync) + Pending-inbox + atomic-triage services
    // for the BACKEND-3 server functions. Plaid is wired only when the credentials +
    // Key Vault URL are present in the environment (otherwise Pull 503s, SPEC §6).
    let triage_state = TriageState::from_connections(
        triage_txns_db,
        triage_funds_db,
        triage_fund_budgets_db,
        triage_fund_uow_db,
        triage_uow_db,
        triage_plaid_items_db,
        triage_plaid_months_db,
        triage_plaid_users_db,
        triage_plaid_txns_db,
        triage_plaid_engine_uow_db,
        triage_plaid_sync_uow_db,
    )
    .map_err(|e| anyhow::anyhow!("building the triage/plaid state: {e}"))?;

    // AI Portfolio Insights state (positions / cash / review). The cash-balance
    // adapter is bound to the single app user (SPEC §9); the user is provisioned
    // out-of-band, so resolve its id at startup from the `BUDGET_USER_EMAIL`
    // config. If that is unset or the user does not exist yet, the portfolio
    // Extension is NOT mounted (the portfolio routes then return the standard
    // "state unavailable" 500, mirroring the Plaid-optional posture) — no other
    // route is affected. The advisor / market / vault selection inside
    // `from_connections` is driven by `AI_MODE` (mock vs real Gemini), mirroring
    // `PLAID_MODE`.
    let portfolio_state = match std::env::var("BUDGET_USER_EMAIL") {
        Ok(email) if !email.trim().is_empty() => {
            match state.users.find_by_email(email.trim()).await {
                Ok(Some(user)) => match PortfolioState::from_connections(
                    portfolio_positions_db,
                    portfolio_balances_db,
                    portfolio_review_runs_db,
                    portfolio_review_uow_db,
                    user.id,
                ) {
                    Ok(ps) => Some(ps),
                    Err(e) => {
                        tracing::warn!(
                            "AI Portfolio Insights not wired (AI real-path \
                             prerequisites missing): {e}"
                        );
                        None
                    }
                },
                Ok(None) => {
                    tracing::warn!(
                        "AI Portfolio Insights not wired: BUDGET_USER_EMAIL set but no \
                         matching user (provision the user first)"
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!("AI Portfolio Insights not wired (user lookup failed): {e}");
                    None
                }
            }
        }
        _ => {
            tracing::info!(
                "AI Portfolio Insights not wired: set BUDGET_USER_EMAIL to the \
                 provisioned single-user email to enable the portfolio routes"
            );
            None
        }
    };

    // The bind address the `dx` CLI / Container Apps injects; localhost for a
    // bare `cargo run`.
    let address = dioxus_cli_config::fullstack_address_or_localhost();

    // Build the Axum router: SSR + static client bundle + server-function
    // endpoints (RUST-DIOXUS-11), then mount the auth layers AROUND it so they
    // apply to the server-function routes too. `budget_ui::App` is the single
    // shared root component (RUST-DIOXUS-16).
    //
    // PWA static assets (manifest.json, service-worker.js, icons) are served
    // from the `static/` directory via `tower_http::services::ServeDir`.
    let config = ServeConfig::new();
    let router = axum::Router::new()
        // PWA manifest and service worker (Phase B3: installable PWA shell)
        .route(
            "/manifest.json",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    include_str!("../static/manifest.json"),
                )
                    .into_response()
            }),
        )
        .route(
            "/service-worker.js",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/javascript")],
                    include_str!("../static/service-worker.js"),
                )
                    .into_response()
            }),
        )
        // Global stylesheet (FE3: consistent spacing, nav, currency, group-header)
        .route(
            "/app.css",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../static/app.css"),
                )
                    .into_response()
            }),
        )
        // Dioxus fullstack app: SSR HTML + client bundle + server functions
        .serve_dioxus_application(config, budget_ui::App)
        // Layer order: session layer first (populates `Session` extension),
        // then state extensions. All three extensions are visible to server-
        // function handlers downstream.
        .layer(Extension(triage_state))
        .layer(Extension(month_view_state))
        .layer(Extension(state))
        .layer(session_layer);

    // Mount the AI Portfolio Insights state only when it resolved (see above).
    let router = match portfolio_state {
        Some(ps) => router.layer(Extension(ps)),
        None => router,
    };

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding the server to {address}"))?;
    tracing::info!(%address, "budget-server listening");

    axum::serve(listener, router.into_make_service())
        .await
        .context("running the Axum server")?;

    Ok(())
}
