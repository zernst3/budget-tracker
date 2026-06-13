# Multi-stage build for the budget-tracker monolith.
#
# The Dioxus fullstack app is ONE crate (crates/budget-ui, PORT-FULLSTACK-1):
# it produces the native server binary `budget-server` (Axum + Dioxus server
# functions + SSR host) AND the wasm client bundle. This Dockerfile is the
# deploy spine for it (CI builds + pushes this image to GHCR, then Container
# Apps pulls it).
#
# NOTE: a full fullstack image must build BOTH the server bin and the wasm
# client assets. The canonical tool is `dx bundle --platform web --release`
# (the dioxus CLI), which emits the server bin + the public/ client bundle the
# server serves. The plain `cargo build` below compiles the server bin only
# (smoke check); switching the build to `dx bundle` + copying its output is the
# remaining deploy-phase step. The `--bin budget-server` artifact path below is
# already correct (the bin lives in the budget-ui package).

# ---- build stage ------------------------------------------------------------
FROM rust:1.95-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY . .

# Build the workspace in release (smoke check). For the real deploy image,
# replace this with `dx bundle --platform web --release` so the wasm client
# bundle is built alongside the `budget-server` bin (which lives in the
# budget-ui package).
RUN cargo build --release --bin budget-server

# ---- runtime stage ----------------------------------------------------------
FROM debian:bookworm-slim AS runtime

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ENV TZ=America/New_York

# Copy the server binary once it exists (frontend phase):
# COPY --from=builder /app/target/release/budget-server /usr/local/bin/budget-server

EXPOSE 8080

# CMD ["budget-server"]
# Placeholder until the server binary lands; keeps the image buildable.
CMD ["/bin/sh", "-c", "echo 'budget-tracker server binary not built yet (frontend phase)'; sleep infinity"]
