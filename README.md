<p align="center">
  <img src="assets/app-icon.png" alt="LoopX logo" width="200" height="200">
</p>

<p align="center">
  <b>English</b> | <a href="README.ja.md">日本語</a> | <a href="README.zh-CN.md">简体中文</a> | <a href="README.fr.md">Français</a>
</p>

<h1 align="center">LoopX Engineering</h1>

<p align="center">
  <a href="https://github.com/encoreshao/loop-x/actions/workflows/rust.yml"><img src="https://github.com/encoreshao/loop-x/actions/workflows/rust.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT"></a>
  <a href="https://github.com/encoreshao/loop-x/releases/latest"><img src="https://img.shields.io/badge/releases-download-brightgreen" alt="Releases"></a>
  <img src="https://img.shields.io/badge/platform-macOS-lightgrey" alt="Platform">
</p>

<p align="center"><strong>Get your time back from the daily grind of issues, email and status checks.</strong></p>

---

LoopX is a local-first AI agent orchestration platform for macOS. It runs specialized, scheduled AI agents that automate engineering workflows and everyday knowledge work, so you only spend attention on what needs your judgment.

- **Loops.** Independent, configurable workflows, each with its own task spec, schedule and run history.
- **AI agents.** Coding agents such as Claude Code, Codex, Gemini, Kimi and Cursor reason about the task and do the work.
- **Orchestration.** LoopX runs each loop on its own, manages the run and carries it from finding the task to a reviewable result.
- **Human in the loop.** Agents fix issues and draft merge requests, replies and briefs. You keep control of merging and every other critical action.
- **Local first.** Everything runs on your Mac with your existing tools and credentials.

A local app shows what it did, in two views you can switch between at any time: a **Dashboard** for loops, runs and settings, and an **Office**, a 3D office with a chat, where you can talk to your agents.

## Goal and mission

**Mission.** Give engineers their attention back. Routine triage, status checks and first drafts should not cost a person's best hours.

**Goal.** Do the repetitive work end to end, safely and visibly, and hand the decision to a human at the point where judgment is needed:

- **Finish what it can.** Fix an issue, draft a reply, write the brief, and stop at an open merge request, a draft or a comment.
- **Ask when unsure.** Escalate unclear work with a question or a `loop:needs-human` label instead of guessing.
- **Stay inspectable.** Every run leaves a history, a log and a dashboard entry, so you can see what happened and why.
- **Stay yours.** It runs on your Mac, with your tools and your Keychain. Nothing is hosted for you.

## The problems it solves

| The problem | What LoopX does | Loop |
| --- | --- | --- |
| Issues assigned to you sit untouched | Fixes the ones it can (opens a merge request), answers questions, and asks you about anything unclear | **GitLab issues** |
| Your inbox buries the urgent mail | Labels new mail and drafts replies to urgent messages. It never sends | **Inbox triage** |
| Staying current on topics takes hours | Researches your topics and sends a daily briefing | **Topic monitor** |
| You start the day not knowing where things stand | One morning brief: todos, issues, reviews waiting on you, today's meetings | **Daily digest** |
| Review requests and failed pipelines pile up | Pre-reviews merge requests as drafts and diagnoses failed CI | **MR review**, **Pipeline doctor** |
| Meetings catch you unprepared | A prep brief before each meeting: agenda, related work, recent mail | **Meeting prep** |
| Work goes quiet and gets forgotten | A weekly list of stale issues and merge requests | **Stale work sweeper** |
| Release notes are tedious | Writes them from the merge requests since the last tag | **Release notes** |
| Feeds and social timelines are noisy | Ranks new items against your interests | **RSS watch**, **LinkedIn autopilot** |

Only the GitLab issues and topic monitor loops are on by default. Turn on the rest from the dashboard under **Loops**. Each loop has its own spec in [`docs/tasks/`](docs/tasks/).

## Safe to leave running

- **It never merges.** It stops at an open merge request, and merging is always your call.
- **It never sends email**, and it never deletes or moves any.
- **It only touches projects you list.** Each change is made on its own `loop/issue-<iid>` branch in an isolated worktree, and a merge request opens only if your project's own lint and tests pass.
- **Your secrets stay in the macOS Keychain**, never in plain files.

The full list is in [Safety boundaries](docs/reference.md#safety-boundaries).

## Get started

You need a Mac, `git` 2.42 or newer, a GitLab account with a personal access token, and one AI CLI: Claude Code (`claude`), Codex (`codex`), Gemini (`gemini`), Kimi (`kimi`) or Cursor (`agent`/`cursor-agent`). Claude is the default; the others are chosen in Settings → AI CLI. A Slack webhook is optional, for notifications.

**1. Install**

```bash
curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-x/main/bin/scripts/install-loopx.sh | bash
```

Prefer an app? [**Download the latest `LoopX-<version>.dmg`**](https://github.com/encoreshao/loop-x/releases/latest), open it and drag LoopX onto Applications. Or run `brew install encoreshao/tap/loopx`.

**2. Check your setup.** The Mac app offers to install the `loopx` command on first launch (or use Settings → Command line). The script installer instead puts the binary at `~/.loopx/bin/loopx` and does not change your `PATH`, so add it first (or call it by full path):

```bash
echo 'export PATH="$HOME/.loopx/bin:$PATH"' >> ~/.zshrc && source ~/.zshrc
loopx doctor
```

**3. Tell it what to watch.** Open the dashboard and go to **Loops → GitLab Issues → Projects**. Add your GitLab token and the projects to track.

**4. Try one run** before trusting it with a schedule:

```bash
loopx run gitlab-loop
```

**5. Let it run.** In **Settings → Daemons**, load the scheduler. From then on it runs each loop on its own schedule.

To update later, re-run the installer with `--upgrade`. To remove it, run `bin/scripts/uninstall-loopx.sh --keep-config`.

## Best practices

1. **Start small.** Keep only the GitLab issues and topic monitor loops on at first, and list one or two projects you know well.
2. **Try one run before scheduling.** Run `loopx run gitlab-loop` by hand and read the result before loading the scheduler.
3. **Check the checks the agent picks.** The agent works out each project's lint and test commands and logs them under each issue in the Live view. A merge request only opens when the loop's own re-run of them passes, so glance at them for a new project; saved `lint_cmd`/`test_cmd` values act as hints it starts from.
4. **Review every merge request.** LoopX stops at an open merge request. Read the diff as you would a teammate's.
5. **Write clear issues.** Include the expected behaviour and how to reproduce it. A precise issue gets a fix, a vague one gets a question.
6. **Keep the gate verification mode** (the default) so the loop re-runs the checks itself and escalates when they fail.
7. **Keep secrets in the Keychain.** Add tokens through the dashboard, never in config files or issue text.
8. **Run `loopx doctor`** after installing and after any upgrade.
9. **Enable more loops one at a time**, and watch **Runs** on the dashboard before adding the next.
10. **Evaluate prompt changes.** If you edit agent instructions, run `loopx loop eval --golden` before and after and compare.

## Learn more

| I want to… | Read |
| --- | --- |
| Look up config files, scheduling, the dashboard, connectors or CLI commands | [`docs/reference.md`](docs/reference.md) |
| Understand how a loop decides what to do | [`docs/tasks/`](docs/tasks/) and [`TASK.md`](TASK.md) |
| Understand the architecture | [`docs/architecture.md`](docs/architecture.md) |
| Build from source or contribute | [`docs/building.md`](docs/building.md) and [`CLAUDE.md`](CLAUDE.md) |
| Cut a release | [`docs/RELEASING.md`](docs/RELEASING.md) |
| Fix a crash-looping background agent | [`docs/troubleshooting/crash-looping-launchd-agent.md`](docs/troubleshooting/crash-looping-launchd-agent.md) |

## License

[MIT](LICENSE). Created by Encore Shao ([encore.shao@gmail.com](mailto:encore.shao@gmail.com)).
