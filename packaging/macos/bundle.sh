#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
version=${TOKEN_MONITOR_VERSION:-0.1.0}
app="$root/dist/Token Monitor Lite.app"
binary="$root/target/release/token-monitor-lite"

test -x "$binary"
rm -rf "$root/dist"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/token-monitor-lite"
cp "$root/packaging/macos/Info.plist" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$app/Contents/Info.plist"
cp "$root/LICENSE" "$app/Contents/Resources/LICENSE"

# Ad-hoc signing keeps the bundle internally consistent. It is not a Developer
# ID signature and does not satisfy Gatekeeper for downloaded releases.
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

ditto -c -k --keepParent "$app" "$root/dist/token-monitor-lite-${version}-macos-arm64-unsigned.zip"
shasum -a 256 "$root/dist/token-monitor-lite-0.1.0-macos-arm64-unsigned.zip"
