#!/bin/bash
# Runs the real `supabase` target against the LOCAL Supabase stack of chulacraft-web
# (container supabase_db_chulacraft, never production) with a throwaway
# backup_reader role, then restores into a throwaway postgres:17-alpine
# (container name t29-restore-*) and prints the pg_restore errors.
# The password below is a local-only dummy.
set -uo pipefail
HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
DB=supabase_db_chulacraft
NET=supabase_network_chulacraft
IMG=postgres:17-alpine
PW=local-dummy-not-a-secret
T="$(mktemp -d)"
RESTORE="t29-restore-$$"
cleanup() {
    docker rm -f "$RESTORE" > /dev/null 2>&1
    docker exec "$DB" psql -U postgres -qc "drop role if exists backup_reader" > /dev/null 2>&1
    rm -rf "$T"
}
trap cleanup EXIT

docker exec "$DB" psql -U postgres -qc "drop role if exists backup_reader" 2> /dev/null
docker exec "$DB" psql -U postgres -v ON_ERROR_STOP=1 -qc \
    "create role backup_reader login bypassrls password '$PW'; grant pg_read_all_data to backup_reader;" || exit 1

mkdir -p "$T/pg" "$T/mf" "$T/root" "$T/req"
cat > "$T/pg/pg_service.conf" << EOF
[chulacraft]
host=$DB
port=5432
dbname=postgres
user=backup_reader
sslmode=${SSLMODE:-require}
EOF
echo "$DB:5432:postgres:backup_reader:$PW" > "$T/pg/pgpass"
chmod 0600 "$T/pg/pgpass" "$T/pg/pg_service.conf"
cat > "$T/backup.env" << EOF
ALERT_WEBHOOK_URL=http://hook.invalid/x
BACKUP_ROOT=$T/root
MANIFEST_DIR=$T/mf
REQUEST_DIR=$T/req
PG_IMAGE=$IMG
PG_CONF_DIR=$T/pg
PG_DOCKER_ARGS="--network $NET"
MIN_FREE_GIB=1
MIN_FREE_PCT=1
EOF
export PG_SSL_INSECURE_LOCAL=1 # test-only: the local stack has no TLS
export BACKUP_ENV="$T/backup.env" BACKUP_NICED=1
# The webhook is unreachable by design; a failure here shows in the exit code.
"$HERE/../chulacraft-backup.sh" supabase || { echo "backup failed"; exit 1; }
DUMP="$(ls "$T"/root/supabase/daily/*/supabase.dump)"
ls -l "$DUMP"
jq . "$T"/mf/*.json

docker run -d --name "$RESTORE" -e POSTGRES_PASSWORD=x "$IMG" > /dev/null
for _ in $(seq 60); do docker exec "$RESTORE" pg_isready -U postgres -q && break; sleep 1; done
sleep 2
for r in anon authenticated service_role; do docker exec "$RESTORE" psql -U postgres -qc "create role $r nologin"; done
# Supabase provides schema `extensions` with pgcrypto; public.event_checkin_secrets
# defaults to extensions.gen_random_bytes(), so the CREATE TABLE fails without them.
docker exec "$RESTORE" psql -U postgres -qc "create schema extensions; create extension pgcrypto schema extensions"
docker exec -i "$RESTORE" pg_restore --no-owner --no-privileges --no-comments -U postgres -d postgres < "$DUMP" 2> "$T/restore.err"
echo "pg_restore exit: $?"
echo "---- pg_restore stderr ----"
cat "$T/restore.err"
echo "---- counts (restored | source) ----"
for q in "select count(*) from public.profiles" "select count(*) from auth.users"; do
    echo "$q: $(docker exec "$RESTORE" psql -U postgres -Atc "$q") | $(docker exec "$DB" psql -U postgres -Atc "$q")"
done
