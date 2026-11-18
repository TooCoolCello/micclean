#!/bin/bash
# Builds the Mac release with Velopack's vpk (pinned in dotnet-tools.json; needs the .NET SDK).
# MicClean.app holds Apple Silicon and Intel in one binary, and the MicClean audio device
# (mac-driver/) in Contents/PlugIns; Setup installs it from there. Into dist/releases:
# MicClean-osx-Setup.pkg (the installer), MicClean-osx-Portable.zip (the app on its own), and
# the update feed: MicClean-<version>-full.nupkg and releases.osx.json.
#
# Also dist/MicClean.dmg, the bridge for copies from before Velopack (0.11 and older), whose
# Updates button opens MicClean.dmg in the browser: the same app, to drag into Applications.
# Publish it with 0.12.0 only.
#
# Needs Xcode's command line tools and both Rust targets:
# rustup target add aarch64-apple-darwin x86_64-apple-darwin
#
# Signed ad hoc (no Apple Developer account): Apple Silicon runs nothing unsigned, and macOS asks
# once before opening a downloaded copy (System Settings -> Privacy & Security -> Open Anyway).
# vpk adds its updater to the app, so vpk signs it last ("-" is codesign's ad hoc identity).
#
# A test build: set MICCLEAN_TEST_FEED to a folder first. It is built as the separate app
# MicCleanTest (see src/os.rs) and packed into that folder, which it updates from, so building
# again with a higher version in Cargo.toml publishes an update to it.
set -euo pipefail
cd "$(dirname "$0")"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
if [ -n "${MICCLEAN_TEST_FEED:-}" ]; then
	id=MicCleanTest bundle=io.github.toocoolcello.micclean.test out=$MICCLEAN_TEST_FEED
else
	id=MicClean bundle=io.github.toocoolcello.micclean out=dist/releases
	# Full packages only: deltas would need the last release downloaded first (vpk download github).
	rm -rf "$out"
fi
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
	MACOSX_DEPLOYMENT_TARGET=14.2 cargo build --release --locked --bin micclean --target "$target"
done

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/micclean" \
	target/aarch64-apple-darwin/release/micclean target/x86_64-apple-darwin/release/micclean
sed -e "s/@VERSION@/$version/g" -e "s/io\.github\.toocoolcello\.micclean/$bundle/" \
	-e "s|<string>MicClean</string>|<string>$id</string>|g" assets/Info.plist > "$app/Contents/Info.plist"
cp assets/micclean.icns "$app/Contents/Resources/"

mkdir -p "$out"
dotnet tool restore > /dev/null
dotnet vpk pack --packId "$id" --packVersion "$version" --packDir "$app" --mainExe micclean \
	--packTitle "$id" --packAuthors TooCoolCello --icon assets/micclean.icns --outputDir "$out" \
	--signAppIdentity - --signEntitlements assets/mac-entitlements.plist

# The bridge disk image: the packed app (with vpk's updater in it), from the portable zip.
mkdir dist/dmg
ditto -x -k "$out/$id-osx-Portable.zip" dist/dmg
codesign --verify --strict --deep --verbose=1 "dist/dmg/$id.app"
ln -s /Applications dist/dmg/Applications
hdiutil create -volname MicClean -srcfolder dist/dmg -format UDZO -ov dist/MicClean.dmg
rm -rf dist/dmg
echo "Built $out ($id $version) and dist/MicClean.dmg"
