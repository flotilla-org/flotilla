# Formal modelling tools. Version/checksum sources: inputs.json.
ARG BASE
FROM ${BASE}
ARG TARGETARCH
ARG JDK_VERSION=21.0.12.1+1
ARG QUINT_VERSION=0.33.0
ARG TLC_VERSION=1.7.4
ARG APALACHE_VERSION=0.62.1
ARG QUINT_EVALUATOR_VERSION=0.7.0
ARG JDK_AMD64_SHA256=ce79869e1307ed8ee1e2baa86a412b1eb5b75d10a01006d788a6f968bcfaee94
ARG JDK_ARM64_SHA256=23e37e026f12f3e706f18938ff611db3032d075b09d0879a25d06718c773e223
ARG EVALUATOR_AMD64_SHA256=869adb8762a518483eb39f67fde622253a58527494e15afe77d610a0b6dab7d2
ARG EVALUATOR_ARM64_SHA256=26c052831243cdba9cd81f8da0c1df053f3c92cfd3fd8f050c87c45642414f93
ARG APALACHE_SHA256=38202a8c4ebeb3ab73e0fe5a7e4bb948442d46ad5de8a7fb45c9e5cac2beb2a6
ARG TLC_SHA256=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88

ENV JAVA_HOME=/opt/java
ENV QUINT_HOME=/opt/quint
ENV PATH="/opt/java/bin:/opt/quint/bin:${PATH}"
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
RUN case "${TARGETARCH}" in \
        amd64) java_arch=x64; evaluator_arch=x86_64; java_sha="${JDK_AMD64_SHA256}"; evaluator_sha="${EVALUATOR_AMD64_SHA256}" ;; \
        arm64) java_arch=aarch64; evaluator_arch=aarch64; java_sha="${JDK_ARM64_SHA256}"; evaluator_sha="${EVALUATOR_ARM64_SHA256}" ;; \
        *) echo "unsupported formal-modelling architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && jdk_archive_version="$(printf '%s' "${JDK_VERSION}" | tr '+' '_')" \
    && curl -fsSL "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-${JDK_VERSION}/OpenJDK21U-jdk_${java_arch}_linux_hotspot_${jdk_archive_version}.tar.gz" -o /tmp/jdk.tar.gz \
    && printf '%s  %s\n' "${java_sha}" /tmp/jdk.tar.gz | sha256sum --check --strict - \
    && mkdir -p "${JAVA_HOME}" "${QUINT_HOME}/rust-evaluator-v${QUINT_EVALUATOR_VERSION}" "${QUINT_HOME}/apalache-dist-${APALACHE_VERSION}" "${QUINT_HOME}/bin" \
    && tar -xzf /tmp/jdk.tar.gz -C "${JAVA_HOME}" --strip-components=1 \
    && curl -fsSL "https://github.com/quint-co/quint/releases/download/evaluator/v${QUINT_EVALUATOR_VERSION}/quint_evaluator-${evaluator_arch}-unknown-linux-gnu.tar.gz" -o /tmp/evaluator.tar.gz \
    && printf '%s  %s\n' "${evaluator_sha}" /tmp/evaluator.tar.gz | sha256sum --check --strict - \
    && tar -xzf /tmp/evaluator.tar.gz -C "${QUINT_HOME}/rust-evaluator-v${QUINT_EVALUATOR_VERSION}" \
    && curl -fsSL "https://github.com/apalache-mc/apalache/releases/download/v${APALACHE_VERSION}/apalache.tgz" -o /tmp/apalache.tgz \
    && printf '%s  %s\n' "${APALACHE_SHA256}" /tmp/apalache.tgz | sha256sum --check --strict - \
    && tar -xzf /tmp/apalache.tgz -C "${QUINT_HOME}/apalache-dist-${APALACHE_VERSION}" \
    && curl -fsSL "https://github.com/tlaplus/tlaplus/releases/download/v${TLC_VERSION}/tla2tools.jar" -o "${QUINT_HOME}/tla2tools.jar" \
    && printf '%s  %s\n' "${TLC_SHA256}" "${QUINT_HOME}/tla2tools.jar" | sha256sum --check --strict - \
    && ln -s "${QUINT_HOME}/apalache-dist-${APALACHE_VERSION}/apalache/bin/apalache-mc" "${QUINT_HOME}/bin/apalache-mc" \
    && npm install --global "@informalsystems/quint@${QUINT_VERSION}" \
    && npm cache clean --force \
    && rm /tmp/jdk.tar.gz /tmp/evaluator.tar.gz /tmp/apalache.tgz \
    && chmod -R a+rX,a-w "${JAVA_HOME}" "${QUINT_HOME}"
COPY .flotilla/image-layers/60-formal-modelling.sh /etc/flotilla/prelude.d/60-formal-modelling.sh
COPY .flotilla/image-layers/formal-modelling/smoke.qnt /opt/flotilla/formal-modelling/smoke.qnt
COPY .flotilla/image-layers/formal-modelling/smoke.tla /opt/flotilla/formal-modelling/smoke.tla
COPY .flotilla/image-layers/formal-modelling/smoke.cfg /opt/flotilla/formal-modelling/smoke.cfg
COPY .flotilla/image-layers/formal-modelling/probe.sh /opt/flotilla/formal-modelling/probe.sh
COPY .flotilla/image-layers/formal-modelling/tlc /usr/local/bin/tlc
RUN chmod -R a+rX,a-w /opt/flotilla/formal-modelling \
    && chmod a-w /usr/local/bin/tlc /etc/flotilla/prelude.d/60-formal-modelling.sh
# Exercise the same baked paths as admission; runtime acceptance also runs offline
# as an arbitrary non-root user, so an accidental auto-download cannot pass.
RUN /opt/flotilla/formal-modelling/probe.sh all
