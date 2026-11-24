[CmdletBinding()]
param(
    [ValidateSet('stock','dev','archenova')][string]$Config = 'dev',
    [ValidateSet('players','housing','farming','effects','water','combined')][string]$Scenario = 'players',
    [ValidateRange(1,30)][int]$Repeat = 3,
    [ValidateSet('low','high')][string]$Preset = 'low',
    [ValidateSet(1,100,200,300,500)][int]$Bots = 200,
    [ValidateRange(10,300)][int]$Seconds = 30,
    [switch]$TraceEtw,
    [switch]$StageObserverFirst,
    # Paths default to AAEMU_STRESS_* environment variables (see README.md).
    [string]$StockDriver = $env:AAEMU_STRESS_DRIVER,
    # Stock only: the driver's client performance patch list (aaemu-launcher --patch), '' = defaults.
    [string]$Patch = '',
    # Stock only: more aaemu-launcher arguments (for example '--frame-latency','2').
    [string[]]$LauncherArgs = @(),
    # Stock only: add the driver's diag-frame-trace (hitch stack samples, frametrace-<pid>.tsv).
    [switch]$TraceFrames,
    # housing / combined: houses placed in the copied database before the game server starts.
    [ValidateRange(0,1000)][int]$Houses = 300,
    # effects / combined: firework item and its use skill, used by every bot every tick.
    [int]$EffectItem = 15863,
    [int]$EffectSkill = 13392,
    # Cargo output folder holding aaemu-game, aaemu-login and aaemu-testclient.
    [string]$Target = $env:AAEMU_STRESS_TARGET,
    # Results folder (default: runs\ next to this script).
    [string]$Output = $env:AAEMU_STRESS_OUTPUT,
    # Stopped AAEmu runtime (configuration, data, MySQL data folder), copied for every repeat.
    [string]$Runtime = $env:AAEMU_STRESS_RUNTIME,
    # r208022 AAEmu dev client folder (aadev-launch and its pipe helper).
    [string]$DevClient = $env:AAEMU_STRESS_DEVCLIENT,
    # Folder for the per-config client copies (perf-stock, perf-dev).
    [string]$ClientRoot = $env:AAEMU_STRESS_CLIENTS,
    # Read-only r208022 client install the copies are made from (game_pak is hard-linked).
    [string]$SourceClient = $env:AAEMU_STRESS_CLIENT_SOURCE,
    [string]$MySqlBin = $env:AAEMU_STRESS_MYSQL_BIN
)
$ErrorActionPreference = 'Stop'
if ($Config -eq 'archenova') { throw 'ArcheNova requires a dedicated NativeAbility server and client fixture.' }
if (!$Output) { $Output = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) 'runs' }
if (!$MySqlBin) { $MySqlBin = 'C:\Program Files\MySQL\MySQL Server 8.4\bin' }
foreach ($required in 'Target', 'Runtime', 'DevClient', 'ClientRoot', 'SourceClient') {
    if (!(Get-Variable $required -ValueOnly)) { throw "-$required (or its AAEMU_STRESS_* variable) is required; see README.md." }
}
if ($Config -eq 'stock' -and !$StockDriver) { throw '-StockDriver (or AAEMU_STRESS_DRIVER) is required for the stock config.' }

$audit = $Runtime
$dev = $DevClient
$mysql = $MySqlBin
$client = Join-Path $ClientRoot "perf-$Config"
$ports = @(3417,28537,28534,28539,28550,28580)
$source = $SourceClient
$center = if ($Scenario -eq 'water') { @(15458,10366,100.5) } else { @(15562,15354,128.0) }
# r208022's client scene uses a cell-relative origin. The land route showed
# (4298, 4090) after a server teleport to (15562, 15354).
$localCenter = if ($Scenario -eq 'water') { @(4194,3198) } else { @(4298,4090) }
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
function Start-BotFixture($Root) {
    $botConfig = "$runtime\bot.json"
    $bot = @{
        login='127.0.0.1:28537';user='unused';password=$env:AAEMU_STRESS_PASSWORD
        character='unused';busy_center=$center;busy_secs=($Seconds + 420)
        busy_skip_boat=($Scenario -ne 'water')
    }
    if ($Scenario -eq 'water') {
        # Half the crowd swims at the sea point, the other half circles in summoned rowboats.
        $bot.busy_boat_every = 2; $bot.busy_boat_move = $true
    }
    if ($Scenario -in 'effects','combined') { $bot.busy_effect_item = $EffectItem; $bot.busy_effect_skill = $EffectSkill }
    [IO.File]::WriteAllText($botConfig, ($bot | ConvertTo-Json))
    $botsProcess = Start-Owned bots "$Target\aaemu-testclient.exe" $Root @(
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
    $botsProcess
}

# Housing fixture: finished r208022 small Nuian houses (templates 172-181, 3 build steps, garden
# radius 7.5 m) on an 18 m grid around the scene, each with six floor decorations (vase, bonsai,
# two tables, two chairs) as house-owned doodads. Written into the copied database before the game
# server starts, which loads them like player housing (LoadPlayerHousing + persistent doodads).
# The height is the scene's z; uneven ground leaves some houses slightly sunk or raised.
function Seed-HousingDb([int]$Count) {
    if (-not $Count) { return }
    $gameDb = (Get-Content "$runtime\game\Config.Local.json" -Raw | ConvertFrom-Json).Connections.MySQLProvider.Database
    $decor = @(5450,5454,5462,5464,5483,5487)
    $cols = [Math]::Ceiling([Math]::Sqrt($Count))
    $sql = New-Object Text.StringBuilder
    [void]$sql.AppendLine("USE ``$gameDb``;")
    $now = [DateTime]::UtcNow.ToString('yyyy-MM-dd HH:mm:ss')
    $until = [DateTime]::UtcNow.AddDays(30).ToString('yyyy-MM-dd HH:mm:ss')
    for ($i = 0; $i -lt $Count; $i++) {
        $gx = $i % $cols; $gy = [Math]::Floor($i / $cols)
        # skip the 40 m square around the observer's route
        $x = $center[0] + 18 * ($gx - [Math]::Floor($cols / 2)); $y = $center[1] + 18 * ($gy - [Math]::Floor($cols / 2))
        if ([Math]::Abs($x - $center[0]) -lt 20 -and [Math]::Abs($y - $center[1]) -lt 20) { $x += 18 * $cols }
        $id = 50000 + $i
        [void]$sql.AppendLine("INSERT INTO housings (id,account_id,owner,co_owner,template_id,name,x,y,z,yaw,pitch,roll,current_step,current_action,permission,place_date,protected_until,faction_id,sell_to,sell_price,allow_recover) VALUES ($id,0,0,0,$(172 + $i % 10),'Perf house $i',$x,$y,$($center[2]),0,0,0,-1,0,2,'$now','$until',2,0,0,0);")
        for ($k = 0; $k -lt $decor.Count; $k++) {
            $dx = 2.0 * (($k % 3) - 1); $dy = 2.0 * ([Math]::Floor($k / 3) - 0.5)
            [void]$sql.AppendLine("INSERT INTO doodads (owner_id,owner_type,attach_point,template_id,current_phase_id,plant_time,growth_time,phase_time,x,y,z,roll,pitch,yaw,scale,item_id,house_id,parent_doodad,item_template_id,item_container_id,data,farm_type) VALUES ($id,3,0,$($decor[$k]),0,'$now','$now','$now',$($x + $dx),$($y + $dy),$($center[2] + 0.3),0,0,0,1,0,$id,0,0,0,0,0);")
        }
    }
    $file = "$runtime\housing-fixture.sql"
    [IO.File]::WriteAllText($file, $sql.ToString())
    $p = Start-Process "$mysql\mysql.exe" -ArgumentList '--host=127.0.0.1','--port=3417','--user=root' -RedirectStandardInput $file `
        -RedirectStandardOutput "$run\housing-seed.out" -RedirectStandardError "$run\housing-seed.err" -WindowStyle Hidden -PassThru -Wait
    if ($p.ExitCode) { throw "Housing fixture insert failed, see housing-seed.err" }
    "$Count houses, $($Count * $decor.Count) decorations" | Set-Content "$run\housing-fixture.txt"
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
    # combined: the house grid blocks part of the farm grid; accept at least half
    if (($Scenario -ne 'combined' -and $spawned.Count -ne 200) -or $spawned.Count -lt 100) {
        throw "Farm fixture expected 200 new doodads and found $($spawned.Count). See farm-doodads.json."
    }
    $cropPhases = @($spawned | Where-Object template -EQ $templates[0] | Group-Object phase)
    if ($cropPhases.Count -lt 2 -and $Scenario -ne 'combined') {
        throw 'Farm fixture did not produce mixed growth stages. See farm-doodads.json.'
    }
}

# combined: a zone transition inside the measured route (to the sea point at half time, back at 3/4).
function Invoke-RouteTransition($Clock) {
    if ($Scenario -ne 'combined') { return }
    $t = $Clock.Elapsed.TotalSeconds
    if (-not $script:awaySent -and $t -ge $Seconds * 0.5) {
        Send-Gm '/move 15458 10366 100.5'; $script:awaySent = $true
        "$([DateTime]::UtcNow.ToString('o'))`taway 15458 10366" | Add-Content "$run\transitions.tsv"
    } elseif ($script:awaySent -and -not $script:backSent -and $t -ge $Seconds * 0.75) {
        Send-Gm "/move $($center -join ' ')"; $script:backSent = $true
        "$([DateTime]::UtcNow.ToString('o'))`tback $($center[0]) $($center[1])" | Add-Content "$run\transitions.tsv"
    }
}
# combined: time from a teleport request to the observer standing at the target (zone change and back).
function Measure-ZoneTransition($To, $LocalTo) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    Send-Gm "/move $($To -join ' ')"
    Wait-Dev 'zone transition' 60 {
        $p = (Lua "local id=X2Unit:GetUnitId('player'); local x,y,z=X2Unit:GetUnitWorldPosition(id); return string.format('%.2f %.2f',x,y)").Trim('"') -split ' '
        $p.Count -eq 2 -and (([Math]::Abs($To[0] - [double]$p[0]) -lt 5 -and [Math]::Abs($To[1] - [double]$p[1]) -lt 5) -or
            ([Math]::Abs($LocalTo[0] - [double]$p[0]) -lt 5 -and [Math]::Abs($LocalTo[1] - [double]$p[1]) -lt 5))
    }
    $sw.ElapsedMilliseconds
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
        $script:awaySent = $false; $script:backSent = $false
        while ($clock.Elapsed.TotalSeconds -lt $Seconds) {
            Invoke-RouteTransition $clock
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
            staged_observer=[bool]$StageObserverFirst
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
        $script:awaySent = $false; $script:backSent = $false
        while ($clock.Elapsed.TotalSeconds -lt $Seconds) {
            Invoke-RouteTransition $clock
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
            staged_observer=[bool]$StageObserverFirst
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
    $n = if ($Scenario -in 'farming','housing') { 0 } else { $Bots }
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
            # The runtime's my.ini names its own folders (forward slashes); point them at the copy.
            $auditRoot = [regex]::Escape(((Resolve-Path -LiteralPath $audit).Path.TrimEnd('\')).Replace('\','/'))
            $iniText = (Get-Content "$audit\mysql\my.ini" -Raw) -replace "(?i)$auditRoot/mysql/data","$root/mysql" -replace "(?i)$auditRoot/logs","$root" -replace "(?i)$auditRoot/mysql","$root"
            $iniText | Set-Content -LiteralPath $ini -Encoding ASCII
            foreach ($server in 'login','game') {
                $path = "$runtime\$server\Config.Local.json"
                $local = Get-Content $path -Raw | ConvertFrom-Json
                $local.Network.NumConnections = 1200
                if ($server -eq 'game') {
                    $local | Add-Member -Force NoteProperty Account @{AccessLevelDefault=100}
                    $local.ClientData.Sources = @("$client\game_pak")
                    if ($Scenario -in 'farming','combined') {
                        $local.WebApiNetwork | Add-Member -Force NoteProperty ApiKey $farmApiKey
                    }
                } else {
                    $local | Add-Member -Force NoteProperty AutoAccount $true
                }
                [IO.File]::WriteAllText($path, ($local | ConvertTo-Json -Depth 30))
            }
            $db = Start-Owned mysql "$mysql\mysqld.exe" $root @("--defaults-file=`"$ini`"")
            Wait-Port 3417 $db 90
            if ($Scenario -in 'housing','combined') { Seed-HousingDb $Houses }
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
            if ($n -and -not $StageObserverFirst) { $botsProcess = Start-BotFixture $root }
            $loginStart = [Diagnostics.Stopwatch]::StartNew()
            if ($Config -eq 'stock') {
                $passwordFile = "$runtime\stock-password.txt"
                [IO.File]::WriteAllText($passwordFile, $env:AAEMU_STRESS_PASSWORD)
                $stockStartedUtc = [DateTime]::UtcNow
                # A per-run copy of the driver: aadriver.log and frametrace-<pid>.tsv land next to it.
                $runDriver = (New-Item -ItemType Directory -Path "$run\driver" -Force).FullName
                Copy-Item -LiteralPath $StockDriver -Destination "$runDriver\aadriver.dll"
                Copy-Item -LiteralPath (Join-Path (Split-Path $StockDriver) 'aainject.exe') -Destination "$runDriver\aainject.exe"
                $patchList = @($Patch) + $(if ($TraceFrames) { '+diag-frame-trace' }) | Where-Object { $_ }
                $stockArgs = @(
                    '--client',"`"$client\bin32\archeage.exe`"",'--driver-dll',"`"$runDriver\aadriver.dll`"",
                    '--login','127.0.0.1:28537','--user',$user,'--password-file',"`"$passwordFile`"",
                    '--no-compressed-pak','--multi-client','--arg','-aaemu-hidden',
                    '--arg','+r_driver','--arg','DX10','--arg','+r_width','--arg','1024',
                    '--arg','+r_height','--arg','768','--arg','+r_fullscreen','--arg','0',
                    '--arg','+r_vsync','--arg','0','--arg','+sys_spec_full','--arg',$spec,
                    # The hidden window is unfocused: without this the engine sleeps 40 ms per frame.
                    '--cvar','sys_sleep_background=0'
                )
                if ($patchList) { $stockArgs += @('--patch', ($patchList -join ',')) }
                $launcher = Start-Owned launcher "$Target\aaemu-launcher.exe" "$client\bin32" ($stockArgs + @($LauncherArgs | Where-Object { $_ }))
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
            if ($n -and $StageObserverFirst) {
                # Hold the observer outside the crowd's visibility radius while the bots join.
                Send-Gm '/move 11041 15622 246'
                Start-Sleep -Seconds 3
                $botsProcess = Start-BotFixture $root
            }
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
            if ($Scenario -in 'farming','combined') {
                Write-TraceMarker 'farm-fixture-start'
                Seed-Farm
            } else {
                Write-TraceMarker 'scene-teleport-request'
                Send-Gm "/move $($center -join ' ')"
            }
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
            if ($Config -eq 'stock') { Measure-StockRoute; try { Call-Stock 'Q' | Set-Content "$run\perf-status.txt" } catch {} } else { Measure-Route }
            if ($Scenario -notin 'farming','combined') {
                try {
                    if ($Config -eq 'stock') { Call-Stock 'C' "$run\scene.png" | Out-Null }
                    else { Save-Json (Call-Dev screenshot @{format='png';inline=$false;path="$run\scene.png"}) 'scene.json' }
                } catch { Write-Warning "Scene screenshot failed: $($_.Exception.Message)" }
            }
            if ($Scenario -eq 'combined') {
                try {
                    $away = Measure-ZoneTransition @(15458,10366,100.5) @(4194,3198)
                    $back = Measure-ZoneTransition $center $localCenter
                    Save-Json @{away_ms=$away; back_ms=$back} 'zone-transition.json'
                } catch { Write-Warning "Zone transition timing failed. $($_.Exception.Message)" }
            }
            if ($Scenario -in 'farming','combined') {
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
            $result = [pscustomobject]@{config=$Config;scenario=$Scenario;preset=$Preset;bots=$n;staged_observer=[bool]$StageObserverFirst;complete=$false}
        }
        $result | Add-Member -NotePropertyName run -NotePropertyValue (Split-Path -Leaf $run)
        $result | Add-Member -NotePropertyName failure -NotePropertyValue $failure
        $results.Add($result)
        $failure = $null
    }
    $results.ToArray() | Select-Object run,config,scenario,preset,bots,staged_observer,complete,frames,missed_frames,
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
