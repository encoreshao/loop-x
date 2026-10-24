#!/usr/bin/env bash
set -euo pipefail

# The online installer for the Rust `loopx` binary - no clone, no Python:
#
#   curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash
#
# Already installed and just want the latest release? Same command plus
# --upgrade:
#
#   curl -fsSL https://raw.githubusercontent.com/encoreshao/loop-engineering/main/bin/scripts/install-loopx.sh | bash -s -- --upgrade
#
# This is the loopx counterpart of install.sh (which clones the repo and runs
# the Python code; it stays as the legacy installer). Instead of cloning, it:
#
#   1. downloads, from a GitHub Release, the prebuilt `loopx` for this Mac's
#      architecture (loopx-darwin-<arch>.tar.gz, else the universal
#      loopx-darwin-universal.tar.gz) and loopx-support.tar.gz (the config
#      and launchd templates), and verifies both against the release's
#      SHA256SUMS before anything is installed;
#   2. installs the binary as <dir>/bin/loopx (replaced atomically);
#   3. scaffolds the per-machine config from config/*.template ({{HOME}}
#      substituted) only where a file does not exist yet - projects.json,
#      topics.json, loops.json, ai_cli.json, ... - and seeds
#      loop_scheduler_state.json with today's date for every registered loop,
#      so the scheduler's first poll does not fire an immediate run;
#   4. renders launchd/loopx/*.plist.template into <dir>/launchd/*.plist the
#      first time only (a rendered plist is the user's own source of truth:
#      the Daemons page edits its schedule) and, unless --skip-service,
#      refreshes the agents that are already loaded.
#
# --upgrade behaves like install.sh --upgrade: it fails fast when nothing is
# installed at --dir (a legacy install.sh clone counts as installed, so it
# can be migrated), keeps every config file, replaces only the program,
# backfills legacy runs into the ledger (best effort), migrates a rendered
# plist that still points at the Python code (re-rendered, same port, and
# reloaded if loaded), removes the orphaned topic-monitor agent, and
# refreshes the agents that are already loaded: the dashboard gets
# `launchctl kickstart -k` (an always-on server; `load` on a loaded agent is
# a silent no-op), the scheduler only an `unload` + `load -w` re-registration
# - never kickstarted, because that would trigger an immediate poll and
# possibly a real run against live GitLab/Slack. Neither is started when it
# is not loaded yet, unless --load-agents is given (the one deliberate
# difference from install.sh, which starts the dashboard by default: loading
# a launchd agent is something to ask for).
#
# Sandboxed runs: pass --dir, --launch-agents-dir and --skip-service (or put
# a fake launchctl first on PATH), and point --release-base at a local
# file:// directory. Never run this bare while developing.

if [ -t 1 ]; then
  C_BLUE=$'\033[1;34m'; C_GREEN=$'\033[1;32m'; C_YELLOW=$'\033[1;33m'; C_RED=$'\033[1;31m'; C_RESET=$'\033[0m'
else
  C_BLUE=""; C_GREEN=""; C_YELLOW=""; C_RED=""; C_RESET=""
fi

DIR="$HOME/.loopx"
REPO_SLUG="encoreshao/loop-engineering"
VERSION=""
RELEASE_BASE="${LOOPX_RELEASE_BASE:-}"
ARCH="${LOOPX_ARCH:-}"
LAUNCH_AGENTS_DIR="$HOME/Library/LaunchAgents"
UPGRADE=0
SKIP_SERVICE=0
LOAD_AGENTS=0
INSTALL_SKILLS=0
PORT=""
CONFIG_PATH=""
TOPICS_CONFIG_PATH=""
INBOXES_CONFIG_PATH=""
CONNECTORS_CONFIG_PATH=""
AI_CLI_CONFIG_PATH=""
LOOPS_CONFIG_PATH=""
STATE_PATH=""

USAGE="Usage: install-loopx.sh [--dir PATH] [--upgrade] [--version X.Y.Z] [--release-base URL] [--arch arm64|x86_64] [--launch-agents-dir PATH] [--skip-service] [--load-agents] [--port PORT] [--install-skills] [--config-path PATH] [--topics-config-path PATH] [--inboxes-config-path PATH] [--connectors-config-path PATH] [--ai-cli-config-path PATH] [--loops-config-path PATH] [--state-path PATH]"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dir) DIR="$2"; shift 2 ;;
    --upgrade) UPGRADE=1; shift ;;
    --version) VERSION="$2"; shift 2 ;;
    --release-base) RELEASE_BASE="$2"; shift 2 ;;
    --arch) ARCH="$2"; shift 2 ;;
    --launch-agents-dir) LAUNCH_AGENTS_DIR="$2"; shift 2 ;;
    --skip-service|--skip-launchd-daemons) SKIP_SERVICE=1; shift ;;
    --load-agents) LOAD_AGENTS=1; shift ;;
    --port) PORT="$2"; shift 2 ;;
    --install-skills) INSTALL_SKILLS=1; shift ;;
    # Accepted so install.sh's command lines keep working; loopx needs neither.
    --skip-skills-install|--skip-nginx) shift ;;
    --config-path) CONFIG_PATH="$2"; shift 2 ;;
    --topics-config-path) TOPICS_CONFIG_PATH="$2"; shift 2 ;;
    --inboxes-config-path) INBOXES_CONFIG_PATH="$2"; shift 2 ;;
    --connectors-config-path) CONNECTORS_CONFIG_PATH="$2"; shift 2 ;;
    --ai-cli-config-path) AI_CLI_CONFIG_PATH="$2"; shift 2 ;;
    --loops-config-path) LOOPS_CONFIG_PATH="$2"; shift 2 ;;
    --state-path) STATE_PATH="$2"; shift 2 ;;
    *)
      echo "${C_RED}$USAGE${C_RESET}" >&2
      exit 1
      ;;
  esac
done

# The directory used to be ~/.loop-engineering. Move an existing one into place and
# leave a symlink at the old name, so paths already written into configs and launchd
# plists keep working (loopx does the same on its first run).
LEGACY_DIR="$HOME/.loop-engineering"
if [ "$DIR" = "$HOME/.loopx" ] && [ -d "$LEGACY_DIR" ] && [ ! -L "$LEGACY_DIR" ] && [ ! -e "$DIR" ]; then
  echo "${C_BLUE}==> Moving $LEGACY_DIR to $DIR (a symlink stays at the old path)${C_RESET}"
  mv "$LEGACY_DIR" "$DIR" && ln -s "$DIR" "$LEGACY_DIR"
fi

: "${CONFIG_PATH:=$DIR/projects.json}"
: "${TOPICS_CONFIG_PATH:=$DIR/topics.json}"
: "${INBOXES_CONFIG_PATH:=$DIR/inboxes.json}"
: "${CONNECTORS_CONFIG_PATH:=$DIR/connectors.json}"
: "${AI_CLI_CONFIG_PATH:=$DIR/ai_cli.json}"
: "${LOOPS_CONFIG_PATH:=$DIR/loops.json}"
: "${STATE_PATH:=$DIR/loop_scheduler_state.json}"

die() { echo "${C_RED}$*${C_RESET}" >&2; exit 1; }
info() { echo "${C_BLUE}==> $*${C_RESET}"; }
warn() { echo "${C_YELLOW}    Warning: $*${C_RESET}" >&2; }

for tool in curl tar; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required but was not found on PATH."
done
if command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | awk '{print $1}'; }
else
  die "shasum (or sha256sum) is required to verify the download."
fi

# ---------------------------------------------------------------- arch / URLs
if [ -z "$ARCH" ]; then
  ARCH="$(uname -m)"
fi
case "$ARCH" in
  arm64|aarch64) ARCH="arm64" ;;
  x86_64|amd64) ARCH="x86_64" ;;
  *) die "Unsupported architecture: $ARCH (prebuilt loopx binaries exist for arm64 and x86_64)." ;;
esac

if [ -z "$RELEASE_BASE" ]; then
  if [ -n "$VERSION" ]; then
    RELEASE_BASE="https://github.com/$REPO_SLUG/releases/download/v${VERSION#v}"
  else
    RELEASE_BASE="https://github.com/$REPO_SLUG/releases/latest/download"
  fi
fi
RELEASE_BASE="${RELEASE_BASE%/}"

# ------------------------------------------------------------- install state
INSTALLED=0
if [ -x "$DIR/bin/loopx" ] || [ -d "$DIR/.git" ]; then
  INSTALLED=1
fi
if [ "$UPGRADE" -eq 1 ] && [ "$INSTALLED" -eq 0 ]; then
  die "--upgrade given but nothing is installed at $DIR yet - run install-loopx.sh without --upgrade first."
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/install-loopx.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# ------------------------------------------------------------------ download
info "Downloading loopx from $RELEASE_BASE ..."
curl -fsSL -o "$TMP/SHA256SUMS" "$RELEASE_BASE/SHA256SUMS" \
  || die "Could not download $RELEASE_BASE/SHA256SUMS - is there a release there?"

# The recorded hash for an asset name, or nothing.
expected_hash() {
  awk -v name="$1" '{ n=$2; sub(/^\*/, "", n); if (n == name) { print $1; exit } }' "$TMP/SHA256SUMS"
}

# Download <asset> and fail unless it matches SHA256SUMS.
fetch_verified() {
  local asset="$1" want got
  want="$(expected_hash "$asset")"
  [ -n "$want" ] || die "$asset is not listed in SHA256SUMS - refusing to install an unverified file."
  curl -fsSL -o "$TMP/$asset" "$RELEASE_BASE/$asset" || die "Could not download $RELEASE_BASE/$asset"
  got="$(sha256_of "$TMP/$asset")"
  if [ "$got" != "$want" ]; then
    die "SHA-256 mismatch for $asset (expected $want, got $got) - nothing was installed."
  fi
}

BIN_ASSET=""
for candidate in "loopx-darwin-$ARCH.tar.gz" "loopx-darwin-universal.tar.gz"; do
  if [ -n "$(expected_hash "$candidate")" ]; then
    BIN_ASSET="$candidate"
    break
  fi
done
[ -n "$BIN_ASSET" ] || die "SHA256SUMS lists no loopx build for $ARCH (looked for loopx-darwin-$ARCH.tar.gz and loopx-darwin-universal.tar.gz)."

fetch_verified "$BIN_ASSET"
fetch_verified "loopx-support.tar.gz"
echo "    Verified $BIN_ASSET and loopx-support.tar.gz (SHA-256)"

mkdir -p "$TMP/bin" "$TMP/support"
tar -xzf "$TMP/$BIN_ASSET" -C "$TMP/bin"
tar -xzf "$TMP/loopx-support.tar.gz" -C "$TMP/support"
[ -f "$TMP/bin/loopx" ] || die "$BIN_ASSET does not contain a loopx binary."
chmod 755 "$TMP/bin/loopx"
NEW_VERSION="$("$TMP/bin/loopx" --version 2>/dev/null)" || die "The downloaded loopx does not run on this machine."

# ------------------------------------------------------------ install binary
info "Installing $NEW_VERSION into $DIR/bin ..."
mkdir -p "$DIR/bin"
cp "$TMP/bin/loopx" "$DIR/bin/.loopx.new"
chmod 755 "$DIR/bin/.loopx.new"
mv -f "$DIR/bin/.loopx.new" "$DIR/bin/loopx"
LOOPX="$DIR/bin/loopx"

# -------------------------------------------------------------- config files
SUPPORT_CONFIG="$TMP/support/config"
scaffold() {
  # scaffold <dest> <template name> <hint>
  local dest="$1" template="$2" hint="$3"
  if [ -f "$dest" ]; then
    info "$dest already exists, leaving it alone"
    return
  fi
  [ -f "$SUPPORT_CONFIG/$template" ] || die "loopx-support.tar.gz has no config/$template"
  info "Creating $dest from the template"
  mkdir -p "$(dirname "$dest")"
  sed "s|{{HOME}}|$(printf '%s' "$HOME" | sed -e 's/[\\|&]/\\&/g')|g" "$SUPPORT_CONFIG/$template" > "$dest"
  echo "${C_YELLOW}    $hint${C_RESET}"
}
scaffold "$CONFIG_PATH" projects.json.template "Edit it now: GitLab username and each project's path/branch/commands."
scaffold "$TOPICS_CONFIG_PATH" topics.json.template "Edit it now: which topics to monitor (only needed for the topic monitor loop)."
scaffold "$INBOXES_CONFIG_PATH" inboxes.json.template "Only needed for the Inbox Triage loop."
scaffold "$CONNECTORS_CONFIG_PATH" connectors.json.template "Connector accounts - add them from the dashboard's Connectors page; secrets go to the Keychain, never this file."
scaffold "$AI_CLI_CONFIG_PATH" ai_cli.json.template "Defaults to Claude - switch with 'loopx ai-cli set codex'."
scaffold "$LOOPS_CONFIG_PATH" loops.json.template "Registers the loops with the scheduler ('loopx scheduler')."

if [ -f "$STATE_PATH" ]; then
  info "$STATE_PATH already exists, leaving it alone"
else
  # Same reason as setup.sh: without today's date for every loop, the first
  # poll sees "never attempted" and fires an unattended run.
  info "Seeding $STATE_PATH so the first poll doesn't fire an immediate run"
  mkdir -p "$(dirname "$STATE_PATH")"
  today="$(date +%Y-%m-%d)"
  names="$(grep -o '"name"[[:space:]]*:[[:space:]]*"[^"]*"' "$LOOPS_CONFIG_PATH" | sed -e 's/.*:[[:space:]]*"\(.*\)"/\1/' || true)"
  {
    printf '{'
    first=1
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      if [ "$first" -eq 1 ]; then first=0; else printf ','; fi
      printf '\n  "%s": {\n    "last_attempted_date": "%s"\n  }' "$name" "$today"
    done <<EOF_NAMES
$names
EOF_NAMES
    if [ "$first" -eq 0 ]; then printf '\n'; fi
    printf '}\n'
  } > "$STATE_PATH"
fi

if [ "$INSTALL_SKILLS" -eq 1 ]; then
  info "Installing the gitlab-config skill (encore-skills)..."
  curl -fsSL https://raw.githubusercontent.com/encoreshao/encore-skills/main/scripts/setup.sh | bash -s -- --claude
fi

if [ "$UPGRADE" -eq 1 ]; then
  # Legacy result.json runs -> event ledger (idempotent). Best effort.
  info "Backfilling legacy runs into the ledger..."
  LOOP_ENGINEERING_HOME="$DIR" "$LOOPX" ledger backfill \
    || echo "${C_YELLOW}ledger backfill failed (non-fatal); retry with: $LOOPX ledger backfill${C_RESET}" >&2
fi

# ------------------------------------------------------------- launchd plists
info "Rendering launchd/*.plist for this machine..."
mkdir -p "$DIR/launchd" "$DIR/outputs/history"
SUPPORT_LAUNCHD="$TMP/support/launchd/loopx"

sed_escape() { printf '%s' "$1" | sed -e 's/[\\|&]/\\&/g'; }

DASHBOARD_LABEL="com.hermes.loop-engineering-dashboard"
SCHEDULER_LABEL="com.hermes.loop-engineering"
dashboard_plist="$DIR/launchd/$DASHBOARD_LABEL.plist"

# The port is whatever the already-rendered dashboard plist says (a legacy
# Python plist carries the same <string>PORT</string>), else --port, else one
# random pick from 48420-48620 - never re-picked on upgrade.
if [ -f "$dashboard_plist" ]; then
  existing_port="$(grep -o '<string>[0-9]\{4,5\}</string>' "$dashboard_plist" | head -1 | grep -o '[0-9]\{4,5\}' || true)"
  if [ -n "$existing_port" ]; then
    PORT="$existing_port"
  fi
fi
if [ -z "$PORT" ]; then
  PORT=$((48420 + RANDOM % 201))
fi

render_plist() {
  local template="$1" out="$2"
  sed -e "s|{{LOOPX}}|$(sed_escape "$LOOPX")|g" \
      -e "s|{{LOOP_DIR}}|$(sed_escape "$DIR")|g" \
      -e "s|{{HOME}}|$(sed_escape "$HOME")|g" \
      -e "s|{{PORT}}|$PORT|g" "$template" > "$out"
  # The Python dashboard is only there to serve pages loopx has not ported
  # yet; with no legacy checkout in --dir there is nothing to proxy to.
  if [ -f "$DIR/bin/web/dashboard_server.py" ]; then
    sed -e '/<string>--no-legacy<\/string>/d' "$out" > "$out.tmp" && mv "$out.tmp" "$out"
  fi
}

MIGRATED_LABELS=""
shopt -s nullglob
for template in "$SUPPORT_LAUNCHD"/*.plist.template; do
  name="$(basename "${template%.template}")"
  label="${name%.plist}"
  rendered="$DIR/launchd/$name"
  if [ -f "$rendered" ]; then
    # Keep a plist that already runs this loopx (it may carry a saved custom
    # schedule); re-render one that still runs the Python code (or another
    # install dir), like install.sh's stale-plist migration.
    if grep -qF "$LOOPX" "$rendered"; then
      continue
    fi
    echo "${C_BLUE}==> Migrating $label's plist to loopx...${C_RESET}"
    MIGRATED_LABELS="$MIGRATED_LABELS $label "
  fi
  render_plist "$template" "$rendered"
done
shopt -u nullglob

DASHBOARD_URL="http://127.0.0.1:$PORT"

# ---------------------------------------------------------- launchd services
if [ "$SKIP_SERVICE" -eq 1 ]; then
  info "Skipping launchd (--skip-service)"
else
  # The old topic-monitor daemon (replaced by the unified scheduler).
  orphan_label="com.hermes.loop-engineering-topic-monitor"
  orphan_dest="$LAUNCH_AGENTS_DIR/$orphan_label.plist"
  if [ -f "$orphan_dest" ] || [ -f "$DIR/launchd/$orphan_label.plist" ]; then
    info "Removing the old topic-monitor daemon (replaced by the unified scheduler)..."
    launchctl unload -w "$orphan_dest" >/dev/null 2>&1 || true
    rm -f "$orphan_dest" "$DIR/launchd/$orphan_label.plist"
  fi

  dest="$LAUNCH_AGENTS_DIR/$DASHBOARD_LABEL.plist"
  if [ ! -f "$dashboard_plist" ]; then
    warn "$dashboard_plist not found, skipping dashboard daemon"
  elif launchctl list "$DASHBOARD_LABEL" >/dev/null 2>&1; then
    mkdir -p "$LAUNCH_AGENTS_DIR"
    cp "$dashboard_plist" "$dest"
    case "$MIGRATED_LABELS" in
      *" $DASHBOARD_LABEL "*)
        # kickstart -k re-runs the definition launchd already has in memory;
        # a plist whose program changed needs a real reload.
        info "Reloading the dashboard (its plist now runs loopx)..."
        launchctl unload "$dest" >/dev/null 2>&1 || true
        if launchctl load -w "$dest"; then
          echo "${C_GREEN}    Dashboard reloaded - $DASHBOARD_URL${C_RESET}"
        else
          warn "launchctl load failed - re-enable it from the dashboard's Daemons page, or run 'launchctl load -w $dest'."
        fi
        ;;
      *)
        info "Restarting the dashboard to pick up the update..."
        launchctl kickstart -k "gui/$(id -u)/$DASHBOARD_LABEL"
        echo "${C_GREEN}    Dashboard restarted - $DASHBOARD_URL${C_RESET}"
        ;;
    esac
  elif [ "$LOAD_AGENTS" -eq 1 ]; then
    info "Starting the dashboard as an always-on launchd agent..."
    mkdir -p "$LAUNCH_AGENTS_DIR"
    cp "$dashboard_plist" "$dest"
    if launchctl load -w "$dest"; then
      echo "${C_GREEN}    Dashboard running - $DASHBOARD_URL${C_RESET}"
    else
      rm -f "$dest"
      warn "launchctl load failed - run 'launchctl load -w $dest' by hand."
    fi
  fi

  # Every other agent (the scheduler): refresh a loaded registration, never
  # kickstart it and never auto-start it.
  shopt -s nullglob
  for plist in "$DIR"/launchd/*.plist; do
    label="$(basename "$plist" .plist)"
    [ "$label" = "$DASHBOARD_LABEL" ] && continue
    if launchctl list "$label" >/dev/null 2>&1; then
      info "Refreshing $label's launchd registration..."
      dest="$LAUNCH_AGENTS_DIR/$(basename "$plist")"
      mkdir -p "$LAUNCH_AGENTS_DIR"
      cp "$plist" "$dest"
      launchctl unload "$dest" >/dev/null 2>&1 || true
      if launchctl load -w "$dest"; then
        echo "${C_GREEN}    $label refreshed - it'll run the updated code at its next scheduled time${C_RESET}"
      else
        rm -f "$dest"
        warn "failed to refresh $label - re-enable it later from the Daemons page."
      fi
    fi
  done
  shopt -u nullglob
fi

cat <<EOF

${C_GREEN}Install done.${C_RESET} Installed $NEW_VERSION at $LOOPX
Next: run '$LOOPX doctor' to see what is left to set up (AI CLI, GitLab/Slack
credentials, $CONFIG_PATH), then '$LOOPX serve' to open the dashboard
($DASHBOARD_URL once the dashboard agent is loaded; pass --load-agents to
start it, or run 'launchctl load -w $LAUNCH_AGENTS_DIR/$DASHBOARD_LABEL.plist').
EOF
