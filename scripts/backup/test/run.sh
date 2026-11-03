#!/bin/bash
# Tests for chulacraft-backup.sh with PATH stubs and a fake clock. No root, no
# docker, no network: scripts/backup/test/run.sh
# (The real-Supabase check is separate: scripts/backup/test/supabase-local.sh.)
set -uo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/../chulacraft-backup.sh"
REAL_DATE="$(command -v date)"
REAL_TAR="$(command -v tar)"
export REAL_DATE REAL_TAR
PASS=0
FAIL=0

check() { # description, then a command
    local d="$1"
    shift
    if "$@" > /dev/null 2>&1; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); echo "FAIL: $d"; fi
}
eq() { [ "$1" = "$2" ]; }

# --- sandbox: fresh dirs and stubs per test -------------------------------------
setup() {
    T="$(mktemp -d)"
    export STUB="$T/stub" ROOT_DIR="$T/root" FIFO_DIR="$T/run" WORLD_BASE="$T/inst"
    mkdir -p "$STUB/bin" "$ROOT_DIR" "$FIFO_DIR" "$WORLD_BASE" "$T/manifest" "$T/req"
    : > "$STUB/webhook.log"
    : > "$STUB/calls.log"
    echo 1760000000 > "$STUB/clock" # Wed 2025-10-09 08:53:20 UTC
    export PATH="$STUB/bin:$PATH"
    stubs
    cat > "$T/backup.env" << EOF
ALERT_WEBHOOK_URL=http://hook.invalid/x
BACKUP_ROOT=$ROOT_DIR
MANIFEST_DIR=$T/manifest
REQUEST_DIR=$T/req
WORLD_BASE=$WORLD_BASE
FIFO_DIR=$FIFO_DIR
WORLD_INSTANCES="a"
BOT_DB_PATH=$T/db.json
PG_IMAGE=postgres:17-alpine@sha256:test
SAVE_TIMEOUT=120
EOF
    export BACKUP_ENV="$T/backup.env" BACKUP_NICED=1
    export DF_TOTAL=$((1000 * 1073741824)) DF_AVAIL=$((500 * 1073741824))
    echo '{"users": []}' > "$T/db.json"
    mkdir -p "$T/pg"
    echo "ca" > "$T/pg/supabase-ca.crt"
    echo "PG_CONF_DIR=$T/pg" >> "$T/backup.env"
    unset TAR_KILL STUB_SAVED STUB_CHAT_ONLY STUB_FORGE STUB_FORGE_CHAT PG_SSL_INSECURE_LOCAL
    READERS=()
}

teardown() {
    local p
    for p in "${READERS[@]:-}"; do [ -n "$p" ] && kill "$p" 2> /dev/null; done
    wait 2> /dev/null
    PATH="${PATH#"$STUB/bin:"}"
    rm -rf "$T"
}

stubs() {
    local b="$STUB/bin"
    cat > "$b/date" << 'EOF'
#!/bin/bash
args=("$@"); out=()
for a in "${args[@]}"; do [ "$a" = -u ] || out+=("$a"); done
exec "$REAL_DATE" -u -d "@$(cat "$STUB/clock")" "${out[@]}"
EOF
    cat > "$b/sleep" << 'EOF'
#!/bin/bash
echo $(( $(cat "$STUB/clock") + ${1%.*} )) > "$STUB/clock"
EOF
    cat > "$b/df" << 'EOF'
#!/bin/bash
echo "Filesystem 1-blocks Used Available Capacity Mounted"
echo "fake $DF_TOTAL 0 $DF_AVAIL 0% /"
EOF
    cat > "$b/systemctl" << 'EOF'
#!/bin/bash
# systemctl is-active --quiet minecraft@a.service
echo "systemctl $*" >> "$STUB/calls.log"
[ "$1" = is-active ] && [ -e "$STUB/active-${@: -1}" ]
EOF
    cat > "$b/journalctl" << 'EOF'
#!/bin/bash
unit=""; while [ $# -gt 0 ]; do [ "$1" = -u ] && unit="$2"; shift; done
[ -n "${STUB_CHAT_ONLY:-}" ] && [ -e "$STUB/active-$unit" ] && echo "[12:00:00] [Server thread/INFO]: <mallory> Saved the game"
[ -n "${STUB_FORGE:-}" ] && [ -e "$STUB/active-$unit" ] && echo "[12:00:00] [Server thread/INFO] [minecraft/MinecraftServer]: Saved the game"
[ -n "${STUB_FORGE_CHAT:-}" ] && [ -e "$STUB/active-$unit" ] && echo "[12:00:00] [Server thread/INFO] [minecraft/MinecraftServer]: <mallory> Saved the game"
[ -n "${STUB_SAVED:-}" ] && [ -e "$STUB/active-$unit" ] && echo "[12:00:00] [Server thread/INFO]: Saved the game"
exit 0
EOF
    cat > "$b/curl" << 'EOF'
#!/bin/bash
echo "$*" >> "$STUB/curl-args.log"
[[ " $* " == *" --config - "* ]] && cat >> "$STUB/curl-config.log"
while [ $# -gt 0 ]; do [ "$1" = -d ] && echo "$2" >> "$STUB/webhook.log"; shift; done
EOF
    cat > "$b/runuser" << 'EOF'
#!/bin/bash
# runuser -u USER -- cmd...: record the user and the command name, then run cmd as ourselves.
echo "runuser $2" >> "$STUB/calls.log"; echo "runuser-cmd $4 $5" >> "$STUB/calls.log"; shift 3; exec "$@"
EOF
    cat > "$b/docker" << 'EOF'
#!/bin/bash
echo "docker $*" >> "$STUB/calls.log"
printf 'PGDMP-fake-dump'
EOF
    cat > "$b/tar" << 'EOF'
#!/bin/bash
if [ -n "${TAR_KILL:-}" ]; then kill -9 $$; fi
exec "$REAL_TAR" "$@"
EOF
    cat > "$b/nice" << 'EOF'
#!/bin/bash
echo "nice $*" >> "$STUB/calls.log"; shift 2; exec "$@"
EOF
    cat > "$b/ionice" << 'EOF'
#!/bin/bash
echo "ionice $*" >> "$STUB/calls.log"; shift 1; exec "$@"
EOF
    chmod +x "$b"/*
}

# A FIFO with a reader that appends every line written to <name>.log.
fifo() {
    mkfifo "$FIFO_DIR/$1.stdin"
    ( while :; do cat "$FIFO_DIR/$1.stdin"; done >> "$STUB/fifo-$1.log" ) 2> /dev/null &
    READERS+=("$!")
}
running() { : > "$STUB/active-minecraft@$1.service"; }
world() { # instance, with a little data
    mkdir -p "$WORLD_BASE/$1/world/region"
    echo "level $1" > "$WORLD_BASE/$1/world/level.dat"
    head -c 20000 /dev/urandom > "$WORLD_BASE/$1/world/region/r.0.0.mca"
}
backup() { "$SCRIPT" "$1" > "$T/out.log" 2>&1; }
settle() { sleep 0.3 2> /dev/null || true; }
fifolog() { cat "$STUB/fifo-$1.log" 2> /dev/null; }
manifest_of() { jq -r "$2" "$T/manifest/$1.json"; }
only_manifest() { ls "$T/manifest"/bk-*-"$1".json 2> /dev/null | head -n 1; }
count_dirs() { find "$1" -mindepth 1 -maxdepth 1 -type d 2> /dev/null | wc -l; }
real_sleep() { /bin/sleep "$1"; }

# =============================================================================
echo "== AC 8: three targets, one archive per instance for world"
setup
world a; world b; fifo a; fifo b
sed -i 's/^WORLD_INSTANCES=.*/WORLD_INSTANCES="a b"/' "$T/backup.env"
backup world; rc=$?
check "world exits 0" eq "$rc" 0
id="$(basename "$(only_manifest world)" .json)"
check "world: one archive per instance under one id" eq "$(ls "$ROOT_DIR/world/daily/$id" | sort | tr '\n' ' ')" "a.tar.zst b.tar.zst "
sz=$(($(stat -c %s "$ROOT_DIR/world/daily/$id/a.tar.zst") + $(stat -c %s "$ROOT_DIR/world/daily/$id/b.tar.zst")))
check "world: size_bytes is the sum" eq "$(manifest_of "$id" .size_bytes)" "$sz"
check "world: manifest ok/nightly" eq "$(manifest_of "$id" '.status+"/"+.kind+"/"+.target')" "ok/nightly/world"
backup bot; check "bot exits 0" eq $? 0
bid="$(basename "$(only_manifest bot)" .json)"
check "bot: db.json copied" cmp -s "$T/db.json" "$ROOT_DIR/bot/daily/$bid/db.json"
backup supabase; check "supabase exits 0" eq $? 0
sid="$(basename "$(only_manifest supabase)" .json)"
check "supabase: dump written" test -s "$ROOT_DIR/supabase/daily/$sid/supabase.dump"
check "supabase: docker run used the pinned image, schemas and custom format" grep -q 'postgres:17-alpine@sha256:test pg_dump service=chulacraft sslmode=verify-full sslrootcert=/pg/supabase-ca.crt -Fc -n public -n auth -n storage' "$STUB/calls.log"
check "three manifests" eq "$(ls "$T/manifest" | wc -l)" 3
check "no .partial left" eq "$(find "$ROOT_DIR" -name '*.partial' | wc -l)" 0
check "archives are not world-readable" eq "$(stat -c %a "$ROOT_DIR/world/daily/$id/a.tar.zst")" 600
teardown

echo "== AC 8: 40 days -> exactly 7 daily and 4 weekly per target"
setup
world a
start=1759881600 # 2025-10-08 00:00 UTC (Wednesday)
for d in $(seq 0 39); do
    echo $((start + d * 86400 + 14400)) > "$STUB/clock"
    for t in world bot supabase; do backup "$t" || echo "run failed d=$d t=$t"; done
done
for t in world bot supabase; do
    check "$t: 7 daily" eq "$(count_dirs "$ROOT_DIR/$t/daily")" 7
    check "$t: 4 weekly" eq "$(count_dirs "$ROOT_DIR/$t/weekly")" 4
    check "$t: weekly are Sundays" eq "$(for w in "$ROOT_DIR/$t/weekly"/*; do $REAL_DATE -u -d "$(basename "$w" | sed -E 's/bk-([0-9]{4})([0-9]{2})([0-9]{2}).*/\1-\2-\3/')" +%u; done | sort -u)" 7
    check "$t: manifests only for kept archives" test "$(ls "$T/manifest"/*"-$t.json" | wc -l)" -le 11
done
newest_weekly="$(ls "$ROOT_DIR/world/weekly" | tail -1)"
f="$(ls "$ROOT_DIR/world/weekly/$newest_weekly" | head -1)"
check "weekly is a hard link of the nightly" eq "$(stat -c %h "$ROOT_DIR/world/weekly/$newest_weekly/$f")" 2
teardown

echo "== manual backups keep the last 3"
setup
for d in 1 2 3 4 5; do
    echo $((1760000000 + d * 100)) > "$STUB/clock"
    echo "bk-2025100${d}T000000Z-bot" > "$T/req/bot.id"
    backup bot
done
check "3 manual dirs" eq "$(count_dirs "$ROOT_DIR/bot/manual")" 3
check "oldest manual gone" test ! -d "$ROOT_DIR/bot/manual/bk-20251001T000000Z-bot"
teardown

echo "== AC 9: low disk -> failed, no archive, webhook, NO save-off"
setup
world a; fifo a; running minecraft@a.service 2> /dev/null; running a
DF_AVAIL=$((10 * 1073741824)) backup world; rc=$?
check "exit non-zero" test "$rc" -ne 0
id="$(basename "$(only_manifest world)" .json)"
check "failed manifest" eq "$(manifest_of "$id" .status)" failed
check "no archive written" eq "$(find "$ROOT_DIR" -type f | wc -l)" 0
check "no leftover dirs for the run" eq "$(find "$ROOT_DIR/world" -mindepth 2 2> /dev/null | wc -l)" 0
check "webhook called" test -s "$STUB/webhook.log"
check "webhook names the target" grep -q 'world FAILED' "$STUB/webhook.log"
settle
check "no save-off sent (fifo untouched)" eq "$(fifolog a)" ""
check "systemctl not even consulted" eq "$(grep -c is-active "$STUB/calls.log")" 0
teardown

echo "== AC 9: percentage threshold, estimate from the last ok size"
setup
world a
DF_TOTAL=$((1000 * 1073741824)) DF_AVAIL=$((120 * 1073741824)) backup world
check "120 of 1000 GiB free is below 15 percent" test $? -ne 0
check "refused by the disk guard (percentage)" grep -q 'disk guard' "$STUB/webhook.log"
teardown
setup
world a
echo '{"id":"bk-20251001T000000Z-world","target":"world","kind":"nightly","size_bytes":10737418240,"created_at":"2025-10-01T00:00:00Z","status":"ok"}' > "$T/manifest/bk-20251001T000000Z-world.json"
DF_TOTAL=$((100 * 1073741824)) DF_AVAIL=$((35 * 1073741824)) backup world
check "estimate 12 GiB (1.2 x 10) leaves 23 GiB >= 20: runs" eq $? 0
teardown
setup
world a
echo '{"id":"bk-20251001T000000Z-world","target":"world","kind":"nightly","size_bytes":10737418240,"created_at":"2025-10-01T00:00:00Z","status":"ok"}' > "$T/manifest/bk-20251001T000000Z-world.json"
DF_TOTAL=$((100 * 1073741824)) DF_AVAIL=$((31 * 1073741824)) backup world
rc=$?
check "estimate 12 GiB leaves 19 GiB < 20: refused" test "$rc" -ne 0
check "refused by the disk guard" grep -q 'disk guard' "$STUB/webhook.log"
teardown

echo "== AC 9: ulimit stops an archive whose estimate was wrong; save-on follows"
setup
world a; fifo a; running a
head -c 4194304 /dev/urandom > "$WORLD_BASE/a/world/big.bin"
echo '{"id":"bk-20251001T000000Z-world","target":"world","kind":"nightly","size_bytes":1,"created_at":"2025-10-01T00:00:00Z","status":"ok"}' > "$T/manifest/bk-20251001T000000Z-world.json"
DF_TOTAL=$((100 * 1073741824)) DF_AVAIL=$((20 * 1073741824 + 1048576)) STUB_SAVED=1 backup world; rc=$?
check "exit non-zero" test "$rc" -ne 0
settle
check "save-off, flush, then save-on" eq "$(fifolog a | tr '\n' '|')" "save-off|save-all flush|save-on|"
check "no partial file or archive left" eq "$(find "$ROOT_DIR" -type f | wc -l)" 0
check "webhook called" test -s "$STUB/webhook.log"
check "newest manifest failed" eq "$(manifest_of "$(basename "$(ls -1 "$T/manifest"/bk-*-world.json | tail -1)" .json)" .status)" failed
teardown

echo "== save-on after a killed tar"
setup
world a; fifo a; running a
TAR_KILL=1 STUB_SAVED=1 backup world; rc=$?
check "exit non-zero" test "$rc" -ne 0
settle
check "save-on sent after the kill" eq "$(fifolog a | tr '\n' '|')" "save-off|save-all flush|save-on|"
check "webhook called" test -s "$STUB/webhook.log"
check "nothing published" eq "$(find "$ROOT_DIR" -type f | wc -l)" 0
teardown

echo "== save-on after the 120 s timeout"
setup
world a; fifo a; running a
t0="$(cat "$STUB/clock")"
backup world; rc=$?
check "exit non-zero" test "$rc" -ne 0
settle
check "save-on sent after the timeout" eq "$(fifolog a | tr '\n' '|')" "save-off|save-all flush|save-on|"
check "waited 120 fake seconds" test "$(($(cat "$STUB/clock") - t0))" -ge 120
check "fake clock did not run much past the timeout" test "$(($(cat "$STUB/clock") - t0))" -le 125
check "webhook mentions the timeout" grep -q 'timed out' "$STUB/webhook.log"
teardown

echo "== a chat line is not the server's save confirmation"
setup
world a; fifo a; running a
t0="$(cat "$STUB/clock")"
STUB_CHAT_ONLY=1 backup world; rc=$?
check "chat 'Saved the game' does not satisfy the wait: fails" test "$rc" -ne 0
settle
check "still waited out the timeout" test "$(($(cat "$STUB/clock") - t0))" -ge 120
check "save-on still sent" eq "$(fifolog a | tr '\n' '|')" "save-off|save-all flush|save-on|"
teardown

echo "== Forge/NeoForge save line is accepted; its chat look-alike is not"
setup
world a; fifo a; running a
STUB_FORGE=1 backup world; rc=$?
check "forge-format 'Saved the game' satisfies the wait" eq "$rc" 0
teardown
setup
world a; fifo a; running a
STUB_FORGE_CHAT=1 backup world; rc=$?
check "forge-format chat line does not satisfy the wait" test "$rc" -ne 0
teardown

echo "== supabase enforces TLS itself"
setup
rm "$T/pg/supabase-ca.crt"
backup supabase; rc=$?
check "missing CA: fails" test "$rc" -ne 0
check "missing CA: docker never run" bash -c '! grep -q "^docker" "$1"' _ "$STUB/calls.log"
: > "$T/pg/supabase-ca.crt"
backup supabase; rc=$?
check "empty CA: fails" test "$rc" -ne 0
teardown
setup
rm "$T/pg/supabase-ca.crt"
PG_SSL_INSECURE_LOCAL=1 backup supabase; rc=$?
check "PG_SSL_INSECURE_LOCAL=1 skips the CA requirement" eq "$rc" 0
check "insecure-local: plain service string" grep -q 'pg_dump service=chulacraft -Fc' "$STUB/calls.log"
teardown

echo "== FIFO writes use dd with nofollow,append"
setup
world a; fifo a; running a
STUB_SAVED=1 backup world > /dev/null 2>&1
check "runuser runs dd as the game user" grep -q '^runuser-cmd dd of=' "$STUB/calls.log"
check "script passes oflag=nofollow,append and conv=notrunc" grep -qE 'oflag=nofollow,append conv=notrunc' "$SCRIPT"
teardown

echo "== webhook URL stays off the command line; FIFO writes drop root"
setup
world a; fifo a; running a
STUB_SAVED=1 backup world > /dev/null 2>&1
echo 'not json' > "$T/db.json"
backup bot > /dev/null 2>&1
check "curl was called" test -s "$STUB/curl-args.log"
check "webhook URL not in any curl argv" bash -c '! grep -q hook.invalid "$1"' _ "$STUB/curl-args.log"
check "webhook URL given on stdin" grep -q 'url = "http://hook.invalid/x"' "$STUB/curl-config.log"
check "FIFO writes go through runuser as mcsv" eq "$(grep -c '^runuser mcsv$' "$STUB/calls.log")" 3
teardown

echo "== running vs stopped server"
setup
world a; fifo a; running a
STUB_SAVED=1 backup world; rc=$?
settle
check "running: exit 0" eq "$rc" 0
check "running: save-off, flush, save-on in order" eq "$(fifolog a | tr '\n' '|')" "save-off|save-all flush|save-on|"
teardown
setup
world a; fifo a # FIFO exists, unit not active
backup world; rc=$?
settle
check "stopped: exit 0, archive made" eq "$rc" 0
check "stopped: no FIFO write at all" eq "$(fifolog a)" ""
check "stopped: archive exists" test -s "$ROOT_DIR/world/daily/$(basename "$(only_manifest world)" .json)/a.tar.zst"
teardown
setup
world a; running a # unit active but no FIFO
STUB_SAVED=1 backup world; rc=$?
check "no FIFO: exit 0 and archived" eq "$rc" 0
teardown

echo "== two instances"
setup
world a; world b; fifo a; fifo b; running a
sed -i 's/^WORLD_INSTANCES=.*/WORLD_INSTANCES="a b"/' "$T/backup.env"
STUB_SAVED=1 backup world; rc=$?
settle
check "exit 0" eq "$rc" 0
check "a (running) saved" eq "$(fifolog a | tr '\n' '|')" "save-off|save-all flush|save-on|"
check "b (stopped) untouched" eq "$(fifolog b)" ""
teardown
setup
world a; world b; fifo a; fifo b; running a; running b
sed -i 's/^WORLD_INSTANCES=.*/WORLD_INSTANCES="a b"/' "$T/backup.env"
TAR_KILL= STUB_SAVED=1 backup world
settle
check "both running: both get the full sequence" eq "$(fifolog a | tr '\n' '|')$(fifolog b | tr '\n' '|')" "save-off|save-all flush|save-on|save-off|save-all flush|save-on|"
teardown
setup
world a; fifo a; fifo b; running a; running b # b has no data: second tar fails
sed -i 's/^WORLD_INSTANCES=.*/WORLD_INSTANCES="a b"/' "$T/backup.env"
STUB_SAVED=1 backup world; rc=$?
settle
check "second instance fails: exit non-zero" test "$rc" -ne 0
check "second instance gets its save-on" eq "$(fifolog b | tr '\n' '|')" "save-off|save-all flush|save-on|"
check "first archive discarded with the run" eq "$(find "$ROOT_DIR" -type f | wc -l)" 0
teardown
setup
sed -i 's/^WORLD_INSTANCES=.*/WORLD_INSTANCES="..\/etc"/' "$T/backup.env"
backup world
check "path traversal in WORLD_INSTANCES refused" test $? -ne 0
teardown

echo "== request id handling (D6a)"
setup
echo "bk-20251009T085000Z-bot" > "$T/req/bot.id"
backup bot
check "valid id used" test -f "$T/manifest/bk-20251009T085000Z-bot.json"
check "kind manual" eq "$(manifest_of bk-20251009T085000Z-bot .kind)" manual
check "request file deleted" test ! -e "$T/req/bot.id"
check "dir under manual" test -d "$ROOT_DIR/bot/manual/bk-20251009T085000Z-bot"
teardown
setup
backup bot
check "no file: own id, nightly" eq "$(manifest_of bk-20251009T085320Z-bot .kind)" nightly
teardown
for bad in '../../etc/passwd' 'bk-1-world' 'bk-123Z-bot; rm -rf /' 'bk--bot' 'bk-12-bot-x' ''; do
    setup
    printf '%s\n' "$bad" > "$T/req/bot.id"
    backup bot; rc=$?
    check "malformed [$bad]: still runs, own id" eq "$rc" 0
    check "malformed [$bad]: nightly with own id" eq "$(manifest_of bk-20251009T085320Z-bot .kind)" nightly
    check "malformed [$bad]: file deleted" test ! -e "$T/req/bot.id"
    teardown
done
setup
printf 'bk-1-bot\nbk-2-bot\n' > "$T/req/bot.id"
backup bot
check "two-line file: first valid line is not trusted blindly (only line 1 read)" test -f "$T/manifest/bk-1-bot.json"
teardown
setup
head -c 5000 /dev/zero | tr '\0' 'a' > "$T/req/bot.id"
backup bot
check "huge file: ignored" eq "$(manifest_of bk-20251009T085320Z-bot .kind)" nightly
teardown
setup
echo "bk-20251009T085000Z-bot" > "$T/real.id"; ln -s "$T/real.id" "$T/req/bot.id"
backup bot
check "symlink: ignored" eq "$(manifest_of bk-20251009T085320Z-bot .kind)" nightly
teardown
setup
echo "bk-20251009T085000Z-world" > "$T/req/bot.id"
backup bot
check "id for another target: ignored" eq "$(manifest_of bk-20251009T085320Z-bot .kind)" nightly
teardown

echo "== bot validation and config"
setup
echo 'not json{' > "$T/db.json"
backup bot; rc=$?
check "invalid db.json: fails" test "$rc" -ne 0
check "invalid db.json: webhook" test -s "$STUB/webhook.log"
check "invalid db.json: nothing published" eq "$(find "$ROOT_DIR" -type f | wc -l)" 0
teardown
setup
sed -i '/^ALERT_WEBHOOK_URL/d' "$T/backup.env"
backup bot; rc=$?
check "missing ALERT_WEBHOOK_URL: exit 2, nothing written" eq "$rc$(ls "$T/manifest" | wc -l)" 20
teardown
setup
"$SCRIPT" nonsense > /dev/null 2>&1
check "unknown target: exit 2" eq $? 2
teardown

echo "== nice / ionice re-exec"
setup
unset BACKUP_NICED
"$SCRIPT" bot > /dev/null 2>&1
check "re-executed under nice -n 19 ionice -c3" grep -qE '^nice -n 19 ionice -c3 .*chulacraft-backup.sh bot$' "$STUB/calls.log"
check "and ionice -c3 runs the script" grep -qE '^ionice -c3 .*chulacraft-backup.sh bot$' "$STUB/calls.log"
check "run completed" eq "$(ls "$T/manifest" | wc -l)" 1
teardown

echo "== SIGTERM mid-run: save-on and a failed manifest"
setup
world a; fifo a; running a
cat > "$STUB/bin/journalctl" << 'EOF'
#!/bin/bash
# Stalls until killed, like a save that never finishes.
touch "$STUB/waiting"; exec /bin/sleep 30
EOF
"$SCRIPT" world > "$T/out.log" 2>&1 &
pid=$!
for _ in $(seq 50); do [ -e "$STUB/waiting" ] && break; /bin/sleep 0.1; done
pkill -TERM -P "$pid" 2> /dev/null; kill -TERM "$pid" 2> /dev/null
wait "$pid" 2> /dev/null
settle
check "save-on after SIGTERM" eq "$(fifolog a | tr '\n' '|')" "save-off|save-all flush|save-on|"
check "manifest failed" eq "$(manifest_of "$(basename "$(only_manifest world)" .json)" .status)" failed
teardown

echo "== restore round trip"
setup
world a; world b
sed -i 's/^WORLD_INSTANCES=.*/WORLD_INSTANCES="a b"/' "$T/backup.env"
backup world
id="$(basename "$(only_manifest world)" .json)"
mkdir "$T/restore"
for w in a b; do
    "$REAL_TAR" --zstd -xf "$ROOT_DIR/world/daily/$id/$w.tar.zst" -C "$T/restore"
    check "restored $w equals source" diff -r "$WORLD_BASE/$w" "$T/restore/$w"
done
backup bot
bid="$(basename "$(only_manifest bot)" .json)"
check "restored db.json parses and equals" jq -e --slurpfile a "$ROOT_DIR/bot/daily/$bid/db.json" --slurpfile b "$T/db.json" -n '$a == $b'
teardown

echo
echo "passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ]
