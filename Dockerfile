# Multi-stage build for the budget-tracker monolith.
#
# The server binary (Axum + Dioxus server functions) lands in the frontend
# phase under crates/budget-server; this Dockerfile is the deploy spine for it
# (CI builds + pushes this image to GHCR, then Container Apps pulls it).
# Until that binary exists, the build stage compiles the workspace as a
# smoke check; the runtime stage is ready for the binary once it lands.

# ---- build stage ------------------------------------------------------------
FROM rust:1.95-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY . .

# Build the whole workspace in release. Once crates/budget-server exists,
# narrow this to `--bin budget-server` and copy that artifact below.
RUN cargo build --release --workspace

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
