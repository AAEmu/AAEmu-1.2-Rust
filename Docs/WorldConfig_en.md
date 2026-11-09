# `World.json` settings (aaemu-game)

This document describes the settings in `Configurations/World.json` of the game
server's application directory (`rust/crates/aaemu-game/app/` in a source
checkout, `AAEmu.Game/` in a published build, `.server_files/AAEmu.Game/` with
Docker).

## Where to configure

- **File**: `rust/crates/aaemu-game/app/Configurations/World.json`
- **Local override**: put a `World` section in `Config.Local.json` next to
  `Config.json` (it is loaded after `Configurations/*.json`), or use environment
  variables such as `World__ExpRate=2`
- **Section**: `World`

Example:

```json
{
  "World": {
    "MOTD": "Welcome to AAEmu!",
    "TargetPhysicsTps": 25.0
  }
}
```

## Parameters

### `MOTD`
- **Type**: `string`
- **Description**: Message of the Day (shown in chat on login).

### `LogoutMessage`
- **Type**: `string`
- **Description**: Message shown when leaving the game.

### `AutoSaveInterval`
- **Type**: `number`
- **Description**: Auto-save interval (minutes).

### `ExpRate`
- **Type**: `number`
- **Description**: Server-side EXP multiplier.

### `HonorRate`
- **Type**: `number`
- **Description**: Server-side honor points multiplier.

### `VocationRate`
- **Type**: `number`
- **Description**: Server-side vocation badge / vocation points multiplier.

### `LootRate`
- **Type**: `number`
- **Description**: Loot dice multiplier (not all loot types are affected).

### `GoldLootMultiplier`
- **Type**: `number`
- **Description**: Gold multiplier for gold obtained from loot.

### `GrowthRate`
- **Type**: `number`
- **Description**: Growth rate multiplier for doodads (growth steps, not simple timers).

### `DaysForTaxPayment`
- **Type**: `number`
- **Description**: Number of days one tax payment covers (default: 7).

### `IgnoreFallDamageAccessLevel`
- **Type**: `number`
- **Description**: Minimum access level that ignores fall damage (dev/testing).

### `RadarGmAccessLevel`
- **Type**: `number`
- **Description**: Minimum effective access level at which the radar / telescope shows every object in the world regardless of the registered range (default: `100`; lower it to give the GM range to more accounts).

### `GodMode`
- **Type**: `boolean`
- **Description**: When `true`, players take no damage.

### `MoveGuardEnabled`
- **Type**: `boolean`
- **Description**: Server-side movement guard (default: `true`). Every client-reported position of a player on foot, an owned mate or a driven land vehicle is checked against a speed budget kept per unit; a move beyond it is dropped, logged (`audit_char_sus`, category `Cheat`) and the client is teleported back to the server position.

### `MoveGuardMaxSpeed`
- **Type**: `number`
- **Description**: Sustained horizontal speed in m/s the guard allows a player or mate (default: `30.0`).

### `MoveGuardMaxUpSpeed`
- **Type**: `number`
- **Description**: Sustained upward speed in m/s (default: `30.0`); falling is never limited.

### `MoveGuardBurstSeconds`
- **Type**: `number`
- **Description**: Seconds of speed the budget can bank, so lag spikes and dash skills are not refused (default: `3.0`).

### `MoveGuardVehicleMaxSpeed`
- **Type**: `number`
- **Description**: Sustained speed in m/s of a client-driven land vehicle (default: `45.0`).

### `MoveGuardExemptAccessLevel`
- **Type**: `number`
- **Description**: Characters at or above this access level are not checked (default: `50`).

### `MoveGuardKickAfterViolations`
- **Type**: `number`
- **Description**: Disconnect a player after this many refused moves within `MoveGuardViolationWindowSeconds` (default: `0` = never).

### `MoveGuardViolationWindowSeconds`
- **Type**: `number`
- **Description**: The window of `MoveGuardKickAfterViolations` (default: `60.0`).

### `GeoDataMode`
- **Type**: `boolean`
- **Description**: Enables loading GeoData/NavMesh (dungeons/navigation).

### `PreLoadTerrain`
- **Type**: `boolean`
- **Description**: When `true`, preloads terrain data (slower startup, lower runtime spikes, higher memory usage).

### `MaxInstances`
- **Type**: `number`
- **Description**: Maximum number of instances (including system instances).

### `TargetPhysicsTps`
- **Type**: `number`
- **Description**: Target physics TPS (tick rate for physics threads).

### `ActabilityRate`
- **Type**: `number`
- **Description**: Server-side actability points multiplier.

## Ship wind

### `WindModel`
- **Type**: `string`
- **Path**: `World.WindModel`
- **Allowed values**:
  - **`Official`**: retail-like wind model.
    - wind does **not** change with time of day;
    - a **+15%** max speed bonus applies only when sailing within **±15°** of the **North↔South** axis (both directions);
    - outside the cone, the bonus is **0%**.
  - **`Realistic`**: more realistic model.
    - wind direction rotates smoothly over the day (and sail rig profile logic applies).

Examples:

```json
{
  "World": {
    "WindModel": "Official"
  }
}
```

```json
{
  "World": {
    "WindModel": "Realistic"
  }
}
```

## Sea weather (marine events)

### `SeaWeatherModel`
- **Type**: `string`
- **Path**: `World.SeaWeatherModel`
- **What it affects**: ship-side behavior while marine weather doodads are active (storm cloud / whirlpool). It does **not** control spawn locations; those are driven by marine-weather spawners (marker template `2768`) in world spawn data.
- **Allowed values**:
  - **`Official`**: retail-like (default).
    - keeps the **retail-like slowdown** behavior in storms;
    - whirlpool pull (buff-driven drift toward active whirlpools) still applies.
  - **`Realistic`**: advanced / more immersive.
    - keeps the **retail-like slowdown** behavior in storms;
    - enables additional ship-in-storm behavior when the storm buff is active:
      - blocks firing **ship cannons** while in storm (harpoon is not blocked);
      - automatically turns on ship lamps (front/rear) while in storm;
      - forces the client time-of-day display to **02:00** while the storm buff is active.
    - whirlpool pull remains enabled (same as `Official`).

Examples:

```json
{
  "World": {
    "SeaWeatherModel": "Official"
  }
}
```

```json
{
  "World": {
    "SeaWeatherModel": "Realistic"
  }
}
```

