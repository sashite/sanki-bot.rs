# syntax=docker/dockerfile:1

# ---- Build stage --------------------------------------------------------------
# Pinned to the toolchain declared in rust-toolchain.toml.
FROM rust:1.96-slim-bookworm AS builder
WORKDIR /build

# Compile dependencies first, against a stub binary, so they stay cached across
# source-only changes. `--locked` enforces the committed Cargo.lock.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

# Build the real binary; dependency artifacts above are reused.
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

# ---- Runtime stage ------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# CA certificates are needed to validate TLS for `wss://` relay connections.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged, no-home system user.
RUN useradd --system --user-group --no-create-home players

# The rule system's event and module are cached where the fleet file's
# `rules_cache_dir` points (ADR-0034): mount a volume there so a restart never
# depends on the relay or the blob host.
RUN mkdir -p /var/lib/sashite/rules && chown players:players /var/lib/sashite/rules
VOLUME ["/var/lib/sashite/rules"]

COPY --from=builder /build/target/release/players /usr/local/bin/players

USER players

# FLEET_CONFIG_PATH and the per-bot PLAYER_NSEC_* variables must be supplied at
# runtime (config mounted, keys via secrets); RUST_LOG is optional. No secrets
# are baked into the image.
ENTRYPOINT ["players"]
