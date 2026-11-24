#!/bin/bash
# Install omarchy-recipe: build the Rust binary and install it into ~/.local/bin
# (plus argv0 compat symlinks: omarchy-recipe-export, -import, -validate, -build-iso).
set -euo pipefail

SRC_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
DEST="${1:-$HOME/.local/bin}"
mkdir -p "$DEST"
DEST="$(cd -- "$DEST" && pwd)"

export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. Install Rust first: https://rustup.rs" >&2
  exit 1
fi

cargo build --release --locked --manifest-path "$SRC_DIR/Cargo.toml"
TARGET_DIR="${CARGO_TARGET_DIR:-$SRC_DIR/target}"
BIN="$TARGET_DIR/release/omarchy-recipe"

# Copy the binary so deleting or moving the source checkout does not break the
# installed command. Aliases point at the installed copy, not into target/.
install -m 755 "$BIN" "$DEST/omarchy-recipe"
ln -sf "$DEST/omarchy-recipe" "$DEST/omarchy-recipe-export"
ln -sf "$DEST/omarchy-recipe" "$DEST/omarchy-recipe-import"
ln -sf "$DEST/omarchy-recipe" "$DEST/omarchy-recipe-validate"
ln -sf "$DEST/omarchy-recipe" "$DEST/omarchy-recipe-build-iso"

echo "Installed to $DEST. Ensure it is on PATH, then run:"
echo "  omarchy-recipe export --out ./my-machine.recipe"
