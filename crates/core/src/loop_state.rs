//! `LoopState` and its explicit transition table. Mirrors `bin/loop_state.py`:
//! `transition()` errors rather than silently allowing an unlisted pair.

use serde::{Serialize, Serializer};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoopState {
    Pending,
    Discovering,
    Planning,
    Executing,
    Verifying,
    Evaluating,
    Completed,
    Failed,
    Blocked,
    Escalated,
    Stopped,
}

impl LoopState {
    pub const ALL: [LoopState; 11] = [
        LoopState::Pending,
        LoopState::Discovering,
        LoopState::Planning,
        LoopState::Executing,
        LoopState::Verifying,
        LoopState::Evaluating,
        LoopState::Completed,
        LoopState::Failed,
        LoopState::Blocked,
        LoopState::Escalated,
        LoopState::Stopped,
    ];

    /// The serialized value (`LoopState.X.value` in Python).
    pub fn as_str(self) -> &'static str {
        match self {
            LoopState::Pending => "pending",
            LoopState::Discovering => "discovering",
            LoopState::Planning => "planning",
            LoopState::Executing => "executing",
            LoopState::Verifying => "verifying",
            LoopState::Evaluating => "evaluating",
            LoopState::Completed => "completed",
            LoopState::Failed => "failed",
            LoopState::Blocked => "blocked",
            LoopState::Escalated => "escalated",
            LoopState::Stopped => "stopped",
        }
    }

    pub fn parse(s: &str) -> Option<LoopState> {
        Self::ALL.into_iter().find(|st| st.as_str() == s)
    }

    /// States reachable from `self` in one step.
    pub fn next_states(self) -> &'static [LoopState] {
        use LoopState::*;
        match self {
            Pending => &[Discovering],
            Discovering => &[Planning],
            Planning => &[Executing],
            Executing => &[Verifying],
            Verifying => &[Evaluating],
            Evaluating => &[Completed, Executing, Blocked, Escalated, Stopped, Failed],
            Blocked => &[Escalated],
            Completed | Failed | Escalated | Stopped => &[],
        }
    }

    pub fn is_terminal(self) -> bool {
        self.next_states().is_empty()
    }
}

impl fmt::Display for LoopState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for LoopState {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot transition from {from} to {to}")]
pub struct InvalidTransition {
    pub from: LoopState,
    pub to: LoopState,
}

/// True if `target` is a valid next state from `current`.
pub fn can_transition(current: LoopState, target: LoopState) -> bool {
    current.next_states().contains(&target)
}

/// `target` if the transition is valid, else [`InvalidTransition`].
pub fn transition(current: LoopState, target: LoopState) -> Result<LoopState, InvalidTransition> {
    if can_transition(current, target) {
        Ok(target)
    } else {
        Err(InvalidTransition {
            from: current,
            to: target,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LoopState::*;

    #[test]
    fn valid_transitions_succeed() {
        let valid = [
            (Pending, Discovering),
            (Discovering, Planning),
            (Planning, Executing),
            (Executing, Verifying),
            (Verifying, Evaluating),
            (Evaluating, Completed),
            (Evaluating, Executing),
            (Evaluating, Blocked),
            (Evaluating, Escalated),
            (Evaluating, Stopped),
            (Evaluating, Failed),
            (Blocked, Escalated),
        ];
        for (a, b) in valid {
            assert!(can_transition(a, b));
            assert_eq!(transition(a, b), Ok(b));
        }
    }

    #[test]
    fn invalid_transitions_error() {
        let invalid = [
            (Pending, Executing),
            (Completed, Executing),
            (Failed, Pending),
            (Stopped, Evaluating),
            (Escalated, Completed),
            (Discovering, Evaluating),
        ];
        for (a, b) in invalid {
            assert!(!can_transition(a, b));
            assert_eq!(transition(a, b), Err(InvalidTransition { from: a, to: b }));
        }
    }

    #[test]
    fn terminal_states_have_no_outgoing_transitions() {
        for s in [Completed, Failed, Stopped, Escalated] {
            assert!(s.is_terminal());
        }
        assert!(!Blocked.is_terminal());
    }

    #[test]
    fn values_round_trip_and_serialize_as_strings() {
        for s in LoopState::ALL {
            assert_eq!(LoopState::parse(s.as_str()), Some(s));
        }
        assert_eq!(serde_json::to_string(&Escalated).unwrap(), "\"escalated\"");
        assert_eq!(LoopState::parse("nope"), None);
    }
}
