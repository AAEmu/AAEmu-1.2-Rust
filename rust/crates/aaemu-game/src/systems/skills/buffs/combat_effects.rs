use crate::server::GameServer;
use crate::world::WorldObject;

use super::super::types::DamageType;

/// The stealth state the 1.2 client caches per unit (`ClientBuffMan` +4 / +8, recomputed by
/// `ClientBuffMan::UpdateCache` and its per-buff callback, x2game `FUN_39387e00` /
/// `FUN_3989f230`): `stealthed` and `level` from the active `buffs.stealth` effects, and
/// `anti_stealth` from the active `buffs.anti_stealth` effects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StealthState {
    /// Any active effect whose template has `buffs.stealth`.
    pub stealthed: bool,
    /// Highest `ab_level` among those effects (the client's `stealth_level` = the instance
    /// word at +0x2E, the `sourceAbLevel` of `SCBuffCreated`).
    pub level: u32,
    /// Any active effect whose template has `buffs.anti_stealth` (the client's "cannot hide"
    /// byte at +8 that `CanDetect` reads).
    pub anti_stealth: bool,
}

/// The cached stealth state of `owner` (see [`StealthState`]).
pub fn stealth_state(server: &GameServer, owner: &WorldObject) -> StealthState {
    let data = &server.game_data.systems.skills;
    owner.with_base_unit(|b| {
        let mut state = StealthState::default();
        for e in &b.buffs().effects {
            let Some(t) = data.buff(e.template.id) else {
                continue;
            };
            if t.stealth {
                state.stealthed = true;
                // The client stores `sourceAbLevel` as a 16-bit word (+0x2E).
                state.level = state.level.max(u32::from(e.ab_level as u16));
            }
            if t.anti_stealth {
                state.anti_stealth = true;
            }
        }
        state
    })
}

/// Whether `owner` carries an active buff whose 1.2 template is a stealth effect
/// (`buffs.stealth`). The 1.2 client hides a unit by this column (`ClientUnit::ApplyStealth` /
/// `CanDetect`, x2game `FUN_3935c270` / `FUN_3935a2b0`), not by buff tag 100 (`TagsEnum.Stealth`);
/// 30 of the 71 stealth buffs lack tag 100 and 10 tag-100 buffs are not stealth.
pub fn has_stealth(server: &GameServer, owner: &WorldObject) -> bool {
    stealth_state(server, owner).stealthed
}

/// Whether `owner` carries an active `buffs.detect_stealth` effect. The column's rows say the
/// holder detects stealth ("stealth detection", 2232 / 2233), but the 1.2 client's `CanDetect`
/// does not read it; the server applies it as a reveal (see PORTING.md).
pub fn has_detect_stealth(server: &GameServer, owner: &WorldObject) -> bool {
    let data = &server.game_data.systems.skills;
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .any(|e| data.buff(e.template.id).is_some_and(|t| t.detect_stealth))
    })
}

/// `Buffs.GetAbsorptionEffects()` indices.
pub fn absorption_effects(server: &GameServer, owner: &WorldObject) -> Vec<u32> {
    let data = &server.game_data.systems.skills;
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .filter(|e| {
                data.buff(e.template.id)
                    .is_some_and(|t| t.damage_absorption_type_id > 0)
            })
            .map(|e| e.index)
            .collect()
    })
}

/// One live reflection buff of an owner whose `reflection_type_id` is the incoming hit's
/// `DamageType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflectionBuff {
    pub index: u32,
    pub buff_id: u32,
    /// `reflection_chance` (percent, 0-100).
    pub chance: i32,
    /// `reflection_target_ratio` (percent of the received damage returned to the attacker).
    pub target_ratio: i32,
}

/// The owner's live reflection buffs that match an incoming hit, in buff order.
///
/// `reflection_type_id` is the `DamageType` of the hit to reflect (1 Melee, 2 Magic, 3 Siege,
/// 4 Ranged; verified against the 1.2 rows and their tooltips). `reflection_chance` is the
/// percent chance to reflect and `reflection_target_ratio` the percent of the received damage
/// that is returned; `reflection_ratio` has no settled meaning and is not read here
/// (REFLECT-1, capture-pending; see `PORTING.md`).
pub fn reflection_effects(
    server: &GameServer,
    owner: &WorldObject,
    damage_type: DamageType,
) -> Vec<ReflectionBuff> {
    let data = &server.game_data.systems.skills;
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .filter(|e| !e.is_ended())
            .filter_map(|e| {
                let t = data.buff(e.template.id)?;
                (t.reflection_chance > 0 && t.reflection_type_id == damage_type as u32).then_some(
                    ReflectionBuff {
                        index: e.index,
                        buff_id: t.id,
                        chance: t.reflection_chance,
                        target_ratio: t.reflection_target_ratio,
                    },
                )
            })
            .collect()
    })
}
