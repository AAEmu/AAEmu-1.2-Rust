param(
    [Parameter(Mandatory=$true)][string]$ClientArchive,
    [Parameter(Mandatory=$true)][string]$SevenZip,
    [Parameter(Mandatory=$true)][string]$OutputDirectory,
    [string]$DirectXDirectory
)
$ErrorActionPreference = 'Stop'
if (Test-Path -LiteralPath $OutputDirectory) { throw 'Choose a new output directory; existing bundles are never overwritten.' }
if (-not (Test-Path -LiteralPath $ClientArchive) -or -not (Test-Path -LiteralPath $SevenZip)) { throw 'Archive or 7-Zip executable missing.' }
# Check the archive index and require the exact pak/build used in compatibility tests.
$listing = @(& $SevenZip l -slt $ClientArchive)
if ($LASTEXITCODE -ne 0) { throw 'Cannot read the client archive.' }
$paths = @($listing | Where-Object { $_ -match '^Path = .*\\ArcheAge\\game_pak$' })
if ($paths.Count -ne 1) { throw 'Expected the stock r223748 archive folder layout.' }
$pakPath = $paths[0].Substring(7)
$entry = ($listing -join "`n") -split "`n\s*`n" | Where-Object { $_.Contains("Path = $pakPath`n") }
if ($entry -notmatch '(?m)^Size = 24720175616$' -or $entry -notmatch '(?m)^CRC = 803A3643$') { throw 'Archive game_pak does not match verified r223748.' }
New-Item -ItemType Directory -Path $OutputDirectory | Out-Null
$destination = Join-Path $OutputDirectory 'AAEmu-r223748-client.7z'
Copy-Item -LiteralPath $ClientArchive -Destination $destination
$stage = Join-Path $OutputDirectory 'staging'
$clientRoot = Join-Path $stage (Split-Path -Parent $pakPath)
New-Item -ItemType Directory -Path (Join-Path $clientRoot 'Launcher') -Force | Out-Null
foreach ($name in @('Play.cmd','server.json','README.txt')) { Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination $clientRoot }
foreach ($name in @('Start-Client.ps1','ClientRuntime.cs')) { Copy-Item -LiteralPath (Join-Path $PSScriptRoot ('Launcher\'+$name)) -Destination (Join-Path $clientRoot 'Launcher') }
if ($DirectXDirectory) {
    $dx = Join-Path $clientRoot 'Launcher\DirectX'
    New-Item -ItemType Directory -Path $dx | Out-Null
    foreach ($name in @('d3dx9_42.dll','d3dx11_42.dll','D3DCompiler_42.dll')) {
        Copy-Item -LiteralPath (Join-Path $DirectXDirectory $name) -Destination $dx
    }
}
# Updating the copied archive preserves its existing compressed client blocks.
Push-Location $stage
try {
    & $SevenZip a -t7z -mx=1 $destination '*'
    if ($LASTEXITCODE -ne 0) { throw 'Client archive update failed.' }
} finally { Pop-Location }
& $SevenZip t $destination
if ($LASTEXITCODE -ne 0) { throw 'Bundle integrity check failed.' }
$hash = Get-FileHash -LiteralPath $destination -Algorithm SHA256
($hash.Hash+'  '+[IO.Path]::GetFileName($destination)) | Set-Content -LiteralPath (Join-Path $OutputDirectory 'SHA256SUMS.txt') -Encoding ASCII
'Extract AAEmu-r223748-client.7z, then open Play.cmd in the ArcheAge folder. Read README.txt there.' |
    Set-Content -LiteralPath (Join-Path $OutputDirectory 'START-HERE.txt') -Encoding UTF8
Get-Item -LiteralPath $destination | Select-Object FullName,Length
