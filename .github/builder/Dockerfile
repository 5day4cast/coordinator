# The release builder: toolchain, static OpenSSL, and the Linux and browser WASM release
# dependencies, precompiled with cargo-chef. The release jobs copy the source into /build
# and compile only the workspace's own crates. See docs/BUILD_SPEED.md.
FROM ubuntu:24.04@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3 AS toolchain

ARG RUST_VERSION=1.98.1
ARG RUSTUP_VERSION=1.29.1
ARG RUSTUP_SHA256=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71
ARG CARGO_CHEF_VERSION=0.1.78
ARG WASM_BINDGEN_VERSION

ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    CARGO_TERM_COLOR=always \
    SQLX_OFFLINE=true

# git for actions/checkout, python3 for scripts/release.py, clang for the WASM C code.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential pkg-config perl curl ca-certificates git python3 clang \
 && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -fsSL -o /tmp/rustup-init \
      "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/x86_64-unknown-linux-gnu/rustup-init" \
 && echo "$RUSTUP_SHA256  /tmp/rustup-init" | sha256sum -c - \
 && chmod +x /tmp/rustup-init \
 && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "$RUST_VERSION" \
      --target x86_64-unknown-linux-gnu --target wasm32-unknown-unknown \
 && rm /tmp/rustup-init

RUN test -n "$WASM_BINDGEN_VERSION" \
 && cargo install --locked cargo-chef --version "$CARGO_CHEF_VERSION" \
 && cargo install --locked wasm-bindgen-cli --version "$WASM_BINDGEN_VERSION" --bin wasm-bindgen \
 && rm -rf "$CARGO_HOME/registry"

COPY scripts/build-release-openssl.sh /tmp/build-release-openssl.sh
RUN OPENSSL_BUILD_JOBS=$(nproc) bash /tmp/build-release-openssl.sh x86_64-unknown-linux-gnu /opt/openssl \
 && rm /tmp/build-release-openssl.sh
# What build-release-openssl.sh prints for the release job's environment.
ENV OPENSSL_DIR=/opt/openssl \
    OPENSSL_LIB_DIR=/opt/openssl/lib \
    OPENSSL_INCLUDE_DIR=/opt/openssl/include \
    OPENSSL_STATIC=1 \
    OPENSSL_NO_VENDOR=1 \
    PKG_CONFIG_PATH=/opt/openssl/lib/pkgconfig \
    X86_64_UNKNOWN_LINUX_GNU_OPENSSL_DIR=/opt/openssl \
    X86_64_UNKNOWN_LINUX_GNU_OPENSSL_LIB_DIR=/opt/openssl/lib \
    X86_64_UNKNOWN_LINUX_GNU_OPENSSL_INCLUDE_DIR=/opt/openssl/include \
    X86_64_UNKNOWN_LINUX_GNU_OPENSSL_STATIC=1 \
    X86_64_UNKNOWN_LINUX_GNU_OPENSSL_NO_VENDOR=1

FROM toolchain AS planner
WORKDIR /build
COPY . .
RUN cargo chef prepare --recipe-path /tmp/recipe.json

FROM toolchain
LABEL org.opencontainers.image.source=https://github.com/5day4cast/coordinator \
      org.opencontainers.image.description="Coordinator release builder with precompiled dependencies"
WORKDIR /build
COPY .cargo .cargo
COPY scripts/release-cargo.sh /usr/local/bin/release-cargo
COPY --from=planner /tmp/recipe.json /tmp/recipe.json
RUN RECIPE=/tmp/recipe.json release-cargo cook linux \
 && RECIPE=/tmp/recipe.json release-cargo cook wasm \
 && rm /tmp/recipe.json
