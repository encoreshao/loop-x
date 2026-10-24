# A `com.hermes.loop-engineering*` launchd agent keeps writing to its log

## Symptom

`~/.loopx/outputs/history/*.log` (`dashboard.err.log`,
`launchd.err.log`) keeps growing — new lines keep appearing even though you
haven't touched the machine.

## Diagnosis

```bash
launchctl list | grep hermes
```

```
-  2  com.hermes.loop-engineering-dashboard
-  78 com.hermes.loop-engineering
```

The second column is the job's last exit code. `78` is `EX_CONFIG`
(misconfiguration); a non-zero code here means the job failed on its last
run. There are only two `com.hermes.loop-engineering*` daemons now — the
always-on dashboard, and the unified scheduler
(`loopx scheduler`, see
`docs/superpowers/specs/2026-09-14-unified-loop-scheduler-design.md`) that
polls `~/.loopx/loops.json` and runs whichever registered loop
(the GitLab issue loop, the topic monitor loop) is due. There is no
separate per-loop daemon anymore. `com.hermes.loop-engineering-dashboard`
is the one to watch closely: its plist sets `KeepAlive`, so if it exits at
all — crash or otherwise — launchd immediately relaunches it, and a broken
dashboard crash-loops forever, appending a fresh error line to
`dashboard.err.log` on every relaunch. `com.hermes.loop-engineering` is a
`StartInterval` poll loop instead (no `KeepAlive`), so a failure there
only writes once per poll (every 15 minutes by default), not continuously.

To confirm which one is actively looping and see the real error, tail its
stderr log and watch it grow, or inspect the job directly:

```bash
tail -f ~/.loopx/outputs/history/dashboard.err.log

launchctl print gui/$(id -u)/com.hermes.loop-engineering-dashboard | grep -iE "state|last exit|program|stdout|stderr"
```

## Common root cause: the plist points at a program that isn't there

Both agents' plists (rendered by `bin/scripts/install-loopx.sh` from
`launchd/loopx/*.plist.template`) point `ProgramArguments`/`StandardOutPath`/
`StandardErrorPath` at the installed `loopx` binary and log paths under
`~/.loopx`. If that binary was removed or moved, or the plist is an old
pre-Rust one that still runs Python code that no longer exists in this repo,
every one of these jobs fails immediately. An old plist looks like this in
the error log:

```
python3: can't open file '/Users/encore/.loop-engineering/bin/web/dashboard_server.py': [Errno 2] No such file or directory
```

Check what the plist actually runs and whether it is there:

```bash
grep -A4 ProgramArguments ~/Library/LaunchAgents/com.hermes.loop-engineering-dashboard.plist
ls -l ~/.loopx/bin/loopx
```

## Fix

**Option A — re-run the installer (recommended):** `--upgrade` keeps your
config, replaces the `loopx` binary, re-renders a plist that still points at
the Python code (same port) and reloads the agents that are loaded:

```bash
curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash -s -- --upgrade
```

If nothing is installed yet, run it without `--upgrade` (see `README.md`'s
install section).

**Option B — stop the crash-loop without installing yet.** `kill`ing a PID
does nothing useful here — `KeepAlive` respawns it instantly. Unload the
job from launchd instead:

```bash
launchctl bootout gui/$(id -u)/com.hermes.loop-engineering-dashboard
launchctl bootout gui/$(id -u)/com.hermes.loop-engineering

# Verify:
launchctl list | grep hermes   # should print nothing
```

(`bootout` is the modern replacement for `launchctl unload` — use one or
the other, don't mix them on the same job.)

Reload later, once the install is fixed, with:

```bash
launchctl load -w ~/Library/LaunchAgents/com.hermes.loop-engineering-dashboard.plist
launchctl load -w ~/Library/LaunchAgents/com.hermes.loop-engineering.plist
```
