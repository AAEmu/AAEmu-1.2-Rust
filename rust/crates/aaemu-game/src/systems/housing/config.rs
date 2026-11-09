//! World policy for territory property tax rules absent from the available 1.2 server source.

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DominionPropertyTaxBase {
    /// Charge the territory percentage on the weekly amount after multiple-property tax.
    #[default]
    AfterHeavy,
    /// Charge the territory percentage only on the template's ordinary base tax.
    Base,
}

impl crate::config::ConfigEnum for DominionPropertyTaxBase {
    fn parse_config(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("AfterHeavy") || value == "0" {
            Some(Self::AfterHeavy)
        } else if value.eq_ignore_ascii_case("Base") || value == "1" {
            Some(Self::Base)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HousingConfig {
    #[serde(
        rename = "dominionpropertytaxbase",
        deserialize_with = "crate::config::lenient_enum"
    )]
    pub dominion_property_tax_base: DominionPropertyTaxBase,
    /// Drives the weekly siege periods from `siege_zones`/`siege_plans` instead of the
    /// explicit GM schedule alone. It does **not** gate Auroria claims or the client
    /// `FeatureSet.siege` bit: those follow the era profile. The era profile decides when the
    /// key is missing (`World.Profile`: on for `EuAuroria` and `Kr12`, off for `EuLaunch`), an
    /// explicit `true` / `false` wins. `Housing.SiegeCalendar`.
    #[serde(
        rename = "siegecalendar",
        default,
        deserialize_with = "crate::config::lenient_option"
    )]
    pub siege_calendar: Option<bool>,
    /// Offset of the `siege_zones` weekday/hour/minute columns, e.g. `+09:00`. Empty (the
    /// default) or `utc` is UTC; `local` uses the server's current local offset. UTC is the
    /// default because the Western 2014 schedule names the same wall-clock times as GMT
    /// (`round6-siege.md` 1, grade C); the retail data's zone is not proven.
    /// `Housing.SiegeCalendarTimezone`.
    #[serde(rename = "siegecalendartimezone")]
    pub siege_calendar_timezone: String,
    /// How many territories one expedition may own before a new claim is refused with
    /// `TOO_MANY_OWNED_DOMINIONS` (441). The 1.2 limit is UNKNOWN; `siege_settings` has
    /// reinforcement rows for 0..=10 held castles, so 10 is the non-binding default.
    /// `Housing.DominionClaimLimit`.
    #[serde(
        rename = "dominionclaimlimit",
        default = "default_dominion_claim_limit",
        deserialize_with = "crate::config::lenient"
    )]
    pub dominion_claim_limit: u32,
    /// World-space offset from the lodestone where the server spawns the defender's HQ
    /// doodad (3410) at the warmup of a declared siege. The retail rule is UNKNOWN; the
    /// default `[0, 0, 0]` disables the automatic spawn and keeps the GM-bound HQ behaviour.
    /// `Housing.DefenseHqOffset`.
    #[serde(rename = "defensehqoffset")]
    pub defense_hq_offset: [f32; 3],
    /// Gate merchant packs 224/225 ("siege-time goods", defense / attack side) to the
    /// warmup and siege periods of the NPC's territory and to registered participants of
    /// the pack's side. The rule is INFERRED from the 1.2 pack names only (`merchant_packs`:
    /// "defense-side goods sold during a siege" / "siege-side goods sold during a siege");
    /// there is no capture of the retail restriction or its side rule. `false` (default)
    /// keeps the previous behaviour: both packs sell at any time to anyone.
    /// `Housing.SiegeShopGate`.
    #[serde(rename = "siegeshopgate", deserialize_with = "crate::config::lenient")]
    pub siege_shop_gate: bool,
    /// Apply the 1.2 client's place-skin terrain material rule to player doodad placements:
    /// the terrain material under the spot must be one of the template's
    /// `doodad_place_skins` kinds, else `DOODAD_SIM_AREA` (276). `false` (default) keeps the
    /// previous behaviour (no terrain material check). See `PORTING.md`.
    /// `Housing.PlacementStrictSurface`.
    #[serde(
        rename = "placementstrictsurface",
        deserialize_with = "crate::config::lenient"
    )]
    pub placement_strict_surface: bool,
    /// Hits by other units on an abandoned (tax-demolished, ownerless) house remove at least
    /// `max_hp / AbandonedHouseHitsToBreak`, so about this many hits break any house. `0`
    /// disables the scaling. Server policy, not a 1.2 rule. `Housing.AbandonedHouseHitsToBreak`.
    #[serde(
        rename = "abandonedhousehitstobreak",
        default = "default_abandoned_house_hits_to_break",
        deserialize_with = "crate::config::lenient"
    )]
    pub abandoned_house_hits_to_break: u32,
    /// Distance in metres beyond a house's garden plot that refuses a Siege Camp declaration
    /// (`SIEGE_CANNOT_DECLARE_NEAR_HOUSE` 633). Farm and garden plot house designs use the
    /// same `garden_radius` plot, so they are refused too. GameMeca 2013-04-22 (grade C) says
    /// the camp may not be declared within about 10 m of houses (incl. farmland); the default
    /// is `10.0`. `0` refuses only a spot inside a plot. `Housing.SiegeDeclareHouseMargin`.
    #[serde(
        rename = "siegedeclarehousemargin",
        deserialize_with = "crate::config::lenient"
    )]
    pub siege_declare_house_margin: f32,
    /// Distance in metres from farmland that refuses a Siege Camp declaration
    /// (`SIEGE_CANNOT_DECLARE_NEAR_HOUSE` 633). Farmland is a public farm area (a
    /// `PublicFarmManager._farmZones` sub zone, where common-farm doodads may be planted) or
    /// a player-planted farm doodad (owner a character or a house; a template with a
    /// `DoodadFuncHouseFarm` phase func, or a doodad carrying a `farm_type`). Wild gathering
    /// nodes of the same templates do not count. GameMeca 2013-04-22 (grade C) says about
    /// 10 m from houses (incl. farmland); the default is `10.0`.
    /// `Housing.SiegeDeclareFarmMargin`.
    #[serde(
        rename = "siegedeclarefarmmargin",
        deserialize_with = "crate::config::lenient"
    )]
    pub siege_declare_farm_margin: f32,
    /// Distance in metres from the declared spot at which a different zone group refuses the
    /// declaration (`SIEGE_CANNOT_DECLARE_NEAR_OTHER_ZONE` 661). `0` (default) refuses when
    /// the spot itself lies in another zone group than the besieged dominion. The retail
    /// margin is UNKNOWN: INFERRED, capture-pending. `Housing.SiegeDeclareZoneMargin`.
    #[serde(
        rename = "siegedeclarezonemargin",
        deserialize_with = "crate::config::lenient"
    )]
    pub siege_declare_zone_margin: f32,
    /// Enable the resurrection-point refusal (`SIEGE_CANNOT_DECLARE_NEAR_RESURRECT_POS` 625).
    /// GameMeca 2013-04-22 (grade C): the camp may not be declared within about 100 m of a
    /// resurrection point. The server's resurrection points are the `Data/Portal/respawns.json`
    /// portals (`PortalTables::respawns`). `true` (default) applies the rule.
    /// `Housing.SiegeDeclareResurrectCheck`.
    #[serde(
        rename = "siegedeclareresurrectcheck",
        deserialize_with = "crate::config::lenient"
    )]
    pub siege_declare_resurrect_check: bool,
    /// Distance in metres (horizontal) from the declared spot to the nearest resurrection
    /// point that refuses the declaration when [`Self::siege_declare_resurrect_check`] is
    /// on. GameMeca 2013-04-22 (grade C) says "about 100 m"; the default is `100.0`. `0`
    /// refuses only a spot on a resurrection point. `Housing.SiegeDeclareResurrectMargin`.
    #[serde(
        rename = "siegedeclareresurrectmargin",
        deserialize_with = "crate::config::lenient"
    )]
    pub siege_declare_resurrect_margin: f32,
    /// Half extent in metres of the collision box tested around the declared spot
    /// (`SIEGE_CANNOT_DECLARE_OBSTACLE` 741). The retail radius is UNKNOWN; the default is
    /// the server's player capsule radius (0.35 m, `collision::displacement`). INFERRED,
    /// capture-pending. `Housing.SiegeDeclareObstacleRadius`.
    #[serde(
        rename = "siegedeclareobstacleradius",
        deserialize_with = "crate::config::lenient"
    )]
    pub siege_declare_obstacle_radius: f32,
}

fn default_abandoned_house_hits_to_break() -> u32 {
    5
}

fn default_siege_declare_obstacle_radius() -> f32 {
    0.35
}

fn default_dominion_claim_limit() -> u32 {
    10
}

impl Default for HousingConfig {
    fn default() -> Self {
        Self {
            dominion_property_tax_base: DominionPropertyTaxBase::default(),
            siege_calendar: None,
            siege_calendar_timezone: String::new(),
            dominion_claim_limit: default_dominion_claim_limit(),
            defense_hq_offset: [0.0; 3],
            siege_shop_gate: false,
            placement_strict_surface: false,
            abandoned_house_hits_to_break: default_abandoned_house_hits_to_break(),
            siege_declare_house_margin: 10.0,
            siege_declare_farm_margin: 10.0,
            siege_declare_zone_margin: 0.0,
            siege_declare_resurrect_check: true,
            siege_declare_resurrect_margin: 100.0,
            siege_declare_obstacle_radius: default_siege_declare_obstacle_radius(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_explicit_property_tax_policy_follow_the_configuration_binder() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        assert_eq!(
            default.housing.dominion_property_tax_base,
            DominionPropertyTaxBase::AfterHeavy
        );
        for value in [serde_json::json!("base"), serde_json::json!(1)] {
            let config = crate::config::AppConfig::from_value(serde_json::json!({
                "Housing": { "DominionPropertyTaxBase": value }
            }))
            .unwrap();
            assert_eq!(
                config.housing.dominion_property_tax_base,
                DominionPropertyTaxBase::Base
            );
        }
        assert!(crate::config::AppConfig::from_value(serde_json::json!({
            "Housing": { "DominionPropertyTaxBase": "unknown" }
        }))
        .is_err());
    }

    #[test]
    fn dominion_claim_limit_defaults_to_ten_and_binds_numbers_or_strings() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        assert_eq!(default.housing.dominion_claim_limit, 10);
        for value in [serde_json::json!(1), serde_json::json!("1")] {
            let config = crate::config::AppConfig::from_value(serde_json::json!({
                "Housing": { "DominionClaimLimit": value }
            }))
            .unwrap();
            assert_eq!(config.housing.dominion_claim_limit, 1);
        }
    }

    #[test]
    fn defense_hq_offset_defaults_to_disabled_and_binds_three_numbers() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        assert_eq!(default.housing.defense_hq_offset, [0.0, 0.0, 0.0]);
        let config = crate::config::AppConfig::from_value(serde_json::json!({
            "Housing": { "DefenseHqOffset": [10.5, -4.0, 2.0] }
        }))
        .unwrap();
        assert_eq!(config.housing.defense_hq_offset, [10.5, -4.0, 2.0]);
    }

    #[test]
    fn siege_shop_gate_defaults_off_and_binds_booleans_or_strings() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        assert!(!default.housing.siege_shop_gate);
        for value in [serde_json::json!(true), serde_json::json!("true")] {
            let config = crate::config::AppConfig::from_value(serde_json::json!({
                "Housing": { "SiegeShopGate": value }
            }))
            .unwrap();
            assert!(config.housing.siege_shop_gate);
        }
    }

    #[test]
    fn placement_strict_surface_defaults_off_and_binds_booleans_or_strings() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        assert!(!default.housing.placement_strict_surface);
        for value in [serde_json::json!(true), serde_json::json!("true")] {
            let config = crate::config::AppConfig::from_value(serde_json::json!({
                "Housing": { "PlacementStrictSurface": value }
            }))
            .unwrap();
            assert!(config.housing.placement_strict_surface);
        }
    }

    #[test]
    fn abandoned_house_hits_to_break_defaults_to_five_and_binds_numbers_or_strings() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        assert_eq!(default.housing.abandoned_house_hits_to_break, 5);
        for value in [serde_json::json!(0), serde_json::json!("0")] {
            let config = crate::config::AppConfig::from_value(serde_json::json!({
                "Housing": { "AbandonedHouseHitsToBreak": value }
            }))
            .unwrap();
            assert_eq!(config.housing.abandoned_house_hits_to_break, 0);
        }
    }

    #[test]
    fn siege_declare_placement_thresholds_default_and_bind() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        assert_eq!(default.housing.siege_declare_house_margin, 10.0);
        assert_eq!(default.housing.siege_declare_farm_margin, 10.0);
        assert_eq!(default.housing.siege_declare_zone_margin, 0.0);
        assert!(default.housing.siege_declare_resurrect_check);
        assert_eq!(default.housing.siege_declare_resurrect_margin, 100.0);
        assert_eq!(default.housing.siege_declare_obstacle_radius, 0.35);
        for value in [serde_json::json!(false), serde_json::json!("false")] {
            let config = crate::config::AppConfig::from_value(serde_json::json!({
                "Housing": { "SiegeDeclareResurrectCheck": value }
            }))
            .unwrap();
            assert!(!config.housing.siege_declare_resurrect_check);
        }
        let config = crate::config::AppConfig::from_value(serde_json::json!({
            "Housing": {
                "SiegeDeclareHouseMargin": 2.5,
                "SiegeDeclareFarmMargin": "3.5",
                "SiegeDeclareZoneMargin": "7.5",
                "SiegeDeclareResurrectMargin": 50,
                "SiegeDeclareObstacleRadius": "1.25"
            }
        }))
        .unwrap();
        assert_eq!(config.housing.siege_declare_house_margin, 2.5);
        assert_eq!(config.housing.siege_declare_farm_margin, 3.5);
        assert_eq!(config.housing.siege_declare_zone_margin, 7.5);
        assert_eq!(config.housing.siege_declare_resurrect_margin, 50.0);
        assert_eq!(config.housing.siege_declare_obstacle_radius, 1.25);
    }

    #[test]
    fn siege_calendar_binds_explicitly_and_defaults_from_the_profile() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        // Missing key: the default profile (EuAuroria) enables sieges.
        assert_eq!(default.housing.siege_calendar, None);
        assert!(default.siege_calendar_enabled());
        assert!(default.housing.siege_calendar_timezone.is_empty());
        for value in [serde_json::json!(true), serde_json::json!("true")] {
            let config = crate::config::AppConfig::from_value(serde_json::json!({
                "Housing": {
                    "SiegeCalendar": value,
                    "SiegeCalendarTimezone": "+09:00"
                }
            }))
            .unwrap();
            assert_eq!(config.housing.siege_calendar, Some(true));
            assert!(config.siege_calendar_enabled());
            assert_eq!(config.housing.siege_calendar_timezone, "+09:00");
        }
        // An explicit false wins over the Auroria profile; an explicit true wins over launch.
        let config = crate::config::AppConfig::from_value(serde_json::json!({
            "Housing": { "SiegeCalendar": false }
        }))
        .unwrap();
        assert!(!config.siege_calendar_enabled());
        let config = crate::config::AppConfig::from_value(serde_json::json!({
            "World": { "Profile": "EuLaunch" }
        }))
        .unwrap();
        assert!(!config.siege_calendar_enabled());
        let config = crate::config::AppConfig::from_value(serde_json::json!({
            "World": { "Profile": "EuLaunch" },
            "Housing": { "SiegeCalendar": true }
        }))
        .unwrap();
        assert!(config.siege_calendar_enabled());
    }
}
