# --- Install shpool via cargo-binstall ---
FROM rust:slim-bookworm AS cargo-installer
RUN cargo install cargo-binstall
RUN cargo binstall --no-confirm shpool

# --- Install codex CLI ---
FROM node:20-slim AS npm-installer
RUN npm install -g @openai/codex

# --- Runtime: layer on flotilla-base ---
FROM flotilla-base

# codex CLI from npm-installer stage (copy node, modules, and npm-generated wrapper)
COPY --from=npm-installer /usr/local/lib/node_modules /usr/local/lib/node_modules
COPY --from=npm-installer /usr/local/bin/node /usr/local/bin/node
COPY --from=npm-installer /usr/local/bin/codex /usr/local/bin/codex

# Placeholder codex auth file for CodexAuthDetector
RUN mkdir -p /home/flotilla/.codex \
    && echo '{"auth_mode":"placeholder"}' > /home/flotilla/.codex/auth.json \
    && chown -R flotilla:flotilla /home/flotilla/.codex

# shpool from cargo-installer stage
COPY --from=cargo-installer /usr/local/cargo/bin/shpool /usr/local/bin/shpool
