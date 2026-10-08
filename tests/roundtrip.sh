#!/bin/bash
# Integration: export -> validate -> import roundtrip with mocked host tools.
# Also covers: --without blacklist, v1 bundle import, argv0 dispatch.
set -euo pipefail

REPO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_DIR/target/debug/omarchy-recipe"
[[ -x $BIN ]] || { echo "FAIL: build first (cargo build)" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
MOCKBIN="$WORK/mockbin"
mkdir -p "$MOCKBIN"
export PATH="$MOCKBIN:$PATH"

# --- fake omarchy checkout ---
mkdir -p "$WORK/omarchy-a/install" "$WORK/omarchy-a/themes/tokyo-night"
printf 'base-pkg\n' >"$WORK/omarchy-a/install/omarchy-base.packages"
printf 'other-pkg\n' >"$WORK/omarchy-a/install/omarchy-other.packages"
printf '4.0.0.test\n' >"$WORK/omarchy-a/version"

# --- mocked commands ---
cat >"$MOCKBIN/pacman" <<'EOF'
#!/bin/bash
# -Qeq lists every explicit install incl. AUR ones; -Qm lists foreign.
# AUR packages must land in `aur`, never in the repo mirror.
if [[ $1 == "-Qeq" ]]; then printf 'base-pkg\nother-pkg\nuser-pkg\naur-pkg\n'; exit 0; fi
if [[ $1 == "-Qm" ]]; then printf 'aur-pkg\n'; exit 0; fi
echo "mock pacman $*" >&2; exit 0
EOF
cat >"$MOCKBIN/yay" <<'EOF'
#!/bin/bash
if [[ $1 == "-Qmq" ]]; then printf 'aur-pkg\n'; exit 0; fi
echo "mock yay $*" >&2; exit 0
EOF
cat >"$MOCKBIN/omarchy-version-channel" <<'EOF'
#!/bin/bash
printf 'dev\n'
EOF
cat >"$MOCKBIN/omarchy-font-current" <<'EOF'
#!/bin/bash
printf 'JetBrainsMono Nerd Font\n'
EOF
cat >"$MOCKBIN/omarchy-plugin-list" <<'EOF'
#!/bin/bash
printf '[{"id":"omarchy.agents","enabled":true,"firstParty":true}]'
EOF
cat >"$MOCKBIN/systemctl" <<'EOF'
#!/bin/bash
exit 0
EOF
for stub in omarchy-theme-install omarchy-theme-set omarchy-theme-refresh omarchy-font-set omarchy-plugin-enable omarchy-plugin-disable omarchy-pkg-add; do
  printf '#!/bin/bash\necho "mock %s $*" >&2\nexit 0\n' "$stub" >"$MOCKBIN/$stub"
done
chmod +x "$MOCKBIN"/*

# --- fake machine A HOME ---
export HOME="$WORK/homeA"
mkdir -p "$HOME/.config/omarchy/themes" "$HOME/.config/nvim" \
  "$HOME/.config/hypr" "$HOME/.config/omarchy/themes/tokyo-night" \
  "$HOME/.config/yay" "$HOME/.local/share/applications" \
  "$HOME/.local/state/omarchy/current" "$HOME/.config/omarchy"
printf 'tokyo-night' >"$HOME/.local/state/omarchy/current/theme.name"
printf 'set nocompatible\n' >"$HOME/.config/nvim/init.vim"
printf '{"version":1}\n' >"$HOME/.config/omarchy/shell.json"
printf '$monitor = eDP-1\n' >"$HOME/.config/hypr/monitors.conf"
printf 'should-be-excluded\n' >"$HOME/.config/yay/cache-file"
printf '[Desktop Entry]\nName=TestApp\nExec=xdg-open https://example.com\n' \
  >"$HOME/.local/share/applications/testapp.desktop"
export OMARCHY_PATH="$WORK/omarchy-a"

"$BIN" export --out "$WORK/a.recipe" >/dev/null
[[ -f $WORK/a.recipe/recipe.json && -f $WORK/a.recipe/dotfiles.tar.zst ]] \
  || { echo "FAIL: bundle files missing" >&2; exit 1; }

command -v jq >/dev/null || { echo "SKIP: jq needed for assertions"; exit 0; }
jq -e '.schema_version == 2' "$WORK/a.recipe/recipe.json" >/dev/null \
  || { echo "FAIL: expected schema v2" >&2; exit 1; }
jq -e '.packages.explicit_repo == ["user-pkg"]' "$WORK/a.recipe/recipe.json" >/dev/null \
  || { echo "FAIL: explicit_repo wrong" >&2; jq .packages "$WORK/a.recipe/recipe.json" >&2; exit 1; }
jq -e '.packages.aur == ["aur-pkg"]' "$WORK/a.recipe/recipe.json" >/dev/null \
  || { echo "FAIL: aur list wrong" >&2; exit 1; }
jq -e '(.webapps|length) == 1' "$WORK/a.recipe/recipe.json" >/dev/null \
  || { echo "FAIL: webapps not recorded" >&2; exit 1; }
if tar --zstd -tf "$WORK/a.recipe/dotfiles.tar.zst" 2>/dev/null | grep -q 'yay/cache-file'; then
  echo "FAIL: blocklisted yay cache leaked into tarball" >&2; exit 1
fi
if ! tar --zstd -tf "$WORK/a.recipe/dotfiles.tar.zst" 2>/dev/null | grep -q 'testapp.desktop'; then
  echo "FAIL: webapp .desktop not shipped in tarball" >&2; exit 1
fi

# --- blacklist export ---
"$BIN" export --out "$WORK/b.recipe" --without webapps,aur,backgrounds --exclude '*/nvim/*' >/dev/null
jq -e '(.webapps|length) == 0 and (.packages.aur|length) == 0' "$WORK/b.recipe/recipe.json" >/dev/null \
  || { echo "FAIL: --without not honored in recipe.json" >&2; exit 1; }
jq -e '.selection.excluded_sections | contains(["webapps"])' "$WORK/b.recipe/recipe.json" >/dev/null \
  || { echo "FAIL: selection not recorded" >&2; exit 1; }
if tar --zstd -tf "$WORK/b.recipe/dotfiles.tar.zst" 2>/dev/null | grep -q 'testapp.desktop\|nvim/init.vim'; then
  echo "FAIL: excluded content leaked into tarball" >&2; exit 1
fi

# --- validate (incl. argv0 symlink dispatch) ---
ln -sf "$BIN" "$WORK/omarchy-recipe-validate"
"$WORK/omarchy-recipe-validate" "$WORK/a.recipe" >/dev/null \
  || { echo "FAIL: validate rejected good bundle" >&2; exit 1; }

# --- fake machine B import ---
export HOME="$WORK/homeB"
mkdir -p "$HOME/.config/hypr"
printf '$monitor = HDMI-A-1\n' >"$HOME/.config/hypr/monitors.conf"
printf 'y\n' | "$BIN" import "$WORK/a.recipe" --skip-packages >/dev/null
[[ -f $HOME/.config/nvim/init.vim ]] || { echo "FAIL: nvim config not restored" >&2; exit 1; }
[[ -f $HOME/.local/share/applications/testapp.desktop ]] \
  || { echo "FAIL: webapp desktop not restored" >&2; exit 1; }
grep -q 'HDMI-A-1' "$HOME/.config/hypr/monitors.conf" \
  || { echo "FAIL: B monitor config was clobbered" >&2; exit 1; }

# --- v1 bundle still imports ---
mkdir -p "$WORK/v1.recipe" "$WORK/v1files"
printf 'v1-dotfiles\n' >"$WORK/v1files/dot.txt"
tar -czf "$WORK/v1.recipe/dotfiles.tar.zst" -C "$WORK/v1files" dot.txt
SHA="$(sha256sum "$WORK/v1.recipe/dotfiles.tar.zst" | awk '{print $1}')"
cat >"$WORK/v1.recipe/recipe.json" <<EOF
{"schema_version":1,"generated_by":"test","generated_at":"t",
 "omarchy":{"version":"x","channel":"dev","ref":"abc"},
 "packages":{"explicit_repo":[],"aur":[]},
 "themes":{"current":"tokyo-night","installed":[]},
 "dotfiles":{"file":"dotfiles.tar.zst","sha256":"$SHA"}}
EOF
"$BIN" validate "$WORK/v1.recipe" >/dev/null \
  || { echo "FAIL: v1 bundle rejected" >&2; exit 1; }

# --- secrets: filtered by default, shipped with --include-secrets ---
export HOME="$WORK/homeC"
mkdir -p "$HOME/.config" "$HOME/.ssh"
printf 'shhh\n' >"$HOME/.ssh/id_test"
"$BIN" export --out "$WORK/c.recipe" >/dev/null
if tar --zstd -tf "$WORK/c.recipe/dotfiles.tar.zst" 2>/dev/null | grep -q '\.ssh/id_test'; then
  echo "FAIL: secrets leaked into default bundle" >&2; exit 1
fi
jq -e '.selection.include_secrets == false' "$WORK/c.recipe/recipe.json" >/dev/null \
  || { echo "FAIL: include_secrets should default false" >&2; exit 1; }
"$BIN" export --out "$WORK/s.recipe" --include-secrets >/dev/null 2>&1
if ! tar --zstd -tf "$WORK/s.recipe/dotfiles.tar.zst" 2>/dev/null | grep -q '\.ssh/id_test'; then
  echo "FAIL: --include-secrets did not ship .ssh" >&2; exit 1
fi
jq -e '.selection.include_secrets == true' "$WORK/s.recipe/recipe.json" >/dev/null \
  || { echo "FAIL: include_secrets not recorded" >&2; exit 1; }
export HOME="$WORK/homeD"
mkdir -p "$HOME"
"$BIN" import "$WORK/s.recipe" --yes --skip-packages >/dev/null 2>&1
[[ -f $HOME/.ssh/id_test ]] || { echo "FAIL: secrets not restored on import" >&2; exit 1; }

echo "PASS: roundtrip"
