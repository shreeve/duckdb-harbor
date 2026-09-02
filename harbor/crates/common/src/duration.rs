//! Parsing and formatting short durations — `--statement-timeout` on the
//! way in, an uptime on the way out.
//!
//! Two shapes, one each direction: [`parse_duration`] turns `"90s"`,
//! `"10m"`, `"2h"`, or bare seconds into a [`Duration`], and [`humanize`]
//! turns a [`Duration`] back into a coarse, readable `4m` / `1h12m` / `3d`.

use std::time::Duration;

/// `"90s"`, `"10m"`, `"2h"`, or bare seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    // Split off a trailing alphabetic unit. `trim_end_matches` works in whole
    // chars, so a multibyte unit (`5µs`, `5é`) can't land `split_at` inside a
    // UTF-8 char and panic — it just fails the unit match below.
    let num = s.trim_end_matches(char::is_alphabetic);
    let unit = &s[num.len()..];
    let n: u64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
    // checked, because `n * 3600` wraps in release: `9999999999999999h`
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

/// Coarse and readable: `4m`, `1m30s`, `1h12m`, `3d`.
///
/// Coarse, but never lossy in a way that misreports a value: truncating 90s
/// to "1m" made an uptime read as something other than what actually elapsed.
pub fn humanize(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => match (s / 60, s % 60) {
            (m, 0) => format!("{m}m"),
            (m, r) => format!("{m}m{r}s"),
        },
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
        // The one that used to lie: 90s must not read as "1m".
        assert_eq!(humanize(Duration::from_secs(90)), "1m30s");
        assert_eq!(humanize(Duration::from_secs(4320)), "1h12m");
        assert_eq!(humanize(Duration::from_secs(7200)), "2h");
        assert_eq!(humanize(Duration::from_secs(300000)), "3d");
    }
}
