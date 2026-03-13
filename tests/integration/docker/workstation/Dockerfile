# --- Install Rust tools via cargo-binstall ---
FROM rust:slim-bookworm AS cargo-installer
RUN cargo install cargo-binstall
RUN cargo binstall --no-confirm zellij shpool

# --- Install Node tools ---
FROM node:20-slim AS npm-installer
RUN npm install -g @anthropic-ai/claude-code

# --- Runtime: layer on flotilla-base ---
FROM flotilla-base

# tmux + gh dependencies via apt
RUN apt-get update && apt-get install -y --no-install-recommends \
    tmux \
    curl \
    gnupg \
    && rm -rf /var/lib/apt/lists/*

# gh (GitHub CLI) — official apt repo
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    | tee /etc/apt/sources.list.d/github-cli.list > /dev/null \
    && apt-get update \
    && apt-get install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

# claude CLI from npm-installer stage
COPY --from=npm-installer /usr/local/lib/node_modules /usr/local/lib/node_modules
COPY --from=npm-installer /usr/local/bin/node /usr/local/bin/node
RUN ln -s /usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js /usr/local/bin/claude

# zellij + shpool from cargo-installer stage
COPY --from=cargo-installer /usr/local/cargo/bin/zellij /usr/local/bin/zellij
COPY --from=cargo-installer /usr/local/cargo/bin/shpool /usr/local/bin/shpool
