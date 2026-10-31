#!/bin/bash
# Runs inside the container started by run.sh. Exits non-zero on the first failed check.
set -euo pipefail
source /tests/lib/jwt.sh

STATUS=1 LOGS=2 CONSOLE_READ=4
BASE=http://127.0.0.1/api/mcsv_manager
fail=0

check() { # name want got
    if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: want $2, got $3"; fail=1; fi
}
code() { curl -s -o /tmp/body -w '%{http_code}' "$@"; }

# Host layout the setup script creates: the API socket belongs to mcsv-http, and
# www-data (what Apache runs as) is a member of it.
addgroup --system mcsv-mgr > /dev/null
addgroup --system mcsv-http > /dev/null
adduser --system --ingroup mcsv-mgr --no-create-home mcsv-mgr > /dev/null
usermod -aG mcsv-http mcsv-mgr
usermod -aG mcsv-http www-data
chmod o+w /run   # lets the unprivileged manager create /run/mcsv_manager.sock

cp /config/apache2/sites-available/mcsv_manager.conf /etc/apache2/sites-available/
a2enmod -q proxy proxy_http proxy_wstunnel headers
a2ensite -q mcsv_manager
echo 'ServerName localhost' > /etc/apache2/conf-enabled/servername.conf
apachectl configtest

mkdir -p /tmp/keys
gen_keys /tmp/keys
# Same mode and group as the shipped socket unit: 0660, mcsv-http.
( umask 0117
  MCSV_FAKE_LISTEN=unix:/run/mcsv_manager.sock \
      setpriv --reuid=mcsv-mgr --regid=mcsv-http --init-groups /out/mcsv_manager > /out/manager.log 2>&1 & )
for _ in $(seq 50); do [ -S /run/mcsv_manager.sock ] && break; sleep 0.1; done
[ -S /run/mcsv_manager.sock ] || { cat /out/manager.log; echo "manager did not start"; exit 1; }
check "socket mode and group" "660 mcsv-http" "$(stat -c '%a %G' /run/mcsv_manager.sock)"

apachectl start
for _ in $(seq 50); do curl -s -o /dev/null "$BASE/servers" && break; sleep 0.1; done

assert_matrix() {
    local label="$1"
    check "$label: valid token from www-data -> 200" 200 "$(code -H "Authorization: Bearer $(mint_token /tmp/keys $STATUS)" "$BASE/servers")"
    check "$label: body" '["survival","creative"]' "$(cat /tmp/body)"
    check "$label: no token -> 401" 401 "$(code "$BASE/servers")"
    check "$label: bad token -> 401" 401 "$(code -H "Authorization: Bearer nope" "$BASE/servers")"
    check "$label: token without the bit -> 403" 403 "$(code -H "Authorization: Bearer $(mint_token /tmp/keys $LOGS)" "$BASE/servers")"
    check "$label: json 401 on /services" '{"error":"unauthorized"}' "$(curl -s "$BASE/services")"
    check "$label: json 403 on /services" '{"error":"forbidden"}' "$(curl -s -H "Authorization: Bearer $(mint_token /tmp/keys $LOGS)" "$BASE/services")"
}
assert_matrix "www-data only in mcsv-http"

# The AC 6 case: www-data also in mcsv-mgr must change nothing without a token.
usermod -aG mcsv-mgr www-data
apachectl restart
for _ in $(seq 50); do curl -s -o /dev/null "$BASE/servers" && break; sleep 0.1; done
assert_matrix "www-data also in mcsv-mgr"

ws() { # permission or empty; prints the response head
    local proto=""
    [ -z "$1" ] || proto="-H Sec-WebSocket-Protocol:\ mcsv.jwt,\ $(mint_token /tmp/keys "$1")"
    eval curl -s -i --http1.1 --max-time 2 \
        -H "'Connection: Upgrade'" -H "'Upgrade: websocket'" \
        -H "'Sec-WebSocket-Version: 13'" -H "'Sec-WebSocket-Key: $(openssl rand -base64 16)'" \
        $proto "$BASE/server/survival/console" || true
}
check "ws: no token -> 401" "HTTP/1.1 401" "$(ws "" | head -1 | tr -d '\r' | cut -c1-12)"
check "ws: token without CONSOLE_READ -> 403" "HTTP/1.1 403" "$(ws $STATUS | head -1 | tr -d '\r' | cut -c1-12)"
head="$(ws $CONSOLE_READ | tr -d '\r')"
check "ws: valid subprotocol token -> 101" "HTTP/1.1 101" "$(echo "$head" | head -1 | cut -c1-12)"
check "ws: subprotocol echoed" "yes" "$(echo "$head" | grep -qi '^sec-websocket-protocol: mcsv.jwt$' && echo yes || echo no)"

# The manager saw www-data (uid 33) as the peer, not Apache's own user or root.
check "journal line names peer uid 33" yes "$(grep -q 'auth denied: 401 path=/servers peer_uid=33 reason=no token' /out/manager.log && echo yes || echo no)"
check "no token text in the manager log" yes "$(grep -q 'eyJ' /out/manager.log && echo no || echo yes)"

exit "$fail"
