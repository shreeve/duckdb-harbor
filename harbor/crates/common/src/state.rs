//! What a berth is doing, in one word — and what that word looks like.
//!
//! A berth is either [`State::Running`] or [`State::Stopped`]; both binaries
//! import this so a berth is never described two different ways.
//!
//! Severity is a ladder — [`Level`] — stated as meaning rather than as color,
//! and wider than the two states so a front end has room to grow into it:
//!
//! * [`Level::Idle`] — nothing is wrong, nothing is happening (stopped).
//! * [`Level::Good`] — running and healthy.
//! * [`Level::Warn`] — works, but something disagrees with something else.
//! * [`Level::Bad`] — broken.
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
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Stopped => "stopped",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            State::Running => "●",
            State::Stopped => "○",
        }
    }

    pub fn level(self) -> Level {
        match self {
            State::Running => Level::Good,
            State::Stopped => Level::Idle,
        }
    }

    /// `● running`, ready for a cell.
    pub fn label(self) -> String {
        format!("{} {}", self.glyph(), self.word())
    }

    /// Is this a berth you can talk to right now?
    pub fn is_live(self) -> bool {
        matches!(self, State::Running)
    }

    /// Sort order for the fleet view: running and stopped both rank together —
    /// they are the configured berths, listed by name, not by liveness.
    pub fn rank(self) -> u8 {
        match self {
            State::Running | State::Stopped => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_climbs_with_the_problem() {
        assert!(State::Stopped.level() < State::Running.level());
    }

    #[test]
    fn the_word_survives_without_color() {
        // A reader with color off, or in a pipe, still gets the state.
        for s in [State::Running, State::Stopped] {
            assert!(!s.word().is_empty() && !s.glyph().is_empty());
            assert!(s.label().contains(s.word()));
        }
    }
}
