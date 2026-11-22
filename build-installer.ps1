# Builds the Windows release with Velopack's vpk (pinned in dotnet-tools.json; needs the .NET SDK).
# Into dist\releases: MicClean-win-Setup.exe (the installer: per user, no admin, Start menu entry,
# Settings -> Apps entry), and the update feed: MicClean-<version>-full.nupkg and releases.win.json.
#
# Also dist\MicClean-Setup.exe: the bridge for copies from before Velopack (0.11 and older),
# whose updater can only run the old setup program. It is that program with Velopack's setup
# appended (src\setup.rs): [setup bytes][length: u64 LE]["MCVELOP1"]. Publish it with 0.12.0 only.
#
# A test build: set MICCLEAN_TEST_FEED to a folder first. It is built as the separate app
# MicCleanTest (see src\os.rs) and packed into that folder, which it updates from, so building
# again with a higher version in Cargo.toml publishes an update to it.
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

# A running copy locks its exe, but Windows still lets it be renamed out of the way.
if (Test-Path target\release\micclean.exe) {
    Remove-Item target\release\micclean.old.exe -Force -ErrorAction SilentlyContinue
    Rename-Item target\release\micclean.exe micclean.old.exe -ErrorAction SilentlyContinue
}
& "$PSScriptRoot\licenses.ps1" # the exe embeds the notices, so they match its dependencies
cargo build --release
if ($LASTEXITCODE) { throw "cargo build failed" }

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
if ($env:MICCLEAN_TEST_FEED) {
    $id, $title, $out = 'MicCleanTest', 'MicClean Test', $env:MICCLEAN_TEST_FEED
} else {
    # Full packages only: deltas would need the last release downloaded first (vpk download github).
    $id, $title, $out = 'MicClean', 'MicClean', "$PSScriptRoot\dist\releases"
    Remove-Item $out -Recurse -Force -ErrorAction SilentlyContinue
}
# vpk packs a whole folder: just the exe (it carries everything else inside it).
$pack = "$PSScriptRoot\dist\pack"
Remove-Item $pack -Recurse -Force -ErrorAction SilentlyContinue
New-Item $pack -ItemType Directory -Force | Out-Null
Copy-Item target\release\micclean.exe $pack
dotnet tool restore | Out-Null
if ($LASTEXITCODE) { throw "dotnet tool restore failed" }
dotnet vpk pack --packId $id --packVersion $version --packDir $pack --mainExe micclean.exe `
    --packTitle $title --runtime win-x64 --packAuthors TooCoolCello --icon assets\micclean.ico --shortcuts StartMenuRoot --noPortable --outputDir $out
if ($LASTEXITCODE) { throw "vpk pack failed" }

$setup = "$out\$id-win-Setup.exe"
$stub = [IO.File]::ReadAllBytes("$PSScriptRoot\target\release\micclean-setup.exe")
$payload = [IO.File]::ReadAllBytes($setup)
$bridge = [IO.File]::Create("$PSScriptRoot\dist\MicClean-Setup.exe")
try {
    $bridge.Write($stub, 0, $stub.Length)
    $bridge.Write($payload, 0, $payload.Length)
    $bridge.Write([BitConverter]::GetBytes([uint64]$payload.Length), 0, 8)
    $magic = [Text.Encoding]::ASCII.GetBytes('MCVELOP1')
    $bridge.Write($magic, 0, 8)
} finally { $bridge.Close() }
Get-Item $setup, dist\MicClean-Setup.exe | Select-Object FullName, @{ n = 'MB'; e = { [math]::Round($_.Length / 1MB, 1) } }
