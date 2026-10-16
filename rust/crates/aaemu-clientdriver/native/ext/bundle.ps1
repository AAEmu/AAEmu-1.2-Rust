# Concatenates the manifest's Lua files into one resource ("--@@ext chunk <name>" sections).
# Used by build.bat: powershell -NoProfile -File ext\bundle.ps1 -Out <ext_bundle.lua>
param([Parameter(Mandatory = $true)][string]$Out)
$ErrorActionPreference = 'Stop'
$dir = $PSScriptRoot
$names = Get-Content -LiteralPath (Join-Path $dir 'manifest.txt') |
    ForEach-Object { $_.Trim() } | Where-Object { $_ -and -not $_.StartsWith('#') }
if (-not $names -or $names[0] -ne 'core.lua') { throw 'manifest.txt must start with core.lua' }
$sb = New-Object System.Text.StringBuilder
foreach ($n in $names) {
    $path = Join-Path $dir $n
    if (-not (Test-Path -LiteralPath $path)) { throw "manifest entry missing: $n" }
    $text = [IO.File]::ReadAllText($path)
    [void]$sb.Append("--@@ext chunk $n`n")
    [void]$sb.Append($text.Replace("`r`n", "`n"))
    if (-not $text.EndsWith("`n")) { [void]$sb.Append("`n") }
}
[IO.File]::WriteAllText($Out, $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))
