#!/bin/bash
# Install omarchy-recipe: build the Rust binary and link it into ~/.local/bin
# (plus argv0 compat symlinks: omarchy-recipe-export, -import, -validate, -build-iso).
set -euo pipefail

SRC_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
DEST="${1:-$HOME/.local/bin}"
mkdir -p "$DEST"

export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found. Install Rust first: https://rustup.rs" >&2
  exit 1
fi

cargo build --release --manifest-path "$SRC_DIR/Cargo.toml"
BIN="$SRC_DIR/target/release/omarchy-recipe"

ln -sf "$BIN" "$DEST/omarchy-recipe"
for sub in export import validate build-iso; do
  ln -sf "$BIN" "$DEST/omarchy-recipe-$sub"
done

echo "Installed to $DEST. Ensure it is on PATH, then run:"
echo "  omarchy-recipe export --out ./my-machine.recipe"
