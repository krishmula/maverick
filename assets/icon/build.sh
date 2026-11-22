#!/bin/sh
# Renders each icon SVG to a 512 px PNG, which the app shows and sets as its Dock icon,
# and renders the default icon into Maverick.icns for the app bundle.
# Needs rsvg-convert (brew install librsvg).
set -eu
cd "$(dirname "$0")"
default=sunset
for svg in *.svg; do
	rsvg-convert -w 512 -h 512 "$svg" -o "${svg%.svg}.png"
done
set_dir=$(mktemp -d)/Maverick.iconset
mkdir -p "$set_dir"
for size in 16 32 128 256 512; do
	rsvg-convert -w "$size" -h "$size" "$default.svg" -o "$set_dir/icon_${size}x${size}.png"
	rsvg-convert -w $((size * 2)) -h $((size * 2)) "$default.svg" -o "$set_dir/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$set_dir" -o Maverick.icns
