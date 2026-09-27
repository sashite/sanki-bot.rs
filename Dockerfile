# syntax=docker/dockerfile:1

# ---- Build stage --------------------------------------------------------------
# Pinned to the toolchain declared in rust-toolchain.toml.
FROM rust:1.96-slim-bookworm AS builder
WORKDIR /build

# Compile dependencies first, against stubs, so they stay cached across
# source-only changes. `--locked` enforces the committed Cargo.lock.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir -p src/bin \
    && echo '' > src/lib.rs \
    && echo 'fn main() {}' > src/bin/sanki-bot.rs \
    && cargo build --release --locked \
    && rm -rf src

# Build the real binary; dependency artifacts above are reused.
COPY src ./src
RUN touch src/lib.rs src/bin/sanki-bot.rs && cargo build --release --locked

# ---- Runtime stage ------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# CA certificates validate TLS for the `wss://` relay and the module's host.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged, no-home system user.
RUN useradd --system --user-group --no-create-home sanki

# The bot's data directory (`connection.data_dir`: the lease, the rule
# system's cache) — mount a volume there so a restart never depends on the
# relay or the module's host. The configuration and the key file are
# mounted read-only by the deployment; the engine, if any, is the
# deployment's to install (SEI leaves the process's confinement to it).
RUN mkdir -p /var/lib/sanki && chown sanki:sanki /var/lib/sanki
VOLUME ["/var/lib/sanki"]

COPY --from=builder /build/target/release/sanki-bot /usr/local/bin/sanki-bot

USER sanki

# `sanki-bot CONFIG.toml`; RUST_LOG is optional. No secrets are baked into
# the image: the key file is the configuration's `identity.file`.
ENTRYPOINT ["sanki-bot"]
CMD ["/etc/sanki/bot.toml"]
