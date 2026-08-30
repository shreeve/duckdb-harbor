//! Small shared helpers.

pub fn clone_str(s: &str) -> String {
    s.to_string()
}

/// A count in compact SI form: exact under 1000, then k/M/G/T with one
/// decimal below 10 ("4.6M") and integers to 999 ("13k", "999k"); the
/// tenth-rounded value rolls to the next unit at 999.5.
pub fn human_count(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let mut value = n as f64;
    for unit in ["k", "M", "G", "T"] {
        value /= 1000.;
        if value < 999.5 || unit == "T" {
            let tenth = (value * 10.).round() / 10.;
            return if tenth >= 10. {
                format!("{}{unit}", value.round())
            } else {
                format!("{tenth:.1}{unit}")
            };
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::human_count;

    #[test]
    fn si_boundaries() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1_000), "1.0k");
        assert_eq!(human_count(9_949), "9.9k");
        assert_eq!(human_count(9_999), "10k");
        assert_eq!(human_count(99_999), "100k");
        assert_eq!(human_count(999_499), "999k");
        assert_eq!(human_count(999_500), "1.0M");
        assert_eq!(human_count(4_600_000), "4.6M");
        assert_eq!(human_count(14_964), "15k");
        assert_eq!(human_count(89_607), "90k");
    }
}
