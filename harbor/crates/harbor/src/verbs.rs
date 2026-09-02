//! The verb grammar: `harbor <db> [attach|detach] [start|stop] [autostart]`.
//!
//! A bag of BARE verbs — no `--options` — in any order, at most one per axis.
//! The bag is validated as a whole (nothing half-applies) and resolved into a
//! [`Plan`] the caller enacts. Two axes:
//!
//!   membership — attach / detach   (on your list; persisted in config)
//!   running    — start / stop      (green now)
//!
//! and one property, `autostart` (launch at login), which HARD-implies attach
//! and SOFT-defaults start (an explicit `stop` overrides the start, giving
//! "armed for login but off right now").
//!
//! Membership carries the lifetime: a running server that is attached is
//! persistent; one that is detached is ephemeral (what `--ephemeral` used to
//! spell). So `detach start` is the ephemeral start, `attach start` the
//! persistent one.

// The dispatch that calls these lands in the next layer; for now only the
// tests exercise them, so quiet dead-code until `main` wires the grammar in.
#![allow(dead_code)]

use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Verb {
    Attach,
    Detach,
    Start,
    Stop,
    Autostart,
}

impl Verb {
    pub fn parse(s: &str) -> Option<Verb> {
        Some(match s {
            "attach" => Verb::Attach,
            "detach" => Verb::Detach,
            "start" => Verb::Start,
            "stop" => Verb::Stop,
            "autostart" => Verb::Autostart,
            _ => return None,
        })
    }

    /// Is this token one of our verbs? Lets the dispatcher tell a management
    /// invocation (`harbor <db> start`) from a client one (`harbor <db> -c …`).
    pub fn is_verb(s: &str) -> bool {
        Verb::parse(s).is_some()
    }

    fn as_str(self) -> &'static str {
        match self {
            Verb::Attach => "attach",
            Verb::Detach => "detach",
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Autostart => "autostart",
        }
    }
}

/// What a validated verb bag resolves to. `None` on an axis means "leave it
/// unchanged"; `Some(true)`/`Some(false)` are the two directions.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// membership: Some(true)=attach, Some(false)=detach, None=unchanged.
    pub attach: Option<bool>,
    /// running: Some(true)=start, Some(false)=stop, None=unchanged.
    pub run: Option<bool>,
    /// install the login item (implies attach + start-unless-stop).
    pub autostart: bool,
}

impl Plan {
    /// A running server is ephemeral exactly when it is being started while
    /// detached — membership carries the lifetime. (`attach start` and a bare
    /// `start` are persistent; only `detach start` is ephemeral.)
    pub fn ephemeral(&self) -> bool {
        self.run == Some(true) && self.attach == Some(false)
    }
}

/// Validate a bag of verb words and resolve it — the whole pipeline the CLI
/// designed:
///
///   1) grab all verbs           4) error if any valid verb repeats
///   2) count each               5) error on a conflicting pair
///   3) error on any unknown      6) everything else is valid → resolve
///
/// On any error nothing is applied (the caller only acts on `Ok`).
pub fn plan(words: &[String]) -> Result<Plan, String> {
    // 1-3) parse each word, rejecting unknowns; 2) count on the way.
    let mut counts: HashMap<Verb, u32> = HashMap::new();
    for w in words {
        let v = Verb::parse(w).ok_or_else(|| {
            format!("unknown verb '{w}' — verbs are: attach, detach, start, stop, autostart")
        })?;
        *counts.entry(v).or_insert(0) += 1;
    }

    // 4) a verb given more than once is a malformed request, not a doubling.
    for (v, n) in &counts {
        if *n > 1 {
            return Err(format!("'{}' given more than once", v.as_str()));
        }
    }

    let has = |v: Verb| counts.contains_key(&v);

    // 5) the three conflicts: the two same-axis opposites, plus autostart's
    // hard-attach against detach. (autostart + stop is NOT a conflict — stop
    // is a soft override of autostart's default start.)
    if has(Verb::Attach) && has(Verb::Detach) {
        return Err("can't attach and detach at once — pick one".into());
    }
    if has(Verb::Start) && has(Verb::Stop) {
        return Err("can't start and stop at once — pick one".into());
    }
    if has(Verb::Autostart) && has(Verb::Detach) {
        return Err(
            "autostart keeps the database (it implies attach); drop detach or drop autostart"
                .into(),
        );
    }

    // 6) resolve. autostart pre-fills attach (hard) and start (default), then
    // any explicit verb wins — except detach, ruled out above.
    let autostart = has(Verb::Autostart);
    let attach = if has(Verb::Attach) || autostart {
        Some(true)
    } else if has(Verb::Detach) {
        Some(false)
    } else {
        None
    };
    let run = if has(Verb::Start) {
        Some(true)
    } else if has(Verb::Stop) {
        Some(false)
    } else if autostart {
        Some(true) // autostart's soft default, no explicit stop overrode it
    } else {
        None
    };

    Ok(Plan { attach, run, autostart })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bag(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }
    fn ok(s: &str) -> Plan {
        plan(&bag(s)).expect("should be valid")
    }
    fn err(s: &str) -> String {
        plan(&bag(s)).expect_err("should be invalid")
    }

    #[test]
    fn singles() {
        assert_eq!(ok("attach"), Plan { attach: Some(true), run: None, autostart: false });
        assert_eq!(ok("detach"), Plan { attach: Some(false), run: None, autostart: false });
        assert_eq!(ok("start"), Plan { attach: None, run: Some(true), autostart: false });
        assert_eq!(ok("stop"), Plan { attach: None, run: Some(false), autostart: false });
    }

    #[test]
    fn empty_bag_is_a_noop_plan() {
        assert_eq!(ok(""), Plan { attach: None, run: None, autostart: false });
    }

    #[test]
    fn order_does_not_matter() {
        assert_eq!(ok("attach start"), ok("start attach"));
        assert_eq!(ok("detach stop"), ok("stop detach"));
    }

    #[test]
    fn membership_carries_the_lifetime() {
        assert!(ok("detach start").ephemeral(), "detach start is ephemeral");
        assert!(!ok("attach start").ephemeral(), "attach start is persistent");
        assert!(!ok("start").ephemeral(), "a bare start is persistent");
        assert!(!ok("detach stop").ephemeral(), "stopping is never 'ephemeral running'");
    }

    #[test]
    fn autostart_implies_attach_and_start() {
        assert_eq!(ok("autostart"), Plan { attach: Some(true), run: Some(true), autostart: true });
    }

    #[test]
    fn autostart_stop_arms_login_but_stays_off() {
        // The soft start yields to an explicit stop; the hard attach stays.
        assert_eq!(
            ok("autostart stop"),
            Plan { attach: Some(true), run: Some(false), autostart: true }
        );
    }

    #[test]
    fn autostart_redundant_with_attach_or_start() {
        assert_eq!(ok("autostart attach start"), ok("autostart"));
    }

    #[test]
    fn unknown_verb_is_rejected() {
        assert!(err("frobnicate").contains("unknown verb 'frobnicate'"));
    }

    #[test]
    fn duplicates_are_rejected() {
        assert!(err("start start").contains("more than once"));
    }

    #[test]
    fn same_axis_opposites_conflict() {
        assert!(err("attach detach").contains("attach and detach"));
        assert!(err("start stop").contains("start and stop"));
    }

    #[test]
    fn autostart_detach_conflicts() {
        assert!(err("autostart detach").contains("autostart keeps the database"));
    }

    #[test]
    fn nothing_applies_on_a_conflict() {
        // A conflict returns Err with no partial Plan — the caller never acts.
        assert!(plan(&bag("attach detach start")).is_err());
    }
}
