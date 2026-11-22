#!/bin/sh
# Renders icon.svg into Maverick.icns. Needs rsvg-convert (brew install librsvg).
set -eu
cd "$(dirname "$0")"
set_dir=$(mktemp -d)/Maverick.iconset
mkdir -p "$set_dir"
for size in 16 32 128 256 512; do
	rsvg-convert -w "$size" -h "$size" icon.svg -o "$set_dir/icon_${size}x${size}.png"
	rsvg-convert -w $((size * 2)) -h $((size * 2)) icon.svg -o "$set_dir/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$set_dir" -o Maverick.icns
rsvg-convert -w 512 -h 512 icon.svg -o icon.png
