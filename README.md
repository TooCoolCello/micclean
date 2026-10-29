# MicClean

A small Windows app that cleans up your microphone before Discord (or anything else) hears it.

- Noise removal (RNNoise) and echo cancellation (WebRTC AEC3, for speakers or open-back headphones)
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

Download `MicClean-Setup.exe` from the [latest release](../../releases/latest) and run it. It installs
for your account only (no admin needed) and adds a Start menu entry. The installed copy checks GitHub
once a day for new versions and updates with one click; this can be turned off in Settings.

Windows will probably show "Windows protected your PC" because the installer isn't code-signed.
Choose More info, then Run anyway. The checksum for each download is published next to it.

## Use it with Discord

MicClean sends its output to a virtual cable. Install the free [VB-Cable](https://vb-audio.com/Cable/)
(not bundled, its license doesn't allow that), then click the Output box in the signal chain and
follow its "Using it with Discord" steps.

## Build

Needs Rust (MSVC toolchain) and the Windows SDK.

```
cargo test
./build-installer.ps1      # dist\MicClean-Setup.exe
```

`licenses.ps1` rewrites `THIRD-PARTY-NOTICES.txt` (shown in Settings) from the license files of every
library in the exe. `build-installer.ps1` runs it; run it yourself after changing dependencies.

A release is built by pushing a tag that matches the version in `Cargo.toml`, e.g. `git tag v0.2.0 && git push --tags`.

## About this code

Most of this project was written with AI assistance (Claude, by Anthropic), directed and tested by its
author. The audio chain has unit tests; things that need real hardware (echo cancellation, device
reconnects, the installer prompts) are tested by hand.
