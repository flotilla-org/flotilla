ARG BASE
FROM ${BASE}
ARG TARGETARCH
# 3. Forge CLI and day-to-day utilities.
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        file \
        gh \
        jq \
        less \
        openssh-client \
        procps \
        python3 \
        ripgrep \
        strace \
        unzip \
        zsh \
    && rm -rf /var/lib/apt/lists/*

# Forgejo/Gitea CLI for the lab hub (project-map and lab forks live on
# forgejo.lab.flotilla.work; `gh` only covers GitHub). tea ships arch-named
# static binaries; TARGETARCH (amd64/arm64) matches its asset naming directly
# and is already in scope from the Node.js layer above. Verify the download's
# published SHA-256, matching the Node.js integrity pattern in this file.
ARG TEA_VERSION=0.16.0
RUN cd /tmp \
    && asset="tea-${TEA_VERSION}-linux-${TARGETARCH}" \
    && curl -fsSLO "https://dl.gitea.com/tea/${TEA_VERSION}/${asset}" \
    && curl -fsSLO "https://dl.gitea.com/tea/${TEA_VERSION}/${asset}.sha256" \
    && sha256sum --check --strict "${asset}.sha256" \
    && install -m 0755 "${asset}" /usr/local/bin/tea-real \
    && rm -f "${asset}" "${asset}.sha256" \
    && tea-real --version
COPY ci/crew-image/tea-crew.py /usr/local/bin/tea
RUN chmod 0755 /usr/local/bin/tea && tea --version

# Cargo's registry/cache and rustup-installed proxies must be writable by the
# host-mapped runtime user, so Cargo's mutable home lives beneath Flotilla's
# writable base. RUSTUP_HOME stays at /usr/local/rustup, made writable by any
# runtime UID in the toolchain layer above (containers are per vessel), so a
# checkout's rust-toolchain.toml can install its pinned toolchain or missing
# components on demand.
ENV CARGO_HOME=/tmp/flotilla-config/cargo
ENV PATH="/tmp/flotilla-config/cargo/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
