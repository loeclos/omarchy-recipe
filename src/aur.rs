//! AUR handling: vendoring prebuilt packages for fully-offline bundles/ISOs.

use std::path::{Path, PathBuf};

use crate::output;
use crate::recipe::{sha256_file, VendoredPkg};
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
    std::fs::create_dir_all(&dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;

    let mut out = vec![];
    for pkg in pkgs {
        if !valid_pkg_name(pkg) {
            return Err(format!("invalid AUR package name: '{pkg}'"));
        }
        output::info(&format!("Vendoring AUR package: {pkg}"));
        let files = ensure_pkg(pkg, &dest, pkgdir)?;
        for file in files {
            let hash = sha256_file(&dest.join(&file))?;
            out.push(VendoredPkg {
                name: pkg.clone(),
                file: format!("aur-pkgs/{file}"),
                sha256: Some(hash),
            });
        }
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

pub(crate) fn valid_pkg_name(pkg: &str) -> bool {
    let mut bytes = pkg.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'@' | b'-'))
}

fn ensure_pkg(pkg: &str, dest: &Path, pkgdir: Option<&str>) -> Result<Vec<String>, String> {
    if let Some(dir) = pkgdir {
        let hits = find_prebuilt(Path::new(dir), pkg);
        if !hits.is_empty() {
            let mut names = vec![];
            for hit in hits {
                let name = hit
                    .file_name()
                    .ok_or_else(|| format!("prebuilt package has no filename: {}", hit.display()))?
                    .to_string_lossy()
                    .to_string();
                std::fs::copy(&hit, dest.join(&name))
                    .map_err(|e| format!("cannot copy prebuilt {pkg}: {e}"))?;
                output::info(&format!("Reusing prebuilt {name}"));
                names.push(name);
            }
            return Ok(names);
        }
        output::warn(&format!("no prebuilt file for {pkg} in {dir}; building"));
    }
    build_pkg(pkg, dest)
}

fn find_prebuilt(dir: &Path, pkg: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut hits: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().map(|e| e == "zst").unwrap_or(false)
                && p.file_name()
                    .map(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with(&format!("{pkg}-")) && n.contains(".pkg.tar.")
                    })
                    .unwrap_or(false)
        })
        .collect();
    hits.sort();
    // Keep every split-package artifact, but only the newest matching version
    // of each artifact name. Passing two versions of one package to pacman -U
    // would make the install fail.
    let mut newest: std::collections::HashMap<String, (String, PathBuf)> =
        std::collections::HashMap::new();
    for path in hits {
        let Some(file) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = file.strip_suffix(".pkg.tar.zst") else {
            continue;
        };
        let parts: Vec<&str> = stem.rsplitn(4, '-').collect();
        if parts.len() != 4 {
            continue;
        }
        let name = parts[3].to_string();
        let version = format!("{}-{}-{}", parts[2], parts[1], parts[0]);
        let current = newest.get(&name).map(|(version, _)| version.as_str());
        let newer = match current {
            Some(current) => system::run("vercmp", &[current, &version])
                .ok()
                .filter(|result| result.status == 0)
                .map(|result| result.stdout.trim() == "-1")
                .unwrap_or_else(|| version.as_str() > current),
            None => true,
        };
        if !newer {
            continue;
        }
        newest.insert(name, (version, path));
    }
    let mut selected: Vec<PathBuf> = newest.into_values().map(|(_, path)| path).collect();
    selected.sort();
    selected
}

fn build_pkg(pkg: &str, dest: &Path) -> Result<Vec<String>, String> {
    if is_root() {
        return Err(format!(
            "cannot build AUR package '{pkg}' as root (makepkg refuses). \
             Run export as your user, or pass --aur-pkgdir with prebuilt packages."
        ));
    }
    for tool in ["makepkg", "git"] {
        if !system::cmd_exists(tool) {
            return Err(format!(
                "'{tool}' not found; needed to build AUR package '{pkg}'"
            ));
        }
    }
    let build_root = system::home_dir()
        .map(|home| home.join(".cache/omarchy-recipe/aur-build"))
        .unwrap_or_else(std::env::temp_dir);
    let work = system::unique_dir(&build_root, "omr-aur")?;
    let srcdir = work.join(pkg);

    // Fetch PKGBUILD: prefer yay -G, fall back to AUR git.
    let fetched = if system::cmd_exists("yay") {
        system::run_in(&work, "yay", &["-G", pkg])
            .map(|r| r.status == 0)
            .unwrap_or(false)
    } else {
        false
    } || system::run_in(
        &work,
        "git",
        &[
            "clone",
            "--",
            &format!("https://aur.archlinux.org/{pkg}.git"),
            &srcdir.to_string_lossy(),
        ],
    )
    .map(|r| r.status == 0)
    .unwrap_or(false);
    if !fetched || !srcdir.join("PKGBUILD").is_file() {
        std::fs::remove_dir_all(&work).ok();
        return Err(format!("cannot fetch AUR sources for '{pkg}'"));
    }

    output::info(&format!(
        "Building {pkg} (makepkg -sf; sudo may prompt for deps) ..."
    ));
    let build_spin = output::spinner::spin(&format!("building {pkg}"));
    let r = match system::run_in(&srcdir, "makepkg", &["--noconfirm", "-sf"]) {
        Ok(r) => r,
        Err(e) => {
            build_spin.fail(&format!("cannot run makepkg for {pkg}"));
            std::fs::remove_dir_all(&work).ok();
            return Err(e);
        }
    };
    if r.status != 0 {
        build_spin.fail(&format!("makepkg failed for {pkg}"));
        std::fs::remove_dir_all(&work).ok();
        return Err(format!("makepkg failed for '{pkg}': {}", tail(&r.stderr)));
    }
    build_spin.succeed(None);
    // Collect every artifact from this PKGBUILD, including split packages.
    let mut built: Vec<PathBuf> = std::fs::read_dir(&srcdir)
        .map_err(|e| format!("cannot list build dir: {e}"))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .map(|n| {
                        let n = n.to_string_lossy();
                        n.contains(".pkg.tar.") && !n.ends_with(".sig")
                    })
                    .unwrap_or(false)
        })
        .collect();
    built.sort();
    if built.is_empty() {
        std::fs::remove_dir_all(&work).ok();
        return Err(format!("makepkg produced no package file for '{pkg}'"));
    }
    let mut names = vec![];
    for b in &built {
        let name = b
            .file_name()
            .ok_or_else(|| format!("built package has no filename: {}", b.display()))?
            .to_string_lossy()
            .to_string();
        std::fs::copy(b, dest.join(&name)).map_err(|e| format!("cannot collect {name}: {e}"))?;
        names.push(name);
    }
    std::fs::remove_dir_all(&work).ok();
    Ok(names)
}

fn tail(s: &str) -> String {
    s.lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
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
        let hits = find_prebuilt(&dir, "foo");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].ends_with("foo-1.0-1-x86_64.pkg.tar.zst"));
    }

    #[test]
    fn vendor_reuses_pkgdir_without_building() {
        let dir = std::env::temp_dir().join("omr-aur-reuse");
        let pkgdir = dir.join("prebuilt");
        let bundle = dir.join("bundle");
        std::fs::create_dir_all(&pkgdir).unwrap();
        std::fs::write(pkgdir.join("foo-1.0-1-x86_64.pkg.tar.zst"), b"fake").unwrap();
        let out = vendor(
            &["foo".to_string()],
            &bundle,
            Some(pkgdir.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file, "aur-pkgs/foo-1.0-1-x86_64.pkg.tar.zst");
        assert!(out[0].sha256.as_deref().is_some_and(|s| s.len() == 64));
        assert!(bundle.join(&out[0].file).is_file());
    }

    #[test]
    fn vendor_reuses_all_split_package_artifacts() {
        let dir = system::temp_dir("omr-aur-split").unwrap();
        let pkgdir = dir.join("prebuilt");
        let bundle = dir.join("bundle");
        std::fs::create_dir_all(&pkgdir).unwrap();
        std::fs::write(pkgdir.join("foo-1.0-1-x86_64.pkg.tar.zst"), b"fake").unwrap();
        std::fs::write(pkgdir.join("foo-tools-1.0-1-x86_64.pkg.tar.zst"), b"fake").unwrap();
        std::fs::write(pkgdir.join("foo-2.0-1-x86_64.pkg.tar.zst"), b"new").unwrap();
        let out = vendor(
            &["foo".to_string()],
            &bundle,
            Some(pkgdir.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .any(|pkg| pkg.file.ends_with("foo-2.0-1-x86_64.pkg.tar.zst")));
        assert!(!out
            .iter()
            .any(|pkg| pkg.file.ends_with("foo-1.0-1-x86_64.pkg.tar.zst")));
        assert!(out.iter().all(|pkg| bundle.join(&pkg.file).is_file()));
    }
}
