#!/usr/bin/env bash
set -euo pipefail

# Fills loopx.rb's placeholders and prints the formula on stdout.
#
#   render_formula.sh <version> <sha256 of loopx-darwin-universal.tar.gz> <sha256 of loopx-support.tar.gz> [owner/repo]
#
# <version> is X.Y.Z (a leading v is stripped). Used by the release workflow;
# runnable locally to eyeball the result.

if [ "$#" -lt 3 ] || [ "$#" -gt 4 ]; then
  echo "Usage: render_formula.sh <version> <sha256> <support-sha256> [owner/repo]" >&2
  exit 1
fi

VERSION="${1#v}"
SHA="$2"
SUPPORT_SHA="$3"
REPO="${4:-encoreshao/loop-engineering}"

if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$'; then
  echo "render_formula.sh: '$VERSION' is not a version like 1.2.3" >&2
  exit 1
fi
for h in "$SHA" "$SUPPORT_SHA"; do
  if ! printf '%s' "$h" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "render_formula.sh: '$h' is not a SHA-256 hex digest" >&2
    exit 1
  fi
done

TEMPLATE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/loopx.rb"
sed -e "s|@VERSION@|$VERSION|g" \
    -e "s|@SHA256@|$SHA|g" \
    -e "s|@SUPPORT_SHA256@|$SUPPORT_SHA|g" \
    -e "s|@REPO@|$REPO|g" "$TEMPLATE"
