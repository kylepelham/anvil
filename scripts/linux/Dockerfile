# Anvil's Linux test bed: gcc, clang (with LLVM's tools, OpenMP and sanitizer runtimes), the tools Anvil drives (cmake,
# ninja, git, pkg-config, clang-format, clang-tidy, sccache, gdb), Rust with the macOS targets for `cargo check`, and a
# vcpkg for the tests that install a port. scripts/linux-test.sh builds it as `anvil-linux`.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    VCPKG_ROOT=/opt/vcpkg \
    VCPKG_DISABLE_METRICS=1 \
    PATH=/usr/local/cargo/bin:$PATH

# Compilers and tools
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential gcc g++ clang lld llvm libomp-dev libclang-rt-dev clang-format clang-tidy gdb \
        cmake ninja-build pkg-config git curl ca-certificates zip unzip tar python3 \
 && rm -rf /var/lib/apt/lists/*

# sccache, for the tests of compiles through a compiler cache
ARG SCCACHE_VERSION=0.19.0
RUN curl -sSfL https://github.com/mozilla/sccache/releases/download/v$SCCACHE_VERSION/sccache-v$SCCACHE_VERSION-x86_64-unknown-linux-musl.tar.gz \
    | tar -xz -C /usr/local/bin --strip-components=1 --wildcards '*/sccache' \
 && chmod +x /usr/local/bin/sccache

# Rust
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable --component rustfmt,clippy \
 && rustup target add aarch64-apple-darwin x86_64-apple-darwin \
 && chmod -R a+w $RUSTUP_HOME $CARGO_HOME

# vcpkg (shallow; ports come from the fixtures' overlays or download on demand)
RUN git clone --depth 1 https://github.com/microsoft/vcpkg.git $VCPKG_ROOT \
 && $VCPKG_ROOT/bootstrap-vcpkg.sh -disableMetrics

WORKDIR /work
