# Writes THIRD-PARTY-NOTICES.txt: the license files of every library compiled into MicClean,
# read from cargo's local copy of each crate. The app shows it under Settings. Run it after
# changing dependencies; build-installer.ps1 runs it before every release build.
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$meta = cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc | ConvertFrom-Json
if ($LASTEXITCODE) { throw "cargo metadata failed" }
$pkgs = @{}; foreach ($p in $meta.packages) { $pkgs[$p.id] = $p }
$deps = @{}; foreach ($n in $meta.resolve.nodes) { $deps[$n.id] = $n.deps }

# Normal dependencies only: build scripts and tests don't end up in the exe.
$root = ($meta.packages | Where-Object name -eq 'micclean').id
$shipped = @{}
$todo = [Collections.Generic.Queue[string]]::new()
$todo.Enqueue($root)
while ($todo.Count) {
    $id = $todo.Dequeue()
    if ($shipped.ContainsKey($id)) { continue }
    $shipped[$id] = $true
    foreach ($d in $deps[$id]) {
        if ($d.dep_kinds | Where-Object { -not $_.kind }) { $todo.Enqueue($d.pkg) }
    }
}
$shipped.Remove($root)
$list = $shipped.Keys | ForEach-Object { $pkgs[$_] } | Sort-Object name, version

function LicenseFiles($p) {
    $dir = Split-Path $p.manifest_path
    $files = @(Get-ChildItem $dir -File | Where-Object Name -match '^(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE|UNLICENSE)')
    if ($p.license_file) { $files += Get-Item (Join-Path $dir $p.license_file) }
    @($files | Sort-Object FullName -Unique)
}
function RepoRoot($p) { "$($p.repository)" -replace '/tree/.*$', '' -replace '\.git$', '' -replace '/$', '' }

$filesOf = @{}
$byRepo = @{}
foreach ($p in $list) {
    $filesOf[$p.id] = LicenseFiles $p
    if ($filesOf[$p.id] -and $p.repository -and -not $byRepo.ContainsKey((RepoRoot $p))) { $byRepo[(RepoRoot $p)] = $filesOf[$p.id] }
}

# Each distinct license text once, followed by the crates it covers. A crate that offers MIT
# as one choice ("MIT OR Apache-2.0") only needs its MIT file. A crate without files of its
# own uses those of a sibling crate from the same repository.
$texts = [ordered]@{}
$bare = @()
foreach ($p in $list) {
    $files = $filesOf[$p.id]
    if (-not $files -and $p.repository) { $files = $byRepo[(RepoRoot $p)] }
    $mit = @($files | Where-Object Name -match 'MIT')
    if ($mit -and $p.license -match '\bMIT\b' -and $p.license -match '\bOR\b|/') { $files = $mit }
    if (-not $files) { $bare += $p; $files = @() }
    # Bundled fonts carry their own licenses next to them, for this crate only.
    $fonts = Join-Path (Split-Path $p.manifest_path) 'fonts'
    if (Test-Path $fonts) { $files += Get-ChildItem $fonts -File -Filter *.txt }
    foreach ($f in $files) {
        $text = (Get-Content $f.FullName -Raw).Replace("`r`n", "`n").Trim()
        if (-not $texts.Contains($text)) { $texts[$text] = [Collections.Generic.List[string]]::new() }
        $texts[$text].Add("$($p.name) $($p.version)")
    }
}

$rule = '-' * 72
$out = [Text.StringBuilder]::new()
[void]$out.Append("Open-source licenses`n====================`n`n")
[void]$out.Append("MicClean is built with the libraries below. Each license text appears once, after the`nlibraries it covers.`n")
foreach ($e in $texts.GetEnumerator()) {
    [void]$out.Append("`n$rule`n$($e.Value -join ', ')`n$rule`n`n$($e.Key)`n")
}
if ($bare) {
    # No license file anywhere in their packages: their copyright and the standard text instead.
    [void]$out.Append("`n$rule`nThese libraries ship no license file. They are used under the MIT License below (or,`nwhere MIT isn't offered, the Apache License 2.0 text above).`n$rule`n`n")
    foreach ($p in $bare) {
        $by = if ($p.authors) { "Copyright (c) $($p.authors -join ', ')" } else { "Copyright (c) its authors ($(RepoRoot $p))" }
        [void]$out.Append("$($p.name) $($p.version) ($($p.license)): $by`n")
    }
    [void]$out.Append(@"

MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy of this software
and associated documentation files (the "Software"), to deal in the Software without
restriction, including without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

"@.Replace("`r`n", "`n"))
}
[IO.File]::WriteAllText("$PSScriptRoot\THIRD-PARTY-NOTICES.txt", $out.ToString())
"$($list.Count) libraries, $($texts.Count) distinct license texts, $($bare.Count) under the standard text: $(($bare | ForEach-Object name) -join ', ')"
