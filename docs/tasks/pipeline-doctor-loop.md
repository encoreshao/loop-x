# Pipeline Doctor Loop

## Goal

Every hour, find failed CI pipelines on the default branch of your tracked
projects and on your own open merge requests, have the model diagnose each
from the failed jobs' log tails, and send you the diagnosis. Failures that
keep coming back are flagged so flaky tests and chronic breakage stand out.

## How it runs

- Plugin: `crates/core/src/loop_plugins/pipeline_doctor.rs` on LoopKit (`crates/core/src/loopkit.rs`);
  definition `loops/pipeline-doctor/loop.yaml`, prompt
  `loops/pipeline-doctor/prompt.md`.
- Schedule: hourly from its `loops.json` entry (disabled by default;
  `requires: ["pipelines"]`). Only `gitlab` accounts are used, and only those
  with at least one tracked project in `projects.json` (the project's
  `instance`, else the top-level `gitlab_instance`).
- Discovery per account: failed pipelines on each tracked project's default
  branch updated in the last 26 hours (`default_branch` from the project
  entry, else GitLab's), plus the `head_pipeline` of your open MRs when it
  failed. One item per pipeline (`pipe:<account>:<project_id>#<pipeline_id>`),
  at most 10 per run, never re-diagnosed once seen.
- Each item carries up to 3 failed jobs, each with the last 300 lines of its
  trace (ANSI escapes and GitLab section markers stripped, capped at 20 KB).
- The answer has `category` (`flaky|infra|test_failure|lint|build|dependency|config|unknown`),
  `culprit`, `explanation`, `suggested_fix` and `confidence`.

- Scope limits: only the default branch is watched (other branches' failures
  are seen only when they are the head pipeline of one of your open MRs), and
  an account with no tracked project in `projects.json` is not scanned at all,
  not even for your MRs. If the MR list omits `head_pipeline`, the loop fetches
  the MR detail for up to 20 such MRs per account.

## Recurring failures

A fingerprint (`sha1(job name + explanation's first line)[:12]`) is stored in
`outputs/loops/pipeline-doctor-loop/fingerprints.json` (gitignored). Three or
more occurrences in 7 days mark the diagnosis as recurring; the digest lists
recurring items first, prefixed with a repeat marker. Each pipeline counts
once (tracked in `recorded_pipelines.json` beside it), so a `--force`
re-diagnosis does not inflate the count.

## Safety boundary

- No GitLab writes at all: only `GET` requests. The loop notifies, nothing else.
- CI logs are untrusted third-party data; the prompt says so and the model
  runs sealed. Notification text is built from the validated answer through
  the chat sanitizers.
- Errors log only the exception class name (never URLs or tokens); one
  failing account, project, pipeline or job trace does not stop the others.
