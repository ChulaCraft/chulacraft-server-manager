#!/bin/bash

set -e

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
BUILD_TARGET_DIR="target/release"

install-target() {
    install -m 755 "$BUILD_TARGET_DIR/$1" /usr/local/bin/$1
}

install-target mcsv_manager
install-target mcsvctl
install-target mcsv-console

# Feature scripts (docker proxy, backups, ...) add themselves here instead of
# editing this file. They are sourced, so they can use install-target.
for fragment in "$SCRIPT_DIR"/install.d/*.sh; do
    [ -e "$fragment" ] || continue
    source "$fragment"
done
