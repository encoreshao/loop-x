//! How much each AI CLI can be fenced in at each place loopx calls one.
//!
//! `claude` takes a per-tool allowlist, so every call site is `Restricted`.
//! The other CLIs have no equivalent, so they run `Unrestricted` and the
//! dashboard shows [`warning`] before the user picks one.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallSite {
    GitlabLoop,
    TopicMonitor,
    CalendarPrep,
    InboxTriage,
    Chat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restriction {
    Restricted,
    Unrestricted,
}

pub fn restriction(cli: &str, _site: CallSite) -> Restriction {
    if cli == "claude" {
        Restriction::Restricted
    } else {
        Restriction::Unrestricted
    }
}

/// English text for an unrestricted combination; the UI translates it.
pub fn warning(cli: &str, site: CallSite) -> Option<&'static str> {
    if restriction(cli, site) == Restriction::Restricted {
        return None;
    }
    Some(match cli {
        "codex" => {
            "Codex gives the model a shell and records prompts, including email text, under ~/.codex/sessions."
        }
        "gemini" => {
            "Gemini CLI cannot be limited to a fixed set of tools here, so the model runs with its full tool access."
        }
        "kimi" => {
            "Kimi CLI cannot be limited to a fixed set of tools here, so the model runs with its full tool access."
        }
        "cursor" => {
            "Cursor Agent cannot be limited to a fixed set of tools here, so the model runs with its full tool access."
        }
        _ => "This AI CLI cannot be limited to a fixed set of tools here.",
    })
}

/// Newer integrations that have had less real-world use.
pub fn experimental(cli: &str) -> bool {
    matches!(cli, "gemini" | "kimi" | "cursor")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITES: [CallSite; 5] = [
        CallSite::GitlabLoop,
        CallSite::TopicMonitor,
        CallSite::CalendarPrep,
        CallSite::InboxTriage,
        CallSite::Chat,
    ];

    #[test]
    fn claude_is_restricted_everywhere_and_needs_no_warning() {
        for site in SITES {
            assert_eq!(restriction("claude", site), Restriction::Restricted);
            assert_eq!(warning("claude", site), None);
        }
    }

    #[test]
    fn other_clis_are_unrestricted_and_warn() {
        for cli in ["codex", "gemini", "kimi", "cursor"] {
            for site in SITES {
                assert_eq!(restriction(cli, site), Restriction::Unrestricted);
                assert!(warning(cli, site).is_some(), "{cli}");
            }
        }
        assert_eq!(
            restriction("codex", CallSite::InboxTriage),
            Restriction::Unrestricted
        );
    }

    #[test]
    fn codex_warning_names_where_prompts_are_recorded() {
        assert!(warning("codex", CallSite::InboxTriage)
            .unwrap()
            .contains("~/.codex/sessions"));
    }

    #[test]
    fn only_the_newer_clis_are_experimental() {
        for (cli, want) in [
            ("claude", false),
            ("codex", false),
            ("gemini", true),
            ("kimi", true),
            ("cursor", true),
        ] {
            assert_eq!(experimental(cli), want, "{cli}");
        }
    }
}
