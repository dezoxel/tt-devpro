#!/usr/bin/env bash
# install.sh — build tt-devpro and install it as a global binary on ~/.local/bin.
#
# One self-contained binary, no runtime engine and no build-time toolchain
# beyond a stock Rust install: `cargo build --release`, then copy the result.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

BIN_DIR="$HOME/.local/bin"
mkdir -p "$BIN_DIR"

echo "Building release binary..."
cargo build --release

BIN="target/release/tt-devpro"
if [ ! -x "$BIN" ]; then
    echo "Build succeeded but $BIN is missing or not executable." >&2
    exit 1
fi

install -m 0755 "$BIN" "$BIN_DIR/tt-devpro"
echo "Installed → $BIN_DIR/tt-devpro"

# The pre-Rust installer's JVM fallback unpacked a distribution here and pointed
# the launcher at it. Nothing installs into it any more, so a copy left over from
# that installer would sit on disk forever; this tool is its sole owner.
rm -rf "$HOME/.local/lib/tt-devpro"

echo
case ":$PATH:" in
    *":$BIN_DIR:"*) echo "Done. Run: tt-devpro settle --dry-run" ;;
    *) echo "Done, but $BIN_DIR is not on your PATH. Add it, then run: tt-devpro settle --dry-run" ;;
esac
