#!/bin/sh
# Build, install, and load the tethering daemon. Run with sudo.
set -eu

LABEL=dev.jost.rndis-tether
PLIST="/Library/LaunchDaemons/$LABEL.plist"
BIN_DIR=/usr/local/bin
SRC_DIR=$(cd "$(dirname "$0")" && pwd)

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh must run as root: sudo $0" >&2
    exit 1
fi

# Build as the invoking user so the toolchain and cargo cache are theirs.
BUILD_USER=${SUDO_USER:-$(id -un)}
echo "building as $BUILD_USER..."
sudo -u "$BUILD_USER" cargo build --release --manifest-path "$SRC_DIR/Cargo.toml"

if [ -f "$PLIST" ]; then
    echo "unloading the running daemon..."
    launchctl bootout "system/$LABEL" 2>/dev/null || true
fi

echo "installing binaries to $BIN_DIR..."
install -d "$BIN_DIR"
install -m 755 "$SRC_DIR/target/release/rndis-tetherd" "$BIN_DIR/rndis-tetherd"
install -m 755 "$SRC_DIR/target/release/rndis-tetherctl" "$BIN_DIR/rndis-tetherctl"

echo "installing $PLIST..."
install -m 644 -o root -g wheel "$SRC_DIR/launchd/$LABEL.plist" "$PLIST"

echo "loading the daemon..."
launchctl bootstrap system "$PLIST"
launchctl enable "system/$LABEL"

echo
echo "installed. enable USB tethering on the phone, then:"
echo "  rndis-tetherctl status"
echo "  tail -f /var/log/rndis-tether.log"
echo
echo "to remove: sudo launchctl bootout system/$LABEL && sudo rm $PLIST"
