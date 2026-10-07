# omarchy-recipe

One-click export on machine A → small shareable artifact, import or build-iso on machine B → exactly-like-mine Omarchy. Sharing a recipe (JSON + dotfiles tarball, KBs/MBs) beats sharing a 5GB ISO — unless you want the ISO, which this also bakes.

Rust port (V2): colored output, `export` section blacklists, vendored-AUR offline mode, and real `build-iso`.

## Install

```sh
./install.sh            # cargo build --release + links into ~/.local/bin
omarchy-recipe export --out ./my-machine.recipe
```

## Use

```sh
# Export (full clone: packages, themes, font, plugins, services, webapps)
omarchy-recipe export --out ./my-machine.recipe

# Lean export: skip bulky or personal sections
omarchy-recipe export --out ./lean.recipe --without webapps,backgrounds,icons,aur
omarchy-recipe export --out ./x.recipe --without services --exclude '*/notesy/*'

# Fully-offline bundle: prebuild AUR packages into the bundle
omarchy-recipe export --out ./airgap.recipe --aur-mode vendored
omarchy-recipe export --out ./airgap.recipe --aur-mode vendored --aur-pkgdir ./prebuilt

# Company cloning (trusted channels only): keep secrets in the bundle
# (.ssh, .gnupg, .pki, *secret*, *token* — filtered out by default)
omarchy-recipe export --out ./fleet.recipe --include-secrets

omarchy-recipe validate ./my-machine.recipe   # no arg = use ./
omarchy-recipe import ./my-machine.recipe --yes

# Bake a custom ISO (needs docker + sudo + network; --dry-run needs none).
# No bundle = export this machine on the spot and bake that.

omarchy-recipe build-iso --mirror edge
omarchy-recipe build-iso ./my-machine.recipe --mirror edge --dry-run
omarchy-recipe build-iso ./airgap.recipe --mirror stable

# Verbose mode: full detail (default shows animated stage spinners, warnings
# and the end summary; docker output streams to <workdir>/iso-build.log
# unless verbose, while the spinner tracks bake phases live)
omarchy-recipe --verbose build-iso ./my-machine.recipe --mirror edge
```

`--without` sections: `packages,aur,themes,font,plugins,services,webapps,backgrounds,icons`. The selection is recorded in `recipe.json` so import skips gracefully. `import` with no directory uses `./recipe.json` in cwd. Import backs up `~/.config` first and never overwrites the target's Hypr monitor config.

## build-iso: how it works

1. Clones `omacom-io/omarchy-iso` (or reuses `--iso-checkout`).
2. Stages the payload on the live ISO at `/usr/share/omarchy-iso/recipe/` (recipe.json, dotfiles, vendored AUR, this binary, first-boot unit).
3. Appends recipe repo packages to the offline mirror **and** the ISO-bundled base list, so they install on target with no network (anchor patches on `builder/build-iso.sh`; fails loudly on upstream drift).
4. Registers a `stage_recipe` orchestrator phase that copies the payload to `/var/lib/omarchy/recipe/` on target and enables `omarchy-apply-recipe.service`.
5. First boot runs this same binary (`import --first-boot`): restores dotfiles over the fresh home (waits for the owner under deferred provisioning), `pacman -U`s vendored AUR offline **or** retries `yay -S` with wifi, then disables itself. `Type=simple` so boot is never held hostage.

## Layout

- `src/` — Rust crate (`export,import,validate,aur,iso` + `recipe` v1/v2 model)
- `assets/` — `omarchy-apply-recipe.service`, `stage_recipe.py` (appended to the ISO orchestrator)
- `schema/recipe.schema.json` — v2 contract (reads v1)
- `tests/roundtrip.sh` — mocked export→import integration test

`cargo test` runs the unit suite (schema, validator, AUR reuse, patch application incl. bash/python syntax checks).
