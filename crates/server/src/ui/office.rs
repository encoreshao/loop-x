//! Maps each loop to one of the office view's five states.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeState {
    Disabled,
    Waiting,
    Failed,
    Working,
    Idle,
}

impl OfficeState {
    pub fn as_str(self) -> &'static str {
        match self {
            OfficeState::Disabled => "disabled",
            OfficeState::Waiting => "waiting",
            OfficeState::Failed => "failed",
            OfficeState::Working => "working",
            OfficeState::Idle => "idle",
        }
    }
}

/// Precedence: disabled, waiting, failed, working, idle. `pending_approvals` is
/// what the caller attributes to this loop (see [`pending_for`]).
pub fn office_state(enabled: bool, run_state: &str, pending_approvals: usize) -> OfficeState {
    if !enabled {
        return OfficeState::Disabled;
    }
    if pending_approvals > 0 {
        return OfficeState::Waiting;
    }
    match run_state {
        "failed" | "stale" => OfficeState::Failed,
        "running" => OfficeState::Working,
        _ => OfficeState::Idle,
    }
}

/// Approvals carry no loop name and only the GitLab issue loop creates them, so
/// the whole pending total belongs to it and to no other loop.
pub fn pending_for(loop_name: &str, pending_total: usize) -> usize {
    if loop_name == "gitlab-loop" {
        pending_total
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_row_of_the_mapping() {
        assert_eq!(office_state(false, "running", 0), OfficeState::Disabled);
        assert_eq!(office_state(false, "failed", 3), OfficeState::Disabled);
        assert_eq!(office_state(true, "running", 0), OfficeState::Working);
        assert_eq!(office_state(true, "failed", 0), OfficeState::Failed);
        assert_eq!(office_state(true, "stale", 0), OfficeState::Failed);
        for s in ["idle", "stopped", "never_run", "unknown", "weird"] {
            assert_eq!(office_state(true, s, 0), OfficeState::Idle, "{s}");
        }
        assert_eq!(office_state(true, "idle", 1), OfficeState::Waiting);
        assert_eq!(OfficeState::Waiting.as_str(), "waiting");
    }

    #[test]
    fn precedence() {
        assert_eq!(office_state(false, "running", 2), OfficeState::Disabled);
        assert_eq!(office_state(true, "running", 2), OfficeState::Waiting);
        assert_eq!(office_state(true, "failed", 2), OfficeState::Waiting);
    }

    #[test]
    fn pending_only_counts_for_gitlab_loop() {
        assert_eq!(pending_for("gitlab-loop", 2), 2);
        assert_eq!(pending_for("rss-watch", 2), 0);
    }
}
