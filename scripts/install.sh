#/bin/bash

set -e

BUILD_TARGET_DIR="target/release"

install-target() {
    install -m 755 "$BUILD_TARGET_DIR/$1" /usr/local/bin/$1
}

install-target mcsv_manager
install-target mcsvctl
install-target mcsv-console
