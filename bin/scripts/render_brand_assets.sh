#!/usr/bin/env bash
set -euo pipefail

# Renders every size of the LoopX brand from the image masters in assets/brand/:
#
#   bin/scripts/render_brand_assets.sh
#
# Masters (cut from the brand mockup, assets/brand/mockup.png):
#   app-icon.png          1024px macOS app icon (rounded tile on the standard 824px grid, transparent outside)
#   name.png              the "LoopX" name with white lettering, for dark backgrounds
#   name-light.png        the same name with ink lettering, for light backgrounds
#   wordmark.png, lockup.png                   name + tagline, and icon + name + tagline, for dark backgrounds
#   wordmark-on-dark.png, lockup-on-dark.png   the same with the mockup's own navy background, for anywhere
#
# Outputs:
#   assets/app-icon.png                         the 1024px icon (README, release pages)
#   assets/brand/AppIcon.iconset                every macOS size, 16-1024px (build_loopx_app.sh -> AppIcon.icns)
#   assets/favicon.ico, crates/server/assets/favicon.ico   16-64px, the tile without the icon-grid padding
#   crates/server/assets/brand/{mark,name,name-light}.png  what the dashboard serves under /brand/
#
# Needs ImageMagick (`brew install imagemagick`). Rerun it after replacing a master and commit the
# outputs; the golden favicon snapshot changes with the favicon (copy crates/server/assets/favicon.ico
# to tests/golden_ui/expected/en/favicon.bin).

LOOP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ASSETS="$LOOP_DIR/assets"
BRAND="$ASSETS/brand"
SERVED="$LOOP_DIR/crates/server/assets/brand"
command -v magick >/dev/null || { echo "error: ImageMagick is required (brew install imagemagick)" >&2; exit 1; }

ICON="$BRAND/app-icon.png"
resize() { magick "$1" -filter Lanczos -resize "${2}x${2}" -strip "$3"; }

cp "$ICON" "$ASSETS/app-icon.png"

ICONSET="$BRAND/AppIcon.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  resize "$ICON" "$size" "$ICONSET/icon_${size}x${size}.png"
  resize "$ICON" $((size * 2)) "$ICONSET/icon_${size}x${size}@2x.png"
done

# The tile alone (the icon grid puts it at 100..924 of 1024): the favicon and the dashboard mark,
# where every pixel counts and the macOS padding would only shrink it.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
magick "$ICON" -crop 824x824+100+100 +repage "$TMP/tile.png"
ICO_PNGS=()
for size in 16 24 32 48 64; do
  resize "$TMP/tile.png" "$size" "$TMP/fav-$size.png"
  ICO_PNGS+=("$TMP/fav-$size.png")
done
magick "${ICO_PNGS[@]}" "$ASSETS/favicon.ico"
cp "$ASSETS/favicon.ico" "$LOOP_DIR/crates/server/assets/favicon.ico"

mkdir -p "$SERVED"
resize "$TMP/tile.png" 256 "$SERVED/mark.png"
for name in name name-light; do
  magick "$BRAND/$name.png" -filter Lanczos -resize x120 -strip "$SERVED/$name.png"
done

echo "==> rendered app-icon.png, AppIcon.iconset ($(ls "$ICONSET" | wc -l | tr -d ' ') sizes), favicon.ico and the dashboard's /brand images"
