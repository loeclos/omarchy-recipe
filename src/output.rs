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
    spinner::clear_active();
    let tag = bold_cyan(&format!("[{i}/{n}]"));
    println!("{} {}", tag, bold(msg));
}

/// Unnumbered step (section headers that don't fit the i/n spine).
pub fn step(msg: &str) {
    spinner::clear_active();
    println!("{} {}", bold_cyan("==>"), bold(msg));
}

/// Chatty detail. Hidden under --quiet.
pub fn info(msg: &str) {
    if is_quiet() {
        return;
    }
    spinner::clear_active();
    println!("{} {}", dim(" ->"), msg);
}

pub fn ok(msg: &str) {
    spinner::clear_active();
    println!("{} {}", bold_green("✓"), msg);
}

pub fn warn(msg: &str) {
    let _guard = spinner::lock_line();
    spinner::clear_active_locked();
    eprintln!("{} {}", yellow("!"), msg);
}

pub fn error(msg: &str) {
    let _guard = spinner::lock_line();
    spinner::clear_active_locked();
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
    spinner::clear_active();    let key_w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
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

/// Animated step spinners.
///
/// `spin("compressing dotfiles")` starts a guard: on ttys it animates a
/// braille frame on one stderr line while the step runs; on pipes/logs it
/// prints plain start/done lines. Dropping the guard reports `done`
/// (call `.fail()` first on error paths so a failure never gets a ✓).
/// Nested `spin()` calls (e.g. export running inside build-iso) degrade to
/// silent guards so two animations never fight over one line.
pub mod spinner {
    use std::cell::Cell;
    use std::io::{IsTerminal, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    thread_local! {
        static DEPTH: Cell<usize> = const { Cell::new(0) };
    }

    static LINE_LOCK: Mutex<()> = Mutex::new(());

    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    fn stderr_tty() -> bool {
        std::io::stderr().is_terminal()
    }

    fn animate_enabled() -> bool {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if std::env::var("TERM").as_deref() == Ok("dumb") {
            return false;
        }
        stderr_tty()
    }

    pub(crate) fn lock_line() -> std::sync::MutexGuard<'static, ()> {
        LINE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Erase the in-progress spinner line (tty only; no-op on pipes/logs).
    /// Caller must hold [`lock_line`].
    pub(crate) fn clear_active_locked() {
        if DEPTH.get() > 0 && stderr_tty() {
            eprint!("\r\x1b[K");
            let _ = std::io::stderr().flush();
        }
    }

    /// Erase the in-progress spinner line, locking.
    pub(crate) fn clear_active() {
        let _guard = lock_line();
        clear_active_locked();
    }

    pub fn fmt_elapsed(d: Duration) -> String {
        let ms = d.as_millis();
        if ms < 1000 {
            format!("{:.1}s", ms as f64 / 1000.0)
        } else if ms < 60_000 {
            format!("{}s", ms / 1000)
        } else if ms < 3_600_000 {
            format!("{}m {:02}s", ms / 60_000, (ms % 60_000) / 1000)
        } else {
            format!("{}h {:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
        }
    }

    pub struct Spinner {
        label: String,
        detail: Arc<Mutex<String>>,
        history: Arc<Mutex<Vec<String>>>,
        started: Instant,
        stop: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
        silent: bool,
        finished: bool,
    }

    /// Guard that clears the spinner line while a child inherits the
    /// terminal, then resumes animation on drop.
    pub struct PauseGuard<'a> {
        spinner: &'a Spinner,
    }

    impl Drop for PauseGuard<'_> {
        fn drop(&mut self) {
            self.spinner.paused.store(false, Ordering::Relaxed);
        }
    }

    pub fn spin(label: &str) -> Spinner {
        let depth = DEPTH.get();
        let nested = depth > 0;
        DEPTH.set(depth + 1);
        let animated = !nested && animate_enabled();
        if !animated {
            if !nested {
                let _guard = lock_line();
                eprintln!("… {label}");
            }
            return Spinner {
                label: label.to_string(),
                detail: Arc::new(Mutex::new(label.to_string())),
                history: Arc::new(Mutex::new(Vec::new())),
                started: Instant::now(),
                stop: Arc::new(AtomicBool::new(false)),
                paused: Arc::new(AtomicBool::new(false)),
                handle: None,
                silent: nested,
                finished: false,
            };
        }
        let detail = Arc::new(Mutex::new(label.to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let d2 = Arc::clone(&detail);
        let s2 = Arc::clone(&stop);
        let p2 = Arc::clone(&paused);
        let handle = std::thread::spawn(move || {
            let mut i = 0usize;
            let start = Instant::now();
            loop {
                if s2.load(Ordering::Relaxed) {
                    break;
                }
                if !p2.load(Ordering::Relaxed) {
                    let text = d2.lock().map(|g| g.clone()).unwrap_or_default();
                    let _guard = lock_line();
                    eprint!(
                        "\r  {} {} ({})   ",
                        FRAMES[i % FRAMES.len()],
                        text,
                        fmt_elapsed(start.elapsed())
                    );
                    let _ = std::io::stderr().flush();
                }
                std::thread::sleep(Duration::from_millis(80));
                i += 1;
            }
        });
        Spinner {
            label: label.to_string(),
            detail,
            history: Arc::new(Mutex::new(Vec::new())),
            started: Instant::now(),
            stop,
            paused,
            handle: Some(handle),
            silent: false,
            finished: false,
        }
    }

    impl Spinner {
        /// Update the animated detail text (e.g. bake sub-phase).
        pub fn set_detail(&self, detail: &str) {
            if let Ok(mut guard) = self.detail.lock() {
                *guard = detail.to_string();
            }
        }

        /// Temporarily clear the animation while a child owns the terminal.
        pub fn pause(&self) -> PauseGuard<'_> {
            self.paused.store(true, Ordering::Relaxed);
            {
                let _guard = lock_line();
                if stderr_tty() {
                    eprint!("\r\x1b[K");
                    let _ = std::io::stderr().flush();
                }
            }
            PauseGuard { spinner: self }
        }

        /// Milestone advance: the previous detail is retired as a permanent
        /// ✓ line and the new phase starts animating. Same text twice is a
        /// no-op, so repeated log markers don't spam history.
        pub fn advance(&self, detail: &str) {
            let prev = {
                let mut guard = match self.detail.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                if *guard == detail {
                    return;
                }
                std::mem::replace(&mut *guard, detail.to_string())
            };
            if self.silent {
                return;
            }
            if let Ok(mut history) = self.history.lock() {
                history.push(prev.clone());
            }
            let _guard = lock_line();
            if stderr_tty() {
                eprint!("\r\x1b[K");
            }
            eprintln!("  ✓ {prev}");
        }

        #[cfg(test)]
        pub(crate) fn history(&self) -> Vec<String> {
            self.history.lock().map(|g| g.clone()).unwrap_or_default()
        }

        fn finish(&mut self, ok: bool, msg: &str) {
            if self.finished {
                return;
            }
            self.finished = true;
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            DEPTH.set(DEPTH.get().saturating_sub(1));
            if self.silent {
                return;
            }
            let elapsed = fmt_elapsed(self.started.elapsed());
            let _guard = lock_line();
            if stderr_tty() {
                eprint!("\r\x1b[K");
            }
            let mark = if ok { "✓" } else { "✗" };
            eprintln!("  {mark} {} ({elapsed})", msg);
        }

        /// Step succeeded. Consumes the guard.
        pub fn succeed(mut self, msg: Option<&str>) {
            let label = self.label.clone();
            self.finish(true, msg.unwrap_or(&label));
        }

        /// Step failed (prints ✗; caller still returns the error).
        pub fn fail(mut self, msg: &str) {
            self.finish(false, msg);
        }
    }

    impl Drop for Spinner {
        fn drop(&mut self) {
            let label = self.label.clone();
            self.finish(true, &label);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn spin_lifecycle_no_hang() {
            let sp = spin("testing");
            sp.set_detail("still testing");
            sp.succeed(None);
        }

        #[test]
        fn nested_spins_are_silent() {
            let outer = spin("outer");
            let inner = spin("inner");
            assert!(inner.silent);
            inner.succeed(None);
            outer.succeed(None);
        }

        #[test]
        fn pause_resume_roundtrip() {
            let sp = spin("pausable");
            {
                let _p = sp.pause();
            }
            sp.succeed(None);
        }

        #[test]
        fn advance_records_milestones_once() {
            let sp = spin("phases");
            sp.advance("one");
            sp.advance("one"); // repeat marker: no duplicate history
            sp.advance("two");
            assert_eq!(
                sp.history(),
                vec!["phases".to_string(), "one".to_string()]
            );
            sp.succeed(None);
        }

        #[test]
        fn elapsed_formatting() {
            assert_eq!(fmt_elapsed(Duration::from_millis(350)), "0.3s");
            assert_eq!(fmt_elapsed(Duration::from_secs(5)), "5s");
            assert_eq!(fmt_elapsed(Duration::from_secs(90)), "1m 30s");
            assert_eq!(fmt_elapsed(Duration::from_secs(3700)), "1h 01m");
        }
    }
}
