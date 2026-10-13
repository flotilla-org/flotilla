# Isolated link producer instructions; the composer owns FROM, audit and export.
ARG TARGETARCH
ARG NODE_VERSION=24.18.0
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/*
RUN cd /tmp \
    && case "${TARGETARCH}" in amd64) node_arch=x64 ;; arm64) node_arch=arm64 ;; *) exit 1 ;; esac \
    && node_archive="node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
    && curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/${node_archive}" \
    && curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt" \
    && grep " ${node_archive}$" SHASUMS256.txt | sha256sum --check --strict - \
    && tar -xJf "${node_archive}" -C /usr/local --strip-components=1 \
    && rm "${node_archive}" SHASUMS256.txt \
    && node --version \
    && npm --version
ARG CODEX_VERSION=0.160.0
RUN mkdir -p /out/opt/flotilla/layers/harness-codex /out/opt/flotilla/bin \
    && npm install --prefix /out/opt/flotilla/layers/harness-codex --global "@openai/codex@${CODEX_VERSION}" \
    && ln -s /opt/flotilla/layers/harness-codex/bin/codex /out/opt/flotilla/bin/codex \
    && npm cache clean --force
