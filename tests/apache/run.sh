#!/bin/bash
# Builds the fake-backend manager, then runs tests/apache/in-container.sh in an
# Ubuntu container with real Apache and the real config/apache2 site file.
# Needs only Docker.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." &> /dev/null && pwd)"
OUT="$ROOT/target-docker/apache-test"
mkdir -p "$OUT"

"$ROOT/scripts/dev-cargo.sh" build --features fake-backend --bin mcsv_manager
cp "$ROOT/target-docker/debug/mcsv_manager" "$OUT/mcsv_manager"

docker build -q -t mcsv-apache-test -f "$ROOT/tests/apache/Dockerfile" "$ROOT/tests/apache" > /dev/null
docker run --rm \
    -v "$ROOT/tests":/tests:ro \
    -v "$ROOT/config":/config:ro \
    -v "$OUT":/out \
    mcsv-apache-test bash /tests/apache/in-container.sh
