//! Host introspection: commands, Omarchy checkout, pacman/AUR queries.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct CmdResult {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn cmd_exists(name: &str) -> bool {
    // Pass the command name as an argument rather than interpolating it into
    // shell source. All current callers use fixed names, but this keeps the
    // helper safe if a recipe-supplied value is ever checked.
    Command::new("sh")
        .args(["-c", "command -v -- \"$1\" >/dev/null 2>&1", "sh", name])
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
        Ok(r) if r.status == 0 && !r.stdout.trim().is_empty() => Some(r.stdout.trim().to_string()),
        _ => None,
    }
}

/// Bare package names from omarchy-base.packages + omarchy-other.packages.
pub fn base_pkg_set(root: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    for f in [
        "install/omarchy-base.packages",
        "install/omarchy-other.packages",
    ] {
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

/// pacman (and the yay/paru wrappers) exit 1 when a filtered `-Q` query
/// matches nothing — e.g. zero foreign packages on a machine without AUR
/// packages. That is a valid empty list, not a failure. Real failures
/// (locked/corrupt db, bad args) print an explanation to stderr.
fn empty_match_ok(r: &CmdResult) -> bool {
    r.stdout.trim().is_empty() && r.stderr.trim().is_empty()
}

fn query_failure(prog: &str, args: &str, r: &CmdResult) -> String {
    let detail = if !r.stderr.trim().is_empty() {
        r.stderr.trim().to_string()
    } else if !r.stdout.trim().is_empty() {
        r.stdout.trim().to_string()
    } else {
        format!("exit {}", r.status)
    };
    format!("{prog} {args} failed: {detail}")
}

/// Explicitly-installed repo packages minus the Omarchy base set.
///
/// `pacman -Qeq` lists every explicitly-installed package, including AUR ones
/// installed via a helper (they're marked explicit). Those belong in `aur`,
/// not the repo mirror — sending them to the offline mirror produced
/// "target not found: <pkg>" during the mirror fetch. Drop foreign packages.
pub fn explicit_repo_pkgs(root: &Path) -> Result<Vec<String>, String> {
    let base = base_pkg_set(root);
    let foreign = foreign_pkg_set()?;
    let r =
        run("pacman", &["-Qeq"]).map_err(|e| format!("cannot list repository packages: {e}"))?;
    if r.status != 0 {
        if empty_match_ok(&r) {
            return Ok(vec![]);
        }
        return Err(query_failure("pacman", "-Qeq", &r));
    }
    let mut out: Vec<String> = r
        .stdout
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty() && !base.contains(*p) && !foreign.contains(*p))
        .map(str::to_string)
        .collect();
    out.sort();
    Ok(out)
}

/// Packages installed from a foreign source (AUR via a helper).
fn foreign_pkg_set() -> Result<HashSet<String>, String> {
    let mut set = HashSet::new();
    let r = run("pacman", &["-Qm"]).map_err(|e| format!("cannot list foreign packages: {e}"))?;
    if r.status != 0 {
        if empty_match_ok(&r) {
            return Ok(set);
        }
        return Err(query_failure("pacman", "-Qm", &r));
    }
    for line in r.stdout.lines() {
        let name = line.split_whitespace().next().unwrap_or("").trim();
        if !name.is_empty() {
            set.insert(name.to_string());
        }
    }
    Ok(set)
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
pub fn aur_pkgs() -> Result<Vec<String>, String> {
    let Some(helper) = aur_helper() else {
        return Err("no AUR helper (yay/paru) found; cannot record the AUR package list".into());
    };
    let r = run(helper, &["-Qmq"])
        .map_err(|e| format!("cannot list AUR packages with {helper}: {e}"))?;
    if r.status != 0 {
        if empty_match_ok(&r) {
            return Ok(vec![]);
        }
        return Err(query_failure(helper, "-Qmq", &r));
    }
    let mut out: Vec<String> = r
        .stdout
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    out.sort();
    Ok(out)
}

/// Create a unique private temporary directory. PID-only names collide with
/// stale directories and concurrent runs.
#[cfg(test)]
pub fn temp_dir(prefix: &str) -> Result<PathBuf, String> {
    unique_dir(&std::env::temp_dir(), prefix)
}

pub fn unique_dir(base: &Path, prefix: &str) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    std::fs::create_dir_all(base).map_err(|e| format!("cannot create {}: {e}", base.display()))?;
    for _ in 0..100 {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| format!("cannot read clock: {e}"))?
            .as_nanos();
        let path = base.join(format!("{prefix}-{}-{nanos}-{nonce}", std::process::id()));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .map_err(|e| format!("cannot secure {}: {e}", path.display()))?;
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
        }
    }
    Err(format!(
        "cannot allocate a unique temporary directory for {prefix}"
    ))
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

pub fn user_home(user: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|line| {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 7 && parts[0] == user && !parts[5].is_empty() {
            Some(PathBuf::from(parts[5]))
        } else {
            None
        }
    })
}

pub fn user_uid(user: &str) -> Option<u32> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    text.lines().find_map(|line| {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[0] == user {
            parts[2].parse::<u32>().ok()
        } else {
            None
        }
    })
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
        if (1000..60000).contains(&uid)
            && name != "nobody"
            && !shell.ends_with("nologin")
            && !shell.ends_with("false")
        {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(status: i32, stdout: &str, stderr: &str) -> CmdResult {
        CmdResult {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn empty_output_exit_1_is_empty_match_not_failure() {
        // pacman/yay exit 1 with no output when a filtered -Q query matches
        // nothing (zero foreign/AUR packages). Must be Ok(empty), not Err.
        assert!(empty_match_ok(&result(1, "", "")));
        assert!(empty_match_ok(&result(1, "  \n", "  ")));
    }

    #[test]
    fn stderr_output_is_real_failure() {
        assert!(!empty_match_ok(&result(1, "", "error: failed to synchronize")));
        assert!(!empty_match_ok(&result(1, "some-pkg 1.0", "")));
    }

    #[test]
    fn failure_message_never_ends_bare() {
        let m = query_failure("pacman", "-Qm", &result(1, "", ""));
        assert!(m.contains("exit 1"), "got: {m}");
        let m = query_failure("pacman", "-Qm", &result(1, "", "db locked"));
        assert!(m.contains("db locked"), "got: {m}");
    }
}
