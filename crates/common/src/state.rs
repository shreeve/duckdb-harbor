//! What a berth is doing, in one word — and what that word looks like.
//!
//! The set exists because desired and actual can disagree, and every way
//! they can disagree needs a name the reader can act on. Both binaries
//! import this so a berth is never described two different ways.
//!
//! Severity is a ladder, and it is stated as meaning rather than as color:
//!
//! * [`Level::Idle`] — nothing is wrong. Configured and down, or leftovers.
//! * [`Level::Good`] — running and matching its config.
//! * [`Level::Warn`] — running, but not the way the file says. Worth a look.
//! * [`Level::Bad`] — the registry claims a process that is not there.
//!
//! What a level *looks* like is the front end's business: dim/green/yellow/red
//! in a terminal, a palette token in a GUI. The glyph carries the same
//! meaning independently, so the table still reads with color off, in a pipe,
//! or for someone who cannot separate the hues.

/// How alarming a state is, with no opinion about color.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    /// Nothing is wrong, nothing is happening.
    Idle,
    /// Healthy.
    Good,
    /// Works, but something disagrees with something else.
    Warn,
    /// Broken.
    Bad,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Configured, running, and serving what the config says.
    Running,
    /// Configured, not running. Starts on use, or with `harbor start`.
    Stopped,
    /// Configured, stopped by the operator's own word — `harbor stop` holds
    /// it down against every client until `harbor start` lifts the hold.
    Held,
    /// Running, but the database or the options differ from the config now.
    Drifted,
    /// Running, with nothing in the config about it — summoned by a client,
    /// or started by hand. Not an error; `harbor ./x.duckdb` is meant to work.
    Unmanaged,
    /// A sidecar claims a live process and the lock says otherwise.
    Dead,
    /// Files left behind by a berth that is gone. Harmless, just untidy.
    Stale,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Stopped => "stopped",
            State::Held => "held",
            State::Drifted => "drifted",
            State::Unmanaged => "unmanaged",
            State::Dead => "dead",
            State::Stale => "stale",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            State::Running => "●",
            State::Stopped => "○",
            State::Held => "○",
            State::Drifted => "◆",
            State::Unmanaged => "◍",
            State::Dead => "✕",
            State::Stale => "◌",
        }
    }

    pub fn level(self) -> Level {
        match self {
            State::Running => Level::Good,
            State::Stopped | State::Held | State::Stale => Level::Idle,
            State::Drifted | State::Unmanaged => Level::Warn,
            State::Dead => Level::Bad,
        }
    }

    /// `● running`, ready for a cell.
    pub fn label(self) -> String {
        format!("{} {}", self.glyph(), self.word())
    }

    /// Is this a berth you can talk to right now?
    pub fn is_live(self) -> bool {
        matches!(self, State::Running | State::Drifted | State::Unmanaged)
    }

    /// Sort order for the fleet view: what you have, then what is running
    /// without being configured, then the mess to sweep. Stale goes last
    /// because it is acted on in bulk, not one at a time.
    pub fn rank(self) -> u8 {
        match self {
            State::Running | State::Drifted | State::Stopped | State::Held => 0,
            State::Unmanaged => 1,
            State::Dead => 2,
            State::Stale => 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_climbs_with_the_problem() {
        assert!(State::Stopped.level() < State::Running.level());
        assert!(State::Running.level() < State::Drifted.level());
        assert!(State::Drifted.level() < State::Dead.level());
    }

    #[test]
    fn the_word_survives_without_color() {
        // A reader with color off, or in a pipe, still gets the state.
        for s in [State::Running, State::Stopped, State::Held, State::Drifted, State::Dead, State::Stale] {
            assert!(!s.word().is_empty() && !s.glyph().is_empty());
            assert!(s.label().contains(s.word()));
        }
    }

    #[test]
    fn the_mess_sorts_last() {
        let mut v = [State::Stale, State::Running, State::Unmanaged, State::Stopped];
        v.sort_by_key(|s| s.rank());
        assert_eq!(v, [State::Running, State::Stopped, State::Unmanaged, State::Stale]);
    }
}
