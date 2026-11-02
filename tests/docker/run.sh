#!/bin/bash
# Integration test of mcsv-docker-proxy and DockerClient against the real Docker
# daemon. Starts throwaway containers t27-target (configured, fake secret in its
# environment), t27-other (not configured) and t27-proxy, then removes them.
# Needs only Docker and the mcsv-manager-dev image (scripts/dev-cargo.sh builds it).
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." &> /dev/null && pwd)"
IMAGE="${MCSV_DEV_IMAGE:-mcsv-manager-dev}"
DIR="$ROOT/target-docker/t27"
SECRET="fake-t27-secret-value"

cleanup() { docker rm -f t27-target t27-other t27-proxy t27-proxy-deny > /dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup

"$ROOT/scripts/dev-cargo.sh" build --bin mcsv-docker-proxy

rm -rf "$DIR"
mkdir -p "$DIR"
cat > "$DIR/config.toml" <<TOML
[[service]]
id = "t27"
name = "T27"
backend = "docker"
container = "t27-target"
compose_file = "/tmp/t27-compose.yaml"
compose_service = "t27"
TOML

docker run -d --name t27-target -e SUPABASE_SECRET_KEY="$SECRET" -e DISCORD_TOKEN=fake-t27-token \
    "$IMAGE" sleep 600 > /dev/null
docker run -d --name t27-other -e SUPABASE_SECRET_KEY="$SECRET" "$IMAGE" sleep 600 > /dev/null

docker run -d --name t27-proxy \
    --user "$(id -u):$(id -g)" --group-add "$(stat -c %g /var/run/docker.sock)" \
    -v /var/run/docker.sock:/var/run/docker.sock \
    -v "$ROOT/target-docker":/bin-dir:ro -v "$DIR":/t27 \
    -e MCSV_CONFIG=/t27/config.toml -e MCSV_DOCKER_PROXY_LISTEN=/t27/proxy.sock \
    -e MCSV_DOCKER_UPSTREAM=/var/run/docker.sock -e MCSV_DOCKER_PROXY_UIDS="$(id -u)" \
    "$IMAGE" /bin-dir/debug/mcsv-docker-proxy > /dev/null

# A second proxy that does not list our uid: the peer check must refuse us even for
# the configured container.
docker run -d --name t27-proxy-deny \
    --user "$(id -u):$(id -g)" --group-add "$(stat -c %g /var/run/docker.sock)" \
    -v /var/run/docker.sock:/var/run/docker.sock \
    -v "$ROOT/target-docker":/bin-dir:ro -v "$DIR":/t27 \
    -e MCSV_CONFIG=/t27/config.toml -e MCSV_DOCKER_PROXY_LISTEN=/t27/deny.sock \
    -e MCSV_DOCKER_UPSTREAM=/var/run/docker.sock -e MCSV_DOCKER_PROXY_UIDS=99999 \
    "$IMAGE" /bin-dir/debug/mcsv-docker-proxy > /dev/null

for _ in $(seq 100); do [ -S "$DIR/proxy.sock" ] && break; sleep 0.1; done
[ -S "$DIR/proxy.sock" ] || { docker logs t27-proxy; echo "proxy did not start"; exit 1; }

# The same checks with plain curl, as an outside observer of the wire.
code() { curl -s -o /tmp/t27-body -w "%{http_code}" --unix-socket "$DIR/proxy.sock" "$@"; }
[ "$(code http://x/containers/t27-target/json)" = 200 ]
! grep -q -e "$SECRET" -e SUPABASE_SECRET_KEY -e fake-t27-token /tmp/t27-body
for _ in $(seq 100); do [ -S "$DIR/deny.sock" ] && break; sleep 0.1; done
[ -S "$DIR/deny.sock" ] || { docker logs t27-proxy-deny; echo "deny proxy did not start"; exit 1; }
[ "$(code --unix-socket "$DIR/deny.sock" http://x/containers/t27-target/json)" = 403 ]
[ "$(code http://x/containers/t27-other/json)" = 403 ]
! grep -q "$SECRET" /tmp/t27-body
[ "$(code -X POST http://x/containers/t27-other/start)" = 403 ]
[ "$(code -X POST http://x/containers/create)" = 403 ]
rm -f /tmp/t27-body

"$ROOT/scripts/dev-cargo.sh" test --test docker_integration -- --ignored --test-threads=1
echo "docker integration: ok"
