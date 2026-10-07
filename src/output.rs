//! Pretty, colored terminal output with quiet mode and numbered stages.
//!
//! - `info` is chatty detail, hidden under `--quiet`.
//! - `stage(i, n, msg)` is the always-visible progress spine: `[i/n] msg`.
//! - `summary` renders the end-of-run info panel.
//! Colors honor NO_COLOR, dumb terminals, and non-ttys.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

// Quiet is the default; --verbose flips it off.
static QUIET: AtomicBool = AtomicBool::new(true);

pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("TERM").as_deref() == Ok("dumb") {
        return false;
    }
    std::io::stdout().is_terminal() || std::env::var_os("OMARCHY_RECIPE_FORCE_COLOR").is_some()
}

fn paint(code: &str, msg: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{msg}\x1b[0m")
    } else {
        msg.to_string()
    }
}

// Palette
fn cyan(s: &str) -> String {
    paint("36", s)
}
fn bold_cyan(s: &str) -> String {
    paint("1;36", s)
}
fn green(s: &str) -> String {
    paint("32", s)
}
fn bold_green(s: &str) -> String {
    paint("1;32", s)
}
fn yellow(s: &str) -> String {
    paint("33", s)
}
fn bold_red(s: &str) -> String {
    paint("1;31", s)
}
fn magenta(s: &str) -> String {
    paint("35", s)
}
fn bold(s: &str) -> String {
    paint("1", s)
}
fn dim(s: &str) -> String {
    paint("2", s)
}

/// Value stylers for embedding color in messages.
pub fn path(s: &str) -> String {
    magenta(s)
}
pub fn num(n: usize) -> String {
    bold(&n.to_string())
}

/// Numbered stage header. Always visible, even in quiet mode.
pub fn stage(i: usize, n: usize, msg: &str) {
    let tag = bold_cyan(&format!("[{i}/{n}]"));
    println!("{} {}", tag, bold(msg));
}

/// Unnumbered step (section headers that don't fit the i/n spine).
pub fn step(msg: &str) {
    println!("{} {}", bold_cyan("==>"), bold(msg));
}

/// Chatty detail. Hidden under --quiet.
pub fn info(msg: &str) {
    if is_quiet() {
        return;
    }
    println!("{} {}", dim(" ->"), msg);
}

pub fn ok(msg: &str) {
    println!("{} {}", bold_green("✓"), msg);
}

pub fn warn(msg: &str) {
    eprintln!("{} {}", yellow("!"), msg);
}

pub fn error(msg: &str) {
    eprintln!("{} {}", bold_red("✗"), msg);
}

/// Fatal error: print + exit code.
pub fn die(msg: &str) -> ! {
    error(msg);
    std::process::exit(1);
}

/// End-of-run info panel:
/// ╭─ title ─╮
/// │ key   value │
/// ╰─────────╯
pub fn summary(title: &str, rows: &[(&str, String)]) {
    let key_w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let val_w = rows
        .iter()
        .map(|(_, v)| strip_ansi(v).len())
        .max()
        .unwrap_or(0);
    let inner = key_w + val_w + 3; // "key SP value" + padding
    let width = inner.max(strip_ansi(title).len() + 2);

    println!("{}", green(&format!("╭─{}─╮", "─".repeat(width))));
    println!("{} {} {}", green("│"), bold(title), green(&format!("{}│", " ".repeat(width.saturating_sub(strip_ansi(title).len())))));
    println!("{}", green(&format!("├─{}─┤", "─".repeat(width))));
    for (k, v) in rows {
        let plain_len = k.len() + 1 + strip_ansi(v).len();
        let pad = width.saturating_sub(plain_len);
        println!(
            "{} {} {} {}{}",
            green("│"),
            cyan(k),
            v,
            " ".repeat(pad),
            green("│")
        );
    }
    println!("{}", green(&format!("╰─{}─╯", "─".repeat(width))));
}

fn strip_ansi(s: &str) -> String {
    // Strip \x1b[...m sequences for width math. Minimal state machine.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_works() {
        assert_eq!(strip_ansi("\x1b[1;36mhi\x1b[0m"), "hi");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn output_state_and_summary() {
        // Single test: QUIET is process-global, so toggling must not race
        // other tests touching it.
        set_quiet(true);
        assert!(is_quiet());
        info("hidden");
        stage(1, 2, "staged");
        set_quiet(false);
        assert!(!is_quiet());
        summary("test", &[("key", "value".to_string()), ("longer-key", num(42))]);
    }

}
