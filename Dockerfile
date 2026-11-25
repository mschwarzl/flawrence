# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder
ARG COMMITRAIDER_REV=0a5eb9784e4506e0acee0cbeef5e6a4c53f27502
RUN cargo install --locked --git https://github.com/mschwarzl/commitraider \
        --rev "${COMMITRAIDER_REV}" commitraider
WORKDIR /src
COPY . .
RUN cargo build --release --locked --bin flawrence-runner --bin flawrence-web --bin flawrence-index-cli

FROM node:22-bookworm-slim AS frontend
WORKDIR /frontend
COPY frontend/package*.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

FROM eclipse-temurin:26-jdk-noble AS tools

ARG UBUNTU_MIRROR
ARG ANDROID_COMMAND_LINE_TOOLS_VERSION=15859902
ARG ANDROID_COMMAND_LINE_TOOLS_SHA256=4e4c464f145a7512b57d088ac6c278c03c9eea610886b35a5e0804e74eedf583
ARG ANDROID_PLATFORM=36
ARG ANDROID_BUILD_TOOLS=36.0.0
ARG GRADLE_VERSION=9.7.1
ARG GRADLE_SHA256=acd53f1edaf02f1a8ff99879f8a34b302661a057d9b063ae9e35b552f804d20a
ARG KOTLIN_VERSION=2.4.10
ARG KOTLIN_SHA256=473dd66c7a3ef4b182065b3da670466c1bf2773a9dbb0ed8b33a39fe9d4f876d

ENV ANDROID_HOME=/opt/android-sdk \
    ANDROID_SDK_ROOT=/opt/android-sdk \
    GRADLE_USER_HOME=/root/.gradle \
    FLAWRENCE_JAVA_INSTALLATIONS=/opt/jdk-21,/opt/jdk-26 \
    FLAWRENCE_IN_CONTAINER=1 \
    TRAILMARK_BIN=/opt/trailmark/bin/trailmark \
    XDG_CACHE_HOME=/opt/trailmark-cache \
    FLAWRENCE_AGENTS_DIR=/usr/local/bin/agents \
    FLAWRENCE_SKILLS_DIR=/usr/local/bin/skills \
    FLAWRENCE_ARTIFACTS_BASE_DIR=/artifacts \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/opt/android-sdk/cmdline-tools/latest/bin:/opt/android-sdk/platform-tools:/opt/trailmark/bin:/opt/gradle/bin:/opt/kotlin/bin:${PATH}

# HTTPS avoids HTTP forwarding failures on the Ubuntu ports endpoint. A mirror
# override must carry every configured suite, including noble-security.
RUN sed -i 's|http://|https://|g' /etc/apt/sources.list.d/ubuntu.sources && \
    if [ -n "${UBUNTU_MIRROR}" ]; then \
        sed -i "s|^URIs:.*|URIs: ${UBUNTU_MIRROR}|" /etc/apt/sources.list.d/ubuntu.sources; \
    fi && \
    printf 'Acquire::Retries "3";\n' > /etc/apt/apt.conf.d/80-flawrence-retries && \
    apt-get update --error-on=any && apt-get install -y --no-install-recommends \
        bash build-essential ca-certificates clang cmake curl git git-lfs jq \
        libffi-dev libsqlite3-dev lld maven ninja-build \
        openjdk-21-jdk-headless pkg-config python3 ripgrep screen unzip zip zlib1g-dev && \
    rm -rf /var/lib/apt/lists/*

# Two JDKs, and 21 is the default on purpose: Gradle's support for a new JDK
# lags its release by months, so a project whose wrapper predates the image's
# JDK dies while Groovy is still parsing the build script. Broad support first.
# 26 stays installed and registered, so a project that declares a Java toolchain
# needing it still gets it -- Gradle picks per project from the paths the
# pre-warm records, rather than everything running on whatever is on PATH.
# Symlinked to stable names because the package installs under an
# architecture-suffixed directory.
RUN ln -s "/usr/lib/jvm/java-21-openjdk-$(dpkg --print-architecture)" /opt/jdk-21 && \
    ln -s "${JAVA_HOME}" /opt/jdk-26 && \
    /opt/jdk-21/bin/java -version && \
    /opt/jdk-26/bin/java -version

ENV JAVA_HOME=/opt/jdk-21 \
    PATH=/opt/jdk-21/bin:${PATH}

# The command-line manager is Java, but Google's Linux SDK native binaries
# (aapt2, adb, NDK, etc.) are x86-64-only. See README's ARM limitation.
RUN curl --fail --location --show-error \
        "https://dl.google.com/android/repository/commandlinetools-linux-${ANDROID_COMMAND_LINE_TOOLS_VERSION}_latest.zip" \
        --output /tmp/android-tools.zip && \
    echo "${ANDROID_COMMAND_LINE_TOOLS_SHA256}  /tmp/android-tools.zip" | sha256sum --check && \
    mkdir -p "${ANDROID_HOME}/cmdline-tools" && \
    unzip -q /tmp/android-tools.zip -d /tmp/android-tools && \
    mv /tmp/android-tools/cmdline-tools "${ANDROID_HOME}/cmdline-tools/latest" && \
    rm -rf /tmp/android-tools /tmp/android-tools.zip && \
    yes | sdkmanager --licenses >/dev/null && \
    sdkmanager \
        "build-tools;${ANDROID_BUILD_TOOLS}" \
        "platform-tools" \
        "platforms;android-${ANDROID_PLATFORM}"

# Kotlin toolchain: Gradle and the Kotlin compiler, pinned and checksummed the
# same way as the Android tools above. The JDK comes from the base image.
# Kotlin/Native is deliberately not preinstalled (~217MB); Gradle fetches it on
# demand for multiplatform native targets.
RUN curl --fail --location --show-error \
        "https://services.gradle.org/distributions/gradle-${GRADLE_VERSION}-bin.zip" \
        --output /tmp/gradle.zip && \
    echo "${GRADLE_SHA256}  /tmp/gradle.zip" | sha256sum --check && \
    unzip -q /tmp/gradle.zip -d /opt && \
    mv "/opt/gradle-${GRADLE_VERSION}" /opt/gradle && \
    rm /tmp/gradle.zip && \
    curl --fail --location --show-error \
        "https://github.com/JetBrains/kotlin/releases/download/v${KOTLIN_VERSION}/kotlin-compiler-${KOTLIN_VERSION}.zip" \
        --output /tmp/kotlin.zip && \
    echo "${KOTLIN_SHA256}  /tmp/kotlin.zip" | sha256sum --check && \
    unzip -q /tmp/kotlin.zip -d /opt && \
    mv /opt/kotlinc /opt/kotlin && \
    rm /tmp/kotlin.zip && \
    gradle --version && \
    kotlinc -version

COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /usr/local/bin/
RUN UV_PYTHON_INSTALL_DIR=/opt/uv-python uv python install 3.13 && \
    UV_PYTHON_INSTALL_DIR=/opt/uv-python uv venv --python 3.13 /opt/trailmark && \
    UV_PYTHON_INSTALL_DIR=/opt/uv-python uv pip install --python /opt/trailmark/bin/python \
        'trailmark==0.5.*' && \
    /opt/trailmark/bin/python -c \
        'import tree_sitter_language_pack as p; p.prefetch(["tact", "proto", "ruby", "cpp", "csharp", "php", "move", "swift", "erlang", "thrift", "cairo", "objc", "python", "haskell", "go", "graphql", "solidity", "dart", "kotlin", "javascript", "func", "java", "rego", "c", "typescript", "sway", "rust"])'

FROM tools

COPY --from=builder /src/target/release/flawrence-runner /usr/local/bin/
COPY --from=builder /src/target/release/flawrence-web /usr/local/bin/
COPY --from=frontend /frontend/dist /usr/local/bin/frontend/dist
COPY --from=builder /src/target/release/flawrence-index-cli /usr/local/bin/
COPY --from=builder /usr/local/cargo/bin /usr/local/cargo/bin
COPY --from=builder /usr/local/rustup /usr/local/rustup
COPY agents /usr/local/bin/agents
COPY skills /usr/local/bin/skills

RUN mkdir -p /workspace /artifacts

WORKDIR /workspace
ENV FLAWRENCE_WEB_MAX_EVENTS=1000
EXPOSE 8080
CMD ["flawrence-web", "--host", "0.0.0.0"]
