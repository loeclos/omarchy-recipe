//! AUR handling: vendoring prebuilt packages for fully-offline bundles/ISOs.

use std::path::{Path, PathBuf};

use crate::output;
use crate::recipe::VendoredPkg;
use crate::system;

/// Vendor AUR packages: build (or reuse) .pkg.tar.zst files into
/// `<bundle>/aur-pkgs/` + `aur.db`, returning the manifest entries.
///
/// Order of preference per package:
/// 1. `--aur-pkgdir` reuse: exact `<name>-*.pkg.tar.zst` already on disk.
/// 2. Fresh build: fetch PKGBUILD (`yay -G` / AUR git) + `makepkg --noconfirm -sf`.
pub fn vendor(
    pkgs: &[String],
    bundle: &Path,
    pkgdir: Option<&str>,
) -> Result<Vec<VendoredPkg>, String> {
    let dest = bundle.join("aur-pkgs");
    std::fs::create_dir_all(&dest)
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;

    let mut out = vec![];
    for pkg in pkgs {
        output::info(&format!("Vendoring AUR package: {pkg}"));
        let file = ensure_pkg(pkg, &dest, pkgdir)?;
        out.push(VendoredPkg {
            name: pkg.clone(),
            file: format!("aur-pkgs/{file}"),
        });
    }

    // Index the repo so pacman -U/-S can resolve the set (unsigned: local files).
    if system::cmd_exists("repo-add") {
        let db = dest.join("aur.db.tar.gz");
        let mut files: Vec<String> = out
            .iter()
            .map(|v| {
                dest.join(v.file.trim_start_matches("aur-pkgs/"))
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        files.sort();
        let db_str = db.to_string_lossy().to_string();
        let mut cmd_args: Vec<&str> = vec![&db_str];
        for f in &files {
            cmd_args.push(f.as_str());
        }
        let r = system::run_in(&dest, "repo-add", &cmd_args)?;
        if r.status != 0 {
            output::warn(&format!("repo-add failed: {}", r.stderr.trim()));
        }
    } else {
        output::warn("repo-add not found; vendored dir has no repo index (pacman -U still works)");
    }
    Ok(out)
}

fn ensure_pkg(pkg: &str, dest: &Path, pkgdir: Option<&str>) -> Result<String, String> {
    if let Some(dir) = pkgdir {
        if let Some(hit) = find_prebuilt(Path::new(dir), pkg) {
            let name = hit.file_name().unwrap().to_string_lossy().to_string();
            std::fs::copy(&hit, dest.join(&name))
                .map_err(|e| format!("cannot copy prebuilt {pkg}: {e}"))?;
            output::info(&format!("Reusing prebuilt {name}"));
            return Ok(name);
        }
        output::warn(&format!("no prebuilt file for {pkg} in {dir}; building"));
    }
    build_pkg(pkg, dest)
}

fn find_prebuilt(dir: &Path, pkg: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut hits: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().map(|e| e == "zst").unwrap_or(false)
                && p.file_name()
                    .map(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with(&format!("{pkg}-")) && n.contains(".pkg.tar.")
                    })
                    .unwrap_or(false)
        })
        .collect();
    hits.sort();
    hits.into_iter().next()
}

fn build_pkg(pkg: &str, dest: &Path) -> Result<String, String> {
    if is_root() {
        return Err(format!(
            "cannot build AUR package '{pkg}' as root (makepkg refuses). \
             Run export as your user, or pass --aur-pkgdir with prebuilt packages."
        ));
    }
    for tool in ["makepkg", "git"] {
        if !system::cmd_exists(tool) {
            return Err(format!("'{tool}' not found; needed to build AUR package '{pkg}'"));
        }
    }
    let work = std::env::temp_dir().join(format!("omr-aur-{}-{}", pkg, std::process::id()));
    let srcdir = work.join(pkg);
    std::fs::create_dir_all(&work).map_err(|e| format!("cannot create workdir: {e}"))?;

    // Fetch PKGBUILD: prefer yay -G, fall back to AUR git.
    let fetched = if system::cmd_exists("yay") {
        system::run_in(&work, "yay", &["-G", pkg]).map(|r| r.status == 0).unwrap_or(false)
    } else {
        false
    } || system::run_in(
        &work,
        "git",
        &["clone", &format!("https://aur.archlinux.org/{pkg}.git")],
    )
    .map(|r| r.status == 0)
    .unwrap_or(false);
    if !fetched || !srcdir.join("PKGBUILD").is_file() {
        std::fs::remove_dir_all(&work).ok();
        return Err(format!("cannot fetch AUR sources for '{pkg}'"));
    }

    output::info(&format!("Building {pkg} (makepkg -sf; sudo may prompt for deps) ..."));
    let build_spin = output::spinner::spin(&format!("building {pkg}"));
    let r = system::run_in(&srcdir, "makepkg", &["--noconfirm", "-sf"])?;
    if r.status != 0 {
        build_spin.fail(&format!("makepkg failed for {pkg}"));
        std::fs::remove_dir_all(&work).ok();
        return Err(format!("makepkg failed for '{pkg}': {}", tail(&r.stderr)));
    }
    build_spin.succeed(None);
    // Collect exactly this package's artifacts (split packages possible).
    let mut built: Vec<PathBuf> = std::fs::read_dir(&srcdir)
        .map_err(|e| format!("cannot list build dir: {e}"))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with(&format!("{pkg}-"))
                        && n.contains(".pkg.tar.")
                        && !n.ends_with(".sig")
                })
                .unwrap_or(false)
        })
        .collect();
    built.sort();
    if built.is_empty() {
        std::fs::remove_dir_all(&work).ok();
        return Err(format!("makepkg produced no package file for '{pkg}'"));
    }
    for b in &built {
        let name = b.file_name().unwrap().to_string_lossy().to_string();
        std::fs::copy(b, dest.join(&name)).map_err(|e| format!("cannot collect {name}: {e}"))?;
    }
    let first = built[0].file_name().unwrap().to_string_lossy().to_string();
    std::fs::remove_dir_all(&work).ok();
    Ok(first)
}

fn tail(s: &str) -> String {
    s.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
}

fn is_root() -> bool {
    unsafe { geteuid() == 0 }
}
extern "C" {
    fn geteuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_prebuilt_matches_name_prefix() {
        let dir = std::env::temp_dir().join("omr-aur-find");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("foo-1.0-1-x86_64.pkg.tar.zst"), b"x").unwrap();
        std::fs::write(dir.join("foobar-2.0-1-x86_64.pkg.tar.zst"), b"x").unwrap();
        let hit = find_prebuilt(&dir, "foo").unwrap();
        assert!(hit.ends_with("foo-1.0-1-x86_64.pkg.tar.zst"));
    }

    #[test]
    fn vendor_reuses_pkgdir_without_building() {
        let dir = std::env::temp_dir().join("omr-aur-reuse");
        let pkgdir = dir.join("prebuilt");
        let bundle = dir.join("bundle");
        std::fs::create_dir_all(&pkgdir).unwrap();
        std::fs::write(pkgdir.join("foo-1.0-1-x86_64.pkg.tar.zst"), b"fake").unwrap();
        let out = vendor(&["foo".to_string()], &bundle, Some(pkgdir.to_str().unwrap())).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file, "aur-pkgs/foo-1.0-1-x86_64.pkg.tar.zst");
        assert!(bundle.join(&out[0].file).is_file());
    }
}
