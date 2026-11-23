use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{now_ms, PlayerRef, WorldObject};

use super::super::broadcast;
use super::super::state::AggroKind;
use super::super::types::SkillCaster;
use super::add_unit_aggro;
use super::aggro::remove_first;

/// `DeathWaitTimesSeconds`: the C# port's hard-coded escalation (15..240 s) and its 5-minute
/// reset. The 1.2 `resurrection_waiting_times` table replaces it; these values are only the
/// fallback when a compact database has no rows (the C# behaviour), so an old database does not
/// start granting instant resurrections.
const LEGACY_DEATH_WAIT_TIMES_SECONDS: [i32; 9] = [15, 30, 60, 90, 120, 150, 180, 210, 240];
const LEGACY_DEATH_COUNT_RESET_MS: i64 = 5 * 60_000;
/// `PvpAssistWindowSeconds`.
const PVP_ASSIST_WINDOW_MS: i64 = 30_000;
/// `WarZoneHonorLoss` (clamped to the victim's current honor).
pub(super) const WAR_ZONE_HONOR_LOSS: i32 = 10;

/// True when the killing object is an NPC whose template has `no_penalty` (guards, siege
/// cannons, battlefield pets, alliance NPCs). Killer attribution passes the original caster for
/// skill / DoT hits, so the flag follows the source the death packet names.
fn killer_is_no_penalty(server: &GameServer, killer: Option<&WorldObject>) -> bool {
    let Some(npc) = killer.and_then(WorldObject::as_npc) else {
        return false;
    };
    let template_id = npc.data().unit.template_id;
    server
        .game_data
        .systems
        .npcs
        .npc
        .template(template_id)
        .is_some_and(|t| t.no_penalty)
}

/// True when the killing object is another player or a unit owned by one (mate / slave, e.g. a
/// manned siege cannon). 1.2 "death by player" is a PvP death and carries no exp or death
/// durability loss (see [`character_death_losses`]); a self-inflicted death (`killer` is the
/// victim, e.g. fall damage) is not PvP and keeps the penalty.
fn killer_is_player(server: &GameServer, victim: &PlayerRef, killer: Option<&WorldObject>) -> bool {
    let Some(killer) = killer else {
        return false;
    };
    super::relation::owner_character(server, killer)
        .is_some_and(|owner| owner.character_id != victim.character_id)
}

/// Whether a player death pays the PvE exp / durability loss: false for a `no_penalty` NPC and
/// for a PvP kill (another player, or that player's mate / slave). A self-inflicted death
/// (`killer` is the victim, e.g. fall damage) keeps the penalty.
pub(in crate::systems::skills) fn death_penalty_applies(
    server: &GameServer,
    victim: &PlayerRef,
    killer: Option<&WorldObject>,
) -> bool {
    !killer_is_no_penalty(server, killer) && !killer_is_player(server, victim, killer)
}

/// The player's zone is a zone group whose dominion siege is running (`SiegeState::active_at`).
fn player_died_in_active_siege(server: &GameServer, zone_key: u32, now: i64) -> bool {
    let group = server
        .game_data
        .zones
        .zone_by_key(zone_key)
        .map_or(0, |z| z.group_id);
    server
        .systems
        .housing
        .dominions
        .get(group)
        .and_then(|state| state.siege)
        .is_some_and(|siege| siege.active_at(now.div_euclid(1000)))
}

/// `RecordPvpDamageFrom(attacker)`.
pub fn record_pvp_damage_from(victim: &PlayerRef, attacker_id: u32) {
    record_at(
        &mut victim.data().skill_char.pvp_damage_history,
        attacker_id,
        now_ms(),
    );
}

/// `RecordPvpHealFrom(healer)`.
pub fn record_pvp_heal_from(target: &PlayerRef, healer_id: u32) {
    record_at(
        &mut target.data().skill_char.pvp_heal_history,
        healer_id,
        now_ms(),
    );
}

fn record_at(history: &mut Vec<(u32, i64)>, id: u32, now: i64) {
    match history.iter_mut().find(|(i, _)| *i == id) {
        Some(e) => e.1 = now,
        None => history.push((id, now)),
    }
}

/// `ComputeDeathWaitTime()`: stores the escalating wait in `RezWaitDuration` and `DeadTime`.
///
/// The 1.2 `resurrection_waiting_times` rows drive it (see
/// [`crate::systems::skills::data::ResurrectionWaitingTime`]): the 1-based consecutive-death
/// step selects the row, `penalty_duration` is the window that keeps the step alive, and the
/// zone's active dominion siege selects `siege_waiting_time` over `waiting_time`. With no rows
/// (an old or fixture database) the port's old hard-coded table is kept.
pub fn compute_death_wait_time(server: &GameServer, p: &PlayerRef, now: i64) {
    let zone_key = p.data().unit.transform.zone_id;
    let siege = player_died_in_active_siege(server, zone_key, now);
    let data = &server.game_data.systems.skills;
    let mut c = p.data();
    let s = &mut c.skill_char;
    // The window belongs to the step the previous death reached (row 1 before the first
    // death); every real row carries the same 600 s.
    let prev_step = u32::try_from(s.consecutive_death_count.max(1)).unwrap_or(u32::MAX);
    let penalty_ms = data
        .resurrection_waiting_time(prev_step)
        .map_or(LEGACY_DEATH_COUNT_RESET_MS, |row| {
            i64::from(row.penalty_duration) * 1000
        });
    if s.last_death_time_ms != 0 && now - s.last_death_time_ms >= penalty_ms {
        s.consecutive_death_count = 0;
    }
    let step = u32::try_from(s.consecutive_death_count.saturating_add(1)).unwrap_or(u32::MAX);
    let wait_ms = match data.resurrection_waiting_time(step) {
        Some(row) => {
            let seconds = if siege {
                row.siege_waiting_time
            } else {
                row.waiting_time
            };
            i64::from(seconds) * 1000
        }
        None => {
            let index = s
                .consecutive_death_count
                .min(LEGACY_DEATH_WAIT_TIMES_SECONDS.len() - 1);
            i64::from(LEGACY_DEATH_WAIT_TIMES_SECONDS[index]) * 1000
        }
    };
    s.consecutive_death_count += 1;
    s.last_death_time_ms = now;
    let count = s.consecutive_death_count;
    c.rez_wait_duration = i32::try_from(wait_ms).unwrap_or(i32::MAX);
    c.dead_time = now.div_euclid(1000);
    tracing::debug!(
        "Death #{count} for {}: respawn wait = {} ms (siege: {siege})",
        c.unit.name,
        c.rez_wait_duration
    );
}

/// The loss part of `Character.DoDie` before `base.DoDie`: `LastDurabilityLoss`
/// (`GetDeathDurabilityLossRatio()` from `MinimumExpLossLevel` on) and, below the max level in
/// the main world, `LastExpLoss` (`GetExpLoss(level, ExpLossRateAtDeath)` capped to the exp of
/// the current level, 80 % of it recoverable; a free resurrection offer below
/// `MinimumExpLossLevel`). Returns `(LastExpLoss, LastDurabilityLoss)`.
///
/// `penalty == false` for a PvE-exempt death: a kill by a `no_penalty` NPC or by a player
/// (PvP, including that player's mate / slave). Exp and durability are then not touched, but
/// the stale resurrection offer is still cleared and the free resurrection below the minimum
/// level is still offered. 1.2 "death by player" has no negative effects beyond the respawn
/// timer (fandom Combat "Death by Player"; Ars Technica 2014-09 "If you die in PvP, you lose
/// 0 XP"; mmogah "no experience or item durability"; the 1.2 patch notes list the exp-loss
/// exemptions only for PvE sources). The Nov 2014 siege death dialog agrees: "You lost 0 XP"
/// (`video-evidence/siege-2014-11-30-ollo.md` §2.8, grade C).
pub(in crate::systems::skills) fn character_death_losses(
    server: &GameServer,
    p: &PlayerRef,
    penalty: bool,
) -> (i32, u8) {
    let world = &server.config.get().world;
    let exp_table = &server.game_data.experience;
    let (level, experience, instance_id) = {
        let c = p.data();
        (c.unit.level, c.experience, c.unit.transform.instance_id)
    };
    let qualifies = i32::from(level) >= world.minimum_exp_loss_level;
    let durability_loss = if qualifies && penalty {
        server.game_data.items.config.death_durability_loss_ratio
    } else {
        0
    };
    let mut lost_exp = 0;
    let mut recoverable = p.data().recoverable_exp;
    // An offer left over from an earlier death (a GM revive does not consume it) must not carry
    // over: only this death's free resurrection or a new Resurrection effect grants one.
    p.data().skill_char.resurrect_offer = false;
    if level < exp_table.max_player_level()
        && instance_id == crate::models::character::DEFAULT_INSTANCE_ID
    {
        if qualifies {
            if penalty {
                // ExperienceManager.GetExpLoss(level, rate)
                let this_level = exp_table.exp_for_level(level, false);
                let next_level = exp_table.exp_for_level(level.saturating_add(1), false);
                let loss = (f64::from(next_level - this_level)
                    * f64::from(world.exp_loss_rate_at_death))
                .floor() as i32;
                lost_exp = loss.min(experience - this_level);
                recoverable = (lost_exp as f32 * 0.80f32).round() as i32;
                p.data().recoverable_exp = recoverable;
            }
        } else {
            // Free resurrect below level 10
            p.send(super::packets_special::SCNotifyResurrectionPacket {
                caster: SkillCaster::Unit(p.obj_id),
            });
            p.data().skill_char.resurrect_offer = true;
        }
    }
    if penalty && (lost_exp > 0 || recoverable > 0) {
        p.data().experience -= lost_exp;
        p.send(super::progress::SCRecoverableExpPacket {
            obj_id: p.obj_id,
            recoverable_exp: recoverable,
            penaltied_exp: lost_exp,
            reason: 1, // KillReason.Damage
        });
        if lost_exp > 0 {
            p.send(super::packets::SCExpChangedPacket {
                obj_id: p.obj_id,
                exp: -lost_exp,
                should_add_ability_exp: false,
            });
        }
    }
    (lost_exp, durability_loss)
}

/// `CollectAssists(killer)`: players who damaged the victim recently, players who healed the
/// killer recently (both within `PvpAssistWindowSeconds`, the killer excluded) and the
/// character casters of active crowd-control debuffs on the victim.
pub fn collect_assists(
    server: &GameServer,
    victim: &PlayerRef,
    killer: &PlayerRef,
    now: i64,
) -> Vec<u32> {
    let mut assists: Vec<u32> = Vec::new();
    let mut add = |id: u32| {
        if !assists.contains(&id) {
            assists.push(id);
        }
    };
    let cutoff = now - PVP_ASSIST_WINDOW_MS;
    let killer_id = killer.character_id;
    let (damage, bad_casters) = {
        let c = victim.data();
        let damage = c.skill_char.pvp_damage_history.clone();
        let bad: Vec<(u32, u32)> = c
            .unit
            .buffs
            .all_buffs(false)
            .map(|(_, bad, _)| {
                bad.iter()
                    .filter_map(|b| b.caster.map(|(id, _)| (id, b.template.id)))
                    .collect()
            })
            .unwrap_or_default();
        (damage, bad)
    };
    // 1) Players who damaged the victim recently (excluding the killer)
    for (id, time) in damage {
        if id != killer_id && time >= cutoff {
            add(id);
        }
    }
    // 2) Players who healed the killer recently (excluding the killer themselves)
    let heals = killer.data().skill_char.pvp_heal_history.clone();
    for (id, time) in heals {
        if id != killer_id && time >= cutoff {
            add(id);
        }
    }
    // 3) Active CC-debuff casters on the victim
    let skills = &server.game_data.systems.skills;
    for (caster_id, buff_id) in bad_casters {
        // `buff.Caster is not Character ccCaster` (a non-character caster has no character id)
        if caster_id == 0 || caster_id == killer_id {
            continue;
        }
        if let Some(t) = skills.buff(buff_id) {
            if t.stun || t.root || t.sleep || t.silence || t.crippled {
                add(caster_id);
            }
        }
    }
    assists
}

/// `AwardPvpHonor(killer, victimZone, conflictData, zoneState)`: Conflict 10 solo (6 killer +
/// 4 each assist), War 20 solo (16 killer + 4 each assist), no honor in other states. The kill
/// is registered with the zone conflict **in every state**: `conflict_zones.num_kills_0..4`
/// (70/100/140/190/250 for zone groups 14-27) escalate Tension -> Danger -> Dispute -> Unrest ->
/// Crisis -> Conflict, so the counter must see the kills after the timer chain returns to
/// Tension. The 1.2 C# (and the previous port) called `AddZoneKill` only from the honor branch,
/// i.e. in Conflict / War, where `ZoneConflict.AddZoneKill` ignores it; the zone then stayed in
/// Tension forever after its first Peace.
pub(super) fn award_pvp_honor(
    server: &GameServer,
    victim: &PlayerRef,
    killer: &PlayerRef,
    victim_zone_group: Option<u32>,
    conflict: Option<&crate::data::world_data::ZoneConflict>,
    zone_state: u8,
) {
    use crate::data::world_data::conflict_type;
    // Register zone kill (drives zone state escalation) in Tension..Crisis too.
    if let Some(c) = conflict {
        server
            .systems
            .social
            .conflicts
            .add_zone_kill(server, c.zone_group_id);
    }
    let (solo, killer_share, assist_share) = match zone_state {
        conflict_type::CONFLICT => (10, 6, 4),
        conflict_type::WAR => (20, 16, 4),
        // No honor outside Conflict/War zones
        _ => return,
    };
    let rate = server.config.get().world.pvp_honor_rate;
    let assist_ids = collect_assists(server, victim, killer, now_ms());
    // Resolve to online assistants before choosing the kill path (offline assists fall back
    // to the solo award).
    let online: Vec<PlayerRef> = assist_ids
        .iter()
        .filter_map(|id| server.world.character_by_id(*id))
        .collect();
    let round = |share: i32| (f64::from(share) * rate).round_ties_even() as i32;
    let gain = |p: &PlayerRef, honor: i32| {
        // PvP / conflict honor is the War source; `pvp_honor_rate` stays in the shares above.
        super::progress::grant_honor(server, p, super::progress::HonorSource::War, honor);
        let total = {
            let mut c = p.data();
            c.honor_gained_in_combat = c.honor_gained_in_combat.wrapping_add(honor as u32);
            c.honor_gained_in_combat
        };
        // GET_HONOR_POINT (zone-conflict honor: value1 0, value2 the zone group or 0 = any).
        crate::systems::report_achievement(
            server,
            p,
            AchievementInput::Increment {
                kind_id: char_record_kind::GET_HONOR_POINT,
                amount: i64::from(honor),
                matcher: RecordMatch::Value1OrZeroValue2 {
                    value1: 0,
                    value2: victim_zone_group.unwrap_or(0),
                },
            },
        );
        total
    };
    if !online.is_empty() {
        let killer_honor = round(killer_share);
        if killer_honor > 0 {
            gain(killer, killer_honor);
        }
        let assist_honor = round(assist_share);
        if assist_honor > 0 {
            for assistant in &online {
                let total = gain(assistant, assist_honor);
                broadcast(
                    server,
                    &WorldObject::from(assistant),
                    &crate::packets::g2c_misc::SCUnitPvPPointsChangedPacket {
                        unit_id: assistant.obj_id,
                        kind: 0,
                        point: total as i32,
                    },
                    true,
                );
            }
        }
    } else {
        let honor = round(solo);
        if honor > 0 {
            gain(killer, honor);
        }
    }
    if let Some(group) = victim_zone_group {
        // The conflict-zone honor UI keys on ZoneGroupId.
        let sum = killer.data().honor_gained_in_combat;
        killer.send(
            crate::packets::g2c_misc::SCConflictZoneHonorPointSumPacket {
                zone_id: group as u16,
                sum: sum as i32,
            },
        );
    }
}

/// `Character.ClearAssaultList()`: this character leaves the assault lists of the characters
/// it assaulted / was assaulted by, then both of its lists are cleared.
pub fn clear_assault_list(server: &GameServer, p: &PlayerRef) {
    let (me, assaulted_by, assault_on) = {
        let mut c = p.data();
        (
            c.id,
            std::mem::take(&mut c.skill_char.assaulted_by),
            std::mem::take(&mut c.skill_char.assault_on),
        )
    };
    for criminal_id in assaulted_by {
        if let Some(criminal) = server.world.character_by_id(criminal_id) {
            remove_first(&mut criminal.data().skill_char.assault_on, me);
        }
    }
    for victim_id in assault_on {
        if let Some(victim) = server.world.character_by_id(victim_id) {
            remove_first(&mut victim.data().skill_char.assaulted_by, me);
        }
    }
}

/// `OnAbuserHealed`: every unit that has `healed` in its aggro table gains heal aggro from the
/// healer.
pub fn on_abuser_healed(
    server: &GameServer,
    healed: &WorldObject,
    healer: &WorldObject,
    amount: i32,
) {
    let holders: Vec<u32> = server
        .systems
        .skills
        .aggro_watch
        .lock()
        .expect("aggro watch poisoned")
        .get(&healed.obj_id())
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default();
    for h in holders {
        if let Some(holder) = server.world.object(h) {
            add_unit_aggro(server, &holder, AggroKind::Heal, healer, amount);
        }
    }
}
