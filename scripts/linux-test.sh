#!/usr/bin/env bash
# Runs Anvil's test suite on Linux, in Docker: the build, the unit tests, and the integration tests twice (clang, then
# gcc as the default toolchain), both compilers required (ANVIL_IT_TOOLCHAINS=gcc,clang). Works from Git Bash on
# Windows, from WSL, or on Linux.
#
#   scripts/linux-test.sh                    everything
#   scripts/linux-test.sh shared_library     integration tests whose name contains `shared_library` (any arguments
#                                            go to `cargo test --test cli`, e.g. `-- --test-threads=4`)
#   scripts/linux-test.sh shell              a shell in the container, in the source's copy, with Anvil built and on PATH
#
# The image (`anvil-linux`, from scripts/linux/Dockerfile) is built on first use; `docker rmi anvil-linux` rebuilds it.
# The source is copied into a named volume on every run (not target/ or out/: Windows bind mounts are slow), and
# cargo's target folder and registry stay in named volumes too, so reruns only rebuild what changed. To start over,
# remove the volumes: `docker volume ls -q -f name=anvil-linux- | xargs docker volume rm`.
set -uo pipefail

IMAGE=anvil-linux
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Git Bash would turn the container's paths (/source) into Windows ones
export MSYS_NO_PATHCONV=1

# The image, once
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "Building the $IMAGE image (once)..."
    docker build -t "$IMAGE" "$ROOT/scripts/linux" || exit 1
fi

# Each checkout gets its own source and target volumes, so two checkouts can run at once; the registry is shared
CHECKOUT="$(basename "$ROOT")-$(printf '%s' "$ROOT" | cksum | cut -d' ' -f1)"
SOURCE_VOLUME="anvil-linux-source-$CHECKOUT"
VOLUMES=(-v "$SOURCE_VOLUME:/source" -v "anvil-linux-target-$CHECKOUT:/target"
         -v anvil-linux-cargo-registry:/usr/local/cargo/registry -v anvil-linux-cargo-git:/usr/local/cargo/git)
ENVIRONMENT=(-e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=always -e ANVIL_IT_TOOLCHAINS=gcc,clang)

# The tests' own steps, quoted for the shell inside the container
TEST_ARGS=""
for arg in "$@"; do
    TEST_ARGS+=" $(printf '%q' "$arg")"
done

if [ "${1:-}" = "shell" ]; then
    STEPS='cargo build && export PATH=/target/debug:$PATH && exec bash'
    TTY=(-it)
    if command -v winpty >/dev/null 2>&1 && [ -n "${MSYSTEM:-}" ]; then
        TTY_PREFIX=(winpty)
    else
        TTY_PREFIX=()
    fi
else
    STEPS="
        cargo build || exit 1
        failed=0
        echo '=== unit tests'
        cargo test --bins || failed=1
        echo '=== integration tests (gcc and clang; clang the default toolchain)'
        cargo test --test cli$TEST_ARGS || failed=1
        echo '=== integration tests (gcc and clang; gcc the default toolchain)'
        /target/debug/anvil toolchains --set-default gcc >/dev/null && cargo test --test cli$TEST_ARGS || failed=1
        exit \$failed"
    TTY=()
    TTY_PREFIX=()
fi

# Copy the source into its volume (a fresh copy each run; tar keeps the time stamps, so cargo rebuilds only what
# changed), then run
tar -C "$ROOT" --exclude=./target --exclude=./out --exclude=./.git -cf - . \
    | docker run --rm -i -v "$SOURCE_VOLUME:/source" "$IMAGE" \
        bash -c 'rm -rf /source/anvil && mkdir -p /source/anvil && tar -C /source/anvil -xf -' || exit 1

"${TTY_PREFIX[@]}" docker run --rm "${TTY[@]}" "${VOLUMES[@]}" "${ENVIRONMENT[@]}" \
    -w /source/anvil "$IMAGE" bash -c "$STEPS"
