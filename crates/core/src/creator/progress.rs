//! How far along a Creator job is (a Find videos run, or an inspire-video Generate job), as "stage N of 9" and a
//! percentage, worked out from the steps already in its log.
//!
//! The skill's workflow is fixed, so each tool call can be matched to the stage it
//! belongs to (a download, the transcription, the render, ...). The job's stage is the
//! furthest one seen, so a re-run of an earlier command never moves it back. It is an
//! estimate: stages take very different times and the agent may skip or reorder some.

use super::jobs::JobKind;

/// The stages of one video, in order. The label is English; the page translates it.
pub const STAGES: [&str; 9] = [
    "Fetch the source",
    "Probe and extract audio",
    "Transcribe",
    "Choose a segment and write the copy",
    "Cut the clip",
    "Build the render manifest",
    "Render",
    "Quality check",
    "Write the publishing copy",
];

/// The stages of one Find videos run (the video-scout skill), in order.
pub const SCOUT_STAGES: [&str; 4] = [
    "Check earlier results",
    "Search for candidates",
    "Inspect candidates",
    "Write the results",
];

/// The stages of one showcase video (the inspire-showcase skill), in order.
pub const SHOWCASE_STAGES: [&str; 7] = [
    "Read the source",
    "Capture the pages",
    "Write the beats",
    "Validate the manifest",
    "Preview",
    "Render and review",
    "Write the publishing copy",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// 1-based index into the job kind's stage list.
    pub stage: usize,
    pub total: usize,
    /// 0-100, the middle of the current stage's share of the bar.
    pub percent: u8,
    /// The stage's English name (the page translates it by `stage`).
    pub label: &'static str,
}

/// The stage a Find videos step line belongs to (the scout job has web tools and
/// file tools, no shell).
fn scout_stage_of(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("Step ")?;
    let (_, call) = rest.split_once(" · ")?;
    let wrote = call.starts_with("Write:") || call.starts_with("Edit:");
    if wrote && (call.contains("catalog") || call.contains("report")) {
        Some(4)
    } else if call.starts_with("WebFetch:") {
        Some(3)
    } else if call.starts_with("WebSearch:") {
        Some(2)
    } else if call.starts_with("Read:") && call.contains("catalog") {
        Some(1)
    } else {
        None
    }
}

/// The stage a rendered step line (`Step 4 · Bash: python scripts/fetch.py ...`)
/// belongs to; `None` for a line that is not a step or matches no stage.
fn stage_of(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("Step ")?;
    let (_, call) = rest.split_once(" · ")?;
    let has = |needles: &[&str]| needles.iter().any(|n| call.contains(n));
    let writes = |name: &str| {
        (call.starts_with("Write:") || call.starts_with("Edit:")) && call.contains(name)
    };
    // Latest stage first: the first match wins.
    if writes("publish.json") || has(&["publish_meta.py"]) {
        Some(9)
    } else if has(&["volumedetect", "blackdetect", "crop=864"])
        || (has(&["media.py ffprobe"]) && has(&["output/final"]))
        || (call.starts_with("Read:") && has(&["output/final"]))
    {
        Some(8)
    } else if has(&["render.py"]) {
        Some(7)
    } else if writes("video_manifest.json") {
        Some(6)
    } else if has(&["media.py ffmpeg"])
        && has(&[" -ss ", "clip.mp4", "poster"])
        && !has(&["audio.wav"])
    {
        Some(5)
    } else if writes("copy.json") || writes("publish_draft") || has(&["refine_segment.py"]) {
        Some(4)
    } else if has(&["transcribe.py"]) {
        Some(3)
    } else if has(&["audio.wav", "media.py ffprobe", "media.py ffmpeg"]) {
        Some(2)
    } else if has(&["fetch.py", "check_env.py", "precheck.py"]) {
        Some(1)
    } else {
        None
    }
}

/// The stage a showcase step line belongs to.
fn showcase_stage_of(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("Step ")?;
    let (_, call) = rest.split_once(" · ")?;
    let has = |needles: &[&str]| needles.iter().any(|n| call.contains(n));
    let writes = |name: &str| {
        (call.starts_with("Write:") || call.starts_with("Edit:")) && call.contains(name)
    };
    if writes("project.json") {
        Some(7)
    } else if has(&["render.py"]) && !has(&["--preview"]) || has(&["checks.py"]) {
        Some(6)
    } else if has(&["--preview"]) || has(&["narrate.py"]) {
        Some(5)
    } else if has(&["manifest.py validate"]) {
        Some(4)
    } else if has(&["manifest.py scaffold"]) || writes("video_manifest.json") {
        Some(3)
    } else if has(&["capture_page.py", "capture.py"]) {
        Some(2)
    } else if has(&["ingest.py", "check_env.py"]) {
        Some(1)
    } else {
        None
    }
}

/// Maps one rendered step line to its stage.
type StageFn = fn(&str) -> Option<usize>;

/// The furthest stage reached in `rendered` (see [`super::logfmt::render`]) for a
/// job of `kind`; `None` before the first recognised step.
pub fn of_rendered_log(kind: JobKind, rendered: &str) -> Option<Progress> {
    let (stages, stage_of): (&[&'static str], StageFn) = match kind {
        JobKind::Generate => (&STAGES, stage_of),
        JobKind::Scout => (&SCOUT_STAGES, scout_stage_of),
        JobKind::Showcase => (&SHOWCASE_STAGES, showcase_stage_of),
    };
    let stage = rendered.lines().filter_map(stage_of).max()?;
    let total = stages.len();
    let percent = ((2 * stage - 1) * 100 / (2 * total)) as u8;
    Some(Progress {
        stage,
        total,
        percent,
        label: stages[stage - 1],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(lines: &[&str]) -> Option<Progress> {
        of_rendered_log(JobKind::Generate, &lines.join("\n"))
    }

    fn sp(lines: &[&str]) -> Option<Progress> {
        of_rendered_log(JobKind::Scout, &lines.join("\n"))
    }

    #[test]
    fn a_scout_run_goes_from_history_to_search_to_inspection_to_results() {
        let steps = [
            ("Step 1 · Read: /h/.video-scout/catalog-index.json", 1),
            ("Step 2 · WebSearch: leadership talks 2026", 2),
            ("Step 3 · WebFetch: https://www.ted.com/talks/x", 3),
            (
                "Step 4 · Write: /h/.video-scout/catalog-2026-11-05-a.json",
                4,
            ),
            ("Step 5 · Write: /h/.video-scout/report-2026-11-05-a.md", 4),
        ];
        for (line, stage) in steps {
            assert_eq!(scout_stage_of(line), Some(stage), "{line}");
        }
        let now = sp(&[
            "Step 1 · WebSearch: a",
            "Step 2 · WebSearch: b",
            "Step 3 · WebFetch: u",
        ])
        .unwrap();
        assert_eq!((now.stage, now.total, now.percent), (3, 4, 62)); // (2*3-1)/8
        assert_eq!(now.label, "Inspect candidates");
        assert_eq!(
            sp(&["Step 1 · Read: /h/.claude/skills/video-scout/SKILL.md"]),
            None
        );
        // A Generate command means nothing to a scout run, and the other way round.
        assert_eq!(sp(&["Step 1 · Bash: python scripts/fetch.py u"]), None);
        assert_eq!(p(&["Step 1 · WebSearch: a"]), None);
    }

    #[test]
    fn nothing_recognised_means_no_progress_yet() {
        assert_eq!(p(&[]), None);
        assert_eq!(
            p(&[
                "Started · claude",
                "Claude: I will start",
                "Step 1 · Read: /h/SKILL.md"
            ]),
            None
        );
    }

    #[test]
    fn each_stage_of_a_real_run_is_recognised_in_order() {
        let steps = [
            ("Step 1 · Bash: python scripts/check_env.py", 1),
            ("Step 2 · Bash: python scripts/fetch.py https://ted.com/talks/x --yes-rights", 1),
            ("Step 3 · Bash: python scripts/media.py ffprobe -v error -of json output/cache/x/source.mp4", 2),
            ("Step 4 · Bash: python scripts/media.py ffmpeg -i output/cache/x/source.mp4 -vn -ar 16000 output/projects/s/assets/audio.wav", 2),
            ("Step 5 · Bash: python scripts/transcribe.py output/projects/s/assets/audio.wav", 3),
            ("Step 6 · Write: /h/.inspire-video/output/projects/s/copy.json", 4),
            ("Step 7 · Bash: python scripts/media.py ffmpeg -ss 46 -to 120 -i output/cache/x/source.mp4 -vf scale=1280:720 output/projects/s/assets/clip.mp4", 5),
            ("Step 8 · Write: /h/.inspire-video/output/projects/s/video_manifest.json", 6),
            ("Step 9 · Bash: python scripts/render.py output/projects/s/video_manifest.json", 7),
            ("Step 10 · Bash: python scripts/media.py ffmpeg -i output/final/s/s.mp4 -af volumedetect -vn -f null -", 8),
            ("Step 11 · Bash: python scripts/media.py ffprobe -v error output/final/s/s.mp4", 8),
            ("Step 12 · Write: /h/.inspire-video/output/projects/s/publish.json", 9),
            ("Step 13 · Bash: python scripts/publish_meta.py output/projects/s", 9),
        ];
        for (line, stage) in steps {
            assert_eq!(stage_of(line), Some(stage), "{line}");
        }
    }

    #[test]
    fn each_showcase_stage_is_recognised_in_order() {
        let steps = [
            ("Step 1 · Bash: python3 scripts/showcase/ingest.py https://example.com --project output/projects/s", 1),
            ("Step 2 · Bash: python3 scripts/showcase/capture_page.py https://example.com --project output/projects/s", 2),
            ("Step 3 · Bash: python3 scripts/showcase/manifest.py scaffold output/projects/s --beats 5", 3),
            ("Step 4 · Write: /h/.inspire-video/output/projects/s/video_manifest.json", 3),
            ("Step 5 · Bash: python3 scripts/showcase/manifest.py validate output/projects/s", 4),
            ("Step 6 · Bash: python3 scripts/render.py output/projects/s --preview", 5),
            ("Step 7 · Bash: python3 scripts/render.py output/projects/s", 6),
            ("Step 8 · Bash: python3 scripts/showcase/checks.py output/final/s/s.mp4 --final", 6),
            ("Step 9 · Write: /h/.inspire-video/output/final/s/project.json", 7),
        ];
        for (line, stage) in steps {
            assert_eq!(showcase_stage_of(line), Some(stage), "{line}");
        }
        let lines: Vec<String> = steps.iter().take(5).map(|(l, _)| l.to_string()).collect();
        let now = of_rendered_log(JobKind::Showcase, &lines.join("\n")).unwrap();
        assert_eq!((now.stage, now.total), (4, 7));
        assert_eq!(now.label, "Validate the manifest");
        // A talk's command means nothing to a showcase run.
        assert_eq!(
            showcase_stage_of("Step 1 · Bash: python scripts/fetch.py u"),
            None
        );
    }

    #[test]
    fn the_skills_precheck_and_segment_refine_steps_belong_to_their_stages() {
        assert_eq!(
            stage_of("Step 3 · Bash: python scripts/precheck.py output/cache/x/source.mp4"),
            Some(1)
        );
        assert_eq!(
            stage_of("Step 8 · Bash: python scripts/refine_segment.py output/projects/s"),
            Some(4)
        );
    }

    #[test]
    fn the_furthest_stage_wins_and_the_percent_is_mid_stage() {
        let up_to_transcribe = p(&[
            "Step 1 · Bash: python scripts/fetch.py u",
            "Step 2 · Bash: python scripts/transcribe.py a.wav",
            "Step 3 · Bash: python scripts/fetch.py u",
        ])
        .unwrap();
        assert_eq!((up_to_transcribe.stage, up_to_transcribe.total), (3, 9));
        assert_eq!(up_to_transcribe.percent, 27); // (2*3-1)/18
        assert_eq!(up_to_transcribe.label, "Transcribe");
        let first = p(&["Step 1 · Bash: python scripts/fetch.py u"]).unwrap();
        assert_eq!(first.percent, 5);
        let last = p(&["Step 1 · Bash: python scripts/publish_meta.py output/projects/s"]).unwrap();
        assert_eq!((last.stage, last.percent), (9, 94));
    }

    #[test]
    fn only_step_lines_count_not_what_claude_says() {
        assert_eq!(
            p(&["Claude: now I will run python scripts/render.py"]),
            None
        );
        assert_eq!(p(&["    ↳ render.py finished"]), None);
    }
}
