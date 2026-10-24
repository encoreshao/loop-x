# Release Notes Loop

## Goal

When a tracked GitLab project gets a new tag, write release notes for the MRs
merged since the previous tag: a markdown file ready to paste into a release,
and a short summary notification.

## How it runs

- Plugin: `crates/core/src/loop_plugins/release_notes.rs` on LoopKit (`crates/core/src/loopkit.rs`);
  definition `loops/release-notes/loop.yaml`, prompt
  `loops/release-notes/prompt.md`.
- Schedule: hourly from its `loops.json` entry, disabled by default;
  `requires: ["merge_requests"]`, so a GitLab connector must exist.
- Projects: the tracked projects in `projects.json` (same list as Pipeline
  Doctor), matched to GitLab connector accounts by instance; the optional
  `projects` setting (comma-separated aliases) limits them.
- Discovery, per project: the two newest tags
  (`/repository/tags?per_page=2`). The last tag handled per project is kept in
  `outputs/loops/release-notes-loop/tags.json` (gitignored, written
  atomically). The first time a project is seen, its newest tag is only
  recorded as the baseline - no notes - so enabling the loop never writes
  notes for old releases. A newer tag becomes one item, key
  `rel:<instance>:<project id>:<tag>`.
- MRs: merged into the default branch after the previous tag's commit date
  and up to the new tag's commit date (all MRs up to the tag if it is the
  first tag), at most 100, oldest first; title, labels, author, URL and
  description (trimmed to 1000 characters).
- Answer: `{highlights, fixes, internal}`, each a list of `{text, mr}`; every
  text is sanitised and capped at 200 characters, at most 10 per section. An
  entry whose `mr` is not one of the offered MRs keeps its text but gets no
  link; links are always the offered MR's own URL.
- Output: `outputs/loops/release-notes-loop/<alias>-<tag>.md` (characters
  outside `A-Za-z0-9._-` become `_`) with Highlights / Fixes / Internal
  sections, then one notification per tag: counts, the highlights, the tag
  URL and the markdown path. The baseline advances to the new tag.
- Two tags pushed between runs: only the newest gets notes, covering the MRs
  since the tag just before it.

## Settings

| key | default | meaning |
|---|---|---|
| `projects` | empty = all tracked | comma-separated project aliases to watch |

## Safety boundary

- Read-only on GitLab; it never creates a release or a tag. Creating the
  GitLab release is out of scope until it can go through the P4 gate.
- MR text is untrusted; the prompt says so, the model runs sealed, and the
  notification goes through the chat sanitizers.
- Errors log only the exception class name; one failing project or account
  does not stop the others.
- A failed model call leaves the tag unhandled, so the next hourly run
  retries it.
