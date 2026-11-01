#!/bin/bash
# Runs the website's manager contract suite against this manager built with
# --features fake-backend, over TCP. Needs Docker, Node 24 and git.
#
#   scripts/conformance.sh
#   WEB_DIR=/path/to/chulacraft-web scripts/conformance.sh   # an existing checkout with `npm ci` done
#
# Without WEB_DIR the public website repo is cloned at WEB_REF (the contract commit; tag
# v1.1-contract) into a temp directory. WEB_REPO overrides the clone source, for a local path.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." &> /dev/null && pwd)"
WEB_REPO="${WEB_REPO:-https://github.com/ChulaCraft/chulacraft-web.git}"
WEB_REF="${WEB_REF:-v1.1-contract}"
PORT="${CONFORMANCE_PORT:-3212}"
IMAGE="${MCSV_DEV_IMAGE:-mcsv-manager-dev}"
WORK="$(mktemp -d)"
NAME="mcsv-conformance-$$"

cleanup() {
    docker rm -f "$NAME" > /dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT

WEB="${WEB_DIR:-}"
if [ -z "$WEB" ]; then
    WEB="$WORK/web"
    git clone --quiet "$WEB_REPO" "$WEB"
    git -C "$WEB" checkout --quiet "$WEB_REF"
    (cd "$WEB" && npm ci --silent)
fi
[ -f "$WEB/vitest.contract.config.ts" ] || { echo "conformance: $WEB has no vitest.contract.config.ts" >&2; exit 1; }

"$ROOT/scripts/dev-cargo.sh" build --features fake-backend --bin mcsv_manager
mkdir -p "$WORK/state"
cp "$ROOT/target-docker/debug/mcsv_manager" "$WORK/mcsv_manager"

# shellcheck source=../tests/lib/jwt.sh
source "$ROOT/tests/lib/jwt.sh"
gen_keys "$WORK"   # key.pem, and MCSV_JWT_PUBLIC_KEY in this shell

# The fake backend knows the instances survival and creative; the contract's backups need a world instance.
cat > "$WORK/config.toml" <<'TOML'
[[service]]
id = "survival"
name = "Survival"
backend = "systemd"
unit = "minecraft@survival.service"

[[service]]
id = "creative"
name = "Creative"
backend = "systemd"
unit = "minecraft@creative.service"
actions = ["restart"]

[backups]
world_instances = ["survival"]
TOML

docker run -d --name "$NAME" --network host --user "$(id -u):$(id -g)" \
    -v "$WORK":/work \
    -e MCSV_CONFIG=/work/config.toml \
    -e MCSV_METRICS_PATH=/work/state/metrics.json \
    -e MCSV_BACKUP_MANIFEST_DIR=/work/state/manifest \
    -e MCSV_BACKUP_REQUEST_DIR=/work/state/requests \
    -e MCSV_JWT_PUBLIC_KEY="$MCSV_JWT_PUBLIC_KEY" \
    -e MCSV_FAKE_LISTEN="tcp:127.0.0.1:$PORT" \
    "$IMAGE" /work/mcsv_manager > /dev/null

for _ in $(seq 100); do
    curl -s -o /dev/null "http://127.0.0.1:$PORT/services" && break
    docker inspect -f '{{.State.Running}}' "$NAME" | grep -q true || { docker logs "$NAME" >&2; exit 1; }
    sleep 0.1
done

cd "$WEB"
status=0
MANAGER_URL="http://127.0.0.1:$PORT" MCSV_JWT_PRIVATE_KEY="$(cat "$WORK/key.pem")" \
    npx vitest run --config vitest.contract.config.ts || status=$?
if [ "$status" -ne 0 ]; then docker logs "$NAME" 2>&1 | tail -30 >&2; fi
exit "$status"
