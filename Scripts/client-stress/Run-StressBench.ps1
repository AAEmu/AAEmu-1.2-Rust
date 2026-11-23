[CmdletBinding()]
param(
    [ValidateSet('stock','dev','archenova')][string]$Config = 'dev',
    [ValidateSet('players','housing','farming','effects','water','combined')][string]$Scenario = 'players',
    [ValidateRange(1,30)][int]$Repeat = 3,
    [ValidateSet('low','high')][string]$Preset = 'low',
    [ValidateSet(1,100,200,300,500)][int]$Bots = 200,
    [ValidateRange(10,300)][int]$Seconds = 30,
    [switch]$TraceEtw,
    [string]$StockDriver = 'C:\aa-re\lanes\targets\perf-bench\aadriver\aadriver.dll',
    [string]$Target = 'C:\aa-re\lanes\targets\perf-bench\debug',
    [string]$Output = 'C:\aa-re\lanes\client-perf\bench\runs'
)
$ErrorActionPreference = 'Stop'
if ($Config -eq 'archenova') { throw 'ArcheNova requires a dedicated NativeAbility server and client fixture.' }
if ($Scenario -in 'housing','effects','combined') { throw "The $Scenario seed fixture is pending." }

$audit = 'C:\aa-re\lanes\live-audit'
$dev = 'C:\aa-re\devclient-r208022'
$mysql = 'C:\Program Files\MySQL\MySQL Server 8.4\bin'
$client = "C:\aa-re\testclients\perf-$Config"
$ports = @(3417,28537,28534,28539,28550,28580)
$source = 'C:\AA1.2'
$center = if ($Scenario -eq 'water') { @(15458,10366,100.5) } else { @(15562,15354,128.0) }
# r208022's client scene uses a cell-relative origin. The land route showed
# (4298, 4090) after a server teleport to (15562, 15354).
$localCenter = if ($Scenario -eq 'water') { @(4194,4222) } else { @(4298,4090) }
$spec = if ($Preset -eq 'high') { 4 } else { 1 }
$lease = New-Object Threading.Mutex($false, 'Local\AAEmuLiveAuditStressBench')

function Copy-Tree($From, $To, $Exclude) {
    & robocopy $From $To /E /XJ /R:0 /W:0 /NFL /NDL /NJH /NJS /NP @Exclude | Out-Null
    if ($LASTEXITCODE -ge 8) { throw "Copy failed from $From (robocopy $LASTEXITCODE)" }
}
function Assert-Idle {
    $listeners = [Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners()
    $occupied = @($listeners | Where-Object Port -In $ports)
    if ($occupied.Count) { throw "Benchmark ports in use: $($occupied.Port -join ', ')" }
}
function Wait-Port([int]$Port, $Process, [int]$TimeoutSeconds) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        if ($Process.HasExited) { throw "PID $($Process.Id) exited before opening port $Port" }
        $listeners = [Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners()
        if ($listeners.Port -contains $Port) { return }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for port $Port"
}
function Start-Owned($Name, $File, $Dir, [string[]]$Arguments) {
    $params = @{
        FilePath = $File; WorkingDirectory = $Dir; PassThru = $true; WindowStyle = 'Hidden'
        RedirectStandardOutput = (Join-Path $run "$Name.out"); RedirectStandardError = (Join-Path $run "$Name.err")
    }
    if ($Arguments.Count) { $params.ArgumentList = $Arguments }
    $p = Start-Process @params
    $owned.Add($p)
    # Keep a process handle so Windows PowerShell can read ExitCode after exit.
    $null = $p.Handle
    $p
}
function Stop-OwnedMySql($Ini) {
    $pattern = '(?:^|\s)--defaults-file="?'+ [regex]::Escape($Ini) + '"?(?:\s|$)'
    $connections = @(Get-NetTCPConnection -State Listen -LocalPort 3417 -ErrorAction SilentlyContinue)
    foreach ($connection in $connections) {
        $owner = Get-CimInstance Win32_Process -Filter "ProcessId=$($connection.OwningProcess)"
        if ($owner.ExecutablePath -eq "$mysql\mysqld.exe" -and
            $owner.CommandLine -match $pattern) {
            $live = Get-Process -Id $owner.ProcessId -ErrorAction SilentlyContinue
            if ($live) {
                $shutdown = Start-Process "$mysql\mysqladmin.exe" -ArgumentList '--host=127.0.0.1','--port=3417','--user=root','shutdown' -WindowStyle Hidden -PassThru
                if (-not $shutdown.WaitForExit(15000)) { $shutdown.Kill() }
                if (-not $live.WaitForExit(5000)) { $live.Kill(); $live.WaitForExit(5000) | Out-Null }
                $shutdown.Dispose()
                $live.Dispose()
            }
        }
    }
}
function Call-Dev($Command, $Payload = @{}) {
    if ($gameClient.HasExited) { throw "Client exited ($($gameClient.ExitCode)) before $Command. See the client log and crash directory." }
    $json = $Payload | ConvertTo-Json -Compress
    $r = & "$dev\tools\aadev-call.ps1" -ClientPid $gameClient.Id -TimeoutMs 5000 $Command $json | ConvertFrom-Json
    if ($null -eq $r) { throw "Dev pipe returned no response to $Command. See the client log and crash directory." }
    if (-not $r.ok) { throw "$Command failed: $($r.error)" }
    $r
}
function Call-Stock($Request, $ScreenshotPath) {
    if ($gameClient.HasExited) { throw "Stock client exited before $Request ($($gameClient.ExitCode))" }
    $pipe = New-Object IO.Pipes.NamedPipeClientStream('.', "aaemu-clientdriver-$($gameClient.Id)",
        [IO.Pipes.PipeDirection]::InOut, [IO.Pipes.PipeOptions]::Asynchronous)
    try {
        $pipe.Connect(3000)
        $payload = [Text.Encoding]::UTF8.GetBytes($Request)
        $writer = New-Object IO.BinaryWriter($pipe)
        $writer.Write([int]$payload.Length)
        $writer.Write([byte[]]$payload)
        $writer.Flush()
        $timeout = if ($ScreenshotPath) { 12000 } else { 5000 }
        $length = [BitConverter]::ToInt32((Read-StockBytes $pipe 4 $timeout), 0)
        if ($length -lt 1 -or $length -gt 64MB) { throw "Invalid stock driver response length $length" }
        $response = Read-StockBytes $pipe $length $timeout
        if ($response[0] -ne 48) {
            $reason = [Text.Encoding]::UTF8.GetString($response, 1, $length - 1)
            throw "Stock driver rejected $Request. $reason"
        }
        if ($ScreenshotPath) {
            if ($length -lt 9 -or [BitConverter]::ToUInt64($response, 1) -ne 0x0a1a0a0d474e5089) {
                throw 'Stock driver returned an invalid PNG.'
            }
            [IO.File]::WriteAllBytes($ScreenshotPath, [byte[]]$response[1..($length - 1)])
        } else {
            [Text.Encoding]::UTF8.GetString($response, 1, $length - 1)
        }
    } finally {
        $pipe.Dispose()
    }
}

function Read-StockBytes($Pipe, [int]$Length, [int]$TimeoutMs) {
    $bytes = New-Object byte[] $Length
    $offset = 0
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    while ($offset -lt $Length) {
        $read = $Pipe.ReadAsync($bytes, $offset, $Length - $offset)
        if (-not $read.Wait([Math]::Max(0, $TimeoutMs - [int]$deadline.ElapsedMilliseconds))) {
            throw 'Stock driver response timed out.'
        }
        if (-not $read.Result) { throw 'Stock driver disconnected.' }
        $offset += $read.Result
    }
    return ,$bytes
}

function Lua($Code) {
    if ($Config -eq 'stock') { return (Call-Stock "U$Code") }
    $r = Call-Dev lua @{code = $Code}
    if ($r.status -ne 'ok') { throw "Lua failed: $($r.values)" }
    $r.values
}
function Send-Gm($Command) {
    Lua "X2Chat:SendChatMessage(CHAT_SAY, '$Command', '')" | Out-Null
}
function Save-Json($Value, $Name) {
    ConvertTo-Json -InputObject $Value -Depth 32 | Set-Content (Join-Path $run $Name) -Encoding UTF8
}
function Get-CpuSnapshot {
    Get-Process | Where-Object { $null -ne $_.CPU } |
        Select-Object Id, ProcessName, CPU, @{n='StartUtc';e={if ($_.StartTime) { $_.StartTime.ToUniversalTime().ToString('o') }}}
}
function Percentile($Sorted, [double]$P) {
    if ($Sorted.Count) { $Sorted[[int][Math]::Max(0, [Math]::Ceiling($P * $Sorted.Count) - 1)] }
}
function Assert-BotsActive {
    if ($n -and ($botsProcess.HasExited -or
        (Select-String -LiteralPath "$run\bots.out" -Pattern '^busy finished ' -Quiet))) {
        throw 'A bot ended before the measurement finished. See bots.out and bots.err.'
    }
}
function Wait-Dev([string]$Description, [int]$TimeoutSeconds, [scriptblock]$Check) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        if ($gameClient.HasExited) { throw "Client exited ($($gameClient.ExitCode)) during $Description" }
        try { if (& $Check) { return } } catch { $lastError = $_.Exception.Message }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out during $Description. $lastError"
}
function Invoke-FarmCommand($Name, $Arguments) {
    $reply = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:28580/api/commands/$Name" -TimeoutSec 30 `
        -Headers @{Authorization="Bearer $farmApiKey"} -ContentType 'application/json' `
        -Body (@{character='Perfobserver';arguments=$Arguments} | ConvertTo-Json -Compress)
    if ($reply.ErrorMessages.Count) {
        throw "Farm command $Name failed. $($reply.ErrorMessages -join ' ')"
    }
    $reply.Messages
}

function Get-FarmDoodads {
    foreach ($line in (Invoke-FarmCommand 'around' 'doodad 85')) {
        if ($line -match 'BcId: (\d+) DoodadTemplateId: (\d+).*FuncGroupId: (-?\d+)') {
            [pscustomobject]@{id=[int]$Matches[1];template=[int]$Matches[2];phase=[int]$Matches[3]}
        }
    }
}

function Seed-Farm {
    # r208022 templates from the aaemu-testclient plant scenario.
    $templates = @(2274,2259,2756,4867,4891,4594,367,2274)
    $centerArgs = $center -join ' '
    Invoke-FarmCommand 'move' $centerArgs | Out-Null
    Start-Sleep -Milliseconds 700
    $original = @{}
    foreach ($doodad in (Get-FarmDoodads)) { $original[$doodad.id] = $true }
    for ($i=0; $i -lt $templates.Count; $i++) {
        $x = $center[0] + 20 * ($i % 4) - 30
        $y = $center[1] + 20 * [Math]::Floor($i / 4) - 10
        Invoke-FarmCommand 'move' "$x $y $($center[2])" | Out-Null
        Start-Sleep -Milliseconds 700
        Invoke-FarmCommand 'spawngrid' "doodad $($templates[$i]) 5 5 3" | Out-Null
        if ($i -eq 0) {
            # Advance the nearby crop while leaving its second grid at the initial stage.
            Invoke-FarmCommand 'tickdoodad' "$($templates[$i])" |
                Set-Content "$run\farm-phase-command.txt"
        }
    }
    Invoke-FarmCommand 'move' $centerArgs | Out-Null
    Start-Sleep -Milliseconds 700
    $spawned = @(Get-FarmDoodads | Where-Object { -not $original.ContainsKey($_.id) -and $_.template -in $templates })
    Save-Json $spawned 'farm-doodads.json'
    if ($spawned.Count -ne 200) {
        throw "Farm fixture expected 200 new doodads and found $($spawned.Count). See farm-doodads.json."
    }
    $cropPhases = @($spawned | Where-Object template -EQ $templates[0] | Group-Object phase)
    if ($cropPhases.Count -lt 2) {
        throw 'Farm fixture did not produce mixed growth stages. See farm-doodads.json.'
    }
}

function Write-TraceMarker($Name) {
    if ($script:traceStarted) {
        "$([DateTime]::UtcNow.ToString('o'))`t$Name" | Add-Content "$run\trace-markers.tsv"
    }
}

function Measure-Route {
    Write-TraceMarker 'route-setup'
    $frames = New-Object 'Collections.Generic.List[object]'
    $memory = New-Object 'Collections.Generic.List[object]'
    $gap = 0
    $start = [DateTime]::UtcNow
    $cpuBefore = @(Get-CpuSnapshot)
    $threadsBefore = (Call-Dev threads).threads
    $stats = Call-Dev stats @{history=$true}
    $previous = [long]$stats.gfx.frames
    $previousMs = $null
    $nextSample = 0
    $clockAnchors = New-Object 'Collections.Generic.List[object]'
    $nextStep = 2000
    Call-Dev io_stats @{reset=$true} | Out-Null
    Call-Dev profile_start @{duration_ms=($Seconds * 1000);interval_ms=10} | Out-Null
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $completed = $false
    $movement = 'W'
    try {
        Call-Dev input_key @{key=$movement;action='down'} | Out-Null
        while ($clock.Elapsed.TotalSeconds -lt $Seconds) {
            $pollStart = [DateTime]::UtcNow
            $stats = Call-Dev stats @{history=$true}
            if ($script:traceStarted) {
                $clockAnchors.Add([pscustomobject]@{
                    poll_start_utc = $pollStart.ToString('o')
                    poll_end_utc = [DateTime]::UtcNow.ToString('o')
                    client_uptime_ms = $stats.uptime_ms
                    present_age_ms = $stats.gfx.present_age_ms
                    frame = $stats.gfx.frames
                })
            }
            if ($stats.gfx.width -ne 1024 -or $stats.gfx.height -ne 768) {
                throw "Expected a 1024x768 Present surface, got $($stats.gfx.width)x$($stats.gfx.height)"
            }
            $count = [long]$stats.gfx.frames - $previous
            $history = @($stats.gfx.history)
            $missed = [Math]::Max(0, $count - $history.Count)
            $gap += $missed
            $new = [int][Math]::Min($count, $history.Count)
            $first = $history.Count - $new
            if ($new -and ($null -eq $previousMs -or $missed)) {
                $previousMs = [double]$stats.uptime_ms - [double]$stats.gfx.present_age_ms
                for ($i = $first; $i -lt $history.Count; $i++) {
                    $previousMs -= [double]$history[$i]
                }
            }
            for ($i = $first; $i -lt $history.Count; $i++) {
                $ms = [double]$history[$i]
                $previousMs += $ms
                $frames.Add([pscustomobject]@{
                    frame = [long]$stats.gfx.frames - ($history.Count - 1 - $i)
                    client_uptime_ms = [Math]::Round($previousMs, 3)
                    frame_ms = $ms
                })
            }
            $previous = [long]$stats.gfx.frames
            if ($clock.ElapsedMilliseconds -ge $nextSample) {
                Assert-BotsActive
                $gameClient.Refresh()
                $memory.Add([pscustomobject]@{
                    utc = [DateTime]::UtcNow.ToString('o')
                    private_bytes = $gameClient.PrivateMemorySize64
                    virtual_bytes = $gameClient.VirtualMemorySize64
                    largest_free_mib = $stats.memory.largest_free_block_mb
                    handles = $gameClient.HandleCount
                })
                $nextSample = $clock.ElapsedMilliseconds + 1000
            }
            if ($clock.ElapsedMilliseconds -ge $nextStep) {
                Call-Dev input_key @{key=$movement;action='up'} | Out-Null
                $movement = if ($movement -eq 'W') { 'S' } else { 'W' }
                Call-Dev input_key @{key=$movement;action='down'} | Out-Null
                Call-Dev input_key @{key='SPACE';action='press'} | Out-Null
                $nextStep += 2000
            }
            Start-Sleep -Milliseconds 100
        }
        Assert-BotsActive
        $completed = $true
    } finally {
        try { Call-Dev input_key @{key=$movement;action='up'} | Out-Null } catch { Write-Warning $_.Exception.Message }
        Write-TraceMarker 'route-ended'
        if ($clockAnchors.Count) {
            $clockAnchors.ToArray() | Export-Csv "$run\present-clock.tsv" -Delimiter "`t" -NoTypeInformation
        }
        $frames.ToArray() | Export-Csv (Join-Path $run 'frames.tsv') -Delimiter "`t" -NoTypeInformation
        $hitches = @($frames.ToArray() | Where-Object frame_ms -GT 50)
        $hitches | Export-Csv (Join-Path $run 'hitches.tsv') -Delimiter "`t" -NoTypeInformation
        $memory.ToArray() | Export-Csv (Join-Path $run 'memory.tsv') -Delimiter "`t" -NoTypeInformation
        Save-Json @{before=$cpuBefore; after=@(Get-CpuSnapshot)} 'process-cpu.json'
        $sorted = @($frames.ToArray().frame_ms | Sort-Object)
        Save-Json @{
            config=$Config; scenario=$Scenario; preset=$Preset; bots=$n; start_utc=$start.ToString('o')
            complete=$completed; frames=$frames.Count; missed_frames=$gap
            p50_ms=(Percentile $sorted 0.50); p95_ms=(Percentile $sorted 0.95)
            p99_ms=(Percentile $sorted 0.99); max_ms=(Percentile $sorted 1.0)
            hitches_over_50_ms=$hitches.Count
            peak_virtual_bytes=($memory.ToArray() | Measure-Object virtual_bytes -Maximum).Maximum
            peak_private_bytes=($memory.ToArray() | Measure-Object private_bytes -Maximum).Maximum
            smallest_free_block_mib=($memory.ToArray() | Measure-Object largest_free_mib -Minimum).Minimum
            login_to_world_ms=$worldMs
        } 'summary.json'
        foreach ($name in 'threads','profile_result','io_stats','crash_list') {
            try { Save-Json (Call-Dev $name) "$name.json" } catch { Write-Warning $_.Exception.Message }
        }
        Save-Json $threadsBefore 'threads-before.json'
    }
}
function Measure-StockRoute {
    Write-TraceMarker 'route-setup'
    $cpuBefore = @(Get-CpuSnapshot)
    $start = [DateTime]::UtcNow
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $memory = New-Object 'Collections.Generic.List[object]'
    $frames = New-Object 'Collections.Generic.List[object]'
    $complete = $false
    $tracing = $false
    $dropped = $null
    $ioCaptured = $false
    try {
        Call-Stock 'Fprofile' | Out-Null
        Call-Stock 'Rstart frame' | Out-Null
        $tracing = $true
        $direction = 'w'
        while ($clock.Elapsed.TotalSeconds -lt $Seconds) {
            Assert-BotsActive
            Call-Stock "I$direction 2000" | Out-Null
            if ($clock.ElapsedMilliseconds -ge 1000) { Call-Stock 'Ispace 100' | Out-Null }
            $direction = if ($direction -eq 'w') { 's' } else { 'w' }
            $gameClient.Refresh()
            $memory.Add([pscustomobject]@{
                utc=[DateTime]::UtcNow.ToString('o'); private_bytes=$gameClient.PrivateMemorySize64
                virtual_bytes=$gameClient.VirtualMemorySize64; handles=$gameClient.HandleCount
            })
            $remaining = $Seconds * 1000 - [int]$clock.ElapsedMilliseconds
            if ($remaining -gt 0) { Start-Sleep -Milliseconds ([Math]::Min(2000, $remaining)) }
            Call-Stock 'Irelease' | Out-Null
            foreach ($row in ((Call-Stock 'Rdrain') -split "`n")) {
                if ($row.Trim()) { $frames.Add(($row | ConvertFrom-Json)) }
            }
        }
        Assert-BotsActive
        $complete = $true
    } finally {
        Write-TraceMarker 'route-ended'
        if (-not $gameClient.HasExited) {
            if ($tracing) {
                try {
                    $traceStatus = (Call-Stock 'Rstop') | ConvertFrom-Json
                    $dropped = $traceStatus.dropped
                    foreach ($row in ((Call-Stock 'Rdrain') -split "`n")) {
                        if ($row.Trim()) { $frames.Add(($row | ConvertFrom-Json)) }
                    }
                    Save-Json $traceStatus 'stock-trace-status.json'
                } catch { Write-Warning $_.Exception.Message }
            }
            try {
                Save-Json ((Call-Stock 'Foff') | ConvertFrom-Json) 'io_stats.json'
                Call-Stock 'Ftrace' | Set-Content "$run\stock-pak-io.csv"
                $ioCaptured = $true
                Call-Stock 'Irelease' | Out-Null
            } catch { Write-Warning $_.Exception.Message }
        }
        $frames.ToArray() | Select-Object frame,wall_us,@{n='frame_ms';e={$_.dt_ms}} |
            Export-Csv "$run\frames.tsv" -Delimiter "`t" -NoTypeInformation
        $hitches = @($frames.ToArray() | Where-Object dt_ms -GT 50)
        $hitches | Select-Object frame,wall_us,@{n='frame_ms';e={$_.dt_ms}} |
            Export-Csv "$run\hitches.tsv" -Delimiter "`t" -NoTypeInformation
        $memory.ToArray() | Export-Csv "$run\memory.tsv" -Delimiter "`t" -NoTypeInformation
        Save-Json @{before=$cpuBefore; after=@(Get-CpuSnapshot)} 'process-cpu.json'
        $sorted = @($frames.ToArray().dt_ms | Sort-Object)
        Save-Json @{
            config=$Config; scenario=$Scenario; preset=$Preset; bots=$n; start_utc=$start.ToString('o')
            complete=($complete -and $null -ne $dropped -and $ioCaptured); frames=$frames.Count; missed_frames=$dropped
            p50_ms=(Percentile $sorted 0.50); p95_ms=(Percentile $sorted 0.95)
            p99_ms=(Percentile $sorted 0.99); max_ms=(Percentile $sorted 1.0)
            hitches_over_50_ms=$hitches.Count; login_to_world_ms=$worldMs
            peak_virtual_bytes=($memory.ToArray() | Measure-Object virtual_bytes -Maximum).Maximum
            peak_private_bytes=($memory.ToArray() | Measure-Object private_bytes -Maximum).Maximum
        } 'summary.json'
    }
}

if (-not $lease.WaitOne(0)) { $lease.Dispose(); throw 'Another benchmark owns the port block.' }
try {
    Assert-Idle
    $launcherFiles = if ($Config -eq 'stock') { @("$Target\aaemu-launcher.exe", $StockDriver) }
        else { @("$dev\bin\aadev-launch.exe") }
    foreach ($file in (@("$Target\aaemu-game.exe","$Target\aaemu-login.exe","$Target\aaemu-testclient.exe") + $launcherFiles)) {
        if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "Missing required executable $file" }
    }
    if (-not (Test-Path -LiteralPath $client)) {
        Copy-Tree $source $client @('/XF','game_pak')
    }
    if (-not (Test-Path -LiteralPath "$client\game_pak")) {
        New-Item -ItemType HardLink -Path "$client\game_pak" -Target "$source\game_pak" | Out-Null
    }
    $archive = Get-Item -LiteralPath "$client\game_pak"
    if ($archive.LinkType -ne 'HardLink' -or $archive.Target -notcontains "$source\game_pak") {
        throw 'Expected a hard link to the original r208022 game_pak'
    }
    $n = if ($Scenario -eq 'farming') { 0 } else { $Bots }
    $series = [Guid]::NewGuid().ToString('N').Substring(0,12)
    $results = New-Object 'Collections.Generic.List[object]'
    for ($rep=1; $rep -le $Repeat; $rep++) {
        Assert-Idle
        $stamp = [Guid]::NewGuid().ToString('N').Substring(0,12)
        $run = (New-Item -ItemType Directory -Path (Join-Path $Output "$Config-$Scenario-$Preset-$stamp") -Force).FullName
        $runtime = (New-Item -ItemType Directory -Path (Join-Path $run 'runtime')).FullName
        $owned = New-Object 'Collections.Generic.List[Diagnostics.Process]'
        $db = $null
        $gameClient = $null
        $worldMs = $null
        $stockStartedUtc = $null
        $farmApiKey = [Guid]::NewGuid().ToString('N')
        $script:traceStarted = $false
        $ini = "$runtime\my.ini"
        try {
            Copy-Tree "$audit\mysql\data" "$runtime\mysql" @('/XF','*.pid','*.sock')
            Copy-Tree "$audit\rs-login" "$runtime\login" @('/XD','Logs')
            Copy-Tree "$audit\rs-game" "$runtime\game" @('/XD','Logs')
            $root = $runtime.Replace('\','/')
            $iniText = (Get-Content "$audit\mysql\my.ini" -Raw) -replace 'C:/aa-re/lanes/live-audit/mysql/data',"$root/mysql" -replace 'C:/aa-re/lanes/live-audit/logs',"$root" -replace 'C:/aa-re/lanes/live-audit/mysql',"$root"
            $iniText | Set-Content -LiteralPath $ini -Encoding ASCII
            foreach ($server in 'login','game') {
                $path = "$runtime\$server\Config.Local.json"
                $local = Get-Content $path -Raw | ConvertFrom-Json
                $local.Network.NumConnections = 1200
                if ($server -eq 'game') {
                    $local | Add-Member -Force NoteProperty Account @{AccessLevelDefault=100}
                    $local.ClientData.Sources = @("$client\game_pak")
                    if ($Scenario -eq 'farming') {
                        $local.WebApiNetwork | Add-Member -Force NoteProperty ApiKey $farmApiKey
                    }
                } else {
                    $local | Add-Member -Force NoteProperty AutoAccount $true
                }
                [IO.File]::WriteAllText($path, ($local | ConvertTo-Json -Depth 30))
            }
            $db = Start-Owned mysql "$mysql\mysqld.exe" $root @("--defaults-file=`"$ini`"")
            Wait-Port 3417 $db 90
            $login = Start-Owned login "$Target\aaemu-login.exe" "$runtime\login"
            Wait-Port 28537 $login 60
            Wait-Port 28534 $login 60
            $game = Start-Owned game "$Target\aaemu-game.exe" "$runtime\game" @("`"$runtime\game`"")
            Wait-Port 28539 $game 300
            Wait-Port 28550 $game 60
            Wait-Port 28580 $game 60

            $env:AAEMU_STRESS_PASSWORD = [Guid]::NewGuid().ToString('N')
            $user = "perf$stamp"
            $fixture = "$runtime\observer.json"
            [IO.File]::WriteAllText($fixture, (@{
                login='127.0.0.1:28537';user=$user;password=$env:AAEMU_STRESS_PASSWORD
                character='Perfobserver'
            } | ConvertTo-Json))
            $observer = Start-Owned observer "$Target\aaemu-testclient.exe" $root @(
                'run','--config',"`"$fixture`"",'--scenario','lobby'
            )
            if (-not $observer.WaitForExit(60000) -or
                -not (Select-String -LiteralPath "$run\observer.out" -Pattern '^\[run\] ok\s+game.quit\s' -Quiet)) {
                throw 'Observer setup failed, see observer.out and observer.err'
            }
            if ($n) {
                $botConfig = "$runtime\bot.json"
                [IO.File]::WriteAllText($botConfig, (@{
                    login='127.0.0.1:28537';user='unused';password=$env:AAEMU_STRESS_PASSWORD
                    character='unused';busy_center=$center;busy_secs=($Seconds + 420)
                    busy_skip_boat=($Scenario -ne 'water')
                } | ConvertTo-Json))
                $botsProcess = Start-Owned bots "$Target\aaemu-testclient.exe" $root @(
                    'load','--config',"`"$botConfig`"",'--prefix','perfbot','--bots',$n,'--ramp-ms','50','--scenario','busy'
                )
                $deadline = [DateTime]::UtcNow.AddSeconds(240)
                do {
                    Assert-BotsActive
                    $ready = @(Select-String -LiteralPath "$run\bots.out" -Pattern '^busy ready perfbot[0-9]+$').Count
                    if ($ready -eq $n) { break }
                    Start-Sleep -Milliseconds 500
                } while ([DateTime]::UtcNow -lt $deadline)
                if ($ready -ne $n) { throw "Only $ready of $n bots became ready." }
            }
            $loginStart = [Diagnostics.Stopwatch]::StartNew()
            if ($Config -eq 'stock') {
                $passwordFile = "$runtime\stock-password.txt"
                [IO.File]::WriteAllText($passwordFile, $env:AAEMU_STRESS_PASSWORD)
                $stockStartedUtc = [DateTime]::UtcNow
                $launcher = Start-Owned launcher "$Target\aaemu-launcher.exe" "$client\bin32" @(
                    '--client',"`"$client\bin32\archeage.exe`"",'--driver-dll',"`"$StockDriver`"",
                    '--login','127.0.0.1:28537','--user',$user,'--password-file',"`"$passwordFile`"",
                    '--no-compressed-pak','--multi-client','--arg','-aaemu-hidden',
                    '--arg','+r_driver','--arg','DX10','--arg','+r_width','--arg','1024',
                    '--arg','+r_height','--arg','768','--arg','+r_fullscreen','--arg','0',
                    '--arg','+r_vsync','--arg','0','--arg','+sys_spec_full','--arg',$spec
                )
                $pidDeadline = [Diagnostics.Stopwatch]::StartNew()
                do {
                    $line = Select-String -LiteralPath "$run\launcher.out" -Pattern '^client started \(pid (\d+)\)' |
                        Select-Object -First 1
                    if ($line) { break }
                    if ($launcher.HasExited) { throw 'Stock launcher exited before starting the client.' }
                    Start-Sleep -Milliseconds 100
                } while ($pidDeadline.Elapsed.TotalSeconds -lt 30)
                if (-not $line) { throw 'Stock launcher did not report a client PID.' }
                $gameClient = Get-Process -Id ([int]$line.Matches[0].Groups[1].Value)
                if ($gameClient.Path -ne "$client\bin32\archeage.exe") { throw 'Unexpected stock client path.' }
                $owned.Add($gameClient)
                Wait-Dev 'stock world list' 100 { (Lua 'return X2LoginCharacter:GetCurrentStage()') -eq '3' }
                Lua 'X2World:EnterWorld(1,0)' | Out-Null
                Wait-Dev 'stock character lobby' 60 { (Lua 'return X2LoginCharacter:GetCurrentStage()') -eq '4' }
                $slot = Lua "for i=1,X2LoginCharacter:GetNumLoginCharacters() do if X2LoginCharacter:GetLoginCharacterName(i)=='Perfobserver' then return i end end"
                if ($slot -notmatch '^\d+$') { throw 'Stock observer character was missing.' }
                Lua "X2LoginCharacter:SelectCharacter($slot)" | Out-Null
                Lua "characterSelectWindow.bottomPanel.startButton:OnClick('LeftButton')" | Out-Null
                Wait-Dev 'stock world entry' 100 { (Lua "return X2:IsEnteredWorld() and X2Unit:UnitName('player')=='Perfobserver'") -eq 'true' }
                Call-Stock 'P' | Set-Content "$run\stock-driver.txt"
                try {
                    Call-Stock 'Sreturn System.GetCVar("r_Width"),System.GetCVar("r_Height"),System.GetCVar("r_VSync"),System.GetCVar("sys_spec_full")' |
                        Set-Content "$run\stock-graphics.txt"
                } catch { Write-Warning "Stock graphics query failed. $($_.Exception.Message)" }
            } else {
                $devLaunch = Start-Owned launcher "$dev\bin\aadev-launch.exe" $dev @(
                    '--exe',"`"$client\bin32\archeage.exe`"",'--user',$user,'--password-env','AAEMU_STRESS_PASSWORD',
                    '--ip','127.0.0.1','--port','28537','--headless','--windowed','1024x768','--spec',$spec,
                    '--vsync','0','--profile',"bench-$stamp",'--tag',"bench-$stamp",'--set','fps_cap=0',
                    '--set','io_trace=1','--run-dir',"`"$run\client`"",'--json'
                )
                if (-not $devLaunch.WaitForExit(60000)) { throw 'Dev launcher timed out, see launcher.err' }
                $launch = Get-Content "$run\launcher.out" -Raw | ConvertFrom-Json
                if (-not $launch.ok -or -not $launch.pid) { throw 'Dev launcher failed, see launcher.err' }
                $gameClient = Get-Process -Id $launch.pid
                if ($gameClient.Path -ne "$client\bin32\archeage.exe") {
                    throw "Launcher returned an unexpected client path"
                }
                $owned.Add($gameClient)
                $clientLog = Join-Path $launch.user_dir 'ArcheAge\ArcheAge.log'
                Wait-Dev 'login screen' 100 {
                    (Call-Dev status).engine.ready -and (
                        ((Test-Path -LiteralPath $clientLog) -and
                            (Select-String -LiteralPath $clientLog -Pattern 'Auth: client connected to server' -Quiet)) -or
                        ((Test-Path -LiteralPath "$run\client\aadev.log") -and
                            (Select-String -LiteralPath "$run\client\aadev.log" -Pattern 'intro skip finished .*\(timeout\)' -Quiet))
                    )
                }
                Start-Sleep -Seconds 2
                foreach ($point in @(@(340,237),@(512,535))) {
                    Call-Dev input_mouse @{x=$point[0];y=$point[1];action='click';button='left'} | Out-Null
                    Start-Sleep -Seconds 3
                }
                Wait-Dev 'character lobby' 60 {
                    (Test-Path -LiteralPath $clientLog) -and
                        (Select-String -LiteralPath $clientLog -Pattern 'stream: authorized by world server' -Quiet)
                }
                Start-Sleep -Seconds 3
                Call-Dev input_mouse @{x=948;y=749;action='click';button='left'} | Out-Null
                Wait-Dev 'world entry' 100 { (Lua 'return X2:IsEnteredWorld()') -eq 'true' }
            }
            $worldMs = $loginStart.ElapsedMilliseconds
            if ($Scenario -eq 'farming') { Seed-Farm }
            if ($TraceEtw) {
                $traceName = "AAEmuStress-$stamp"
                $startTrace = & wpr.exe -start CPU -start FileIO -start GPU -filemode -instancename $traceName
                $traceExit = $LASTEXITCODE
                $script:traceStarted = $traceExit -eq 0
                $startTrace | Set-Content "$run\wpr-start.txt"
                if ($traceExit -ne 0) { throw "WPR start failed ($traceExit), see wpr-start.txt" }
                "utc`tevent" | Set-Content "$run\trace-markers.tsv"
                Write-TraceMarker "client-pid=$($gameClient.Id)"
            }
            Write-TraceMarker 'scene-teleport-request'
            Send-Gm "/move $($center -join ' ')"
            Wait-Dev 'scene entry' 30 {
                $position = Lua "local id=X2Unit:GetUnitId('player'); local x,y,z=X2Unit:GetUnitWorldPosition(id); return string.format('%.2f %.2f %.2f',x,y,z)"
                $position | Set-Content "$run\scene-position.txt"
                $coords = $position.Trim('"') -split ' '
                if ($coords.Count -ne 3) { return $false }
                $x = [double]$coords[0]
                $y = [double]$coords[1]
                $dx = $center[0] - $x
                $dy = $center[1] - $y
                ([Math]::Abs($dx) -lt 5 -and [Math]::Abs($dy) -lt 5) -or
                    ([Math]::Abs($localCenter[0] - $x) -lt 5 -and [Math]::Abs($localCenter[1] - $y) -lt 5)
            }
            Write-TraceMarker 'scene-position-confirmed'
            if ($Scenario -eq 'farming') {
                try {
                    $doodads = Get-Content "$run\farm-doodads.json" -Raw | ConvertFrom-Json
                    $ids = $doodads.id -join ','
                    $recognized = Lua "local n=0; for _,id in ipairs({$ids}) do local ok,v=pcall(function() return X2Unit:GetDoodadInfoById(id) end); if ok and v then n=n+1 end end; return n"
                    $recognized | Set-Content "$run\farm-client-recognized.txt"
                    if ($Config -eq 'stock') {
                        Call-Stock 'C' "$run\farm-scene.png"
                    } else {
                        Save-Json (Call-Dev screenshot @{
                            format='png';inline=$false;path="$run\farm-scene.png"
                        }) 'farm-scene.json'
                    }
                } catch {
                    Write-Warning "Farm scene screenshot failed: $($_.Exception.Message)"
                }
            }
            if ($Config -eq 'stock') { Measure-StockRoute } else { Measure-Route }
            $result = Get-Content "$run\summary.json" -Raw | ConvertFrom-Json
            if (-not $result.complete -or -not $result.frames -or $result.missed_frames) {
                throw 'The route did not produce a complete frame capture. See summary.json.'
            }
            @("# $Config / $Scenario / $Preset", '', "Bots ready $n. Frames $($result.frames), missed $($result.missed_frames).",
                "Present p50 $($result.p50_ms) ms, p95 $($result.p95_ms) ms, p99 $($result.p99_ms) ms, max $($result.max_ms) ms.") |
                Set-Content "$run\README.md" -Encoding UTF8
        } catch {
            $failure = $_.Exception.Message
            $failure | Set-Content "$run\failure.txt"
            Write-Warning "Attempt $rep of ${Repeat}: $failure"
            if (-not $worldMs -and $gameClient -and $owned.Contains($gameClient) -and -not $gameClient.HasExited) {
                try {
                    if ($Config -eq 'stock') {
                        Call-Stock 'C' "$run\lobby-failure.png"
                    } else {
                        Call-Dev screenshot @{format='png';inline=$false;path="$run\lobby-failure.png"} | Out-Null
                    }
                } catch { Write-Warning "Lobby screenshot failed. $($_.Exception.Message)" }
            }
        } finally {
            if ($script:traceStarted) {
                try {
                    $stopTrace = & wpr.exe -stop "$run\scene.etl" -instancename $traceName
                    $traceExit = $LASTEXITCODE
                    if ($traceExit -ne 0) { throw "WPR stop failed ($traceExit), see wpr-stop.txt" }
                    $stopTrace | Set-Content "$run\wpr-stop.txt"
                } catch {
                    $traceFailure = $_.Exception.Message
                    $failure = (@($failure, $traceFailure) | Where-Object { $_ }) -join ' '
                    try {
                        $failure | Set-Content "$run\failure.txt"
                        $stopTrace | Set-Content "$run\wpr-stop.txt"
                    } catch {
                        Write-Warning $_.Exception.Message
                    }
                    try { & wpr.exe -cancel -instancename $traceName | Out-Null }
                    catch { Write-Warning $_.Exception.Message }
                }
                $script:traceStarted = $false
            }
            Remove-Item Env:AAEMU_STRESS_PASSWORD -ErrorAction SilentlyContinue
            for ($i=$owned.Count - 1; $i -ge 0; $i--) {
                $p = $owned[$i]
                if ($db -and $p.Id -eq $db.Id) { Stop-OwnedMySql $ini }
                if (-not $p.HasExited) { $p.Kill(); $p.WaitForExit(15000) | Out-Null }
                $p.Dispose()
            }
            if ($stockStartedUtc) {
                try {
                    $documents = Join-Path ([Environment]::GetFolderPath('MyDocuments')) 'ArcheAge'
                    $artifacts = @(Get-ChildItem -LiteralPath $documents -File -ErrorAction SilentlyContinue |
                        Where-Object { $_.Extension -in '.crash','.dmp' -and $_.LastWriteTimeUtc -ge $stockStartedUtc })
                    if ($artifacts.Count) {
                        $destination = New-Item -ItemType Directory -Path "$run\stock-crash-candidates"
                        foreach ($artifact in $artifacts) {
                            Copy-Item -LiteralPath $artifact.FullName -Destination $destination.FullName
                        }
                    }
                } catch { Write-Warning "Stock crash evidence collection failed. $($_.Exception.Message)" }
            }
            $resolvedRuntime = (Resolve-Path -LiteralPath $runtime).ProviderPath
            if ($resolvedRuntime -eq (Join-Path $run 'runtime') -and
                -not ((Get-Item -LiteralPath $runtime).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                for ($retry=0; $retry -lt 5 -and (Test-Path -LiteralPath $resolvedRuntime); $retry++) {
                    try { Remove-Item -LiteralPath $resolvedRuntime -Recurse -Force }
                    catch {
                        if ($retry -eq 4) { Write-Warning "Could not fully remove the copied runtime: $resolvedRuntime" }
                        else { Start-Sleep -Milliseconds 500 }
                    }
                }
            }
        }
        if (Test-Path -LiteralPath "$run\summary.json") {
            $result = Get-Content "$run\summary.json" -Raw | ConvertFrom-Json
        } else {
            $result = [pscustomobject]@{config=$Config;scenario=$Scenario;preset=$Preset;bots=$n;complete=$false}
        }
        $result | Add-Member -NotePropertyName run -NotePropertyValue (Split-Path -Leaf $run)
        $result | Add-Member -NotePropertyName failure -NotePropertyValue $failure
        $results.Add($result)
        $failure = $null
    }
    $results.ToArray() | Select-Object run,config,scenario,preset,bots,complete,frames,missed_frames,
        p50_ms,p95_ms,p99_ms,max_ms,hitches_over_50_ms,login_to_world_ms,peak_virtual_bytes,peak_private_bytes,
        smallest_free_block_mib,failure |
        Export-Csv (Join-Path $Output "series-$series.tsv") -Delimiter "`t" -NoTypeInformation
    $valid = @($results | Where-Object { $_.complete -and -not $_.failure -and $_.frames -and -not $_.missed_frames })
    $lines = @("# $Config / $Scenario / $Preset", '', "Bots $n. Complete captures $($valid.Count) of $Repeat.", '',
        '| Metric | Median across runs | Min | Max |', '| --- | ---: | ---: | ---: |')
    foreach ($key in 'p50_ms','p95_ms','p99_ms','max_ms','hitches_over_50_ms','login_to_world_ms','peak_virtual_bytes','peak_private_bytes') {
        if ($valid.Count) {
            $samples = @($valid | ForEach-Object { $_.$key } | Sort-Object)
            $median = ($samples[[int][Math]::Floor(($samples.Count - 1) / 2)] + $samples[[int][Math]::Floor($samples.Count / 2)]) / 2
            $lines += "| $key | $median | $($samples[0]) | $($samples[-1]) |"
        }
    }
    $lines | Set-Content (Join-Path $Output "series-$series.md") -Encoding UTF8
    if (-not $valid.Count) { throw "No complete captures in series $series. See the attempt directories." }
} finally {
    $lease.ReleaseMutex()
    $lease.Dispose()
}
