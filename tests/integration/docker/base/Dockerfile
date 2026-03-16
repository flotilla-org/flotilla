# === Cargo build stages (only executed when building 'full' target) ===
FROM rust:slim-trixie AS chef
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release

# === Shared runtime base (used by both full and dev) ===
FROM debian:trixie-slim AS runtime-base

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    openssh-client \
    git \
    ca-certificates \
    libssl3t64 \
    gosu \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /run/sshd

# Disable password auth (pubkey is already the default)
RUN mkdir -p /etc/ssh/sshd_config.d \
    && echo "PasswordAuthentication no" > /etc/ssh/sshd_config.d/flotilla.conf

RUN useradd -m -s /bin/bash flotilla \
    && mkdir -p /home/flotilla/.ssh \
    && chmod 700 /home/flotilla/.ssh \
    && chown flotilla:flotilla /home/flotilla/.ssh

COPY tests/integration/docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22
ENTRYPOINT ["/entrypoint.sh"]
CMD ["sleep", "infinity"]

# --- Full build target (default): cargo-built binary ---
FROM runtime-base AS full
COPY --from=builder /app/target/release/flotilla /usr/local/bin/flotilla

# --- Dev target: pre-built binary from host ---
FROM runtime-base AS dev
COPY target/release/flotilla /usr/local/bin/flotilla
