#!/usr/bin/env bash
set -euo pipefail

# Renders every size of the LoopX brand from the image masters in assets/brand/:
#
#   bin/scripts/render_brand_assets.sh
#
# Masters (unmixed from the brand mockup onto a fully transparent
# background: the glows are translucent colour, with no dark tile, gray rim or halo):
#   mark.png              the infinity-X mark alone, ~1100px wide
#   name.png              the "LoopX" name with white lettering, for dark backgrounds
#   name-light.png        the same name with ink lettering, for light backgrounds
#   wordmark.png, lockup.png   name + tagline, and mark + name + tagline, for dark backgrounds
#
# Outputs:
#   assets/app-icon.png                         the 1024px icon: the mark on the 824px macOS grid (README, release pages)
#   assets/brand/AppIcon.iconset                every macOS size, 16-1024px (build_loopx_app.sh -> AppIcon.icns)
#   assets/favicon.ico, crates/server/assets/favicon.ico   16-64px, the mark without the icon-grid padding
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

# Every resize stays transparent: -background none for the padding, and ImageMagick weights
# the filter by alpha, so no edge picks up the colour of a transparent neighbour.
# At 64px and below Lanczos turns the fine glow lines into speckle, so small sizes average
# (Triangle) and thicken the alpha so the mark still reads as a shape.
resize() {
  if [ "$2" -le 64 ]; then
    magick "$1" -background none -filter Triangle -resize "${2}x${2}" -gravity center -extent "${2}x${2}" \
      -channel A -fx '1-(1-u)^1.6' +channel -strip "PNG32:$3"
  else
    magick "$1" -background none -filter Lanczos -resize "${2}x${2}" -gravity center -extent "${2}x${2}" -strip "PNG32:$3"
  fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
# The mark squared up (centred, transparent padding): the favicon and the dashboard mark,
# where every pixel counts and the macOS padding would only shrink it.
side=$(magick identify -format '%[fx:max(w,h)]' "$BRAND/mark.png")
magick "$BRAND/mark.png" -background none -gravity center -extent "${side}x${side}" "$TMP/tile.png"

ICON="$ASSETS/app-icon.png"
magick "$TMP/tile.png" -background none -filter Lanczos -resize 824x824 -gravity center -extent 1024x1024 -strip "PNG32:$ICON"

ICONSET="$BRAND/AppIcon.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  resize "$ICON" "$size" "$ICONSET/icon_${size}x${size}.png"
  resize "$ICON" $((size * 2)) "$ICONSET/icon_${size}x${size}@2x.png"
done

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
  magick "$BRAND/$name.png" -filter Lanczos -resize x160 -strip "PNG32:$SERVED/$name.png"
done

echo "==> rendered app-icon.png, AppIcon.iconset ($(ls "$ICONSET" | wc -l | tr -d ' ') sizes), favicon.ico and the dashboard's /brand images"
