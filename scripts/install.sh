#!/usr/bin/env bash
# Build the plugin and install it into OpenDeck's plugin directory.
#
#   ./scripts/install.sh              build + install
#   ./scripts/install.sh --restart    also restart OpenDeck so it reloads
#
# OpenDeck reads manifest.json once, at startup. Any change to the manifest
# needs --restart, and any change to an action's shape needs the action's
# instances removed and re-added in the UI afterwards.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGIN_DIR_NAME="com.biviale.pwsounddeck.sdPlugin"
BIN_NAME="pwsounddeck"
SRC="$ROOT/$PLUGIN_DIR_NAME"
DEST="${XDG_CONFIG_HOME:-$HOME/.config}/opendeck/plugins/$PLUGIN_DIR_NAME"

echo "==> cargo build --release"
cargo build --release --manifest-path "$ROOT/Cargo.toml"

echo "==> staging binary into $PLUGIN_DIR_NAME"
install -m 755 "$ROOT/target/release/$BIN_NAME" "$SRC/$BIN_NAME"

echo "==> installing to $DEST"
mkdir -p "$DEST"
# --delete keeps the installed copy honest when a file is removed from the
# source tree; the binary is copied last so a half-synced plugin never runs.
rsync -a --delete --exclude "$BIN_NAME" "$SRC/" "$DEST/"
install -m 755 "$SRC/$BIN_NAME" "$DEST/$BIN_NAME"

if [[ "${1:-}" == "--restart" ]]; then
	echo "==> restarting OpenDeck"
	pkill -x opendeck 2>/dev/null || true
	# Wait for the old process to release the device before starting again.
	for _ in $(seq 20); do
		pgrep -x opendeck >/dev/null || break
		sleep 0.25
	done
	setsid opendeck --hide >/dev/null 2>&1 < /dev/null &
	disown || true
	echo "    OpenDeck restarted."
else
	echo "    Installed. Restart OpenDeck to load it (or re-run with --restart)."
fi

echo "==> logs: ~/.local/share/opendeck/logs/plugins/$PLUGIN_DIR_NAME.log"
