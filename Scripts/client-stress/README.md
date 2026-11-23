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
| Housing | Pending a verified fixture for several hundred varied houses with furnishings. |
| Farming | Observer requests 200 GM doodad spawns on a grid using seven templates from the r208022 plant scenario. The grid uses default spawn phases. Count and placement require in-world verification. |
| Effects | Pending validated AoE, siege and consumable effects. The players workload includes basic skills and buffs. |
| Water | Runs the busy bots around the existing sea point at 15458, 10366, 100.5, with rowboat summons. Boat navigation, larger ships and swimming are pending. |
| Combined | Pending the housing, effect and water fixtures and a validated zone boundary route. |

## Capture

The dev launcher sets 1024×768, spec 1 or 4, VSync off and the explicit frame cap disabled. The observer enters from the character screen, teleports to the scene, and alternates short forward and backward movements with jumps. The route uses the character's initial camera direction.

Each attempt has a separate result directory. Present frame durations come from the dev DLL's 240-entry ring. A frame counter delta larger than the returned history increments `missed_frames`. Treat runs with gaps or an incomplete route as incomplete captures. Hitch timestamps are reconstructed in the client uptime domain using the reported age of the most recent Present. `summary.json` reports nearest-rank percentiles across the captured frames. The series report gives the median and range across complete captures. The script retains process CPU counters before and after the route, client thread CPU counters, an asynchronous main-thread sampling profile, game file read statistics, sampled memory and handle counts, and the dev crash summaries. Login timing begins at launcher invocation and ends at the first successful in-world check.

The stock launcher path still needs an automated observer route and a passive D3D Present and I/O collector. ArcheNova additionally needs an isolated revision-compatible NativeAbility game server and an account with the matching character fixture. Main-thread aggregate sampling cannot assign an individual hitch to another thread. Per-hitch attribution, automated hang dumps and zone transition timing remain open.
