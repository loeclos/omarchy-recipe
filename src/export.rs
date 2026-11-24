//! `export`: capture this machine into a .recipe bundle.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::cli::AurMode;
use crate::output;
use crate::recipe::*;
use crate::system;

/// Always filtered: caches, regenerable bulk, machine-specific monitor layout.
const BASE_BLOCKLIST: &[&str] = &[
    ".config/hypr/monitors.conf",
    ".config/hypr/monitors/*.conf",
    "*/Cache/*",
    "*/CachedData/*",
    "*/GPUCache/*",
    "*/node_modules/*", // regenerable via npm; dwarfs the bundle otherwise
    ".config/google-chrome*",
    ".config/google-chrome-beta*",
    ".config/google-chrome-unstable*",
    ".config/BraveSoftware*",
    ".config/chromium*",
    ".config/obsidian/*/Cache*",
    ".config/yay/*",
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
        excludes.push(".config/omarchy/themes/*".into());
    }
    if skipped(without, "backgrounds") {
        excludes.push("*/backgrounds/*".into());
    }
    if skipped(without, "webapps") {
        excludes.push(".local/share/applications/*.desktop".into());
    }
    if skipped(without, "icons") {
        excludes.push(".local/share/icons/hicolor/256x256/apps/*".into());
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
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;

    let vendor_planned =
        matches!(opts.aur_mode, AurMode::Vendored) && !skipped(&opts.without, "aur");
    let total = if vendor_planned { 5 } else { 4 };

    output::stage(
        1,
        total,
        &format!("Detecting system (Omarchy {version}, channel {channel})"),
    );
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
        system::explicit_repo_pkgs(&omarchy_root)?
    };
    let aur = if skipped(&opts.without, "aur") {
        vec![]
    } else {
        system::aur_pkgs()?
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
        collect_plugins(&home)?
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
    let shell_sha = std::fs::read(home.join(".config/omarchy/shell.json"))
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
    // A previous export into the same directory must not leave vendored
    // packages behind when this export uses wifi mode (or a smaller AUR set).
    let vendored_dir = out.join("aur-pkgs");
    if vendored_dir.exists() {
        std::fs::remove_dir_all(&vendored_dir)
            .map_err(|e| format!("cannot clear stale {}: {e}", vendored_dir.display()))?;
    }

    let excludes = build_excludes(&opts.without, &opts.extra_excludes, opts.include_secrets);
    let use_zstd = tar_supports_zstd()?;
    let tarball_name = if use_zstd {
        "dotfiles.tar.zst"
    } else {
        "dotfiles.tar.gz"
    };
    let tarball = out.join(tarball_name);
    let tar_spin = output::spinner::spin("compressing dotfiles");
    if let Err(e) = archive_dotfiles(
        &home,
        &tarball,
        &excludes,
        skipped(&opts.without, "webapps"),
        skipped(&opts.without, "icons"),
        opts.include_secrets,
        use_zstd,
    ) {
        tar_spin.fail("dotfiles archive failed");
        return Err(e);
    }
    tar_spin.succeed(None);
    // Remove the old compression format after a successful replacement.
    let stale_name = if use_zstd {
        "dotfiles.tar.gz"
    } else {
        "dotfiles.tar.zst"
    };
    let stale = out.join(stale_name);
    if stale.exists() {
        let _ = std::fs::remove_file(stale);
    }
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
            output::info(
                "AUR vendoring requested but no AUR packages installed; nothing to vendor",
            );
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

    let mut excluded_sections: Vec<String> = opts.without.iter().cloned().collect();
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
        themes: if skipped(&opts.without, "themes") {
            None
        } else {
            Some(Themes {
                current: theme_current,
                installed,
                background: Some(Background {
                    current: bg_current,
                    bundled_in_dotfiles: !skipped(&opts.without, "backgrounds"),
                }),
            })
        },
        font: if skipped(&opts.without, "font") {
            None
        } else {
            Some(Font { monospace: font })
        },
        plugins,
        services: if skipped(&opts.without, "services") {
            None
        } else {
            Some(services)
        },
        webapps,
        shell: Some(Shell {
            bar_layout_sha256: shell_sha,
            restored_via_dotfiles: true,
        }),
        dotfiles: Dotfiles {
            file: tarball_name.into(),
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

    let json_size = std::fs::metadata(out.join("recipe.json"))
        .map(|m| m.len())
        .unwrap_or(0);
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
                format!("{} / {}", recipe.omarchy.version, recipe.omarchy.channel),
            ),
            (
                "packages",
                format!(
                    "{} repo, {} AUR ({})",
                    output::num(recipe.packages.explicit_repo.len()),
                    output::num(recipe.packages.aur.len()),
                    recipe.packages.aur_mode
                ),
            ),
            (
                "theme",
                recipe
                    .themes
                    .as_ref()
                    .and_then(|t| t.current.clone())
                    .unwrap_or_else(|| "—".into()),
            ),
            ("dotfiles", human_size(tar_size)),
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
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn collect_themes(
    home: &Path,
    omarchy_root: &Path,
) -> (Option<String>, Vec<ThemeEntry>, Option<String>) {
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
            let source = if bundled && url.is_none() {
                "bundled"
            } else {
                "custom"
            };
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
                let first = r
                    .stdout
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim();
                if !first.is_empty() {
                    return Some(first.to_string());
                }
            }
        }
    }
    None
}

fn collect_plugins(home: &Path) -> Result<Vec<PluginEntry>, String> {
    if !system::cmd_exists("omarchy-plugin-list") {
        return Err("omarchy-plugin-list not found; cannot record plugin state".into());
    }
    let r = system::run("omarchy-plugin-list", &["--json"])?;
    if r.status != 0 {
        return Err(format!(
            "omarchy-plugin-list --json failed: {}",
            r.stderr.trim()
        ));
    }
    let v: serde_json::Value = serde_json::from_str(&r.stdout)
        .map_err(|e| format!("invalid JSON from omarchy-plugin-list: {e}"))?;
    let mut plugins = parse_plugins_value(v)?;
    for plugin in &mut plugins {
        if !plugin.first_party && plugin.url.is_none() {
            // Current omarchy-plugin-list uses clonedFrom for first-party
            // identity, not provenance. Third-party installs are git clones,
            // so their origin remote is the reproducible source URL.
            plugin.url = plugin_source_url(home, &plugin.id);
        }
    }
    Ok(plugins)
}

fn plugin_source_url(home: &Path, id: &str) -> Option<String> {
    system::git_remote(&home.join(".config/omarchy/plugins").join(id))
}

fn parse_plugins_value(v: serde_json::Value) -> Result<Vec<PluginEntry>, String> {
    let arr = v
        .as_array()
        .ok_or_else(|| "omarchy-plugin-list --json did not return an array".to_string())?;
    Ok(arr
        .iter()
        .filter_map(|p| {
            Some(PluginEntry {
                id: p.get("id")?.as_str()?.to_string(),
                enabled: p.get("enabled").and_then(|b| b.as_bool()).unwrap_or(false),
                first_party: p
                    .get("firstParty")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(true),
                url: p
                    .get("url")
                    .and_then(|u| u.as_str())
                    .filter(|url| !url.is_empty())
                    .map(|s| s.to_string()),
            })
        })
        .collect())
}

fn collect_services() -> Services {
    let mut user = vec![];
    let mut system_svcs = vec![];
    if system::cmd_exists("systemctl") {
        if let Ok(r) = system::run(
            "systemctl",
            &[
                "--user",
                "list-unit-files",
                "--state=enabled",
                "--no-legend",
            ],
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
            } else {
                output::warn(&format!(
                    "could not list user services: {}",
                    r.stderr.trim()
                ));
            }
        } else {
            output::warn("could not run systemctl --user; recording no user services");
        }
        match system::run(
            "systemctl",
            &["list-unit-files", "--state=enabled", "--no-legend"],
        ) {
            Ok(r) if r.status == 0 => {
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
            Ok(r) => output::warn(&format!(
                "could not list system services: {}",
                r.stderr.trim()
            )),
            Err(e) => output::warn(&format!("could not run systemctl: {e}")),
        }
    }
    user.sort();
    system_svcs.sort();
    Services {
        user,
        system: system_svcs,
    }
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
        let base = f
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
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

fn tar_supports_zstd() -> Result<bool, String> {
    if !system::cmd_exists("tar") {
        return Err("tar not found".into());
    }
    let probe = system::run(
        "tar",
        &["--zstd", "-cf", "/dev/null", "--files-from", "/dev/null"],
    )?;
    Ok(probe.status == 0)
}

/// Create dotfiles.tar.zst (or .tar.gz on older tar) from $HOME in one pass.
/// A single invocation is important: tar cannot append members to an already
/// compressed archive, which used to drop web-app icons.
fn archive_dotfiles(
    home: &Path,
    tarball: &Path,
    excludes: &[String],
    skip_webapps: bool,
    skip_icons: bool,
    include_secrets: bool,
    use_zstd: bool,
) -> Result<(), String> {
    let tmp_name = format!(
        ".{}.tmp-{}",
        tarball.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    );
    let tmp = tarball.with_file_name(tmp_name);
    let _ = std::fs::remove_file(&tmp);

    let mut args: Vec<String> = vec![];
    args.push(if use_zstd { "--zstd" } else { "-z" }.into());
    args.push("-cf".into());
    args.push(tmp.to_string_lossy().to_string());
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
    // V2 ships webapp launchers and their icons in the same archive.
    let apps = home.join(".local/share/applications");
    if !skip_webapps && apps.is_dir() {
        args.push(".local/share/applications".into());
    }
    let icons = home.join(".local/share/icons/hicolor/256x256/apps");
    if !skip_icons && icons.is_dir() {
        args.push(".local/share/icons/hicolor/256x256/apps".into());
    }

    output::info("Archiving ~/.config (filtered) ...");
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let result = system::run("tar", &arg_refs);
    match result {
        Ok(r) if r.status == 0 => {}
        Ok(r) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("tar failed: {}", r.stderr.trim()));
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    std::fs::rename(&tmp, tarball)
        .map_err(|e| format!("cannot finalize {}: {e}", tarball.display()))?;
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
        assert!(ex.contains(&".config/hypr/monitors.conf".to_string()));
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
            ".config/hypr/monitors.conf",
            "*/Cache/*",
            "*/node_modules/*",
            ".config/yay/*",
        ] {
            assert!(ex.contains(&pat.to_string()), "lost hygiene: {pat}");
        }
    }

    #[test]
    fn without_sections_still_apply_with_secrets() {
        let ex = build_excludes(
            &without(&["webapps", "backgrounds", "icons"]),
            &["*/custom/*".into()],
            true,
        );
        assert!(ex.contains(&".local/share/applications/*.desktop".to_string()));
        assert!(ex.contains(&"*/backgrounds/*".to_string()));
        assert!(ex.contains(&".local/share/icons/hicolor/256x256/apps/*".to_string()));
        assert!(ex.contains(&"*/custom/*".to_string()));
    }

    #[test]
    fn plugin_parser_ignores_cloned_from_identity() {
        let plugins = parse_plugins_value(serde_json::json!([{
            "id": "acme.weather",
            "enabled": true,
            "firstParty": false,
            "clonedFrom": "omarchy.clock"
        }]))
        .unwrap();
        assert_eq!(plugins.len(), 1);
        assert!(plugins[0].url.is_none());
    }

    #[test]
    fn third_party_plugin_url_comes_from_git_remote() {
        if !system::cmd_exists("git") {
            return;
        }
        let home = system::temp_dir("omr-plugin-url").unwrap();
        let plugin = home.join(".config/omarchy/plugins/acme.weather");
        std::fs::create_dir_all(&plugin).unwrap();
        assert!(
            system::run_in(&plugin, "git", &["init", "-q"])
                .unwrap()
                .status
                == 0
        );
        assert!(
            system::run_in(
                &plugin,
                "git",
                &["remote", "add", "origin", "https://example.com/weather.git"]
            )
            .unwrap()
            .status
                == 0
        );
        assert_eq!(
            plugin_source_url(&home, "acme.weather").as_deref(),
            Some("https://example.com/weather.git")
        );
    }

    #[test]
    fn archive_gzip_fallback_is_real_gzip() {
        let dir = system::temp_dir("omr-archive-gzip").unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".config")).unwrap();
        std::fs::write(home.join(".config/file"), b"test").unwrap();
        let tarball = dir.join("dotfiles.tar.gz");
        archive_dotfiles(
            &home,
            &tarball,
            &build_excludes(&HashSet::new(), &[], false),
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let listing = system::run("tar", &["-tzf", &tarball.to_string_lossy()]).unwrap();
        assert!(listing.status == 0, "{}", listing.stderr);
        assert!(listing.stdout.contains(".config/file"));
    }

    #[test]
    fn archive_filters_real_config_paths_and_includes_icons() {
        if !tar_supports_zstd().unwrap() {
            return;
        }
        let dir = system::temp_dir("omr-archive-test").unwrap();
        let home = dir.join("home");
        for path in [
            ".config/hypr/monitors.conf",
            ".config/yay/cache-file",
            ".config/google-chrome/Default/Cache/data",
            ".config/nvim/init.vim",
            ".local/share/applications/test.desktop",
            ".local/share/icons/hicolor/256x256/apps/test.png",
        ] {
            let file = home.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, b"test").unwrap();
        }
        let tarball = dir.join("dotfiles.tar.zst");
        let excludes = build_excludes(&HashSet::new(), &[], false);
        archive_dotfiles(&home, &tarball, &excludes, false, false, false, true).unwrap();
        let listing = system::run("tar", &["--zstd", "-tf", &tarball.to_string_lossy()]).unwrap();
        assert!(listing.status == 0, "{}", listing.stderr);
        assert!(!listing.stdout.contains("hypr/monitors.conf"));
        assert!(!listing.stdout.contains("yay/cache-file"));
        assert!(!listing.stdout.contains("google-chrome"));
        assert!(listing.stdout.contains("nvim/init.vim"));
        assert!(listing.stdout.contains("test.desktop"));
        assert!(listing
            .stdout
            .contains("icons/hicolor/256x256/apps/test.png"));
    }
}
