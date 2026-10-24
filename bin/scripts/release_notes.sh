#!/usr/bin/env bash
set -euo pipefail

# Prints the CHANGELOG.md section of a release (the lines between its
# "## [X.Y.Z] - DATE" heading and the next "## [" heading) - the body of the
# GitHub Release. Fails if the section is missing or empty, so a tag cannot
# be released without its changelog entry.
#
#   release_notes.sh <version> [--changelog FILE]

VERSION=""
CHANGELOG="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/CHANGELOG.md"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --changelog) CHANGELOG="$2"; shift 2 ;;
    -*) echo "Usage: release_notes.sh <version> [--changelog FILE]" >&2; exit 1 ;;
    *) VERSION="${1#v}"; shift ;;
  esac
done
[ -n "$VERSION" ] || { echo "Usage: release_notes.sh <version> [--changelog FILE]" >&2; exit 1; }
[ -f "$CHANGELOG" ] || { echo "release_notes.sh: $CHANGELOG not found" >&2; exit 1; }

body="$(awk -v v="$VERSION" '
  /^## \[/ {
    if (on) exit
    if (index($0, "## [" v "]") == 1) { on = 1; next }
  }
  on { print }
' "$CHANGELOG")"

# Trim leading/trailing blank lines.
body="$(printf '%s\n' "$body" | sed -e '/./,$!d' | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}')"
if [ -z "$(printf '%s' "$body" | tr -d '[:space:]')" ]; then
  echo "release_notes.sh: no non-empty '## [$VERSION]' section in $CHANGELOG" >&2
  exit 1
fi
printf '%s\n' "$body"
