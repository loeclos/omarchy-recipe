//! `validate`: check a .recipe bundle.

use std::path::Path;

use crate::output;
use crate::recipe::*;

pub fn run(bundle_arg: Option<String>) -> Result<(), String> {
    let bundle = resolve_bundle_dir(bundle_arg.as_deref())?;
    validate_bundle(&bundle)?;
    let recipe = Recipe::load(&bundle.join("recipe.json"))?;
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
    let recipe = Recipe::load(&recipe_path)?;
    if recipe.dotfiles.file.is_empty() || recipe.dotfiles.sha256.is_empty() {
        return Err("recipe.json .dotfiles must have file + sha256".into());
    }
    let tarball = tarball_path(bundle, &recipe);
    if !tarball.is_file() {
        return Err(format!("dotfiles tarball not found: {}", tarball.display()));
    }
    if recipe.dotfiles.sha256 == "unknown" {
        output::warn("tarball has no recorded sha256; skipping integrity check");
    } else {
        let actual = sha256_file(&tarball)?;
        if actual != recipe.dotfiles.sha256 {
            return Err(format!(
                "sha256 mismatch for {} (expected {}, got {actual})",
                tarball.display(),
                recipe.dotfiles.sha256
            ));
        }
    }
    Ok(recipe)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
