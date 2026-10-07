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
    pub fn load(path: &Path) -> Result<Recipe, String> {
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
                let mut r: Recipe = serde_json::from_value(v)
                    .map_err(|e| format!("schema v1 parse error: {e}"))?;
                r.schema_version = 2;
                r.selection = Selection::default();
                Ok(r)
            }
            2 => serde_json::from_value(v).map_err(|e| format!("schema v2 parse error: {e}")),
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

/// Effective tarball path (bundle-relative unless absolute).
pub fn tarball_path(bundle: &Path, recipe: &Recipe) -> PathBuf {
    let f = PathBuf::from(&recipe.dotfiles.file);
    if f.is_absolute() {
        f
    } else {
        bundle.join(f)
    }
}

/// sha256 hex of a file.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&bytes);
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
            }],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Recipe = serde_json::from_str(&s).unwrap();
        assert!(back.excluded("webapps"));
        assert!(!back.excluded("aur"));
        assert_eq!(back.aur_vendored[0].file, "aur-pkgs/foo.pkg.tar.zst");
    }
}
