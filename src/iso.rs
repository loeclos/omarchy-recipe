//! `build-iso`: bake a .recipe bundle into a custom Omarchy ISO.
//!
//! Strategy (verified against omacom-io/omarchy-iso `quattro`):
//! - recipe repo packages ride the existing offline mirror: they are appended
//!   to the ISO-bundled `omarchy-base.packages` copy (so the orchestrator's
//!   `_runtime_package_list` installs them on target) and to the mirror's
//!   `all_packages` (so `pacman -Syw` fetches them). Both via anchor patches.
//! - the recipe payload (recipe.json, dotfiles, vendored AUR, this binary, the
//!   first-boot unit) ships on the live ISO at
//!   `/usr/share/omarchy-iso/recipe/` and is staged onto the target by a new
//!   `stage_recipe` orchestrator phase; the unit runs this same binary as
//!   `import --first-boot` on the installed machine.

use std::path::{Path, PathBuf};

use crate::cli::AurMode;
use crate::output;
use crate::recipe::*;
use crate::system;

const ISO_REPO: &str = "https://github.com/omacom-io/omarchy-iso";

const STAGE_RECIPE_PY: &str = include_str!("../assets/stage_recipe.py");
const APPLY_UNIT: &str = include_str!("../assets/omarchy-apply-recipe.service");

pub struct BuildIsoOptions {
    pub bundle: Option<String>,
    pub iso_checkout: Option<String>,
    pub mirror: String,
    pub aur: Option<AurMode>,
    pub aur_pkgdir: Option<String>,
    pub without: std::collections::HashSet<String>,
    pub extra_excludes: Vec<String>,
    pub allow_secrets: bool,
    pub dry_run: bool,
    pub workdir: Option<String>,
    pub boot: crate::cli::BootWhen,
}

pub fn run(opts: BuildIsoOptions) -> Result<(), String> {
    if !["stable", "rc", "edge"].contains(&opts.mirror.as_str()) {
        return Err(format!(
            "unknown mirror '{}' (expected stable, rc or edge)",
            opts.mirror
        ));
    }
    let bundle = match opts.bundle.as_deref() {
        Some(b) => resolve_bundle_dir(Some(b))?,
        None => {
            output::info("No bundle given: exporting this machine on the spot");
            auto_export_bundle(
                opts.aur.unwrap_or(AurMode::Wifi),
                opts.aur_pkgdir.as_deref(),
                opts.without,
                opts.extra_excludes,
            )?
        }
    };
    output::stage(1, 5, "Validating bundle");
    let mut recipe = crate::validate::validate_bundle(&bundle)?;
    if recipe.selection.include_secrets && !opts.allow_secrets {
        return Err(
            "this bundle contains secrets; refusing to bake it into an ISO. \
             Re-export without --include-secrets, or pass --allow-secrets only for a trusted private ISO."
                .into(),
        );
    }

    // Resolve AUR mode: CLI override wins over the bundle.
    let aur_mode: String = match opts.aur {
        Some(AurMode::Vendored) => "vendored".into(),
        Some(AurMode::Wifi) => "wifi".into(),
        None => recipe.packages.aur_mode.clone(),
    };
    recipe.packages.aur_mode = aur_mode.clone();
    if aur_mode == "wifi" {
        // A CLI override must not leave a vendored manifest in the payload
        // without shipping the corresponding package files.
        recipe.aur_vendored.clear();
    }
    if aur_mode == "vendored" && !recipe.excluded("aur") && !recipe.packages.aur.is_empty() {
        if recipe.aur_vendored.is_empty() {
            output::step("Vendoring AUR packages for the offline ISO");
            recipe.aur_vendored =
                crate::aur::vendor(&recipe.packages.aur, &bundle, opts.aur_pkgdir.as_deref())?;
            output::info("Prepared vendored AUR manifest for the ISO payload");
        } else {
            output::info("Bundle already carries vendored AUR packages");
        }
    }

    let repo_pkgs: Vec<String> = if recipe.excluded("packages") {
        vec![]
    } else {
        recipe.packages.explicit_repo.clone()
    };

    // --- prepare checkout ---
    output::stage(2, 5, "Preparing omarchy-iso checkout");
    let workdir = match opts.workdir {
        Some(w) => PathBuf::from(w),
        None => default_workdir()?,
    };
    let workdir = absolute_path(workdir)?;
    if !opts.dry_run {
        check_free_space(&workdir)?;
    }
    let checkout = prepare_checkout(opts.iso_checkout.as_deref(), &workdir)?;
    // With --iso-checkout the checkout lives elsewhere, but --workdir is still
    // the log directory and may not exist yet.
    if !workdir.exists() {
        std::fs::create_dir_all(&workdir)
            .map_err(|e| format!("cannot create {}: {e}", workdir.display()))?;
    }
    let checkout_ref = system::run_in(&checkout, "git", &["rev-parse", "HEAD"])
        .ok()
        .filter(|r| r.status == 0)
        .map(|r| r.stdout.trim().to_string())
        .filter(|s| !s.is_empty());
    output::info(&format!(
        "Checkout: {}",
        output::path(&checkout.display().to_string())
    ));

    // --- payload ---
    output::stage(3, 5, "Staging recipe payload");
    let payload = checkout.join("configs/airootfs/usr/share/omarchy-iso/recipe");
    if payload.exists() {
        std::fs::remove_dir_all(&payload)
            .map_err(|e| format!("cannot clear stale payload dir: {e}"))?;
    }
    std::fs::create_dir_all(&payload).map_err(|e| format!("cannot create payload dir: {e}"))?;
    let tarball = tarball_path(&bundle, &recipe)?;
    // The live payload always uses one canonical filename, regardless of the
    // compression format selected by the exporter.
    recipe.dotfiles.file = "dotfiles.tar.zst".into();
    let recipe_json = serde_json::to_string_pretty(&recipe)
        .map_err(|e| format!("cannot serialize prepared recipe: {e}"))?;
    std::fs::write(payload.join("recipe.json"), recipe_json + "\n")
        .map_err(|e| format!("cannot stage recipe.json: {e}"))?;
    std::fs::copy(&tarball, payload.join("dotfiles.tar.zst"))
        .map_err(|e| format!("cannot stage dotfiles: {e}"))?;
    if aur_mode == "vendored" && !recipe.aur_vendored.is_empty() {
        let src_aur = bundle.join("aur-pkgs");
        if !src_aur.is_dir() {
            return Err(format!(
                "aur_mode=vendored but no vendored package directory at {}",
                src_aur.display()
            ));
        }
        copy_dir(&src_aur, &payload.join("aur-pkgs"))?;
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own binary: {e}"))?;
    std::fs::copy(&exe, payload.join("omarchy-recipe"))
        .map_err(|e| format!("cannot stage applier binary: {e}"))?;
    std::fs::write(payload.join("omarchy-apply-recipe.service"), APPLY_UNIT)
        .map_err(|e| format!("cannot stage unit: {e}"))?;

    // --- repo package list for the mirror ---
    let extra = recipe_packages_extra(&repo_pkgs)?;
    let extra_path = checkout.join("configs/recipe-packages.extra");
    std::fs::write(&extra_path, &extra)
        .map_err(|e| format!("cannot write recipe-packages.extra: {e}"))?;

    // --- patches ---
    output::stage(4, 5, "Applying custom-layer patches");
    apply_iso_patches(&checkout)?;

    output::ok(&format!(
        "Custom layer ready ({} repo pkgs, AUR mode: {aur_mode})",
        repo_pkgs.len()
    ));

    if opts.dry_run {
        output::info("Dry run: stopping before docker/mkarchiso.");
        output::summary(
            "Planned ISO",
            &[
                ("bundle", output::path(&bundle.display().to_string())),
                ("channel", opts.mirror.clone()),
                ("repo pkgs", output::num(repo_pkgs.len())),
                ("aur", aur_mode.clone()),
                ("checkout", output::path(&checkout.display().to_string())),
                (
                    "checkout ref",
                    checkout_ref.clone().unwrap_or_else(|| "unknown".into()),
                ),
            ],
        );
        let flag_hint = match opts.mirror.as_str() {
            "edge" => " --edge",
            "rc" => " --rc",
            _ => "",
        };
        output::info(&format!(
            "To build for real, run: OMARCHY_MIRROR={} {}/bin/omarchy-iso-make{}",
            opts.mirror,
            checkout.display(),
            flag_hint
        ));
        return Ok(());
    }

    // --- real build via omarchy-iso-make (needs docker + sudo + network) ---
    output::stage(5, 5, "Baking ISO (docker + mkarchiso)");
    for tool in ["docker", "sudo", "git"] {
        if !system::cmd_exists(tool) {
            return Err(format!("'{tool}' not found; required for ISO builds"));
        }
    }
    let log_path = workdir.join("iso-build.log");
    let pull_log_path = workdir.join("docker-pull.log");

    // 5a. Docker daemon probe. Informational only: omarchy-iso-make retries
    // with sudo, which may prompt interactively where our probe cannot.
    probe_docker();

    // 5b. Pre-authorize sudo while the terminal is clean. Without this, the
    // `sudo docker` fallback inside omarchy-iso-make would print its password
    // prompt into the redirected build log (invisible) or mid-spinner-line.
    ensure_docker_elevation();

    let bake_spin = output::spinner::spin("baking ISO");

    // 5c. Explicit base-image pull so the biggest download shows progress
    // instead of hiding inside the container run. Best effort: the build
    // retries (possibly elevated) on its own.
    let base_image = docker_base_image(&checkout);
    bake_spin.set_detail(&format!("pulling {base_image}"));
    if !pre_pull(&bake_spin, &pull_log_path, &base_image) {
        bake_spin.set_detail("base pull deferred — retrying inside build");
    }

    // 5d. The bake. Output always lands in the log (the watcher below needs
    // it); verbose mode additionally streams it live.
    bake_spin.advance(&format!("building (log: {})", log_path.display()));
    let build_started = std::time::SystemTime::now();
    let prepared = (|| -> Result<std::process::Child, String> {
        let log_file = std::fs::File::create(&log_path)
            .map_err(|e| format!("cannot create build log: {e}"))?;
        let mut cmd = std::process::Command::new(checkout.join("bin/omarchy-iso-make"));
        cmd.current_dir(&checkout)
            .arg("--keep-pkg-cache")
            .arg("--no-boot-offer")
            .env("OMARCHY_MIRROR", &opts.mirror)
            .stdout(
                log_file
                    .try_clone()
                    .map_err(|e| format!("log redirect failed: {e}"))?,
            )
            .stderr(log_file);
        match opts.mirror.as_str() {
            "edge" => {
                cmd.arg("--edge");
            }
            "rc" => {
                cmd.arg("--rc");
            }
            _ => {}
        }
        cmd.spawn()
            .map_err(|e| format!("cannot run omarchy-iso-make: {e}"))
    })();
    let mut child = match prepared {
        Ok(child) => child,
        Err(e) => {
            bake_spin.fail("cannot start ISO bake");
            return Err(e);
        }
    };
    let started = std::time::Instant::now();
    let status = match pump_log(&log_path, &bake_spin, &mut child) {
        Ok(status) => status,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            bake_spin.fail("ISO build monitoring failed");
            return Err(e);
        }
    };
    let elapsed = started.elapsed();
    if !status.success() {
        bake_spin.fail("bake failed");
        return Err(format!(
            "omarchy-iso-make failed after {}; see {}",
            fmt_duration(elapsed),
            log_path.display()
        ));
    }
    let (iso_path, iso_size) = match newest_iso(&checkout, build_started) {
        Ok(result) => result,
        Err(e) => {
            bake_spin.fail("ISO output missing");
            return Err(e);
        }
    };
    bake_spin.succeed(Some(&format!("ISO baked in {}", fmt_duration(elapsed))));
    let iso_name = iso_path.display().to_string();
    let mut rows = vec![
        ("file", output::path(&iso_name)),
        ("size", iso_size),
        ("channel", opts.mirror.clone()),
        ("recipe pkgs", output::num(repo_pkgs.len())),
        ("aur", format!("{aur_mode} (first boot)")),
        ("bundle", output::path(&bundle.display().to_string())),
        (
            "checkout ref",
            checkout_ref.unwrap_or_else(|| "unknown".into()),
        ),
        ("build time", fmt_duration(elapsed)),
    ];
    if output::is_quiet() {
        rows.push(("build log", output::path(&log_path.display().to_string())));
    }
    output::summary("Your ISO", &rows);
    maybe_boot(opts.boot, &checkout, &iso_path);
    Ok(())
}

/// Offer (or auto-run / skip) a QEMU test drive of the baked ISO, using the
/// official `omarchy-iso-boot` helper from the same checkout.
fn maybe_boot(boot: crate::cli::BootWhen, checkout: &Path, iso_path: &Path) {
    use crate::cli::BootWhen;
    use std::io::IsTerminal as _;
    let interactive = std::io::stdin().is_terminal();
    let go = if matches!(boot, BootWhen::Ask) && !interactive {
        output::info("skipping QEMU boot (non-interactive; pass --boot yes/no)");
        false
    } else if matches!(boot, BootWhen::Ask) {
        prompt_boot()
    } else {
        should_boot(boot, interactive, false)
    };
    if !go {
        return;
    }
    if !iso_path.is_file() {
        output::warn("no ISO found in release/; skipping QEMU boot");
        return;
    }
    let tool = "qemu-system-x86_64";
    if !system::cmd_exists(tool) {
        output::warn(&format!("'{tool}' not found; cannot boot the ISO in QEMU"));
        return;
    }
    if !Path::new("/dev/kvm").exists() {
        output::warn("/dev/kvm missing: QEMU will fall back to (slow) software emulation");
    }
    let script = checkout.join("bin/omarchy-iso-boot");
    output::step(&format!("Booting {} in QEMU", iso_path.display()));
    if let Err(e) = launch_boot(&script, iso_path) {
        output::warn(&format!("cannot run omarchy-iso-boot: {e}"));
    } else {
        output::ok("QEMU exited");
    }
}

fn launch_boot(script: &Path, iso_path: &Path) -> Result<(), String> {
    let status = std::process::Command::new(script)
        .arg(iso_path)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("omarchy-iso-boot exited with {status}"))
    }
}

/// Pure decision helper (prompt I/O lives in `prompt_boot`).
fn should_boot(mode: crate::cli::BootWhen, interactive: bool, answer_yes: bool) -> bool {
    use crate::cli::BootWhen;
    match mode {
        BootWhen::Yes => true,
        BootWhen::No => false,
        BootWhen::Ask => interactive && answer_yes,
    }
}

/// Yes/no prompt on a clean terminal line. Default is No.
fn prompt_boot() -> bool {
    match output::prompt("Boot this ISO in QEMU now? [y/N] ") {
        Ok(ans) => matches!(ans.trim().to_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

fn fmt_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{}h {:02}m {:02}s", s / 3600, (s % 3600) / 60, s % 60)
}

/// Newest *.iso under <checkout>/release, with human size.
fn newest_iso(
    checkout: &Path,
    build_started: std::time::SystemTime,
) -> Result<(PathBuf, String), String> {
    let release = checkout.join("release");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(&release) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "iso").unwrap_or(false) {
                let m = e.metadata().and_then(|m| m.modified()).ok();
                match (m, &best) {
                    (Some(t), Some((bt, _))) if t <= *bt => {}
                    (None, Some(_)) => {}
                    _ => best = Some((m.unwrap_or(std::time::UNIX_EPOCH), p)),
                }
            }
        }
    }
    match best {
        Some((modified, p)) if modified >= build_started => {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            Ok((p, human_size(size)))
        }
        _ => Err(format!(
            "omarchy-iso-make exited successfully but produced no new ISO in {}",
            release.display()
        )),
    }
}

fn human_size(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1}{}", UNITS[u])
    }
}

/// Docker daemon probe. Informational only — omarchy-iso-make retries with
/// sudo, which may prompt interactively where this probe cannot.
fn probe_docker() {
    if system::run("docker", &["version"])
        .map(|r| r.status == 0)
        .unwrap_or(false)
    {
        output::info("docker daemon reachable");
        return;
    }
    if system::run("sudo", &["-n", "docker", "version"])
        .map(|r| r.status == 0)
        .unwrap_or(false)
    {
        output::info("docker needs sudo (omarchy-iso-make handles it)");
        return;
    }
    output::warn("docker daemon not reachable as you; the build will try sudo and may prompt");
}

/// Explicit base-image pull so the biggest download gets its own progress
/// instead of hiding inside the container run. Best effort: returns false
/// when the pull failed (the build retries, possibly elevated).
fn docker_base_image(checkout: &Path) -> String {
    const DEFAULT: &str = "archlinux/archlinux:latest";
    // Upstream pins the actual image in its build wrapper; prefer that over a
    // Dockerfile default so the progress pull matches the real bake.
    if let Ok(text) = std::fs::read_to_string(checkout.join("bin/omarchy-iso-make")) {
        for line in text.lines() {
            let line = line.trim();
            let value = line
                .strip_prefix("CONTAINER_IMAGE=")
                .or_else(|| line.strip_prefix("readonly CONTAINER_IMAGE="))
                .or_else(|| line.strip_prefix("export CONTAINER_IMAGE="));
            if let Some(value) = value {
                let image = value.trim().trim_matches(['"', '\'']);
                if !image.is_empty() && !image.contains('$') {
                    return image.to_string();
                }
            }
        }
    }
    for rel in [
        "Dockerfile",
        "builder/Dockerfile",
        "builder/docker/Dockerfile",
    ] {
        let Ok(text) = std::fs::read_to_string(checkout.join(rel)) else {
            continue;
        };
        for line in text.lines() {
            let Some(rest) = line.trim().strip_prefix("FROM ") else {
                continue;
            };
            let image = rest.split_whitespace().next().unwrap_or("");
            if !image.is_empty() && !image.contains('$') {
                return image.to_string();
            }
        }
    }
    DEFAULT.to_string()
}

fn pre_pull(spin: &output::spinner::Spinner, log_path: &Path, image: &str) -> bool {
    if output::is_quiet() {
        let log = match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(log_path)
        {
            Ok(f) => f,
            Err(e) => {
                output::warn(&format!("cannot open build log: {e}"));
                return false;
            }
        };
        let err_log = match log.try_clone() {
            Ok(f) => f,
            Err(e) => {
                output::warn(&format!("log redirect failed: {e}"));
                return false;
            }
        };
        let r = std::process::Command::new("docker")
            .args(["pull", image])
            .stdout(log)
            .stderr(err_log)
            .status();
        match r {
            Ok(s) if s.success() => {
                output::info("base image ready");
                true
            }
            _ => {
                output::warn("base image pull failed; continuing (the build retries)");
                false
            }
        }
    } else {
        let _paused = spin.pause();
        let r = std::process::Command::new("docker")
            .args(["pull", image])
            .status();
        match r {
            Ok(s) if s.success() => true,
            _ => {
                output::warn("base image pull failed; continuing (the build retries)");
                false
            }
        }
    }
}

/// Pre-authorize sudo on a clean terminal line, before any spinner or log
/// redirection is active. Otherwise the `sudo docker` fallback would print
/// its password prompt into the build log (invisible) or mid-animation.
fn ensure_docker_elevation() {
    if system::run("docker", &["version"])
        .map(|r| r.status == 0)
        .unwrap_or(false)
    {
        return;
    }
    output::info("docker needs elevation — one password prompt, then the bake runs unattended");
    // Inherited stdio on purpose: sudo must talk to the real terminal.
    match std::process::Command::new("sudo").arg("-v").status() {
        Ok(s) if s.success() => output::info("sudo authorized"),
        _ => output::warn(
            "could not pre-authorize sudo; if the build stalls, it is waiting on a hidden password prompt",
        ),
    }
}

/// Map a build-log line to spinner detail text.
fn bake_detail(line: &str) -> Option<&'static str> {
    if line.contains("Cloning into") {
        Some("fetching archiso sources")
    } else if line.contains("full system upgrade") {
        Some("upgrading build container")
    } else if line.contains("Synchronizing package databases") {
        Some("syncing package databases")
    } else if line.contains("resolving dependencies") {
        Some("resolving dependencies")
    } else if line.contains("Target install resolves to") {
        Some("offline mirror ready")
    } else if line.contains("Parallel mksquashfs") || line.contains("Creating SquashFS") {
        Some("building live filesystem (slow)")
    } else if line.contains("Creating checksum") {
        Some("writing checksums")
    } else if line.contains("Creating ISO image") {
        Some("writing ISO image")
    } else if line.contains("[mkarchiso] INFO: Done!") {
        Some("finalizing")
    } else if line.contains("ERROR") || line.contains("FAILURE") {
        Some("failed — see build log")
    } else {
        None
    }
}

/// Pump the build log while the child runs: stream lines live in verbose
/// mode and advance the spinner detail on recognized markers.
fn pump_log(
    log_path: &Path,
    spin: &output::spinner::Spinner,
    child: &mut std::process::Child,
) -> Result<std::process::ExitStatus, String> {
    use std::io::{Read, Seek};

    let verbose = !output::is_quiet();
    let mut offset: u64 = 0;
    let mut pending = String::new();
    let emit = |line: &str| {
        if verbose && !line.trim().is_empty() {
            output::print_line(line);
        }
        if let Some(detail) = bake_detail(line) {
            spin.advance(detail);
        }
    };

    loop {
        if let Ok(mut f) = std::fs::File::open(log_path) {
            if f.seek(std::io::SeekFrom::Start(offset)).is_ok() {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match f.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            offset += n as u64;
                            pending.push_str(&String::from_utf8_lossy(&buf[..n]));
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        drain_log_lines(&mut pending, &emit);
        match child.try_wait() {
            Err(e) => return Err(format!("bake child error: {e}")),
            Ok(Some(status)) => {
                // Final drain: lines written between last poll and exit.
                if let Ok(mut f) = std::fs::File::open(log_path) {
                    if f.seek(std::io::SeekFrom::Start(offset)).is_ok() {
                        let mut buf = vec![0u8; 64 * 1024];
                        loop {
                            match f.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => pending.push_str(&String::from_utf8_lossy(&buf[..n])),
                                Err(_) => break,
                            }
                        }
                    }
                }
                drain_log_lines(&mut pending, &emit);
                if !pending.trim().is_empty() {
                    emit(pending.trim_end());
                }
                return Ok(status);
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

/// Docker and mkarchiso progress often uses carriage returns rather than
/// newlines. Treat both as line boundaries and cap partial lines so a chatty
/// child cannot grow this buffer without bound.
fn drain_log_lines(pending: &mut String, mut emit: impl FnMut(&str)) {
    loop {
        let pos = pending.find(['\n', '\r']);
        match pos {
            Some(pos) => {
                let line: String = pending.drain(..=pos).collect();
                emit(line.trim_end());
            }
            None if pending.len() > 1024 * 1024 => {
                let line = std::mem::take(pending);
                emit(line.trim_end());
            }
            None => return,
        }
    }
}

/// Snapshot the current machine into a fresh temp bundle (no-bundle mode).
/// Honors the build's AUR mode; secrets are never auto-included.
fn auto_export_bundle(
    aur_mode: AurMode,
    aur_pkgdir: Option<&str>,
    without: std::collections::HashSet<String>,
    extra_excludes: Vec<String>,
) -> Result<PathBuf, String> {
    let base = system::home_dir()
        .map(|home| home.join(".cache/omarchy-recipe/auto-bundles"))
        .unwrap_or_else(std::env::temp_dir);
    let dir = system::unique_dir(&base, "omr-auto-bundle")?;
    crate::export::run(crate::export::ExportOptions {
        out: Some(dir.to_string_lossy().to_string()),
        without,
        extra_excludes,
        aur_mode,
        aur_pkgdir: aur_pkgdir.map(|s| s.to_string()),
        include_secrets: false,
    })?;
    output::info(&format!(
        "Auto-exported bundle kept at {}",
        output::path(&dir.display().to_string())
    ));
    Ok(dir)
}

fn absolute_path(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| format!("cannot get current directory: {e}"))
    }
}

/// Default build root on persistent storage. /tmp is often a small tmpfs that
/// cannot hold the multi-GB mirror + mkarchiso work tree + final ISO.
fn default_workdir() -> Result<PathBuf, String> {
    let base = system::home_dir()
        .map(|h| h.join(".cache/omarchy-recipe"))
        .unwrap_or_else(std::env::temp_dir);
    Ok(base.join(format!("iso-build-{}", std::process::id())))
}

/// Minimum free bytes on the workdir filesystem: offline mirror + work tree +
/// final ISO + headroom. Fails fast instead of dying in xorriso hours later.
const MIN_FREE_BYTES: u64 = 25 * 1024 * 1024 * 1024;

fn check_free_space(workdir: &Path) -> Result<(), String> {
    // Measure the nearest existing ancestor; prepare_checkout creates the
    // requested directory later. Never silently skip the guard.
    let anchor = workdir
        .ancestors()
        .find(|path| path.is_dir())
        .ok_or_else(|| format!("cannot measure free space for {}", workdir.display()))?;
    let r = system::run(
        "df",
        &["-B1", "--output=avail", "--", &anchor.to_string_lossy()],
    )?;
    if r.status != 0 {
        return Err(format!(
            "df failed for {}: {}",
            anchor.display(),
            r.stderr.trim()
        ));
    }
    let avail: Option<u64> = r.stdout.lines().skip(1).find_map(|l| l.trim().parse().ok());
    match avail {
        Some(bytes) if space_ok(bytes) => Ok(()),
        Some(bytes) => Err(format!(
            "only {} free on {} — ISO builds need {}+ (mirror + work tree + ISO). \
             Pass --workdir on a bigger filesystem (NOT /tmp: it is usually a small tmpfs).",
            human_size(bytes),
            anchor.display(),
            human_size(MIN_FREE_BYTES)
        )),
        None => Err(format!(
            "cannot parse free space from df for {}",
            anchor.display()
        )),
    }
}

fn space_ok(bytes: u64) -> bool {
    bytes >= MIN_FREE_BYTES
}

fn prepare_checkout(iso_checkout: Option<&str>, workdir: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = iso_checkout {
        let p = absolute_path(PathBuf::from(dir))?;
        if !p.join("builder/build-iso.sh").is_file() {
            return Err(format!("{} is not an omarchy-iso checkout", p.display()));
        }
        return Ok(p);
    }
    if workdir.exists() {
        return Err(format!(
            "{} exists; pass --workdir elsewhere or reuse with --iso-checkout",
            workdir.display()
        ));
    }
    if !system::cmd_exists("git") {
        return Err("git not found; needed to clone omarchy-iso".into());
    }
    output::info(&format!("Cloning {ISO_REPO} ..."));
    if let Some(parent) = workdir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let clone_spin = output::spinner::spin("cloning omarchy-iso");
    let r = match system::run(
        "git",
        &[
            "clone",
            "--depth",
            "1",
            ISO_REPO,
            &workdir.to_string_lossy(),
        ],
    ) {
        Ok(r) => r,
        Err(e) => {
            clone_spin.fail("cannot start omarchy-iso clone");
            return Err(e);
        }
    };
    if r.status != 0 {
        clone_spin.fail("clone failed");
        return Err(format!("clone failed: {}", r.stderr.trim()));
    }
    clone_spin.succeed(None);
    Ok(workdir.to_path_buf())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("cannot create {}: {e}", dst.display()))?;
    let r = system::run(
        "cp",
        &[
            "-a",
            &format!("{}/.", src.display()),
            &dst.to_string_lossy(),
        ],
    )?;
    if r.status != 0 {
        return Err(format!("copy failed: {}", r.stderr.trim()));
    }
    Ok(())
}

/// Insert `addition` after the line containing `anchor` in `path`.
/// Fails if the anchor is missing (upstream drift) or already applied.
fn patch_after_anchor(
    path: &Path,
    marker: &str,
    anchor: &str,
    addition: &str,
) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if text.contains(marker) {
        return Err(format!(
            "{} already patched (marker present); refusing to double-apply",
            path.display()
        ));
    }
    let matches = text.match_indices(anchor).count();
    if matches == 0 {
        return Err(format!(
            "anchor not found in {}: '{anchor}' — upstream omarchy-iso drifted; refusing to patch",
            path.display()
        ));
    }
    if matches > 1 {
        return Err(format!(
            "anchor is ambiguous in {} ({} matches): '{anchor}' — refusing to patch the wrong site",
            path.display(),
            matches
        ));
    }
    let idx = text.find(anchor).ok_or_else(|| {
        format!(
            "anchor not found in {}: '{anchor}' — upstream omarchy-iso drifted; refusing to patch",
            path.display()
        )
    })?;
    let line_end = text[idx..]
        .find('\n')
        .map(|i| idx + i + 1)
        .unwrap_or(text.len());
    let mut patched = String::with_capacity(text.len() + addition.len());
    patched.push_str(&text[..line_end]);
    patched.push_str(addition);
    patched.push_str(&text[line_end..]);
    std::fs::write(path, patched).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(())
}

fn recipe_packages_extra(repo_pkgs: &[String]) -> Result<String, String> {
    if repo_pkgs.is_empty() {
        // Keep the file truly empty: the upstream patch uses [[ -s ]] as its
        // guard, and a header-only file would trigger grep's no-match exit.
        return Ok(String::new());
    }
    let mut extra = String::from("# Generated by omarchy-recipe build-iso. Do not edit.\n");
    for p in repo_pkgs {
        if !crate::aur::valid_pkg_name(p) {
            return Err(format!("invalid repository package name: '{p}'"));
        }
        extra.push_str(p);
        extra.push('\n');
    }
    Ok(extra)
}

fn apply_iso_patches(checkout: &Path) -> Result<(), String> {
    let orch = checkout.join("configs/airootfs/usr/share/omarchy-iso/orchestrator");
    let targets = [
        checkout.join("builder/build-iso.sh"),
        orch.join("main.py"),
        orch.join("phases_impl.py"),
    ];
    let originals: Vec<(PathBuf, String)> = targets
        .iter()
        .map(|path| {
            std::fs::read_to_string(path)
                .map(|text| (path.clone(), text))
                .map_err(|e| format!("cannot read {}: {e}", path.display()))
        })
        .collect::<Result<_, _>>()?;
    let result = (|| {
        patch_build_script(checkout)?;
        patch_orchestrator(checkout)
    })();
    if let Err(error) = result {
        // A missing later anchor must not leave an earlier patch applied in a
        // caller-supplied checkout, making the next attempt refuse to run.
        for (path, text) in originals {
            let _ = std::fs::write(path, text);
        }
        return Err(error);
    }
    Ok(())
}

fn patch_build_script(checkout: &Path) -> Result<(), String> {
    let script = checkout.join("builder/build-iso.sh");
    // 1. Ship recipe repo packages on the ISO so _runtime_package_list installs them.
    patch_after_anchor(
        &script,
        "# omarchy-recipe: ship recipe packages",
        r#"cp "${base_pkg_lists[1]}" "$build_cache_dir/airootfs/usr/share/omarchy-iso/omarchy-other.packages""#,
        "\n# omarchy-recipe: ship recipe packages on target via _runtime_package_list.\n\
         if [[ -s /configs/recipe-packages.extra ]]; then\n\
         \x20 grep -hv '^#\\|^$' /configs/recipe-packages.extra >> \"$build_cache_dir/airootfs/usr/share/omarchy-iso/omarchy-base.packages\" || true\n\
         fi\n",
    )?;
    // 2. Fetch them into the offline mirror.
    patch_after_anchor(
        &script,
        "# omarchy-recipe: mirror recipe packages",
        "mkdir -p /tmp/offlinedb",
        "# omarchy-recipe: mirror recipe packages so pacstrap finds them offline.\n\
         if [[ -s /configs/recipe-packages.extra ]]; then\n\
         \x20 mapfile -t recipe_extra_pkgs < <(grep -hv '^#\\|^$' /configs/recipe-packages.extra | sort -u)\n\
         \x20 if (( ${#recipe_extra_pkgs[@]} )); then\n\
         \x20   all_packages+=(\"${recipe_extra_pkgs[@]}\")\n\
         \x20   mapfile -t all_packages < <(printf '%s\\n' \"${all_packages[@]}\" | sort -u)\n\
         \x20 fi\n\
         fi\n",
    )?;
    Ok(())
}

fn patch_orchestrator(checkout: &Path) -> Result<(), String> {
    let orch = checkout.join("configs/airootfs/usr/share/omarchy-iso/orchestrator");
    // 3. Register the stage_recipe phase after Tailscale config.
    patch_after_anchor(
        &orch.join("main.py"),
        "stage_recipe,",
        "        configure_tailscale,",
        "        stage_recipe,\n",
    )?;
    patch_after_anchor(
        &orch.join("main.py"),
        "Applying machine recipe",
        r#"        ("Configuring Tailscale",      configure_tailscale),"#,
        "        (\"Applying machine recipe\",   stage_recipe),\n",
    )?;
    // 4. Append the stage_recipe implementation.
    let impl_path = orch.join("phases_impl.py");
    let text = std::fs::read_to_string(&impl_path)
        .map_err(|e| format!("cannot read phases_impl.py: {e}"))?;
    if !text.contains("def stage_recipe(") {
        let mut new = text;
        if !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str("\n\n");
        new.push_str(STAGE_RECIPE_PY);
        std::fs::write(&impl_path, new).map_err(|e| format!("cannot patch phases_impl.py: {e}"))?;
    }
    // 5. archinstall >= 4.5 compat: the orchestrator's `sanity_check(offline=True)`
    //    is a 4.4-ism and TypeErrors on current Arch (the build container always
    //    pulls latest archinstall). Idempotent: no-op when already stripped or
    //    when upstream restructured the call.
    compat_sanity_check(&impl_path)?;
    Ok(())
}

/// Strip the removed `offline` kwarg from the orchestrator's sanity_check call.
/// The ISO's offline mirror is SigLevel=Never, so no keyring wait was ever
/// needed; skip_ntp/skip_wkd already cover the remaining waits.
///
/// Scoped to `sanity_check(...)` calls only: other `offline=True` uses (e.g.
/// `arch.make_mirror_handler(offline=True)`) are legitimate and must be left
/// alone. Upstream already migrated to `arch.sanity_check(installer)` with a
/// version-conditional adapter; that shape is a no-op here.
fn compat_sanity_check(impl_path: &Path) -> Result<(), String> {
    const OLD: &str = "        installer.sanity_check(\n            offline=True,\n            skip_ntp=True,\n            skip_wkd=True,\n        )";
    const NEW: &str = "        # omarchy-recipe compat: archinstall >= 4.5 dropped the `offline` kwarg.\n        installer.sanity_check(\n            skip_ntp=True,\n            skip_wkd=True,\n        )";
    let text = std::fs::read_to_string(impl_path)
        .map_err(|e| format!("cannot read {}: {e}", impl_path.display()))?;
    if text.contains(NEW) && find_offending_sanity_check(&text)?.is_none() {
        return Ok(());
    }
    // Fast path for the known 4.4 shape (keeps the compat marker for traceability).
    let mut patched = if text.contains(OLD) {
        text.replacen(OLD, NEW, 1)
    } else {
        text.clone()
    };
    // Generic pass: strip `offline=True` from any remaining sanity_check call,
    // whatever its formatting (single-line, reordered, extra spaces).
    patched = strip_offline_from_sanity_checks(patched)?;
    if find_offending_sanity_check(&patched)?.is_some() {
        return Err(
            "archinstall sanity_check still passes offline=True in an unrecognized shape; \
             upstream omarchy-iso drifted and the compatibility shim cannot safely patch it"
                .into(),
        );
    }
    if patched != text {
        std::fs::write(impl_path, patched)
            .map_err(|e| format!("cannot write {}: {e}", impl_path.display()))?;
    }
    Ok(())
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_py_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Locate `offline = True` (any spacing) inside an argument list.
/// Returns byte offsets relative to `args`, or None.
fn find_offline_kwarg(args: &str) -> Option<(usize, usize)> {
    let bytes = args.as_bytes();
    let mut i = 0;
    while i + "offline".len() <= bytes.len() {
        let Some(off) = args[i..].find("offline") else {
            break;
        };
        let s = i + off;
        let e = s + "offline".len();
        let before_ok = s == 0 || !is_ident_byte(bytes[s - 1]);
        let after_ok = e >= bytes.len() || !is_ident_byte(bytes[e]);
        if before_ok && after_ok {
            let mut j = e;
            while j < bytes.len() && is_py_space(bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                // Skip `==` comparisons; we only match a kwarg assignment.
                if j + 1 < bytes.len() && bytes[j + 1] == b'=' {
                    i = e;
                    continue;
                }
                j += 1;
                while j < bytes.len() && is_py_space(bytes[j]) {
                    j += 1;
                }
                if args[j..].starts_with("True") {
                    let te = j + "True".len();
                    if te >= bytes.len() || !is_ident_byte(bytes[te]) {
                        return Some((s, te));
                    }
                }
            }
        }
        i = e;
    }
    None
}

/// Find the first `sanity_check(...)` call that still passes a literal
/// `offline=True`. Returns (open paren, close paren, kwarg start, kwarg end)
/// as absolute byte offsets. Skips `def sanity_check` definitions and calls
/// without the kwarg (e.g. the upstream `arch.sanity_check(installer)`
/// wrapper or `sanity_check(**options)`).
fn find_offending_sanity_check(text: &str) -> Result<Option<(usize, usize, usize, usize)>, String> {
    let bytes = text.as_bytes();
    let mut search_from = 0;
    while search_from < text.len() {
        let Some(rel) = text[search_from..].find("sanity_check") else {
            break;
        };
        let name_start = search_from + rel;
        let name_end = name_start + "sanity_check".len();
        // Skip `def sanity_check(` definitions.
        let line_start = text[..name_start].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let line_prefix = text[line_start..name_start].trim_end();
        if line_prefix == "def" || line_prefix.ends_with(" def") {
            search_from = name_end;
            continue;
        }
        // The call's opening paren (allowing whitespace/newlines).
        let mut k = name_end;
        while k < bytes.len() && is_py_space(bytes[k]) {
            k += 1;
        }
        if k >= bytes.len() || bytes[k] != b'(' {
            search_from = name_end;
            continue;
        }
        let open = k;
        // Match the closing paren, ignoring parens inside strings/comments.
        let mut depth = 0usize;
        let mut in_single = false;
        let mut in_double = false;
        let mut j = open;
        let mut close_opt: Option<usize> = None;
        while j < bytes.len() {
            let c = bytes[j];
            if in_single {
                if c == b'\\' {
                    j += 2;
                    continue;
                }
                if c == b'\'' {
                    in_single = false;
                }
                j += 1;
                continue;
            }
            if in_double {
                if c == b'\\' {
                    j += 2;
                    continue;
                }
                if c == b'"' {
                    in_double = false;
                }
                j += 1;
                continue;
            }
            match c {
                b'\'' => {
                    in_single = true;
                    j += 1;
                }
                b'"' => {
                    in_double = true;
                    j += 1;
                }
                b'#' => {
                    while j < bytes.len() && bytes[j] != b'\n' {
                        j += 1;
                    }
                }
                b'(' => {
                    depth += 1;
                    j += 1;
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        close_opt = Some(j);
                        break;
                    }
                    j += 1;
                }
                _ => {
                    j += 1;
                }
            }
        }
        let Some(close) = close_opt else {
            return Err(
                "archinstall sanity_check call has unbalanced parentheses; \
                 upstream omarchy-iso drifted and the compatibility shim cannot safely patch it"
                    .into(),
            );
        };
        let args = &text[open + 1..close];
        if let Some((rs, re)) = find_offline_kwarg(args) {
            return Ok(Some((open, close, open + 1 + rs, open + 1 + re)));
        }
        search_from = close + 1;
    }
    Ok(None)
}

/// Remove the `offline=True` kwarg from every `sanity_check(...)` call,
/// keeping the remaining arguments valid Python.
fn strip_offline_from_sanity_checks(mut text: String) -> Result<String, String> {
    loop {
        let found = find_offending_sanity_check(&text)?;
        let Some((open, close, ks, ke)) = found else {
            break;
        };
        let bytes = text.as_bytes().to_vec();
        // Comma after the kwarg (skipping whitespace)?
        let mut f = ke;
        while f < close && is_py_space(bytes[f]) {
            f += 1;
        }
        let has_after = f < close && bytes[f] == b',';
        // Comma before the kwarg (skipping whitespace)?
        let mut bb = ks;
        while bb > open + 1 && is_py_space(bytes[bb - 1]) {
            bb -= 1;
        }
        let has_before = bb > open + 1 && bytes[bb - 1] == b',';
        let (rm_start, rm_end) = if has_before {
            // `..., offline=True` -> `...` (keeps any trailing comma/space).
            (bb - 1, ke)
        } else if has_after {
            // `offline=True, ...` -> `...` (also eats the separator's whitespace).
            let mut e = f + 1;
            while e < close && is_py_space(bytes[e]) {
                e += 1;
            }
            (ks, e)
        } else {
            (ks, ke)
        };
        text.replace_range(rm_start..rm_end, "");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("omr-iso-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn patch_inserts_after_anchor() {
        let p = fixture("build.sh", "line1\nmkdir -p /tmp/offlinedb\nline3\n");
        patch_after_anchor(&p, "# omr", "mkdir -p /tmp/offlinedb", "# omr\nEXTRA=1\n").unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert_eq!(t, "line1\nmkdir -p /tmp/offlinedb\n# omr\nEXTRA=1\nline3\n");
    }

    #[test]
    fn patch_missing_anchor_fails() {
        let p = fixture("drift.sh", "nothing here\n");
        assert!(patch_after_anchor(&p, "# omr", "mkdir -p /tmp/offlinedb", "x").is_err());
    }

    #[test]
    fn patch_double_apply_refused() {
        let p = fixture("twice.sh", "a\nANCHOR\nb\n");
        patch_after_anchor(&p, "# omr", "ANCHOR", "# omr\nx\n").unwrap();
        assert!(patch_after_anchor(&p, "# omr", "ANCHOR", "# omr\nx\n").is_err());
    }

    #[test]
    fn full_patch_set_applies_to_realistic_files() {
        // Mirrors the real upstream layout (trimmed). If upstream drifts, the
        // live verify step (not this fixture) catches it.
        let dir = std::env::temp_dir().join("omr-iso-full");
        let _ = std::fs::remove_dir_all(&dir);
        let orch = dir.join("configs/airootfs/usr/share/omarchy-iso/orchestrator");
        std::fs::create_dir_all(&orch).unwrap();
        std::fs::write(dir.join("builder_placeholder"), "").unwrap();
        std::fs::create_dir_all(dir.join("builder")).unwrap();
        std::fs::write(
            dir.join("builder/build-iso.sh"),
            "cp \"${base_pkg_lists[0]}\" \"$build_cache_dir/airootfs/usr/share/omarchy-iso/omarchy-base.packages\"\n\
             cp \"${base_pkg_lists[1]}\" \"$build_cache_dir/airootfs/usr/share/omarchy-iso/omarchy-other.packages\"\n\
             setup_form_stuff\n\
             mkdir -p /tmp/offlinedb\n\
             download_stuff\n",
        )
        .unwrap();
        std::fs::write(
            orch.join("main.py"),
            "    from .phases_impl import (\n\
             \x20       configure_tailscale,\n\
             \x20       validate_boot,\n\
             \x20   )\n\
             \x20   return [\n\
             \x20       (\"Configuring Tailscale\",      configure_tailscale),\n\
             \x20       (\"Validating boot setup\",      validate_boot),\n\
             \x20   ]\n",
        )
        .unwrap();
        std::fs::write(orch.join("phases_impl.py"), "from pathlib import Path\n").unwrap();

        patch_build_script(&dir).unwrap();
        patch_orchestrator(&dir).unwrap();

        let build = std::fs::read_to_string(dir.join("builder/build-iso.sh")).unwrap();
        assert!(build.contains("# omarchy-recipe: ship recipe packages"));
        assert!(build.contains("# omarchy-recipe: mirror recipe packages"));
        assert_eq!(build.matches("mkdir -p /tmp/offlinedb").count(), 1);
        // Generated bash must parse in full, including the mirror patch.
        let build_path = dir.join("builder/build-iso.sh");
        std::fs::write(&build_path, &build).unwrap();
        let r = system::run("bash", &["-n", &build_path.to_string_lossy()]).unwrap();
        assert!(
            r.status == 0,
            "generated bash failed syntax check: {}",
            r.stderr
        );

        let main = std::fs::read_to_string(orch.join("main.py")).unwrap();
        assert!(main.contains("stage_recipe,"));
        assert!(main.contains("Applying machine recipe"));
        let impl_path = orch.join("phases_impl.py");
        let impl_py = std::fs::read_to_string(&impl_path).unwrap();
        assert!(impl_py.contains("def stage_recipe("));
        // Parse the generated Python file itself, not a JSON string literal.
        let r = system::run(
            "python3",
            &[
                "-c",
                "import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text())",
                &impl_path.to_string_lossy(),
            ],
        );
        let r = r.unwrap();
        assert!(
            r.status == 0,
            "generated python failed syntax check: {}",
            r.stderr
        );
    }

    #[test]
    fn compat_strips_offline_kwarg() {
        let dir = std::env::temp_dir().join("omr-compat-strip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("phases_impl.py");
        std::fs::write(
            &p,
            "x = 1\n        installer.sanity_check(\n            offline=True,\n            skip_ntp=True,\n            skip_wkd=True,\n        )\ny = 2\n",
        )
        .unwrap();
        compat_sanity_check(&p).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(!t.contains("offline=True"));
        assert!(t.contains("skip_ntp=True"));
        // Idempotent second run.
        compat_sanity_check(&p).unwrap();
        let t2 = std::fs::read_to_string(&p).unwrap();
        assert_eq!(t, t2);
    }

    #[test]
    fn compat_leaves_unknown_shapes_alone() {
        let dir = std::env::temp_dir().join("omr-compat-leave");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("other.py");
        std::fs::write(&p, "installer.sanity_check(foo=1)\n").unwrap();
        compat_sanity_check(&p).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "installer.sanity_check(foo=1)\n"
        );
    }

    #[test]
    fn space_threshold() {
        // 25 GiB floor: the failed bake needed ~6.5G for the ISO alone,
        // plus mirror + work tree.
        assert!(space_ok(25 * 1024 * 1024 * 1024));
        assert!(space_ok(200 * 1024 * 1024 * 1024));
        assert!(!space_ok(3_900_000_000)); // ~3.6G tmpfs that killed the bake
        assert!(!space_ok(0));
    }

    #[test]
    fn parse_avail_reads_df_output() {
        let out = "      Avail\n 27492389888\n";
        let avail: Option<u64> = out.lines().skip(1).find_map(|l| l.trim().parse().ok());
        assert_eq!(avail, Some(27492389888));
        assert!(avail.unwrap() >= MIN_FREE_BYTES);
        let small = "      Avail\n  3900000000\n";
        let avail2: Option<u64> = small.lines().skip(1).find_map(|l| l.trim().parse().ok());
        assert!(avail2.unwrap() < MIN_FREE_BYTES);
    }

    #[test]
    fn default_workdir_is_not_tmp() {
        let w = default_workdir().unwrap();
        assert!(!w.starts_with("/tmp"), "workdir must avoid tmpfs: {w:?}");
    }

    #[test]
    fn bake_detail_maps_phases() {
        assert_eq!(
            bake_detail("Cloning into 'archiso'..."),
            Some("fetching archiso sources")
        );
        assert_eq!(
            bake_detail("Target install resolves to 912 packages."),
            Some("offline mirror ready")
        );
        assert_eq!(
            bake_detail("[mkarchiso] INFO: Creating ISO image..."),
            Some("writing ISO image")
        );
        assert_eq!(bake_detail("[mkarchiso] INFO: Done!"), Some("finalizing"));
        assert_eq!(
            bake_detail("xorriso : FAILURE : blah"),
            Some("failed — see build log")
        );
        assert_eq!(bake_detail(":: downloading foo.pkg"), None);
    }

    #[test]
    fn pump_drains_prefilled_log() {
        let dir = std::env::temp_dir().join("omr-pump");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("bake.log");
        std::fs::write(
            &log,
            "Cloning into x\nTarget install resolves to 900 packages.\n",
        )
        .unwrap();
        let sp = output::spinner::spin("pump");
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let status = pump_log(&log, &sp, &mut child).unwrap();
        assert!(status.success());
        sp.succeed(None);
    }

    #[test]
    fn empty_repo_list_produces_truly_empty_extra_file() {
        assert!(recipe_packages_extra(&[]).unwrap().is_empty());
        assert!(recipe_packages_extra(&["foo".into()])
            .unwrap()
            .contains("foo\n"));
        assert!(recipe_packages_extra(&["../evil".into()]).is_err());
        assert!(recipe_packages_extra(&["..".into()]).is_err());
        assert!(recipe_packages_extra(&["-foo".into()]).is_err());
    }

    #[test]
    fn ambiguous_anchor_is_rejected() {
        let dir = system::temp_dir("omr-iso-ambiguous").unwrap();
        let p = dir.join("script.sh");
        std::fs::write(&p, "ANCHOR\nANCHOR\n").unwrap();
        assert!(patch_after_anchor(&p, "# marker", "ANCHOR", "added\n").is_err());
    }

    #[test]
    fn compat_strips_single_line_offline_kwarg() {
        // Drifted single-line shape: strip it instead of failing.
        let dir = system::temp_dir("omr-compat-single").unwrap();
        let p = dir.join("phases_impl.py");
        std::fs::write(&p, "installer.sanity_check(offline=True, skip_ntp=True)\n").unwrap();
        compat_sanity_check(&p).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(!t.contains("offline"));
        assert!(t.contains("skip_ntp=True"));
        // Valid Python, idempotent.
        let r = system::run(
            "python3",
            &[
                "-c",
                "import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text())",
                &p.to_string_lossy(),
            ],
        )
        .unwrap();
        assert!(r.status == 0, "stripped call must parse: {}", r.stderr);
        compat_sanity_check(&p).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), t);
    }

    #[test]
    fn compat_ignores_offline_outside_sanity_check() {
        // The reported false positive: upstream migrated to
        // arch.sanity_check(installer) while make_mirror_handler keeps its
        // own legitimate offline=True. The shim must no-op.
        let dir = system::temp_dir("omr-compat-current").unwrap();
        let p = dir.join("phases_impl.py");
        let original = "        ctx.state[\"mirror_handler\"] = arch.make_mirror_handler(offline=True)\n\
             \x20       with _time_step(\"STEP.sanity_check\"):\n\
             \x20           arch.sanity_check(installer)\n";
        std::fs::write(&p, original).unwrap();
        compat_sanity_check(&p).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), original);
    }

    #[test]
    fn compat_strips_reordered_and_spaced_shapes() {
        let dir = system::temp_dir("omr-compat-shapes").unwrap();
        let p = dir.join("phases_impl.py");
        std::fs::write(
            &p,
            "installer.sanity_check(\n    skip_ntp=True,\n    offline = True,\n    skip_wkd=True,\n)\n",
        )
        .unwrap();
        compat_sanity_check(&p).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(!t.contains("offline"));
        assert!(t.contains("skip_ntp=True"));
        assert!(t.contains("skip_wkd=True"));
        let r = system::run(
            "python3",
            &[
                "-c",
                "import ast, pathlib, sys; ast.parse(pathlib.Path(sys.argv[1]).read_text())",
                &p.to_string_lossy(),
            ],
        )
        .unwrap();
        assert!(r.status == 0, "stripped call must parse: {}", r.stderr);
    }

    #[test]
    fn compat_fails_loudly_on_unparseable_sanity_check() {
        let dir = system::temp_dir("omr-compat-unknown").unwrap();
        let p = dir.join("phases_impl.py");
        std::fs::write(&p, "installer.sanity_check(offline=True, skip_ntp=True\n").unwrap();
        assert!(compat_sanity_check(&p).is_err());
    }

    #[test]
    fn missing_or_stale_iso_is_not_success() {
        let dir = system::temp_dir("omr-iso-output").unwrap();
        std::fs::create_dir_all(dir.join("release")).unwrap();
        let now = std::time::SystemTime::now();
        assert!(newest_iso(&dir, now).is_err());
        std::fs::write(dir.join("release/old.iso"), b"old").unwrap();
        let future = now + std::time::Duration::from_secs(60);
        assert!(newest_iso(&dir, future).is_err());
    }

    #[test]
    fn log_drain_handles_carriage_returns_and_bounds_partial_lines() {
        let mut pending = "one\rtwo\nthree".to_string();
        let mut lines = vec![];
        drain_log_lines(&mut pending, |line| lines.push(line.to_string()));
        assert_eq!(lines, vec!["one", "two"]);
        assert_eq!(pending, "three");

        pending.push_str(&"x".repeat(1024 * 1024 + 1));
        drain_log_lines(&mut pending, |line| lines.push(line.to_string()));
        assert!(pending.is_empty());
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn pinned_container_image_from_upstream_wrapper_wins() {
        let dir = system::temp_dir("omr-container-image").unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(
            dir.join("bin/omarchy-iso-make"),
            "#!/bin/bash\nCONTAINER_IMAGE=\"example/pinned@sha256:abc\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM fallback:latest\n").unwrap();
        assert_eq!(docker_base_image(&dir), "example/pinned@sha256:abc");
    }

    #[test]
    fn relative_workdir_is_resolved_against_cwd() {
        let resolved = absolute_path(PathBuf::from("build")).unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, std::env::current_dir().unwrap().join("build"));
    }

    #[test]
    fn patch_failure_rolls_back_earlier_files() {
        let dir = system::temp_dir("omr-iso-rollback").unwrap();
        let orch = dir.join("configs/airootfs/usr/share/omarchy-iso/orchestrator");
        std::fs::create_dir_all(dir.join("builder")).unwrap();
        std::fs::create_dir_all(&orch).unwrap();
        let build = dir.join("builder/build-iso.sh");
        let build_text = "cp \"${base_pkg_lists[1]}\" \"$build_cache_dir/airootfs/usr/share/omarchy-iso/omarchy-other.packages\"\nmkdir -p /tmp/offlinedb\n";
        std::fs::write(&build, build_text).unwrap();
        std::fs::write(orch.join("main.py"), "# no anchors\n").unwrap();
        std::fs::write(orch.join("phases_impl.py"), "# original\n").unwrap();
        assert!(apply_iso_patches(&dir).is_err());
        assert_eq!(std::fs::read_to_string(build).unwrap(), build_text);
    }

    #[test]
    fn boot_decision_matrix() {
        use crate::cli::BootWhen;
        assert!(should_boot(BootWhen::Yes, false, false));
        assert!(should_boot(BootWhen::Yes, true, false));
        assert!(!should_boot(BootWhen::No, true, true));
        assert!(!should_boot(BootWhen::No, false, false));
        assert!(should_boot(BootWhen::Ask, true, true));
        assert!(!should_boot(BootWhen::Ask, true, false));
        // Non-interactive ask always declines (no prompt possible).
        assert!(!should_boot(BootWhen::Ask, false, true));
    }
}

#[cfg(test)]
mod boot_launch_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn boot_invokes_helper_with_iso_path() {
        let dir = system::temp_dir("omr-boot-launch").unwrap();
        let bindir = dir.join("checkout/bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let sentinel = dir.join("booted.txt");
        std::fs::write(
            bindir.join("omarchy-iso-boot"),
            format!("#!/bin/sh\necho \"$1\" > {}\n", sentinel.display()),
        )
        .unwrap();
        std::fs::set_permissions(
            bindir.join("omarchy-iso-boot"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let iso = dir.join("test.iso");
        std::fs::write(&iso, b"fake").unwrap();

        launch_boot(&bindir.join("omarchy-iso-boot"), &iso).unwrap();
        let got = std::fs::read_to_string(&sentinel).unwrap();
        assert_eq!(got.trim(), iso.display().to_string());
    }
}
