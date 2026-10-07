# Base OS and toolchains. Sources and defaults: inputs.json.
ARG BASE=ubuntu:24.04
ARG ZIG_VERSION=0.16.0
FROM ${BASE} AS zig-toolchain
ARG TARGETARCH
ARG ZIG_VERSION
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq xz-utils \
    && rm -rf /var/lib/apt/lists/*
RUN case "${TARGETARCH}" in \
        amd64) zig_arch=x86_64 ;; \
        arm64) zig_arch=aarch64 ;; \
        *) echo "unsupported Zig architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && zig_archive="zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz" \
    && curl -fsSL "https://ziglang.org/download/index.json" -o /tmp/zig-index.json \
    && zig_sha="$(jq -er --arg version "${ZIG_VERSION}" --arg target "${zig_arch}-linux" '.[$version][$target].shasum' /tmp/zig-index.json)" \
    && curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/${zig_archive}" -o "/tmp/${zig_archive}" \
    && printf '%s  %s\n' "${zig_sha}" "/tmp/${zig_archive}" | sha256sum --check --strict - \
    && mkdir -p /opt/zig \
    && tar -xJf "/tmp/${zig_archive}" -C /opt/zig --strip-components=1 \
    && ln -s /opt/zig/zig /usr/local/bin/zig \
    && rm "/tmp/${zig_archive}" /tmp/zig-index.json \
    && test "$(zig version)" = "${ZIG_VERSION}"

FROM ${BASE}

ARG DEBIAN_FRONTEND=noninteractive

# 1. Base OS, trust store, and source/network clients.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
    && rm -rf /var/lib/apt/lists/*

# 2. Language toolchains used to verify crew work.
# Temporary formatter until #2794 lands.
COPY rust-toolchain.toml /opt/flotilla/rust-toolchain.toml
COPY ci/toolchain/pin.sh /opt/flotilla/pin.sh
ARG NODE_VERSION=24.18.0
ARG ZIG_VERSION
ARG TARGETARCH

ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV PATH="/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        clang \
        libc6-dev \
        libegl-dev \
        libfontconfig-dev \
        libfreetype-dev \
        libgl-dev \
        libssl-dev \
        libx11-dev \
        libxcb1-dev \
        libxcursor-dev \
        libxext-dev \
        libxfixes-dev \
        libxi-dev \
        libxinerama-dev \
        libxkbcommon-dev \
        libxkbcommon-x11-dev \
        libxrandr-dev \
        libxrender-dev \
        libxss-dev \
        linux-libc-dev \
        lld \
        make \
        ncurses-bin \
        pkg-config \
        xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && . /opt/flotilla/pin.sh \
    && RUST_VERSION="$(read_rust_pin /opt/flotilla/rust-toolchain.toml)" \
    && curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}" \
    && (cd /opt/flotilla && rustup show active-toolchain) \
    && rm -rf "${RUSTUP_HOME}/downloads" "${RUSTUP_HOME}/tmp" \
    && chmod -R a+rwX "${RUSTUP_HOME}"

RUN case "${TARGETARCH}" in \
        amd64) node_arch=x64 ;; \
        arm64) node_arch=arm64 ;; \
        *) echo "unsupported Node.js architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && node_archive="node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
    && curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/${node_archive}" \
    && curl -fsSLO "https://nodejs.org/dist/v${NODE_VERSION}/SHASUMS256.txt" \
    && grep " ${node_archive}$" SHASUMS256.txt | sha256sum --check --strict - \
    && tar -xJf "${node_archive}" -C /usr/local --strip-components=1 \
    && rm "${node_archive}" SHASUMS256.txt \
    && node --version \
    && npm --version

COPY --from=zig-toolchain /opt/zig /opt/zig
RUN ln -s /opt/zig/zig /usr/local/bin/zig \
    && test "$(zig version)" = "${ZIG_VERSION}"
