#!/usr/bin/env bash
set -euo pipefail

IDENTIFIER="dev.pkarpovich.mimi"
APP="Mimi.app"

if [ $# -lt 2 ]; then
	echo "usage: ${0##*/} <binary> <out-dir> [identity]" >&2
	echo "assembles $APP around a built mimi and signs it when an identity is given" >&2
	exit 2
fi

binary=$1
out_dir=$2
identity=${3:-}
root=$(cd "$(dirname "$0")/.." && pwd)

if [ ! -x "$binary" ]; then
	echo "$binary is not an executable" >&2
	exit 1
fi

version=$(grep -m1 '^version' "$root/Cargo.toml" | cut -d'"' -f2)
if [ -z "$version" ]; then
	echo "no version in Cargo.toml" >&2
	exit 1
fi

icon="$root/assets/AppIcon.icns"
if [ ! -f "$icon" ]; then
	echo "$icon is missing - run scripts/icon.sh" >&2
	exit 1
fi

app="$out_dir/$APP"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

plist="$app/Contents/Info.plist"
sed "s/__VERSION__/$version/g" "$root/Info.plist.template" > "$plist"

# Bundle-only keys, deliberately absent from Info.plist.template because build.rs
# embeds that template into the bare binary's __TEXT,__info_plist. Declaring a
# loose daemon an APPL bundle makes macOS treat it as a UI application, and a
# daemon that owns a Dock tile is not what launchd should be starting.
/usr/libexec/PlistBuddy -c "Add :CFBundleExecutable string mimi" "$plist"
/usr/libexec/PlistBuddy -c "Add :CFBundlePackageType string APPL" "$plist"
/usr/libexec/PlistBuddy -c "Add :LSUIElement bool true" "$plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string AppIcon" "$plist"
plutil -lint "$plist"
cp "$binary" "$app/Contents/MacOS/mimi"
cp "$icon" "$app/Contents/Resources/AppIcon.icns"

# The bundle is what makes a macOS permission durable: TCC identifies a bundle by
# its identifier at a path that does not move, and a loose binary by its path
# alone - which Homebrew changes on every version. The microphone entitlement
# rides with the signature, so the same command that stabilises the identity is
# the one that grants the daemon its input.
if [ -n "$identity" ]; then
	codesign \
		--force \
		--timestamp \
		--options runtime \
		--entitlements "$root/mimi.entitlements" \
		--identifier "$IDENTIFIER" \
		--sign "$identity" \
		"$app"
	codesign --verify --strict --deep --verbose=2 "$app"
	codesign --display --verbose=2 "$app" 2>&1 | grep '^Identifier='
fi

echo "$app"
