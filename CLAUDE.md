# Loop X Engineering — agent guide

One Rust binary, `loopx`, plus the macOS app that hosts it (no Python runtime).
Read [`README.md`](README.md) for what it is. Before touching the loop's decision logic,
read [`TASK.md`](TASK.md), [`docs/tasks/gitlab-issue-loop.md`](docs/tasks/gitlab-issue-loop.md)
and [`instructions/loopx/gitlab-issue.md`](instructions/loopx/gitlab-issue.md) — that is the
loop's real spec, not this file. This file is operational conventions; follow them unasked.

## Commands

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd macos/LoopX && swift test)     # when the app changed
bin/scripts/ci-check.sh            # exactly what GitHub CI runs; run before every push
LOOPX_GOLDEN_BLESS=1 cargo test -p loopx-server --test golden_ui -- --test-threads=1   # regenerate page snapshots
```

## Layout

- `crates/core` — config, loop runners (GitLab issue, topic monitor, inbox triage, LoopKit), scheduler, connectors, chat tool registry, evals, launchd helpers.
- `crates/server` — dashboard (`loopx serve`), page registry `src/ui/pages/*.rs` (one module per page; `build.rs` registers them), API/SSE for the Mac app, `src/chat_tool.rs`, translation catalogs `locales/{ja,zh,fr}.json`, UI assets `assets/`.
- `crates/cli` — `loopx` subcommands (thin wrappers over core/server).
- `macos/LoopX` — Swift app.
- `instructions/loopx/*.md`, `loops/*/`, `templates/`, `config/` — agent prompts and loop definitions (some embedded at build time).
- `bin/scripts/*.sh` — shell only (build/install/uninstall, release notes, brand assets, `setup.sh`). `tests/golden_ui/` — page snapshots.

## Development: never touch the real `~/.loopx`, Keychain or launchd agents

Verify against a disposable sandbox inside this checkout. Real state is live and personal.

```bash
export LOOP_ENGINEERING_HOME=$(mktemp -d)   # per-machine config (unset = REAL ~/.loopx)
export HOME=$(mktemp -d)                    # connectors read ~/.gitlab and ~/.slack
export LOOPX_ROOT=$(mktemp -d)              # outputs/, launchd/, logs/
export LOOP_UPDATE_DIR=$(mktemp -d)         # update check does a real `git fetch`
export LOOP_EVENTS_DIR=$(mktemp -d)
cargo run -p loopx -- serve --port 18420 &  # any free port, never the live one
sleep 1; curl -s -o /dev/null -w "HTTP %{http_code}\n" http://127.0.0.1:18420/<page>; kill %1
```

- `bin/scripts/*.sh` do NOT read `LOOP_ENGINEERING_HOME`: never run `install-loopx.sh`/`uninstall-loopx.sh` bare; pass `--dir`/`--project-dir`/`--launch-agents-dir` to a scratch dir.
- Unit tests use `loopx_core::keychain::MemoryStore`, never a real Keychain. A scratch `HOME` stops "Send test message" posting to real Slack.
- Touch the real install only to confirm an already-merged change, and only when asked. The server does not hot-reload; restart the daemon.
- More detail: [`docs/dev-sandbox.md`](docs/dev-sandbox.md).

## Testing

- Test first: watch it fail for the right reason, then pass. Run the full suite before calling anything done.
- Prefer real subprocesses and temp dirs over mocks; inject fakes via existing seams (`UiState::explicit`, `chat_tools::Env::with_provider`, `Launchctl { command }`, `run_control::Launcher`).
- A page change must regenerate and commit `tests/golden_ui/expected/` in the same commit (the usual red CI is forgetting this).

## Paid evals required in the PR

- Changing `instructions/loopx/*.md`, `loops/*/prompt.md`, `crates/core/src/prompt.rs`, or any `allowed_tools()`/`disallowed_tools()`: paste before/after `loopx loop eval --golden [--budget-usd N] [--case NAME]` (default $10).
- Changing `crates/core/src/chat_prompts/*.txt`, `ChatCommand::build_command` flags, or a `crates/core/src/chat_tools/` description or write behavior: paste before/after `loopx loop eval --chat` (default $3).
- Run by hand, once on the base commit and once on your change; never in `cargo test`. The golden suite is not a hard sandbox (real `HOME`, unscoped Edit/Write): run only on a checkout you'll expose, check `git status` after, keep fixtures synthetic (no real issue text or project names). Details: [`docs/evals.md`](docs/evals.md).

## Server rules

- Every POST route in `crates/server/src/ui/pages/*.rs` goes through the registry CSRF check (`route.csrf` → `ctx.csrf_ok()`); never weaken it to "POST-only". JSON write endpoints in `api.rs` use the lock-file token (`authorized_to_write`).
- `/oauth/google/callback` serves Inbox Triage and connector sign-ins; the pending state's `kind` picks the handler.
- Every new user-visible string in `crates/server/src/ui/` is `ctx.t("literal")` / `ctx.tf("literal {x}", &[("x", v)])` (literal first argument, translate at render time, never at static time) and gets the same key in `locales/ja.json`, `zh.json`, `fr.json`. Escape translations placed in HTML attributes (French apostrophes). The `every_translated_literal_in_the_ui_sources_has_a_catalog_entry` test enforces it.

## UI rules

Details and reasons: [`docs/ui-conventions.md`](docs/ui-conventions.md).

- Look like Apple's apps and website: system fonts, hairline separators, 12-16px radii, soft shadows, blurred material, one system-blue accent, pill buttons, eased 200-350ms transitions, light/dark parity. Colors come from the `--md-*` tokens. Prefer a sheet or right-hand inspector over a new tab or popup.
- Not done until rendered: run the sandbox, open every state (collapsed/expanded, empty/filled, light/dark), take a headless Chrome screenshot (`--headless=new --screenshot=...`) and read it.
- Never reuse a layout-coupled class in another layout (`.daemon-action-form`'s `flex: 1 1 160px` is a height in a column). Write the layout's own classes; every input, select, textarea and button has an explicit height.
- One form, one Save; one control per setting.
- New icon: append `<symbol id="i-NAME" viewBox="0 0 24 24">` to `crates/server/assets/icons/sprite.svg`, add `NAME` to `icons::NAMES`, bless the golden snapshots.
- `.md-spinner`'s animation is deliberately NOT behind `prefers-reduced-motion`; decorative animations are.
- A flex child with `flex-shrink: 0` pushes all shrinkage onto an `overflow: hidden` sibling and can crush it to zero; check flex math before touching `display`.
- `.card` has no margin: page-level siblings get spacing from `.wrap > .card + *`; otherwise use `.grid` or a `gap` stack. No per-page margin hacks or extra wrapper divs.

## Lint gotchas in loop worktrees

Bare `bundle exec rubocop` under a dot-directory `worktree_root` skips per-file excludes,
and RuboCop's global cache can serve stale results when `config/database.yml` is missing.
Pass an explicit path and `--cache false`. Full write-up: [`docs/lint-gotchas.md`](docs/lint-gotchas.md).

## Changelog and releases

`CHANGELOG.md` follows Keep a Changelog with SemVer (`0.x` for now). Every
user-visible change - a new loop, page, setting, connector, CLI command or a
behavior change someone would notice - adds one line under `## [Unreleased]`
(in `Added` / `Changed` / `Fixed` / `Removed` / `Security`) in the same commit
or PR. Pure refactors, test-only and internal-doc changes don't need a line.
Keep the structure: Unreleased first, dated `X.Y.Z` releases in descending
order, a link reference for each heading.

To cut a release, only when asked:

1. Rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` and add a fresh empty
   `## [Unreleased]` above it.
2. Update the link references at the bottom: `[Unreleased]` compares
   `vX.Y.Z...HEAD`, and add `[X.Y.Z]` pointing at the `vX.Y.Z` release tag.
3. Commit, then `git tag -a vX.Y.Z -m "vX.Y.Z"` on that commit and push the
   branch and the tag.

Minor bump for new features, patch for fixes only; while on `0.x` a breaking
change is a minor bump and must be called out under `Changed`/`Removed`.

## Git hygiene for this repo

- `outputs/` (`daily-review.md`, `messages.json`, `history/*.md`), `.claude/`,
  and `.superpowers/` are gitignored — they're personal/live/session state,
  never project source. If you find real GitLab issue content or internal
  project names about to be committed, stop and ask; this repo has already
  had its history scrubbed once for exactly that (`git filter-repo --path
  outputs --invert-paths`).
- Never bake a personal username into a shared identifier (launchd labels
  are `com.hermes.*`, not `com.<developer>.*`). This doesn't apply to real
  OS paths like `/Users/<you>/...` — those aren't a style choice.
- Before any history-rewriting operation (`git filter-repo`, rebase across
  many commits), take a `git bundle create <path> --all` backup first, and
  copy any live untracked runtime files elsewhere — a rewrite's checkout
  step can otherwise wipe them from the working tree.
- Only commit when explicitly asked. Stage precisely the files the current
  task touched, not `-A`/`.` — this repo's own live run state
  (`PROGRESS.md`, `outputs/`) is often sitting modified in the working tree
  from the loop's own runs and isn't part of whatever you were just asked
  to do.
