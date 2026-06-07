# Budget Tracker

A single-user, self-hosted personal budget app built in Rust (Axum + SeaORM monolith, Dioxus + [chorale](https://github.com/zernst3/rust-chorale) UI), deployed to Azure Container Apps with a Neon Postgres backend, all under Terraform.

It replaces a long-running spreadsheet budget. The distinguishing feature is a **rolling "Other" balance**: each month's net leftover carries into the next as an auditable, system-generated transaction, with bank data pulled read-only via Plaid.

## Status

In development via an orchestrated, model-tiered build routine (manually triggered). Built backend-first, then the UI.

## Docs

- [`SPEC.md`](SPEC.md) — the authoritative, Phase-1-resolved spec (see §12 for resolved decisions).
- [`CONVENTIONS.md`](CONVENTIONS.md) — the project-local `BUDGET-*` rules.
- [`PHASE_1_REPORT.md`](PHASE_1_REPORT.md) — the planning/investigation report.
- [`DECISIONS.md`](DECISIONS.md) — Phase-1 decision resolutions.

This project is built by orchestrating AI under documented convention rules; see `SPEC.md` and `CONVENTIONS.md`.
