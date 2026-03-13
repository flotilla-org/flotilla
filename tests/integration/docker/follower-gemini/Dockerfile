# --- Install gemini CLI in Node 20 environment ---
FROM node:20-slim AS npm-installer
RUN npm install -g @google/gemini-cli

# --- Runtime: layer on flotilla-base ---
FROM flotilla-base
COPY --from=npm-installer /usr/local/lib/node_modules /usr/local/lib/node_modules
COPY --from=npm-installer /usr/local/bin/node /usr/local/bin/node
RUN ln -s /usr/local/lib/node_modules/@google/gemini-cli/dist/index.js /usr/local/bin/gemini
