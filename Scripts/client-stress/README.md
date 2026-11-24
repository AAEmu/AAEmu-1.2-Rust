# r208022 client stress benchmark

The current harness exercises an experimental dev-client route. The workload and capture coverage below describe what this draft can measure. See the local benchmark report for measured results and failure logs.

## Run

From the worktree's `rust` directory, build the upstream binaries:

```powershell
$env:CARGO_TARGET_DIR = 'C:\aa-re\lanes\targets\perf-bench'
cargo build -j 4 -p aaemu-game -p aaemu-login -p aaemu-testclient -p aaemu-launcher
```

```powershell
.\Scripts\client-stress\Run-StressBench.ps1 -Config dev -Scenario players -Preset low -Bots 1 -Repeat 1 -Seconds 10
.\Scripts\client-stress\Run-StressBench.ps1 -Config dev -Scenario players -Preset high -Bots 300 -Repeat 3
```

Run the smoke test before the 100, 200, 300 and 500-player sweeps. The script uses Windows PowerShell, the local r208022 dev launcher and its pipe helper, the read-only `C:\AA1.2` install, and the stopped `C:\aa-re\lanes\live-audit` stack. It copies the runtime configuration, data and MySQL files for each repeat, binds to the audit port block, and runs the specified build's binaries. Temporary databases, GM defaults and AutoAccount changes are scoped to the copied runtime. The client copy hard-links only `game_pak`. Keep the original runtime stopped for the duration of the benchmark.

## Scenarios

| Scenario | Current workload |
| --- | --- |
| Players | Fixed scene centered at 15562, 15354, 128. Bots spawn within 25 m of that point, rotate across the four r208022 races and run the existing GM-enabled busy scenario. Mate summon, NPC and doodad spawns, basic casts, buffs, movement and chat reuse the existing scenario. Mixed gear, gliding and player jumping are pending. |
| Housing | `-Houses N` (default 300) finished small Nuian houses (templates 172-181) on an 18 m grid around the scene, six floor decorations each (house-owned doodads), written into the copied database before the game server starts; no bots. Heights use the scene z. |
| Effects | The players crowd plus a firework (`-EffectItem` 15863 / `-EffectSkill` 13392 by default) used by every bot every tick through its item skill. |
| Water | Busy bots at the sea point 15458, 10366, 100.5: every second bot summons a rowboat and steers it in a 12 m circle (`busy_boat_move`), the others swim. Larger ships remain open. |
| Combined | Houses, the farm grid and the effects crowd at the land scene. The route teleports to the sea point at half time and back at three quarters (`transitions.tsv`); afterwards both transitions are timed to the confirmed position (`zone-transition.json`). |
| Farming | Observer requests 200 GM doodad spawns on a grid using seven templates from the r208022 plant scenario. The grid uses default spawn phases. Count and placement require in-world verification. |

## Capture

The dev launcher sets 1024×768, spec 1 or 4, VSync off and the explicit frame cap disabled. The observer enters from the character screen, teleports to the scene, and alternates short forward and backward movements with jumps. The route uses the character's initial camera direction.

Each attempt has a separate result directory. Present frame durations come from the dev DLL's 240-entry ring. A frame counter delta larger than the returned history increments `missed_frames`. Treat runs with gaps or an incomplete route as incomplete captures. Hitch timestamps are estimates in the client uptime domain. The collector anchors each continuous segment to the age of the most recent Present and accumulates the recorded durations from that anchor. `summary.json` reports nearest-rank percentiles across the captured frames. Failed attempts retain partial results and a failure message. The series report lists the attempts and gives the median and range across complete captures. The script retains process CPU counters before and after the route, client thread CPU counters, an asynchronous main-thread sampling profile, game file read statistics, sampled memory and handle counts, and the dev crash summaries. Login timing begins at launcher invocation and ends at the first successful in-world check.

The stock path uses the full native aadriver build. Set `AADRIVER_OUT` to
`C:\aa-re\lanes\targets\perf-bench\aadriver` and run
`rust\crates\aaemu-clientdriver\native\build.bat`, or supply the DLL path
with `-StockDriver`. The stock route uses the original launcher and client,
the driver’s Lua and input commands, frame-only Present records and pak
I/O counters. Its Present records carry Unix microseconds. The stock
client uses the Windows account’s shared ArcheAge Documents directory.
The dev path uses its separate profile. ArcheNova needs an isolated
NativeAbility server and matching character fixture.

The farming fixture uses the isolated server’s authenticated loopback
Web API to place mixed r208022 grids and advance one crop group. It
requires 200 newly observed object IDs. The run retains object IDs and
phases, a client-side recognition count and a screenshot when available.
Automatic hang dumps remain open; stock runs attribute hitches with
`-TraceFrames` and time zone transitions in the combined scene.

### Scene tracing

Add `-TraceEtw` to a dev run from a Windows session authorized to record
kernel profiles. The harness starts WPR CPU, FileIO and GPU profiles before
the scene teleport and saves the ETL locally. Each attempt uses a distinct
WPR instance. An attempted trace that fails to start or stop is reported
separately from a successful trace.

Use `trace-markers.tsv` for the client PID and scene phase boundaries.
Each row in `present-clock.tsv` brackets a stats request with host UTC
timestamps and records the client clock and Present age returned by the
request. To place a hitch in the trace, estimate the client clock offset
from an anchor with a short round trip. The hitch begins one frame interval
before its reported Present timestamp. Check the clock uncertainty against
the hitch duration before interpreting CPU stacks, thread waits, file I/O
or GPU activity.

If observer startup is unreliable with a fully active crowd, use
`-StageObserverFirst` for a separate capture. It opens the observer world
session first, moves the observer away from the scene, waits for the full
fixture, and then takes the usual scene route. The series records the
flag. Launcher-to-world time for a staged run measures an empty fixture
and is recorded separately from crowded startup.

### Stock patch sets and hitch traces

Stock runs copy the driver into each attempt directory (`driver\aadriver.log`). `-Patch` passes a
patch list to `aaemu-launcher --patch` (empty = the driver defaults, `none` = no patch), and
`-LauncherArgs` adds launcher arguments. Every stock run sets `sys_sleep_background=0`: the hidden
window has no focus, and the engine otherwise sleeps 40 ms per frame. `-TraceFrames` adds the
driver's `diag-frame-trace`: main and render frame phases, and main and render thread stack samples
taken only during frames over 50 ms, in `driver\frametrace-<pid>.tsv` and `aadriver.log`.
