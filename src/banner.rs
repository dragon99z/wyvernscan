//! The ASCII-art startup banner for console mode.
//!
//! Pure `std`, like the rest of `cli.rs`'s dependencies: it must work on a
//! server with no display, so nothing here may touch `eframe`/`egui`.
//!
//! The art is plain 7-bit ASCII on purpose. Box-drawing or block characters
//! turn into mojibake on legacy Windows code pages and some serial/SSH
//! consoles, and a banner that garbles is worse than none. Color is added
//! only where it is known to work (see `ansi_ok`) and never changes the
//! text itself, so stripping the escapes always gives the plain art back.

use std::io::IsTerminal;

/// "WyvernScan" in the figlet "standard" font (60 columns wide).
const WORDMARK: [&str; 6] = [
    r"__        __                          ____",
    r"\ \      / /   ___   _____ _ __ _ __ / ___|  ___ __ _ _ __",
    r" \ \ /\ / / | | \ \ / / _ \ '__| '_ \\___ \ / __/ _` | '_ \",
    r"  \ V  V /| |_| |\ V /  __/ |  | | | |___) | (_| (_| | | | |",
    r"   \_/\_/  \__, | \_/ \___|_|  |_| |_|____/ \___\__,_|_| |_|",
    r"           |___/",
];

const TAGLINE: &str = "see what's eating your disk";
const WIDTH: usize = 60;

// Basic 16-color SGR codes only: they work on every terminal that supports
// color at all, unlike 256-color or truecolor.
const GREEN: &str = "\x1b[1;32m";
const DIM_GREEN: &str = "\x1b[32m";
const AMBER: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// Builds the banner. `color` adds ANSI escapes and nothing else.
///
/// The underline is the wyvern's spine running into its barbed tail: a row
/// of dorsal spikes that tapers into a tail ending in a stinger.
pub fn render(color: bool) -> String {
    let mut out = String::new();
    out.push('\n');
    for line in WORDMARK {
        out.push(' ');
        out.push_str(&paint(color, GREEN, line));
        out.push('\n');
    }

    let spine = "/\\".repeat(25);
    let tail = "~~~~~--=<>";
    out.push(' ');
    out.push_str(&paint(color, DIM_GREEN, &spine));
    out.push_str(&paint(color, AMBER, tail));
    out.push('\n');

    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let pad = WIDTH.saturating_sub(TAGLINE.len() + version.len()).max(1);
    out.push(' ');
    out.push_str(&paint(color, AMBER, TAGLINE));
    out.push_str(&" ".repeat(pad));
    out.push_str(&paint(color, DIM, &version));
    out.push_str("\n\n");
    out
}

/// Whether it is safe to emit ANSI escapes. Conservative on Windows, where
/// the classic console host prints escapes literally unless virtual-terminal
/// processing was switched on; Windows Terminal and mintty/Git Bash announce
/// themselves through the variables checked below.
fn ansi_ok() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(std::env::var("TERM").as_deref(), Ok("dumb")) {
        return false;
    }
    if cfg!(windows) {
        return std::env::var_os("WT_SESSION").is_some()
            || std::env::var_os("TERM").is_some()
            || std::env::var_os("ANSICON").is_some()
            || matches!(std::env::var("ConEmuANSI").as_deref(), Ok("ON"));
    }
    true
}

/// Prints the banner to stderr, but only when stderr is a terminal: the
/// report goes to stdout and progress to stderr, and a banner landing in a
/// cron log or a redirected file would just be noise.
pub fn print_to_stderr() {
    if std::io::stderr().is_terminal() {
        eprint!("{}", render(ansi_ok()));
    }
}

/// Same, for `--help`, which prints to stdout.
pub fn print_to_stdout() {
    if std::io::stdout().is_terminal() {
        print!("{}", render(ansi_ok()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removes SGR escape sequences, to compare colored output to plain.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn plain_banner_is_ascii_and_fits_80_columns() {
        let plain = render(false);
        assert!(plain.is_ascii(), "banner must stay 7-bit ASCII");
        assert!(!plain.contains('\x1b'), "plain banner must have no escapes");
        for line in plain.lines() {
            assert!(line.len() <= 79, "line too wide ({}): {line:?}", line.len());
        }
    }

    #[test]
    fn color_only_adds_escapes() {
        let colored = render(true);
        assert!(colored.contains('\x1b'));
        assert_eq!(strip_ansi(&colored), render(false));
    }

    #[test]
    fn banner_names_the_tool_and_version() {
        let plain = render(false);
        assert!(plain.contains(TAGLINE));
        assert!(plain.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }
}
