use crate::models::buff::Buff;
use crate::server::GameServer;
use crate::world::{now_ms, PlayerRef, WorldObject};

use super::super::geometry::{distance_to, Target};
use super::super::packets::SCCombatFirstHitPacket;
use super::super::state::{Aggro, AggroKind};
use super::super::types::tags;
use super::super::{buffs, send_to_players};
use super::{clear_assault_list, mark_combat_activity, MAX_LOOTING_RANGE};

// ------------------------------------------------------------------ aggro

/// `Tagging.AddTagger(unit, damage)` on `owner`. A mate's damage tags its owner (C#); a slave's
/// tags the character in its gunner / driver seat, else its owner, so kills made with a cannon
/// credit the gunner (the C# tagged nobody for slave damage, so "Rout the Bloodhands" 2325,
/// "Use the Bloodhand cannons to defeat Bloodhand Pillagers", could not count a cannon kill).
fn add_tagger(server: &GameServer, owner: &WorldObject, unit: &WorldObject, damage: i32) {
    let Some(player) = tagging_credit(server, unit) else {
        return;
    };
    add_tagger_for(server, owner, &player, damage);
}

/// The character `unit`'s damage is tagged for: the unit itself, its mate's owner, its slave's
/// gunner / owner, or the character a gimmick acts for.
pub(crate) fn tagging_credit(server: &GameServer, unit: &WorldObject) -> Option<PlayerRef> {
    let mut check = unit.clone();
    if let Some(m) = unit.as_mate() {
        let owner_obj = m.data().owner_obj_id;
        if let Some(c) = server.world.character_by_obj_id(owner_obj) {
            check = WorldObject::from(c);
        }
    } else if let Some(s) = unit.as_slave() {
        let gunner = s
            .data()
            .attached_characters
            .iter()
            .find(|(point, _)| *point == crate::models::unit::attach_point::DRIVER)
            .map(|(_, id)| *id);
        let credited = gunner
            .and_then(|id| server.world.character_by_obj_id(id))
            .or_else(|| super::relation::owner_character(server, unit));
        if let Some(c) = credited {
            check = WorldObject::from(c);
        }
    }
    // A gimmick's damage is tagged for the character it acts for (`relation::credited_player`).
    super::relation::credited_player(server, &check)
}

fn add_tagger_for(server: &GameServer, owner: &WorldObject, player: &PlayerRef, damage: i32) {
    let social = server.systems.skills.ext.social();
    let team = social.team_of(server, player.obj_id);
    let max_hp = owner.with_unit(|u| u.unit().max_hp).unwrap_or(0);
    let owner_target = Target::Obj(owner.clone());
    // members of the team within looting range (their damage counts for the team)
    let team_members: Vec<u32> = if team != 0 {
        let (_, members) = social.team_members(server, team);
        members
            .into_iter()
            .filter(|m| {
                server.world.character_by_obj_id(*m).is_some_and(|c| {
                    distance_to(
                        server,
                        &Target::Obj(WorldObject::from(c)),
                        &owner_target,
                        true,
                    ) <= MAX_LOOTING_RANGE
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    owner.with_unit(|u| {
        let t = &mut u.unit_mut().skill_state.tagging;
        match t.taggers.iter_mut().find(|(id, _)| *id == player.obj_id) {
            Some((_, d)) => *d += damage,
            None => {
                t.taggers.push((player.obj_id, damage));
                if t.tagger == 0 {
                    t.tagger = player.obj_id;
                }
            }
        }
        if team != 0 {
            let party_damage: i32 = t
                .taggers
                .iter()
                .filter(|(id, _)| team_members.contains(id))
                .map(|(_, d)| *d)
                .sum();
            if f64::from(party_damage) > f64::from(max_hp) * 0.5 {
                t.tag_team = team;
            }
        } else if let Some((_, d)) = t.taggers.iter().find(|(id, _)| *id == player.obj_id) {
            if f64::from(*d) > f64::from(max_hp) * 0.5 {
                t.tagger = player.obj_id;
            }
        }
    });
}

/// `Unit.AddUnitAggro(kind, unit, amount)` on `owner`: `true` when the unit was already in the
/// table (the C# returns `isNewAggro = true` for an existing entry).
pub fn add_unit_aggro(
    server: &GameServer,
    owner: &WorldObject,
    kind: AggroKind,
    unit: &WorldObject,
    amount: i32,
) -> bool {
    add_aggro_amounts(server, owner, kind, unit, amount, None)
}

/// Damage effects award actual damage credit independently of their authored threat.
/// Other callers retain `add_unit_aggro`'s historical zero/negative amount semantics.
pub fn add_damage_aggro(
    server: &GameServer,
    owner: &WorldObject,
    unit: &WorldObject,
    damage: i32,
    threat: i32,
) -> bool {
    add_aggro_amounts(
        server,
        owner,
        AggroKind::Damage,
        unit,
        threat.max(0),
        Some(damage.max(0)),
    )
}

/// An owned sport fish never takes aggro or damage credit from a character other than its
/// owner, so its landing credits the owner. Every aggro-table insert, and with it
/// [`add_tagger`], passes through here (taunt and aggro copy included).
pub(crate) fn refuses_foreign_aggro(
    server: &GameServer,
    holder: &WorldObject,
    unit: &WorldObject,
) -> bool {
    let Some(npc) = holder.as_npc() else {
        return false;
    };
    crate::systems::economy::fishing::owned_sport_fish_owner(server, npc).is_some_and(|owner| {
        tagging_credit(server, unit).is_some_and(|credit| credit.character_id != owner)
    })
}

fn add_aggro_amounts(
    server: &GameServer,
    owner: &WorldObject,
    kind: AggroKind,
    unit: &WorldObject,
    amount: i32,
    damage_credit: Option<i32>,
) -> bool {
    if refuses_foreign_aggro(server, owner, unit) {
        return false;
    }
    if buffs::check_buff_tag(server, owner, tags::NO_FIGHT)
        || buffs::check_buff_tag(server, owner, tags::RETURNING)
    {
        clear_aggro_of_unit_quiet(server, owner, unit);
        return false;
    }
    if buffs::check_buff_tag(server, unit, tags::NO_FIGHT)
        || buffs::check_buff_tag(server, unit, tags::RETURNING)
    {
        clear_aggro_of_unit_quiet(server, owner, unit);
        return false;
    }
    if kind == AggroKind::Damage {
        add_tagger(server, owner, unit, damage_credit.unwrap_or(amount));
    }
    let aggro_mul = super::stats::stat(server, unit, super::stats::StatKind::AggroMul);
    let incoming = super::stats::stat(server, owner, super::stats::StatKind::IncomingAggroMul);
    let amount = (amount as f32 * (aggro_mul as f32 / 100.0)) as i32;
    let amount = (amount as f32 * (incoming as f32 / 100.0)) as i32;
    let unit_id = unit.obj_id();
    if damage_credit.is_some() && amount <= 0 {
        // A zero-threat hit still earns damage credit, but creates no aggro/watch.
        return owner
            .with_unit(|u| u.unit().skill_state.aggro_table.contains_key(&unit_id))
            .unwrap_or(false);
    }
    let existed = owner
        .with_unit(|u| {
            let table = &mut u.unit_mut().skill_state.aggro_table;
            match table.get_mut(&unit_id) {
                Some(a) => {
                    if damage_credit.is_some() {
                        a.damage_aggro = a.damage_aggro.saturating_add(amount);
                    } else {
                        a.add(kind, amount);
                    }
                    true
                }
                None => {
                    let mut a = Aggro::default();
                    a.add(kind, amount);
                    table.insert(unit_id, a);
                    false
                }
            }
        })
        .unwrap_or(false);
    let player = unit.as_player().cloned();
    if !existed {
        server
            .systems
            .skills
            .aggro_watch
            .lock()
            .expect("aggro watch poisoned")
            .entry(unit_id)
            .or_default()
            .insert(owner.obj_id());
        if let (Some(n), Some(p)) = (owner.as_npc(), &player) {
            let quest = {
                let npc = n.data();
                server
                    .systems
                    .skills
                    .ext
                    .npcs()
                    .npc_engage_combat_quest(server, &npc)
            };
            if quest > 0 {
                server
                    .systems
                    .skills
                    .ext
                    .quests()
                    .engage_combat_quest(server, p, quest, n.obj_id);
            }
        }
        // The first-hit packet puts both player participants in combat on the client,
        // even for sight aggro or a call for help that deals no damage.
        let now = now_ms();
        for participant in [owner, unit] {
            if participant.as_player().is_some() {
                mark_combat_activity(participant, now);
            }
        }
        send_to_players(
            server,
            &[owner.clone(), unit.clone()],
            &SCCombatFirstHitPacket {
                victim: owner.obj_id(),
                hitter: unit_id,
                hit_type: 0,
            },
        );
    }
    let Some(player) = player else {
        return existed;
    };
    let (total, dead) = owner
        .with_unit(|u| {
            let u = u.unit();
            (
                u.skill_state
                    .aggro_table
                    .get(&unit_id)
                    .map_or(0, Aggro::total),
                u.is_dead(),
            )
        })
        .unwrap_or((0, true));
    if total > 0 && !dead {
        let mut c = player.data();
        if !c.skill_char.is_in_aggro_list_of.contains(&owner.obj_id()) {
            c.skill_char.is_in_aggro_list_of.push(owner.obj_id());
        }
    }
    if let Some(n) = owner.as_npc() {
        server
            .systems
            .skills
            .ext
            .quests()
            .on_aggro(server, &player, n);
    }
    existed
}

/// `NpcAggroTable::TauntedBy(caster, copyTop)` of the 10.x zone server (10x-ONLY; the 1.2
/// client and the C# have no taunt logic, but buffs 171 / 502 and skill 17300 describe it).
///
/// Only an NPC holder has a stamped table (10x checks the victim's unit kind). 10x refuses
/// `CreateAggro` when the NPC cannot attack the caster; in the 1.2 data most PvE monsters are
/// faction-neutral to players and attack through AI aggression instead, so `CanAttack` alone
/// would refuse every taunt on them. Rust therefore refuses only an explicitly friendly caster
/// (plus the holder itself above). 10x additionally refuses members of the NPC's own group;
/// `npc_groups` is loaded by `npc/group_data.rs` for spawner expansion, but no runtime
/// membership links a spawned NPC back to its group, so there is nothing to check. Creates the
/// entry with the same side effects as a zero-threat `AddUnitAggro`, stamps
/// it, and with `copy_top` raises its total to the current highest total of the table (a one-off
/// copy, never above it).
pub fn taunted_by(server: &GameServer, npc: &WorldObject, caster: &WorldObject, copy_top: bool) {
    if npc.as_npc().is_none() || !caster.is_unit() || npc.same(caster) {
        return;
    }
    if super::relation::relation_to(
        server,
        &Target::Obj(npc.clone()),
        &Target::Obj(caster.clone()),
    ) == super::types::RelationState::Friendly
    {
        return;
    }
    add_unit_aggro(server, npc, AggroKind::Etc, caster, 0);
    let caster_id = caster.obj_id();
    // 10x uses its server clock. A stamp of 0 means "no taunt", so keep it non-zero; the
    // 64-bit unix-ms value never wraps (a `u32` stamp wrapped every ~49.7 days and broke the
    // latest-stamp order across the wrap).
    let stamp = now_ms().max(1) as u64;
    let stamped = npc
        .with_unit(|u| {
            let table = &mut u.unit_mut().skill_state.aggro_table;
            let top = if copy_top {
                table.values().map(Aggro::total).max().unwrap_or(0)
            } else {
                0
            };
            let Some(entry) = table.get_mut(&caster_id) else {
                return false;
            };
            entry.taunt_time = stamp;
            if copy_top {
                let total = entry.total();
                if top > total {
                    entry.etc_aggro = entry.etc_aggro.wrapping_add(top - total);
                }
            }
            true
        })
        .unwrap_or(false);
    if stamped {
        tracing::debug!(
            "taunt: npc {} locked on caster {} (copy_top = {copy_top})",
            npc.obj_id(),
            caster_id
        );
        crate::systems::ai::manager::post(server, npc.obj_id(), |c| c.on_aggro_target_changed());
    }
}

/// `NpcAggroTable::CancelTauntedBy(caster)` (10x-ONLY): clears the caster's stamp only when no
/// other taunt buff from that caster remains on the NPC. Threat is not reduced.
pub fn cancel_taunted_by(server: &GameServer, npc: &WorldObject, caster_obj_id: u32) {
    if npc.as_npc().is_none() || caster_obj_id == 0 {
        return;
    }
    let remaining = npc.with_base_unit(|b| {
        b.buffs().effects.iter().any(|e| {
            e.caster_obj_id == caster_obj_id
                && server
                    .game_data
                    .systems
                    .skills
                    .buff(e.template.id)
                    .is_some_and(|t| t.taunt)
        })
    });
    if remaining {
        return;
    }
    let cleared = npc
        .with_unit(|u| {
            u.unit_mut()
                .skill_state
                .aggro_table
                .get_mut(&caster_obj_id)
                .map(|e| {
                    let stamped = e.taunt_time != 0;
                    e.taunt_time = 0;
                    stamped
                })
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if cleared {
        tracing::debug!(
            "taunt: npc {} released caster {caster_obj_id}",
            npc.obj_id()
        );
        crate::systems::ai::manager::post(server, npc.obj_id(), |c| c.on_aggro_target_changed());
    }
}

/// The generic buff-start hook of a taunt buff: 10.x runs `TauntedBy` when the buff is created
/// on an NPC. Re-stamping a refresh is capture-pending (TAUNT-1; the 10x refresh path is
/// unknown).
pub fn on_taunt_buff_applied(server: &GameServer, owner: &WorldObject, b: &Buff) {
    let Some(template) = server.game_data.systems.skills.buff(b.template.id) else {
        return;
    };
    if !template.taunt || b.caster_obj_id == 0 {
        return;
    }
    let Some(caster) = buffs::find_obj(server, b.caster_obj_id) else {
        return;
    };
    taunted_by(server, owner, &caster, template.taunt_with_top_aggro);
}

/// The refresh hook of a taunt buff. The stack-rule overwrite replaces the effect's caster, so
/// a refresh by a second taunter would otherwise leave the previous caster's stamp on the NPC
/// for good: its buff is gone, and `on_removed` only cancels the current (new) caster. Release
/// the previous caster first (their other live taunt buffs, if any, keep the stamp), then stamp
/// the new caster like a fresh application.
pub fn on_taunt_buff_refreshed(
    server: &GameServer,
    owner: &WorldObject,
    b: &Buff,
    previous_caster_obj_id: u32,
) {
    let Some(template) = server.game_data.systems.skills.buff(b.template.id) else {
        return;
    };
    if !template.taunt {
        return;
    }
    if previous_caster_obj_id != 0 && previous_caster_obj_id != b.caster_obj_id {
        cancel_taunted_by(server, owner, previous_caster_obj_id);
    }
    on_taunt_buff_applied(server, owner, b);
}

/// `Unit.ClearAggroOfUnit(unit)` on `owner`.
pub fn clear_aggro_of_unit(server: &GameServer, owner: &WorldObject, unit: &WorldObject) {
    clear_aggro_of_unit_inner(server, owner, unit, true);
}

/// `ClearAggroOfUnit` for callers that legitimately reach it for a unit that is not in the
/// owner's table (the `AddUnitAggro` NoFight / Returning guards, the death fan-out of
/// `clear_incoming_aggro`): the C# logged "Failed to remove ..." for these as well, here the
/// absent entry is only a debug line.
fn clear_aggro_of_unit_quiet(server: &GameServer, owner: &WorldObject, unit: &WorldObject) {
    clear_aggro_of_unit_inner(server, owner, unit, false);
}

fn clear_aggro_of_unit_inner(
    server: &GameServer,
    owner: &WorldObject,
    unit: &WorldObject,
    warn_if_absent: bool,
) {
    if let Some(tp) = unit.as_player() {
        tp.data()
            .skill_char
            .is_in_aggro_list_of
            .retain(|id| *id != owner.obj_id());
        if let Some(me) = owner.as_player() {
            let (me_id, target_id) = (me.character_id, tp.character_id);
            me.data()
                .skill_char
                .assault_on
                .retain(|id| *id != target_id);
            tp.data().skill_char.assaulted_by.retain(|id| *id != me_id);
        }
    }
    let unit_id = unit.obj_id();
    let (last, removed, now_count) = owner
        .with_unit(|u| {
            let table = &mut u.unit_mut().skill_state.aggro_table;
            let last = table.len();
            let removed = table.remove(&unit_id).is_some();
            (last, removed, table.len())
        })
        .unwrap_or((0, false, 0));
    if last == 0 {
        return;
    }
    if removed {
        if let Some(set) = server
            .systems
            .skills
            .aggro_watch
            .lock()
            .expect("aggro watch poisoned")
            .get_mut(&unit_id)
        {
            set.remove(&owner.obj_id());
        }
    } else if warn_if_absent {
        tracing::warn!(
            "Failed to remove unit[{unit_id}] aggro from NPC[{}]",
            owner.obj_id()
        );
    } else {
        tracing::debug!(
            "Unit[{unit_id}] was not in the aggro table of NPC[{}]",
            owner.obj_id()
        );
    }
    if now_count != last {
        if let Some(n) = owner.as_npc() {
            server
                .systems
                .skills
                .ext
                .npcs()
                .npc_aggro_removed(server, n, unit_id);
        }
    }
}

/// `Unit.ClearAllAggro()` (taggers cleared, abuser event subscriptions dropped; the table
/// itself stays, as in the C# base implementation).
pub fn clear_all_aggro(server: &GameServer, owner: &WorldObject) {
    let ids: Vec<u32> = owner
        .with_unit(|u| {
            let s = &mut u.unit_mut().skill_state;
            s.tagging = Default::default();
            s.aggro_table.keys().copied().collect()
        })
        .unwrap_or_default();
    let mut watch = server
        .systems
        .skills
        .aggro_watch
        .lock()
        .expect("aggro watch poisoned");
    for id in ids {
        if let Some(set) = watch.get_mut(&id) {
            set.remove(&owner.obj_id());
        }
    }
    drop(watch);
    // Character.ClearAllAggro: AggroTable.Clear() + ClearAssaultList()
    if let Some(p) = owner.as_player() {
        p.data().unit.skill_state.aggro_table.clear();
        clear_assault_list(server, p);
    }
}

/// `List<uint>.Remove(value)` (first occurrence).
pub(super) fn remove_first(list: &mut Vec<u32>, value: u32) {
    if let Some(i) = list.iter().position(|v| *v == value) {
        list.remove(i);
    }
}

/// Remove the unit from every aggro table watching it. Death and FakeDeath share
/// this operation (native WZFakeDeath handler 0x3936F060).
pub(crate) fn clear_incoming_aggro(server: &GameServer, victim: &WorldObject) {
    let holders: Vec<u32> = server
        .systems
        .skills
        .aggro_watch
        .lock()
        .expect("aggro watch poisoned")
        .remove(&victim.obj_id())
        .map(|s| s.into_iter().collect())
        .unwrap_or_default();
    for h in holders {
        if let Some(holder) = server.world.object(h) {
            clear_aggro_of_unit_quiet(server, &holder, victim);
        }
    }
}

/// Empties `owner`'s aggro table and unregisters it from the `aggro_watch` reverse index of
/// every unit that was in it, so a later death of such a unit does not fan out to an owner
/// that no longer holds it. (Characters' lists and the NPC combat state are the caller's.)
pub fn drop_aggro_table(server: &GameServer, owner: &WorldObject) {
    let ids: Vec<u32> = owner
        .with_unit(|u| {
            let t = &mut u.unit_mut().skill_state.aggro_table;
            let ids = t.keys().copied().collect();
            t.clear();
            ids
        })
        .unwrap_or_default();
    unwatch_aggro(server, owner.obj_id(), &ids);
}

/// Removes `owner_id` from the `aggro_watch` sets of `abusers`.
pub(crate) fn unwatch_aggro(server: &GameServer, owner_id: u32, abusers: &[u32]) {
    let mut watch = server
        .systems
        .skills
        .aggro_watch
        .lock()
        .expect("aggro watch poisoned");
    for id in abusers {
        if let Some(set) = watch.get_mut(id) {
            set.remove(&owner_id);
            if set.is_empty() {
                watch.remove(id);
            }
        }
    }
}
