# Sourced by scripts/install.sh (so install-target is available): the Docker
# socket proxy, the update unit, and the update env files.
# Needs root and a built target/release/mcsv-docker-proxy.
CONFIG="${MCSV_CONFIG:-/etc/mcsv_manager/config.toml}"
UNIT_DIR="${MCSV_UNIT_DIR:-/etc/systemd/system}"
UPDATE_ENV_DIR="${MCSV_UPDATE_ENV_DIR:-/etc/chulacraft/update}"
DOCKER_UNITS_SRC="$SCRIPT_DIR/../config/systemd/system"

install-target mcsv-docker-proxy

# Group of the proxy socket: holds only the manager account (setup.sh does the same).
getent group mcsv-docker > /dev/null || groupadd --system mcsv-docker
usermod -aG mcsv-docker mcsv-mgr

install -m 644 "$DOCKER_UNITS_SRC/mcsv-docker-proxy.service" "$UNIT_DIR/mcsv-docker-proxy.service"
install -m 644 "$DOCKER_UNITS_SRC/chulacraft-update@.service" "$UNIT_DIR/chulacraft-update@.service"

# Root-owned 0600 env files, one per updatable service; env files of services
# that are gone from the config are removed.
install -d -m 700 "$UPDATE_ENV_DIR"
MCSV_CONFIG="$CONFIG" /usr/local/bin/mcsv-docker-proxy update-env "$UPDATE_ENV_DIR"

if [ -z "${MCSV_SKIP_SYSTEMCTL:-}" ]; then
    systemctl daemon-reload
    systemctl enable mcsv-docker-proxy.service
    systemctl restart mcsv-docker-proxy.service
fi
