//! Terminal presentation for the CLI: severity colors, health markers, and
//! the TTY/NO_COLOR policy (https://clig.dev/).
//!
//! Policy: an interactive stdout gets unicode markers, and ANSI color unless
//! NO_COLOR (any value) or --no-color asks otherwise. A non-interactive
//! stdout (pipe, redirect, capture) gets the same layout with ASCII marker
//! tags and never any escape code, so captured output stays paste- and
//! grep-clean. Machine modes (--json, --plain) bypass this module entirely.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

/// The global `--no-color` flag, recorded once at CLI startup so every
/// output site (status, setup, error reporting) honors it without plumbing
/// a bool through each call chain.
static NO_COLOR_FLAG: AtomicBool = AtomicBool::new(false);

pub fn set_no_color(v: bool) {
    NO_COLOR_FLAG.store(v, Ordering::Relaxed);
}

fn color_suppressed() -> bool {
    NO_COLOR_FLAG.load(Ordering::Relaxed) || std::env::var_os("NO_COLOR").is_some()
}

/// Severity of a state-carrying value, ordered so `max()` picks the worst.
///
/// A `Bad` field does not by itself mean the node is down: it means "this
/// line is broken". Whether that adds up to a critical verdict is the status
/// command's call (it keys on "is the node safely serving").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sev {
    Good,
    Warn,
    Bad,
}

pub struct Style {
    color: bool,
    unicode: bool,
}

impl Style {
    /// Detect presentation from stdout: an interactive terminal gets unicode
    /// markers, plus color unless NO_COLOR (any value) or --no-color is set.
    pub fn for_stdout() -> Self {
        Self::for_tty(std::io::stdout().is_terminal())
    }

    /// Same policy for stderr (error reporting).
    pub fn for_stderr() -> Self {
        Self::for_tty(std::io::stderr().is_terminal())
    }

    fn for_tty(tty: bool) -> Self {
        Style {
            color: tty && !color_suppressed(),
            unicode: tty,
        }
    }

    fn sgr(sev: Sev) -> &'static str {
        match sev {
            Sev::Good => "32",
            Sev::Warn => "33",
            Sev::Bad => "31",
        }
    }

    /// Color `text` by severity (identity without color).
    pub fn paint(&self, sev: Sev, text: &str) -> String {
        if self.color {
            format!("\x1b[{}m{text}\x1b[0m", Self::sgr(sev))
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[1m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn dim(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// Left gutter for a line that carries no health marker. Same display
    /// width as `marker()` so labels stay in one column.
    pub fn gutter(&self) -> &'static str {
        if self.unicode { "  " } else { "     " }
    }

    /// Left gutter carrying a health marker: `✓ ` / `⚠ ` / `✗ ` on a
    /// terminal, `[OK] ` / `[!!] ` / `[XX] ` otherwise.
    pub fn marker(&self, sev: Sev) -> String {
        if self.unicode {
            let sym = match sev {
                Sev::Good => "✓",
                Sev::Warn => "⚠",
                Sev::Bad => "✗",
            };
            format!("{} ", self.paint(sev, sym))
        } else {
            let tag = match sev {
                Sev::Good => "[OK]",
                Sev::Warn => "[!!]",
                Sev::Bad => "[XX]",
            };
            format!("{tag} ")
        }
    }

    /// The one-line health verdict that opens `status` output.
    pub fn verdict(&self, sev: Sev, word: &str, reason: &str) -> String {
        if self.unicode {
            let (dot, word) = if self.color {
                let sgr = Self::sgr(sev);
                (
                    format!("\x1b[1;{sgr}m●\x1b[0m"),
                    format!("\x1b[1;{sgr}m{word}\x1b[0m"),
                )
            } else {
                ("●".to_string(), word.to_string())
            };
            format!("{dot} {word} -- {reason}")
        } else {
            format!("health: {word} -- {reason}")
        }
    }
}

/// A CLI-level failure with operator guidance attached: what happened
/// (`msg`), optionally why the command refused in one sentence (`explain`),
/// and the next step (`next`). `report_error` prints `next` last, because
/// the last line is where the eye lands.
#[derive(Debug)]
pub struct CliError {
    msg: String,
    explain: Option<String>,
    next: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl CliError {
    pub fn new(msg: impl Into<String>, next: impl Into<String>) -> Self {
        CliError {
            msg: msg.into(),
            explain: None,
            next: next.into(),
            source: None,
        }
    }

    /// One sentence of why (printed between the message and the next step).
    pub fn explain(mut self, text: impl Into<String>) -> Self {
        self.explain = Some(text.into());
        self
    }

    /// Chain the underlying failure (printed as `caused by:`).
    pub fn caused_by(mut self, err: anyhow::Error) -> Self {
        self.source = Some(err.into());
        self
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// Print a command failure to stderr: `error: <what>`, the cause chain, then
/// the guidance (why + `next:`) when the error carries any.
pub fn report_error(err: &anyhow::Error) {
    let st = Style::for_stderr();
    eprintln!(
        "{} {err}",
        if st.color {
            "\x1b[1;31merror:\x1b[0m"
        } else {
            "error:"
        }
    );
    for cause in err.chain().skip(1) {
        eprintln!("  caused by: {cause}");
    }
    if let Some(g) = err.downcast_ref::<CliError>() {
        if let Some(x) = &g.explain {
            eprintln!();
            eprintln!("  {x}");
        }
        eprintln!();
        eprintln!("  {} {}", st.bold("next:"), g.next);
    }
}

const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Numbered step progress for multi-second commands. On a TTY each step gets
/// an animated spinner that resolves to a ✓/✗ line; piped output gets one
/// plain `[i/n] label` line per step and no animation (clig.dev: progress on
/// a terminal, silence in a pipe).
pub struct Steps {
    st: Style,
    total: usize,
    done: usize,
}

impl Steps {
    pub fn new(total: usize) -> Self {
        Steps {
            st: Style::for_stdout(),
            total,
            done: 0,
        }
    }

    pub async fn run<T, E, F>(&mut self, label: &str, fut: F) -> Result<T, E>
    where
        F: std::future::Future<Output = Result<T, E>>,
    {
        use std::io::Write;
        self.done += 1;
        let prefix = format!("[{}/{}]", self.done, self.total);
        if !self.st.unicode {
            println!("{prefix} {label}");
            return fut.await;
        }
        let start = std::time::Instant::now();
        let mut fut = std::pin::pin!(fut);
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(120));
        let mut frame = 0usize;
        loop {
            tokio::select! {
                res = &mut fut => {
                    let secs = start.elapsed().as_secs();
                    let took = if secs >= 2 { format!(" ({secs}s)") } else { String::new() };
                    let mark = match &res {
                        Ok(_) => self.st.paint(Sev::Good, "✓"),
                        Err(_) => self.st.paint(Sev::Bad, "✗"),
                    };
                    print!("\r\x1b[K");
                    println!("{mark} {prefix} {label}{took}");
                    return res;
                }
                _ = interval.tick() => {
                    print!("\r{} {prefix} {label}", SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]);
                    let _ = std::io::stdout().flush();
                    frame += 1;
                }
            }
        }
    }
}

/// A single self-updating wait line for poll loops ("waiting for the daemon
/// (12/90s)"). Every method is a no-op when stdout is not a terminal, so
/// callers keep their own plain-text progress lines for the piped case.
pub struct Spinner {
    st: Style,
    label: String,
    frame: usize,
    drawn: bool,
}

impl Spinner {
    pub fn new(label: impl Into<String>) -> Self {
        Spinner {
            st: Style::for_stdout(),
            label: label.into(),
            frame: 0,
            drawn: false,
        }
    }

    pub fn is_tty(&self) -> bool {
        self.st.unicode
    }

    /// Redraw the spinner line with a fresh note (elapsed time, current mode).
    pub fn tick(&mut self, note: &str) {
        use std::io::Write;
        if !self.st.unicode {
            return;
        }
        print!(
            "\r\x1b[K{} {}{note}",
            SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()],
            self.label
        );
        let _ = std::io::stdout().flush();
        self.frame += 1;
        self.drawn = true;
    }

    /// Print a persistent line above the spinner without corrupting it.
    pub fn println(&mut self, line: &str) {
        if self.st.unicode && self.drawn {
            print!("\r\x1b[K");
            self.drawn = false;
        }
        println!("{line}");
    }

    /// Erase the spinner line (call before the final result line).
    pub fn finish(&mut self) {
        use std::io::Write;
        if self.st.unicode && self.drawn {
            print!("\r\x1b[K");
            let _ = std::io::stdout().flush();
            self.drawn = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Style {
        Style {
            color: false,
            unicode: false,
        }
    }

    fn tty() -> Style {
        Style {
            color: true,
            unicode: true,
        }
    }

    #[test]
    fn non_tty_output_has_no_escape_codes() {
        let st = plain();
        for sev in [Sev::Good, Sev::Warn, Sev::Bad] {
            assert!(!st.paint(sev, "x").contains('\x1b'));
            assert!(!st.marker(sev).contains('\x1b'));
            assert!(!st.verdict(sev, "degraded", "r").contains('\x1b'));
        }
        assert!(!st.bold("x").contains('\x1b'));
        assert!(!st.dim("x").contains('\x1b'));
    }

    #[test]
    fn non_tty_gutters_share_one_width() {
        let st = plain();
        assert_eq!(st.gutter().len(), st.marker(Sev::Good).len());
        assert_eq!(st.gutter().len(), st.marker(Sev::Bad).len());
    }

    #[test]
    fn tty_gutters_share_one_display_width() {
        let st = tty();
        // Strip the escape codes, then compare character counts (the marker
        // symbols are single-width).
        let stripped = |s: &str| {
            let mut out = String::new();
            let mut in_esc = false;
            for c in s.chars() {
                match (in_esc, c) {
                    (false, '\x1b') => in_esc = true,
                    (false, c) => out.push(c),
                    (true, 'm') => in_esc = false,
                    (true, _) => {}
                }
            }
            out
        };
        assert_eq!(
            st.gutter().chars().count(),
            stripped(&st.marker(Sev::Warn)).chars().count()
        );
    }

    #[test]
    fn severity_orders_worst_last() {
        assert!(Sev::Good < Sev::Warn && Sev::Warn < Sev::Bad);
    }

    #[test]
    fn cli_error_guidance_survives_anyhow() {
        let e: anyhow::Error = CliError::new("refused", "re-run later")
            .explain("because")
            .caused_by(anyhow::anyhow!("root cause"))
            .into();
        assert_eq!(e.to_string(), "refused");
        let g = e.downcast_ref::<CliError>().expect("guidance reachable");
        assert_eq!(g.next, "re-run later");
        assert_eq!(g.explain.as_deref(), Some("because"));
        // The cause chain still walks to the underlying error.
        assert_eq!(e.chain().count(), 2);
        assert_eq!(e.chain().nth(1).unwrap().to_string(), "root cause");
    }
}
