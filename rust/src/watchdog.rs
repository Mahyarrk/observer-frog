//! The dead man's switch: a tail that notices its head has stopped answering.
//!
//! This is the one failure a head cannot report about itself. The heartbeat is the report a tail
//! already sends, so nothing new is polled and no socket is held here: this module is a clock and a
//! verdict, fed by the poll wave.
//!
//! Invariants:
//!   Only a 200 is life. An answer that is not 200 proves the head is alive and refusing, which is a
//!   different fault: it becomes `degraded`, never `down`.
//!   A recovery must be observed. A head that returns and dies inside the threshold was never up.
//!   One message per transition. Nothing repeats while a state holds.

use crate::tail::Outcome;
use std::time::{Duration, Instant};

/// Five minutes. Fixed rather than derived from the poll interval, because the interval is itself
/// bounded at 60 seconds in `config`, so the worst case is five missed pings.
pub const DOWN_AFTER: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Reporting succeeds. Nobody is told anything.
    Routine,
    /// The head has been declared down and the switch is waiting to confirm or contradict that.
    Armed,
    /// The head answered but refused the report. The operator has been told once; the next news will
    /// be either up or down, so nothing more is sent while this holds.
    Waiting,
}

/// What the switch wants the caller to publish, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    Down,
    Degraded,
    Up,
}

pub struct Watchdog {
    state: State,
    /// Monotonic, never wall clock: a laptop that suspends for two hours wakes with a wall-clock gap
    /// indistinguishable from an outage, and would alarm on every lid close.
    last_up: Option<Instant>,
    /// The last reason a report failed, carried into the alert so it says something useful.
    last_why: String,
}

impl Watchdog {
    pub fn new() -> Self {
        Self {
            state: State::Routine,
            // Seeded at construction so a head that has never answered still has a clock: a tail
            // that starts with the head already dead must alarm like any other.
            last_up: Some(Instant::now()),
            last_why: String::new(),
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Feed one report verdict. Returns a signal only when a transition warrants a message.
    pub fn observe(&mut self, outcome: &Outcome) -> Option<Signal> {
        match outcome {
            Outcome::Up(_) => {
                self.last_up = Some(Instant::now());
                let was = self.state;
                self.state = State::Routine;
                // Only a state that had told the operator something needs the all-clear. A routine
                // success says nothing, which is the everyday case.
                match was {
                    State::Armed | State::Waiting => Some(Signal::Up),
                    State::Routine => None,
                }
            }
            Outcome::Answered { code, why, .. } => {
                self.last_why = format!("{code}: {why}");
                let was = self.state;
                self.state = State::Waiting;
                // Once per arrival at Waiting. Staying there sends nothing, which is the point of
                // the state existing at all.
                match was {
                    State::Routine | State::Armed => Some(Signal::Degraded),
                    State::Waiting => None,
                }
            }
            Outcome::Unreachable { why } => {
                self.last_why = why.clone();
                self.check_silence()
            }
        }
    }

    /// Called when no report was attempted or the attempt failed to complete. Kept separate so the
    /// caller can tick the clock without inventing an `Outcome`.
    pub fn check_silence(&mut self) -> Option<Signal> {
        let elapsed = self.last_up.map(|t| t.elapsed()).unwrap_or(DOWN_AFTER);
        if elapsed < DOWN_AFTER {
            return None;
        }
        match self.state {
            // Already declared down: staying silent is the difference between a switch and a nag.
            State::Armed => None,
            State::Routine | State::Waiting => {
                self.state = State::Armed;
                Some(Signal::Down)
            }
        }
    }

    pub fn last_why(&self) -> &str {
        &self.last_why
    }
}

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up() -> Outcome {
        Outcome::Up(serde_json::json!({}))
    }
    fn refused(code: u16) -> Outcome {
        Outcome::Answered { code, why: "refused".into(), head_name: None }
    }
    fn silent() -> Outcome {
        Outcome::Unreachable { why: "cannot reach".into() }
    }

    /// Push the clock past the threshold without waiting five minutes.
    fn age(w: &mut Watchdog) {
        w.last_up = Some(Instant::now() - DOWN_AFTER - Duration::from_secs(1));
    }

    #[test]
    fn a_healthy_head_says_nothing() {
        let mut w = Watchdog::new();
        assert_eq!(w.observe(&up()), None);
        assert_eq!(w.observe(&up()), None);
        assert_eq!(w.state(), State::Routine);
    }

    #[test]
    fn a_refusal_is_degraded_not_down_and_says_it_once() {
        let mut w = Watchdog::new();
        assert_eq!(w.observe(&refused(401)), Some(Signal::Degraded));
        assert_eq!(w.state(), State::Waiting);
        // Staying degraded says nothing more, however many refusals arrive.
        assert_eq!(w.observe(&refused(401)), None);
        assert_eq!(w.observe(&refused(503)), None);
        // And a refusal is a life sign, so the clock is not what decides here.
        age(&mut w);
        assert_eq!(w.check_silence(), Some(Signal::Down), "silence still alarms from Waiting");
    }

    #[test]
    fn silence_under_the_threshold_is_not_down() {
        let mut w = Watchdog::new();
        assert_eq!(w.observe(&silent()), None);
        assert_eq!(w.state(), State::Routine);
    }

    #[test]
    fn silence_past_the_threshold_alarms_exactly_once() {
        let mut w = Watchdog::new();
        age(&mut w);
        assert_eq!(w.observe(&silent()), Some(Signal::Down));
        assert_eq!(w.state(), State::Armed);
        // The whole point: a head down for an hour is one message, not twelve.
        for _ in 0..12 {
            assert_eq!(w.observe(&silent()), None);
        }
        assert_eq!(w.check_silence(), None);
    }

    #[test]
    fn a_recovery_is_announced_once_and_returns_to_routine() {
        let mut w = Watchdog::new();
        age(&mut w);
        assert_eq!(w.observe(&silent()), Some(Signal::Down));
        assert_eq!(w.observe(&up()), Some(Signal::Up));
        assert_eq!(w.state(), State::Routine);
        assert_eq!(w.observe(&up()), None, "back to silent routine");
    }

    #[test]
    fn a_recovery_into_a_refusal_is_degraded() {
        let mut w = Watchdog::new();
        age(&mut w);
        assert_eq!(w.observe(&silent()), Some(Signal::Down));
        // Back, but not accepting reports: a different fault with a different fix.
        assert_eq!(w.observe(&refused(401)), Some(Signal::Degraded));
        assert_eq!(w.state(), State::Waiting);
        assert_eq!(w.observe(&refused(401)), None);
        assert_eq!(w.observe(&up()), Some(Signal::Up));
    }

    #[test]
    fn a_brief_return_does_not_clear_an_alarm_that_is_still_true() {
        let mut w = Watchdog::new();
        age(&mut w);
        assert_eq!(w.observe(&silent()), Some(Signal::Down));
        // Comes back, then goes again before the threshold: the operator is told up, then down again
        // only after another full threshold. There is no third state hiding here.
        assert_eq!(w.observe(&up()), Some(Signal::Up));
        assert_eq!(w.observe(&silent()), None, "a fresh alarm needs a fresh threshold");
        age(&mut w);
        assert_eq!(w.check_silence(), Some(Signal::Down));
    }

    #[test]
    fn a_tail_that_starts_with_a_dead_head_still_alarms() {
        let mut w = Watchdog::new();
        age(&mut w);
        assert_eq!(w.check_silence(), Some(Signal::Down));
    }

    #[test]
    fn the_reason_is_kept_for_the_message() {
        let mut w = Watchdog::new();
        w.observe(&refused(503));
        assert!(w.last_why().contains("503"), "{}", w.last_why());
        w.observe(&silent());
        assert!(w.last_why().contains("cannot reach"), "{}", w.last_why());
    }
}
