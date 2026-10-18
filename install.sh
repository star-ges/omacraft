#!/usr/bin/env bash
# Build and install Omacraft for the current user (~/.local), or update an existing install.
# Run it from a clone, or on its own:
#   curl -fsSL https://raw.githubusercontent.com/star-ges/omacraft/master/install.sh | bash
set -euo pipefail

REPO=https://github.com/star-ges/omacraft
MIN_RUST=1.95
BIN_DIR="$HOME/.local/bin"
SHARE_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"

say() { printf '\033[1m%s\033[0m\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || fail "Omacraft runs on Linux only."
[ "$(uname -m)" = x86_64 ] || fail "Omacraft launches games on x86_64 only (this is $(uname -m))."

export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || fail "Rust isn't installed. Get it from https://rustup.rs, then run this again."
rust=$(rustc --version | awk '{print $2}')
if [ "$(printf '%s\n%s\n' "$MIN_RUST" "$rust" | sort -V | head -n1)" != "$MIN_RUST" ]; then
    fail "Omacraft needs Rust $MIN_RUST or newer (you have $rust). Run 'rustup update', then this again."
fi

# Use the clone this script sits in, else fetch a fresh copy.
src=""
here=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || true)
if [ -n "$here" ] && [ -f "$here/Cargo.toml" ] && grep -q '^name = "omacraft"' "$here/Cargo.toml"; then
    src=$here
else
    command -v git >/dev/null || fail "git isn't installed."
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    say "Downloading Omacraft…"
    git clone --quiet --depth 1 "$REPO" "$tmp/omacraft"
    src="$tmp/omacraft"
fi

say "Building (this takes a few minutes the first time)…"
cargo build --release --manifest-path "$src/Cargo.toml"

say "Installing…"
# Copy beside the old binary and rename over it, so a running Omacraft keeps working.
install -Dm755 "$src/target/release/omacraft" "$BIN_DIR/.omacraft.new"
mv -f "$BIN_DIR/.omacraft.new" "$BIN_DIR/omacraft"
install -Dm644 "$src/packaging/omacraft.desktop" "$SHARE_DIR/applications/omacraft.desktop"
install -Dm644 "$src/packaging/omacraft.svg" "$SHARE_DIR/icons/hicolor/scalable/apps/omacraft.svg"
command -v update-desktop-database >/dev/null && update-desktop-database -q "$SHARE_DIR/applications" || true

say "Omacraft is installed. Start it from your app launcher, or run 'omacraft'."
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Note: $BIN_DIR isn't on your PATH, so 'omacraft' won't run from a terminal until you add it." ;;
esac
if pgrep -x omacraft >/dev/null; then
    echo "Omacraft is running: restart it to use the new version."
fi
