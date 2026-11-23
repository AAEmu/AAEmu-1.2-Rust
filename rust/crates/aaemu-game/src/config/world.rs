use super::{lenient, lenient_enum, ConfigEnum};
use serde::Deserialize;
use std::fmt::Display;

/// `WindModelType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WindModelType {
    /// Retail-like: wind only along the N-S axis.
    #[default]
    Official,
    /// Wind direction rotates over the day.
    Realistic,
}

/// `SeaWeatherModelType`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SeaWeatherModelType {
    #[default]
    Official,
    Realistic,
}

impl_config_enum!(WindModelType { Official = 0, Realistic = 1 });
impl_config_enum!(SeaWeatherModelType { Official = 0, Realistic = 1 });

/// The retail era the server emulates (`World.Profile`, Rust only).
///
/// The default is the owner decision of 2026-11-08: Trion EU "Auroria" (4 Nov 2014 to
/// 9 Mar 2015), i.e. the Trion 1.2 build after the Conquest of Auroria update. `EuLaunch` is
/// the Trion NA/EU launch era (12/16 Sep to 3 Nov 2014, before Auroria). `Kr12` is the XL KR
/// 1.2 data our templates come from (`r208022`). The timeline and sources are in the audit
/// research `research/trion-1.2-eu-na.md`; `PORTING.md` cites the dates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ServerProfile {
    /// Trion EU "Auroria" era: level cap 50, no player nations, sieges and Auroria claims on.
    #[default]
    EuAuroria,
    /// Trion NA/EU launch era: level cap 50, no player nations, no sieges and no Auroria.
    EuLaunch,
    /// XL KR 1.2 (`r208022`): level cap 55, player nations and dominion sieges on.
    Kr12,
}

impl ServerProfile {
    /// Player level cap (`ExperienceManager.PlayerLevelCap`). Trion's 1.2 builds kept level 50
    /// until Secrets of Ayanad (2015-03-10); the KR 1.2 data goes to 55.
    pub fn player_level_cap(self) -> u8 {
        match self {
            Self::EuAuroria | Self::EuLaunch => 50,
            Self::Kr12 => 55,
        }
    }

    /// Player nations (independence, diplomacy, national tax, monuments): KR only. No public
    /// Trion 1.2 source shows player-nation founding before the later updates.
    pub fn player_nations(self) -> bool {
        matches!(self, Self::Kr12)
    }

    /// Dominion sieges and Auroria territory claims: the Conquest of Auroria update
    /// (2014-11-04 NA, 2014-11-06 EU) shipped them; the launch era did not have them.
    pub fn sieges(self) -> bool {
        !matches!(self, Self::EuLaunch)
    }

    /// The C# dungeon entry cap (`IndunManager.CheckEntryAttemptCount`: 3 entries per 4 hours).
    /// The owner's EU 1.2 recollection (2026-11-10) is that 1.2 had no entry limit (the
    /// "5 per dungeon" cap came later), so `EuAuroria` leaves it off; the other profiles keep
    /// the C# default. `Dungeons.EntryLimit.Enabled` overrides this when set.
    pub fn dungeon_entry_limit(self) -> bool {
        !matches!(self, Self::EuAuroria)
    }

    /// The Archeum Lodestone max HP of the era, over the `r208022` `housings.hp` value
    /// (1,000,000); `None` keeps the template value.
    ///
    /// `EuAuroria` is the NA Nov 2014 siege HUD's 4,100,000/4,100,000 bar
    /// (`D:/aaemu-audits/video-evidence/siege-2014-11-30-ollo.md` §2.6, grade C; the same
    /// Nov 2014 change is described in the period r/archeage thread "Wall HP went from 400k
    /// to 1.2M and the lodestone went from 1M to 4M HP"). The value is a Trion Auroria
    /// server balance change and is **not** in the `r208022` client data; the launch era had
    /// no sieges, and `Kr12` keeps the template value.
    pub fn lodestone_max_hp(self) -> Option<i32> {
        match self {
            Self::EuAuroria => Some(4_100_000),
            Self::EuLaunch | Self::Kr12 => None,
        }
    }

    /// The castle Defense Tower max HP of the era, over the `r208022` `housings.hp` value
    /// (600,000); `None` keeps the template value.
    ///
    /// `EuAuroria` is the NA Nov 2014 target frame "Nuian Defense Tower" 1,500,000 HP
    /// (`siege-2014-11-30-ollo.md` §1/§2.6, grade C). Like [`Self::lodestone_max_hp`] this is
    /// a Trion Auroria server balance value, not a `r208022` data value.
    pub fn defense_tower_max_hp(self) -> Option<i32> {
        match self {
            Self::EuAuroria => Some(1_500_000),
            Self::EuLaunch | Self::Kr12 => None,
        }
    }
}

impl_config_enum!(ServerProfile {
    EuAuroria = 0,
    EuLaunch = 1,
    Kr12 = 2
});

impl Display for ServerProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EuAuroria => "EuAuroria",
            Self::EuLaunch => "EuLaunch",
            Self::Kr12 => "Kr12",
        })
    }
}

/// `WorldConfig` (`Configurations/World.json`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorldConfig {
    /// The retail era profile (`World.Profile`, Rust only): see [`ServerProfile`].
    #[serde(rename = "profile", deserialize_with = "lenient_enum")]
    pub profile: ServerProfile,
    /// `MOTD`.
    #[serde(rename = "motd")]
    pub motd: String,
    #[serde(rename = "logoutmessage")]
    pub logout_message: String,
    /// `GameTimeOffset`: game seconds added to the wall-clock-derived game clock
    /// (`(unix_seconds * 6 + offset) mod 86400`, `6` = one game minute per 10 real seconds).
    /// The default `0` makes the mapping deterministic (Unix epoch); positive values move the
    /// clock forward. Rust only: the C# clock started at 12:00 on every boot.
    #[serde(rename = "gametimeoffset", deserialize_with = "lenient")]
    pub game_time_offset: f64,
    /// Minutes.
    #[serde(rename = "autosaveinterval", deserialize_with = "lenient")]
    pub auto_save_interval: f64,
    #[serde(rename = "exprate", deserialize_with = "lenient")]
    pub exp_rate: f64,
    #[serde(rename = "honorrate", deserialize_with = "lenient")]
    pub honor_rate: f64,
    #[serde(rename = "pvphonorrate", deserialize_with = "lenient")]
    pub pvp_honor_rate: f64,
    #[serde(rename = "vocationrate", deserialize_with = "lenient")]
    pub vocation_rate: f64,
    #[serde(rename = "lootrate", deserialize_with = "lenient")]
    pub loot_rate: f64,
    #[serde(rename = "goldlootmultiplier", deserialize_with = "lenient")]
    pub gold_loot_multiplier: f64,
    /// `GrowthRate` (doodad growth time divisor).
    #[serde(rename = "growthrate", deserialize_with = "lenient")]
    pub growth_rate: f64,
    #[serde(rename = "daysfortaxpayment", deserialize_with = "lenient")]
    pub days_for_tax_payment: u32,
    #[serde(rename = "ignorefalldamageaccesslevel", deserialize_with = "lenient")]
    pub ignore_fall_damage_access_level: i32,
    /// Minimum effective access level at which the radar / telescope shows every object in the
    /// world regardless of the registered range (`RadarManager`, the C# had this hard-coded).
    #[serde(rename = "radargmaccesslevel", deserialize_with = "lenient")]
    pub radar_gm_access_level: i32,
    #[serde(rename = "godmode", deserialize_with = "lenient")]
    pub god_mode: bool,
    /// `MoveGuardEnabled`: the server checks every client-reported unit position against a
    /// speed budget (`world::move_guard`) and refuses jumps / gross speed hacks.
    #[serde(rename = "moveguardenabled", deserialize_with = "lenient")]
    pub move_guard_enabled: bool,
    /// `MoveGuardMaxSpeed`: sustained horizontal speed (m/s) of a player on foot or an owned
    /// mate (the budget refills at this rate of server time).
    #[serde(rename = "moveguardmaxspeed", deserialize_with = "lenient")]
    pub move_guard_max_speed: f32,
    /// `MoveGuardMaxUpSpeed`: sustained upward speed (m/s); falling is free.
    #[serde(rename = "moveguardmaxupspeed", deserialize_with = "lenient")]
    pub move_guard_max_up_speed: f32,
    /// `MoveGuardBurstSeconds`: how many seconds of speed the budget may hold (lag spikes,
    /// dash skills).
    #[serde(rename = "moveguardburstseconds", deserialize_with = "lenient")]
    pub move_guard_burst_seconds: f32,
    /// `MoveGuardVehicleMaxSpeed`: sustained speed (m/s) of a client-driven land vehicle.
    #[serde(rename = "moveguardvehiclemaxspeed", deserialize_with = "lenient")]
    pub move_guard_vehicle_max_speed: f32,
    /// `MoveGuardExemptAccessLevel`: characters at or above this access level are not checked.
    #[serde(rename = "moveguardexemptaccesslevel", deserialize_with = "lenient")]
    pub move_guard_exempt_access_level: i32,
    /// `MoveGuardKickAfterViolations`: disconnect a player after this many refused moves within
    /// `MoveGuardViolationWindowSeconds`; `0` never kicks.
    #[serde(rename = "moveguardkickafterviolations", deserialize_with = "lenient")]
    pub move_guard_kick_after_violations: u32,
    /// `MoveGuardViolationWindowSeconds`: the window of `MoveGuardKickAfterViolations`.
    #[serde(
        rename = "moveguardviolationwindowseconds",
        deserialize_with = "lenient"
    )]
    pub move_guard_violation_window_seconds: f32,
    /// `GeoDataMode` (navmesh heights and A* paths, `systems::ai::geodata`).
    #[serde(rename = "geodatamode", deserialize_with = "lenient")]
    pub geo_data_mode: bool,
    /// `PrewarmNavmesh`: warm path-block cells from player positions in the background.
    #[serde(rename = "prewarmnavmesh", deserialize_with = "lenient")]
    pub prewarm_navmesh: bool,
    /// `PreLoadTerrain`: load every heightmap cell at startup instead of on demand.
    #[serde(rename = "preloadterrain", deserialize_with = "lenient")]
    pub pre_load_terrain: bool,
    #[serde(rename = "maxinstances", deserialize_with = "lenient")]
    pub max_instances: u32,
    #[serde(rename = "targetphysicstps", deserialize_with = "lenient")]
    pub target_physics_tps: f32,
    #[serde(rename = "windmodel", deserialize_with = "lenient_enum")]
    pub wind_model: WindModelType,
    #[serde(rename = "seaweathermodel", deserialize_with = "lenient_enum")]
    pub sea_weather_model: SeaWeatherModelType,
    #[serde(rename = "actabilityrate", deserialize_with = "lenient")]
    pub actability_rate: f64,
    #[serde(rename = "tagshareenabled", deserialize_with = "lenient")]
    pub tag_share_enabled: bool,
    #[serde(rename = "usepersistenthousedoodads", deserialize_with = "lenient")]
    pub use_persistent_house_doodads: bool,
    #[serde(rename = "explossrateatdeath", deserialize_with = "lenient")]
    pub exp_loss_rate_at_death: f32,
    #[serde(rename = "pvedurabilitylossrate", deserialize_with = "lenient")]
    pub pve_durability_loss_rate: f32,
    #[serde(rename = "pvpdurabilitylossrate", deserialize_with = "lenient")]
    pub pvp_durability_loss_rate: f32,
    #[serde(rename = "minimumexplosslevel", deserialize_with = "lenient")]
    pub minimum_exp_loss_level: i32,
    /// `PortalUseRange`: how close (metres, 2D, minus the model radii) a character must be to a
    /// portal gate NPC or navi doodad to use it (`CSUsePortal`, `CSNaviTeleport`).
    #[serde(rename = "portaluserange", deserialize_with = "lenient")]
    pub portal_use_range: f32,
    /// `NpcPrewarmRegionMargin`: how many extra regions beyond the current player visibility
    /// neighborhoods are prewarmed as players move, so nearby NPCs are already prepared before
    /// a player reaches them. Server-side prewarming only; client-side NPC visibility is still
    /// decided per player by the usual region/view distance.
    #[serde(rename = "npcprewarmregionmargin", deserialize_with = "lenient")]
    pub npc_prewarm_region_margin: u32,
    /// `NpcSpawnTickMs`: how often (milliseconds) the proximity spawn/despawn pass runs.
    #[serde(rename = "npcspawntickms", deserialize_with = "lenient")]
    pub npc_spawn_tick_ms: u64,
    /// `NpcSpawnsPerTick`: maximum number of NPCs activated per proximity pass, budgeting
    /// work on busy ticks.
    #[serde(rename = "npcspawnspertick", deserialize_with = "lenient")]
    pub npc_spawns_per_tick: u32,
    /// `NpcWarmRetentionSeconds`: how long ordinary NPC instances stay warm after the last
    /// player leaves before they are reclaimed; their AI may already sleep during this time.
    #[serde(rename = "npcwarmretentionseconds", deserialize_with = "lenient")]
    pub npc_warm_retention_seconds: u64,
    /// `NpcResidentServiceNpcs`: when true, ordinary service NPCs (quest givers, vendors, ...)
    /// are retained after being visited/prepared; this does not mean their AI never sleeps.
    #[serde(rename = "npcresidentservicenpcs", deserialize_with = "lenient")]
    pub npc_resident_service_npcs: bool,
    /// `AiWorkerThreads`: worker threads used to run AI update batches. `1` (the default)
    /// runs AI updates sequentially in order; values above `1` enable bounded concurrency
    /// over independent NPC AI updates and relax strict ordering.
    #[serde(rename = "aiworkerthreads", deserialize_with = "lenient")]
    pub ai_worker_threads: u32,
    /// `NpcSpawnScatterTrigger`: a spawning NPC that would land within this distance (m, 3D) of
    /// another live NPC is scattered instead (see `systems::npcs::stacked_npcs`).
    #[serde(rename = "npcspawnscattertrigger", deserialize_with = "lenient")]
    pub npc_spawn_scatter_trigger: f32,
    /// `NpcSpawnScatterMin`: inner radius (m) of the scatter ring.
    #[serde(rename = "npcspawnscattermin", deserialize_with = "lenient")]
    pub npc_spawn_scatter_min: f32,
    /// `NpcSpawnScatterMax`: outer radius (m) of the scatter ring; `0` disables scattering.
    #[serde(rename = "npcspawnscattermax", deserialize_with = "lenient")]
    pub npc_spawn_scatter_max: f32,
    /// `StackedNpcCheckSeconds`: interval of the stacked-NPC check; `0` disables it.
    #[serde(rename = "stackednpccheckseconds", deserialize_with = "lenient")]
    pub stacked_npc_check_seconds: u64,
    /// `StackedNpcRadius`: NPCs closer than this (m, 3D) count as stacked.
    #[serde(rename = "stackednpcradius", deserialize_with = "lenient")]
    pub stacked_npc_radius: f32,
    /// `StackedNpcMinSeconds`: how long a stack must persist before it is logged.
    #[serde(rename = "stackednpcminseconds", deserialize_with = "lenient")]
    pub stacked_npc_min_seconds: u64,
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            profile: ServerProfile::default(),
            motd: String::new(),
            logout_message: String::new(),
            game_time_offset: 0.0,
            auto_save_interval: 5.0,
            exp_rate: 1.0,
            honor_rate: 1.0,
            pvp_honor_rate: 1.0,
            vocation_rate: 1.0,
            loot_rate: 1.0,
            gold_loot_multiplier: 1.0,
            growth_rate: 1.0,
            days_for_tax_payment: 7,
            ignore_fall_damage_access_level: 100,
            radar_gm_access_level: 100,
            god_mode: false,
            move_guard_enabled: true,
            move_guard_max_speed: 30.0,
            move_guard_max_up_speed: 30.0,
            move_guard_burst_seconds: 3.0,
            move_guard_vehicle_max_speed: 45.0,
            move_guard_exempt_access_level: 50,
            move_guard_kick_after_violations: 0,
            move_guard_violation_window_seconds: 60.0,
            geo_data_mode: false,
            prewarm_navmesh: false,
            pre_load_terrain: false,
            max_instances: 32,
            target_physics_tps: 25.0,
            wind_model: WindModelType::Official,
            sea_weather_model: SeaWeatherModelType::Official,
            actability_rate: 1.0,
            tag_share_enabled: false,
            use_persistent_house_doodads: false,
            exp_loss_rate_at_death: 0.05,
            pve_durability_loss_rate: 1.0,
            pvp_durability_loss_rate: 1.0,
            minimum_exp_loss_level: 10,
            // The data's `open_portal_effects.distance` is 3 m and the 1.2 tooltip places the
            // gate 5 m away; 5 m also matches `portal::NAVI_USE_RANGE` and the NPC services.
            portal_use_range: 5.0,
            npc_prewarm_region_margin: 1,
            npc_spawn_tick_ms: 250,
            npc_spawns_per_tick: 64,
            npc_warm_retention_seconds: 60,
            npc_resident_service_npcs: true,
            ai_worker_threads: 1,
            npc_spawn_scatter_trigger: 1.0,
            npc_spawn_scatter_min: 1.5,
            npc_spawn_scatter_max: 3.0,
            stacked_npc_check_seconds: 60,
            stacked_npc_radius: 0.5,
            stacked_npc_min_seconds: 120,
        }
    }
}
