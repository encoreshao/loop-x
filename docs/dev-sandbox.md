# Dev sandbox details

Moved out of `CLAUDE.md`. The env-var block and the hard rules stay there.

`LOOP_ENGINEERING_HOME` overrides the default `~/.loopx` base directory
everywhere it is resolved (`loopx_core::config::home_dir()`, which also migrates a legacy
`~/.loop-engineering` into `~/.loopx` and leaves a symlink); leave it unset and
everything falls back to the real path, which is exactly why it must be set before
running anything in dev/verification. `LOOPX_ROOT` is the per-checkout root
(`outputs/`, `launchd/`, `logs/`); `LOOP_EVENTS_DIR`, `LOOP_EVALS_DIR` and
`LOOP_UPDATE_DIR` redirect the event ledger, eval results and update state.

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
dev/verification; always point them at a scratch directory.

The one exception: confirming a reviewed, already-merged change is live on this
machine's real install. Do that only when explicitly asked, never as the routine way
to check a change works. The server does not hot-reload, so a code change only shows
on the real daemon after a restart.
