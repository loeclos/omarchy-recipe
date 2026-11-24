//! `validate`: check a .recipe bundle.

use std::path::Path;

use crate::output;
use crate::recipe::*;

pub fn run(bundle_arg: Option<String>) -> Result<(), String> {
    let bundle = resolve_bundle_dir(bundle_arg.as_deref())?;
    let recipe = validate_bundle(&bundle)?;
    output::ok(&format!(
        "{} (schema v{}, {} repo pkgs, {} AUR pkgs)",
        bundle.join("recipe.json").display(),
        recipe.schema_version,
        recipe.packages.explicit_repo.len(),
        recipe.packages.aur.len()
    ));
    Ok(())
}

pub fn validate_bundle(bundle: &Path) -> Result<Recipe, String> {
    let recipe_path = bundle.join("recipe.json");
    let (recipe, source_version) = Recipe::load_with_version(&recipe_path)?;
    if recipe.dotfiles.file.is_empty() || recipe.dotfiles.sha256.is_empty() {
        return Err("recipe.json .dotfiles must have file + sha256".into());
    }
    let tarball = tarball_path(bundle, &recipe)?;
    if !tarball.is_file() {
        return Err(format!("dotfiles tarball not found: {}", tarball.display()));
    }
    if recipe.dotfiles.sha256 == "unknown" && source_version == 1 {
        output::warn("legacy v1 tarball has no recorded sha256; skipping integrity check");
    } else {
        if !valid_sha256(&recipe.dotfiles.sha256) {
            return Err(format!(
                "recipe.json .dotfiles.sha256 is not a valid sha256 hash: '{}'",
                recipe.dotfiles.sha256
            ));
        }
        let actual = sha256_file(&tarball)?;
        if !actual.eq_ignore_ascii_case(&recipe.dotfiles.sha256) {
            return Err(format!(
                "sha256 mismatch for {} (expected {}, got {actual})",
                tarball.display(),
                recipe.dotfiles.sha256
            ));
        }
    }

    for pkg in recipe
        .packages
        .explicit_repo
        .iter()
        .chain(recipe.packages.aur.iter())
    {
        if !crate::aur::valid_pkg_name(pkg) {
            return Err(format!("invalid package name in recipe.json: '{pkg}'"));
        }
    }
    if let Some(themes) = &recipe.themes {
        if let Some(current) = themes.current.as_deref() {
            if current.is_empty()
                || current == "."
                || current == ".."
                || current.starts_with('-')
                || current.contains('/')
            {
                return Err(format!("unsafe current theme name: '{current}'"));
            }
        }
        for theme in &themes.installed {
            if theme.name.is_empty()
                || theme.name == "."
                || theme.name == ".."
                || theme.name.contains('/')
            {
                return Err(format!(
                    "unsafe theme name in recipe.json: '{}'",
                    theme.name
                ));
            }
            if theme.url.as_deref().is_some_and(|url| url.starts_with('-')) {
                return Err(format!("unsafe theme URL for '{}'", theme.name));
            }
        }
    }
    if let Some(font) = recipe
        .font
        .as_ref()
        .and_then(|font| font.monospace.as_deref())
    {
        if font.starts_with('-') || font.contains('/') {
            return Err(format!("unsafe font name: '{font}'"));
        }
    }
    for plugin in &recipe.plugins {
        if plugin.id.is_empty() || plugin.id.starts_with('-') || plugin.id.contains('/') {
            return Err(format!("invalid plugin id in recipe.json: '{}'", plugin.id));
        }
        if plugin
            .url
            .as_deref()
            .is_some_and(|url| url.starts_with('-'))
        {
            return Err(format!("unsafe plugin URL for '{}'", plugin.id));
        }
    }
    if let Some(services) = &recipe.services {
        for unit in services.user.iter().chain(services.system.iter()) {
            if unit.is_empty() || unit.starts_with('-') || unit.contains('/') {
                return Err(format!("invalid service unit in recipe.json: '{unit}'"));
            }
        }
    }

    // New v2 writers hash every vendored package. Older bundles are still
    // readable, but make the missing integrity coverage visible.
    for pkg in &recipe.aur_vendored {
        let path = bundle_path(bundle, Path::new(&pkg.file))?;
        if !path.is_file() {
            return Err(format!("vendored package not found: {}", path.display()));
        }
        match &pkg.sha256 {
            Some(expected) if valid_sha256(expected) => {
                let actual = sha256_file(&path)?;
                if !actual.eq_ignore_ascii_case(expected) {
                    return Err(format!(
                        "sha256 mismatch for vendored package {} (expected {}, got {actual})",
                        path.display(),
                        expected
                    ));
                }
            }
            Some(_) => {
                return Err(format!(
                    "invalid sha256 for vendored package {}",
                    path.display()
                ));
            }
            None => output::warn(&format!(
                "vendored package has no recorded sha256 (legacy bundle): {}",
                path.display()
            )),
        }
    }
    Ok(recipe)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system;

    fn write_bundle(dir: &Path, version: u32, tamper: bool) -> std::path::PathBuf {
        let bundle = dir.join(format!("b{version}{tamper}"));
        std::fs::create_dir_all(&bundle).unwrap();
        let tarball = bundle.join("dotfiles.tar.zst");
        std::fs::write(&tarball, b"hello dotfiles").unwrap();
        if tamper {
            std::fs::write(&tarball, b"hello dotfiles TAMPERED").unwrap();
        }
        let sha = sha256_file(&bundle.join("dotfiles.tar.zst")).unwrap();
        let sha = if tamper { "wrong".to_string() } else { sha };
        let recipe = serde_json::json!({
            "schema_version": version,
            "generated_by": "test", "generated_at": "t",
            "omarchy": {"version": "x", "channel": "dev", "ref": "abc"},
            "packages": {"explicit_repo": [], "aur": []},
            "themes": {"current": "tokyo-night", "installed": []},
            "dotfiles": {"file": "dotfiles.tar.zst", "sha256": sha}
        });
        std::fs::write(bundle.join("recipe.json"), recipe.to_string()).unwrap();
        bundle
    }

    #[test]
    fn good_bundle_validates() {
        let dir = std::env::temp_dir().join("omr-validate");
        std::fs::create_dir_all(&dir).unwrap();
        let b = write_bundle(&dir, 2, false);
        assert!(validate_bundle(&b).is_ok());
    }

    #[test]
    fn tampered_bundle_rejected() {
        let dir = std::env::temp_dir().join("omr-validate");
        std::fs::create_dir_all(&dir).unwrap();
        let b = write_bundle(&dir, 2, true);
        assert!(validate_bundle(&b).is_err());
    }

    #[test]
    fn bad_version_rejected() {
        let dir = std::env::temp_dir().join("omr-validate");
        std::fs::create_dir_all(&dir).unwrap();
        let b = write_bundle(&dir, 99, false);
        assert!(validate_bundle(&b).is_err());
    }

    #[test]
    fn unknown_hash_and_unsafe_paths_rejected() {
        let dir = system::temp_dir("omr-validate-safety").unwrap();
        let b = write_bundle(&dir, 2, false);
        let recipe_path = b.join("recipe.json");
        let original: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&recipe_path).unwrap()).unwrap();

        let mut unknown = original.clone();
        unknown["dotfiles"]["sha256"] = serde_json::json!("unknown");
        std::fs::write(&recipe_path, unknown.to_string()).unwrap();
        assert!(validate_bundle(&b).is_err());

        let mut traversal = original.clone();
        traversal["dotfiles"]["file"] = serde_json::json!("../dotfiles.tar.zst");
        std::fs::write(&recipe_path, traversal.to_string()).unwrap();
        assert!(validate_bundle(&b).is_err());

        let mut absolute = original;
        absolute["dotfiles"]["file"] = serde_json::json!("/etc/passwd");
        std::fs::write(&recipe_path, absolute.to_string()).unwrap();
        assert!(validate_bundle(&b).is_err());
    }

    #[test]
    fn legacy_v1_unknown_hash_remains_readable() {
        let dir = system::temp_dir("omr-validate-v1-unknown").unwrap();
        let b = write_bundle(&dir, 1, false);
        let recipe_path = b.join("recipe.json");
        let mut recipe: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&recipe_path).unwrap()).unwrap();
        recipe["dotfiles"]["sha256"] = serde_json::json!("unknown");
        std::fs::write(&recipe_path, recipe.to_string()).unwrap();
        assert!(validate_bundle(&b).is_ok());
    }

    #[test]
    fn vendored_package_hash_is_checked() {
        let dir = system::temp_dir("omr-validate-vendored").unwrap();
        let b = write_bundle(&dir, 2, false);
        std::fs::create_dir_all(b.join("aur-pkgs")).unwrap();
        let pkg = b.join("aur-pkgs/foo.pkg.tar.zst");
        std::fs::write(&pkg, b"package").unwrap();
        let hash = sha256_file(&pkg).unwrap();
        let recipe_path = b.join("recipe.json");
        let mut recipe: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&recipe_path).unwrap()).unwrap();
        recipe["aur_vendored"] = serde_json::json!([{
            "name": "foo",
            "file": "aur-pkgs/foo.pkg.tar.zst",
            "sha256": hash
        }]);
        std::fs::write(&recipe_path, recipe.to_string()).unwrap();
        assert!(validate_bundle(&b).is_ok());

        std::fs::write(&pkg, b"tampered").unwrap();
        assert!(validate_bundle(&b).is_err());
    }
}
