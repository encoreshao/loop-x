# Loop X Engineering — agent guide

Read [`README.md`](README.md) for what this project is. [`TASK.md`](TASK.md)
indexes every scheduled task this repo runs; read the task's own spec under
`docs/tasks/` (currently just
[`docs/tasks/gitlab-issue-loop.md`](docs/tasks/gitlab-issue-loop.md)) and
[`instructions/loopx/gitlab-issue.md`](instructions/loopx/gitlab-issue.md) before touching
the loop's own decision logic — those are the loop's actual spec, not this file.
This file is operational conventions learned the hard way while building the
dashboard and tooling around it; follow them without being asked.

## Layout

The product is one Rust binary, `loopx`, plus the macOS app that hosts it. There
is no Python runtime left.

- `crates/core` — everything that is not HTTP or CLI parsing: config files, the
  loop runners (GitLab issue, topic monitor, inbox triage, LoopKit plugins), the
  scheduler, connectors, the chat tool registry, evals, launchd helpers.
- `crates/server` — the dashboard: `loopx serve`, the page registry
  (`src/ui/pages/*.rs`, one module per page — `build.rs` registers them), the API
  and SSE the Mac app uses, `src/chat_tool.rs` (the `loopx chat-tool` actions),
  the translation catalogs in `crates/server/locales/{ja,zh,fr}.json`, and UI
  assets in `crates/server/assets/`.
- `crates/cli` — the `loopx` subcommands (thin wrappers over core/server).
- `macos/LoopX` — the Swift app (`swift build`/`swift test` in that directory).
- `instructions/loopx/*.md`, `loops/*/`, `templates/`, `config/` — agent
  prompts and loop definitions; some are embedded into the binary at build time.
- `bin/scripts/*.sh` — shell only: `build_loopx_app.sh`, `install-loopx.sh`,
  `uninstall-loopx.sh`, `release_notes.sh`, and `setup.sh` (installs the
  `gitlab-config` skill). `tests/` — the black-box pytest suite (it drives the
  built binary through `LOOPX_BIN`) and `tests/golden_ui/` page snapshots.

## Development mode: never touch the real `~/.loopx` or real daemons

Day-to-day development and verification happens entirely inside this
checked-out repo directory, against a disposable sandbox — never against the
real `~/.loopx` (this machine's live `projects.json`, `topics.json`,
`ai_cli.json`, `loops.json`, run history), the real Keychain, or the real
installed `launchd` agents. Those are live, personal, possibly-in-use state; a
dev/verification step has no business touching them.

To run a sandboxed dev instance of the dashboard from the current directory:

```bash
export LOOP_ENGINEERING_HOME=$(mktemp -d)   # per-machine config
export HOME=$(mktemp -d)                    # connectors read ~/.gitlab and ~/.slack
export LOOPX_ROOT=$(mktemp -d)              # the "checkout": outputs/, launchd/, logs/
export LOOP_UPDATE_DIR=$(mktemp -d)         # the background update check does a real `git fetch`
export LOOP_EVENTS_DIR=$(mktemp -d)
cargo run -p loopx -- serve --port 18420 &  # any free port, never the live one
sleep 1
curl -s -o /dev/null -w "HTTP %{http_code}\n" http://127.0.0.1:18420/<page>
kill %1
```

`LOOP_ENGINEERING_HOME` overrides the default `~/.loopx` base directory
everywhere it is resolved (`loopx_core::config::home_dir()`, which also migrates a legacy
`~/.loop-engineering` into `~/.loopx` and leaves a symlink); leave it unset and
everything falls back to the real path, which is exactly why it must be set before
running anything in dev/verification. `LOOPX_ROOT` is the per-checkout root
(`outputs/`, `launchd/`, `logs/`); `LOOP_EVENTS_DIR`, `LOOP_EVALS_DIR` and
`LOOP_UPDATE_DIR` redirect the event ledger, eval results and update state.
`tests/conftest.py` sets all of them for every pytest test.

The Keychain service names (`loop-engineering.mail`, `loop-engineering.connectors`)
get a `.sandbox-<hash>` suffix whenever `LOOP_ENGINEERING_HOME` is set, so a
sandboxed run can never read or overwrite the real tokens. Unit tests must still use
the in-memory store (`loopx_core::keychain::MemoryStore`) rather than touch any
Keychain at all. For connector work, a scratch `HOME` matters too:
`connectors_config` reads GitLab instances and Slack webhooks from
`~/.gitlab/config.json` and `~/.slack/config.json`, so without it "Send test
message" on `slack-default` posts to your real Slack.

**`bin/scripts/*.sh` do NOT read `LOOP_ENGINEERING_HOME`**: `install-loopx.sh` and
`uninstall-loopx.sh` default straight to the real install directory unless you pass
their own `--dir`/`--project-dir`/`--launch-agents-dir` flags. Never run them bare for
dev/verification; always point them at a scratch directory (the tests in
`tests/test_install_loopx.py` and `tests/test_uninstall_loopx.py` do exactly this).

The one exception: confirming a reviewed, already-merged change is live on this
machine's real install. Do that only when explicitly asked, never as the routine way
to check a change works. The server does not hot-reload, so a code change only shows
on the real daemon after a restart.

## Test-driven, no exceptions

Every behavior change gets a test first — watch it fail for the right reason, then
make it pass. Run the full suite before considering anything done:

```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p loopx && LOOPX_BIN="$PWD/target/debug/loopx" python3 -m pytest tests/ -q
(cd macos/LoopX && swift test)        # when the app changed
```

Rust unit tests live next to the code; the pytest suite is for black-box
behavior (CLI contracts, end-to-end runs, installers, packaging). Prefer real
subprocesses and temp dirs over mocks; inject fakes through the existing seams
(`UiState::explicit`, `chat_tools::Env::with_provider`, `Launchctl { command }`,
`run_control::Launcher`).

`tests/golden_ui/` holds the rendered HTML of every page in four languages
(`expected/`) and the fixture each page renders against; `crates/server/tests/golden_ui.rs`
compares the Rust pages byte for byte. The snapshots were first captured from the old
Python dashboard and are now plain baselines: when a page changes on purpose, update
the affected files under `expected/` in the same commit.

## Agent instruction/tool changes need a golden eval before/after

Any change to `instructions/loopx/*.md`, `loops/*/prompt.md`,
`crates/core/src/prompt.rs`, or any `allowed_tools()`/`disallowed_tools()` must paste
a before/after `loopx loop eval --golden` summary in the PR (the PR template asks for
it). The unit suite can't tell whether the real agent still fixes, answers and
escalates correctly; the golden suite
(`loopx loop eval --golden [--budget-usd N] [--case NAME]`, cases under
`evals/golden/<name>/case.yaml`) runs the real agent in gate mode, offline, on
synthetic fixture repos. It costs real money (default budget $10, it stops launching
cases once spent), so it is never part of `cargo test` or `pytest` — run it by hand,
once on the base commit and once on your change. Each run writes
`outputs/evals/golden-last.json`. It runs every case under a throwaway
`LOOP_ENGINEERING_HOME`/`LOOP_EVENTS_DIR` and hard-denies the GitLab, Slack and
dashboard tools. That allowlist is **not a hard sandbox**: the fixtures' allowed
`pytest` runs arbitrary code from the fixture repo; the real `HOME` is kept so
`claude` can authenticate; gate-mode handoffs land in this checkout's real
`outputs/handoffs/golden-*`; and the bare `Read Edit Write` allow is not path-scoped,
so the agent can write into the repo as well as the fixture. Run it only on a
machine and checkout you are willing to expose to the agent, and check `git status`
afterwards. Fixtures stay synthetic: never put real issue text or project names in a
case.

The dashboard chat has its own paid suite, since the golden one never exercises the
chat prompt: any change to `crates/core/src/chat_prompts/*.txt`,
`ChatCommand::build_command`'s flags, or a `crates/core/src/chat_tools/` tool
description or write behavior must paste a before/after
`loopx loop eval --chat [--budget-usd N] [--case NAME]` summary in the PR (default
budget $3, about $0.05 a case; cases under `evals/chat/<name>/case.yaml`, written to
`outputs/evals/chat-last.json`). It runs the dashboard's real `claude -p` command with
its one allowed Bash command pointed at `loopx _chat-eval-stub`
(`chat_eval::stub_main`), which drives the real tool registry against canned
connectors, so nothing reaches GitLab, mail or the calendar and no real Keychain or
`~/.loopx` is read. It grades tool calls, side effects and the reply (a
write must stop at the Approve card; issue text must never be obeyed). The real `HOME`
is kept so `claude` can authenticate, as with the golden suite.

## Every state-changing route needs CSRF

Every POST route registered in `crates/server/src/ui/pages/*.rs` is checked by the
registry (`route.csrf` → `ctx.csrf_ok()`) before its handler runs. The token is a
per-process secret embedded only in pages this server itself renders — never weaken
this to "POST-only", which is not a real CSRF defense. The JSON write endpoints in
`crates/server/src/api.rs` use the lock-file token instead (`authorized_to_write`).

`/oauth/google/callback` serves both Inbox Triage and connector (Google Calendar)
sign-ins: the pending state's `kind` (`inbox` by default, `connector`) picks the
handler, so one Google Cloud redirect URI works for both.

## Every new UI string goes through the translator

The dashboard is translated into Japanese, Simplified Chinese, and French
(`crates/server/src/ui/i18n.rs`; the topbar's translate-icon menu sets a `loop_lang`
cookie, falling back to `Accept-Language`, then English). Wrap every new
user-visible string in `crates/server/src/ui/` as `ctx.t("English text")` or
`ctx.tf("English {placeholder}", &[("placeholder", value)])` — always a literal first
argument — and add the same key to `crates/server/locales/ja.json`, `zh.json`, and
`fr.json`. The unit test `every_translated_literal_in_the_ui_sources_has_a_catalog_entry`
fails on any literal missing from a catalog or any placeholder mismatch. Translations
contain apostrophes (French), so anything placed inside an HTML attribute must be
escaped. Never translate at import/static time — translate at render time, since the
language is per request.

## Adding a Material Symbols icon

Roboto and the Material Symbols Outlined icon font are loaded from Google Fonts at
request time, not self-hosted. Adding a new icon means adding its glyph name to
`_MATERIAL_SYMBOLS_ICON_NAMES` in `crates/server/assets/constants.json`, keeping the
list alphabetically sorted: Google's `icon_names=` parameter subsets the served font to
exactly that list, so a glyph name used in markup but missing from this list renders
as tofu/missing glyph (and the golden snapshots change, so update them too).

## A loading spinner's motion is functional, not decorative

Purely decorative animations (`.pulse-dot`, the topbar progress sliver) are gated
behind `@media (prefers-reduced-motion: no-preference)`, and should stay that way. A
*loading* spinner is different: its motion is the only signal a page is still working,
so `.md-spinner`'s animation is deliberately **not** gated — under
`prefers-reduced-motion`, it would otherwise look permanently frozen rather than just
calmer. Don't move it back inside that media block.

## Flexbox: watch for the shrink-and-clip trap

A flex child with `flex-shrink: 0` forces *all* the shrinkage onto its sibling(s). If
that sibling also has `overflow: hidden`, it can be crushed to near-zero width and its
content effectively disappears — even though every `display`/visibility property on it
is technically correct. This bit the collapsed sidebar once (the brand icon vs. the
toggle button competing for a 64px rail); if a collapsed/narrow layout ever "loses" an
element that has `display` set correctly, check the flex-shrink math on its container
before touching `display` again.

## Page sections: let the shared card spacing do it

`.card` has no margin by design. Sibling cards at page level (direct children of
`.wrap`) get their 1.25rem gap from the shared `.wrap > .card + *` rule; cards inside a
container go in `.grid` or a flex stack that sets `gap`. Never add per-page margin
hacks, and never wrap a new view's cards in an extra div that isn't `.grid`/a gap
stack — that silently opts out of the shared rule. This bit Connectors → Accounts and
Harness → Gates.

## A bare `bundle exec rubocop` lint_cmd silently breaks under `worktree_root`

Any `worktree_root` nested under a dot-directory (the scaffolded default,
`~/.loopx/worktrees`, is one) trips a real RuboCop quirk:
`TargetFinder#hidden_path?` (`lib/rubocop/target_finder.rb`) treats any
scan root whose path contains a hidden (dot-prefixed) directory component
as a "hidden path," and for that case only, skips every *file-level*
`AllCops: Exclude` entry — directory-level excludes like `vendor/**/*`
still get pruned normally, but per-file excludes (`Gemfile`, `Capfile`,
`bin/*`, a specific old migration) do not. So a project's own worktree,
scanned with a bare `bundle exec rubocop`, reports pre-existing "offenses"
in files the project explicitly excludes — offenses a normal checkout of
the same commit never shows, since it isn't under a dot-directory. This
showed up for real as a loop-posted GitLab comment second-guessing a lint
"baseline" that didn't actually exist; nothing was wrong with the target
project's code, config, or the loop's decision logic — only with how the
configured `lint_cmd` string happened to be phrased.

Fix it at the `lint_cmd` layer in `~/.loopx/projects.json`, not
by moving `worktree_root`: pass an explicit path argument, e.g.
`bundle exec rubocop .` instead of bare `bundle exec rubocop`. An explicit
path argument makes `TargetFinder` take `process_explicit_path`/the
`target_files_in_dir(arg)` branch with a *relative* base dir ("."), which
isn't subject to `hidden_path?`, so per-file Excludes apply correctly
again — confirmed directly by running both forms back-to-back in the same
worktree and diffing the file counts and offenses. This isn't
rubocop-specific in principle (any tool with its own "am I scanning a
dotfiles-style hidden path" heuristic could do the same thing), so if a
future project's `lint_cmd` shows a similar "offenses only in the loop's
worktree, never locally" gap, suspect this same class of bug before
suspecting the project's config.

## An untracked config file can flip lint results, and RuboCop's cache won't notice

`new_worktree.sh` copies `.ruby-version` into every fresh worktree because
it's gitignored and `git worktree add` only checks out tracked files. The
same is true of `config/database.yml` (per-developer DB credentials) — and
for kurrant.web specifically, its absence doesn't just break
`bundle exec rspec` (`RuntimeError: Could not load database configuration`);
it also makes `bundle exec rubocop` report bogus offenses. rubocop-rails'
`Rails/BulkChangeTable` cop (`DatabaseTypeResolvable#database_from_yaml`)
reads `config/database.yml` to detect the DB adapter; when the file is
missing, the cop can't tell whether `bulk: true` even applies and quietly
disables itself — which makes every pre-existing
`# rubocop:disable Rails/BulkChangeTable` comment in old migrations look
like a `Lint/RedundantCopDisableDirective` offense. This looked exactly
like real, slowly-shrinking lint debt (a run once reported 42 offenses,
a later run 5) and got escalated as "please confirm whether this baseline
is acceptable" — but it was never project debt at all: the same commit,
rubocop'd from a normal checkout that has `database.yml`, reports zero
offenses. Confirmed directly: reproduced a worktree under a dot-directory
path with `database.yml` deliberately absent (42, then 5, offenses,
matching the real runs), then copied `database.yml` in and reran — same
result, still 5 — before realizing why: see the cache paragraph below.
`new_worktree.sh` now copies `config/database.yml` the same way it copies
`.ruby-version`, which fixes this at the source.

Don't stop at "copy the file," though — RuboCop's result cache
(`~/.cache/rubocop_cache` by default, global and shared across every
project and every worktree on the machine) keys each cached result on
`file path + file mode + effective-config signature + file content digest`
(`ResultCache#file_checksum`). None of that includes `config/database.yml`,
so a cop whose *result* depends on that file's presence (like
`Rails/BulkChangeTable` above) can get its wrong answer cached and keep
serving it forever, even after the missing file is fixed — confirmed by
rerunning the same worktree with `--cache false`: 0 offenses, immediately,
with no other change. Because a loop worktree's path is deterministic per
issue (`worktree_root/<repo>-issue-<iid>`), a run made **before** the
`database.yml` copy fix could have already poisoned the cache for that
exact path, and a later run — even after the fix — would keep reading the
stale cached offenses rather than recomputing. `kurrant.web`'s `lint_cmd`
in `~/.loopx/projects.json` now passes `--cache false` for this
reason. Weigh this against the loop's runtime budget if a future project's
lint step is slow enough that full-project caching actually matters —
but for any cop (in any project) that reads external state outside what
RuboCop's own cache key covers, prefer disabling the cache over trying to
guess which cached entries are stale.

## Changelog and releases

`CHANGELOG.md` follows Keep a Changelog with SemVer (`0.x` for now). Every
user-visible change - a new loop, page, setting, connector, CLI command or a
behavior change someone would notice - adds one line under `## [Unreleased]`
(in `Added` / `Changed` / `Fixed` / `Removed` / `Security`) in the same commit
or PR. Pure refactors, test-only and internal-doc changes don't need a line.
`tests/test_changelog.py` checks the structure (Unreleased first, dated
`X.Y.Z` releases in descending order, a link reference for each heading).

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
