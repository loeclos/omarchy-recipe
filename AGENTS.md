# AGENTS.md — omarchy-recipe contributor handbook

Read this before touching the codebase. It records *why* things are built the
way they are, so future agents don't re-litigate settled decisions or repeat
past failures.

## 1. What this is

`omarchy-recipe` makes Omarchy configs portable without hand-operating
archiso. One-click export on machine A → small shareable artifact (JSON +
dotfiles tarball, KBs/MBs — preferred over sharing a 5GB ISO) → import or
`build-iso` on machine B → exactly-like-mine Omarchy.

Single Rust binary (`src/main.rs`), clap CLI, four subcommands:

| Command    | Purpose |
|------------|---------|
| `export`   | Capture this machine into a `.recipe/` bundle dir |
| `import`   | Apply a bundle onto fresh Omarchy (or first boot as root) |
| `validate` | Schema + tarball-sha256 check |
| `build-iso`| Bake a bundle into a custom Omarchy ISO via the official builder |

Install: `./install.sh` (release build + symlinks into `~/.local/bin`,
including `omarchy-recipe-{export,import,validate,build-iso}` argv[0] aliases).

## 2. Non-negotiable requirements

1. **Recipe JSON is the stable contract.** A future UI (or external tooling)
   consumes `recipe.json`, not our code. Schema lives in
   `schema/recipe.schema.json`. Current writer is **v2**; the loader accepts
   **v1 and v2** (`recipe.rs::Recipe::load` migrates v1 in memory). Never
   break v1 reads.
2. **Never reimplement archiso.** ISO customization injects into a clone of
   the official `omacom-io/omarchy-iso` repo (anchor patches + payload
   files), then runs *their* `bin/omarchy-iso-make`. A custom ISO must remain
   a genuine Omarchy ISO.
3. **The applier IS the binary.** First boot on an installed target runs this
   same binary (`import --first-boot`), staged onto the target by the
   installer phase. No second applier implementation may exist.
4. **Quiet by default.** Only stages, warnings/errors, and the end summary
   print. `--verbose` (`-v`) restores detail. Docker output always lands in
   `<workdir>/iso-build.log`; verbose additionally streams it.
5. **Secrets are opt-in, never accidental.** `.ssh`, `.gnupg`, `.pki`,
   `*secret*`, `*token*` are filtered unless `--include-secrets` is passed,
   which prints a loud warning. Top-level `~/.ssh` etc. live *outside*
   `.config`, so lifting excludes alone would silently miss them —
   `archive_dotfiles` adds those member dirs explicitly when the flag is set.

## 3. Architecture (`src/`)

- `main.rs` — argv[0] dispatch (`omarchy-recipe-export` etc. symlinks work),
  then clap dispatch. Note: clap errors (`--help`, bad flags) go through
  `e.exit()` so exit codes stay correct (0/2), never through our `die()`.
- `cli.rs` — clap definitions. Global `--verbose`. Export blacklist
  `--without` sections: `packages,aur,themes,font,plugins,services,webapps,backgrounds,icons`.
  `build-iso`'s `bundle` is **optional**: omitted → auto-export this machine
  to `/tmp/omr-auto-bundle-<pid>` (never with secrets) and bake that.
- `recipe.rs` — serde models + `resolve_bundle_dir` (explicit dir, else cwd
  must hold `recipe.json`) + `sha256_file`. `Selection` records
  `excluded_sections`, `extra_excludes`, `include_secrets`.
- `system.rs` — thin host introspection (commands, Omarchy checkout with
  dev-link awareness, pacman/AUR queries). Subprocess-based; no C deps
  (root check is a raw `geteuid` extern — keep it that way, don't add `libc`).
- `export.rs` — collection + `dotfiles.tar.zst` via system `tar`
  (deliberately not a Rust tar crate: identical flags/excludes to the old
  Bash implementation, no C toolchain needed). Also ships webapp `.desktop`
  files and icons in the tarball. Dynamic stage totals (4 or 5) so no
  phantom skipped numbers.
- `import.rs` — validate → packages → dotfiles (backup `~/.config` first,
  never clobber target's Hypr monitor config) → themes/font/plugins →
  allowlisted services only (never blindly mirror service state) → reload.
  `--first-boot` runs as root for another user (`runuser`, user-service
  symlinks instead of requiring a running user manager). Wifi AUR retries
  with `OMARCHY_RECIPE_NET_RETRIES` override for tests.
- `aur.rs` — vendoring: reuse `--aur-pkgdir` prebuilts, else `yay -G` +
  `makepkg -sf` (refuses as root — makepkg rule, surface the message), then
  `repo-add`. First boot installs vendored sets via `pacman -U` (fully
  offline; deps are assumed from the base install — exotic chains log
  failures instead of breaking the boot).
- `validate.rs` — schema + sha256. Shared by export/import/build-iso.
- `iso.rs` — the ISO layer generator (see §4).
- `output.rs` — TUI: quiet flag, numbered `stage()`, `summary()` box,
  `spinner` module (see §5). Colors honor `NO_COLOR`/dumb/non-tty.
  `OMARCHY_RECIPE_FORCE_COLOR` forces color (used in tests/smoke).

## 4. ISO integration (how build-iso works, and why)

Studied against upstream `quattro`; these are load-bearing facts:

- `builder/build-iso.sh` runs **inside** a privileged `archlinux` container:
  `/configs` = repo `configs/`, `/builder` = repo `builder/`,
  `/out` = host `release/`. The offline mirror is built by `pacman -Syw`
  into `airootfs/var/cache/omarchy/mirror/offline/` + `repo-add`.
- `_runtime_package_list()` (orchestrator) installs the ISO-bundled
  `omarchy-base.packages` copy onto the target — and `build-iso.sh`
  **overwrites** that copy at build time, so pre-seeding it in the clone
  gets clobbered. Hence our two anchor patches: (1) append
  `configs/recipe-packages.extra` to the shipped base-list copy (target
  install), (2) append it to `all_packages` before `mkdir -p /tmp/offlinedb`
  (mirror fetch).
- Payload ships on the live ISO at
  `/usr/share/omarchy-iso/recipe/` (`recipe.json`, `dotfiles.tar.zst`,
  `aur-pkgs/`, this binary, `omarchy-apply-recipe.service`). `profiledef.sh`
  `file_permissions` lint only covers `usr/local/bin` + `root`, so
  `usr/share/...` needs no lint entries — keep payload there.
- `stage_recipe` (appended to the orchestrator's `phases_impl.py`, registered
  in `main.py` after `configure_tailscale`) copies the payload to
  `/var/lib/omarchy/recipe/` on target, installs the binary to
  `/usr/local/bin`, and enables the unit via `arch-chroot systemctl enable`
  (same pattern as the tailscale join unit). No-op when no payload dir
  exists. The unit is `Type=simple` (never holds boot hostage),
  `After=network-online.target omarchy-provision-owner.service`, retries
  across boots, disables itself on success.
- **Patching rules:** `patch_after_anchor()` fails loudly on missing anchors
  (upstream drift) and refuses double-application via `# omarchy-recipe:`
  markers. Never weaken this to fuzzy matching.
- **archinstall drift compat** (`compat_sanity_check`): the build container
  always pulls latest archinstall; the checkout targeted 4.4 whose
  `sanity_check(offline=True)` TypeErrors on ≥4.5 (this killed a real user
  install). The shim strips the removed kwarg idempotently; behavior is
  preserved because the offline mirror is `SigLevel=Never`. If upstream
  restructures the call, the shim no-ops — do not extend it speculatively.
- **Workdir placement matters:** `/tmp` is typically a ~4GB tmpfs; a bake
  needs ~25GB (mirror + work tree + ISO) and otherwise dies in `xorriso`
  hours in (this happened — see `../iso-build-failure-2026-10-07.log`).
  Default workdir is `~/.cache/omarchy-recipe/iso-build-<pid>`, and
  `check_free_space()` fails fast below 25GB with `--workdir` guidance.
- Real bakes need docker + sudo + network and were historically run by the
  user (agents lack docker access). `--dry-run` exercises everything up to
  docker and is the standard agent-side verification.
- After a real bake, `--boot ask|yes|no` (default `ask`) offers a QEMU test
  drive via the checkout's `bin/omarchy-iso-boot`. `ask` auto-declines when
  stdin isn't a tty. The launch path is covered by a hermetic test (stub
  boot script + mocked `qemu-system-x86_64` on PATH); the y/N prompt itself
  is intentionally untested (reads real stdin).

## 5. TUI / spinner system (`output::spinner`)

- `spin(label)` returns a RAII guard. TTYs get a braille animation on one
  stderr line; pipes/logs get `…` / `✓` lines. `Drop` reports success —
  **always call `.fail()` before returning `Err`**, or failures get a ✓.
- `set_detail()` = transient text (retry attempts). `advance()` = milestone:
  retires the previous detail as a permanent `✓` line (dupes suppressed).
  Bake phases use `advance()`; retries use `set_detail()`.
- Every full-line printer (`stage`, `info`, `ok`, `warn`, `error`,
  `summary`, verbose pump lines) erases the active spinner line first
  (`\r\x1b[K`); otherwise prompts/warnings glue onto the frame line.
- Nesting (export inside build-iso) degrades to silent guards via a
  thread-local depth counter — never two animations on one line.
- `pause()` clears the line while a child owns the terminal (verbose
  `docker pull`). `sudo -v` is pre-authorized on a clean line *before* any
  spinner/redirection exists, so its prompt is never swallowed by the log.
- `LINE_LOCK` serializes all stderr line writes. Tests run headless (no
  animation) — keep it that way; verify animation via `script(1)` pty
  captures, never screenshots.

## 6. Testing

- `cargo test` — unit suite: schema v1/v2, validator accept/tamper/version,
  AUR pkgdir reuse, patch insert/missing-anchor/double-apply, full patch set
  incl. generated **bash + python syntax checks**, compat shim, exclude-list
  matrix, spinner lifecycle/nesting/history, bake marker map, pump drain.
- `tests/roundtrip.sh` — mocked-HOME export→validate→import integration:
  blacklist behavior, secrets filter/flag/restore, v1 bundle import, monitor
  preservation, argv[0] dispatch. Requires a built debug binary + `jq`.
- Past flakes and their fixes (do not regress):
  - Shared temp dirs between parallel tests (`omr-compat-*`) — every test
    gets a unique fixture dir.
  - Process-global `QUIET` toggled by parallel tests — merged into one test.
- Release check: `cargo build --release && ./install.sh`. Note `cargo test`
  does **not** refresh `target/debug/omarchy-recipe` — rebuild debug
  explicitly before running `roundtrip.sh` or manual smoke after source
  changes (stale-binary confusion has bitten twice).

## 7. Conventions

- Errors are `Result<_, String>` with human sentences; `die()` only at the
  top level. No panics on user input paths (`unwrap` only in tests).
- New files under `assets/` embedded via `include_str!` when the ISO layer
  needs them.
- Commit messages: imperative, scope-prefixed (`build-iso: …`, `UX: …`).
  This repo uses the GitHub noreply identity locally — don't change global
  git config.
- Keep `README.md`'s command examples runnable; they double as manual smoke
  scripts.
- Don't add dependencies lightly: clap + serde/serde_json + sha2 cover
  everything; system `tar`/`git`/`pacman` stay subprocesses.
