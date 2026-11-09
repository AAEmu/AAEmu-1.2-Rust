//! `siege_items` usage gate (WP-SD3).
//!
//! The 1.2 client loads `siege_items` and the localized `USAGE` text it shows for
//! `CANNOT_USE_SIEGE_ITEM` (431), so the client pre-checks; this is the server's defence in
//! depth for a forged cast. A `true` flag means "allowed in that situation" (EVIDENCED by the
//! row shapes: a tank mine is allowed only inside the siege circle during the siege, an escape
//! scroll everywhere except the siege/HQ areas). The 13 rows and their situations are data;
//! the mapping from the flags to the world is INFERRED (period, `radius_siege` circle and
//! `radius_offense_hq` around the bound HQ).

use crate::server::GameServer;
use crate::systems::housing::dominion::period;
use crate::systems::housing::dominion::state::{SiegeSide, SiegeStatus};
use crate::world::PlayerRef;

/// One `siege_items` row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SiegeItem {
    pub item_id: u32,
    pub outside_siege_zone: bool,
    pub during_no_dominion: bool,
    pub during_declare: bool,
    pub during_peace: bool,
    pub outside_siege_area_during_warmup: bool,
    pub outside_siege_area_during_siege: bool,
    pub offense_hq_during_warmup: bool,
    pub offense_hq_during_siege: bool,
    pub defense_hq_during_warmup: bool,
    pub defense_hq_during_siege: bool,
    pub siege_circle_during_warmup: bool,
    pub siege_circle_during_siege: bool,
}

/// The situations that hold for one use, independent of any world lookup (pure for tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Situation {
    /// The player is not in a zone group with a dominion record.
    pub outside_siege_zone: bool,
    /// 0 no dominion, 1 auction, 2 declare, 3 warmup, 4 siege, 5 peace.
    pub period: u8,
    /// Within `guard_tower_settings.radius_siege` of the lodestone (warmup/siege only).
    pub inside_siege_area: bool,
    /// Within `radius_offense_hq` of the current registered side's HQ.
    pub inside_offense_hq: bool,
    pub inside_defense_hq: bool,
}

impl SiegeItem {
    /// A `true` flag for an applicable situation allows the use; otherwise it is refused.
    pub fn allows(&self, situation: &Situation) -> bool {
        if situation.outside_siege_zone {
            return self.outside_siege_zone;
        }
        match situation.period {
            0 => self.during_no_dominion,
            2 => self.during_declare,
            5 => self.during_peace,
            3 | 4 => {
                let warmup = situation.period == 3;
                if !situation.inside_siege_area {
                    return if warmup {
                        self.outside_siege_area_during_warmup
                    } else {
                        self.outside_siege_area_during_siege
                    };
                }
                (if warmup {
                    self.siege_circle_during_warmup
                } else {
                    self.siege_circle_during_siege
                }) || (situation.inside_offense_hq
                    && if warmup {
                        self.offense_hq_during_warmup
                    } else {
                        self.offense_hq_during_siege
                    })
                    || (situation.inside_defense_hq
                        && if warmup {
                            self.defense_hq_during_warmup
                        } else {
                            self.defense_hq_during_siege
                        })
            }
            // The auction period has no authored flag; the item is not gated there (no
            // evidence for a rule), so the use passes.
            _ => true,
        }
    }
}

fn horizontal_distance2(a: (f32, f32), b: (f32, f32)) -> f64 {
    (f64::from(a.0) - f64::from(b.0)).powi(2) + (f64::from(a.1) - f64::from(b.1)).powi(2)
}

/// Whether the item is refused right now (`true` = send `CANNOT_USE_SIEGE_ITEM` 431 and keep
/// the item). Unlisted items are never refused. The gate is only active for a zone the
/// calendar describes or that has an explicit siege; outside every dominion zone the
/// `outside_siege_zone` flag decides.
pub fn refused(server: &GameServer, player: &PlayerRef, template_id: u32) -> bool {
    let Some(item) = server
        .game_data
        .systems
        .housing
        .castles
        .siege_items
        .get(&template_id)
        .copied()
    else {
        return false;
    };
    let transform = { player.data().unit.transform };
    let character_id = player.character_id;
    let Some(zone) = server
        .game_data
        .zones
        .zone_by_key(transform.zone_id)
        .map(|zone| zone.group_id)
    else {
        return !item.outside_siege_zone;
    };
    let Some(state) = server.systems.housing.dominions.get(zone) else {
        return !item.outside_siege_zone;
    };
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing, server.config.world.profile);
    let explicit = state
        .siege
        .as_ref()
        .is_some_and(|siege| siege.status != SiegeStatus::Finished);
    if !settings.enabled && !explicit {
        return false;
    }
    let period = period::current_period(server, zone).unwrap_or(0);
    let mut situation = Situation {
        period,
        ..Default::default()
    };
    if matches!(period, 3 | 4) {
        if let Some(tower) = server.systems.housing.manager.by_id(state.tower.house_id) {
            let tower_object = crate::world::WorldObject::from(&tower);
            if let Some(world) = server.world.world_of(&tower_object) {
                let tower_pos = world.world_pos_rot(&tower_object);
                let radius_siege = server
                    .game_data
                    .systems
                    .housing
                    .castles
                    .guard_tower_settings
                    .get(&state.tower.guard_tower_setting_id)
                    .map_or(250.0, |setting| f64::from(setting.radius_siege));
                let player_pos = world.world_pos_rot(&crate::world::WorldObject::from(player));
                situation.inside_siege_area =
                    horizontal_distance2((player_pos.x, player_pos.y), (tower_pos.x, tower_pos.y))
                        <= radius_siege.powi(2);
                // The HQ circles use the player's registered side, as the client's row text
                // does; the radius is the offense one for both (defense radius is UNKNOWN).
                let side = state
                    .siege
                    .as_ref()
                    .and_then(|siege| siege.participants.get(&character_id))
                    .map(|participant| participant.side);
                if let Some(side) = side {
                    if let Some(hq) = server
                        .systems
                        .housing
                        .headquarters
                        .snapshot(server, &state, side)
                    {
                        let radius_offense_hq = server
                            .game_data
                            .systems
                            .housing
                            .castles
                            .guard_tower_settings
                            .get(&state.tower.guard_tower_setting_id)
                            .map_or(100.0, |setting| f64::from(setting.radius_offense_hq));
                        let inside = horizontal_distance2(
                            (player_pos.x, player_pos.y),
                            (hq.position.x, hq.position.y),
                        ) <= radius_offense_hq.powi(2);
                        match side {
                            SiegeSide::Attacker => situation.inside_offense_hq = inside,
                            SiegeSide::Defender => situation.inside_defense_hq = inside,
                        }
                    }
                }
            }
        }
    }
    !item.allows(&situation)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mine() -> SiegeItem {
        SiegeItem {
            item_id: 26_397,
            siege_circle_during_siege: true,
            ..Default::default()
        }
    }

    fn catapult() -> SiegeItem {
        SiegeItem {
            item_id: 27_300,
            offense_hq_during_siege: true,
            ..Default::default()
        }
    }

    fn escape_scroll() -> SiegeItem {
        SiegeItem {
            item_id: 34_018,
            outside_siege_zone: true,
            during_no_dominion: true,
            during_declare: true,
            during_peace: true,
            outside_siege_area_during_warmup: true,
            outside_siege_area_during_siege: true,
            ..Default::default()
        }
    }

    fn situation(period: u8) -> Situation {
        Situation {
            period,
            ..Default::default()
        }
    }

    #[test]
    fn the_three_authored_classes_allow_exactly_their_situations() {
        // Tank mine: only inside the siege circle during the siege.
        assert!(mine().allows(&Situation {
            period: 4,
            inside_siege_area: true,
            ..Default::default()
        }));
        assert!(!mine().allows(&Situation {
            period: 4,
            inside_siege_area: false,
            ..Default::default()
        }));
        assert!(!mine().allows(&situation(3)));
        assert!(!mine().allows(&situation(2)));
        assert!(!mine().allows(&situation(5)));
        assert!(!mine().allows(&Situation {
            outside_siege_zone: true,
            ..Default::default()
        }));

        // Catapult: only inside the offense HQ circle during the siege.
        assert!(catapult().allows(&Situation {
            period: 4,
            inside_siege_area: true,
            inside_offense_hq: true,
            ..Default::default()
        }));
        assert!(!catapult().allows(&Situation {
            period: 4,
            inside_siege_area: true,
            inside_offense_hq: false,
            ..Default::default()
        }));
        assert!(!catapult().allows(&Situation {
            period: 3,
            inside_siege_area: true,
            inside_offense_hq: true,
            ..Default::default()
        }));

        // Escape scroll: everywhere except the siege/HQ circles during warmup and siege.
        for period in [0, 2, 5] {
            assert!(
                escape_scroll().allows(&situation(period)),
                "period {period}"
            );
        }
        assert!(escape_scroll().allows(&Situation {
            outside_siege_zone: true,
            ..Default::default()
        }));
        for period in [3, 4] {
            assert!(escape_scroll().allows(&Situation {
                period,
                inside_siege_area: false,
                ..Default::default()
            }));
            assert!(!escape_scroll().allows(&Situation {
                period,
                inside_siege_area: true,
                ..Default::default()
            }));
            assert!(!escape_scroll().allows(&Situation {
                period,
                inside_siege_area: true,
                inside_offense_hq: true,
                ..Default::default()
            }));
        }
        // The auction period has no authored flag: no gate.
        assert!(escape_scroll().allows(&situation(1)));
        assert!(mine().allows(&situation(1)));
    }
}
