//! How long a berth lives, and who decides.
//!
//! There are only two lifetimes, and one key expresses both:
//!
//! * **persistent** — runs until someone stops it. `idle-exit = "never"`.
//! * **ephemeral** — retires once nothing has used it for a while.
//!   `idle-exit = "90s"`.
//!
//! Everything the CLI can express is one of those two. "Start it and leave
//! it", "open it and let it go when I quit", "keep it up after ducktable
//! closes" are not three mechanisms — they are the same knob, set from three
//! places. Most specific wins:
//!
//! ```text
//! 1. a flag            --keep, --idle-exit 5m
//! 2. the entry         [connection.medlabs] idle-exit = "90s"
//! 3. the defaults      [defaults] idle-exit = "90s"
//! 4. who asked         harbor start -> persistent;  pilot -> ephemeral
//! ```
//!
//! Step 4 is the DWIM: a human who typed `harbor start` asked for a server
//! and gets one; a human who typed `pilot` asked for a prompt and gets a
//! process that cleans up after itself. Neither has to say so.
//!
//! # The joiner never changes the lifetime
//!
//! This is the invariant that makes the whole thing safe, and it is worth
//! stating out loud because getting it wrong is silently destructive: a
//! client that **joins** a berth already running inherits nothing and decides
//! nothing. Only the client that **summons** a berth sets its lifetime.
//!
//! Without that rule, opening ducktable against a long-running `medlabs` and
//! then closing the window would take the server — and everyone else on it —
//! down with you. With it, a berth's lifetime is fixed at birth by whoever
//! actually started it, and every later arrival is just a guest.

use std::time::Duration;

/// The default grace period for a berth a client summoned on its way to a
/// prompt: long enough to survive a reconnect, short enough that a forgotten
/// window does not leave a server behind.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lifetime {
    /// Runs until stopped.
    Persistent,
    /// Retires once nothing has used it for this long.
    Idle(Duration),
}

/// Who asked for this berth to exist.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Summoner {
    /// `harbor start` — a human asked for a server.
    Operator,
    /// `pilot`, `ducktable` — a client opened it on the way to a prompt.
    Client,
}

impl Lifetime {
    pub fn is_persistent(self) -> bool {
        self == Lifetime::Persistent
    }

    /// The `harbor serve` argv this lifetime means. Config is translated into
    /// flags rather than read twice, so `serve` stays flag-complete and a
    /// systemd unit or a container never needs a config file at all.
    pub fn to_args(self) -> Vec<String> {
        match self {
            Lifetime::Persistent => vec![],
            Lifetime::Idle(d) => vec!["--idle-exit".into(), format!("{}s", d.as_secs())],
        }
    }

    /// How to say it in a status line.
    pub fn describe(self) -> String {
        match self {
            Lifetime::Persistent => "never".to_string(),
            Lifetime::Idle(d) => humanize(d),
        }
    }
}

/// What the user wrote on the command line, if anything.
#[derive(Clone, Copy, Default, Debug)]
pub struct Override {
    /// `--keep`: leave it running after I disconnect.
    pub keep: bool,
    /// `--idle-exit <d>`: retire it this long after the last use.
    pub idle_exit: Option<Duration>,
}

/// Settle the lifetime of a berth about to be **summoned**.
///
/// Never call this for a berth that is already running — see the module note
/// on joiners.
pub fn resolve(
    flags: Override,
    entry: Option<&str>,
    defaults: Option<&str>,
    who: Summoner,
) -> Result<Lifetime, String> {
    if let Some(d) = flags.idle_exit {
        return Ok(Lifetime::Idle(d));
    }
    if flags.keep {
        return Ok(Lifetime::Persistent);
    }
    for spec in [entry, defaults].into_iter().flatten() {
        return parse(spec);
    }
    Ok(match who {
        Summoner::Operator => Lifetime::Persistent,
        Summoner::Client => Lifetime::Idle(DEFAULT_GRACE),
    })
}

/// `"never"` (or `"off"`), else a duration.
pub fn parse(spec: &str) -> Result<Lifetime, String> {
    match spec.trim() {
        "never" | "off" | "none" => Ok(Lifetime::Persistent),
        s => parse_duration(s).map(Lifetime::Idle),
    }
}

/// `"90s"`, `"10m"`, `"2h"`, or bare seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    // Split off a trailing alphabetic unit. `trim_end_matches` works in whole
    // chars, so a multibyte unit (`5µs`, `5é`) can't land `split_at` inside a
    // UTF-8 char and panic — it just fails the unit match below.
    let num = s.trim_end_matches(char::is_alphabetic);
    let unit = &s[num.len()..];
    let n: u64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
    // checked, because `n * 3600` wraps in release: `--idle-exit 9999999999999999h`
    // parsed fine and then meant something arbitrary and small.
    let secs = match unit {
        "" | "s" => Some(n),
        "m" => n.checked_mul(60),
        "h" => n.checked_mul(3600),
        _ => return Err(format!("bad duration unit in {s:?} (use s, m, h)")),
    };
    let secs = secs.ok_or_else(|| format!("duration {s:?} is too large"))?;
    Ok(Duration::from_secs(secs))
}

/// Coarse and readable: `4m`, `1h12m`, `3d`. Precision here is noise.
pub fn humanize(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => match (s / 3600, (s % 3600) / 60) {
            (h, 0) => format!("{h}h"),
            (h, m) => format!("{h}h{m}m"),
        },
        _ => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Option<&str> = None;

    #[test]
    fn who_asked_decides_when_nothing_else_does() {
        let f = Override::default();
        assert_eq!(resolve(f, NONE, NONE, Summoner::Operator).unwrap(), Lifetime::Persistent);
        assert_eq!(
            resolve(f, NONE, NONE, Summoner::Client).unwrap(),
            Lifetime::Idle(DEFAULT_GRACE)
        );
    }

    #[test]
    fn the_entry_beats_the_defaults_and_who_asked() {
        let f = Override::default();
        // A berth pinned persistent stays persistent even when a client
        // summoned it — this is what `harbor start` then `pilot` then quit
        // must not undo.
        assert_eq!(
            resolve(f, Some("never"), Some("90s"), Summoner::Client).unwrap(),
            Lifetime::Persistent
        );
        assert_eq!(
            resolve(f, Some("5m"), NONE, Summoner::Operator).unwrap(),
            Lifetime::Idle(Duration::from_secs(300))
        );
    }

    #[test]
    fn defaults_apply_to_anything_without_its_own() {
        let f = Override::default();
        assert_eq!(
            resolve(f, NONE, Some("30s"), Summoner::Operator).unwrap(),
            Lifetime::Idle(Duration::from_secs(30))
        );
    }

    #[test]
    fn flags_win_over_everything() {
        let keep = Override { keep: true, ..Default::default() };
        assert_eq!(
            resolve(keep, Some("90s"), Some("90s"), Summoner::Client).unwrap(),
            Lifetime::Persistent
        );
        let five = Override { idle_exit: Some(Duration::from_secs(300)), ..Default::default() };
        assert_eq!(
            resolve(five, Some("never"), NONE, Summoner::Operator).unwrap(),
            Lifetime::Idle(Duration::from_secs(300))
        );
    }

    #[test]
    fn persistent_passes_no_idle_flag_at_all() {
        assert!(Lifetime::Persistent.to_args().is_empty());
        assert_eq!(
            Lifetime::Idle(Duration::from_secs(90)).to_args(),
            ["--idle-exit", "90s"]
        );
    }

    #[test]
    fn never_is_spelled_several_reasonable_ways() {
        for s in ["never", "off", "none", " never "] {
            assert_eq!(parse(s).unwrap(), Lifetime::Persistent, "{s:?}");
        }
    }

    #[test]
    fn durations_do_not_wrap() {
        assert!(parse_duration("9999999999999999h").is_err());
        assert!(parse_duration("5µs").is_err());
        assert_eq!(parse_duration("120").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn humanized_uptime_is_coarse() {
        assert_eq!(humanize(Duration::from_secs(14)), "14s");
        assert_eq!(humanize(Duration::from_secs(240)), "4m");
        assert_eq!(humanize(Duration::from_secs(4320)), "1h12m");
        assert_eq!(humanize(Duration::from_secs(7200)), "2h");
        assert_eq!(humanize(Duration::from_secs(300000)), "3d");
    }
}
