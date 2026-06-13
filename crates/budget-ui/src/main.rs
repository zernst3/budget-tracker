//! `budget-server` — the native entry binary for the `budget-ui` fullstack app.
//!
//! Per `PORT-FULLSTACK-1` / `RUST-DIOXUS-16`, this binary lives in the SAME crate
//! as the shared `App` and the server functions; the target cfg selects which
//! `main` compiles. On native it builds the Axum router
//! ([`budget_ui::server::build_router`]) and serves it; on wasm32 it launches the
//! hydrating client (`dioxus::launch`).
//!
//! One process serves three concerns (`RUST-DIOXUS-11`): SSR HTML, the hydrating
//! wasm client bundle + static assets, and the server-function endpoints declared
//! in `budget_ui::services` (`RUST-DIOXUS-9`). The router construction (DB
//! connections, migrations-on-startup, `BUDGET_USER_EMAIL` resolution, AI_MODE /
//! PLAID_MODE selection, the auth + state Extension layers) lives in
//! `budget_ui::server`; this binary only resolves the bind address and serves.

// Server entry: native binary with the custom Axum router + Dioxus SSR.
// Per PORT-FULLSTACK-1: target-based gating — native builds get the server deps
// automatically via [target.'cfg(not(target_arch="wasm32"))'.dependencies]; no
// dx @server/--features flag is needed to suppress them on wasm32.
#[cfg(not(target_arch = "wasm32"))]
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

    // Build the fully wired Axum router (connections -> migrations -> state ->
    // router). All the startup wiring lives in `budget_ui::server`.
    let router = budget_ui::server::build_router().await?;

    // Bind address resolution:
    // - When `PORT` is set (production container, e.g. Azure Container Apps; and
    //   also `dx serve`, which injects IP+PORT), bind 0.0.0.0:$PORT. Binding all
    //   interfaces is REQUIRED behind a container ingress — 127.0.0.1 would be
    //   unreachable from the platform proxy. 0.0.0.0 still covers localhost, so
    //   `dx serve`'s proxy connects fine in dev.
    // - Otherwise (running the binary bare locally) fall back to
    //   `fullstack_address_or_localhost()`.
    let address: std::net::SocketAddr = match std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
    {
        Some(port) => std::net::SocketAddr::from(([0, 0, 0, 0], port)),
        None => dioxus_cli_config::fullstack_address_or_localhost(),
    };

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|e| anyhow::anyhow!("binding the server to {address}: {e}"))?;
    tracing::info!(%address, "budget-server listening");

    axum::serve(listener, router.into_make_service())
        .await
        .map_err(|e| anyhow::anyhow!("running the Axum server: {e}"))?;

    Ok(())
}

// Web entry: WASM hydration. dx compiles this branch for wasm32-unknown-unknown.
// `dioxus::launch` boots the client-side hydration runtime (RUST-DIOXUS-11 /
// RUST-DIOXUS-16). `budget_ui::App` is the single shared root component.
#[cfg(target_arch = "wasm32")]
fn main() {
    dioxus::launch(budget_ui::App);
}
