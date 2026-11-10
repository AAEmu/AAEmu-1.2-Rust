param([Parameter(Mandatory=$true)][string]$OutputDirectory)
$ErrorActionPreference='Stop'
$compiler=Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
if(-not(Test-Path -LiteralPath $compiler)){throw 'Windows .NET Framework C# compiler not found.'}
New-Item -ItemType Directory -Path $OutputDirectory -Force|Out-Null
$output=Join-Path ([IO.Path]::GetFullPath($OutputDirectory)) 'AAEmu Launcher.exe'
$arguments=@('/nologo','/target:winexe','/platform:x64','/optimize+','/reference:System.dll','/reference:System.Core.dll','/reference:System.Drawing.dll','/reference:System.Windows.Forms.dll','/reference:System.Security.dll','/reference:System.Xml.dll',('/out:'+$output),('/win32icon:'+(Join-Path $PSScriptRoot 'Assets\AAEmu.ico')),('/win32manifest:'+(Join-Path $PSScriptRoot 'launcher.manifest')),('/resource:'+(Join-Path $PSScriptRoot 'Assets\banner.jpg')+',AAEmu.Banner'))
foreach($file in @('AAEmuLauncher.cs','LauncherCore.cs','ClientRuntime.cs','AssemblyInfo.cs')){$arguments+=Join-Path $PSScriptRoot $file}
& $compiler @arguments
if($LASTEXITCODE -ne 0){throw 'Launcher compilation failed.'}
$report=Join-Path $OutputDirectory 'launcher-tests.txt'
$p=Start-Process -FilePath $output -ArgumentList ('--self-test "'+$report+'"') -WindowStyle Hidden -Wait -PassThru
if($p.ExitCode -ne 0){throw 'Launcher self-tests failed.'}
Get-Content -LiteralPath $report
Get-Item -LiteralPath $output|Select-Object FullName,Length
