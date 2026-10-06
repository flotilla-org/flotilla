# --- Install codex CLI ---
FROM node:20-slim AS npm-installer
RUN npm install -g @openai/codex

# --- Runtime: layer on flotilla-base ---
FROM flotilla-base

COPY --from=npm-installer /usr/local/lib/node_modules /usr/local/lib/node_modules
COPY --from=npm-installer /usr/local/bin/node /usr/local/bin/node
RUN ln -s ../lib/node_modules/@openai/codex/bin/codex.js /usr/local/bin/codex

# Placeholder codex auth file for CodexAuthDetector
RUN mkdir -p /home/flotilla/.codex \
    && echo '{"auth_mode":"placeholder"}' > /home/flotilla/.codex/auth.json \
    && chown -R flotilla:flotilla /home/flotilla/.codex
