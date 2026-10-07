//! Host introspection: commands, Omarchy checkout, pacman/AUR queries.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::output;

#[derive(Debug, Clone)]
pub struct CmdResult {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn cmd_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn run(prog: &str, args: &[&str]) -> Result<CmdResult, String> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| format!("cannot execute {prog}: {e}"))?;
    Ok(CmdResult {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

pub fn run_in(dir: &Path, prog: &str, args: &[&str]) -> Result<CmdResult, String> {
    let out = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cannot execute {prog}: {e}"))?;
    Ok(CmdResult {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

/// Locate the Omarchy checkout (dev-link aware).
pub fn find_omarchy_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("OMARCHY_PATH") {
        if Path::new(&p).is_dir() {
            return Ok(PathBuf::from(p));
        }
    }
    if let Ok(conf) = std::fs::read_to_string("/etc/omarchy.conf") {
        for line in conf.lines() {
            if let Some(v) = line.strip_prefix("OMARCHY_PATH=") {
                let v = v.trim().trim_matches(['"', '\'']);
                if Path::new(v).is_dir() {
                    return Ok(PathBuf::from(v));
                }
            }
        }
    }
    if let Some(home) = home_dir() {
        let p = home.join("omarchy");
        if p.is_dir() {
            return Ok(p);
        }
    }
    if Path::new("/usr/share/omarchy").is_dir() {
        return Ok(PathBuf::from("/usr/share/omarchy"));
    }
    Err("cannot locate Omarchy checkout (tried $OMARCHY_PATH, /etc/omarchy.conf, ~/omarchy)".into())
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn omarchy_version(root: &Path) -> String {
    std::fs::read_to_string(root.join("version"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

pub fn omarchy_channel() -> String {
    if cmd_exists("omarchy-version-channel") {
        match run("omarchy-version-channel", &[]) {
            Ok(r) if r.status == 0 && !r.stdout.trim().is_empty() => {
                return r.stdout.trim().to_string()
            }
            _ => {}
        }
    }
    "unknown".to_string()
}

pub fn git_head(dir: &Path) -> Option<String> {
    if !cmd_exists("git") {
        return None;
    }
    match run_in(dir, "git", &["rev-parse", "HEAD"]) {
        Ok(r) if r.status == 0 => Some(r.stdout.trim().to_string()),
        _ => None,
    }
}

pub fn git_remote(dir: &Path) -> Option<String> {
    if !cmd_exists("git") {
        return None;
    }
    match run_in(dir, "git", &["remote", "get-url", "origin"]) {
        Ok(r) if r.status == 0 && !r.stdout.trim().is_empty() => {
            Some(r.stdout.trim().to_string())
        }
        _ => None,
    }
}

/// Bare package names from omarchy-base.packages + omarchy-other.packages.
pub fn base_pkg_set(root: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    for f in ["install/omarchy-base.packages", "install/omarchy-other.packages"] {
        let Ok(text) = std::fs::read_to_string(root.join(f)) else {
            continue;
        };
        for line in text.lines() {
            let name = line.split('#').next().unwrap_or("").trim();
            if !name.is_empty() {
                set.insert(name.to_string());
            }
        }
    }
    set
}

/// Explicitly-installed repo packages minus the Omarchy base set.
pub fn explicit_repo_pkgs(root: &Path) -> Vec<String> {
    let base = base_pkg_set(root);
    let Ok(r) = run("pacman", &["-Qeq"]) else {
        output::warn("pacman not found; recording empty repo package list");
        return vec![];
    };
    if r.status != 0 {
        return vec![];
    }
    let mut out: Vec<String> = r
        .stdout
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty() && !base.contains(*p))
        .map(str::to_string)
        .collect();
    out.sort();
    out
}

pub fn aur_helper() -> Option<&'static str> {
    if cmd_exists("yay") {
        Some("yay")
    } else if cmd_exists("paru") {
        Some("paru")
    } else {
        None
    }
}

/// Explicit foreign (AUR) packages.
pub fn aur_pkgs() -> Vec<String> {
    match aur_helper() {
        Some(helper) => match run(helper, &["-Qmq"]) {
            Ok(r) if r.status == 0 => {
                let mut out: Vec<String> = r
                    .stdout
                    .lines()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect();
                out.sort();
                out
            }
            _ => vec![],
        },
        None => {
            output::warn("no AUR helper (yay/paru) found; recording empty AUR list");
            vec![]
        }
    }
}

pub fn utc_now() -> String {
    // date -u +%Y-%m-%dT%H:%M:%SZ without pulling in chrono.
    match run("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]) {
        Ok(r) if r.status == 0 => r.stdout.trim().to_string(),
        _ => "unknown".to_string(),
    }
}

pub fn hostname() -> String {
    match run("hostname", &[]) {
        Ok(r) if r.status == 0 && !r.stdout.trim().is_empty() => r.stdout.trim().to_string(),
        _ => "machine".to_string(),
    }
}

/// First human (UID >= 1000, not nobody) username from /etc/passwd.
pub fn primary_user() -> Option<String> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in text.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() < 7 {
            continue;
        }
        let (name, uid) = (parts[0], parts[2].parse::<u32>().unwrap_or(0));
        let shell = parts[6];
        if uid >= 1000 && uid < 60000 && name != "nobody" && !shell.ends_with("nologin") && !shell.ends_with("false") {
            return Some(name.to_string());
        }
    }
    None
}
