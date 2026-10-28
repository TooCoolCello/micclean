# Builds dist\MicClean-Setup.exe: the setup program with micclean.exe appended to it.
# Layout of the tail (read back by src\setup.rs): [exe bytes][exe length: u64 LE]["MCPAYLD1"].
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

# A running copy locks its exe, but Windows still lets it be renamed out of the way.
if (Test-Path target\release\micclean.exe) {
    Remove-Item target\release\micclean.old.exe -Force -ErrorAction SilentlyContinue
    Rename-Item target\release\micclean.exe micclean.old.exe -ErrorAction SilentlyContinue
}
cargo build --release
if ($LASTEXITCODE) { throw "cargo build failed" }

$stub = [IO.File]::ReadAllBytes("$PSScriptRoot\target\release\micclean-setup.exe")
$app = [IO.File]::ReadAllBytes("$PSScriptRoot\target\release\micclean.exe")
New-Item dist -ItemType Directory -Force | Out-Null
$out = [IO.File]::Create("$PSScriptRoot\dist\MicClean-Setup.exe")
try {
    $out.Write($stub, 0, $stub.Length)
    $out.Write($app, 0, $app.Length)
    $out.Write([BitConverter]::GetBytes([uint64]$app.Length), 0, 8)
    $magic = [Text.Encoding]::ASCII.GetBytes('MCPAYLD1')
    $out.Write($magic, 0, 8)
} finally { $out.Close() }
Get-Item dist\MicClean-Setup.exe | Select-Object FullName, @{ n = 'MB'; e = { [math]::Round($_.Length / 1MB, 1) } }
