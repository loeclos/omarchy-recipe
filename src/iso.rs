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
            auto_export_bundle(opts.aur.unwrap_or(AurMode::Wifi), opts.aur_pkgdir.as_deref())?
        }
    };
    output::stage(1, 5, "Validating bundle");
    let mut recipe = Recipe::load(&bundle.join("recipe.json"))?;
    crate::validate::validate_bundle(&bundle)?;

    // Resolve AUR mode: CLI override wins over the bundle.
    let aur_mode: String = match opts.aur {
        Some(AurMode::Vendored) => "vendored".into(),
        Some(AurMode::Wifi) => "wifi".into(),
        None => recipe.packages.aur_mode.clone(),
    };
    if aur_mode == "vendored" && !recipe.excluded("aur") && !recipe.packages.aur.is_empty() {
        if recipe.aur_vendored.is_empty() {
            output::step("Vendoring AUR packages for the offline ISO ...");
            let vendored =
                crate::aur::vendor(&recipe.packages.aur, &bundle, opts.aur_pkgdir.as_deref())?;
            recipe.packages.aur_mode = "vendored".into();
            recipe.aur_vendored = vendored;
            let json = serde_json::to_string_pretty(&recipe).map_err(|e| format!("JSON: {e}"))?;
            std::fs::write(bundle.join("recipe.json"), json + "\n")
                .map_err(|e| format!("cannot update bundle recipe.json: {e}"))?;
            // Tarball unchanged, but its recorded sha must still match: recompute not needed.
            output::info("Bundle recipe.json updated with vendored AUR manifest");
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
    check_free_space(&workdir)?;
    let checkout = prepare_checkout(opts.iso_checkout.as_deref(), &workdir)?;
    output::info(&format!("Checkout: {}", output::path(&checkout.display().to_string())));

    // --- payload ---
    output::stage(3, 5, "Staging recipe payload");
    let payload = checkout.join("configs/airootfs/usr/share/omarchy-iso/recipe");
    std::fs::create_dir_all(&payload)
        .map_err(|e| format!("cannot create payload dir: {e}"))?;
    std::fs::copy(bundle.join("recipe.json"), payload.join("recipe.json"))
        .map_err(|e| format!("cannot stage recipe.json: {e}"))?;
    let tarball = tarball_path(&bundle, &recipe);
    std::fs::copy(&tarball, payload.join("dotfiles.tar.zst"))
        .map_err(|e| format!("cannot stage dotfiles: {e}"))?;
    if aur_mode == "vendored" {
        let src_aur = bundle.join("aur-pkgs");
        if src_aur.is_dir() {
            copy_dir(&src_aur, &payload.join("aur-pkgs"))?;
        }
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own binary: {e}"))?;
    std::fs::copy(&exe, payload.join("omarchy-recipe"))
        .map_err(|e| format!("cannot stage applier binary: {e}"))?;
    std::fs::write(payload.join("omarchy-apply-recipe.service"), APPLY_UNIT)
        .map_err(|e| format!("cannot stage unit: {e}"))?;

    // --- repo package list for the mirror ---
    let mut extra = String::from("# Generated by omarchy-recipe build-iso. Do not edit.\n");
    for p in &repo_pkgs {
        extra.push_str(p);
        extra.push('\n');
    }
    std::fs::write(checkout.join("configs/recipe-packages.extra"), &extra)
        .map_err(|e| format!("cannot write recipe-packages.extra: {e}"))?;

    // --- patches ---
    output::stage(4, 5, "Applying custom-layer patches");
    patch_build_script(&checkout)?;
    patch_orchestrator(&checkout)?;

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
                ("channel", opts.mirror.clone().into()),
                ("repo pkgs", output::num(repo_pkgs.len()).into()),
                ("aur", aur_mode.clone().into()),
                ("checkout", output::path(&checkout.display().to_string())),
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
    bake_spin.set_detail("pulling archlinux/archlinux:latest");
    if !pre_pull(&bake_spin, &log_path) {
        bake_spin.set_detail("base pull deferred — retrying inside build");
    }

    // 5d. The bake. Output always lands in the log (the watcher below needs
    // it); verbose mode additionally streams it live.
    bake_spin.advance(&format!("building (log: {})", log_path.display()));
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
    let started = std::time::Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run omarchy-iso-make: {e}"))?;
    let status = pump_log(&log_path, &bake_spin, &mut child)?;
    let elapsed = started.elapsed();
    if !status.success() {
        bake_spin.fail("bake failed");
        return Err(format!(
            "omarchy-iso-make failed after {}; see {}",
            fmt_duration(elapsed),
            log_path.display()
        ));
    }
    bake_spin.succeed(Some(&format!("ISO baked in {}", fmt_duration(elapsed))));
    let (iso_path, iso_size) = newest_iso(&checkout);
    let iso_name = iso_path.display().to_string();
    let mut rows = vec![
        ("file", output::path(&iso_name).into()),
        ("size", iso_size.into()),
        ("channel", opts.mirror.clone().into()),
        ("recipe pkgs", output::num(repo_pkgs.len()).into()),
        ("aur", format!("{aur_mode} (first boot)").into()),
        (
            "bundle",
            output::path(&bundle.display().to_string()).into(),
        ),
        ("build time", fmt_duration(elapsed).into()),
    ];
    if output::is_quiet() {
        rows.push((
            "build log",
            output::path(&log_path.display().to_string()).into(),
        ));
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
    for tool in ["qemu-system-x86_64"] {
        if !system::cmd_exists(tool) {
            output::warn(&format!("'{tool}' not found; cannot boot the ISO in QEMU"));
            return;
        }
    }
    if !Path::new("/dev/kvm").exists() {
        output::warn("/dev/kvm missing: QEMU will fall back to (slow) software emulation");
    }
    let script = checkout.join("bin/omarchy-iso-boot");
    output::step(&format!("Booting {} in QEMU ...", iso_path.display()));
    match std::process::Command::new(&script).arg(iso_path).status() {
        Ok(s) if s.success() => output::ok("QEMU exited"),
        Ok(s) => output::warn(&format!("omarchy-iso-boot exited with {s}")),
        Err(e) => output::warn(&format!("cannot run omarchy-iso-boot: {e}")),
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
    use std::io::Write;
    output::spinner::clear_active();
    print!("Boot this ISO in QEMU now? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut ans = String::new();
    if std::io::stdin().read_line(&mut ans).is_err() {
        return false;
    }
    matches!(ans.trim().to_lowercase().as_str(), "y" | "yes")
}

fn fmt_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{}h {:02}m {:02}s", s / 3600, (s % 3600) / 60, s % 60)
}

/// Newest *.iso under <checkout>/release, with human size.
fn newest_iso(checkout: &Path) -> (PathBuf, String) {
    let release = checkout.join("release");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(&release) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "iso").unwrap_or(false) {
                let m = e.metadata().and_then(|m| m.modified()).ok();
                match (m, &best) {
                    (Some(t), Some((bt, _))) if t <= *bt => {}
                    _ => best = Some((m.unwrap_or(std::time::UNIX_EPOCH), p)),
                }
            }
        }
    }
    match best {
        Some((_, p)) => {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            (p, human_size(size))
        }
        None => (release.join("<not found>"), "—".into()),
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
    if system::run("docker", &["version"]).map(|r| r.status == 0).unwrap_or(false) {
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
fn pre_pull(spin: &output::spinner::Spinner, log_path: &Path) -> bool {
    if output::is_quiet() {
        let log = match std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
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
            .args(["pull", "archlinux/archlinux:latest"])
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
            .args(["pull", "archlinux/archlinux:latest"])
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
    if system::run("docker", &["version"]).map(|r| r.status == 0).unwrap_or(false) {
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
    use std::io::{Read, Seek, SeekFrom};
    let verbose = !output::is_quiet();
    let mut offset: u64 = 0;
    let mut pending = String::new();
    loop {
        if let Ok(mut f) = std::fs::File::open(log_path) {
            if f.seek(SeekFrom::Start(offset)).is_ok() {
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
        while let Some(pos) = pending.find('\n') {
            let line: String = pending.drain(..=pos).collect();
            let line = line.trim_end();
            if verbose && !line.trim().is_empty() {
                output::spinner::clear_active();
                println!("{line}");
            }
            if let Some(detail) = bake_detail(line) {
                spin.advance(detail);
            }
        }
        match child.try_wait() {
            Err(e) => return Err(format!("bake child error: {e}")),
            Ok(Some(status)) => {
                // Final drain: lines written between last poll and exit.
                if let Ok(mut f) = std::fs::File::open(log_path) {
                    if f.seek(SeekFrom::Start(offset)).is_ok() {
                        let mut rest = String::new();
                        use std::io::Read as _;
                        let _ = f.read_to_string(&mut rest);
                        for line in rest.lines() {
                            if verbose && !line.trim().is_empty() {
                                output::spinner::clear_active();
                                println!("{line}");
                            }
                            if let Some(detail) = bake_detail(line) {
                                spin.advance(detail);
                            }
                        }
                    }
                }
                return Ok(status);
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

/// Snapshot the current machine into a fresh temp bundle (no-bundle mode).
/// Honors the build's AUR mode; secrets are never auto-included.
fn auto_export_bundle(aur_mode: AurMode, aur_pkgdir: Option<&str>) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("omr-auto-bundle-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| format!("cannot clear {}: {e}", dir.display()))?;
    }
    crate::export::run(crate::export::ExportOptions {
        out: Some(dir.to_string_lossy().to_string()),
        without: std::collections::HashSet::new(),
        extra_excludes: vec![],
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
    // Ensure the path exists enough for df to report its filesystem.
    let anchor = if workdir.exists() {
        workdir.to_path_buf()
    } else if let Some(parent) = workdir.parent() {
        if parent.exists() {
            parent.to_path_buf()
        } else {
            return Ok(()); // Nowhere to measure; docker will fail loudly instead.
        }
    } else {
        return Ok(());
    };
    let r = system::run("df", &["-B1", "--output=avail", &anchor.to_string_lossy()])?;
    if r.status != 0 {
        return Ok(());
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
        None => Ok(()),
    }
}

fn space_ok(bytes: u64) -> bool {
    bytes >= MIN_FREE_BYTES
}

fn prepare_checkout(iso_checkout: Option<&str>, workdir: &Path) -> Result<PathBuf, String> {    if let Some(dir) = iso_checkout {
        let p = PathBuf::from(dir);
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
    let r = system::run("git", &["clone", "--depth", "1", ISO_REPO, &workdir.to_string_lossy()])?;
    if r.status != 0 {
        clone_spin.fail("clone failed");
        return Err(format!("clone failed: {}", r.stderr.trim()));
    }
    clone_spin.succeed(None);
    Ok(workdir.to_path_buf())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("cannot create {}: {e}", dst.display()))?;
    let r = system::run("cp", &["-a", &format!("{}/.", src.display()), &dst.to_string_lossy()])?;
    if r.status != 0 {
        return Err(format!("copy failed: {}", r.stderr.trim()));
    }
    Ok(())
}

/// Insert `addition` after the line containing `anchor` in `path`.
/// Fails if the anchor is missing (upstream drift) or already applied.
fn patch_after_anchor(path: &Path, marker: &str, anchor: &str, addition: &str) -> Result<(), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if text.contains(marker) {
        return Err(format!(
            "{} already patched (marker present); refusing to double-apply",
            path.display()
        ));
    }
    let idx = text.find(anchor).ok_or_else(|| {
        format!(
            "anchor not found in {}: '{anchor}' — upstream omarchy-iso drifted; refusing to patch",
            path.display()
        )
    })?;
    let line_end = text[idx..].find('\n').map(|i| idx + i + 1).unwrap_or(text.len());
    let mut patched = String::with_capacity(text.len() + addition.len());
    patched.push_str(&text[..line_end]);
    patched.push_str(addition);
    patched.push_str(&text[line_end..]);
    std::fs::write(path, patched).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
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
         \x20 grep -hv '^#\\|^$' /configs/recipe-packages.extra >> \"$build_cache_dir/airootfs/usr/share/omarchy-iso/omarchy-base.packages\"\n\
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
         \x20 all_packages+=(\"${recipe_extra_pkgs[@]}\")\n\
         \x20 mapfile -t all_packages < <(printf '%s\\n' \"${all_packages[@]}\" | sort -u)\n\
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
    let text =
        std::fs::read_to_string(&impl_path).map_err(|e| format!("cannot read phases_impl.py: {e}"))?;
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
fn compat_sanity_check(impl_path: &Path) -> Result<(), String> {
    const OLD: &str = "        installer.sanity_check(\n            offline=True,\n            skip_ntp=True,\n            skip_wkd=True,\n        )";
    const NEW: &str = "        # omarchy-recipe compat: archinstall >= 4.5 dropped the `offline` kwarg.\n        installer.sanity_check(\n            skip_ntp=True,\n            skip_wkd=True,\n        )";
    let text = std::fs::read_to_string(impl_path)
        .map_err(|e| format!("cannot read {}: {e}", impl_path.display()))?;
    if text.contains(NEW) || !text.contains(OLD) {
        return Ok(());
    }
    let patched = text.replacen(OLD, NEW, 1);
    std::fs::write(impl_path, patched)
        .map_err(|e| format!("cannot write {}: {e}", impl_path.display()))?;
    Ok(())
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
        std::fs::write(
            dir.join("builder_placeholder"),
            "",
        )
        .unwrap();
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
        // Generated bash must parse.
        let probe = dir.join("probe.sh");
        std::fs::write(&probe, "#!/bin/bash\nall_packages=(a)\nbuild_cache_dir=/tmp/x\n".to_string() + &build[build.find("# omarchy-recipe: ship").unwrap()..build.find("mkdir -p /tmp/offlinedb").unwrap()]).unwrap();
        let r = system::run("bash", &["-n", &probe.to_string_lossy()]).unwrap();
        assert!(r.status == 0, "generated bash failed syntax check: {}", r.stderr);

        let main = std::fs::read_to_string(orch.join("main.py")).unwrap();
        assert!(main.contains("stage_recipe,"));
        assert!(main.contains("Applying machine recipe"));
        let impl_py = std::fs::read_to_string(orch.join("phases_impl.py")).unwrap();
        assert!(impl_py.contains("def stage_recipe("));
        // Embedded python must parse.
        let r = system::run("python3", &["-c", &format!("import ast; ast.parse({})", serde_json::to_string(&format!("{impl_py}\n")).unwrap())]);
        assert!(r.unwrap().status == 0, "generated python failed syntax check");
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
        std::fs::write(&log, "Cloning into x\nTarget install resolves to 900 packages.\n")
            .unwrap();
        let sp = output::spinner::spin("pump");
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let status = pump_log(&log, &sp, &mut child).unwrap();
        assert!(status.success());
        sp.succeed(None);
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
        let dir = std::env::temp_dir().join("omr-boot-launch");
        let _ = std::fs::remove_dir_all(&dir);
        let mockbin = dir.join("mockbin");
        let bindir = dir.join("checkout/bin");
        std::fs::create_dir_all(&mockbin).unwrap();
        std::fs::create_dir_all(&bindir).unwrap();
        std::fs::write(mockbin.join("qemu-system-x86_64"), "#!/bin/sh\nexit 0\n").unwrap();
        let sentinel = dir.join("booted.txt");
        std::fs::write(
            bindir.join("omarchy-iso-boot"),
            format!("#!/bin/sh\necho \"$1\" > {}\n", sentinel.display()),
        )
        .unwrap();
        for f in [
            mockbin.join("qemu-system-x86_64"),
            bindir.join("omarchy-iso-boot"),
        ] {
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let iso = dir.join("test.iso");
        std::fs::write(&iso, b"fake").unwrap();

        // Prepend mocks; restore PATH before asserting so a failure cannot
        // pollute sibling tests running in other threads.
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths: Vec<_> = std::env::split_paths(&old_path).collect();
        paths.insert(0, mockbin);
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        maybe_boot(crate::cli::BootWhen::Yes, &dir.join("checkout"), &iso);
        std::env::set_var("PATH", &old_path);

        let got = std::fs::read_to_string(&sentinel).unwrap();
        assert_eq!(got.trim(), iso.display().to_string());
    }
}
