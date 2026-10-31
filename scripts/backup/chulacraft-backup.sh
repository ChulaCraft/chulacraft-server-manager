#!/bin/bash
# chulacraft-backup.sh <world|supabase|bot>
# Run as root by chulacraft-backup@<target>.service. See docs/ops-manager-contract.md
# (Backups) and the v1.1 plan, decisions D4, D5, D6, D6a-c.
#
# Order, for every target:
#   1. read the request id (D6a)
#   2. disk guard (D4), before anything touches the game: on refusal, a failed
#      manifest, the webhook, exit 1, and no save-off
#   3. the target's work
#   4. rename .partial, manifest, retention; any failure posts to the webhook
set -euo pipefail

TARGET="${1:-}"
case "$TARGET" in world | supabase | bot) ;; *)
    echo "usage: $0 world|supabase|bot" >&2
    exit 2
    ;;
esac

# Low priority. BACKUP_NICED stops the re-exec from looping.
if [ -z "${BACKUP_NICED:-}" ]; then
    export BACKUP_NICED=1
    exec nice -n 19 ionice -c3 "$0" "$@"
fi

umask 077
# shellcheck disable=SC1090
source "${BACKUP_ENV:-/etc/chulacraft/backup/backup.env}"

BACKUP_ROOT="${BACKUP_ROOT:-/var/backups/chulacraft}"
MANIFEST_DIR="${MANIFEST_DIR:-/var/lib/chulacraft-backup/manifest}"
REQUEST_DIR="${REQUEST_DIR:-/var/lib/mcsv_manager/backup-requests}"
WORLD_BASE="${WORLD_BASE:-/srv/minecraft/instances}"
FIFO_DIR="${FIFO_DIR:-/run/minecraft}"
MIN_FREE_GIB="${MIN_FREE_GIB:-20}"
MIN_FREE_PCT="${MIN_FREE_PCT:-15}"
SAVE_TIMEOUT="${SAVE_TIMEOUT:-120}"
KEEP_DAILY="${KEEP_DAILY:-7}"
KEEP_WEEKLY="${KEEP_WEEKLY:-4}"
KEEP_MANUAL="${KEEP_MANUAL:-3}"
KEEP_FAILED="${KEEP_FAILED:-10}"
SUPABASE_FIRST_ESTIMATE_MIB="${SUPABASE_FIRST_ESTIMATE_MIB:-512}"
PG_CONF_DIR="${PG_CONF_DIR:-/etc/chulacraft/backup}"
PG_DOCKER_ARGS="${PG_DOCKER_ARGS:-}"

ID=""
KIND="nightly"
CREATED_AT=""
SIZE_BYTES=0
RUN_DIR=""        # <ROOT>/<target>/<kind dir>/<id>.partial while running
SAVED_OFF=()      # instances where save-off was sent and save-on is still owed
FINISHED=0

now_epoch() { date -u +%s; }

# --- config checks (nothing is touched yet) -------------------------------------
if [ -z "${ALERT_WEBHOOK_URL:-}" ]; then
    echo "ALERT_WEBHOOK_URL is required (backup.env)" >&2
    exit 2
fi

webhook() {
    local msg="$1"
    local body
    body="$(jq -nc --arg c "$msg" '{content: $c}')"
    curl -fsS --max-time 15 -H 'Content-Type: application/json' -d "$body" \
        "$ALERT_WEBHOOK_URL" > /dev/null || echo "webhook failed" >&2
}

# --- manifest -------------------------------------------------------------------
write_manifest() { # status [error]
    local status="$1" error="${2:-}"
    mkdir -p "$MANIFEST_DIR"
    local tmp="$MANIFEST_DIR/.$ID.json.tmp"
    jq -nc --arg id "$ID" --arg target "$TARGET" --arg kind "$KIND" \
        --argjson size "$SIZE_BYTES" --arg created "$CREATED_AT" \
        --arg status "$status" --arg error "$error" \
        '{id: $id, target: $target, kind: $kind, size_bytes: $size,
          created_at: $created, status: $status}
         + (if $error == "" then {} else {error: $error} end)' > "$tmp"
    chmod 0640 "$tmp"
    mv -f "$tmp" "$MANIFEST_DIR/$ID.json"
}

# --- save-on safety -------------------------------------------------------------
server_running() { # instance: the unit is active AND the FIFO exists.
    # The FIFO can outlive a stopped server (the socket unit owns it), and text
    # written to it would wait in the pipe and run at the next start.
    systemctl is-active --quiet "minecraft@$1.service" && [ -p "$FIFO_DIR/$1.stdin" ]
}

send_cmd() { # instance command
    timeout 10 bash -c 'printf "%s\n" "$1" > "$2"' _ "$2" "$FIFO_DIR/$1.stdin"
}

save_on() { # instance
    local i
    for i in "${!SAVED_OFF[@]}"; do
        if [ "${SAVED_OFF[$i]}" = "$1" ]; then unset "SAVED_OFF[$i]"; fi
    done
    if server_running "$1"; then
        send_cmd "$1" save-on || echo "save-on to $1 failed" >&2
    fi
}

on_exit() {
    local code=$?
    trap - EXIT
    set +e
    local i
    for i in "${SAVED_OFF[@]:-}"; do
        [ -n "$i" ] && save_on "$i"
    done
    if [ "$FINISHED" -ne 1 ]; then
        # Killed, or a command failed under set -e: leave no partial output.
        [ -n "$RUN_DIR" ] && rm -rf "$RUN_DIR"
        if [ -n "$ID" ]; then
            write_manifest failed "script ended unexpectedly (exit $code)"
            webhook "chulacraft-backup $TARGET FAILED ($ID): script ended unexpectedly (exit $code) on $(hostname)"
        fi
        [ "$code" -eq 0 ] && code=1
    fi
    exit "$code"
}
trap on_exit EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
trap 'exit 129' HUP

fail() { # message
    local msg="$1"
    echo "FAILED: $msg" >&2
    # Everything that was sent save-off gets save-on in on_exit; FINISHED stays 0
    # so the same path would also write the manifest, so do it here once.
    FINISHED=1
    local i
    for i in "${SAVED_OFF[@]:-}"; do
        [ -n "$i" ] && save_on "$i"
    done
    [ -n "$RUN_DIR" ] && rm -rf "$RUN_DIR"
    RUN_DIR=""
    write_manifest failed "$msg"
    webhook "chulacraft-backup $TARGET FAILED ($ID): $msg on $(hostname)"
    exit 1
}

# --- 1. request id (D6a) --------------------------------------------------------
read_request_id() {
    local file="$REQUEST_DIR/$TARGET.id" line=""
    if [ -L "$file" ]; then
        echo "ignoring symlink $file" >&2
    elif [ -f "$file" ]; then
        line="$(head -c 200 "$file" | head -n 1 | tr -d '\r')" || true
    fi
    rm -f "$file" 2> /dev/null || true
    if [[ "$line" =~ ^bk-[0-9TZ]+-(world|supabase|bot)$ ]] && [ "${BASH_REMATCH[1]}" = "$TARGET" ]; then
        ID="$line"
        KIND="manual"
    else
        [ -n "$line" ] && echo "ignoring malformed request id" >&2
        ID="bk-$(date -u +%Y%m%dT%H%M%SZ)-$TARGET"
        KIND="nightly"
    fi
    CREATED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}

read_request_id
mkdir -p "$BACKUP_ROOT"
chmod 0700 "$BACKUP_ROOT"
write_manifest running

case "$KIND" in nightly) KIND_DIR=daily ;; manual) KIND_DIR=manual ;; esac
TARGET_DIR="$BACKUP_ROOT/$TARGET"
FINAL_DIR="$TARGET_DIR/$KIND_DIR/$ID"
RUN_DIR_PLAN="$FINAL_DIR.partial"

# --- sources and estimate -------------------------------------------------------
WORLDS=()
if [ "$TARGET" = world ]; then
    read -r -a WORLDS <<< "${WORLD_INSTANCES:-}"
    [ "${#WORLDS[@]}" -gt 0 ] || fail "WORLD_INSTANCES is empty"
    for w in "${WORLDS[@]}"; do
        [[ "$w" =~ ^[A-Za-z0-9_-]+$ ]] || fail "bad instance name in WORLD_INSTANCES"
    done
fi

source_bytes() {
    case "$TARGET" in
        world)
            local w total=0 n
            for w in "${WORLDS[@]}"; do
                n="$(du -sb "$WORLD_BASE/$w" 2> /dev/null | cut -f1)"
                total=$((total + ${n:-0}))
            done
            echo "$total"
            ;;
        bot) du -sb "${BOT_DB_PATH:-/nonexistent}" 2> /dev/null | cut -f1 || echo 0 ;;
        supabase) echo $((SUPABASE_FIRST_ESTIMATE_MIB * 1024 * 1024)) ;;
    esac
}

estimate_bytes() { # 1.2 x the last ok size, or the source size on a first run
    local f last=""
    for f in "$MANIFEST_DIR"/bk-*-"$TARGET".json; do
        [ -e "$f" ] || continue
        if [ "$(jq -r .status "$f" 2> /dev/null)" = ok ]; then last="$f"; fi
    done # glob order is by name, i.e. by time
    if [ -n "$last" ]; then
        echo $(($(jq -r .size_bytes "$last") * 12 / 10))
    else
        echo $(($(source_bytes) * 12 / 10))
    fi
}

# --- 2. disk guard (D4) ---------------------------------------------------------
read -r TOTAL AVAIL < <(df -P -B1 "$BACKUP_ROOT" | awk 'NR==2 {print $2, $4}')
EST="$(estimate_bytes)"
THRESH=$((MIN_FREE_GIB * 1073741824))
PCT=$((TOTAL * MIN_FREE_PCT / 100))
[ "$PCT" -gt "$THRESH" ] && THRESH=$PCT
MARGIN=$((AVAIL - THRESH))
if [ "$MARGIN" -lt "$EST" ]; then
    fail "disk guard: available $AVAIL bytes minus estimate $EST would leave less than $THRESH bytes free"
fi
REMAINING=$MARGIN # bytes the archives of this run may still use (ulimit -f)

limit_blocks() { echo $((REMAINING / 1024)); }

# --- 3. target work -------------------------------------------------------------
RUN_DIR="$RUN_DIR_PLAN"
mkdir -p "$RUN_DIR"

account() { # file written: add to size and shrink the remaining budget
    local n
    n="$(stat -c %s "$1")"
    SIZE_BYTES=$((SIZE_BYTES + n))
    REMAINING=$((REMAINING - n))
}

wait_saved() { # instance start_epoch
    local deadline=$(($2 + SAVE_TIMEOUT)) out
    while :; do
        out="$(journalctl -u "minecraft@$1.service" --since "@$2" --no-pager -o cat 2> /dev/null || true)"
        [[ "$out" == *"Saved the game"* ]] && return 0
        [ "$(now_epoch)" -ge "$deadline" ] && return 1
        sleep 1
    done
}

archive_world() { # instance
    local w="$1" out="$RUN_DIR/$w.tar.zst.partial" running=0
    if server_running "$w"; then
        running=1
        local t0
        t0="$(now_epoch)"
        SAVED_OFF+=("$w")
        send_cmd "$w" save-off || fail "could not send save-off to $w"
        send_cmd "$w" "save-all flush" || fail "could not send save-all flush to $w"
        wait_saved "$w" "$t0" || fail "timed out after ${SAVE_TIMEOUT}s waiting for 'Saved the game' from $w"
    fi
    # shellcheck disable=SC2015
    ( ulimit -f "$(limit_blocks)"; tar --zstd -cf "$out" -C "$WORLD_BASE" "$w" ) \
        || fail "tar of $w failed or exceeded the disk budget"
    [ "$running" -eq 1 ] && save_on "$w"
    mv "$out" "$RUN_DIR/$w.tar.zst"
    account "$RUN_DIR/$w.tar.zst"
}

case "$TARGET" in
    world)
        for w in "${WORLDS[@]}"; do archive_world "$w"; done
        ;;
    bot)
        [ -f "${BOT_DB_PATH:-}" ] || fail "BOT_DB_PATH is not a file"
        cp "$BOT_DB_PATH" "$RUN_DIR/db.json.partial" || fail "copy of db.json failed"
        jq -e . "$RUN_DIR/db.json.partial" > /dev/null 2>&1 || fail "db.json is not valid JSON"
        mv "$RUN_DIR/db.json.partial" "$RUN_DIR/db.json"
        account "$RUN_DIR/db.json"
        ;;
    supabase)
        [ -n "${PG_IMAGE:-}" ] || fail "PG_IMAGE is not set (digest-pinned postgres:17-alpine)"
        # shellcheck disable=SC2086
        ( ulimit -f "$(limit_blocks)"
          docker run --rm --user 0 $PG_DOCKER_ARGS \
              -v "$PG_CONF_DIR:/pg:ro" -e PGPASSFILE=/pg/pgpass -e PGSERVICEFILE=/pg/pg_service.conf \
              "$PG_IMAGE" pg_dump "service=${PG_SERVICE:-chulacraft}" -Fc -n public -n auth -n storage \
              > "$RUN_DIR/supabase.dump.partial" ) \
            || fail "pg_dump failed or exceeded the disk budget"
        [ -s "$RUN_DIR/supabase.dump.partial" ] || fail "pg_dump wrote nothing"
        mv "$RUN_DIR/supabase.dump.partial" "$RUN_DIR/supabase.dump"
        account "$RUN_DIR/supabase.dump"
        ;;
esac

# --- 4. publish, manifest, retention --------------------------------------------
mv "$RUN_DIR" "$FINAL_DIR"
RUN_DIR=""
if [ "$KIND" = nightly ] && [ "$(date -u +%u)" = 7 ]; then
    mkdir -p "$TARGET_DIR/weekly"
    cp -al "$FINAL_DIR" "$TARGET_DIR/weekly/$ID"
fi
write_manifest ok

prune() { # dir keep
    [ -d "$1" ] || return 0
    local old
    old="$(find "$1" -mindepth 1 -maxdepth 1 -type d -name 'bk-*' ! -name '*.partial' -printf '%f\n' | sort | head -n "-$2")"
    [ -z "$old" ] || while read -r d; do rm -rf "${1:?}/$d"; done <<< "$old"
}
prune "$TARGET_DIR/daily" "$KEEP_DAILY"
prune "$TARGET_DIR/weekly" "$KEEP_WEEKLY"
prune "$TARGET_DIR/manual" "$KEEP_MANUAL"

for f in "$MANIFEST_DIR"/bk-*-"$TARGET".json; do
    [ -e "$f" ] || continue
    id="$(basename "$f" .json)"
    if [ "$(jq -r .status "$f" 2> /dev/null)" = ok ] \
        && [ ! -d "$TARGET_DIR/daily/$id" ] && [ ! -d "$TARGET_DIR/weekly/$id" ] \
        && [ ! -d "$TARGET_DIR/manual/$id" ]; then
        rm -f "$f"
    fi
done
failed="$(for f in "$MANIFEST_DIR"/bk-*-"$TARGET".json; do
    [ -e "$f" ] && [ "$(jq -r .status "$f" 2> /dev/null)" = failed ] && echo "$f"
done | sort | head -n "-$KEEP_FAILED" || true)"
[ -z "$failed" ] || while read -r f; do rm -f "$f"; done <<< "$failed"

FINISHED=1
echo "ok $ID $SIZE_BYTES bytes"
