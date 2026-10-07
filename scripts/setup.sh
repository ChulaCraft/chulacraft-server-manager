#/bin/bash

set -e

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
CONFIG_DIR="$SCRIPT_DIR/config"

addgroup --system mcsv-mgr
adduser --system --ingroup mcsv-mgr --no-create-home mcsv-mgr
adduser --system --ingroup mcsv-mgr --no-create-home mcsv
usermod -aG adm mcsv-mgr

# Allow Apache (www-data) to write to the Unix Socket
usermod -aG mcsv-mgr www-data

mkdir -p /srv/minecraft/{instances,shared}
chown -R mcsv-mgr:mcsv-mgr /srv/minecraft
chmod -R 2775 /srv/minecraft

mkdir -p /etc/apache2/sites-available
if [ ! -f /etc/apache2/sites-available/mcsv_manager.conf ]; then cp $CONFIG_DIR/apache2/sites-available/mcsv_manager.conf /etc/apache2/sites-available/; fi
if [ ! -f /etc/mcsv_manager/env ]; then
    mkdir -p /etc/mcsv_manager
    cp $CONFIG_DIR/mcsv_manager/env /etc/mcsv_manager/
fi

cp $CONFIG_DIR/systemd/system/mcsv_manager.socket /usr/lib/systemd/system/
cp $CONFIG_DIR/systemd/system/mcsv_manager.service /usr/lib/systemd/system/
cp $CONFIG_DIR/systemd/system/minecraft@.socket /usr/lib/systemd/system/
cp $CONFIG_DIR/tmpfiles.d/minecraft.conf /usr/lib/tmpfiles.d/

systemd-tmpfiles --create

if [ ! -f /etc/polkit-1/rules.d/mcsv-manager.rules ]; then cp $CONFIG_DIR/polkit-1/rules.d/mcsv-manager.rules /etc/polkit-1/rules.d/; fi
