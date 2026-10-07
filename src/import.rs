//! `import`: apply a .recipe bundle onto fresh Omarchy (or first boot as root).

use std::io::Write;
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
    let recipe_path = bundle.join("recipe.json");
    output::stage(1, 6, "Validating bundle");
    let recipe = Recipe::load(&recipe_path)?;
    crate::validate::validate_bundle(&bundle)?;
    // Vendored-AUR resolution + first-boot unit contract rely on this.
    std::env::set_var("OMARCHY_RECIPE_BUNDLE", &bundle);
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
            None => system::primary_user()
                .ok_or("first-boot: no human user found (UID >= 1000) yet")?,
        };
        let home = match opts.home {
            Some(h) => PathBuf::from(h),
            None => PathBuf::from(format!("/home/{user}")),
        };
        if !home.is_dir() {
            return Err(format!("first-boot: home {} does not exist", home.display()));
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
    if channel_want != "unknown"
        && channel_have != "unknown"
        && channel_want != channel_have
    {
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
        print!("Apply recipe to {}? Backs up ~/.config first. [y/N] ", home.display());
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut ans = String::new();
        std::io::stdin().read_line(&mut ans).map_err(|e| e.to_string())?;
        if !matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
            return Err("aborted".into());
        }
    }

    if !opts.skip_packages {
        output::stage(2, 6, "Installing packages");
        install_packages(&recipe, opts.offline, opts.first_boot)?;
    } else {
        output::info("Skipping packages (--skip-packages).");
    }

    if !opts.skip_dotfiles {
        output::stage(3, 6, "Restoring dotfiles");
        restore_dotfiles(&bundle, &recipe, &home, as_user.as_deref())?;
    } else {
        output::info("Skipping dotfiles (--skip-dotfiles).");
    }

    output::stage(4, 6, "Applying themes, font and plugins");

    if !recipe.excluded("themes") {
        install_themes(&recipe, &home, as_user.as_deref(), opts.offline)?;
    }
    if !recipe.excluded("font") {
        apply_font(&recipe, &home, as_user.as_deref())?;
    }
    if !recipe.excluded("plugins") {
        apply_plugins(&recipe, &home, as_user.as_deref())?;
    }
    if !recipe.excluded("services") {
        output::stage(5, 6, "Enabling services");
        apply_services(&recipe, as_user.as_deref())?;
    }

    // Reload UI.
    output::stage(6, 6, "Reloading desktop");
    if system::cmd_exists("omarchy-theme-refresh") {
        let _ = system::run("omarchy-theme-refresh", &[]);
    } else if system::cmd_exists("hyprctl") {
        let _ = system::run("hyprctl", &["reload"]);
    }

    output::ok("Import complete.");
    if opts.first_boot {
        // Full success: disarm so this runs exactly once. Any Err return
        // above skips this, leaving the unit enabled for the next boot.
        let _ = system::run("systemctl", &["disable", "omarchy-apply-recipe.service"]);
    }
    output::info("Reboot recommended. Old config kept at ~/.config.pre-recipe-*");
    Ok(())
}

/// Run a user-scoped Omarchy command as the target user when importing
/// for someone else (first boot runs as root); direct call otherwise.
fn run_as(home: &Path, as_user: Option<&str>, prog: &str, args: &[&str]) -> Result<system::CmdResult, String> {
    match as_user {
        Some(u) => {
            let home_var = format!("HOME={}", home.display());
            let mut full: Vec<&str> = vec!["-u", u, "--", "env", &home_var, prog];
            full.extend(args);
            system::run("runuser", &full)
        }
        None => system::run(prog, args),
    }
}

fn install_packages(recipe: &Recipe, offline: bool, first_boot: bool) -> Result<(), String> {
    let repo: Vec<&str> = if recipe.excluded("packages") {
        vec![]
    } else {
        recipe.packages.explicit_repo.iter().map(|s| s.as_str()).collect()
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
            if r.status != 0 {
                missing.push(p);
            }
        }
        if missing.is_empty() {
            output::info("All repo packages present.");
        } else if offline {
            output::warn(&format!(
                "offline and {} packages missing (installed at ISO build time only): {}",
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
    if recipe.packages.aur_mode == "vendored" {
        install_vendored_aur(recipe)?;
    } else if offline {
        output::warn(&format!(
            "offline: skipping {} AUR packages (need network): {}",
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
    output::info(&format!("Installing {} AUR packages (needs network) ...", aur.len()));
    let helper = match system::aur_helper() {
        Some(h) => h,
        None => {
            output::warn(&format!("no AUR helper; skipping AUR list: {}", aur.join(" ")));
            return Ok(());
        }
    };
    let mut args = vec!["-S", "--noconfirm", "--needed", "--"];
    args.extend(aur.iter().copied());
    if !first_boot {
        let spin = output::spinner::spin("installing AUR packages");
        let r = system::run(helper, &args)?;
        if r.status != 0 {
            spin.fail("some AUR packages failed");
            output::warn("some AUR packages failed (see above)");
        } else {
            spin.succeed(None);
        }
        return Ok(());
    }
    let spin = output::spinner::spin("installing AUR packages (waiting for network)");
    let attempts = std::env::var("OMARCHY_RECIPE_NET_RETRIES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(120);
    for i in 1..=attempts {
        spin.set_detail(&format!("AUR install attempt {i}/{attempts}"));
        let r = system::run(helper, &args)?;
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
    let status = if system::cmd_exists("omarchy-pkg-add") {
        system::run("omarchy-pkg-add", pkgs)?.status
    } else {
        let mut args: Vec<&str> = vec!["-S", "--noconfirm", "--needed", "--"];
        args.extend(pkgs.iter().copied());
        let r = if is_root() {
            system::run("pacman", &args)?
        } else {
            let mut sudo_args = vec!["pacman"];
            sudo_args.extend(args);
            system::run("sudo", &sudo_args)?
        };
        r.status
    };
    if status != 0 {
        spin.fail("some repo packages failed");
        output::warn("some repo packages failed (see above)");
    } else {
        spin.succeed(None);
    }
    Ok(())
}

/// Install vendored AUR .pkgs fully offline via pacman -U.
fn install_vendored_aur(recipe: &Recipe) -> Result<(), String> {
    if recipe.aur_vendored.is_empty() {
        output::warn("aur_mode=vendored but bundle has no aur-pkgs/; skipping AUR");
        return Ok(());
    }
    output::info(&format!(
        "Installing {} vendored AUR packages (offline) ...",
        recipe.aur_vendored.len()
    ));
    // Resolve bundle-relative files. Store bundle dir in env for first-boot.
    let bundle_dir = std::env::var("OMARCHY_RECIPE_BUNDLE").unwrap_or_else(|_| ".".into());
    let bundle = PathBuf::from(bundle_dir);
    let mut files: Vec<String> = vec![];
    for v in &recipe.aur_vendored {
        let p = bundle.join(&v.file);
        if p.is_file() {
            files.push(p.to_string_lossy().to_string());
        } else {
            output::warn(&format!("vendored package missing: {} (skipping {})", p.display(), v.name));
        }
    }
    if files.is_empty() {
        output::warn("no vendored AUR files found; skipping");
        return Ok(());
    }
    let mut args: Vec<&str> = vec!["-U", "--noconfirm", "--needed", "--"];
    let owned = files.clone();
    for f in &owned {
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
        output::warn("some vendored AUR packages failed (deps may be missing; see above)");
    }
    Ok(())
}

fn restore_dotfiles(
    bundle: &Path,
    recipe: &Recipe,
    home: &Path,
    as_user: Option<&str>,
) -> Result<(), String> {
    let tarball = crate::recipe::tarball_path(bundle, recipe);
    if !tarball.is_file() {
        return Err(format!("dotfiles tarball not found: {}", tarball.display()));
    }
    let config = home.join(".config");
    if config.is_dir() {
        let ts = system::run("date", &["+%Y%m%d-%H%M%S"])?
            .stdout
            .trim()
            .to_string();
        let backup = home.join(format!(".config.pre-recipe-{ts}"));
        output::info(&format!("Backing up {} -> {}", config.display(), backup.display()));
        copy_dir(&config, &backup)?;
        if let Some(u) = as_user {
            let _ = system::run("chown", &["-R", &format!("{u}:{u}"), &backup.to_string_lossy()]);
        }
    }
    let stage = std::env::temp_dir().join(format!("omarchy-recipe-stage-{}", std::process::id()));
    std::fs::create_dir_all(&stage).map_err(|e| format!("cannot create stage: {e}"))?;
    let extract_spin = output::spinner::spin("extracting dotfiles");
    let r = system::run_in(
        &stage,
        "tar",
        &["--zstd", "-xf", &tarball.to_string_lossy()],
    )?;
    // Fall back to auto-detect if zstd flag unsupported.
    if r.status != 0 {
        let r2 = system::run_in(&stage, "tar", &["-xf", &tarball.to_string_lossy()])?;
        if r2.status != 0 {
            extract_spin.fail("dotfiles extraction failed");
            return Err(format!("cannot extract tarball: {}", r2.stderr.trim()));
        }
    }
    extract_spin.succeed(None);

    // Never clobber this machine's monitor layout with machine A's.
    for mon in ["hypr/monitors.conf", "hypr/monitors"] {
        if home.join(".config").join(mon).exists() {
            let staged = stage.join(".config").join(mon);
            if staged.exists() {
                if staged.is_dir() {
                    std::fs::remove_dir_all(&staged).ok();
                } else {
                    std::fs::remove_file(&staged).ok();
                }
                output::warn("keeping this machine's hypr monitor config; recipe's was skipped");
            }
            break;
        }
    }

    output::info("Restoring dotfiles ...");
    // rsync --backup keeps per-file pre-recipe copies; cp fallback otherwise.
    let stage_s = format!("{}/", stage.display());
    let home_s = home.to_string_lossy().to_string();
    let rsync_spin = output::spinner::spin("restoring dotfiles");
    if system::cmd_exists("rsync") {
        let ts = system::run("date", &["+%Y%m%d-%H%M%S"])?.stdout.trim().to_string();
        let r = system::run(
            "rsync",
            &["-a", &format!("--backup"), &format!("--suffix=.pre-recipe-{ts}"), &stage_s, &home_s],
        )?;
        if r.status != 0 {
            rsync_spin.fail("dotfiles restore failed");
            return Err(format!("rsync failed: {}", r.stderr.trim()));
        }
    } else {
        copy_stage(&stage, home)?;
    }
    rsync_spin.succeed(None);
    if let Some(u) = as_user {
        // Everything we placed must belong to the owner, not root.
        let _ = system::run("chown", &["-R", &format!("{u}:{u}"), &home_s]);
        let _ = system::run("chmod", &["755", &home_s]);
    }
    std::fs::remove_dir_all(&stage).ok();
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
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    // cp -a via system cp (preserves everything, no walk crate needed).
    let r = system::run("cp", &["-a", &src.to_string_lossy(), &dst.to_string_lossy()])?;
    if r.status != 0 {
        return Err(format!("backup failed: {}", r.stderr.trim()));
    }
    Ok(())
}

fn copy_stage(stage: &Path, home: &Path) -> Result<(), String> {
    let r = system::run(
        "cp",
        &["-a", &format!("{}/.", stage.display()), &home.to_string_lossy()],
    )?;
    if r.status != 0 {
        return Err(format!("dotfiles copy failed: {}", r.stderr.trim()));
    }
    Ok(())
}

fn install_themes(recipe: &Recipe, home: &Path, as_user: Option<&str>, offline: bool) -> Result<(), String> {
    let Some(themes) = &recipe.themes else { return Ok(()) };
    let want = themes.current.clone().unwrap_or_default();
    if want.is_empty() || want == "unknown" {
        return Ok(());
    }
    for t in themes.installed.iter().filter(|t| t.source == "custom") {
        let Some(url) = t.url.clone() else { continue };
        let dest = home.join(".config/omarchy/themes").join(&t.name);
        if dest.is_dir() {
            continue;
        }
        if offline {
            output::warn(&format!("offline: cannot fetch custom theme '{}' from {url}", t.name));
            continue;
        }
        output::info(&format!("Installing theme {} from {url} ...", t.name));
        if system::cmd_exists("omarchy-theme-install") {
            let r = run_as(home, as_user, "omarchy-theme-install", &[&url])?;
            if r.status != 0 {
                output::warn(&format!("theme install failed: {}", t.name));
            }
        } else if system::cmd_exists("git") {
            std::fs::create_dir_all(dest.parent().unwrap()).ok();
            let r = system::run("git", &["clone", "--", &url, &dest.to_string_lossy()])?;
            if r.status != 0 {
                output::warn(&format!("theme clone failed: {}", t.name));
            }
        }
    }
    if system::cmd_exists("omarchy-theme-set") {
        output::info(&format!("Setting theme: {want}"));
        let r = run_as(home, as_user, "omarchy-theme-set", &[&want])?;
        if r.status != 0 {
            output::warn("theme set failed (continuing)");
        }
    }
    Ok(())
}

fn apply_font(recipe: &Recipe, home: &Path, as_user: Option<&str>) -> Result<(), String> {
    let want = recipe.font.as_ref().and_then(|f| f.monospace.clone()).unwrap_or_default();
    if want.is_empty() || want == "unknown" || !system::cmd_exists("omarchy-font-set") {
        return Ok(());
    }
    let have = run_as(home, as_user, "omarchy-font-current", &[])
        .map(|r| r.stdout.trim().to_string())
        .unwrap_or_default();
    if have != want {
        output::info(&format!("Setting font: {want}"));
        let r = run_as(home, as_user, "omarchy-font-set", &[&want])?;
        if r.status != 0 {
            output::warn("font set failed (font may come from a skipped package)");
        }
    }
    Ok(())
}

fn apply_plugins(recipe: &Recipe, home: &Path, as_user: Option<&str>) -> Result<(), String> {
    if !system::cmd_exists("omarchy-plugin-enable") || !system::cmd_exists("omarchy-plugin-disable") {
        return Ok(());
    }
    for p in &recipe.plugins {
        if p.enabled {
            if !p.first_party {
                output::warn(&format!(
                    "third-party plugin '{}' cannot auto-install in V2; skipping",
                    p.id
                ));
                continue;
            }
            let _ = run_as(home, as_user, "omarchy-plugin-enable", &[&p.id]);
        } else {
            let _ = run_as(home, as_user, "omarchy-plugin-disable", &[&p.id]);
        }
    }
    Ok(())
}

fn apply_services(recipe: &Recipe, as_user: Option<&str>) -> Result<(), String> {
    let Some(svcs) = &recipe.services else { return Ok(()) };
    if !system::cmd_exists("systemctl") {
        return Ok(());
    }
    // User services: enable via wants-symlinks so no running user manager is needed
    // (works on first boot before first login).
    let home = system::home_dir().unwrap_or_else(|| PathBuf::from("/root"));
    for svc in &svcs.user {
        let allowed =
            svc.starts_with("omarchy-") || svc == "owed.service" || svc.starts_with("xdg-user-dirs");
        if !allowed {
            output::warn(&format!("skipping non-allowlisted user service: {svc}"));
            continue;
        }
        let unit_file = if svc.contains('@') { svc.clone() } else { svc.clone() };
        let src = PathBuf::from("/usr/lib/systemd/user").join(&unit_file);
        if !src.exists() {
            // Unit may be user-installed; fall back to systemctl --user (best effort).
            let _ = match as_user {
                Some(u) => system::run("runuser", &["-u", u, "--", "systemctl", "--user", "enable", svc]),
                None => system::run("systemctl", &["--user", "enable", svc]),
            };
            continue;
        }
        let wants = home.join(".config/systemd/user/default.target.wants");
        std::fs::create_dir_all(&wants).ok();
        std::os::unix::fs::symlink(&src, wants.join(svc)).ok();
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
        let r = if is_root() {
            system::run("systemctl", &["enable", svc])?
        } else {
            system::run("sudo", &["systemctl", "enable", svc])?
        };
        if r.status != 0 {
            output::warn(&format!("system service enable failed: {svc}"));
        }
    }
    Ok(())
}

// Minimal euid check without a libc dependency.
fn is_root() -> bool {
    // SAFETY: geteuid is infallible.
    unsafe { geteuid() == 0 }
}
extern "C" {
    fn geteuid() -> u32;
}
