#!/bin/bash
# Builds dist/MicClean.dmg on a Mac: MicClean.app (Apple Silicon and Intel in one binary) on a
# disk image with an Applications shortcut to drag it onto. The app carries the MicClean audio
# device (mac-driver/) in Contents/PlugIns; Setup installs it from there. Needs Xcode's command
# line tools and both Rust targets: rustup target add aarch64-apple-darwin x86_64-apple-darwin
#
# Signed ad hoc (no Apple Developer account): Apple Silicon runs nothing unsigned, and macOS asks
# once before opening a downloaded copy (System Settings -> Privacy & Security -> Open Anyway).
#
# One-click updates on a Mac (Velopack) are off until they've been tried on a Mac: this copy only
# says when a new MicClean.dmg is out (src/update.rs). MICCLEAN_MAC_UPDATES=1 turns them on: the
# app is built with the mac-updates feature and packed with Velopack's vpk (pinned in
# dotnet-tools.json; needs the .NET SDK), which adds its updater and signs the app last with the
# entitlements in assets/mac-entitlements.plist. Into dist/releases: MicClean-osx-Setup.pkg, the
# update feed (MicClean-<version>-full.nupkg, releases.osx.json) and MicClean-osx-Portable.zip,
# whose app goes on the disk image.
#
# A test build of those updates: set MICCLEAN_TEST_FEED to a folder first (it implies
# MICCLEAN_MAC_UPDATES=1). It is built as the separate app MicCleanTest (see src/os.rs) and
# packed into that folder, which it updates from, so building again with a higher version in
# Cargo.toml publishes an update to it.
set -euo pipefail
cd "$(dirname "$0")"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
updates=${MICCLEAN_MAC_UPDATES:-}
features=()
if [ -n "${MICCLEAN_TEST_FEED:-}" ]; then
	updates=1 id=MicCleanTest bundle=io.github.toocoolcello.micclean.test out=$MICCLEAN_TEST_FEED
else
	id=MicClean bundle=io.github.toocoolcello.micclean out=dist/releases
	# Full packages only: deltas would need the last release downloaded first (vpk download github).
	[ -z "$updates" ] || rm -rf "$out"
fi
[ -z "$updates" ] || features=(--features mac-updates)
app=dist/$id.app
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
	MACOSX_DEPLOYMENT_TARGET=14.2 cargo build --release --locked --bin micclean --target "$target" ${features[@]+"${features[@]}"}
done

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/micclean" \
	target/aarch64-apple-darwin/release/micclean target/x86_64-apple-darwin/release/micclean
sed -e "s/@VERSION@/$version/g" -e "s/io\.github\.toocoolcello\.micclean/$bundle/" \
	-e "s|<string>MicClean</string>|<string>$id</string>|g" assets/Info.plist > "$app/Contents/Info.plist"
cp assets/micclean.icns "$app/Contents/Resources/"

mkdir dist/dmg
if [ -z "$updates" ]; then
	codesign --force --sign - "$app"
	cp -R "$app" dist/dmg/
else
	mkdir -p "$out"
	dotnet tool restore > /dev/null
	dotnet vpk pack --packId "$id" --packVersion "$version" --packDir "$app" --mainExe micclean \
		--packTitle "$id" --packAuthors TooCoolCello --icon assets/micclean.icns --outputDir "$out" \
		--signAppIdentity - --signEntitlements assets/mac-entitlements.plist
	# The packed app (with vpk's updater in it), from the portable zip.
	ditto -x -k "$out/$id-osx-Portable.zip" dist/dmg
fi
codesign --verify --strict --deep --verbose=1 "dist/dmg/$id.app"
ln -s /Applications dist/dmg/Applications
hdiutil create -volname MicClean -srcfolder dist/dmg -format UDZO -ov dist/MicClean.dmg
rm -rf dist/dmg
if [ -n "$updates" ]; then echo "Built $out ($id $version, one-click updates) and dist/MicClean.dmg"; else echo "Built dist/MicClean.dmg ($version)"; fi
