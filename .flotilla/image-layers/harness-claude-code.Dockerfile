ARG BASE
FROM ${BASE}
ARG CLAUDE_CODE_VERSION=2.1.287
RUN npm install --global "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
    && npm cache clean --force \
    && claude --version
ENV DISABLE_AUTOUPDATER=1
