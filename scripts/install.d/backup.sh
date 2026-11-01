# Sourced by scripts/install.sh. Installs the backup script and units; does not
# enable the timer (the host admin does, after testing each target by hand).
BACKUP_SRC="$SCRIPT_DIR/backup"
UNIT_SRC="$SCRIPT_DIR/../config/systemd/system"

install -d -m 0755 /usr/local/lib/chulacraft-backup
install -m 0755 "$BACKUP_SRC/chulacraft-backup.sh" /usr/local/lib/chulacraft-backup/chulacraft-backup.sh

install -d -m 0700 -o root -g root /var/backups/chulacraft
install -d -m 0700 -o root -g root /etc/chulacraft /etc/chulacraft/backup
# Manifests: the manager reads sizes and status, never an archive. setgid, so the
# files the script creates keep the group.
install -d -m 2750 -o root -g mcsv-mgr /var/lib/chulacraft-backup /var/lib/chulacraft-backup/manifest
[ -e /etc/chulacraft/backup/backup.env ] \
    || install -m 0600 "$BACKUP_SRC/backup.env.example" /etc/chulacraft/backup/backup.env

for unit in chulacraft-backup@.service chulacraft-backup-nightly.service chulacraft-backup-nightly.timer; do
    install -m 0644 "$UNIT_SRC/$unit" /etc/systemd/system/$unit
done
systemctl daemon-reload
