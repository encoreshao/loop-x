# Paid agent evals (golden and chat)

Moved out of `CLAUDE.md`. Short trigger rules live there; read this before running either suite.

## Agent instruction/tool changes need a golden eval before/after

Any change to `instructions/loopx/*.md`, `loops/*/prompt.md`,
`crates/core/src/prompt.rs`, or any `allowed_tools()`/`disallowed_tools()` must paste
a before/after `loopx loop eval --golden` summary in the PR (the PR template asks for
it). The unit suite can't tell whether the real agent still fixes, answers and
escalates correctly; the golden suite
(`loopx loop eval --golden [--budget-usd N] [--case NAME]`, cases under
`evals/golden/<name>/case.yaml`) runs the real agent in gate mode, offline, on
synthetic fixture repos. It costs real money (default budget $10, it stops launching
cases once spent), so it is never part of `cargo test` — run it by hand,
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
