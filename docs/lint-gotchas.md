# Lint gotchas for loop worktrees

Moved out of `CLAUDE.md` to keep agent context small. The short form lives in step 7 of `instructions/loopx/gitlab-issue.md`.

## A bare `bundle exec rubocop` lint_cmd silently breaks under `worktree_root`

The GitLab agent now chooses its own lint/test commands (step 7 of
`instructions/loopx/gitlab-issue.md`, which carries the short form of this and
the next section); a project's stored `lint_cmd` is only a hint it starts from.

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

