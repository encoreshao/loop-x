<p align="right">

**English** | [日本語](README.ja.md) | [简体中文](README.zh-CN.md) | [Français](README.fr.md)

</p>

<p align="center">
  <img src="assets/app-icon.svg" alt="Loop X logo" width="256" height="256">
</p>

# Loop X Engineering

![CI](https://github.com/encoreshao/loop-x/actions/workflows/rust.yml/badge.svg)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Releases](https://img.shields.io/badge/releases-download-brightgreen)](https://github.com/encoreshao/loop-x/releases/latest)
![Platform](https://img.shields.io/badge/platform-macOS-lightgrey)

**Get your time back from the daily grind of issues, email and status checks.**

Loop X is a teammate that runs quietly on your Mac. Every weekday it works through the routine, so you only spend attention on what needs your judgment. A local dashboard shows what it did.

## The problems it solves

| The problem | What Loop X does | Loop |
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

You need a Mac, `git` 2.42 or newer, a GitLab account with a personal access token, and the `claude` or `codex` CLI. A Slack webhook is optional, for notifications.

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

[MIT](LICENSE)
