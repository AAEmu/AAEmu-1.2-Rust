use crate::server::GameServer;
use crate::world::{now_ms, WorldObject};

use super::super::types::{buff_constants, KillReason};
use super::super::{broadcast, buffs};
use super::reduce_current_hp;

/// Whether any live buff of `unit` has the `buffs.fall_damage_immune` flag (the 1.2 client's
/// `LoadBuffDescs` selects and reads the column, `dll.c:1579862,1579986`; its own consumer was
/// not traced, so its effect on fall damage is INFERRED from the column and the 185 rows).
fn has_fall_damage_immune_buff(server: &GameServer, unit: &WorldObject) -> bool {
    let ids: Vec<u32> =
        unit.with_base_unit(|b| b.buffs().effects.iter().map(|e| e.template.id).collect());
    ids.into_iter().any(|id| {
        server
            .game_data
            .systems
            .skills
            .buff(id)
            .is_some_and(|t| t.fall_damage_immune)
    })
}

/// `Unit.DoFallDamage(fallVel)` (+ the `Character` override that ignores fall damage from
/// `IgnoreFallDamageAccessLevel` up). Returns the damage dealt. Called by the movement code.
///
/// Rust-only guards (PORTING.md, `systems::skills (fall damage)`). The C# curve
/// `MaxHp * (fallVel - 8600) / 15000` is negative for every client `fallVel` below 8600, so a
/// small jump landing healed the unit, and `ReduceCurrentHp` did not stop it:
/// * a fall that does not exceed `pl_fallDamage_SpeedSafe` (20 m/s, `fall::SPEED_SAFE`, the
///   threshold of the zone's own fall check) deals no damage and sends no `SCEnvDamagePacket`;
/// * the damage after `FallDamageMul` is never negative: the buff / item modifiers of that
///   attribute run from -25 % to +50 % and stack, so the sum can go below -100 %.
/// * an active buff whose template has `fall_damage_immune` (185 rows, e.g. buff 1649 "Immune
///   to fall damage") negates the whole fall: no HP loss, stun or `SCEnvDamagePacket`.
pub fn do_fall_damage(server: &GameServer, unit: &WorldObject, fall_vel: u16) -> i32 {
    if crate::systems::ai::collision::fall::is_safe_fall_vel(fall_vel) {
        return 0;
    }
    if has_fall_damage_immune_buff(server, unit) {
        tracing::debug!("FallDamage negated because of a fall-damage-immune buff");
        return 0;
    }
    if let Some(p) = unit.as_player() {
        let access = p.data().access_level;
        if access >= server.config.world.ignore_fall_damage_access_level {
            tracing::debug!("FallDamage negated because of IgnoreFallDamageAccessLevel settings");
            return 0;
        }
    }
    let Some((hp, max_hp, mul)) = unit.with_unit(|u| {
        let u = u.unit();
        (
            u.hp,
            u.max_hp,
            u.calculate_with_bonuses(
                0.0,
                crate::models::unit::UnitAttribute::FallDamageMul,
                now_ms(),
            ) / 100.0,
        )
    }) else {
        return 0;
    };
    let mut fall_dmg =
        max_hp.min((max_hp as f32 * ((i32::from(fall_vel) - 8600) as f32 / 15000.0)) as i32);
    let min_hp_left = max_hp / 20;
    let max_dmg_left = hp - min_hp_left;
    fall_dmg = ((f64::from(fall_dmg) + f64::from(fall_dmg) * mul) as i32).max(0);
    // A fall the multiplier cancelled is no fall: no stun of a unit at the 5 % floor, no packet.
    if fall_dmg == 0 && fall_vel < 32000 {
        return 0;
    }
    if fall_vel >= 32000 {
        reduce_current_hp(server, unit, Some(unit), hp, KillReason::Damage);
    } else if fall_dmg < max_dmg_left {
        reduce_current_hp(server, unit, Some(unit), fall_dmg, KillReason::Damage);
    } else {
        if min_hp_left == 0 {
            tracing::error!("DoFallDamage: MaxHp / 20 == 0 (DivideByZeroException)");
            return fall_dmg;
        }
        let duration = 500 * (fall_dmg / min_hp_left);
        buffs::add_buff_by_id(server, unit, buff_constants::FALL_STUN, unit, duration);
        if hp > min_hp_left {
            reduce_current_hp(server, unit, Some(unit), max_dmg_left, KillReason::Damage);
        }
    }
    broadcast(
        server,
        unit,
        &super::packets::SCEnvDamagePacket {
            source: super::packets::env_source::FALLING,
            target: unit.obj_id(),
            amount: fall_dmg as u32,
            gimmick_id: 0,
        },
        true,
    );
    fall_dmg
}
