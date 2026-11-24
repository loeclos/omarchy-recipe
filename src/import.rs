//! `import`: apply a .recipe bundle onto fresh Omarchy (or first boot as root).

use std::path::{Path, PathBuf};

use crate::output;
use crate::recipe::*;
use crate::system;

pub struct ImportOptions {
    pub bundle: Option<String>,
    pub yes: bool,
    pub skip_packages: bool,
    pub skip_dotfiles: bool,
    pub first_boot: bool,
    pub home: Option<String>,
    pub as_user: Option<String>,
    pub offline: bool,
}

pub fn run(opts: ImportOptions) -> Result<(), String> {
    let bundle = resolve_bundle_dir(opts.bundle.as_deref())?;
    output::stage(1, 6, "Validating bundle");
    let recipe = crate::validate::validate_bundle(&bundle)?;
    if recipe.selection.include_secrets {
        output::warn(
            "this bundle ships secrets (.ssh, .gnupg, ...): \
             make sure you trust its source before applying it to this machine",
        );
    }

    // Resolve target user/home. First-boot runs as root for another user.
    let (home, as_user): (PathBuf, Option<String>) = if opts.first_boot {
        let user = match opts.as_user {
            Some(u) => u,
            None => {
                system::primary_user().ok_or("first-boot: no human user found (UID >= 1000) yet")?
            }
        };
        let home = match opts.home {
            Some(h) => PathBuf::from(h),
            None => system::user_home(&user)
                .ok_or_else(|| format!("first-boot: no home directory found for user '{user}'"))?,
        };
        if !home.is_dir() {
            return Err(format!(
                "first-boot: home {} does not exist",
                home.display()
            ));
        }
        (home, Some(user))
    } else {
        let home = match opts.home {
            Some(h) => PathBuf::from(h),
            None => system::home_dir().ok_or("cannot determine $HOME")?,
        };
        (home, opts.as_user)
    };

    let channel_want = recipe.omarchy.channel.clone();
    let channel_have = system::omarchy_channel();
    if channel_want != "unknown" && channel_have != "unknown" && channel_want != channel_have {
        output::warn(&format!(
            "channel mismatch: recipe={channel_want} this-machine={channel_have} (continuing)"
        ));
    }
    output::step(&format!(
        "Importing recipe (Omarchy {}, channel {channel_want})",
        recipe.omarchy.version,
    ));
    if let Some(u) = &as_user {
        output::info(&format!("Target user: {u} ({})", home.display()));
    }

    if !opts.yes && !opts.first_boot {
        let ans = output::prompt(&format!(
            "Apply recipe to {}? Backs up ~/.config first. [y/N] ",
            home.display()
        ))?;
        if !matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
            return Err("aborted".into());
        }
    }

    let deferred_aur: Option<Vec<&str>> = if opts.first_boot
        && !opts.skip_packages
        && !opts.offline
        && !recipe.excluded("aur")
        && recipe.packages.aur_mode != "vendored"
        && !recipe.packages.aur.is_empty()
    {
        Some(recipe.packages.aur.iter().map(|s| s.as_str()).collect())
    } else {
        None
    };
    if !opts.skip_packages {
        output::stage(2, 6, "Installing packages");
        install_packages(
            &recipe,
            &bundle,
            opts.offline,
            opts.first_boot,
            deferred_aur.is_some(),
        )?;
    } else {
        output::info("Skipping packages (--skip-packages).");
    }

    if !opts.skip_dotfiles {
        output::stage(3, 6, "Restoring dotfiles");
        restore_dotfiles(&bundle, &recipe, &home, as_user.as_deref())?;
    } else {
        output::info("Skipping dotfiles (--skip-dotfiles).");
    }

    // On first boot, restore the user's environment before waiting on the
    // network for AUR packages. A failed AUR retry still leaves the unit armed
    // for the next boot, but it no longer prevents dotfiles from being applied.
    if let Some(aur) = deferred_aur {
        install_aur_wifi(&aur, true)?;
    }

    output::stage(4, 6, "Applying themes, font and plugins");

    if !recipe.excluded("themes") {
        install_themes(&recipe, &home, as_user.as_deref(), opts.offline)?;
    }
    if !recipe.excluded("font") {
        apply_font(&recipe, &home, as_user.as_deref())?;
    }
    if !recipe.excluded("plugins") {
        apply_plugins(&recipe, &home, as_user.as_deref(), opts.offline)?;
    }
    if !recipe.excluded("services") {
        output::stage(5, 6, "Enabling services");
        apply_services(&recipe, &home, as_user.as_deref())?;
    }

    // Reload UI.
    output::stage(6, 6, "Reloading desktop");
    if system::cmd_exists("omarchy-theme-refresh") {
        if let Err(e) = system::run("omarchy-theme-refresh", &[]) {
            output::warn(&format!("desktop refresh failed: {e}"));
        }
    } else if system::cmd_exists("hyprctl") {
        if let Err(e) = system::run("hyprctl", &["reload"]) {
            output::warn(&format!("Hyprland reload failed: {e}"));
        }
    }

    if opts.first_boot {
        // Full success: disarm so this runs exactly once. If this fails, return
        // an error so systemd retries rather than silently re-applying forever.
        let r = system::run("systemctl", &["disable", "omarchy-apply-recipe.service"])?;
        if r.status != 0 {
            return Err(format!(
                "import succeeded but could not disable first-boot service: {}",
                r.stderr.trim()
            ));
        }
    }
    output::ok("Import complete");
    output::info("Reboot recommended. Old config kept at ~/.config.pre-recipe-*");
    Ok(())
}

/// Run a user-scoped Omarchy command as the target user when importing
/// for someone else (first boot runs as root); direct call otherwise.
fn run_as(
    home: &Path,
    as_user: Option<&str>,
    prog: &str,
    args: &[&str],
) -> Result<system::CmdResult, String> {
    match as_user {
        Some(u) => {
            let current = std::env::var("USER").unwrap_or_default();
            if !is_root() && u != current {
                return Err(format!("cannot run {prog} as '{u}': --as-user requires root unless it is the current user"));
            }
            if !is_root() && u == current {
                return system::run(prog, args);
            }
            let home_var = format!("HOME={}", home.display());
            let user_var = format!("USER={u}");
            let logname_var = format!("LOGNAME={u}");
            let runtime_var = system::user_uid(u).and_then(|uid| {
                let dir = PathBuf::from(format!("/run/user/{uid}"));
                dir.is_dir()
                    .then(|| format!("XDG_RUNTIME_DIR={}", dir.display()))
            });
            let mut full: Vec<&str> =
                vec!["-u", u, "--", "env", &home_var, &user_var, &logname_var];
            if let Some(value) = &runtime_var {
                full.push(value);
            }
            full.push(prog);
            full.extend(args);
            system::run("runuser", &full)
        }
        None => system::run(prog, args),
    }
}

fn install_packages(
    recipe: &Recipe,
    bundle: &Path,
    offline: bool,
    first_boot: bool,
    defer_aur: bool,
) -> Result<(), String> {
    let repo: Vec<&str> = if recipe.excluded("packages") {
        vec![]
    } else {
        recipe
            .packages
            .explicit_repo
            .iter()
            .map(|s| s.as_str())
            .collect()
    };
    if repo.is_empty() {
        output::info("No extra repo packages in recipe.");
    } else if offline || first_boot {
        // On first boot after an ISO install, repo packages were already
        // installed from the offline mirror at install time (build-iso appends
        // them to the bundled base list). Verify presence, install stragglers.
        output::info(&format!("Verifying {} repo packages ...", repo.len()));
        let mut missing: Vec<&str> = vec![];
        for p in &repo {
            let r = system::run("pacman", &["-Q", p])?;
            match r.status {
                0 => {}
                1 => missing.push(p),
                _ => return Err(format!("pacman -Q {p} failed: {}", r.stderr.trim())),
            }
        }
        if missing.is_empty() {
            output::info("All repo packages present.");
        } else if offline {
            return Err(format!(
                "offline but {} repository packages are missing: {}",
                missing.len(),
                missing.join(" ")
            ));
        } else {
            install_repo_packages(&missing)?;
        }
    } else {
        install_repo_packages(&repo)?;
    }

    let aur: Vec<&str> = if recipe.excluded("aur") {
        vec![]
    } else {
        recipe.packages.aur.iter().map(|s| s.as_str()).collect()
    };
    if aur.is_empty() {
        return Ok(());
    }
    if defer_aur {
        output::info("Deferring network-dependent AUR install until after dotfiles restore.");
        return Ok(());
    }
    if recipe.packages.aur_mode == "vendored" {
        install_vendored_aur(recipe, bundle, first_boot)?;
    } else if offline {
        return Err(format!(
            "offline import cannot install {} AUR packages: {}",
            aur.len(),
            aur.join(" ")
        ));
    } else {
        install_aur_wifi(&aur, first_boot)?;
    }
    Ok(())
}

/// Install AUR packages with network. On first boot the network may not be up
/// yet (same rationale as omarchy-tailscale-join): retry in the background
/// window, fail after ~30 min so the unit stays enabled for the next boot.
fn install_aur_wifi(aur: &[&str], first_boot: bool) -> Result<(), String> {
    output::info(&format!(
        "Installing {} AUR packages (needs network) ...",
        aur.len()
    ));
    let helper = match system::aur_helper() {
        Some(h) => h,
        None => {
            return Err(format!(
                "no AUR helper (yay/paru) found; cannot install: {}",
                aur.join(" ")
            ));
        }
    };
    let mut args = vec!["-S", "--noconfirm", "--needed", "--"];
    args.extend(aur.iter().copied());
    if !first_boot {
        let spin = output::spinner::spin("installing AUR packages");
        let r = match system::run(helper, &args) {
            Ok(r) => r,
            Err(e) => {
                spin.fail("cannot start AUR install");
                return Err(e);
            }
        };
        if r.status != 0 {
            spin.fail("some AUR packages failed");
            return Err(format!("AUR install failed: {}", r.stderr.trim()));
        }
        spin.succeed(None);
        return Ok(());
    }
    let spin = output::spinner::spin("installing AUR packages (waiting for network)");
    let attempts = std::env::var("OMARCHY_RECIPE_NET_RETRIES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(120);
    for i in 1..=attempts {
        spin.set_detail(&format!("AUR install attempt {i}/{attempts}"));
        let r = match system::run(helper, &args) {
            Ok(r) => r,
            Err(e) => {
                spin.fail("cannot start AUR install");
                return Err(e);
            }
        };
        if r.status == 0 {
            spin.succeed(None);
            return Ok(());
        }
        if i == attempts {
            break;
        }
        output::info(&format!(
            "AUR install attempt {i}/{attempts} failed (likely no network); retrying in 15s ..."
        ));
        std::thread::sleep(std::time::Duration::from_secs(15));
    }
    spin.fail("AUR install gave up");
    Err("AUR install failed after retries; unit stays enabled for next boot".into())
}

fn install_repo_packages(pkgs: &[&str]) -> Result<(), String> {
    output::info(&format!("Installing {} repo packages ...", pkgs.len()));
    let spin = output::spinner::spin("installing repo packages");
    let result = if system::cmd_exists("omarchy-pkg-add") {
        system::run("omarchy-pkg-add", pkgs)
    } else {
        let mut args: Vec<&str> = vec!["-S", "--noconfirm", "--needed", "--"];
        args.extend(pkgs.iter().copied());
        if is_root() {
            system::run("pacman", &args)
        } else {
            let mut sudo_args = vec!["pacman"];
            sudo_args.extend(args);
            system::run("sudo", &sudo_args)
        }
    };
    let r = match result {
        Ok(r) => r,
        Err(e) => {
            spin.fail("cannot start repo package install");
            return Err(e);
        }
    };
    if r.status != 0 {
        spin.fail("some repo packages failed");
        return Err(format!(
            "repository package install failed: {}",
            r.stderr.trim()
        ));
    }
    spin.succeed(None);
    Ok(())
}

/// Install vendored AUR .pkgs fully offline via pacman -U.
fn install_vendored_aur(recipe: &Recipe, bundle: &Path, first_boot: bool) -> Result<(), String> {
    if recipe.aur_vendored.is_empty() {
        if first_boot {
            output::warn(
                "aur_mode=vendored but bundle has no aur-pkgs/ manifest; continuing first boot",
            );
            return Ok(());
        }
        return Err("aur_mode=vendored but bundle has no aur-pkgs/ manifest".into());
    }
    output::info(&format!(
        "Installing {} vendored AUR packages (offline) ...",
        recipe.aur_vendored.len()
    ));
    let mut files: Vec<String> = vec![];
    for v in &recipe.aur_vendored {
        let p = crate::recipe::bundle_path(bundle, Path::new(&v.file))?;
        if !p.is_file() {
            return Err(format!(
                "vendored package missing: {} (required by {})",
                p.display(),
                v.name
            ));
        }
        files.push(p.to_string_lossy().to_string());
    }
    let mut args: Vec<&str> = vec!["-U", "--noconfirm", "--needed", "--"];
    for f in &files {
        args.push(f.as_str());
    }
    // pacman -U needs root; first-boot runs as root, interactive import uses sudo.
    let r = if is_root() {
        system::run("pacman", &args)?
    } else {
        let mut sudo_args = vec!["pacman"];
        sudo_args.extend(args);
        system::run("sudo", &sudo_args)?
    };
    if r.status != 0 {
        if first_boot {
            output::warn(&format!(
                "some vendored AUR packages failed (deps may be missing); continuing first boot: {}",
                r.stderr.trim()
            ));
            return Ok(());
        }
        return Err(format!("vendored AUR install failed: {}", r.stderr.trim()));
    }
    Ok(())
}

fn restore_dotfiles(
    bundle: &Path,
    recipe: &Recipe,
    home: &Path,
    as_user: Option<&str>,
) -> Result<(), String> {
    let tarball = crate::recipe::tarball_path(bundle, recipe)?;
    if !tarball.is_file() {
        return Err(format!("dotfiles tarball not found: {}", tarball.display()));
    }
    let config = home.join(".config");
    if config.is_dir() {
        let ts = system::run("date", &["+%Y%m%d-%H%M%S"])?
            .stdout
            .trim()
            .to_string();
        let mut backup = home.join(format!(".config.pre-recipe-{ts}"));
        let mut n = 1;
        while backup.exists() {
            backup = home.join(format!(".config.pre-recipe-{ts}-{n}"));
            n += 1;
        }
        output::info(&format!(
            "Backing up {} -> {}",
            config.display(),
            backup.display()
        ));
        copy_dir(&config, &backup)?;
        if let Some(u) = as_user {
            let owner = format!("{u}:{u}");
            let r = system::run("chown", &["-hR", &owner, &backup.to_string_lossy()])?;
            if r.status != 0 {
                return Err(format!("cannot set backup ownership: {}", r.stderr.trim()));
            }
        }
    }
    let stage = system::unique_dir(
        &home.join(".cache/omarchy-recipe/import-stage"),
        "omarchy-recipe-stage",
    )?;
    let result = (|| -> Result<(), String> {
        let extract_spin = output::spinner::spin("extracting dotfiles");
        let tar_path = tarball.to_string_lossy().to_string();
        let r = match system::run_in(&stage, "tar", &["-xf", &tar_path]) {
            Ok(r) => r,
            Err(e) => {
                extract_spin.fail("cannot start dotfiles extraction");
                return Err(e);
            }
        };
        if r.status != 0 {
            extract_spin.fail("dotfiles extraction failed");
            return Err(format!("cannot extract tarball: {}", r.stderr.trim()));
        }
        extract_spin.succeed(None);

        // Monitor layout always belongs to the target machine, even on a fresh
        // install where no monitors.conf exists yet.
        let mut skipped_monitors = false;
        for mon in ["hypr/monitors.conf", "hypr/monitors"] {
            let staged = stage.join(".config").join(mon);
            if staged.exists() || staged.symlink_metadata().is_ok() {
                if staged.is_dir() && !staged.is_symlink() {
                    std::fs::remove_dir_all(&staged)
                        .map_err(|e| format!("cannot remove staged {mon}: {e}"))?;
                } else {
                    std::fs::remove_file(&staged)
                        .map_err(|e| format!("cannot remove staged {mon}: {e}"))?;
                }
                skipped_monitors = true;
            }
        }
        if skipped_monitors {
            output::warn("skipping recipe's machine-specific Hypr monitor config");
        }

        // Secrets must stay private. Do this before copying so root-owned
        // extracted files also get safe modes, without following symlinks.
        for dir in [".ssh", ".gnupg", ".pki"] {
            let path = stage.join(dir);
            if path.symlink_metadata().is_ok() {
                restrict_private_permissions(&path)?;
            }
        }
        if let Some(u) = as_user {
            let owner = format!("{u}:{u}");
            let r = system::run("chown", &["-hR", &owner, &stage.to_string_lossy()])?;
            if r.status != 0 {
                return Err(format!(
                    "cannot set restored file ownership: {}",
                    r.stderr.trim()
                ));
            }
        }

        output::info("Restoring dotfiles ...");
        // rsync --backup keeps per-file pre-recipe copies; cp fallback otherwise.
        let stage_s = format!("{}/", stage.display());
        let home_s = home.to_string_lossy().to_string();
        let rsync_spin = output::spinner::spin("restoring dotfiles");
        if system::cmd_exists("rsync") {
            let ts = match system::run("date", &["+%Y%m%d-%H%M%S"]) {
                Ok(r) => r.stdout.trim().to_string(),
                Err(e) => {
                    rsync_spin.fail("cannot prepare dotfiles backup suffix");
                    return Err(e);
                }
            };
            let r = match system::run(
                "rsync",
                &[
                    "-a",
                    "--backup",
                    &format!("--suffix=.pre-recipe-{ts}"),
                    &stage_s,
                    &home_s,
                ],
            ) {
                Ok(r) => r,
                Err(e) => {
                    rsync_spin.fail("cannot start dotfiles restore");
                    return Err(e);
                }
            };
            if r.status != 0 {
                rsync_spin.fail("dotfiles restore failed");
                return Err(format!("rsync failed: {}", r.stderr.trim()));
            }
        } else {
            if let Err(e) = copy_stage(&stage, home) {
                rsync_spin.fail("dotfiles restore failed");
                return Err(e);
            }
        }
        rsync_spin.succeed(None);

        // Re-apply private modes after restore (rsync/cp should preserve them,
        // but this also covers archives made by older exporters).
        for dir in [".ssh", ".gnupg", ".pki"] {
            let path = home.join(dir);
            if path.symlink_metadata().is_ok() {
                restrict_private_permissions(&path)?;
            }
        }
        if system::cmd_exists("gtk-update-icon-cache") {
            let icons = home.join(".local/share/icons/hicolor");
            if icons.is_dir() {
                let _ = system::run("gtk-update-icon-cache", &[&icons.to_string_lossy()]);
            }
        }
        if system::cmd_exists("update-desktop-database") {
            let apps = home.join(".local/share/applications");
            if apps.is_dir() {
                let _ = system::run("update-desktop-database", &[&apps.to_string_lossy()]);
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Remove group/other access from a private directory tree without traversing
/// symlinks (which could otherwise point outside the restored home).
fn restrict_private_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mode = if meta.is_dir() {
        0o700
    } else {
        meta.permissions().mode() & 0o700
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot secure {}: {e}", path.display()))?;
    if meta.is_dir() {
        for entry in
            std::fs::read_dir(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?
        {
            let entry = entry.map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            restrict_private_permissions(&entry.path())?;
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    // cp -a via system cp (preserves everything, no walk crate needed).
    let r = system::run(
        "cp",
        &["-a", &src.to_string_lossy(), &dst.to_string_lossy()],
    )?;
    if r.status != 0 {
        return Err(format!("backup failed: {}", r.stderr.trim()));
    }
    Ok(())
}

fn copy_stage(stage: &Path, home: &Path) -> Result<(), String> {
    let r = system::run(
        "cp",
        &[
            "-a",
            &format!("{}/.", stage.display()),
            &home.to_string_lossy(),
        ],
    )?;
    if r.status != 0 {
        return Err(format!("dotfiles copy failed: {}", r.stderr.trim()));
    }
    Ok(())
}

fn install_themes(
    recipe: &Recipe,
    home: &Path,
    as_user: Option<&str>,
    offline: bool,
) -> Result<(), String> {
    let Some(themes) = &recipe.themes else {
        return Ok(());
    };
    let want = themes.current.clone().unwrap_or_default();
    if want.is_empty() || want == "unknown" {
        return Ok(());
    }
    let mut failures = vec![];
    for t in themes.installed.iter().filter(|t| t.source == "custom") {
        let Some(url) = t.url.clone() else { continue };
        let dest = home.join(".config/omarchy/themes").join(&t.name);
        if !dest.is_dir() {
            if offline {
                failures.push(format!(
                    "offline: cannot fetch custom theme '{}' from {url}",
                    t.name
                ));
                continue;
            }
            output::info(&format!("Installing theme {} from {url} ...", t.name));
            let result = if system::cmd_exists("omarchy-theme-install") {
                run_as(home, as_user, "omarchy-theme-install", &[&url])
            } else if system::cmd_exists("git") {
                if let Some(parent) = dest.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        failures.push(format!("cannot create theme directory: {e}"));
                        continue;
                    }
                }
                system::run("git", &["clone", "--", &url, &dest.to_string_lossy()])
            } else {
                Err("neither omarchy-theme-install nor git is available".into())
            };
            match result {
                Ok(r) if r.status == 0 => {}
                Ok(r) => failures.push(format!(
                    "theme install failed for {}: {}",
                    t.name,
                    r.stderr.trim()
                )),
                Err(e) => failures.push(e),
            }
        }
        // Reproduce the recorded revision when possible. Theme installs are
        // otherwise floating clones of the repository's default branch.
        if let Some(commit) = t
            .r#ref
            .as_deref()
            .filter(|r| !r.is_empty() && *r != "unknown")
        {
            if dest.is_dir() {
                match system::run_in(&dest, "git", &["checkout", "--detach", commit]) {
                    Ok(r) if r.status == 0 => {}
                    Ok(r) => failures.push(format!(
                        "cannot pin theme {} to {}: {}",
                        t.name,
                        commit,
                        r.stderr.trim()
                    )),
                    Err(e) => failures.push(e),
                }
            }
        }
    }
    if !system::cmd_exists("omarchy-theme-set") {
        failures.push("omarchy-theme-set not found".into());
    } else {
        output::info(&format!("Setting theme: {want}"));
        match run_as(home, as_user, "omarchy-theme-set", &[&want]) {
            Ok(r) if r.status == 0 => {}
            Ok(r) => failures.push(format!("theme set failed: {}", r.stderr.trim())),
            Err(e) => failures.push(e),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn apply_font(recipe: &Recipe, home: &Path, as_user: Option<&str>) -> Result<(), String> {
    let want = recipe
        .font
        .as_ref()
        .and_then(|f| f.monospace.clone())
        .unwrap_or_default();
    if want.is_empty() || want == "unknown" || !system::cmd_exists("omarchy-font-set") {
        return Ok(());
    }
    let current = run_as(home, as_user, "omarchy-font-current", &[])?;
    if current.status != 0 {
        return Err(format!(
            "cannot read current font: {}",
            current.stderr.trim()
        ));
    }
    let have = current.stdout.trim().to_string();
    if have != want {
        output::info(&format!("Setting font: {want}"));
        let r = run_as(home, as_user, "omarchy-font-set", &[&want])?;
        if r.status != 0 {
            return Err(format!("font set failed: {}", r.stderr.trim()));
        }
    }
    Ok(())
}

fn plugin_installed(home: &Path, as_user: Option<&str>, id: &str) -> bool {
    // The plugin's manifest is the source of truth and works before the shell
    // has rescanned restored files (or when no shell IPC is available).
    if home
        .join(".config/omarchy/plugins")
        .join(id)
        .join("manifest.json")
        .is_file()
    {
        return true;
    }
    let Ok(result) = run_as(home, as_user, "omarchy-plugin-list", &["--json"]) else {
        return false;
    };
    if result.status != 0 {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&result.stdout)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .is_some_and(|plugins| {
            plugins
                .iter()
                .any(|plugin| plugin.get("id").and_then(|id| id.as_str()) == Some(id))
        })
}

fn apply_plugins(
    recipe: &Recipe,
    home: &Path,
    as_user: Option<&str>,
    offline: bool,
) -> Result<(), String> {
    if recipe.plugins.is_empty() {
        return Ok(());
    }
    if !system::cmd_exists("omarchy-plugin-enable") || !system::cmd_exists("omarchy-plugin-disable")
    {
        return Err("plugin enable/disable commands not found".into());
    }
    let mut failures = vec![];
    for p in &recipe.plugins {
        if p.enabled {
            if !p.first_party && !plugin_installed(home, as_user, &p.id) {
                let Some(url) = p.url.as_deref().filter(|u| !u.is_empty()) else {
                    failures.push(format!("third-party plugin '{}' has no source URL", p.id));
                    continue;
                };
                if offline {
                    failures.push(format!("offline: cannot install plugin '{}'", p.id));
                    continue;
                }
                if !system::cmd_exists("omarchy-plugin-add") {
                    failures.push(
                        "omarchy-plugin-add not found; cannot install third-party plugins".into(),
                    );
                    continue;
                }
                let r = run_as(home, as_user, "omarchy-plugin-add", &[url, "--yes"])?;
                if r.status != 0 {
                    failures.push(format!(
                        "plugin install failed for {}: {}",
                        p.id,
                        r.stderr.trim()
                    ));
                    continue;
                }
            }
            let r = run_as(home, as_user, "omarchy-plugin-enable", &[&p.id])?;
            if r.status != 0 {
                failures.push(format!(
                    "plugin enable failed for {}: {}",
                    p.id,
                    r.stderr.trim()
                ));
            }
        } else {
            let r = run_as(home, as_user, "omarchy-plugin-disable", &[&p.id])?;
            if r.status != 0 {
                failures.push(format!(
                    "plugin disable failed for {}: {}",
                    p.id,
                    r.stderr.trim()
                ));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn apply_services(recipe: &Recipe, home: &Path, as_user: Option<&str>) -> Result<(), String> {
    let Some(svcs) = &recipe.services else {
        return Ok(());
    };
    if !system::cmd_exists("systemctl") {
        return Err("systemctl not found; cannot enable recipe services".into());
    }
    let mut failures = vec![];
    let current_user = std::env::var("USER").ok();
    let unit_user = as_user.or(current_user.as_deref());
    let runtime_unit_dir = unit_user
        .and_then(system::user_uid)
        .map(|uid| PathBuf::from(format!("/run/user/{uid}/systemd/user")));
    // User services: enable via wants-symlinks so no running user manager is
    // needed (works on first boot before first login). Always use the resolved
    // target home, not the caller's possibly-root home.
    for svc in &svcs.user {
        let allowed = svc.starts_with("omarchy-")
            || svc == "owed.service"
            || svc.starts_with("xdg-user-dirs");
        if !allowed {
            output::warn(&format!("skipping non-allowlisted user service: {svc}"));
            continue;
        }
        let Some(src) = find_user_unit(home, svc, runtime_unit_dir.as_deref()) else {
            failures.push(format!("user unit not found: {svc}"));
            continue;
        };
        let wants = home.join(".config/systemd/user/default.target.wants");
        if let Err(e) = std::fs::create_dir_all(&wants) {
            failures.push(format!("cannot create {}: {e}", wants.display()));
            continue;
        }
        let link = wants.join(svc);
        let result = if link.symlink_metadata().is_ok() {
            if std::fs::read_link(&link).ok().as_deref() == Some(src.as_path()) {
                Ok(())
            } else {
                std::fs::remove_file(&link)
                    .map_err(|e| format!("cannot replace {}: {e}", link.display()))
                    .and_then(|_| {
                        std::os::unix::fs::symlink(&src, &link)
                            .map_err(|e| format!("cannot link {}: {e}", link.display()))
                    })
            }
        } else {
            std::os::unix::fs::symlink(&src, &link)
                .map_err(|e| format!("cannot link {}: {e}", link.display()))
        };
        if let Err(e) = result {
            failures.push(e);
        } else if let Some(u) = as_user {
            let owner = format!("{u}:{u}");
            match system::run("chown", &["-hR", &owner, &wants.to_string_lossy()]) {
                Ok(r) if r.status == 0 => {}
                Ok(r) => failures.push(format!(
                    "cannot set user-service ownership for {svc}: {}",
                    r.stderr.trim()
                )),
                Err(e) => failures.push(e),
            }
        }
    }
    for svc in &svcs.system {
        let allowed = svc.starts_with("tailscale")
            || svc.starts_with("docker")
            || svc.starts_with("ufw")
            || svc.starts_with("cups")
            || svc.starts_with("bluetooth")
            || svc.starts_with("avahi-daemon")
            || svc.starts_with("power-profiles-daemon")
            || svc.starts_with("thermald")
            || svc.starts_with("sddm")
            || svc.starts_with("NetworkManager");
        if !allowed {
            output::warn(&format!("skipping non-allowlisted system service: {svc}"));
            continue;
        }
        let result = if is_root() {
            system::run("systemctl", &["enable", svc])
        } else {
            system::run("sudo", &["systemctl", "enable", svc])
        };
        match result {
            Ok(r) if r.status == 0 => {}
            Ok(r) => failures.push(format!(
                "system service enable failed for {svc}: {}",
                r.stderr.trim()
            )),
            Err(e) => failures.push(e),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn find_user_unit(home: &Path, svc: &str, runtime_unit_dir: Option<&Path>) -> Option<PathBuf> {
    // Enabled instances such as foo@bar.service are backed by foo@.service.
    let unit = if let Some(at) = svc.find('@') {
        format!("{}.service", &svc[..=at])
    } else {
        svc.to_string()
    };
    let mut dirs = vec![
        home.join(".config/systemd/user"),
        PathBuf::from("/etc/xdg/systemd/user"),
        PathBuf::from("/etc/systemd/user"),
        home.join(".local/share/systemd/user"),
        PathBuf::from("/usr/local/lib/systemd/user"),
        PathBuf::from("/usr/local/share/systemd/user"),
        PathBuf::from("/usr/share/systemd/user"),
        PathBuf::from("/usr/lib/systemd/user"),
    ];
    if let Some(runtime) = runtime_unit_dir {
        dirs.insert(1, runtime.to_path_buf());
    }
    dirs.into_iter()
        .map(|dir| dir.join(&unit))
        .find(|path| path.is_file())
}

// Minimal euid check without a libc dependency.
fn is_root() -> bool {
    // SAFETY: geteuid is infallible.
    unsafe { geteuid() == 0 }
}
extern "C" {
    fn geteuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_secret_tree_is_secured_without_following_symlinks() {
        let dir = system::temp_dir("omr-private-perms").unwrap();
        let ssh = dir.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let key = ssh.join("id_test");
        std::fs::write(&key, b"key").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        let outside = dir.join("outside");
        std::fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, ssh.join("link")).unwrap();

        restrict_private_permissions(&ssh).unwrap();
        assert_eq!(
            std::fs::metadata(&ssh).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn restored_plugin_manifest_counts_as_installed_without_shell_ipc() {
        let home = system::temp_dir("omr-plugin-installed").unwrap();
        let manifest = home.join(".config/omarchy/plugins/acme.weather/manifest.json");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, b"{}").unwrap();
        assert!(plugin_installed(
            &home,
            Some("no-such-user"),
            "acme.weather"
        ));
    }

    #[test]
    fn runtime_user_unit_takes_precedence_over_data_dir() {
        let home = system::temp_dir("omr-user-unit-order").unwrap();
        let data_unit = home.join(".local/share/systemd/user/example.service");
        std::fs::create_dir_all(data_unit.parent().unwrap()).unwrap();
        std::fs::write(&data_unit, b"data").unwrap();
        let runtime = system::temp_dir("omr-user-unit-runtime").unwrap();
        let runtime_unit = runtime.join("example.service");
        std::fs::write(&runtime_unit, b"runtime").unwrap();
        assert_eq!(
            find_user_unit(&home, "example.service", Some(&runtime)),
            Some(runtime_unit)
        );
    }

    #[test]
    fn user_unit_instances_resolve_to_templates_in_target_home() {
        let home = system::temp_dir("omr-user-unit").unwrap();
        let unit = home.join(".config/systemd/user/example@.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, b"[Service]\n").unwrap();
        assert_eq!(
            find_user_unit(&home, "example@one.service", None),
            Some(unit)
        );
    }
}
