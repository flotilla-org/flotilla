ARG BASE
FROM ${BASE}
ARG CODEX_VERSION=0.160.0
RUN npm install --global "@openai/codex@${CODEX_VERSION}" \
    && npm cache clean --force \
    && codex --version
