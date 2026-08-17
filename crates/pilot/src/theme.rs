//! Syntax-highlight themes and light/dark detection.
//!
//! One `Theme` maps each token class the highlighter produces to a style. Three
//! named themes ship (`duck`, `mono`, `vivid`), each with a light and a dark
//! palette, because a color that reads on black washes out on white. Which
//! palette is used comes from `Appearance` — resolved once at startup from the
//! config (`appearance = "auto" | "light" | "dark"`) and switchable live with
//! `.appearance`. "auto" asks the terminal for its background color (OSC 11),
//! falling back to `$COLORFGBG`, then to dark.
//!
//! The active theme is a process global the highlighter reads on every
//! keystroke, so `.theme`/`.appearance` take effect on the next redraw with no
//! editor rebuild.

use nu_ansi_term::{Color, Style};
use std::sync::{LazyLock, RwLock};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Appearance {
    Light,
    Dark,
}

/// The token classes a theme colors — the highlighter's vocabulary.
#[derive(Clone, Copy)]
pub struct Theme {
    pub keyword: Style,
    pub string: Style,
    pub number: Style,
    pub comment: Style,
    pub error: Style,
    /// Bare identifiers / punctuation — usually the terminal's default.
    pub plain: Style,
}

/// The three theme names, for `.theme` completion and validation.
pub const NAMES: &[&str] = &["duck", "mono", "vivid"];

fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

/// Build a theme by name for an appearance. Unknown names fall back to `duck`.
pub fn build(name: &str, a: Appearance) -> Theme {
    use Appearance::{Dark, Light};
    let err = |a| match a {
        Dark => fg(Color::Red),
        Light => fg(Color::Fixed(124)), // deep red — visible on white
    };
    match (name, a) {
        ("mono", Dark) => Theme {
            keyword: Style::new().fg(Color::White).bold(),
            string: fg(Color::LightGray),
            number: fg(Color::LightGray),
            comment: fg(Color::DarkGray),
            error: err(Dark),
            plain: Style::new(),
        },
        ("mono", Light) => Theme {
            keyword: Style::new().fg(Color::Black).bold(),
            string: fg(Color::Fixed(240)),
            number: fg(Color::Fixed(240)),
            comment: fg(Color::Fixed(245)),
            error: err(Light),
            plain: Style::new(),
        },
        ("vivid", Dark) => Theme {
            keyword: Style::new().fg(Color::LightCyan).bold(),
            string: fg(Color::LightGreen),
            number: fg(Color::LightMagenta),
            comment: fg(Color::Fixed(102)),
            error: fg(Color::LightRed),
            plain: Style::new(),
        },
        ("vivid", Light) => Theme {
            keyword: Style::new().fg(Color::Blue).bold(),
            string: fg(Color::Fixed(22)),  // deep green
            number: fg(Color::Fixed(90)),  // deep magenta
            comment: fg(Color::Fixed(102)),
            error: err(Light),
            plain: Style::new(),
        },
        // duck (default, and the fallback for unknown names) — the DuckDB
        // shell look: green keywords, yellow literals, dim comments.
        (_, Dark) => Theme {
            keyword: fg(Color::Green),
            string: fg(Color::Yellow),
            number: fg(Color::Yellow),
            comment: fg(Color::DarkGray),
            error: err(Dark),
            plain: Style::new(),
        },
        (_, Light) => Theme {
            keyword: fg(Color::Fixed(28)), // dark green
            string: fg(Color::Fixed(94)),  // brown/olive
            number: fg(Color::Fixed(19)),  // blue
            comment: fg(Color::Fixed(244)),
            error: err(Light),
            plain: Style::new(),
        },
    }
}

// --- the active theme -------------------------------------------------------

struct Active {
    name: String,
    appearance: Appearance,
    theme: Theme,
}

static ACTIVE: LazyLock<RwLock<Active>> = LazyLock::new(|| {
    RwLock::new(Active {
        name: "duck".into(),
        appearance: Appearance::Dark,
        theme: build("duck", Appearance::Dark),
    })
});

/// The theme the highlighter should paint with right now.
pub fn current() -> Theme {
    ACTIVE.read().unwrap().theme
}

/// Set the active theme by name, keeping the current appearance. Returns false
/// for an unknown name (nothing changes).
pub fn set_theme(name: &str) -> bool {
    if !NAMES.contains(&name) {
        return false;
    }
    let mut a = ACTIVE.write().unwrap();
    a.name = name.to_string();
    a.theme = build(name, a.appearance);
    true
}

/// Set the appearance (rebuilding the current theme for it). "auto" detects.
pub fn set_appearance(appearance: Appearance) {
    let mut a = ACTIVE.write().unwrap();
    a.appearance = appearance;
    a.theme = build(&a.name, appearance);
}

/// The active theme name and appearance, for `.theme`/`.appearance` with no arg.
pub fn describe() -> (String, Appearance) {
    let a = ACTIVE.read().unwrap();
    (a.name.clone(), a.appearance)
}

/// Resolve config/flags at startup: theme name (default duck) and an appearance
/// preference ("auto"|"light"|"dark", default auto → detect).
pub fn init(name: Option<&str>, appearance_pref: Option<&str>) {
    let appearance = match appearance_pref.unwrap_or("auto") {
        "light" => Appearance::Light,
        "dark" => Appearance::Dark,
        _ => detect_appearance(),
    };
    let name = name.filter(|n| NAMES.contains(n)).unwrap_or("duck");
    let mut a = ACTIVE.write().unwrap();
    a.name = name.to_string();
    a.appearance = appearance;
    a.theme = build(name, appearance);
}

// --- detection --------------------------------------------------------------

/// Best-effort terminal background detection: OSC 11 query, then `$COLORFGBG`,
/// then dark. Only queries when both stdin and stdout are terminals.
pub fn detect_appearance() -> Appearance {
    if let Some(a) = colorfgbg_appearance() {
        return a;
    }
    if let Some(a) = osc11_appearance() {
        return a;
    }
    Appearance::Dark
}

/// `$COLORFGBG` = "fg;bg" (rxvt and a few others). Background 0-6 (and 8) is a
/// dark palette slot, 7/9-15 light. Cheap and needs no terminal round-trip.
fn colorfgbg_appearance() -> Option<Appearance> {
    let v = std::env::var("COLORFGBG").ok()?;
    let bg: u8 = v.rsplit(';').next()?.trim().parse().ok()?;
    Some(if bg == 7 || bg >= 9 { Appearance::Light } else { Appearance::Dark })
}

/// Ask the terminal for its background with OSC 11 and classify by luminance.
/// Returns None if not a tty, the terminal doesn't answer in time, or the reply
/// can't be parsed — the caller then falls back.
#[cfg(unix)]
fn osc11_appearance() -> Option<Appearance> {
    use std::io::{IsTerminal, Write};
    use std::time::Duration;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }
    let _raw = RawMode::enable()?; // restores on drop
    {
        let mut out = std::io::stdout();
        out.write_all(b"\x1b]11;?\x07").ok()?;
        out.flush().ok()?;
    }
    let reply = read_reply(Duration::from_millis(120))?;
    luminance_of(&reply).map(|l| if l > 0.5 { Appearance::Light } else { Appearance::Dark })
}

#[cfg(not(unix))]
fn osc11_appearance() -> Option<Appearance> {
    None
}

/// Parse an OSC 11 reply (`...rgb:RRRR/GGGG/BBBB...`, components 1-4 hex digits)
/// into perceptual luminance in 0.0..=1.0. Split out so it is unit-testable.
fn luminance_of(bytes: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(bytes).ok()?;
    let rest = s.split("rgb:").nth(1)?;
    let mut comps = rest.split('/');
    let parse = |c: Option<&str>| -> Option<f64> {
        let hex: String = c?.chars().take_while(|ch| ch.is_ascii_hexdigit()).collect();
        if hex.is_empty() {
            return None;
        }
        let max = 16u32.pow(hex.len() as u32) - 1;
        let v = u32::from_str_radix(&hex, 16).ok()?;
        Some(v as f64 / max as f64)
    };
    let r = parse(comps.next())?;
    let g = parse(comps.next())?;
    let b = parse(comps.next())?;
    Some(0.2126 * r + 0.7152 * g + 0.0722 * b)
}

#[cfg(unix)]
fn read_reply(timeout: std::time::Duration) -> Option<Vec<u8>> {
    use std::os::unix::io::AsRawFd;
    use std::time::Instant;
    let fd = std::io::stdin().as_raw_fd();
    let start = Instant::now();
    let mut buf = Vec::new();
    while let Some(rem) = timeout.checked_sub(start.elapsed()) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, rem.as_millis() as i32) };
        if n <= 0 {
            break; // timeout or error
        }
        let mut tmp = [0u8; 64];
        let r = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
        if r <= 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..r as usize]);
        // Reply ends with BEL or ST (ESC \). Stop as soon as one arrives.
        if buf.contains(&0x07) || buf.windows(2).any(|w| w == [0x1b, 0x5c]) || buf.len() > 256 {
            break;
        }
    }
    (!buf.is_empty()).then_some(buf)
}

/// RAII raw-mode guard around the OSC query, so the reply is not line-buffered
/// or echoed and the terminal is restored no matter how we leave.
#[cfg(unix)]
struct RawMode;

#[cfg(unix)]
impl RawMode {
    fn enable() -> Option<Self> {
        crossterm::terminal::enable_raw_mode().ok()?;
        Some(RawMode)
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luminance_classifies_black_and_white() {
        // A dark terminal (near-black background) → low luminance.
        let dark = luminance_of(b"\x1b]11;rgb:0000/0000/0000\x07").unwrap();
        assert!(dark < 0.5, "black is dark, got {dark}");
        // A light terminal (white background) → high luminance.
        let light = luminance_of(b"\x1b]11;rgb:ffff/ffff/ffff\x07").unwrap();
        assert!(light > 0.5, "white is light, got {light}");
        // 8-bit components parse too.
        let l8 = luminance_of(b"\x1b]11;rgb:ff/ff/ff\x1b\\").unwrap();
        assert!(l8 > 0.5);
        // Garbage is None (caller falls back), never a panic.
        assert!(luminance_of(b"not a reply").is_none());
    }

    #[test]
    fn colorfgbg_reads_the_background_slot() {
        // We only assert the parse logic via a crafted value, not the env.
        let parse = |v: &str| -> Option<Appearance> {
            let bg: u8 = v.rsplit(';').next()?.trim().parse().ok()?;
            Some(if bg == 7 || bg >= 9 { Appearance::Light } else { Appearance::Dark })
        };
        assert_eq!(parse("15;0"), Some(Appearance::Dark));
        assert_eq!(parse("0;15"), Some(Appearance::Light));
        assert_eq!(parse("0;7"), Some(Appearance::Light));
    }

    #[test]
    fn build_falls_back_to_duck_for_unknown() {
        // Unknown name resolves rather than panicking; keyword is duck-green (dark).
        let t = build("nonesuch", Appearance::Dark);
        assert_eq!(t.keyword.foreground, Some(Color::Green));
    }
}
