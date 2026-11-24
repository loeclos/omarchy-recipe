//! recipe.json data model. Reads schema v1 (migrates in memory) and v2, writes v2.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 2;
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Selection {
    #[serde(default)]
    pub excluded_sections: Vec<String>,
    #[serde(default)]
    pub extra_excludes: Vec<String>,
    /// Bundle ships secrets (.ssh, .gnupg, ...). Exported with --include-secrets.
    #[serde(default)]
    pub include_secrets: bool,
}

impl Selection {
    pub fn excludes(&self, section: &str) -> bool {
        self.excluded_sections.iter().any(|s| s == section)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OmarchyInfo {
    pub version: String,
    pub channel: String,
    pub r#ref: String,
    #[serde(default = "unknown")]
    pub remote: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packages {
    #[serde(default)]
    pub explicit_repo: Vec<String>,
    #[serde(default)]
    pub aur: Vec<String>,
    #[serde(default = "default_wifi")]
    pub aur_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeEntry {
    pub name: String,
    pub source: String,
    pub url: Option<String>,
    pub r#ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Background {
    pub current: Option<String>,
    #[serde(default = "default_true")]
    pub bundled_in_dotfiles: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Themes {
    pub current: Option<String>,
    #[serde(default)]
    pub installed: Vec<ThemeEntry>,
    #[serde(default)]
    pub background: Option<Background>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Font {
    pub monospace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub id: String,
    pub enabled: bool,
    #[serde(default = "default_true")]
    #[serde(rename = "firstParty")]
    pub first_party: bool,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Services {
    #[serde(default)]
    pub user: Vec<String>,
    #[serde(default)]
    pub system: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webapp {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub desktop_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Shell {
    pub bar_layout_sha256: Option<String>,
    #[serde(default = "default_true")]
    pub restored_via_dotfiles: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dotfiles {
    pub file: String,
    pub sha256: String,
    #[serde(default)]
    pub includes: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VendoredPkg {
    pub name: String,
    pub file: String,
    /// Integrity hash for newly-written v2 bundles. Optional so v1/v2 bundles
    /// created before hashes were recorded remain readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub schema_version: u32,
    pub generated_by: String,
    pub generated_at: String,
    pub omarchy: OmarchyInfo,
    pub packages: Packages,
    #[serde(default)]
    pub themes: Option<Themes>,
    #[serde(default)]
    pub font: Option<Font>,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
    #[serde(default)]
    pub services: Option<Services>,
    #[serde(default)]
    pub webapps: Vec<Webapp>,
    #[serde(default)]
    pub shell: Option<Shell>,
    pub dotfiles: Dotfiles,
    #[serde(default)]
    pub selection: Selection,
    #[serde(default)]
    pub aur_vendored: Vec<VendoredPkg>,
}

fn unknown() -> String {
    "unknown".to_string()
}
fn default_true() -> bool {
    true
}
fn default_wifi() -> String {
    "wifi".to_string()
}

impl Recipe {
    /// Parse recipe.json, accepting v1 (migrated) and v2.
    #[cfg(test)]
    pub fn load(path: &Path) -> Result<Recipe, String> {
        Self::load_with_version(path).map(|(recipe, _)| recipe)
    }

    /// Same as [`Recipe::load`], but retains the source schema version so
    /// validation can apply narrow compatibility exceptions for v1 bundles.
    pub fn load_with_version(path: &Path) -> Result<(Recipe, u32), String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("invalid JSON: {e}"))?;
        let version = v
            .get("schema_version")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| "recipe.json missing schema_version".to_string())?;
        match version {
            1 => {
                let mut r: Recipe =
                    serde_json::from_value(v).map_err(|e| format!("schema v1 parse error: {e}"))?;
                r.schema_version = 2;
                r.selection = Selection::default();
                Ok((r, 1))
            }
            2 => serde_json::from_value(v)
                .map(|recipe| (recipe, 2))
                .map_err(|e| format!("schema v2 parse error: {e}")),
            n => Err(format!(
                "unsupported schema_version {n} (this tool handles 1 and 2)"
            )),
        }
    }

    pub fn excluded(&self, section: &str) -> bool {
        self.selection.excludes(section)
    }
}

/// Resolve bundle dir: explicit arg wins, else cwd must hold recipe.json.
pub fn resolve_bundle_dir(arg: Option<&str>) -> Result<PathBuf, String> {
    let dir = match arg {
        Some(a) => PathBuf::from(a),
        None => std::env::current_dir().map_err(|e| format!("cannot get cwd: {e}"))?,
    };
    if !dir.join("recipe.json").is_file() {
        return Err(format!(
            "no recipe.json in {} (pass the .recipe bundle dir, or run from inside it)",
            dir.display()
        ));
    }
    Ok(dir)
}

/// Resolve a bundle-relative path without allowing archives to escape the
/// bundle through absolute paths or `..` components.
pub fn bundle_path(bundle: &Path, relative: &Path) -> Result<PathBuf, String> {
    use std::path::Component;

    if relative.as_os_str().is_empty() {
        return Err("bundle file path is empty".into());
    }
    if relative.is_absolute() {
        return Err(format!(
            "bundle file path must be relative: {}",
            relative.display()
        ));
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(format!(
                "unsafe bundle file path '{}': only relative paths inside the bundle are allowed",
                relative.display()
            ));
        }
    }
    if bundle.is_dir() {
        let base = bundle
            .canonicalize()
            .map_err(|e| format!("cannot resolve {}: {e}", bundle.display()))?;
        let joined = base.join(relative);
        if joined.exists() {
            let resolved = joined
                .canonicalize()
                .map_err(|e| format!("cannot resolve {}: {e}", joined.display()))?;
            if !resolved.starts_with(&base) {
                return Err(format!(
                    "bundle file path escapes the bundle through a symlink: {}",
                    relative.display()
                ));
            }
        }
        return Ok(joined);
    }
    Ok(bundle.join(relative))
}

/// Effective tarball path (bundle-relative and confined to the bundle).
pub fn tarball_path(bundle: &Path, recipe: &Recipe) -> Result<PathBuf, String> {
    bundle_path(bundle, Path::new(&recipe.dotfiles.file))
}

/// sha256 hex of a file, streamed so large dotfiles archives do not have to fit
/// in memory.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// Sections known to --without, for help text and validation.
pub fn known_sections() -> HashSet<&'static str> {
    [
        "packages",
        "aur",
        "themes",
        "font",
        "plugins",
        "services",
        "webapps",
        "backgrounds",
        "icons",
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1_sample() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "generated_by": "omarchy-recipe 0.1.0",
            "generated_at": "2026-10-06T00:00:00Z",
            "omarchy": {"version": "4.0.0.alpha", "channel": "edge", "ref": "abc", "remote": "x"},
            "packages": {"explicit_repo": ["jq"], "aur": []},
            "themes": {"current": "tokyo-night", "installed": [],
                       "background": {"current": null, "bundled_in_dotfiles": true}},
            "dotfiles": {"file": "dotfiles.tar.zst", "sha256": "deadbeef"}
        })
    }

    #[test]
    fn v1_loads_with_defaults() {
        let dir = std::env::temp_dir().join("omr-test-v1");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("recipe.json");
        std::fs::write(&p, serde_json::to_string(&v1_sample()).unwrap()).unwrap();
        let r = Recipe::load(&p).unwrap();
        assert_eq!(r.schema_version, 2);
        assert_eq!(r.packages.explicit_repo, vec!["jq"]);
        assert_eq!(r.packages.aur_mode, "wifi");
        assert!(r.plugins.is_empty());
        assert!(!r.excluded("webapps"));
    }

    #[test]
    fn bundle_paths_stay_inside_bundle() {
        let bundle = Path::new("/tmp/example.recipe");
        assert_eq!(
            bundle_path(bundle, Path::new("aur-pkgs/foo.zst")).unwrap(),
            bundle.join("aur-pkgs/foo.zst")
        );
        assert!(bundle_path(bundle, Path::new("../outside")).is_err());
        assert!(bundle_path(bundle, Path::new("/etc/passwd")).is_err());
        assert!(bundle_path(bundle, Path::new("")).is_err());

        let root = std::env::temp_dir().join("omr-bundle-symlink-test");
        let _ = std::fs::remove_dir_all(&root);
        let real_bundle = root.join("bundle");
        std::fs::create_dir_all(&real_bundle).unwrap();
        let outside = root.join("outside");
        std::fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, real_bundle.join("escape")).unwrap();
        assert!(bundle_path(&real_bundle, Path::new("escape")).is_err());
    }

    #[test]
    fn v3_rejected() {
        let dir = std::env::temp_dir().join("omr-test-v3");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("recipe.json");
        let mut v = v1_sample();
        v["schema_version"] = serde_json::json!(99);
        std::fs::write(&p, serde_json::to_string(&v).unwrap()).unwrap();
        assert!(Recipe::load(&p).is_err());
    }

    #[test]
    fn v2_roundtrip_with_selection() {
        let r = Recipe {
            schema_version: 2,
            generated_by: "test".into(),
            generated_at: "t".into(),
            omarchy: OmarchyInfo {
                version: "v".into(),
                channel: "c".into(),
                r#ref: "r".into(),
                remote: "x".into(),
            },
            packages: Packages {
                explicit_repo: vec![],
                aur: vec!["foo".into()],
                aur_mode: "vendored".into(),
            },
            themes: None,
            font: None,
            plugins: vec![],
            services: None,
            webapps: vec![],
            shell: None,
            dotfiles: Dotfiles {
                file: "dotfiles.tar.zst".into(),
                sha256: "x".into(),
                includes: vec![],
                excludes: vec![],
            },
            selection: Selection {
                excluded_sections: vec!["webapps".into()],
                extra_excludes: vec!["*/foo*".into()],
                include_secrets: false,
            },
            aur_vendored: vec![VendoredPkg {
                name: "foo".into(),
                file: "aur-pkgs/foo.pkg.tar.zst".into(),
                sha256: None,
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Recipe = serde_json::from_str(&s).unwrap();
        assert!(back.excluded("webapps"));
        assert!(!back.excluded("aur"));
        assert_eq!(back.aur_vendored[0].file, "aur-pkgs/foo.pkg.tar.zst");
    }
}
