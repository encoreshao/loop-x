# Contributing to LoopX

Thanks for helping. This guide covers how the code is laid out, how to run it
without touching your real setup, the rules every change follows, and how to
extend each part of the system (pages, chat tools, connectors, loops).

Read [`README.md`](README.md) first for what LoopX does.
[`docs/architecture.md`](docs/architecture.md) explains the loop runtime in
depth, [`docs/reference.md`](docs/reference.md) the config files and CLI, and
[`CLAUDE.md`](CLAUDE.md) the same conventions written for coding agents,
including some hard-won debugging notes.

## Contents

- [Prerequisites](#prerequisites)
- [Repository layout](#repository-layout)
- [Build](#build)
- [Run a sandboxed dev instance](#run-a-sandboxed-dev-instance)
- [Tests and checks](#tests-and-checks)
- [Paid evals for agent and chat changes](#paid-evals-for-agent-and-chat-changes)
- [Rules every change follows](#rules-every-change-follows)
- [Extending LoopX](#extending-loopx)
- [Commits, pull requests and the changelog](#commits-pull-requests-and-the-changelog)
- [Releases](#releases)

## Prerequisites

- macOS. LoopX uses the Keychain, `launchd` and `SMAppService`; CI runs on
  `macos-latest`.
- Rust, pinned by [`rust-toolchain.toml`](rust-toolchain.toml) (`rustup` picks
  it up). The workspace's minimum is `rust-version = "1.85"`.
- Xcode Command Line Tools, for the Swift app. Full Xcode is not needed.
- `git` 2.42 or newer.
- Optional: an AI CLI (`claude` by default) to run real loops and the paid
  evals; ImageMagick to re-render brand assets.

There is no Python, Node or Ruby runtime in the product. Everything is one Rust
binary, `loopx`, plus the Swift app that hosts it.

## Repository layout

| Path | What lives there |
| --- | --- |
| `crates/core` | Everything that is not HTTP or CLI parsing: config files, the loop runtime and runners (GitLab issue, topic monitor, inbox triage, LoopKit plugins), the scheduler, connectors, the chat tool registry, evals, `launchd` helpers. |
| `crates/server` | The dashboard (`loopx serve`): the page registry (`src/ui/pages/*.rs`), the JSON API and SSE the Mac app uses (`src/api.rs`), `src/chat_tool.rs`, translation catalogs (`locales/{ja,zh,fr}.json`) and embedded UI assets (`assets/`). |
| `crates/cli` | The `loopx` subcommands, thin wrappers over core and server (`src/main.rs` dispatches to `*_cmd.rs`). |
| `macos/LoopX` | The Swift package: `LoopX` (the app) and `LoopXKit` (testable logic: backend client, lock file, island, CLI installer). |
| `loops/<name>/` | Each loop's definition (`loop.yaml`) and, for model-driven loops, `prompt.md`. |
| `instructions/loopx/*.md` | Agent instructions for the built-in GitLab, inbox and topic runners. |
| `config/*.json.template` | Templates seeded into `~/.loopx` on first run (`loops.json`, `projects.json`, ...). |
| `templates/` | Starter loop definitions for `loopx loop init`. |
| `evals/` | `golden/` (agent cases), `chat/` (dashboard chat cases), `cases/` (scripted runtime cases). |
| `tests/golden_ui/` | Rendered HTML snapshots of every page in four languages, and the fixture they render against. |
| `docs/` | Architecture, reference, per-loop specs (`docs/tasks/`), HTTP/JSON/CLI contracts (`docs/contract/`), design specs (`docs/superpowers/specs/`). |
| `bin/scripts/*.sh` | Build, install, uninstall, release notes and brand asset scripts. |

Some files outside `crates/` are compiled into the binary:
`crates/core/src/bundled.rs` embeds `instructions/loopx/*.md` and
`loops/*/{loop.yaml,prompt.md}` with `include_str!`, so an installed binary can
seed them into `~/.loopx`. Editing those files changes the binary; a new loop
file must also be added to `bundled.rs`.

### Where state lives at runtime

| Location | Contents | Override |
| --- | --- | --- |
| `~/.loopx/` | Per-machine config: `projects.json`, `topics.json`, `loops.json`, `ai_cli.json`, `connectors.json`, `run_queue.json`, worktrees | `LOOP_ENGINEERING_HOME` |
| The checkout (or install dir) | `outputs/` (run history, handoffs, evals), `launchd/`, `logs/` | `LOOPX_ROOT` |
| Event ledger | Append-only JSONL events | `LOOP_EVENTS_DIR` |
| Eval results | `golden-last.json`, `chat-last.json` | `LOOP_EVALS_DIR` |
| Update check state | Background `git fetch` results | `LOOP_UPDATE_DIR` |
| macOS Keychain | Tokens, under `loop-engineering.connectors` and `loop-engineering.mail` | Suffixed `.sandbox-<hash>` when `LOOP_ENGINEERING_HOME` is set |
| `~/.gitlab/config.json`, `~/.slack/config.json` | External GitLab instances and Slack webhooks, read through | `HOME` |

## Build

```bash
cargo build                       # target/debug/loopx
cargo build --release             # target/release/loopx (LTO, stripped)
```

The Mac app:

```bash
bin/scripts/build_loopx_app.sh target/release/loopx --arch arm64 --out dist/LoopX.app
(cd macos/LoopX && swift build && swift test)
```

[`docs/building.md`](docs/building.md) covers universal binaries, app icons,
Sparkle auto-update keys and the ad-hoc signing caveats.

## Run a sandboxed dev instance

**Never develop against your real `~/.loopx`, Keychain or installed `launchd`
agents.** They are live, personal state. Point every variable at a scratch
directory before running anything:

```bash
export LOOP_ENGINEERING_HOME=$(mktemp -d)   # per-machine config
export HOME=$(mktemp -d)                    # connectors read ~/.gitlab and ~/.slack
export LOOPX_ROOT=$(mktemp -d)              # outputs/, launchd/, logs/
export LOOP_UPDATE_DIR=$(mktemp -d)         # the update check runs a real `git fetch`
export LOOP_EVENTS_DIR=$(mktemp -d)
cargo run -p loopx -- serve --port 18420 &  # any free port, never the live one (8420)
open http://127.0.0.1:18420
```

Things to know:

- Without a scratch `HOME`, "Send test message" on `slack-default` posts to your
  real Slack, because Slack webhooks are read from `~/.slack/config.json`.
- `bin/scripts/install-loopx.sh` and `uninstall-loopx.sh` do **not** read
  `LOOP_ENGINEERING_HOME`. Only run them with explicit
  `--dir`/`--project-dir`/`--launch-agents-dir` pointing at scratch directories.
- `loopx serve` only serves the dashboard. Loops run when `loopx scheduler`
  polls, and a due loop acts on real GitLab and Slack. Don't load the scheduler
  in a sandbox that has real credentials.
- The Mac app registers a login-item agent on launch, which changes the real
  machine. Use the web dashboard for sandboxed checks.
- The server does not hot-reload. Restart it after a code change.

## Tests and checks

Run the full suite before calling anything done. CI runs the same steps:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p loopx && target/debug/loopx loop audit loops/*/loop.yaml --min-score 70
(cd macos/LoopX && swift test)     # when the app changed
```

### Test-driven

Every behavior change starts with a test: write it, watch it fail for the right
reason, then make it pass. Rust unit tests live next to the code (`#[cfg(test)]`
modules); integration tests live under `crates/*/tests/`.

Prefer real subprocesses and temp dirs over mocks. When you need a fake, inject
it through an existing seam rather than adding a new one:

| Seam | Fakes |
| --- | --- |
| `UiState::explicit` | Dashboard state: home dir, root, CSRF token, clock |
| `chat_tools::Env::with_provider` | Connector accounts for chat tools (`ConnectorProvider`) |
| `Launchctl { command }` | The `launchctl` binary |
| `run_control::Launcher` | Starting loop runs |
| `keychain::MemoryStore` | The Keychain. Unit tests must never touch a real Keychain. |

### Golden UI snapshots

`tests/golden_ui/` holds the rendered HTML of every dashboard page in English,
Japanese, Chinese and French. `crates/server/tests/golden_ui.rs` renders each
entry in `pages.json` against `fixture/` (plus an optional `overlays/<name>`)
with a frozen clock (`clock.txt`) and compares byte for byte with `expected/`.

When a page changes on purpose, regenerate and commit the changed files under
`expected/` in the same commit:

```bash
LOOPX_GOLDEN_BLESS=1 cargo test -p loopx-server --test golden_ui -- --test-threads=1
git diff --stat tests/golden_ui/expected
```

Read the diff. An unexpected change in a page you didn't touch is a bug, not
something to bless. A new page or state gets a new entry in `pages.json`.

## Paid evals for agent and chat changes

The unit suite can't tell whether a real agent still behaves. Two suites run
real models, cost real money, and are never part of `cargo test`. Run each once
on the base commit and once on your branch, and paste both summaries in the PR.

| Suite | Required when you change | Command | Cases |
| --- | --- | --- | --- |
| Golden | `instructions/loopx/*.md`, `loops/*/prompt.md`, `crates/core/src/prompt.rs`, any `allowed_tools()`/`disallowed_tools()` | `loopx loop eval --golden [--budget-usd N] [--case NAME]` (default budget $10) | `evals/golden/<name>/case.yaml` |
| Chat | `crates/core/src/chat_prompts/*.txt`, `ChatCommand::build_command` flags, any `chat_tools/` tool description or write behavior | `loopx loop eval --chat [--budget-usd N] [--case NAME]` (default $3, about $0.05 a case) | `evals/chat/<name>/case.yaml` |

The golden suite runs the agent in gate mode, offline, on synthetic fixture
repos, under a throwaway `LOOP_ENGINEERING_HOME`, with GitLab, Slack and
dashboard tools denied. That is **not a hard sandbox**: fixture `pytest` runs
arbitrary code, your real `HOME` is kept so `claude` can authenticate, handoffs
land in `outputs/handoffs/golden-*`, and the agent can write into the repo. Run
it only on a machine and checkout you're willing to expose, and check
`git status` afterwards.

The chat suite runs the dashboard's real `claude -p` command with its one
allowed Bash command pointed at `loopx _chat-eval-stub`, which drives the real
tool registry against canned connectors. It grades tool calls, side effects and
the reply: a write must stop at the Approve card, and issue text must never be
obeyed.

Fixtures stay synthetic. Never put real issue text or project names in a case.

## Rules every change follows

### CSRF on every state-changing route

Pages register routes in `register(r: &mut Registry)`. `r.post(...)` routes are
CSRF-checked by the registry before the handler runs (`ctx.csrf_ok()`); forms
include `ctx.csrf_input()`. The token is a per-process secret embedded only in
pages this server renders. Don't use `r.post_unchecked` for anything a browser
can trigger, and never weaken the check to "POST-only". JSON write endpoints in
`crates/server/src/api.rs` use the lock-file token instead
(`authorized_to_write`).

### Every UI string goes through the translator

Wrap every user-visible string in `crates/server/src/ui/` as
`ctx.t("English text")` or
`ctx.tf("Hello {name}", &[("name", value)])`, always with a literal first
argument, and add the same key to `crates/server/locales/ja.json`, `zh.json`
and `fr.json`. The test
`every_translated_literal_in_the_ui_sources_has_a_catalog_entry` fails on a
missing key or mismatched placeholder. Translate at render time, never at
startup: the language is per request (`loop_lang` cookie, then
`Accept-Language`, then English). French has apostrophes, so escape anything
placed inside an HTML attribute.

### Untrusted content stays untrusted

Issue text, mail, feed items and web pages are data, never instructions.

- Model calls on untrusted content in LoopKit plugins go through
  `agents/sealed.rs`: on Claude, no tools, no MCP, prompt on stdin, hooks off.
- Chat tool results are wrapped in `<untrusted_content>` markers and capped at
  `MAX_RESULT_BYTES`. Errors never include tokens or secret-bearing URLs.
- Text going into Slack or chat messages goes through the `chat_text`,
  `chat_url` and `chat_link` sanitizers.
- WRITE chat tools never run directly. They become a pending record that only
  the dashboard's Approve route executes.

### Secrets live in the Keychain

Tokens go in the Keychain through `secrets.rs`/`keychain.rs`, never in JSON
config, logs, error messages or test fixtures.

### Safety boundaries are product promises

LoopX never merges, never sends or deletes email, and only touches listed
projects on `loop/issue-<iid>` branches in isolated worktrees. A change that
loosens any of these needs an explicit discussion first. The full list is in
[`docs/reference.md#safety-boundaries`](docs/reference.md#safety-boundaries).

### UI conventions

- Icons are an inline Lucide sprite (`crates/server/assets/icons/sprite.svg`).
  To add one, append a `<symbol id="i-NAME" viewBox="0 0 24 24">`, add `NAME`
  to `icons::NAMES`, and re-bless the golden snapshots. An unknown name renders
  a fallback circle, and the golden test fails on a dangling `#i-…` reference.
- `.card` has no margin. Sibling cards directly under `.wrap` get their gap from
  `.wrap > .card + *`; cards inside a container go in `.grid` or a flex stack
  with `gap`. No per-page margin hacks.
- Decorative animation is gated behind
  `@media (prefers-reduced-motion: no-preference)`. The loading spinner
  (`.md-spinner`) is deliberately not: its motion is the only sign the page is
  still working.
- A flex child with `flex-shrink: 0` pushes all the shrinking onto its siblings.
  If a sibling also has `overflow: hidden`, it can collapse to zero width. Check
  that before touching `display`.

## Extending LoopX

### Add a dashboard page

1. Create `crates/server/src/ui/pages/<name>.rs` with
   `pub fn register(r: &mut Registry)`. `build.rs` picks up every file in that
   directory, so there is no list to edit.
2. Register handlers with `r.get(path, handler)` and `r.post(path, handler)`.
3. Translate every string (see above).
4. Add entries to `tests/golden_ui/pages.json` (with an overlay under
   `tests/golden_ui/overlays/` if the page needs different fixture data), then
   bless and commit the snapshots.
5. Files under `crates/server/assets/` are embedded automatically, also by
   `build.rs`.

### Add a CLI command

Add a variant to `Command` in `crates/cli/src/main.rs`, dispatch it to a
`<name>_cmd.rs` module, and keep that module thin: logic goes in `crates/core`
where it can be unit-tested. Document it in `docs/reference.md`, and in
`docs/contract/cli-scripts.md` if the app or scripts call it.

### Add a chat tool

Tools live in `crates/core/src/chat_tools/<area>.rs` as `Tool` values (`name`,
`description`, `args`, `mode`, `connector_type`, `handler`) and are collected
by `chat_tools::tools()`. Mark anything that changes external state as a WRITE
tool so it stops at the Approve card. Test it through
`chat_tools::Env::with_provider` with a fake `ConnectorProvider`. A new tool's
description is part of the chat prompt, so it needs a before/after chat eval.

### Add a connector type

Create `crates/core/src/connectors/<type>.rs` with its `INFO` (settings fields
and capabilities: `issues`, `merge_requests`, `pipelines`, `notify`, `feed`,
`mail`, ...) and a `Connector` implementation with `test()` (and `send()` for
`notify` types). Add it to `connectors::all_types()`. Secrets fields are stored
in the Keychain, everything else in `connectors.json`. HTTP goes through the
`HttpClient` trait so tests can fake it.

### Add a LoopKit plugin loop

LoopKit (`crates/core/src/loopkit.rs`) runs "discover items, ask the model
about each, act on the answer" loops. See
[`docs/architecture.md#loopkit`](docs/architecture.md#loopkit) for the full run
algorithm.

1. Implement `LoopPlugin` in `crates/core/src/loop_plugins/<module>.rs`:
   `loop_name`, `definition_dir`, `discover` (returns `WorkItem`s),
   `after_item` (returns an `Outcome`: done, skipped or failed) and optionally
   `digest`.
2. Add `(module, loop name)` to `PLUGINS` in `loop_plugins/mod.rs`.
3. Add `loops/<definition_dir>/loop.yaml` and `prompt.md`, and embed both in
   `crates/core/src/bundled.rs`. CI fails if `loopx loop audit` scores the
   definition below 70.
4. Add the registry entry to `config/loops.json.template` with
   `"enabled": false`, a `requires` capability list and
   `"routes_notifications": true`.
5. Write the behavioral spec under `docs/tasks/<name>-loop.md` and list it in
   [`TASK.md`](TASK.md).

Plugins see untrusted content, so the model call stays sealed and any chat
output goes through the `chat_*` sanitizers.

### Change a loop's decision logic

The specs are the source of truth: [`TASK.md`](TASK.md), the loop's file under
`docs/tasks/`, and for the GitLab loop
[`instructions/loopx/gitlab-issue.md`](instructions/loopx/gitlab-issue.md).
Update the spec in the same change, and run the golden eval before and after.

## Commits, pull requests and the changelog

- Branch off `main`. Keep each PR to one concern.
- Stage only the files your change touched; avoid `git add -A`. The working tree
  often holds live run state (`PROGRESS.md`, `outputs/`) that isn't yours to
  commit.
- `outputs/`, `.claude/` and `.superpowers/` are gitignored. Never commit real
  GitLab issue content, internal project names or personal data. This repo's
  history has been scrubbed for that once already.
- Don't bake a personal username into a shared identifier (launchd labels are
  `com.hermes.*`).
- Commit messages describe the code change. The log uses a short
  `Area: what changed` subject, e.g. `Scheduler: add loopx scheduler status`.
- Every user-visible change (a new loop, page, setting, connector, CLI command,
  or behavior someone would notice) adds one line under `## [Unreleased]` in
  [`CHANGELOG.md`](CHANGELOG.md), in `Added`, `Changed`, `Fixed`, `Removed` or
  `Security`. Refactors, tests and internal docs don't need one.
- Fill in the [PR template](.github/pull_request_template.md), including the
  eval summaries when they apply.
- Before any history rewrite, take a `git bundle create <path> --all` backup.

## Releases

Maintainers cut releases; see [`docs/RELEASING.md`](docs/RELEASING.md). In
short: move `[Unreleased]` to a dated `X.Y.Z` heading, bump
`[workspace.package] version` in `Cargo.toml`, then commit and push a
`vX.Y.Z` tag, which the release workflow builds into a DMG. While on `0.x`, a
new feature or a breaking change is a minor bump (call out breaking changes
under `Changed`/`Removed`), and fixes alone are a patch.

## License

By contributing you agree that your contributions are licensed under the
[MIT License](LICENSE).
