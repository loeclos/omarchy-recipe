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
}

pub fn run(opts: BuildIsoOptions) -> Result<(), String> {
    if !["stable", "rc", "edge"].contains(&opts.mirror.as_str()) {
        return Err(format!(
            "unknown mirror '{}' (expected stable, rc or edge)",
            opts.mirror
        ));
    }
    let bundle = match opts.bundle {
        Some(b) => resolve_bundle_dir(Some(&b))?,
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
        None => std::env::temp_dir().join(format!("omr-iso-{}", std::process::id())),
    };
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
    let mut cmd = std::process::Command::new(checkout.join("bin/omarchy-iso-make"));
    cmd.current_dir(&checkout)
        .arg("--keep-pkg-cache")
        .arg("--no-boot-offer")
        .env("OMARCHY_MIRROR", &opts.mirror);
    if output::is_quiet() {
        let log = std::fs::File::create(&log_path)
            .map_err(|e| format!("cannot create build log: {e}"))?;
        cmd.stdout(log.try_clone().map_err(|e| format!("log redirect failed: {e}"))?);
        cmd.stderr(log);
    }
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
    let status = cmd.status().map_err(|e| format!("cannot run omarchy-iso-make: {e}"))?;
    let elapsed = started.elapsed();
    if !status.success() {
        if output::is_quiet() {
            return Err(format!(
                "omarchy-iso-make failed after {}; see {}",
                fmt_duration(elapsed),
                log_path.display()
            ));
        }
        return Err("omarchy-iso-make failed (see above)".into());
    }

    let (iso_name, iso_size) = newest_iso(&checkout);
    output::ok(&format!("ISO baked in {}", fmt_duration(elapsed)));
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
    Ok(())
}

fn fmt_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    format!("{}h {:02}m {:02}s", s / 3600, (s % 3600) / 60, s % 60)
}

/// Newest *.iso under <checkout>/release, with human size.
fn newest_iso(checkout: &Path) -> (String, String) {
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
            (p.display().to_string(), human_size(size))
        }
        None => ("<not found in release/>".into(), "—".into()),
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
    let r = system::run("git", &["clone", "--depth", "1", ISO_REPO, &workdir.to_string_lossy()])?;
    if r.status != 0 {
        return Err(format!("clone failed: {}", r.stderr.trim()));
    }
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
}
