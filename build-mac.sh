#!/bin/bash
# Builds dist/MicClean.dmg on a Mac: MicClean.app (Apple Silicon and Intel in one binary) on a
# disk image with an Applications shortcut to drag it onto. The app carries the MicClean audio
# device (mac-driver/) in Contents/PlugIns; Setup installs it from there. Needs Xcode's command
# line tools and both Rust targets: rustup target add aarch64-apple-darwin x86_64-apple-darwin
#
# Everything is signed ad hoc (no Apple Developer account): Apple Silicon runs nothing unsigned,
# and macOS asks once before opening a downloaded copy (System Settings -> Privacy & Security
# -> Open Anyway).
set -euo pipefail
cd "$(dirname "$0")"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
app=dist/MicClean.app
rm -rf "$app" dist/dmg dist/MicClean.dmg

# The device first: it's quick, so a C mistake shows before the long Rust builds.
driver="$app/Contents/PlugIns/MicClean.driver"
mkdir -p "$driver/Contents/MacOS"
clang -arch arm64 -arch x86_64 -mmacosx-version-min=11.0 -O2 -Wall -Wextra -Werror -bundle \
	-o "$driver/Contents/MacOS/MicClean" mac-driver/MicClean.c -framework CoreAudio -framework CoreFoundation
sed "s/@VERSION@/$version/g" mac-driver/Info.plist > "$driver/Contents/Info.plist"
codesign --force --sign - "$driver"

for target in aarch64-apple-darwin x86_64-apple-darwin; do
	# 14.2: the oldest macOS with the process tap the echo source uses (see assets/Info.plist).
	MACOSX_DEPLOYMENT_TARGET=14.2 cargo build --release --locked --bin micclean --target "$target"
done

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/micclean" \
	target/aarch64-apple-darwin/release/micclean target/x86_64-apple-darwin/release/micclean
sed "s/@VERSION@/$version/g" assets/Info.plist > "$app/Contents/Info.plist"
cp assets/micclean.icns "$app/Contents/Resources/"
codesign --force --sign - "$app"
codesign --verify --strict --deep "$app"

mkdir dist/dmg
cp -R "$app" dist/dmg/
ln -s /Applications dist/dmg/Applications
hdiutil create -volname MicClean -srcfolder dist/dmg -format UDZO -ov dist/MicClean.dmg
rm -rf dist/dmg
echo "Built dist/MicClean.dmg ($version)"
