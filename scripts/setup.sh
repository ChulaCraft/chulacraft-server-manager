#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
CONFIG_DIR="$SCRIPT_DIR/../config"

addgroup --system mcsv-mgr
# Who may connect to the API socket. Connecting is not authority: see README.
addgroup --system mcsv-http
adduser --system --ingroup mcsv-mgr --no-create-home mcsv-mgr
adduser --system --ingroup mcsv-mgr --no-create-home mcsv
usermod -aG adm mcsv-mgr
usermod -aG mcsv-http mcsv-mgr
# Apache needs it to reach the socket; restart apache2 so it picks the group up.
if id www-data > /dev/null 2>&1; then usermod -aG mcsv-http www-data; fi

# Only directories created here are chowned. Never recurse: an existing host has
# worlds under instances/ whose owner and modes are not ours to rewrite.
for dir in /srv/minecraft /srv/minecraft/instances /srv/minecraft/shared; do
    if [ ! -e "$dir" ]; then
        mkdir "$dir"
        chown mcsv-mgr:mcsv-mgr "$dir"
        chmod 2775 "$dir"
    fi
done

mkdir -p /etc/apache2/sites-available
if [ ! -f /etc/apache2/sites-available/mcsv_manager.conf ]; then
    cp "$CONFIG_DIR/apache2/sites-available/mcsv_manager.conf" /etc/apache2/sites-available/
fi
# Never overwrite: it holds the operator's key and trusted uids.
if [ ! -f /etc/mcsv_manager/env ]; then
    mkdir -p /etc/mcsv_manager
    cp "$CONFIG_DIR/mcsv_manager/env" /etc/mcsv_manager/
fi

cp "$CONFIG_DIR/systemd/system/mcsv_manager.socket" /usr/lib/systemd/system/
cp "$CONFIG_DIR/systemd/system/mcsv_manager.service" /usr/lib/systemd/system/
cp "$CONFIG_DIR/systemd/system/minecraft@.socket" /usr/lib/systemd/system/
# minecraft@.service may already exist (often customised) on a host with servers:
# never overwrite it, install ours as .dist for the admin to compare.
if [ -e /usr/lib/systemd/system/minecraft@.service ] || [ -e /etc/systemd/system/minecraft@.service ]; then
    cp "$CONFIG_DIR/systemd/system/minecraft@.service" /usr/lib/systemd/system/minecraft@.service.dist
    echo "setup.sh: minecraft@.service already exists, left untouched." >&2
    echo "setup.sh: compare it with /usr/lib/systemd/system/minecraft@.service.dist" >&2
else
    cp "$CONFIG_DIR/systemd/system/minecraft@.service" /usr/lib/systemd/system/
fi
cp "$CONFIG_DIR/tmpfiles.d/minecraft.conf" /usr/lib/tmpfiles.d/

systemd-tmpfiles --create

# Repo-owned and security-relevant (it scopes what mcsv-mgr may manage), so a
# re-run replaces it.
mkdir -p /etc/polkit-1/rules.d
cp "$CONFIG_DIR/polkit-1/rules.d/mcsv-manager.rules" /etc/polkit-1/rules.d/

systemctl daemon-reload
systemctl restart mcsv_manager.socket
