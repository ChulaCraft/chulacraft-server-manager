#!/bin/bash
# Run cargo inside the dev image: scripts/dev-cargo.sh test --features fake-backend
# The image is built on first use. Build output goes to target-docker/ (not target/),
# so host and container builds never mix.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." &> /dev/null && pwd)"
IMAGE="${MCSV_DEV_IMAGE:-mcsv-manager-dev}"

if ! docker image inspect "$IMAGE" > /dev/null 2>&1; then
    docker build -t "$IMAGE" -f "$ROOT/Dockerfile.dev" "$ROOT"
fi

mkdir -p "$ROOT/target-docker" "$ROOT/.cargo-docker"
exec docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e CARGO_TARGET_DIR=/work/target-docker \
    -v "$ROOT":/work \
    -v "$ROOT/.cargo-docker":/usr/local/cargo/registry \
    "$IMAGE" cargo "$@"
