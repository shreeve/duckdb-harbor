//! The verb grammar: `harbor <db> [attach|detach] [start|stop|restart] [autostart [off]]`.
//!
//! A bag of BARE verbs — no `--options` — in any order, at most one per axis.
//! The bag is validated as a whole (nothing half-applies) and resolved into a
//! [`Plan`] the caller enacts. Two axes and one property:
//!
//!   membership — attach / detach          (on your list; persisted in config)
//!   running    — start / stop / restart   (green now)
//!   autostart  — autostart / autostart off (the login item: the session
//!                manager runs `start` at every login and after a crash)
//!
//! `autostart` HARD-implies attach and SOFT-defaults start: the login item is
//! loaded now, so the server comes up under the manager. An explicit `stop`
//! overrides the start, giving "armed for login but off right now". `off` is
//! a modifier and stands only directly after `autostart`; it takes the login
//! item away and implies nothing else, so `autostart off` leaves a running
//! server alone and `autostart off stop` takes both down.
//!
//! Membership carries the lifetime: a running server that is attached is
//! persistent; one that is detached is ephemeral — it lives while anyone is
//! connected and leaves when idle. So `detach start` is the ephemeral start,
//! `attach start` the persistent one.

use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Verb {
    Attach,
    Detach,
    Start,
    Stop,
    Restart,
    Autostart,
    /// The modifier after `autostart`; never a verb on its own.
    Off,
}

impl Verb {
    pub fn parse(s: &str) -> Option<Verb> {
        Some(match s {
            "attach" => Verb::Attach,
            "detach" => Verb::Detach,
            "start" => Verb::Start,
            "stop" => Verb::Stop,
            "restart" => Verb::Restart,
            "autostart" => Verb::Autostart,
            "off" => Verb::Off,
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
            Verb::Restart => "restart",
            Verb::Autostart => "autostart",
            Verb::Off => "off",
        }
    }
}

/// The running axis's three directions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Running {
    Start,
    Stop,
    Restart,
}

/// What a validated verb bag resolves to. `None` on an axis means "leave it
/// unchanged".
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// membership: Some(true)=attach, Some(false)=detach, None=unchanged.
    pub attach: Option<bool>,
    /// running: start, stop, restart, or unchanged.
    pub run: Option<Running>,
    /// the login item: Some(true)=install, Some(false)=remove, None=unchanged.
    pub autostart: Option<bool>,
}

impl Plan {
    /// A running server is ephemeral exactly when it is being started while
    /// detached — membership carries the lifetime. (`attach start` and a bare
    /// `start` are persistent; only `detach start` is ephemeral.) The dispatch
    /// hands this to `start` as the server's refcounted-lifetime fact.
    pub fn ephemeral(&self) -> bool {
        self.run == Some(Running::Start) && self.attach == Some(false)
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
    // 1-3) parse each word, rejecting unknowns; 2) count on the way. `off` is
    // position-bound: it modifies the `autostart` immediately before it.
    let mut counts: HashMap<Verb, u32> = HashMap::new();
    let mut off = false;
    for (i, w) in words.iter().enumerate() {
        let v = Verb::parse(w).ok_or_else(|| {
            format!("unknown verb '{w}' — verbs are: attach, detach, start, stop, restart, autostart [off]")
        })?;
        if v == Verb::Off {
            if i == 0 || Verb::parse(&words[i - 1]) != Some(Verb::Autostart) {
                return Err("'off' goes right after autostart — `autostart off`".into());
            }
            off = true;
        }
        *counts.entry(v).or_insert(0) += 1;
    }

    // 4) a verb given more than once is a malformed request, not a doubling.
    for (v, n) in &counts {
        if *n > 1 {
            return Err(format!("'{}' given more than once", v.as_str()));
        }
    }

    let has = |v: Verb| counts.contains_key(&v);

    // 5) conflicts: the same-axis opposites, and installing the login item
    // for a database being detached. (autostart + stop is NOT a conflict —
    // stop is a soft override of autostart's default start.)
    if has(Verb::Attach) && has(Verb::Detach) {
        return Err("can't attach and detach at once — pick one".into());
    }
    let running = [Verb::Start, Verb::Stop, Verb::Restart].iter().filter(|v| has(**v)).count();
    if running > 1 {
        return Err("start, stop and restart are one axis — pick one".into());
    }
    let install = has(Verb::Autostart) && !off;
    if install && has(Verb::Detach) {
        return Err(
            "autostart keeps the database (it implies attach); drop detach or drop autostart"
                .into(),
        );
    }

    // 6) resolve. An installing autostart pre-fills attach (hard) and start
    // (default), then any explicit verb wins — except detach, ruled out above.
    // `autostart off` implies nothing.
    let autostart = if has(Verb::Autostart) { Some(!off) } else { None };
    let attach = if has(Verb::Attach) || install {
        Some(true)
    } else if has(Verb::Detach) {
        Some(false)
    } else {
        None
    };
    let run = if has(Verb::Start) {
        Some(Running::Start)
    } else if has(Verb::Stop) {
        Some(Running::Stop)
    } else if has(Verb::Restart) {
        Some(Running::Restart)
    } else if install {
        Some(Running::Start) // autostart's soft default, no explicit verb overrode it
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
        assert_eq!(ok("attach"), Plan { attach: Some(true), run: None, autostart: None });
        assert_eq!(ok("detach"), Plan { attach: Some(false), run: None, autostart: None });
        assert_eq!(ok("start"), Plan { attach: None, run: Some(Running::Start), autostart: None });
        assert_eq!(ok("stop"), Plan { attach: None, run: Some(Running::Stop), autostart: None });
        assert_eq!(ok("restart"), Plan { attach: None, run: Some(Running::Restart), autostart: None });
    }

    #[test]
    fn empty_bag_is_a_noop_plan() {
        assert_eq!(ok(""), Plan { attach: None, run: None, autostart: None });
    }

    #[test]
    fn order_does_not_matter() {
        assert_eq!(ok("attach start"), ok("start attach"));
        assert_eq!(ok("detach stop"), ok("stop detach"));
        assert_eq!(ok("stop autostart off"), ok("autostart off stop"));
    }

    #[test]
    fn membership_carries_the_lifetime() {
        assert!(ok("detach start").ephemeral(), "detach start is ephemeral");
        assert!(!ok("attach start").ephemeral(), "attach start is persistent");
        assert!(!ok("start").ephemeral(), "a bare start is persistent");
        assert!(!ok("detach stop").ephemeral(), "stopping is never 'ephemeral running'");
    }

    #[test]
    fn start_and_stop_leave_the_login_item_alone() {
        assert_eq!(ok("start").autostart, None);
        assert_eq!(ok("stop").autostart, None);
        assert_eq!(ok("restart").autostart, None);
    }

    #[test]
    fn autostart_implies_attach_and_start() {
        assert_eq!(
            ok("autostart"),
            Plan { attach: Some(true), run: Some(Running::Start), autostart: Some(true) }
        );
    }

    #[test]
    fn autostart_stop_arms_login_but_stays_off() {
        // The soft start yields to an explicit stop; the hard attach stays.
        assert_eq!(
            ok("autostart stop"),
            Plan { attach: Some(true), run: Some(Running::Stop), autostart: Some(true) }
        );
    }

    #[test]
    fn autostart_restart_hands_the_server_to_the_manager() {
        assert_eq!(
            ok("autostart restart"),
            Plan { attach: Some(true), run: Some(Running::Restart), autostart: Some(true) }
        );
    }

    #[test]
    fn autostart_off_implies_nothing_else() {
        assert_eq!(ok("autostart off"), Plan { attach: None, run: None, autostart: Some(false) });
        assert_eq!(
            ok("autostart off stop"),
            Plan { attach: None, run: Some(Running::Stop), autostart: Some(false) }
        );
        // Detaching a database whose login item is being removed is fine.
        assert_eq!(
            ok("autostart off detach"),
            Plan { attach: Some(false), run: None, autostart: Some(false) }
        );
    }

    #[test]
    fn off_must_follow_autostart() {
        assert!(err("off").contains("right after autostart"));
        assert!(err("off autostart").contains("right after autostart"));
        assert!(err("autostart stop off").contains("right after autostart"));
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
        assert!(err("autostart off autostart off").contains("more than once"));
    }

    #[test]
    fn same_axis_opposites_conflict() {
        assert!(err("attach detach").contains("attach and detach"));
        assert!(err("start stop").contains("one axis"));
        assert!(err("restart stop").contains("one axis"));
        assert!(err("start restart").contains("one axis"));
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
