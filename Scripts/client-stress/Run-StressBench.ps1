[CmdletBinding()]
param(
    [ValidateSet('stock','dev','archenova')][string]$Config = 'dev',
    [ValidateSet('players','housing','farming','effects','water','combined')][string]$Scenario = 'players',
    [ValidateRange(1,30)][int]$Repeat = 3,
    [ValidateSet('low','high')][string]$Preset = 'low',
    [ValidateSet(1,100,200,300,500)][int]$Bots = 200,
    [ValidateRange(10,300)][int]$Seconds = 30,
    [string]$Target = 'C:\aa-re\lanes\targets\perf-bench\debug',
    [string]$Output = 'C:\aa-re\lanes\client-perf\bench\runs'
)
$ErrorActionPreference = 'Stop'
if ($Config -eq 'archenova') { throw 'ArcheNova requires a dedicated NativeAbility server and client fixture.' }
if ($Config -eq 'stock') { throw 'Stock route automation and complete Present capture are pending.' }
if ($Scenario -in 'housing','effects','combined') { throw "The $Scenario seed fixture is pending." }

$audit = 'C:\aa-re\lanes\live-audit'
$dev = 'C:\aa-re\devclient-r208022'
$mysql = 'C:\Program Files\MySQL\MySQL Server 8.4\bin'
$client = 'C:\aa-re\testclients\perf-dev'
$ports = @(3417,28537,28534,28539,28550,28580)
$source = 'C:\AA1.2'
$center = if ($Scenario -eq 'water') { @(15458,10366,100.5) } else { @(15562,15354,128.0) }
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
    $json = $Payload | ConvertTo-Json -Compress
    $r = & "$dev\tools\aadev-call.ps1" -ClientPid $gameClient.Id -TimeoutMs 5000 $Command $json | ConvertFrom-Json
    if (-not $r.ok) { throw "$Command failed: $($r.error)" }
    $r
}
function Lua($Code) {
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
function Seed-Farm {
    # Verified r208022 item-spawn templates from aaemu-testclient's plant scenario.
    $templates = @(2274,2259,2756,4867,4891,4594,367)
    for ($i=0; $i -lt 200; $i++) {
        $x = $center[0] + 3 * ($i % 20) - 30
        $y = $center[1] + 3 * [Math]::Floor($i / 20) - 15
        Send-Gm "/move $x $y $($center[2])"
        Start-Sleep -Milliseconds 250
        Send-Gm "/spawn doodad $($templates[$i % $templates.Count])"
        Start-Sleep -Milliseconds 150
    }
}
function Measure-Route {
    $frames = New-Object 'Collections.Generic.List[object]'
    $memory = New-Object 'Collections.Generic.List[object]'
    $gap = 0
    $start = [DateTime]::UtcNow
    $cpuBefore = @(Get-CpuSnapshot)
    $threadsBefore = (Call-Dev threads).threads
    $stats = Call-Dev stats @{history=$true}
    $previous = [long]$stats.gfx.frames
    $nextSample = 0
    $nextStep = 2000
    Call-Dev io_stats @{reset=$true} | Out-Null
    Call-Dev profile_start @{duration_ms=($Seconds * 1000);interval_ms=10} | Out-Null
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $completed = $false
    $movement = 'W'
    try {
        Call-Dev input_key @{key=$movement;action='down'} | Out-Null
        while ($clock.Elapsed.TotalSeconds -lt $Seconds) {
            $stats = Call-Dev stats @{history=$true}
            if ($stats.gfx.width -ne 1024 -or $stats.gfx.height -ne 768) {
                throw "Expected a 1024x768 Present surface, got $($stats.gfx.width)x$($stats.gfx.height)"
            }
            $count = [long]$stats.gfx.frames - $previous
            $history = @($stats.gfx.history)
            $gap += [Math]::Max(0, $count - $history.Count)
            $new = [int][Math]::Min($count, $history.Count)
            $timestamp = [double]$stats.uptime_ms - [double]$stats.gfx.present_age_ms
            $batch = @()
            for ($i = $history.Count - 1; $i -ge $history.Count - $new; $i--) {
                $ms = [double]$history[$i]
                $batch += [pscustomobject]@{
                    frame = [long]$stats.gfx.frames - ($history.Count - 1 - $i)
                    client_uptime_ms = [Math]::Round($timestamp, 3)
                    frame_ms = $ms
                }
                $timestamp -= $ms
            }
            for ($i = $batch.Count - 1; $i -ge 0; $i--) { $frames.Add($batch[$i]) }
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
if (-not $lease.WaitOne(0)) { $lease.Dispose(); throw 'Another benchmark owns the port block.' }
try {
    Assert-Idle
    foreach ($file in @("$Target\aaemu-game.exe","$Target\aaemu-login.exe","$Target\aaemu-testclient.exe","$dev\bin\aadev-launch.exe")) {
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
            Wait-Dev 'lobby' 100 { (Call-Dev status).engine.ready }
            Start-Sleep -Seconds 6
            foreach ($point in @(@(340,237),@(512,535))) {
                Call-Dev input_mouse @{x=$point[0];y=$point[1];action='click';button='left'} | Out-Null
                Start-Sleep -Seconds 3
            }
            $clientLog = Join-Path $launch.user_dir 'ArcheAge\ArcheAge.log'
            Wait-Dev 'character lobby' 60 {
                (Test-Path -LiteralPath $clientLog) -and
                    (Select-String -LiteralPath $clientLog -Pattern 'stream: authorized by world server' -Quiet)
            }
            Start-Sleep -Seconds 3
            Call-Dev input_mouse @{x=948;y=749;action='click';button='left'} | Out-Null
            Wait-Dev 'world entry' 100 { (Lua 'return X2:IsEnteredWorld()') -eq 'true' }
            $worldMs = $loginStart.ElapsedMilliseconds
            if ($Scenario -eq 'farming') { Seed-Farm }
            Send-Gm "/move $($center -join ' ')"
            Wait-Dev 'scene entry' 30 {
                $position = Lua "local id=X2Unit:GetUnitId('player'); local x,y,z=X2Unit:GetUnitWorldPosition(id); return string.format('%.2f %.2f %.2f',x,y,z)"
                $position | Set-Content "$run\scene-position.txt"
                $coords = $position.Trim('"') -split ' '
                if ($coords.Count -ne 3) { return $false }
                $dx = $center[0] - [double]$coords[0]
                $dy = $center[1] - [double]$coords[1]
                ([Math]::Abs($dx) -lt 5 -and [Math]::Abs($dy) -lt 5) -or
                    ([Math]::Abs($dx - 1024) -lt 5 -and [Math]::Abs($dy - 1024) -lt 5)
            }
            Measure-Route
            $result = Get-Content "$run\summary.json" -Raw | ConvertFrom-Json
            $result | Add-Member -NotePropertyName run -NotePropertyValue (Split-Path -Leaf $run)
            $results.Add($result)
            @("# $Config / $Scenario / $Preset", '', "Bots ready $n. Frames $($result.frames), missed $($result.missed_frames).",
                "Present p50 $($result.p50_ms) ms, p95 $($result.p95_ms) ms, p99 $($result.p99_ms) ms, max $($result.max_ms) ms.") |
                Set-Content "$run\README.md" -Encoding UTF8
        } catch {
            $_.Exception.Message | Set-Content "$run\failure.txt"
            throw
        } finally {
            Remove-Item Env:AAEMU_STRESS_PASSWORD -ErrorAction SilentlyContinue
            for ($i=$owned.Count - 1; $i -ge 0; $i--) {
                $p = $owned[$i]
                if ($db -and $p.Id -eq $db.Id) { Stop-OwnedMySql $ini }
                if (-not $p.HasExited) { $p.Kill(); $p.WaitForExit(15000) | Out-Null }
                $p.Dispose()
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
    }
    $results.ToArray() | Export-Csv (Join-Path $Output "series-$series.tsv") -Delimiter "`t" -NoTypeInformation
    $valid = @($results | Where-Object { $_.complete -and $_.frames -and -not $_.missed_frames })
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
} finally {
    $lease.ReleaseMutex()
    $lease.Dispose()
}
