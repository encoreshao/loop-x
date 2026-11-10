//! The one Slack message a finished "Fix now" run posts: which merge request (by title),
//! how it ended, and the steps the run went through.

use super::jobs::STEPS;
use crate::slack_report::{Field, Link, Report, Section, State, Status};

/// What the message needs from a finished run.
pub struct Finished<'a> {
    pub mr_ref: &'a str,
    pub mr_title: &'a str,
    pub mr_url: Option<&'a str>,
    /// `done`, `failed` or `nothing`.
    pub state: &'a str,
    pub summary: &'a str,
    /// The step the run was on when it ended (index into [`STEPS`]).
    pub step: usize,
    pub took: Option<String>,
}

pub fn report(f: &Finished) -> Report {
    let fixed_none = f.summary.starts_with("Fixed 0 ");
    let (title, state, status) = match f.state {
        "done" if fixed_none => ("Review feedback not fixed", State::Warning, "Nothing fixed"),
        "done" => ("Review feedback fixed", State::Done, "Finished"),
        "nothing" => ("Nothing to fix", State::Done, "Nothing to do"),
        _ => ("Review feedback fix failed", State::Failed, "Failed"),
    };
    let mr = if f.mr_title.trim().is_empty() {
        f.mr_ref
    } else {
        f.mr_title.trim()
    };
    let mut fields = vec![Field {
        label: "Merge request".into(),
        value: mr.to_string(),
        url: f.mr_url.map(str::to_string),
    }];
    if !f.summary.is_empty() {
        fields.push(Field {
            label: "Result".into(),
            value: f.summary.to_string(),
            url: None,
        });
    }
    if let Some(t) = &f.took {
        fields.push(Field {
            label: "Took".into(),
            value: t.clone(),
            url: None,
        });
    }
    // A clean finish went through every step; otherwise the run got as far as `step`.
    let reached = if f.state == "done" {
        STEPS.len()
    } else {
        (f.step + 1).min(STEPS.len())
    };
    let steps = STEPS[..reached]
        .iter()
        .enumerate()
        .map(|(i, s)| match (f.state, i + 1 == reached) {
            ("failed", true) => format!("{s} (stopped here)"),
            _ => s.to_string(),
        })
        .collect();
    Report {
        title: title.into(),
        status: Some(Status {
            state,
            text: status.into(),
        }),
        summary: Some(format!("Fix now on \u{201c}{mr}\u{201d} ({}).", f.mr_ref)),
        fields,
        sections: vec![Section::Steps {
            heading: "What the run did".into(),
            steps,
        }],
        footer: vec![Link {
            text: "LoopX MR Review".into(),
            url: None,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fin<'a>(state: &'a str, summary: &'a str, step: usize) -> Finished<'a> {
        Finished {
            mr_ref: "work:9!4",
            mr_title: "Fix the login redirect",
            mr_url: Some("https://g/x/-/merge_requests/4"),
            state,
            summary,
            step,
            took: Some("3m 02s".into()),
        }
    }

    #[test]
    fn a_fixed_run_names_the_mr_by_title_and_lists_every_step() {
        let r = report(&fin("done", "Fixed 5 of 7", 5));
        assert_eq!(r.title, "Review feedback fixed");
        assert_eq!(r.fields[0].value, "Fix the login redirect");
        assert_eq!(
            r.fields[0].url.as_deref(),
            Some("https://g/x/-/merge_requests/4")
        );
        let Section::Steps { steps, .. } = &r.sections[0] else {
            panic!()
        };
        assert_eq!(steps.len(), STEPS.len());
        assert!(r.summary.unwrap().contains("Fix the login redirect"));
    }

    #[test]
    fn a_failed_run_says_where_it_stopped_and_zero_fixed_is_a_warning() {
        let r = report(&fin("failed", "Not pushed: checks failed", 4));
        assert_eq!(r.status.as_ref().unwrap().state, State::Failed);
        let Section::Steps { steps, .. } = &r.sections[0] else {
            panic!()
        };
        assert_eq!(steps.len(), 5);
        assert!(steps[4].ends_with("(stopped here)"), "{steps:?}");
        let w = report(&fin("done", "Fixed 0 of 3", 5));
        assert_eq!(w.status.unwrap().state, State::Warning);
    }
}
