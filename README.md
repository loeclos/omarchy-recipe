# omarchy-recipe

**Move your Omarchy setup to another machine without rebuilding it by hand.**

`omarchy-recipe` captures your packages, themes, fonts, plugins, services, web apps, and dotfiles into a small **recipe bundle**. You can apply that bundle to another Omarchy installation or bake it into a custom Omarchy ISO.

A recipe bundle is usually much smaller and easier to share than an ISO. Use an ISO when you want an installer that applies your setup automatically on first boot.

---

## Quick start

### Install

You need Rust and Cargo installed.

```sh
./install.sh
```

This builds the release binary and installs it into `~/.local/bin`. Make sure that directory is on your `PATH`.

You can also install to a different directory:

```sh
./install.sh ~/bin
```

The installer creates these commands:

- `omarchy-recipe`
- `omarchy-recipe-export`
- `omarchy-recipe-import`
- `omarchy-recipe-validate`
- `omarchy-recipe-build-iso`

The short commands are aliases. For example, these are equivalent:

```sh
omarchy-recipe export --out ./my-machine.recipe
omarchy-recipe-export --out ./my-machine.recipe
```

### Export your machine

```sh
omarchy-recipe export --out ./my-machine.recipe
```

This creates a directory containing:

```text
my-machine.recipe/
├── recipe.json          # Packages, themes, settings, and file hashes
└── dotfiles.tar.zst     # Filtered copy of your dotfiles
```

If you use vendored AUR mode, it also contains:

```text
aur-pkgs/                # Prebuilt AUR packages and their local repository index
```

### Check the bundle

```sh
omarchy-recipe validate ./my-machine.recipe
```

Validation checks the recipe format, file paths, tarball hash, and hashes for newly vendored AUR packages.

### Apply it to another machine

Copy or share the bundle directory, then run:

```sh
omarchy-recipe import ./my-machine.recipe
```

Review the prompt and confirm. The importer:

1. Validates the bundle.
2. Installs repository and AUR packages.
3. Backs up the target's `~/.config`.
4. Restores dotfiles, themes, fonts, plugins, and allowed services.
5. Reloads the desktop.

The target's Hyprland monitor configuration is never replaced with the source machine's layout.

---

## Choose the right workflow

### 1. Normal clone over a network

Use this when the target machine has internet access.

```sh
# Machine A
omarchy-recipe export --out ./my-machine.recipe

# Machine B
omarchy-recipe import ./my-machine.recipe
```

AUR packages are installed with `yay` or `paru` when the recipe is applied.

### 2. Smaller bundle

Skip large or unnecessary sections:

```sh
omarchy-recipe export --out ./lean.recipe \
  --without webapps,backgrounds,icons,aur
```

Exclude additional paths using `tar`-style globs:

```sh
omarchy-recipe export --out ./lean.recipe \
  --without services \
  --exclude '*/notesy/*' \
  --exclude '*/large-vault/*'
```

`--exclude` can be repeated.

### 3. Bundle AUR packages for offline use

Vendoring builds AUR packages now and puts the resulting `.pkg.tar.zst` files inside the bundle:

```sh
omarchy-recipe export --out ./airgap.recipe --aur-mode vendored
```

If you already have prebuilt packages, reuse them:

```sh
omarchy-recipe export --out ./airgap.recipe \
  --aur-mode vendored \
  --aur-pkgdir ./prebuilt
```

Apply the offline bundle with:

```sh
omarchy-recipe import ./airgap.recipe --offline
```

Offline import cannot download missing repository packages, custom themes, or third-party plugins. Those must already be installed or included in the restored dotfiles.

### 4. Custom ISO

Create an installer that applies your recipe on first boot:

```sh
omarchy-recipe build-iso ./my-machine.recipe --mirror stable
```

Or export the current machine and bake it in one step:

```sh
omarchy-recipe build-iso --mirror edge
```

Automatic export never includes secrets. Its bundle is kept under `~/.cache/omarchy-recipe/auto-bundles/` so you can inspect or reuse it.

A real ISO build needs:

- Docker
- `sudo` access
- Network access
- Plenty of disk space; **25 GB free is the minimum preflight check**, and more may be needed

Test the generated ISO layer without Docker:

```sh
omarchy-recipe build-iso ./my-machine.recipe --dry-run
```

After a successful bake, the tool offers to boot the ISO in QEMU. Control this with `--boot ask`, `--boot yes`, or `--boot no`.

---

## What gets exported

| Section | What it contains |
|---|---|
| `packages` | Explicitly installed repository packages, excluding Omarchy's base package lists |
| `aur` | Explicitly installed foreign/AUR packages |
| `themes` | Current theme and installed custom theme sources |
| `font` | Current monospace font |
| `plugins` | Installed shell plugins and whether they are enabled |
| `services` | Allowlisted systemd user and system services |
| `webapps` | Web-app `.desktop` launchers |
| `backgrounds` | Theme background images stored in dotfiles |
| `icons` | Web-app icons stored in dotfiles |

### Always excluded

The archive always leaves out common caches and machine-specific bulk, including:

- Hyprland monitor configuration
- Browser profiles and caches
- `node_modules`
- Application caches
- `yay` cache files
- Previous recipe backups

### Secrets are opt-in

By default, the archive excludes:

- `~/.ssh`
- `~/.gnupg`
- `~/.pki`
- Paths containing `secret` or `token`

To include them:

```sh
omarchy-recipe export --out ./fleet.recipe --include-secrets
```

**Anyone with the bundle can use those credentials. Only share secret-bearing bundles through trusted channels.**

A bundle containing secrets is also refused by `build-iso` unless you explicitly pass `--allow-secrets`.

---

## Understanding recipe files

### `recipe.json`

This is the portable description of your setup. It records:

- Omarchy version and channel
- Repository and AUR package lists
- Theme, font, plugin, and service state
- Dotfiles archive filename and SHA-256 hash
- Export selections and exclusions
- Vendored AUR package filenames and hashes

The current writer produces schema **v2**. The reader still accepts schema **v1** and migrates it in memory.

Legacy v1 bundles without a recorded tarball hash remain readable with a warning. New bundles require a valid SHA-256 hash.

### Dotfiles archive

The exporter uses your system's `tar`:

- With zstd support, it writes `dotfiles.tar.zst`.
- Without zstd support, it writes a real gzip archive named `dotfiles.tar.gz`.

Web-app launchers and icons are included in the same archive rather than appended later.

### Backups

Before restoring dotfiles, the importer copies the target's existing `~/.config` to:

```text
~/.config.pre-recipe-<timestamp>
```

If multiple imports happen within the same second, a numeric suffix is added.

---

## Complete option reference

Options marked **global** can appear before or after the command. Output is quiet by default: you see stages, warnings, and the final summary. Use `--verbose` for full detail.

### General options

| Option | Description |
|---|---|
| `-v`, `--verbose` | **Global.** Show detailed output instead of only stages, warnings, and summaries. During ISO builds, also stream Docker output live. |
| `-h`, `--help` | Show help. Use `omarchy-recipe <command> --help` for command-specific help. |
| `-V`, `--version` | Show the program version. |

Running `omarchy-recipe` without a command prints a short help screen. You can also use Clap's built-in help command:

```sh
omarchy-recipe help
omarchy-recipe help export
```

---

### `export`

Capture the current machine into a recipe bundle.

```text
omarchy-recipe export [OPTIONS]
```

| Option | Description |
|---|---|
| `--out <DIR>` | Output bundle directory. Created if missing. Default: `./<hostname>.recipe`. |
| `--without <SECTIONS>` | Skip a comma-separated list of sections. See the section list below. |
| `--exclude <GLOB>` | Add a `tar --exclude` pattern. Repeatable. |
| `--aur-mode <MODE>` | `wifi` or `vendored`. Default: `wifi`. |
| `--aur-pkgdir <DIR>` | Reuse matching prebuilt AUR `.pkg.tar.zst` files from this directory instead of building them. |
| `--include-secrets` | Include SSH keys, GPG data, PKI data, and paths matching `*secret*` or `*token*`. |
| `-v`, `--verbose` | **Global.** Show detailed output. |
| `-h`, `--help` | Show export help. |

#### `--without` values

| Value | Skips |
|---|---|
| `packages` | Repository package list |
| `aur` | AUR package list |
| `themes` | Theme state and theme directories |
| `font` | Font selection |
| `plugins` | Plugin state |
| `services` | Service state |
| `webapps` | `.desktop` launchers |
| `backgrounds` | Background images |
| `icons` | Web-app icons |

Example:

```sh
omarchy-recipe export \
  --out ./lean.recipe \
  --without packages,aur,themes,font,plugins,services,webapps,backgrounds,icons
```

#### `--aur-mode` values

| Value | Behavior |
|---|---|
| `wifi` | Record AUR package names and install them with `yay` or `paru` during import. This is the default. |
| `vendored` | Build or reuse AUR packages now and include them in `aur-pkgs/` for offline installation. |

Building AUR packages must run as a normal user because `makepkg` refuses to run as root. Use `--aur-pkgdir` when exporting from an automated root process.

#### Export examples

```sh
# Default export
omarchy-recipe export --out ./my-machine.recipe

# Skip bulky sections
omarchy-recipe export --out ./lean.recipe \
  --without webapps,backgrounds,icons

# Add custom exclusions
omarchy-recipe export --out ./work.recipe \
  --exclude '*/work-vault/*' \
  --exclude '*/node_modules/*'

# Offline AUR packages
omarchy-recipe export --out ./airgap.recipe \
  --aur-mode vendored \
  --aur-pkgdir ./prebuilt

# Trusted bundle containing secrets
omarchy-recipe export --out ./fleet.recipe --include-secrets
```

---

### `import`

Apply a recipe bundle to the current machine.

```text
omarchy-recipe import [OPTIONS] [BUNDLE]
```

| Argument/Option | Description |
|---|---|
| `BUNDLE` | Recipe bundle directory. If omitted, the current directory must contain `recipe.json`. |
| `--yes` | Skip the confirmation prompt. |
| `--skip-packages` | Do not install or verify packages. |
| `--skip-dotfiles` | Do not restore the dotfiles archive. |
| `--first-boot` | Advanced: run as root for another user as part of ISO first-boot provisioning. |
| `--home <DIR>` | Override the target home directory. Mainly useful with `--first-boot`. |
| `--as-user <USER>` | Run user-scoped theme, font, and plugin commands as this user. Mainly useful with `--first-boot`. |
| `--offline` | Never access the network. Requires vendored AUR packages and already-available themes/plugins. |
| `-v`, `--verbose` | **Global.** Show detailed output. |
| `-h`, `--help` | Show import help. |

#### Import examples

```sh
# Import a bundle from the current directory
omarchy-recipe import

# Import a specific bundle without prompting
omarchy-recipe import ./my-machine.recipe --yes

# Apply only dotfiles and desktop settings
omarchy-recipe import ./my-machine.recipe \
  --yes \
  --skip-packages

# Offline import with vendored AUR packages
omarchy-recipe import ./airgap.recipe --offline

# Advanced first-boot form used by the generated systemd service
sudo /usr/local/bin/omarchy-recipe import /var/lib/omarchy/recipe \
  --yes \
  --first-boot
```

#### Import behavior worth knowing

- The target's `~/.config` is backed up before restoration.
- The source machine's Hyprland monitor configuration is always discarded.
- Restored SSH/GPG/PKI files are made private.
- Interactive imports return a non-zero exit code when package, theme, font, plugin, or service application fails.
- First-boot imports restore dotfiles before waiting on the network for AUR packages.
- Vendored AUR dependency failures during first boot are logged but do not block desktop provisioning.
- The first-boot service disables itself after a successful run. If provisioning fails, it remains enabled and retries on a later boot.

---

### `validate`

Check a recipe bundle without applying it.

```text
omarchy-recipe validate [OPTIONS] [BUNDLE]
```

| Argument/Option | Description |
|---|---|
| `BUNDLE` | Recipe bundle directory. If omitted, the current directory must contain `recipe.json`. |
| `-v`, `--verbose` | **Global.** Show detailed output. |
| `-h`, `--help` | Show validate help. |

Validation checks:

- Supported schema version
- Required recipe fields
- Safe bundle-relative file paths
- Valid package, theme, plugin, font, and service names
- Dotfiles archive existence and SHA-256 hash
- Vendored AUR package existence and recorded hashes, when present

Examples:

```sh
omarchy-recipe validate ./my-machine.recipe

# From inside the bundle directory
cd ./my-machine.recipe
omarchy-recipe validate
```

---

### `build-iso`

Bake a recipe into a custom Omarchy ISO.

```text
omarchy-recipe build-iso [OPTIONS] [BUNDLE]
```

| Argument/Option | Description |
|---|---|
| `BUNDLE` | Existing recipe bundle. If omitted, the current machine is exported automatically. |
| `--iso-checkout <DIR>` | Reuse an existing `omarchy-iso` checkout instead of cloning one. |
| `--mirror <CHANNEL>` | `stable`, `rc`, or `edge`. Default: `stable`. |
| `--aur <MODE>` | Override the bundle's AUR mode: `wifi` or `vendored`. |
| `--aur-pkgdir <DIR>` | Reuse prebuilt AUR packages when vendoring is needed. |
| `--without <SECTIONS>` | When auto-exporting, skip a comma-separated list of sections using the same values as `export --without`. Ignored when `BUNDLE` is supplied. |
| `--exclude <GLOB>` | When auto-exporting, add a `tar --exclude` pattern. Repeatable. Ignored when `BUNDLE` is supplied. |
| `--allow-secrets` | Allow a bundle marked as containing secrets to be baked into an ISO. |
| `--dry-run` | Prepare and patch the ISO layer without running Docker or `mkarchiso`. |
| `--workdir <DIR>` | Directory for the prepared checkout and build logs. Default: `~/.cache/omarchy-recipe/iso-build-<pid>`. |
| `--boot <MODE>` | `ask`, `yes`, or `no`. Default: `ask`. Controls the QEMU test-drive prompt after a successful bake. |
| `-v`, `--verbose` | **Global.** Stream detailed output, including Docker logs. |
| `-h`, `--help` | Show build-iso help. |

#### `--mirror` values

| Value | Meaning |
|---|---|
| `stable` | Stable Omarchy ISO channel. Default. |
| `rc` | Release candidate channel. |
| `edge` | Latest development channel. |

#### `--aur` values

| Value | Meaning |
|---|---|
| `wifi` | Install AUR packages with `yay` or `paru` on first boot. |
| `vendored` | Include prebuilt AUR packages for offline first-boot installation. |

#### `--boot` values

| Value | Meaning |
|---|---|
| `ask` | Ask after a successful bake. Automatically skips the prompt when stdin is not interactive. Default. |
| `yes` | Launch the ISO in QEMU without asking. |
| `no` | Never launch QEMU. |

#### Build files and logs

With the default workdir, files are placed under:

```text
~/.cache/omarchy-recipe/iso-build-<pid>/
├── iso-build.log
└── docker-pull.log
```

When `--iso-checkout` is supplied, the checkout stays where you put it; `--workdir` still controls log placement.

#### Build examples

```sh
# Export this machine and bake a stable ISO
omarchy-recipe build-iso --mirror stable

# Bake an existing bundle
omarchy-recipe build-iso ./my-machine.recipe --mirror edge

# Prepare the ISO layer without Docker
omarchy-recipe build-iso ./my-machine.recipe --dry-run

# Auto-export a lean bundle
omarchy-recipe build-iso --mirror edge \
  --without webapps,backgrounds,icons \
  --exclude '*/large-vault/*'

# Reuse a checkout and choose a build directory
omarchy-recipe build-iso ./my-machine.recipe \
  --iso-checkout ~/src/omarchy-iso \
  --workdir ~/iso-work \
  --boot no

# Override the bundle's AUR mode
omarchy-recipe build-iso ./my-machine.recipe \
  --aur vendored \
  --aur-pkgdir ./prebuilt

# Explicitly allow a trusted secret-bearing bundle
omarchy-recipe build-iso ./fleet.recipe \
  --allow-secrets \
  --boot no

# Stream the full build log
omarchy-recipe --verbose build-iso ./my-machine.recipe
```

---

## How custom ISOs work

The tool does not replace Archinstall or reimplement `archiso`. Instead, it:

1. Clones the official [`omacom-io/omarchy-iso`](https://github.com/omacom-io/omarchy-iso) repository, or reuses your checkout.
2. Adds the recipe payload to the live ISO.
3. Patches the official package lists so recipe packages are available in the offline mirror.
4. Registers a first-boot phase that copies the payload onto the installed system.
5. Enables `omarchy-apply-recipe.service`, which runs the same `import --first-boot` code used for manual imports.

Patches use exact anchors and fail loudly if upstream changes their shape. Patch application is transactional, so a failed patch does not leave a caller-supplied checkout half-modified.

The first-boot service is `Type=simple`, so it does not hold up boot. It retries network-dependent work and disables itself after success.

---

## Troubleshooting

### `cargo: command not found`

Install Rust and Cargo, then make sure Cargo's bin directory is on your `PATH`.

### `no recipe.json in ...`

Pass the bundle directory, or `cd` into it:

```sh
omarchy-recipe import ./my-machine.recipe
# or
cd ./my-machine.recipe
omarchy-recipe import
```

### `sha256 mismatch`

The archive changed after it was exported, or the download was incomplete. Re-copy the bundle or export it again.

### `aur_mode=vendored but bundle has no aur-pkgs/ manifest`

Re-export with `--aur-mode vendored`, or import with network access using a Wi-Fi-mode bundle.

### ISO build reports less than 25 GB free

Choose a larger filesystem:

```sh
omarchy-recipe build-iso ./my-machine.recipe \
  --workdir /path/with/more/space
```

Avoid `/tmp` when it is a small tmpfs. Docker's storage filesystem may also need substantial free space.

### `already patched (marker present)`

The supplied `omarchy-iso` checkout already contains recipe patches. Use a clean checkout or a different `--workdir`.

### AUR build fails as root

`makepkg` cannot run as root. Export as your normal user or provide prebuilt packages:

```sh
omarchy-recipe export --out ./airgap.recipe \
  --aur-mode vendored \
  --aur-pkgdir ./prebuilt
```

---

## Pacman and AUR packages

The repository includes two package templates:

| File | Package | Update model |
|---|---|---|
| `PKGBUILD` | `omarchy-recipe` | Stable: pinned to a specific commit until the maintainer updates it |
| `PKGBUILD-git` | `omarchy-recipe-git` | Rolling: rebuilt from the latest `main` commit |

### Stable vs rolling

**Stable** is best for most users:

- You receive a known, tested revision.
- The package version changes only when the maintainer publishes an update.
- Problems are easier to reproduce and support.
- Fixes take longer to reach users because a new package release is required.

**Rolling** is best for testers and contributors:

- You receive new commits shortly after they are pushed.
- There is no separate release step.
- Untested changes can reach users immediately.
- A broken upstream commit can temporarily break the package build.

The two packages conflict with each other and the rolling package provides `omarchy-recipe`, so install only one.

### Build the stable package locally

```sh
makepkg -si
```

This downloads the pinned source archive, verifies its checksum, builds the release binary, runs the tests, and installs the package.

### Build the rolling package locally

```sh
makepkg -p PKGBUILD-git -si
```

### Publishing to the AUR

The stable and rolling packages must live in separate AUR repositories:

- `omarchy-recipe`
- `omarchy-recipe-git`

For the stable package:

```sh
makepkg --printsrcinfo > .SRCINFO
```

For the rolling package:

```sh
makepkg -p PKGBUILD-git --printsrcinfo > .SRCINFO
```

Copy each `PKGBUILD` and its generated `.SRCINFO` into the corresponding AUR repository. Do not publish both files in one AUR package repository.

The stable `PKGBUILD` currently pins commit `3dc58ff56d7ab875e11b483a98d95752744bbf2f`. To publish a new stable version, update `pkgver`, `pkgrel`, the pinned commit, and the source checksum together.

---

## Development

Requirements:

- Rust and Cargo
- `jq` for the integration test
- Python 3 and Bash for generated-script syntax checks

Run the test suite:

```sh
cargo test
```

Build and run the mocked export → validate → import integration test:

```sh
cargo build
bash tests/roundtrip.sh
```

Run Clippy:

```sh
cargo clippy --all-targets -- -D warnings
```

Repository layout:

- `src/` — Rust source
- `assets/` — First-boot systemd unit and ISO orchestrator phase
- `schema/recipe.schema.json` — Recipe schema
- `tests/roundtrip.sh` — Mocked integration test
- `install.sh` — Release build and installer

For implementation details and design decisions, see [AGENTS.md](./AGENTS.md).
