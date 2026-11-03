#!/bin/bash

set -euo pipefail

REMOVE_EVERYTHING=false
if [[ " $* " == *" --remove-everything "* ]]; then
  echo "removing everything"
  REMOVE_EVERYTHING=true
fi

systemctl stop mcsv_manager.socket mcsv_manager.service || true

rm -f /usr/lib/systemd/system/mcsv_manager.socket \
    /usr/lib/systemd/system/mcsv_manager.service \
    /usr/lib/systemd/system/minecraft@.socket \
    /usr/lib/tmpfiles.d/minecraft.conf

systemctl stop mcsv-docker-proxy.service chulacraft-update@*.service 2> /dev/null || true
systemctl disable mcsv-docker-proxy.service 2> /dev/null || true
rm -f /etc/systemd/system/mcsv-docker-proxy.service \
    /etc/systemd/system/chulacraft-update@.service

systemctl daemon-reload

if [ "$REMOVE_EVERYTHING" = true ]; then
    rm -rf /etc/mcsv_manager/ /srv/minecraft/
    deluser --system mcsv-mgr || true
    deluser --system mcsv || true
    delgroup --system mcsv-mgr || true
    delgroup --system mcsv-http || true
    delgroup --system mcsv-docker || true

    rm -f /etc/polkit-1/rules.d/mcsv-manager.rules
fi
