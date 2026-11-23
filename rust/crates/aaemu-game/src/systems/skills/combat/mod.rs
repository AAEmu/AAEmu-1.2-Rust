//! Unit combat behaviour: `Unit.ReduceCurrentHp` (+ the `Character` override),
//! `PostUpdateCurrentHp`, `ReduceCurrentMp`, `DoDie` (+ the npc hooks around it), `IsInBattle`,
//! aggro (`AddUnitAggro`, `ClearAggroOfUnit`, `ClearAllAggro`, `Tagging.AddTagger`),
//! `SetCriminalState`, `SetForceAttack`, `SetFaction`, `SetInvisible`, `OnZoneChange` (zone
//! group buffs).

use crate::models::buff::Buff;
use crate::packets::world::SCUnitPointsPacket;
use crate::server::GameServer;
use crate::world::{now_ms, PlayerRef, WorldObject};

use self::death::do_die_after_claim;
use super::packets::{
    SCCombatClearedPacket, SCForceAttackSetPacket, SCUnitFactionChangedPacket,
    SCUnitInvisiblePacket,
};
use super::types::{buff_constants, KillReason, SkillCaster};
use super::{
    broadcast, buffs, geometry, packets, packets_special, progress, relation, skill, stats, types,
};

mod aggro;
mod death;
mod fall_damage;
mod pvp;

pub(super) use self::aggro::add_damage_aggro_with_combat;
pub use self::aggro::{
    add_damage_aggro, add_unit_aggro, cancel_taunted_by, clear_aggro_of_unit, clear_all_aggro,
    drop_aggro_table, on_taunt_buff_applied, on_taunt_buff_refreshed, taunted_by,
};
pub(crate) use self::aggro::{
    clear_incoming_aggro, refuses_foreign_aggro, tagging_credit, unwatch_aggro,
};
pub use self::death::do_die;
pub use self::fall_damage::do_fall_damage;
pub(super) use self::pvp::{character_death_losses, death_penalty_applies};
pub use self::pvp::{
    clear_assault_list, collect_assists, compute_death_wait_time, on_abuser_healed,
    record_pvp_damage_from, record_pvp_heal_from,
};

/// `LootingContainer.MaxLootingRange`.
pub const MAX_LOOTING_RANGE: f32 = 200.0;

fn unit_hp_mp(obj: &WorldObject) -> Option<(i32, i32)> {
    obj.with_unit(|u| (u.unit().hp, u.unit().mp))
}

/// `Unit.IsInBattle = value` (leaving combat broadcasts `SCCombatClearedPacket`).
pub fn set_in_battle(server: &GameServer, obj: &WorldObject, value: bool) {
    let changed = obj
        .with_unit(|u| {
            let u = u.unit_mut();
            if u.is_in_battle == value {
                false
            } else {
                u.is_in_battle = value;
                true
            }
        })
        .unwrap_or(false);
    if changed && !value {
        broadcast(
            server,
            obj,
            &SCCombatClearedPacket {
                obj_id: obj.obj_id(),
            },
            true,
        );
    }
}

/// Refreshes the timeout for a unit that the client sees enter combat.
pub(super) fn mark_combat_activity(unit: &WorldObject, now: i64) {
    unit.with_unit(|u| {
        let u = u.unit_mut();
        if !u.is_dead() {
            u.is_in_battle = true;
            u.last_combat_activity_ms = now;
        }
    });
}

/// `Character.ReduceCurrentHp` / `Unit.ReduceCurrentHp(attacker, value, killReason)`.
///
/// Rust-only bounds: damage never heals. The C# applied `Hp = Max(Hp - value, 0)` to any value,
/// so a negative one (a fall damage formula below zero) raised `Hp` past `MaxHp`. A negative
/// `value` is no damage here (heals go through `effects::combat::heal`), and the result stays
/// within `[0, max_hp]` (a unit whose `max_hp` is 0, i.e. not known, is not capped).
pub fn reduce_current_hp(
    server: &GameServer,
    target: &WorldObject,
    attacker: Option<&WorldObject>,
    mut value: i32,
    kill_reason: KillReason,
) {
    if value < 0 {
        tracing::warn!(
            "negative damage {value} on {} treated as 0 ({kill_reason:?})",
            target.obj_id()
        );
        value = 0;
    }
    let cfg = server.systems.skills.config();
    if let Some(p) = target.as_player() {
        if cfg.god_mode {
            tracing::debug!(
                "{}'s damage disabled because of GodMode flag (normal damage: {value})",
                p.data().unit.name
            );
            return;
        }
        let in_duel = p.data().unit.is_in_duel;
        if in_duel {
            let mut c = p.data();
            c.unit.hp = (c.unit.hp - value).max(1);
            value = 0;
        }
        // PvP assist tracking: remember who hit us recently
        if let Some(enemy) = attacker.and_then(WorldObject::as_player) {
            if value > 0 && enemy.character_id != p.character_id {
                record_pvp_damage_from(p, enemy.character_id);
            }
        }
    }
    // base.ReduceCurrentHp
    let Some((hp, _)) = unit_hp_mp(target) else {
        return;
    };
    if hp <= 0 {
        return;
    }
    for index in buffs::absorption_effects(server, target) {
        value = buffs::consume_charge(server, target, index, value);
    }
    // Re-check and apply the damage under the same data lock: a concurrent lethal hit may
    // already have transitioned the unit since the early read above. The loser of such a
    // race does nothing at all (no points packet, no death side effects).
    let Some((old_hp, new_hp, mp, was_alive)) = target.with_unit(|u| {
        let u = u.unit_mut();
        if u.hp <= 0 || u.scripted_removed {
            return (0, 0, 0, false);
        }
        // A living unit starts a new life. A claim left over from an earlier life whose
        // HP was restored outside `post_update_current_hp` (repair / load paths) must not
        // swallow this death.
        u.death_processed = false;
        let old_hp = u.hp;
        u.hp = (old_hp - value).max(0);
        if u.max_hp > 0 {
            u.hp = u.hp.min(u.max_hp);
        }
        (old_hp, u.hp, u.mp, true)
    }) else {
        return;
    };
    if !was_alive {
        tracing::debug!(
            "damage on {} ignored: the unit is dead or scripted-removed",
            target.obj_id()
        );
        return;
    }
    broadcast(
        server,
        target,
        &SCUnitPointsPacket {
            obj_id: target.obj_id(),
            hp: new_hp,
            mp: if new_hp > 0 { mp } else { 0 },
        },
        true,
    );
    post_update_current_hp(server, target, attacker, old_hp, new_hp, kill_reason);

    // Character armor durability
    if let Some(p) = target.as_player() {
        use KillReason as K;
        let rate = match kill_reason {
            K::PvpSiege | K::PvpBattleField | K::PvpHonorWar | K::PvpEnemy | K::PvpAlly => {
                cfg.pvp_durability_loss_rate
            }
            K::Unknown | K::Damage => {
                if attacker.is_some_and(|a| a.as_player().is_some()) {
                    cfg.pvp_durability_loss_rate
                } else {
                    cfg.pve_durability_loss_rate
                }
            }
            _ => 0.0,
        };
        if rate > 0.0 {
            let items = server.systems.skills.ext.items();
            let rate = rate * items.durability_decrement_chance(server);
            items.durability_loss(server, p, 1, durability_targets::ALL_ARMOR, rate);
        }
    }
}

/// `DurabilityLossTargets` (the C# enum values).
pub mod durability_targets {
    pub const ALL: u8 = 0;
    pub const ALL_ARMOR: u8 = 1;
    pub const ALL_WEAPONS: u8 = 2;
    pub const ALL_MAIN_WEAPONS: u8 = 3;
    pub const PRIMARY_WEAPON: u8 = 4;
    pub const SECONDARY_WEAPON: u8 = 5;
    pub const RANGED_WEAPON: u8 = 6;
    pub const SHIELD: u8 = 7;
    pub const FEET: u8 = 8;
}

/// `Unit.PostUpdateCurrentHp(attacker, oldHp, newHp, killReason)`.
pub fn post_update_current_hp(
    server: &GameServer,
    target: &WorldObject,
    attacker: Option<&WorldObject>,
    old_hp: i32,
    new_hp: i32,
    kill_reason: KillReason,
) {
    let Some((triggers, max_hp, hp)) = target.with_unit(|u| {
        let u = u.unit_mut();
        let hp = u.hp;
        if hp > 0 {
            // HP is back (resurrection, repair, character load): the next death is a new
            // transition. Covers callers that set HP directly and only report it here.
            u.death_processed = false;
        }
        (u.hp_trigger_points_percent.clone(), u.max_hp, hp)
    }) else {
        return;
    };
    if target.as_slave().is_some() {
        use crate::systems::vehicles::slaves::repair_point_health_band;
        let new_band = repair_point_health_band(new_hp, max_hp);
        if repair_point_health_band(old_hp, max_hp) != new_band {
            crate::systems::vehicles::on_hp_change_trigger(server, target, new_band);
        }
    } else if !triggers.is_empty() && max_hp != 0 {
        let old_p = (old_hp as f32 * 100.0 / max_hp as f32).round_ties_even() as i32;
        let new_p = (new_hp as f32 * 100.0 / max_hp as f32).round_ties_even() as i32;
        // DoHpChangeTrigger: the vehicle overrides (slave repair points); npc AI hooks it.
        if old_p > new_p {
            if let Some(t) = triggers.iter().find(|t| old_p > **t && new_p <= **t) {
                tracing::trace!("hp trigger {t} (down) on {}", target.obj_id());
                crate::systems::vehicles::on_hp_change_trigger(server, target, *t);
            }
        }
        if old_p < new_p {
            if let Some(t) = triggers.iter().find(|t| old_p < **t && new_p >= **t) {
                tracing::trace!("hp trigger {t} (up) on {}", target.obj_id());
                crate::systems::vehicles::on_hp_change_trigger(server, target, *t);
            }
        }
    }
    if old_hp != new_hp {
        crate::systems::housing::dominion::runtime::on_hp_changed(
            server, target, attacker, old_hp, new_hp,
        );
    }
    if let Some(house) = target.as_house() {
        crate::systems::housing::manager::mark_hp_changed(house, old_hp, new_hp);
        let model_max_hp = {
            let h = house.data();
            crate::systems::ai::collision::objects::house_collision_max_hp(&h)
        };
        let objects = crate::systems::ai::collision::objects::house_model_state;
        if objects(old_hp, model_max_hp) != objects(new_hp, model_max_hp) {
            crate::systems::ai::collision::objects::sync_house(server, house);
        }
    }
    if hp > 0 {
        return;
    }
    // The alive -> dead transition is claimed exactly once per life: two lethal hits that
    // raced past the HP checks above must not both run the kill events or `DoDie`.
    if !try_claim_death(target) {
        tracing::debug!(
            "death of {} ignored: already handled for this life",
            target.obj_id()
        );
        return;
    }
    // attackerUnit.Events.OnKill(attackerUnit, { Target = attackerUnit }) (sic, C#); the
    // dungeon `OnUnitKilled` world event is the instances subsystem's. A gimmick's kill counts
    // for the character it acts for (`relation::credited_player`).
    if let Some(ap) = attacker.and_then(|a| super::relation::credited_player(server, a)) {
        crate::systems::quests::events::on_kill(
            server,
            &ap,
            Some(&WorldObject::from(&ap)),
            None,
            None,
        );
    }
    if attacker.is_some_and(WorldObject::is_unit) {
        // `attackerUnit.Events.OnKill` + dungeon `OnUnitKilled` / `OnUnitCombatEnd` / `OnDeath`
        // (systems::instances).
        crate::systems::instances::events::on_kill_in_reduce_hp(server, target);
    }
    do_die_after_claim(server, target, attacker, kill_reason);
}

/// Atomically claims the alive -> dead transition of `target` for the current life.
///
/// The object's data lock serializes every caller that can kill (skill damage, DoTs / regen,
/// fall damage, GM commands, PvP, NPC deaths): exactly one caller gets `true` and runs
/// [`do_die_after_claim`], every later lethal hit of the same life gets `false` and does
/// nothing. The flag is cleared when HP is restored or a living unit is damaged again, so an
/// object (a repaired tower, a resurrected character) can die once per life. A scripted
/// removal (`UnitBase::scripted_removed`) is final for the life, so it refuses every claim.
///
/// Rust-only: the C# `ReduceCurrentHp` checked `Hp <= 0`, applied the damage and called
/// `DoDie` without a lock, so two concurrent lethal hits could both run the death path.
pub fn try_claim_death(target: &WorldObject) -> bool {
    target
        .with_unit(|u| {
            let u = u.unit_mut();
            if u.death_processed || u.scripted_removed {
                false
            } else {
                u.death_processed = true;
                true
            }
        })
        .unwrap_or(false)
}

/// `ReduceCurrentMp(unit, value)`.
///
/// Deviation (the C# broadcast every time): a change that leaves `Mp` unchanged sends no
/// `SCUnitPoints`. The 1.2 client predicts its own regen between packets
/// (`X2::Unit::UpdateManaRegen`, `FUN_390b63f0` @ 0x390b63f0), so a packet for a zero change
/// (a 0-mana item use such as a regrade scroll) only snaps the bar back to the server's older
/// value while the client has already predicted further ahead. See PORTING.md.
pub fn reduce_current_mp(server: &GameServer, target: &WorldObject, value: i32) {
    let Some((hp, old_mp, mp)) = target
        .with_unit(|u| {
            let u = u.unit_mut();
            if u.hp == 0 {
                return None;
            }
            let old = u.mp;
            u.mp = (u.mp - value).max(0);
            Some((u.hp, old, u.mp))
        })
        .flatten()
    else {
        return;
    };
    if mp == old_mp {
        return;
    }
    broadcast(
        server,
        target,
        &SCUnitPointsPacket {
            obj_id: target.obj_id(),
            hp,
            mp,
        },
        true,
    );
}

/// `StopAutoSkill(unit)`: cancels the auto attack task of a character.
pub fn stop_auto_skill(p: &PlayerRef) {
    let c = p.data();
    if let Some(a) = &c.unit.skill_state.auto_attack {
        a.cancel.cancel();
    }
}

// ------------------------------------------------------------------ states

/// `Unit.SetCriminalState(criminalState, attackedTarget)`.
pub fn set_criminal_state(
    server: &GameServer,
    unit: &WorldObject,
    criminal: bool,
    attacked: &WorldObject,
) {
    if criminal {
        if attacked.as_npc().is_some_and(|n| n.data().portal.is_none()) {
            return;
        }
        buffs::add_buff_by_id(server, unit, buff_constants::RETRIBUTION, unit, 0);
    } else {
        buffs::remove_buff(server, unit, buff_constants::RETRIBUTION);
    }
}

/// `Unit.SetForceAttack(value)`.
pub fn set_force_attack(server: &GameServer, unit: &WorldObject, value: bool) {
    // A repeated request changes nothing: no buff churn and no broadcast.
    if unit.with_unit(|u| u.unit().force_attack) == Some(value) {
        return;
    }
    unit.with_unit(|u| u.unit_mut().force_attack = value);
    if value {
        buffs::add_buff_by_id(server, unit, buff_constants::BLOODLUST, unit, 0);
    } else {
        buffs::remove_buff(server, unit, buff_constants::BLOODLUST);
    }
    broadcast(
        server,
        unit,
        &SCForceAttackSetPacket {
            obj_id: unit.obj_id(),
            on: value,
        },
        true,
    );
}

/// `Unit.SetInvisible(value)`.
pub fn set_invisible(server: &GameServer, unit: &WorldObject, value: bool) {
    unit.with_unit(|u| u.unit_mut().invisible = value);
    broadcast(
        server,
        unit,
        &SCUnitInvisiblePacket {
            obj_id: unit.obj_id(),
            invisible: value,
        },
        true,
    );
}

/// `Unit.SetFaction(factionId)` (the `Character` override returns early for the same faction
/// and kicks from foreign guilds / parties: social subsystem).
pub fn set_faction(server: &GameServer, unit: &WorldObject, faction_id: u32) {
    set_faction_with_temp(server, unit, faction_id, false);
}

/// `Unit.SetFaction(factionId)` with the `SCUnitFactionChangedPacket.temp` flag: a temporary
/// change (duel Red / Blue team, battlefield corps) keeps the *first* origin faction until it
/// is restored and is never persisted by `db_character::save_character`.
pub fn set_faction_with_temp(server: &GameServer, unit: &WorldObject, faction_id: u32, temp: bool) {
    let Some((old, name)) = unit.with_unit(|u| {
        let u = u.unit();
        (u.faction_id, u.name.clone())
    }) else {
        return;
    };
    if old == faction_id {
        tracing::info!("SetFaction: faction has already been established factionId={faction_id}");
        if !temp {
            // A persistent change to the faction a temporary change already shows makes that
            // faction permanent (it is then saved and no longer restored).
            unit.with_unit(|u| u.unit_mut().faction_temporary = false);
        }
    } else {
        unit.with_unit(|u| {
            let u = u.unit_mut();
            // Nested temporary changes (a duel inside a battlefield) keep the true origin.
            if !u.faction_temporary || !temp {
                u.origin_faction_id = old;
            }
            u.faction_id = faction_id;
            u.faction_temporary = temp;
        });
        broadcast(
            server,
            unit,
            &SCUnitFactionChangedPacket {
                obj_id: unit.obj_id(),
                unit_name: name,
                id: old,
                id2: faction_id,
                temp,
            },
            true,
        );
        if faction_id == 161 {
            // FactionsEnum.Pirate
            buffs::add_buff_by_id(server, unit, buff_constants::CONTEMPTUOUS, unit, 0);
        } else {
            buffs::remove_buff(server, unit, buff_constants::CONTEMPTUOUS);
        }
    }
    // Npc attacks the characters around it (quest 2486): systems::ai
    if let (Some(n), Some(ai)) = (unit.as_npc(), server.systems.hooks.ai.get()) {
        ai.npc_faction_set(server, n);
    }
}

/// `Unit.OnZoneChange(lastZoneKey, newZoneKey)`: zone group buffs.
pub fn on_zone_change(server: &GameServer, unit: &WorldObject, last_zone: u32, new_zone: u32) {
    let zones = &server.game_data.zones;
    let last = zones.zone_by_key(last_zone).map_or(0, |z| z.group_id);
    let new = zones.zone_by_key(new_zone).map_or(0, |z| z.group_id);
    if last as i16 == new as i16 {
        return;
    }
    let data = &server.game_data.systems.skills;
    if zones.zone_by_key(last_zone).is_some() {
        if let Some(&buff) = data.zone_group_buffs.get(&last) {
            buffs::remove_buff(server, unit, buff);
        }
    }
    if zones.zone_by_key(new_zone).is_some() {
        if let Some(&buff) = data.zone_group_buffs.get(&new) {
            if let Some(t) = data.buff(buff).cloned() {
                let b = buffs::new_buff(
                    server,
                    &t,
                    Some(unit),
                    &SkillCaster::Unit(unit.obj_id()),
                    None,
                    now_ms(),
                );
                buffs::add_buff(server, unit, b, 0, 0);
            }
        }
    }
}

/// `Unit.CombatTick`: leaves combat `DefaultCombatTimeout` after the last combat activity;
/// characters also end their post-cast state 5 s after the last cast.
pub fn combat_tick(server: &GameServer, unit: &WorldObject) {
    let now = now_ms();
    let leave = unit
        .with_unit(|u| {
            let u = u.unit_mut();
            if u.is_in_battle
                && u.last_combat_activity_ms + super::relation::DEFAULT_COMBAT_TIMEOUT_MS < now
            {
                u.is_in_battle = false;
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if leave {
        broadcast(
            server,
            unit,
            &SCCombatClearedPacket {
                obj_id: unit.obj_id(),
            },
            true,
        );
    }
    if let Some(p) = unit.as_player() {
        let mut c = p.data();
        if c.skill_char.is_in_post_cast && c.skill_char.last_cast_ms + 5000 < now {
            c.skill_char.is_in_post_cast = false;
        }
    }
}

/// A buff of `unit` by id (helper).
pub fn buff_of(unit: &WorldObject, buff_id: u32) -> Option<Buff> {
    unit.with_base_unit(|b| b.buffs().effect_from_buff_id(buff_id).cloned())
}
