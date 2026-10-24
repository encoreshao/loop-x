#!/usr/bin/env bash
set -euo pipefail

# Uninstalls loopx, whichever way it was installed: the Homebrew / web
# installer (install-loopx.sh, brew install loopx) or the LoopX Mac app.
# Reverses what those set up on this machine, and is safe to re-run:
#
#   - LaunchAgents: every com.hermes.loop-engineering*.plist (the scheduler,
#     the dashboard, the old topic-monitor) and homebrew.mxcl.loopx.plist
#     (`brew services`) in --launch-agents-dir is `launchctl bootout`-ed and
#     removed.
#   - loopx.lock: a live `loopx` backend recorded in <project-dir>/loopx.lock
#     is stopped (SIGTERM, only if the pid really is a process named loopx),
#     then the lock is removed.
#   - Keychain: every generic-password service named loop-engineering.* is
#     deleted (the Python mailbox/connector services and loopx's .config /
#     .oauth). A `.sandbox-<hash>` service is deleted only when
#     LOOP_ENGINEERING_HOME is set (a sandboxed run), and then only the one
#     for that home; without it, sandbox services are never touched.
#     --keep-keychain leaves them all.
#   - The project directory (default ~/.loopx: the loopx binary,
#     config, run history and, for a legacy install.sh install, the clone):
#     removed, or with --keep-config everything is removed EXCEPT
#     projects.json, topics.json, loops.json and ai_cli.json.
#   - --remove-app also deletes LoopX.app from /Applications and
#     ~/Applications (--apps-dir to point elsewhere). The app's own login
#     item / bundled agent is registered through SMAppService and goes away
#     with the app; remove it under System Settings > General > Login Items
#     if it lingers.
#
# Left alone because other tools share them: ~/.gitlab/config.json,
# ~/.slack/config.json, ~/.encore-skills, Homebrew itself (finish a Homebrew
# install with `brew uninstall loopx`; this script prints the reminder).
#
# Safety rails, same spirit as uninstall.sh: it refuses to run at all when
# this script lives in a different directory than the default project dir
# and --project-dir wasn't passed explicitly. A scratch --launch-agents-dir
# never reaches the real launchd, and a scratch --project-dir never reaches
# the real Keychain, unless you inject your own commands: LOOPX_LAUNCHCTL_CMD
# and LOOPX_SECURITY_CMD (tests point these at fakes; pass --real-launchd /
# --real-keychain to force the real ones for a non-default location).

if [ -t 1 ]; then
  C_BLUE=$'\033[1;34m'; C_GREEN=$'\033[1;32m'; C_YELLOW=$'\033[1;33m'; C_RED=$'\033[1;31m'; C_RESET=$'\033[0m'
else
  C_BLUE=""; C_GREEN=""; C_YELLOW=""; C_RED=""; C_RESET=""
fi

SELF_PATH="${BASH_SOURCE[0]:-}"
if [ -n "$SELF_PATH" ]; then
  LOOP_DIR="$(cd "$(dirname "$SELF_PATH")/../.." && pwd)"
else
  LOOP_DIR=""
fi

DEFAULT_PROJECT_DIR="$HOME/.loopx"
LEGACY_PROJECT_DIR="$HOME/.loop-engineering"
DEFAULT_LAUNCH_AGENTS_DIR="$HOME/Library/LaunchAgents"
PROJECT_DIR="$DEFAULT_PROJECT_DIR"
PROJECT_DIR_GIVEN=0
LAUNCH_AGENTS_DIR="$DEFAULT_LAUNCH_AGENTS_DIR"
KEEP_CONFIG=0
KEEP_KEYCHAIN=0
REMOVE_APP=0
REAL_LAUNCHD=0
REAL_KEYCHAIN=0
APPS_DIRS=()

USAGE="Usage: uninstall-loopx.sh [--project-dir PATH] [--launch-agents-dir PATH] [--keep-config] [--keep-keychain] [--remove-app] [--apps-dir PATH] [--real-launchd] [--real-keychain]"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --project-dir) PROJECT_DIR="$2"; PROJECT_DIR_GIVEN=1; shift 2 ;;
    --launch-agents-dir) LAUNCH_AGENTS_DIR="$2"; shift 2 ;;
    --keep-config) KEEP_CONFIG=1; shift ;;
    --keep-keychain) KEEP_KEYCHAIN=1; shift ;;
    --remove-app) REMOVE_APP=1; shift ;;
    --apps-dir) APPS_DIRS+=("$2"); shift 2 ;;
    --real-launchd) REAL_LAUNCHD=1; shift ;;
    --real-keychain) REAL_KEYCHAIN=1; shift ;;
    *) echo "${C_RED}$USAGE${C_RESET}" >&2; exit 1 ;;
  esac
done

LAUNCHCTL_CMD="${LOOPX_LAUNCHCTL_CMD:-}"
SECURITY_CMD="${LOOPX_SECURITY_CMD:-}"

# Refuse before touching anything when a different checkout's copy of this
# script is run bare against the default (possibly real) install.
# A pre-rename install still lives at ~/.loop-engineering (loopx moves it on first run).
if [ "$PROJECT_DIR_GIVEN" -eq 0 ] && [ ! -e "$PROJECT_DIR" ] && [ -d "$LEGACY_PROJECT_DIR" ] && [ ! -L "$LEGACY_PROJECT_DIR" ]; then
  PROJECT_DIR="$LEGACY_PROJECT_DIR"
  DEFAULT_PROJECT_DIR="$LEGACY_PROJECT_DIR"
fi

if [ "$PROJECT_DIR_GIVEN" -eq 0 ] && [ -n "$LOOP_DIR" ] && [ "$LOOP_DIR" != "$PROJECT_DIR" ]; then
  echo "${C_RED}Refusing to uninstall $PROJECT_DIR: this uninstall-loopx.sh is running from $LOOP_DIR, a different directory.${C_RESET}" >&2
  echo "If you're really uninstalling that real install, pass --project-dir '$PROJECT_DIR' to confirm. If you're developing on a separate clone, pass --project-dir (and --launch-agents-dir) pointing at a scratch directory instead." >&2
  exit 1
fi

# --------------------------------------------------------------- launchctl
if [ -z "$LAUNCHCTL_CMD" ]; then
  if [ "$LAUNCH_AGENTS_DIR" = "$DEFAULT_LAUNCH_AGENTS_DIR" ] || [ "$REAL_LAUNCHD" -eq 1 ]; then
    LAUNCHCTL_CMD="launchctl"
  fi
fi
launchctl_run() {
  if [ -n "$LAUNCHCTL_CMD" ]; then
    # shellcheck disable=SC2086
    $LAUNCHCTL_CMD "$@" >/dev/null 2>&1 || true
  fi
}

echo "${C_BLUE}==> Removing launchd agents${C_RESET}"
if [ -z "$LAUNCHCTL_CMD" ]; then
  echo "    $LAUNCH_AGENTS_DIR is not the real LaunchAgents folder: not calling launchctl (--real-launchd to force)"
fi
removed_agents=0
shopt -s nullglob
for dest in "$LAUNCH_AGENTS_DIR"/com.hermes.loop-engineering*.plist "$LAUNCH_AGENTS_DIR"/homebrew.mxcl.loopx.plist; do
  [ -e "$dest" ] || continue
  name="$(basename "$dest")"
  label="${name%.plist}"
  launchctl_run bootout "gui/$(id -u)/$label"
  rm -f "$dest"
  echo "${C_GREEN}    Removed $name${C_RESET}"
  removed_agents=$((removed_agents + 1))
done
shopt -u nullglob
if [ "$removed_agents" -eq 0 ]; then
  echo "    No loopx launchd agents installed in $LAUNCH_AGENTS_DIR"
fi

# ---------------------------------------------------------------- lock file
echo "${C_BLUE}==> Stopping the backend and removing loopx.lock${C_RESET}"
lock="$PROJECT_DIR/loopx.lock"
if [ -f "$lock" ]; then
  pid="$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$lock" | head -1)"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    comm="$(ps -p "$pid" -o comm= 2>/dev/null || true)"
    case "$(basename "${comm:-}")" in
      loopx)
        kill -TERM "$pid" 2>/dev/null || true
        echo "${C_GREEN}    Stopped loopx backend (pid $pid)${C_RESET}"
        ;;
      *) echo "    pid $pid is not a loopx process ($comm); not signalling it" ;;
    esac
  fi
  rm -f "$lock"
  echo "${C_GREEN}    Removed $lock${C_RESET}"
else
  echo "    No $lock"
fi

# ----------------------------------------------------------------- Keychain
echo "${C_BLUE}==> Keychain entries (loop-engineering.*)${C_RESET}"
if [ "$KEEP_KEYCHAIN" -eq 1 ]; then
  echo "    Left in place (--keep-keychain)"
else
  if [ -z "$SECURITY_CMD" ]; then
    if [ "$PROJECT_DIR" = "$DEFAULT_PROJECT_DIR" ] || [ "$REAL_KEYCHAIN" -eq 1 ]; then
      SECURITY_CMD="/usr/bin/security"
    fi
  fi
  if [ -z "$SECURITY_CMD" ]; then
    echo "    $PROJECT_DIR is not the default install location: not touching the real Keychain (--real-keychain to force)"
  else
    sandbox_suffix=""
    if [ -n "${LOOP_ENGINEERING_HOME:-}" ]; then
      real_home="$(cd "$LOOP_ENGINEERING_HOME" 2>/dev/null && pwd -P || printf '%s' "$LOOP_ENGINEERING_HOME")"
      if command -v shasum >/dev/null 2>&1; then
        digest="$(printf '%s' "$real_home" | shasum -a 256 | cut -c1-10)"
      else
        digest="$(printf '%s' "$real_home" | sha256sum | cut -c1-10)"
      fi
      sandbox_suffix=".sandbox-$digest"
    fi
    # shellcheck disable=SC2086
    services="$($SECURITY_CMD dump-keychain 2>/dev/null | sed -n 's/.*"svce"<blob>="\(loop-engineering\.[^"]*\)".*/\1/p' | sort -u || true)"
    removed_services=0
    while IFS= read -r service; do
      [ -n "$service" ] || continue
      case "$service" in
        *.sandbox-*)
          # Only the service of this exact sandbox, and only when sandboxed.
          if [ -z "$sandbox_suffix" ] || [ "${service%"$sandbox_suffix"}" = "$service" ]; then
            continue
          fi
          ;;
        *)
          # A sandboxed run never deletes the real services.
          if [ -n "$sandbox_suffix" ]; then
            continue
          fi
          ;;
      esac
      # One delete per item: a service may hold several accounts.
      n=0
      # shellcheck disable=SC2086
      while [ "$n" -lt 500 ] && $SECURITY_CMD delete-generic-password -s "$service" >/dev/null 2>&1; do
        n=$((n + 1))
      done
      echo "${C_GREEN}    Removed Keychain service $service ($n item(s))${C_RESET}"
      removed_services=$((removed_services + 1))
    done <<EOF_SERVICES
$services
EOF_SERVICES
    if [ "$removed_services" -eq 0 ]; then
      echo "    No loop-engineering.* Keychain entries"
    fi
  fi
fi

# ---------------------------------------------------------------- Mac app
if [ "$REMOVE_APP" -eq 1 ]; then
  echo "${C_BLUE}==> Removing LoopX.app${C_RESET}"
  if [ "${#APPS_DIRS[@]}" -eq 0 ]; then
    APPS_DIRS=("/Applications" "$HOME/Applications")
  fi
  removed_apps=0
  for apps in "${APPS_DIRS[@]}"; do
    for app in "LoopX.app" "Loop X.app"; do
      if [ -d "$apps/$app" ]; then
        rm -rf "${apps:?}/$app"
        echo "${C_GREEN}    Removed $apps/$app${C_RESET}"
        removed_apps=$((removed_apps + 1))
      fi
    done
  done
  if [ "$removed_apps" -eq 0 ]; then
    echo "    LoopX.app not found"
  fi
fi

# ------------------------------------------------------------ project dir
echo "${C_BLUE}==> Project directory ($PROJECT_DIR)${C_RESET}"
if [ ! -d "$PROJECT_DIR" ]; then
  echo "    $PROJECT_DIR does not exist, nothing to remove"
elif [ "$KEEP_CONFIG" -eq 1 ]; then
  for entry in "$PROJECT_DIR"/* "$PROJECT_DIR"/.[!.]* "$PROJECT_DIR"/..?*; do
    [ -e "$entry" ] || [ -L "$entry" ] || continue
    case "$(basename "$entry")" in
      projects.json|topics.json|loops.json|ai_cli.json) continue ;;
    esac
    rm -rf "$entry"
  done
  echo "${C_GREEN}    Removed everything in $PROJECT_DIR except projects.json, topics.json, loops.json and ai_cli.json (--keep-config)${C_RESET}"
else
  rm -rf "$PROJECT_DIR"
  echo "${C_GREEN}    Removed $PROJECT_DIR${C_RESET}"
  # The compatibility symlink the move left behind now dangles.
  if [ "$PROJECT_DIR_GIVEN" -eq 0 ] && [ -L "$LEGACY_PROJECT_DIR" ] && [ ! -e "$LEGACY_PROJECT_DIR" ]; then
    rm -f "$LEGACY_PROJECT_DIR"
  fi
fi

cat <<EOF

${C_GREEN}Uninstall done.${C_RESET}

Left untouched (shared with other tools, not loopx's alone):
  - ~/.gitlab/config.json, ~/.slack/config.json
  - ~/.encore-skills (the gitlab-config skill)
If you installed with Homebrew, finish with: brew uninstall loopx
If you used the Mac app, drag LoopX.app to the Trash (or re-run with --remove-app).
EOF
