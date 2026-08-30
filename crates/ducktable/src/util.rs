//! Small shared helpers.

/// A deliberate, greppable string clone: call sites that copy a name to
/// move it into a task or a new owner say so by name, so an audit for
/// accidental cloning can skip them.
pub fn clone_str(s: &str) -> String {
    s.to_string()
}

/// A DuckDB identifier, double-quoted with embedded quotes doubled.
pub fn qident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// An exact count with thousands separators: 1117569 -> "1,117,569".
pub fn commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A number in compact decimal form: exact under 1000, then scaled by
/// thousands with one decimal below 10 and integers to 999; the
/// tenth-rounded value rolls to the next magnitude at 999.5. `unit`
/// suffixes every form — "B" gives Finder-style sizes ("999B", "8.5MB",
/// "1.2GB"), "" gives bare counts ("999", "13k", "4.6M") — and the kilo
/// prefix follows it: lowercase alone ("13k"), uppercase with a unit
/// ("13KB").
pub fn human(n: u64, unit: &str) -> String {
    if n < 1000 {
        return format!("{n}{unit}");
    }
    let kilo = if unit.is_empty() { "k" } else { "K" };
    let mut value = n as f64;
    for prefix in [kilo, "M", "G", "T"] {
        value /= 1000.;
        if value < 999.5 || prefix == "T" {
            let tenth = (value * 10.).round() / 10.;
            return if tenth >= 10. {
                format!("{}{prefix}{unit}", value.round())
            } else {
                format!("{tenth:.1}{prefix}{unit}")
            };
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::human;

    #[test]
    fn si_boundaries() {
        assert_eq!(human(0, ""), "0");
        assert_eq!(human(999, ""), "999");
        assert_eq!(human(1_000, ""), "1.0k");
        assert_eq!(human(9_949, ""), "9.9k");
        assert_eq!(human(9_999, ""), "10k");
        assert_eq!(human(99_999, ""), "100k");
        assert_eq!(human(999_499, ""), "999k");
        assert_eq!(human(999_500, ""), "1.0M");
        assert_eq!(human(4_600_000, ""), "4.6M");
        assert_eq!(human(14_964, ""), "15k");
        assert_eq!(human(89_607, ""), "90k");
    }

    #[test]
    fn unit_boundaries() {
        assert_eq!(human(0, "B"), "0B");
        assert_eq!(human(999, "B"), "999B");
        assert_eq!(human(13_000, "B"), "13KB");
        assert_eq!(human(8_500_000, "B"), "8.5MB");
        assert_eq!(human(242_000_000, "B"), "242MB");
        assert_eq!(human(1_200_000_000, "B"), "1.2GB");
    }
}
