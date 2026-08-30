#!/usr/bin/env bash
set -euo pipefail

identity="${1:-}"
if [ -z "$identity" ]; then
	echo "usage: scripts/build-signed.sh <developer-id-identity>" >&2
	echo "list identities with: security find-identity -v -p codesigning" >&2
	exit 2
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

cargo build --release

binary="target/release/mimi"
codesign --force --options runtime --timestamp --sign "$identity" "$binary"
codesign --verify --strict --verbose=2 "$binary"

echo "signed $root/$binary as $identity"
