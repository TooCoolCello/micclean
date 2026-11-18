# MicClean

A small Windows and Mac app that cleans up your microphone before Discord (or anything else) hears it.
The Mac version is new and untested so far; it has everything except plugins.

- Noise removal (RNNoise) and echo cancellation (WebRTC AEC3, for speakers or open-back headphones).
  Echo cancellation can listen to everything you hear except MicClean, so hearing yourself doesn't
  cancel your own voice
- Hidden from screen shares: MicClean takes VB-Cable for itself (exclusive mode), so a Discord
  screen share with sound doesn't send your voice a second time
- A voice gate that knows the difference between singing and fan hum, so long held notes aren't cut off
- Teach it sounds to remove (keyboard, fan) and your own voice to keep
- 5-band parametric EQ, de-esser, auto level and compressor, with live graphs
- Smart setup: 3 seconds of your room and 15 of you reading set every effect for your voice, then
  get tried on that recording so your loudest words stay clear of the limiter. Undo, and a
  Suggest button in each effect
- Record 10 seconds, then compare the original with the processed version
- Setup walks you through your mic (and which input of an audio interface it's on), VB-Cable,
  headphones or speakers, then Smart setup. Home keeps the everyday switches and says what to
  fix when something's off; every effect and setting is under Advanced
- Lives in the tray, can start with Windows, reconnects if the mic is unplugged

## Install

Download `MicClean-win-Setup.exe` from the [latest release](../../releases/latest) and run it. It
installs for your account only (no admin needed) in `%LOCALAPPDATA%\MicClean` and adds a Start menu
entry. Windows will probably show "Windows protected your PC" because the installer isn't
code-signed. Choose More info, then Run anyway. The checksum for each download is published next
to it. To remove MicClean, use Settings → Apps; your settings stay in `%APPDATA%\MicClean` (delete
that folder too to remove them).

On a Mac (macOS 14.2 or later), download `MicClean-osx-Setup.pkg` and open it. Neither the installer
nor the app is signed with an Apple Developer ID, so macOS refuses them the first time: go to
System Settings → Privacy & Security and choose Open Anyway. MicClean then asks for the microphone,
and for system audio recording if you turn on echo cancellation. Settings live in
`~/Library/Application Support/MicClean`.

Installing and updating are done by [Velopack](https://velopack.io). The installed copy checks
GitHub once a day for new versions and updates with one click, or by itself once you aren't
talking; both can be turned off in Settings. If an update can't be installed, the version you had
keeps running. Velopack's own log is `%LOCALAPPDATA%\velopack\velopack_MicClean.log` on Windows and
`~/Library/Logs/velopack_MicClean.log` on a Mac. Your settings are copied to the `backups` folder in
the settings folder above (the last 10 versions), and a damaged settings file is replaced by the
newest copy.

Coming from 0.11 or older: its update to 0.12.0 moves MicClean over to Velopack by itself. On
Windows the old copy in `%LOCALAPPDATA%\Programs\MicClean` is removed, with its Start menu and
Settings → Apps entries, and "Start with Windows" moves to the new copy. On a Mac, Download in
Settings gets `MicClean.dmg` (0.12.0): drag it into Applications over the old one, once. Your
settings are kept either way.

## Use it with Discord

MicClean sends its output to a virtual cable. Install the free [VB-Cable](https://vb-audio.com/Cable/)
(not bundled, its license doesn't allow that), then click the Output box in the signal chain and
follow its "Using it with Discord" steps.

On a Mac, MicClean brings its own virtual device, "MicClean": Setup's Install button adds it (macOS asks
for your password once, and all sound pauses for a moment while Core Audio restarts). Then pick
MicClean as Discord's mic. To remove the device later:
`sudo rm -rf /Library/Audio/Plug-Ins/HAL/MicClean.driver && sudo killall coreaudiod`.

## Build

Needs Rust (MSVC toolchain) and the Windows SDK.

Packaging uses Velopack's `vpk`, pinned in `dotnet-tools.json` to the same version as the `velopack`
crate (`dotnet tool restore` fetches it), so it also needs the .NET SDK, 8 or newer.

```
cargo test
./build-installer.ps1      # dist\releases: MicClean-win-Setup.exe and the update feed
```

On a Mac (Xcode command line tools, `rustup target add aarch64-apple-darwin x86_64-apple-darwin`):

```
cargo test
bash build-mac.sh          # dist/releases: MicClean-osx-Setup.pkg and the update feed, Apple Silicon and Intel
```

Both also make the one-time bridge from 0.11 and older (`dist\MicClean-Setup.exe`,
`dist/MicClean.dmg`), published with 0.12.0 only. `build-installer-old.ps1` and `build-mac-old.sh`
are the scripts from before Velopack, kept for now; what they build doesn't update itself.

To try an update without publishing anything, set `MICCLEAN_TEST_FEED` to an empty folder and run
the build script. It builds "MicCleanTest", a separate app beside the real MicClean (its own
settings and install, no audio) that updates from that folder. Install it with
`MicCleanTest-win-Setup.exe` (or `MicCleanTest-osx-Setup.pkg`) from that folder, raise the version
in `Cargo.toml` and build again: that publishes the update to the folder.

`licenses.ps1` rewrites `THIRD-PARTY-NOTICES.txt` (shown in Settings) from the license files of every
library in the exe. `build-installer.ps1` runs it; run it yourself after changing dependencies.

A release (both installers and both update feeds) is built by pushing a tag that matches the version in `Cargo.toml`, e.g.
`git tag v0.2.0 && git push --tags`. Running the Release workflow by hand builds both as artifacts without
publishing.

## About this code

Most of this project was written with AI assistance (Claude, by Anthropic), directed and tested by its
author. The audio chain has unit tests; things that need real hardware (echo cancellation, device
reconnects, the installer prompts) are tested by hand.
