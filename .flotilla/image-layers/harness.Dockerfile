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

FROM ${BASE}
# 4. Agent adapters. Keep this fastest-moving layer last.
ARG CLAUDE_CODE_VERSION=2.1.287
ARG CODEX_VERSION=0.160.0

RUN npm install --global \
        "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
        "@openai/codex@${CODEX_VERSION}" \
    && npm cache clean --force

# Ghostty terminfo comes from the same pinned source as libghostty-vt (see
# CLEAT_REF and tools/ghostty-toolchain.toml in that checkout). It is MIT
# licensed; retain the source licence alongside the installed database.
COPY --from=ghostty-builder /src/cleat/.tools/ghostty-install/share/terminfo/ /usr/share/terminfo/
COPY --from=ghostty-builder /src/cleat/.tools/ghostty-src/LICENSE /usr/share/doc/ghostty-terminfo/copyright
RUN infocmp xterm-ghostty >/dev/null \
    && infocmp xterm-256color >/dev/null

ENV DISABLE_AUTOUPDATER=1
ENV SHELL=/bin/zsh

RUN curl -LsSf https://astral.sh/uv/install.sh \
    | env UV_INSTALL_DIR=/usr/local/bin INSTALLER_NO_MODIFY_PATH=1 sh

# Build-time smoke check: declaring these adapters in a PlacementPolicy is a
# promise that both entry points are executable in the image.
RUN claude --version \
    && codex --version \
    && tea --version \
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
