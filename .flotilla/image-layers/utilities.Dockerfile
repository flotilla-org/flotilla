# Builder stage: generate xterm-ghostty terminfo from cleat's pinned source.
ARG BASE
FROM ${BASE} AS ghostty-builder
ARG ZIG_VERSION=0.16.0
ARG CLEAT_REF=e233d6d244da0ba495858582c9364ad062572159
RUN apt-get update \
    && apt-get install -y --no-install-recommends git build-essential ncurses-bin \
    && rm -rf /var/lib/apt/lists/*
COPY ci/crew-image/emit-ghostty-terminfo.zig /tmp/emit-ghostty-terminfo.zig
# Cleat's preparation script reads the Ghostty revision from its own
# tools/ghostty-toolchain.toml. Verify Zig matches that same pin.
RUN git clone --filter=blob:none --no-checkout https://github.com/flotilla-org/cleat.git /src/cleat \
    && cd /src/cleat \
    && git fetch --depth 1 origin "${CLEAT_REF}" \
    && git checkout --detach FETCH_HEAD \
    && test "$(sed -n '/^\[zig\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' tools/ghostty-toolchain.toml)" = "${ZIG_VERSION}" \
    && ./tools/prepare-ghostty-vt.sh \
    && cd /src/cleat/.tools/ghostty-src \
    && cp /tmp/emit-ghostty-terminfo.zig src/emit-ghostty-terminfo.zig \
    && zig run src/emit-ghostty-terminfo.zig > /tmp/ghostty.terminfo \
    && mkdir -p /src/cleat/.tools/ghostty-install/share/terminfo \
    && tic -x -o /src/cleat/.tools/ghostty-install/share/terminfo /tmp/ghostty.terminfo \
    && test -f /src/cleat/.tools/ghostty-install/share/terminfo/x/xterm-ghostty

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
ENV PATH="/opt/flotilla/bin:/tmp/flotilla-config/cargo/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

# Ghostty terminfo comes from the same pinned source as libghostty-vt (see
# CLEAT_REF and tools/ghostty-toolchain.toml in that checkout). It is MIT
# licensed; retain the source licence alongside the installed database.
COPY --from=ghostty-builder /src/cleat/.tools/ghostty-install/share/terminfo/ /usr/share/terminfo/
COPY --from=ghostty-builder /src/cleat/.tools/ghostty-src/LICENSE /usr/share/doc/ghostty-terminfo/copyright
RUN infocmp xterm-ghostty >/dev/null \
    && infocmp xterm-256color >/dev/null

ENV SHELL=/bin/zsh

RUN curl -LsSf https://astral.sh/uv/install.sh \
    | env UV_INSTALL_DIR=/usr/local/bin INSTALLER_NO_MODIFY_PATH=1 sh

# Build-time smoke check: declaring these adapters in a PlacementPolicy is a
# promise that both entry points are executable in the image.
RUN tea --version \
    && python3 --version \
    && uv --version \
    && strace --version \
    && clang --version \
    && ld.lld --version \
    && make --version \
    && pkg-config --version \
    && zig version \
    && test -z "${CLEAT_GHOSTTY_PREFIX+x}" \
    && test ! -e /opt/ghostty-install \
    && printf '#include <stdio.h>\nint main(void) { puts("c toolchain ready"); }\n' \
        | clang -fuse-ld=lld -x c - -o /tmp/c-toolchain-smoke \
    && /tmp/c-toolchain-smoke \
    && rm /tmp/c-toolchain-smoke

# Build-time smoke check: an arbitrary non-root runtime UID honours a
# checkout's rust-toolchain.toml through the baked pins and can write
# RUSTUP_HOME for pins or components the image does not bake.
RUN setpriv --reuid=12345 --regid=12345 --clear-groups sh -euc ' \
        test -w "${RUSTUP_HOME}/toolchains" \
        && test -w "${RUSTUP_HOME}/update-hashes" \
        && dir="$(mktemp -d)" \
        && cp /opt/flotilla/rust-toolchain.toml "${dir}/rust-toolchain.toml" \
        && (cd "${dir}" && . /opt/flotilla/pin.sh && assert_rust_pin rust-toolchain.toml && cargo clippy --version) \
        && rm -rf "${dir}"'

WORKDIR /workspace
CMD ["sleep", "infinity"]
