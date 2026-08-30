//! `harbor doctor` — the checks nothing else has a moment to make.
//!
//! Lives in harbor rather than in `harbor-common` because harbor is the only
//! thing that runs it. Pilot connects; it does not audit. If ducktable ever
//! wants a problems panel, this moves back — but not before.
//!
//! The discipline that keeps this verb useful: **doctor reports only what no
//! other command would naturally notice.** A database that has moved is
//! doctor's business, because nothing looks until you try to start it. A
//! misspelled key is not, because the parser already refuses the file and
//! names the key. Every check that duplicates a good error message makes the
//! output longer and the signal weaker, which is how a doctor command ends
//! up as a wall of warnings nobody reads.
//!
//! Every finding names the fix. A finding with no fix is a complaint.

use harbor_common::config::FileConfig;
use harbor_common::paths::shorten;
use harbor_common::ui::Tone;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    /// Something is already broken, or will be the moment it is used.
    Error,
    /// Works today, will surprise someone.
    Warn,
}

impl Severity {
    pub fn tone(self) -> Tone {
        match self {
            Severity::Error => Tone::Red,
            Severity::Warn => Tone::Yellow,
        }
    }
    pub fn glyph(self) -> &'static str {
        match self {
            Severity::Error => "✕",
            Severity::Warn => "▲",
        }
    }
}

#[derive(Debug)]
pub struct Finding {
    pub severity: Severity,
    /// One line: what is wrong.
    pub title: String,
    /// The evidence — paths, names, what disagrees with what.
    pub detail: Vec<String>,
    /// What to run or edit. Never empty.
    pub fix: String,
}

/// Entries that set neither `path` nor `url`, or both.
///
/// Both is the interesting one: it is a question only the author can answer,
/// and picking either silently is how you end up querying the wrong database
/// while everything looks fine.
pub fn check_entries(cfg: &FileConfig) -> Vec<Finding> {
    let mut out = Vec::new();
    for name in cfg.malformed() {
        let c = cfg.get(name).expect("named by malformed()");
        let (title, fix) = match (c.path.is_some(), c.url.is_some()) {
            (true, true) => (
                format!("[connection.{name}] sets both path and url"),
                format!("keep one: path makes {name} a local berth, url makes it a remote"),
            ),
            _ => (
                format!("[connection.{name}] sets neither path nor url"),
                format!("give {name} a path (a local database) or a url (a remote)"),
            ),
        };
        out.push(Finding { severity: Severity::Error, title, detail: vec![], fix });
    }
    out
}

/// Two names for one database file.
///
/// DuckDB takes a single writer, so two berths on one file cannot both run —
/// the second loses the lock and dies with a message about the *name*, which
/// is not where the problem is. Nothing else looks across entries, so this is
/// doctor's most useful check.
pub fn check_duplicates(cfg: &FileConfig, canon: impl Fn(&Path) -> PathBuf) -> Vec<Finding> {
    let mut seen: HashMap<PathBuf, Vec<&str>> = HashMap::new();
    for (name, c) in cfg.berths() {
        if let Some(p) = c.database() {
            seen.entry(canon(&p)).or_default().push(name);
        }
    }
    let mut out: Vec<Finding> = seen
        .into_iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(path, mut names)| {
            names.sort_unstable();
            Finding {
                severity: Severity::Error,
                title: format!("{} names one database", names.join(" and ")),
                detail: vec![shorten(&path)],
                fix: format!(
                    "DuckDB allows one writer — drop all but one of them (harbor forget {})",
                    names[1..].join(", harbor forget ")
                ),
            }
        })
        .collect();
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

/// Berths whose database is not where the config says.
///
/// Latent by nature: the entry is fine, the file moved, and you find out the
/// next time you start it — which for an `autostart` berth is at login, in a
/// context with nobody watching.
pub fn check_databases(cfg: &FileConfig, exists: impl Fn(&Path) -> bool) -> Vec<Finding> {
    let mut out = Vec::new();
    for (name, c) in cfg.berths() {
        let Some(p) = c.database() else { continue };
        if exists(&p) {
            continue;
        }
        let auto = c.autostart == Some(true);
        out.push(Finding {
            severity: if auto { Severity::Error } else { Severity::Warn },
            title: format!("{name} points at a database that is not there"),
            detail: {
                let mut d = vec![shorten(&p)];
                if auto {
                    d.push("and it is marked autostart, so this fails at login".into());
                }
                d
            },
            fix: format!("fix the path in [connection.{name}], or harbor forget {name}"),
        });
    }
    out
}

/// `token-file` pointing at nothing. The connection looks configured and
/// fails with an auth error, which sends people looking in the wrong place.
pub fn check_tokens(cfg: &FileConfig, exists: impl Fn(&Path) -> bool) -> Vec<Finding> {
    let mut out = Vec::new();
    for (name, c) in cfg.remotes() {
        let Some(tf) = c.token_file.as_deref() else { continue };
        let p = harbor_common::paths::expand(tf);
        if exists(&p) {
            continue;
        }
        out.push(Finding {
            severity: Severity::Warn,
            title: format!("{name} has a token-file that is not there"),
            detail: vec![shorten(&p)],
            fix: format!("fix token-file in [connection.{name}], or use token-cmd"),
        });
    }
    out
}

/// A relative `$HARBOR_HOME` is ignored rather than resolved — deliberately,
/// since a relative state root moves with the working directory. But it is
/// ignored *silently*, so someone who set it deserves to be told.
pub fn check_environment(var: Option<&str>) -> Vec<Finding> {
    let Some(v) = var else { return vec![] };
    if v.is_empty() || Path::new(v).is_absolute() {
        return vec![];
    }
    vec![Finding {
        severity: Severity::Warn,
        title: "$HARBOR_HOME is relative and is being ignored".into(),
        detail: vec![
            format!("HARBOR_HOME={v}"),
            "a relative root would move the fleet with the working directory".into(),
        ],
        fix: "set it to an absolute path, or unset it".into(),
    }]
}

/// The checks that touch nothing.
///
/// `harbor show` runs these on every invocation and prints a one-line
/// pointer, because a doctor nobody runs is a doctor that finds nothing. It
/// must never run the rest: `exists()` and `canonicalize()` on a path whose
/// mount is gone block for the mount timeout, and turning an instant command
/// into an occasional hang is a poor trade for a check nobody asked for.
///
/// The duplicate check here compares expanded paths as written, which catches
/// the realistic case — a copy-pasted entry — and misses symlink aliasing.
/// `doctor` canonicalizes and catches both.
pub fn quick(cfg: &FileConfig) -> Vec<Finding> {
    let mut out = check_entries(cfg);
    out.extend(check_duplicates(cfg, |p| p.to_path_buf()));
    out.extend(check_environment(std::env::var("HARBOR_HOME").ok().as_deref()));
    out.sort_by_key(|f| f.severity);
    out
}

/// The footer `show` prints when `quick` found something — and nothing at
/// all when it did not. A clean fleet gets a clean screen.
///
/// Deliberately a count and a verb, not the findings themselves: `show`
/// answers "what do I have", and burying that under diagnostics every time
/// is how a status view becomes a wall nobody reads.
pub fn summary(findings: &[Finding]) -> Option<(Severity, String)> {
    let worst = findings.iter().map(|f| f.severity).min()?;
    let n = findings.len();
    let noun = if n == 1 { "problem" } else { "problems" };
    Some((worst, format!("{} {n} {noun} in your config — harbor doctor", worst.glyph())))
}

/// Everything, against the real filesystem.
pub fn examine(cfg: &FileConfig) -> Vec<Finding> {
    let exists = |p: &Path| p.exists();
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = check_entries(cfg);
    out.extend(check_duplicates(cfg, canon));
    out.extend(check_databases(cfg, exists));
    out.extend(check_tokens(cfg, exists));
    out.extend(check_environment(std::env::var("HARBOR_HOME").ok().as_deref()));
    out.sort_by_key(|f| f.severity);
    out
}

/// Non-zero when anything needs a human, so `harbor doctor` works in a health
/// check.
pub fn exit_code(findings: &[Finding]) -> u8 {
    u8::from(!findings.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(s: &str) -> FileConfig {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn two_names_for_one_database_is_an_error() {
        let c = cfg(r#"
            [connection.medlabs]
            path = "/data/medlabs.duckdb"
            [connection.backup]
            path = "/data/medlabs.duckdb"
            [connection.other]
            path = "/data/other.duckdb"
        "#);
        let f = check_duplicates(&c, |p| p.to_path_buf());
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Error);
        assert!(f[0].title.contains("backup and medlabs"), "{}", f[0].title);
        assert!(f[0].fix.contains("harbor forget"));
    }

    #[test]
    fn distinct_paths_that_canonicalize_together_are_caught() {
        // The whole reason to canonicalize: a symlink and its target are one
        // database, and DuckDB will not let both berths run.
        let c = cfg(r#"
            [connection.a]
            path = "/data/link.duckdb"
            [connection.b]
            path = "/real/db.duckdb"
        "#);
        let same = |_: &Path| PathBuf::from("/real/db.duckdb");
        assert_eq!(check_duplicates(&c, same).len(), 1);
    }

    #[test]
    fn a_missing_database_is_worse_when_it_autostarts() {
        let c = cfg(r#"
            [connection.quiet]
            path = "/gone/a.duckdb"
            [connection.loud]
            path = "/gone/b.duckdb"
            autostart = true
        "#);
        let f = check_databases(&c, |_| false);
        let loud = f.iter().find(|f| f.title.starts_with("loud")).unwrap();
        let quiet = f.iter().find(|f| f.title.starts_with("quiet")).unwrap();
        assert_eq!(loud.severity, Severity::Error);
        assert_eq!(quiet.severity, Severity::Warn);
        assert!(loud.detail.iter().any(|d| d.contains("at login")));
    }

    #[test]
    fn present_databases_say_nothing_at_all() {
        let c = cfg("[connection.ok]\npath = \"/data/ok.duckdb\"\n");
        assert!(check_databases(&c, |_| true).is_empty());
        assert!(check_entries(&c).is_empty());
        assert!(check_duplicates(&c, |p| p.to_path_buf()).is_empty());
    }

    #[test]
    fn both_path_and_url_asks_the_author_rather_than_guessing() {
        let c = cfg("[connection.x]\npath = \"/a.duckdb\"\nurl = \"https://b\"\n");
        let f = check_entries(&c);
        assert_eq!(f[0].severity, Severity::Error);
        assert!(f[0].fix.contains("keep one"));
    }

    #[test]
    fn a_relative_harbor_home_is_reported_because_it_is_ignored() {
        assert!(check_environment(Some("/abs/ok")).is_empty());
        assert!(check_environment(None).is_empty());
        let f = check_environment(Some("relative/harbor"));
        assert_eq!(f.len(), 1);
        assert!(f[0].fix.contains("absolute"));
    }

    #[test]
    fn every_finding_carries_a_fix() {
        let c = cfg(r#"
            [connection.dup1]
            path = "/d.duckdb"
            [connection.dup2]
            path = "/d.duckdb"
            [connection.bad]
            [connection.remote]
            url = "https://x"
            token-file = "/gone/t"
        "#);
        let mut all = check_entries(&c);
        all.extend(check_duplicates(&c, |p| p.to_path_buf()));
        all.extend(check_databases(&c, |_| false));
        all.extend(check_tokens(&c, |_| false));
        assert!(!all.is_empty());
        for f in &all {
            assert!(!f.fix.trim().is_empty(), "no fix: {}", f.title);
            assert!(!f.title.trim().is_empty());
        }
        assert_eq!(exit_code(&all), 1);
    }

    #[test]
    fn a_clean_config_exits_zero() {
        assert_eq!(exit_code(&[]), 0);
    }

    #[test]
    fn quick_touches_no_filesystem_at_all() {
        // The guard that keeps `harbor show` instant: a berth pointed at a
        // path that would block on a dead mount must not be probed here.
        let c = cfg(r#"
            [connection.a]
            path = "/net/dead-mount/a.duckdb"
            [connection.b]
            path = "/net/dead-mount/a.duckdb"
        "#);
        let f = quick(&c);
        // Found the duplicate without asking the filesystem anything.
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("a and b"));
    }

    #[test]
    fn a_clean_fleet_gets_a_clean_screen() {
        assert!(summary(&[]).is_none());
        let c = cfg("[connection.ok]\npath = \"/data/ok.duckdb\"\n");
        assert!(summary(&quick(&c)).is_none());
    }

    #[test]
    fn the_footer_counts_and_points_and_takes_the_worst_tone() {
        let c = cfg(r#"
            [connection.dup1]
            path = "/d.duckdb"
            [connection.dup2]
            path = "/d.duckdb"
            [connection.bad]
        "#);
        let (sev, line) = summary(&quick(&c)).unwrap();
        assert_eq!(sev, Severity::Error);
        assert!(line.contains("2 problems"), "{line}");
        assert!(line.ends_with("harbor doctor"), "{line}");
        // One problem reads as one problem.
        let one = cfg("[connection.bad]\n");
        assert!(summary(&quick(&one)).unwrap().1.contains("1 problem in"));
    }
}
