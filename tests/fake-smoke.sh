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
    printf "[[service]]\nid = \"survival\"\nname = \"Survival\"\nbackend = \"systemd\"\nunit = \"minecraft@survival.service\"\n" > /out/config.toml
    MCSV_CONFIG=/out/config.toml MCSV_METRICS_PATH=/out/metrics.json MCSV_FAKE_LISTEN=tcp:127.0.0.1:18080 /out/mcsv_manager > /out/manager.log 2>&1 &
    for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:18080/servers && break; sleep 0.1; done
    code() { curl -s -o /tmp/body -w "%{http_code}" "$@"; }
    [ "$(code -H "Authorization: Bearer $(mint_token /out 1)" http://127.0.0.1:18080/servers)" = 200 ]
    [ "$(cat /tmp/body)" = "[\"survival\",\"creative\"]" ]
    [ "$(code http://127.0.0.1:18080/servers)" = 401 ]
    # The new service API, end to end over TCP.
    B="Authorization: Bearer"
    [ "$(code -H "$B $(mint_token /out 1)" http://127.0.0.1:18080/services)" = 200 ]
    grep -q "\"id\":\"survival\"" /tmp/body && grep -q "\"kind\":\"systemd\"" /tmp/body
    [ "$(code -H "$B $(mint_token /out 1)" http://127.0.0.1:18080/service/survival/status)" = 200 ]
    grep -q "\"state\":\"up\"" /tmp/body
    [ "$(code -H "$B $(mint_token /out 1)" "http://127.0.0.1:18080/service/survival/metrics?hours=1")" = 200 ]
    [ "$(code -H "$B $(mint_token /out 1)" "http://127.0.0.1:18080/service/survival/uptime?days=1")" = 200 ]
    [ "$(code -H "$B $(mint_token /out 2)" "http://127.0.0.1:18080/service/survival/rlog?max_lines=5&until=2030-01-01T00:00:00Z")" = 200 ]
    [ "$(code -X POST -H "$B $(mint_token /out 64)" http://127.0.0.1:18080/service/survival/restart)" = 202 ]
    [ "$(cat /tmp/body)" = "{}" ]
    [ "$(code -X POST -H "$B $(mint_token /out 64)" http://127.0.0.1:18080/service/nope/restart)" = 404 ]
    [ "$(code -H "$B $(mint_token /out 1)" http://127.0.0.1:18080/service/survival/rlog)" = 403 ]
    echo "fake-smoke: ok"
'
