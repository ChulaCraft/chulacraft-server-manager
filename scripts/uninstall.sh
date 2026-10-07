#/bin/bash

set -e

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
CONFIG_DIR="$SCRIPT_DIR/config"

REMOVE_EVERYTHING=false
if [[ " $* " == *" --remove-everything "* ]]; then
  echo "removing everything"
  REMOVE_EVERYTHING=true
fi

systemctl stop mcsv_manager.socket

rm  /usr/lib/systemd/system/mcsv_manager.socket \
    /usr/lib/systemd/system/mcsv_manager.service \
    /usr/lib/systemd/system/minecraft@.socket \
    /usr/lib/tmpfiles.d/minecraft.conf

systemctl daemon-reload

if REMOVE_EVERYTHING; then

rm -rf /etc/mcsv_manager/ /srv/minecraft/
deluser --system mcsv-mgr
deluser --system mcsv
delgroup --system mcsv-mgr

rm /etc/polkit-1/rules.d/mcsv-manager.rules

fi