//! `budget-server` — the native Axum fullstack host for the `budget-ui` app.
//!
//! One process serves three concerns (`RUST-DIOXUS-11`): it server-renders the
//! initial HTML, serves the hydrating wasm client bundle + static assets, and
//! mounts the server-function endpoints declared in `budget_ui::services`
//! (`RUST-DIOXUS-9`). The router is built explicitly here so the server-function
//! plumbing is visible at the entrypoint, then handed to `axum::serve`.
//!
//! This binary depends on `budget-ui` with the `server` feature, which pulls the
//! app-services + infrastructure stack into the graph; the server-function
//! bodies call those layers directly. The companion admin CLIs (`provision-user`,
//! `seed-onboarding`) are separate `[[bin]]` targets under `src/bin/`.
//!
//! The bind address is supplied by the `dx` CLI / the deploy environment via
//! `dioxus-cli-config`, falling back to localhost for a bare `cargo run`.

// The fullstack integration's documented surface uses fallible setup at the app
// edge; anyhow is the binary-edge error type (RUST-DOMAIN-4).
use anyhow::Context;
use dioxus_server::{DioxusRouterExt, ServeConfig};

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

    // The bind address the `dx` CLI / Container Apps injects; localhost for a
    // bare `cargo run`.
    let address = dioxus_cli_config::fullstack_address_or_localhost();

    // Build the Axum router: SSR + static client bundle + server-function
    // endpoints, all registered by `serve_dioxus_application` (RUST-DIOXUS-11).
    // `budget_ui::App` is the single shared root component (RUST-DIOXUS-16).
    let config = ServeConfig::new();
    let router = axum::Router::new().serve_dioxus_application(config, budget_ui::App);

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding the server to {address}"))?;
    tracing::info!(%address, "budget-server listening");

    axum::serve(listener, router.into_make_service())
        .await
        .context("running the Axum server")?;

    Ok(())
}
