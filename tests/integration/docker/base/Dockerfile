FROM rust:slim-bookworm AS chef
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

FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-server \
    openssh-client \
    git \
    ca-certificates \
    libssl3 \
    gosu \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir /run/sshd

# Permit pubkey auth, disable password auth
RUN sed -i 's/#PubkeyAuthentication yes/PubkeyAuthentication yes/' /etc/ssh/sshd_config \
    && sed -i 's/#PasswordAuthentication yes/PasswordAuthentication no/' /etc/ssh/sshd_config

RUN useradd -m -s /bin/bash flotilla \
    && mkdir -p /home/flotilla/.ssh \
    && chmod 700 /home/flotilla/.ssh \
    && chown flotilla:flotilla /home/flotilla/.ssh

COPY --from=builder /app/target/release/flotilla /usr/local/bin/flotilla
COPY tests/integration/docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22
ENTRYPOINT ["/entrypoint.sh"]
CMD ["sleep", "infinity"]
