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

# Build the real binary; dependency artifacts above are reused. The example
# file is the built-in configuration (`--defaults`), compiled in.
COPY src ./src
COPY sanki-bot.example.toml ./
RUN touch src/lib.rs src/bin/sanki-bot.rs && cargo build --release --locked

# ---- Runtime stage ------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# CA certificates validate TLS for the `wss://` relay and the module's host.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged, no-home system user.
RUN useradd --system --user-group --no-create-home sanki

# The bot's data root (`HOME` below: the key, the lease, the rules cache,
# the engine's working directory) — mount a volume there so a restart never
# depends on the relay or the module's host, and the key survives. The
# configuration is mounted read-only by the deployment; the engine, if any,
# is the deployment's to install (SEI leaves the process's confinement to
# it). A configuration setting its own paths keeps them out of each other
# (the engine's `cwd` outside `data_dir` and the key's directory).
RUN mkdir -p /var/lib/sanki && chown sanki:sanki /var/lib/sanki
VOLUME ["/var/lib/sanki"]

COPY --from=builder /build/target/release/sanki-bot /usr/local/bin/sanki-bot

USER sanki
# The data root of the built-in paths (`sanki-bot` with no `--config`).
ENV HOME=/var/lib/sanki

# `sanki-bot --config /etc/sanki/bot.toml`; RUST_LOG is optional. No
# secrets are baked into the image: the key file is created at the first
# start, in the volume.
ENTRYPOINT ["sanki-bot"]
CMD ["--config", "/etc/sanki/bot.toml"]
