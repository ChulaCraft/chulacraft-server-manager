#!/bin/bash
# The fake-backend binary starts and answers /servers over TCP in a container
# that has no D-Bus. Needs only Docker.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." &> /dev/null && pwd)"
OUT="$ROOT/target-docker/fake-smoke"
mkdir -p "$OUT"

"$ROOT/scripts/dev-cargo.sh" build --features fake-backend --bin mcsv_manager
cp "$ROOT/target-docker/debug/mcsv_manager" "$OUT/mcsv_manager"

docker run --rm -v "$ROOT":/work:ro -v "$OUT":/out mcsv-manager-dev bash -euc '
    source /work/tests/lib/jwt.sh
    [ ! -S /run/dbus/system_bus_socket ] || { echo "D-Bus is present, test is meaningless"; exit 1; }
    gen_keys /out
    MCSV_FAKE_LISTEN=tcp:127.0.0.1:18080 /out/mcsv_manager > /out/manager.log 2>&1 &
    for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:18080/servers && break; sleep 0.1; done
    code() { curl -s -o /tmp/body -w "%{http_code}" "$@"; }
    [ "$(code -H "Authorization: Bearer $(mint_token /out 1)" http://127.0.0.1:18080/servers)" = 200 ]
    [ "$(cat /tmp/body)" = "[\"survival\",\"creative\"]" ]
    [ "$(code http://127.0.0.1:18080/servers)" = 401 ]
    echo "fake-smoke: ok"
'
