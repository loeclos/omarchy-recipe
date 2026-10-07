//! `export`: capture this machine into a .recipe bundle.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::cli::AurMode;
use crate::output;
use crate::recipe::*;
use crate::system;

/// Always filtered: caches, regenerable bulk, machine-specific monitor layout.
const BASE_BLOCKLIST: &[&str] = &[
    "hypr/monitors.conf",
    "hypr/monitors/*.conf",
    "*/Cache/*",
    "*/CachedData/*",
    "*/GPUCache/*",
    "*/node_modules/*", // regenerable via npm; dwarfs the bundle otherwise
    "google-chrome*",
    "google-chrome-beta*",
    "google-chrome-unstable*",
    "BraveSoftware*",
    "chromium*",
    "obsidian/*/Cache*",
    "yay/*",
];

/// Filtered by default; kept only with --include-secrets (company cloning).
const SECRET_BLOCKLIST: &[&str] = &[".ssh", ".gnupg", ".pki", "*secret*", "*token*"];

pub struct ExportOptions {
    pub out: Option<String>,
    pub without: HashSet<String>,
    pub extra_excludes: Vec<String>,
    pub aur_mode: AurMode,
    pub aur_pkgdir: Option<String>,
    pub include_secrets: bool,
}

/// Effective tar --exclude list, split out for testing.
fn build_excludes(
    without: &HashSet<String>,
    extra_excludes: &[String],
    include_secrets: bool,
) -> Vec<String> {
    let mut excludes: Vec<String> = BASE_BLOCKLIST.iter().map(|s| s.to_string()).collect();
    if !include_secrets {
        excludes.extend(SECRET_BLOCKLIST.iter().map(|s| s.to_string()));
    }
    if skipped(without, "themes") {
        excludes.push("omarchy/themes/*".into());
    }
    if skipped(without, "backgrounds") {
        excludes.push("*/backgrounds/*".into());
    }
    if skipped(without, "webapps") {
        excludes.push(".local/share/applications/*.desktop".into());
    }
    excludes.extend(extra_excludes.iter().cloned());
    excludes
}

fn skipped(without: &HashSet<String>, section: &str) -> bool {
    without.contains(section)
}

pub fn run(opts: ExportOptions) -> Result<(), String> {
    for s in &opts.without {
        if !known_sections().contains(s.as_str()) {
            return Err(format!(
                "unknown section '{s}' (known: packages,aur,themes,font,plugins,services,webapps,backgrounds,icons)"
            ));
        }
    }

    let home = system::home_dir().ok_or("cannot determine $HOME")?;
    let omarchy_root = system::find_omarchy_path()?;
    let version = system::omarchy_version(&omarchy_root);
    let channel = system::omarchy_channel();
    let git_ref = system::git_head(&omarchy_root).unwrap_or_else(|| "unknown".into());
    let remote = system::git_remote(&omarchy_root).unwrap_or_else(|| "unknown".into());
    let now = system::utc_now();

    let out = match opts.out {
        Some(o) => PathBuf::from(o),
        None => PathBuf::from(format!("./{}.recipe", system::hostname())),
    };
    std::fs::create_dir_all(&out)
        .map_err(|e| format!("cannot create {}: {e}", out.display()))?;

    let vendor_planned =
        matches!(opts.aur_mode, AurMode::Vendored) && !skipped(&opts.without, "aur");
    let total = if vendor_planned { 5 } else { 4 };

    output::stage(1, total, &format!(
        "Detecting system (Omarchy {version}, channel {channel})"
    ));
    if !opts.without.is_empty() {
        let mut w: Vec<&String> = opts.without.iter().collect();
        w.sort();
        output::info(&format!(
            "Skipping sections: {}",
            w.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }

    // --- packages ---
    output::stage(2, total, "Collecting packages, themes, services");
    let explicit_repo = if skipped(&opts.without, "packages") {
        vec![]
    } else {
        system::explicit_repo_pkgs(&omarchy_root)
    };
    let aur = if skipped(&opts.without, "aur") {
        vec![]
    } else {
        system::aur_pkgs()
    };

    // --- themes ---
    let (theme_current, installed, bg_current) = if skipped(&opts.without, "themes") {
        (None, vec![], None)
    } else {
        collect_themes(&home, &omarchy_root)
    };
    let bg_current = if skipped(&opts.without, "backgrounds") {
        None
    } else {
        bg_current
    };

    // --- font ---
    let font: Option<String> = if skipped(&opts.without, "font") {
        None
    } else {
        current_font()
    };

    // --- plugins ---
    let plugins = if skipped(&opts.without, "plugins") {
        vec![]
    } else {
        collect_plugins()
    };

    // --- services ---
    let services = if skipped(&opts.without, "services") {
        Services::default()
    } else {
        collect_services()
    };

    // --- webapps ---
    let webapps = if skipped(&opts.without, "webapps") {
        vec![]
    } else {
        collect_webapps(&home)
    };

    // --- shell hash ---
    let shell_sha = std::fs::read(&home.join(".config/omarchy/shell.json"))
        .ok()
        .map(|b| sha256_bytes(&b));

    // --- tarball ---
    output::stage(3, total, "Archiving dotfiles");
    if opts.include_secrets {
        output::warn(
            "including secrets (.ssh, .gnupg, .pki, *secret*, *token*): \
             anyone with this bundle owns these credentials — share it only over trusted channels",
        );
    }
    let excludes = build_excludes(&opts.without, &opts.extra_excludes, opts.include_secrets);
    let tarball = out.join("dotfiles.tar.zst");
    let tar_spin = output::spinner::spin("compressing dotfiles");
    if let Err(e) = archive_dotfiles(
        &home,
        &tarball,
        &excludes,
        skipped(&opts.without, "icons"),
        opts.include_secrets,
    ) {
        tar_spin.fail("dotfiles archive failed");
        return Err(e);
    }
    tar_spin.succeed(None);
    let tarball_sha = sha256_file(&tarball)?;

    // --- AUR vendoring ---
    let aur_mode_str = match opts.aur_mode {
        AurMode::Wifi => "wifi",
        AurMode::Vendored => "vendored",
    };
    let mut aur_vendored: Vec<VendoredPkg> = vec![];
    if vendor_planned {
        output::stage(4, total, "Vendoring AUR packages");
        if aur.is_empty() {
            output::info("AUR vendoring requested but no AUR packages installed; nothing to vendor");
        } else {
            let vend_spin = output::spinner::spin("building AUR packages");
            match crate::aur::vendor(&aur, &out, opts.aur_pkgdir.as_deref()) {
                Ok(v) => {
                    vend_spin.succeed(None);
                    aur_vendored = v;
                }
                Err(e) => {
                    vend_spin.fail("AUR vendoring failed");
                    return Err(e);
                }
            }
        }
    }

    let mut excluded_sections: Vec<String> = opts.without.into_iter().collect();
    excluded_sections.sort();

    let mut includes = vec![
        "~/.config (filtered)".into(),
        "~/.local/share/applications/*.desktop".into(),
        "~/.local/share/icons/hicolor/256x256/apps".into(),
    ];
    if opts.include_secrets {
        includes.push("~/.ssh, ~/.gnupg, ~/.pki (secrets)".into());
    }

    let recipe = Recipe {
        schema_version: SCHEMA_VERSION,
        generated_by: format!("omarchy-recipe {}", TOOL_VERSION),
        generated_at: now,
        omarchy: OmarchyInfo {
            version,
            channel,
            r#ref: git_ref,
            remote,
        },
        packages: Packages {
            explicit_repo,
            aur,
            aur_mode: aur_mode_str.into(),
        },
        themes: Some(Themes {
            current: theme_current,
            installed,
            background: Some(Background {
                current: bg_current,
                bundled_in_dotfiles: true,
            }),
        }),
        font: Some(Font { monospace: font }),
        plugins,
        services: Some(services),
        webapps,
        shell: Some(Shell {
            bar_layout_sha256: shell_sha,
            restored_via_dotfiles: true,
        }),
        dotfiles: Dotfiles {
            file: "dotfiles.tar.zst".into(),
            sha256: tarball_sha,
            includes,
            excludes,
        },
        selection: Selection {
            excluded_sections,
            extra_excludes: opts.extra_excludes,
            include_secrets: opts.include_secrets,
        },
        aur_vendored,
    };

    let json = serde_json::to_string_pretty(&recipe).map_err(|e| format!("JSON error: {e}"))?;
    std::fs::write(out.join("recipe.json"), json + "\n")
        .map_err(|e| format!("cannot write recipe.json: {e}"))?;

    // Validate what we just wrote.
    output::stage(total, total, "Assembling recipe.json");
    crate::validate::validate_bundle(&out)?;

    let json_size = std::fs::metadata(out.join("recipe.json")).map(|m| m.len()).unwrap_or(0);
    let tar_size = std::fs::metadata(&tarball).map(|m| m.len()).unwrap_or(0);
    output::ok(&format!(
        "{} (recipe.json {}, dotfiles {})",
        out.display(),
        human_size(json_size),
        human_size(tar_size)
    ));
    output::summary(
        "Bundle ready",
        &[
            ("location", output::path(&out.display().to_string())),
            (
                "omarchy",
                format!("{} / {}", recipe.omarchy.version, recipe.omarchy.channel).into(),
            ),
            (
                "packages",
                format!(
                    "{} repo, {} AUR ({})",
                    output::num(recipe.packages.explicit_repo.len()),
                    output::num(recipe.packages.aur.len()),
                    recipe.packages.aur_mode
                )
                .into(),
            ),
            (
                "theme",
                recipe
                    .themes
                    .as_ref()
                    .and_then(|t| t.current.clone())
                    .unwrap_or_else(|| "—".into())
                    .into(),
            ),
            ("dotfiles", human_size(tar_size).into()),
        ],
    );
    output::info(&format!(
        "Share the '{}' dir (zip it) — import with: omarchy-recipe import {}",
        out.display(),
        out.display()
    ));
    Ok(())
}

fn sha256_bytes(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b);
    format!("{:x}", h.finalize())
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

fn theme_current_raw(home: &Path) -> Option<String> {
    let p = home.join(".local/state/omarchy/current/theme.name");
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn collect_themes(home: &Path, omarchy_root: &Path) -> (Option<String>, Vec<ThemeEntry>, Option<String>) {
    let current = theme_current_raw(home);
    let mut installed = vec![];
    let dir = home.join(".config/omarchy/themes");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            let tdir = dir.join(&name);
            let url = system::git_remote(&tdir);
            let r = system::git_head(&tdir);
            let bundled = omarchy_root.join("themes").join(&name).is_dir();
            let source = if bundled && url.is_none() { "bundled" } else { "custom" };
            if source == "custom" && url.is_none() {
                output::warn(&format!(
                    "theme '{name}' has no git remote; it will not auto-install on import"
                ));
            }
            installed.push(ThemeEntry {
                name,
                source: source.into(),
                url,
                r#ref: r,
            });
        }
    }
    let bg = home
        .join(".local/state/omarchy/current/background")
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
    (current, installed, bg)
}

fn current_font() -> Option<String> {
    if system::cmd_exists("omarchy-font-current") {
        if let Ok(r) = system::run("omarchy-font-current", &[]) {
            if r.status == 0 && !r.stdout.trim().is_empty() {
                return Some(r.stdout.trim().to_string());
            }
        }
    }
    if system::cmd_exists("fc-match") {
        if let Ok(r) = system::run("fc-match", &["monospace", "-f", "%{family}\\n"]) {
            if r.status == 0 {
                let first = r.stdout.lines().next().unwrap_or("").split(',').next().unwrap_or("").trim();
                if !first.is_empty() {
                    return Some(first.to_string());
                }
            }
        }
    }
    None
}

fn collect_plugins() -> Vec<PluginEntry> {
    if !system::cmd_exists("omarchy-plugin-list") {
        output::warn("omarchy-plugin-list not found; recording empty plugin list");
        return vec![];
    }
    let Ok(r) = system::run("omarchy-plugin-list", &["--json"]) else {
        return vec![];
    };
    if r.status != 0 {
        return vec![];
    }
    let v: serde_json::Value = match serde_json::from_str(&r.stdout) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    arr.iter()
        .filter_map(|p| {
            Some(PluginEntry {
                id: p.get("id")?.as_str()?.to_string(),
                enabled: p.get("enabled").and_then(|b| b.as_bool()).unwrap_or(false),
                first_party: p.get("firstParty").and_then(|b| b.as_bool()).unwrap_or(true),
                url: p.get("url").and_then(|u| u.as_str()).map(|s| s.to_string()),
            })
        })
        .collect()
}

fn collect_services() -> Services {
    let mut user = vec![];
    let mut system_svcs = vec![];
    if system::cmd_exists("systemctl") {
        if let Ok(r) = system::run(
            "systemctl",
            &["--user", "list-unit-files", "--state=enabled", "--no-legend"],
        ) {
            if r.status == 0 {
                for line in r.stdout.lines() {
                    let unit = line.split_whitespace().next().unwrap_or("");
                    if unit.starts_with("omarchy-")
                        || unit == "owed.service"
                        || unit.starts_with("xdg-user-dirs")
                    {
                        user.push(unit.to_string());
                    }
                }
            }
        }
        if let Ok(r) = system::run("systemctl", &["list-unit-files", "--state=enabled", "--no-legend"]) {
            if r.status == 0 {
                for line in r.stdout.lines() {
                    let unit = line.split_whitespace().next().unwrap_or("");
                    if unit.starts_with("tailscale")
                        || unit.starts_with("docker")
                        || unit.starts_with("ufw")
                        || unit.starts_with("cups")
                        || unit.starts_with("bluetooth")
                        || unit.starts_with("avahi-daemon")
                        || unit.starts_with("power-profiles-daemon")
                        || unit.starts_with("thermald")
                        || unit.starts_with("sddm")
                        || unit.starts_with("NetworkManager")
                    {
                        system_svcs.push(unit.to_string());
                    }
                }
            }
        }
    }
    user.sort();
    system_svcs.sort();
    Services { user, system: system_svcs }
}

fn collect_webapps(home: &Path) -> Vec<Webapp> {
    let mut out = vec![];
    let dir = home.join(".local/share/applications");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "desktop").unwrap_or(false))
        .collect();
    files.sort();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        let get = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{key}=")))
                .map(|s| s.to_string())
        };
        let base = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let name = get("Name").unwrap_or_else(|| base.trim_end_matches(".desktop").to_string());
        let exec = get("Exec").unwrap_or_default();
        let url = exec
            .split_whitespace()
            .find(|t| t.starts_with("http://") || t.starts_with("https://"))
            .map(|s| s.to_string());
        out.push(Webapp {
            name,
            url,
            icon: get("Icon"),
            desktop_file: Some(base),
        });
    }
    out
}

/// Create dotfiles.tar.zst from $HOME: .config (filtered) + webapp launchers + icons.
/// With include_secrets, top-level ~/.ssh, ~/.gnupg and ~/.pki ride along too
/// (they live outside .config, so lifting the excludes alone would miss them).
fn archive_dotfiles(
    home: &Path,
    tarball: &Path,
    excludes: &[String],
    skip_icons: bool,
    include_secrets: bool,
) -> Result<(), String> {
    if !system::cmd_exists("tar") {
        return Err("tar not found".into());
    }
    // Probe zstd support once.
    let probe = system::run("tar", &["--zstd", "-cf", "/dev/null", "--files-from", "/dev/null"])?;
    let use_zstd = probe.status == 0;
    let mut args: Vec<String> = vec![];
    if use_zstd {
        args.push("--zstd".into());
    } else {
        output::warn("tar lacks zstd support; falling back to gzip (.tar.zst will be gzip)");
    }
    args.push("-cf".into());
    args.push(tarball.to_string_lossy().to_string());
    for pat in excludes {
        args.push(format!("--exclude={pat}"));
    }
    args.push("--exclude=*.pre-recipe*".into());
    args.push("-C".into());
    args.push(home.to_string_lossy().to_string());
    args.push(".config".into());
    if include_secrets {
        for dir in [".ssh", ".gnupg", ".pki"] {
            if home.join(dir).exists() {
                args.push(dir.into());
            }
        }
    }
    // V2: ship webapp launchers so import restores them (tiny).
    let apps = home.join(".local/share/applications");
    let mut extra_staged = false;
    if apps.is_dir() {
        args.push(".local/share/applications".into());
        extra_staged = true;
    }
    let _ = extra_staged;
    output::info("Archiving ~/.config (filtered) ...");
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let r = system::run("tar", &arg_refs)?;
    if r.status != 0 {
        output::warn(&format!("tar reported issues: {}", r.stderr.trim()));
    }
    if !skip_icons {
        let icons = home.join(".local/share/icons/hicolor/256x256/apps");
        if icons.is_dir() {
            let mode = if use_zstd { "--zstd" } else { "-z" };
            let r2 = system::run(
                "tar",
                &[
                    mode,
                    "-rf",
                    &tarball.to_string_lossy(),
                    "-C",
                    &home.to_string_lossy(),
                    ".local/share/icons/hicolor/256x256/apps",
                ],
            )?;
            if r2.status != 0 {
                output::warn("could not append webapp icons; continuing");
            }
        }
    }
    if !tarball.is_file() {
        return Err("tarball was not created".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn without(sections: &[&str]) -> HashSet<String> {
        sections.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn secrets_filtered_by_default() {
        let ex = build_excludes(&without(&[]), &[], false);
        for pat in [".ssh", ".gnupg", ".pki", "*secret*", "*token*"] {
            assert!(ex.contains(&pat.to_string()), "missing {pat}");
        }
        // Non-secret hygiene stays regardless.
        assert!(ex.contains(&"hypr/monitors.conf".to_string()));
        assert!(ex.contains(&"*/Cache/*".to_string()));
    }

    #[test]
    fn include_secrets_lifts_only_secret_patterns() {
        let ex = build_excludes(&without(&[]), &[], true);
        for pat in [".ssh", ".gnupg", ".pki", "*secret*", "*token*"] {
            assert!(!ex.contains(&pat.to_string()), "still filtered: {pat}");
        }
        // Caches, machine layout, bulk stay filtered.
        for pat in [
            "hypr/monitors.conf",
            "*/Cache/*",
            "*/node_modules/*",
            "yay/*",
        ] {
            assert!(ex.contains(&pat.to_string()), "lost hygiene: {pat}");
        }
    }

    #[test]
    fn without_sections_still_apply_with_secrets() {
        let ex = build_excludes(&without(&["webapps", "backgrounds"]), &["*/custom/*".into()], true);
        assert!(ex.contains(&".local/share/applications/*.desktop".to_string()));
        assert!(ex.contains(&"*/backgrounds/*".to_string()));
        assert!(ex.contains(&"*/custom/*".to_string()));
    }
}
