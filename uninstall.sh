#!/usr/bin/env bash
# Remove Omacraft. Instances, worlds, accounts and settings stay unless you pass --purge.
#   ./uninstall.sh            remove the app
#   ./uninstall.sh --purge    also delete every instance, world, backup and signed-in account
set -euo pipefail

BIN_DIR="$HOME/.local/bin"
SHARE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
DATA_DIR="$SHARE_DIR/omacraft"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/omacraft"

purge=false
case "${1:-}" in
    "") ;;
    --purge) purge=true ;;
    *) echo "usage: $0 [--purge]" >&2; exit 2 ;;
esac

# Deleting data under a running game or launcher would leave it writing into nothing.
if $purge && pgrep -x omacraft >/dev/null; then
    echo "Omacraft is running. Quit it first." >&2
    exit 1
fi

rm -f "$BIN_DIR/omacraft" "$SHARE_DIR/applications/omacraft.desktop" "$SHARE_DIR/icons/hicolor/scalable/apps/omacraft.svg"
command -v update-desktop-database >/dev/null && update-desktop-database -q "$SHARE_DIR/applications" || true
echo "Omacraft is uninstalled."

if ! $purge; then
    [ -d "$DATA_DIR" ] && echo "Your instances and worlds are still in $DATA_DIR (run with --purge to delete them)."
    exit 0
fi

echo
echo "This permanently deletes:"
echo "  $DATA_DIR    every instance, world, backup and signed-in account"
echo "  $CONFIG_DIR  settings"
# Read from the terminal, so this also works when the script itself comes through a pipe.
read -r -p "Type 'delete' to go ahead: " answer </dev/tty
if [ "$answer" = delete ]; then
    rm -rf "$DATA_DIR" "$CONFIG_DIR"
    echo "Deleted."
else
    echo "Kept your data."
fi
