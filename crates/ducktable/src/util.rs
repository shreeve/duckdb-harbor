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

/// A number in compact decimal form on the full SI ladder, in BOTH
/// directions: values scale up by thousands (k, M, G, T) and down
/// (m, \u{00b5}, n) until they sit in 1..999, shown with one decimal
/// below ten ONLY when that decimal says something ("1.4s", "9.9k" —
/// but "1k", never "1.0k") and integers to 999; the tenth-rounded
/// value rolls to the next magnitude at 999.5. `unit`
/// suffixes every form \u{2014} "B" gives Finder-style sizes ("999B",
/// "8.5MB"), "" bare counts ("13k", "4.6M"), "s" durations
/// ("132ms", "1.4s", "12ks"). The kilo prefix is SI's lowercase k,
/// except for bytes, where Finder's colloquial KB wins.
pub fn human(n: f64, unit: &str) -> String {
    if n == 0. {
        return format!("0{unit}");
    }
    let kilo = if unit == "B" { "K" } else { "k" };
    let mut value = n;
    let mut prefix = "";
    for up in [kilo, "M", "G", "T"] {
        if value < 999.5 {
            break;
        }
        value /= 1000.;
        prefix = up;
    }
    for down in ["m", "\u{00b5}", "n"] {
        if value >= 0.9995 {
            break;
        }
        value *= 1000.;
        prefix = down;
    }
    let tenth = (value * 10.).round() / 10.;
    if tenth >= 10. || tenth.fract() == 0. {
        format!("{}{prefix}{unit}", value.round())
    } else {
        format!("{tenth:.1}{prefix}{unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::human;

    #[test]
    fn si_boundaries() {
        assert_eq!(human(0., ""), "0");
        assert_eq!(human(999., ""), "999");
        assert_eq!(human(1_000., ""), "1k");
        assert_eq!(human(9_949., ""), "9.9k");
        assert_eq!(human(9_999., ""), "10k");
        assert_eq!(human(99_999., ""), "100k");
        assert_eq!(human(999_499., ""), "999k");
        assert_eq!(human(999_500., ""), "1M");
        assert_eq!(human(4_600_000., ""), "4.6M");
        assert_eq!(human(14_964., ""), "15k");
        assert_eq!(human(89_607., ""), "90k");
    }

    #[test]
    fn unit_boundaries() {
        assert_eq!(human(0., "B"), "0B");
        assert_eq!(human(999., "B"), "999B");
        assert_eq!(human(13_000., "B"), "13KB");
        assert_eq!(human(8_500_000., "B"), "8.5MB");
        assert_eq!(human(242_000_000., "B"), "242MB");
        assert_eq!(human(1_200_000_000., "B"), "1.2GB");
    }

    #[test]
    fn duration_ladder() {
        // Durations are just seconds riding the ladder: milliseconds
        // are one rung down, kiloseconds one rung up.
        let ms = |n: u64| human(n as f64 / 1000., "s");
        assert_eq!(ms(0), "0s");
        assert_eq!(ms(1), "1ms");
        assert_eq!(ms(13), "13ms");
        assert_eq!(ms(130), "130ms");
        assert_eq!(ms(999), "999ms");
        assert_eq!(ms(1_000), "1s");
        assert_eq!(ms(1_049), "1s");
        assert_eq!(ms(1_050), "1.1s");
        assert_eq!(ms(1_400), "1.4s");
        assert_eq!(ms(4_300), "4.3s");
        assert_eq!(ms(9_949), "9.9s");
        assert_eq!(ms(9_999), "10s");
        assert_eq!(ms(130_000), "130s");
        assert_eq!(ms(999_500), "1ks");
        assert_eq!(ms(12_000_000), "12ks");
        // The rung below, ready for a wire that reports finer than ms.
        assert_eq!(human(0.0024, "s"), "2.4ms");
        assert_eq!(human(0.000_004_3, "s"), "4.3\u{00b5}s");
    }
}
