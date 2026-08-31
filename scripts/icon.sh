#!/usr/bin/env bash
set -euo pipefail

# Renders assets/icon.svg into assets/AppIcon.icns. The result is committed, so a
# release never needs a browser - run this only when the artwork changes.

root=$(cd "$(dirname "$0")/.." && pwd)
svg="$root/assets/icon.svg"
out="$root/assets/AppIcon.icns"
chrome=${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}

if [ ! -f "$svg" ]; then
	echo "$svg is missing" >&2
	exit 1
fi

if [ ! -x "$chrome" ]; then
	echo "no headless renderer at $chrome (set CHROME to one)" >&2
	exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
iconset="$work/AppIcon.iconset"
mkdir -p "$iconset"

render() {
	local size=$1 name=$2
	printf '<body style="margin:0"><img src="%s" width="%s" height="%s" style="display:block"></body>' \
		"$svg" "$size" "$size" > "$work/page.html"
	"$chrome" --headless --disable-gpu --hide-scrollbars \
		--allow-file-access-from-files --default-background-color=00000000 \
		--force-device-scale-factor=1 --window-size="$size,$size" \
		--screenshot="$iconset/$name" "file://$work/page.html" 2>/dev/null
}

# The set macOS asks for: every icon size at 1x and 2x.
render 16 icon_16x16.png
render 32 icon_16x16@2x.png
render 32 icon_32x32.png
render 64 icon_32x32@2x.png
render 128 icon_128x128.png
render 256 icon_128x128@2x.png
render 256 icon_256x256.png
render 512 icon_256x256@2x.png
render 512 icon_512x512.png
render 1024 icon_512x512@2x.png

iconutil -c icns "$iconset" -o "$out"
echo "$out"
