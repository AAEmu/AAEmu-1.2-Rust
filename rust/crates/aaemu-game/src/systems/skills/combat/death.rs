use crate::packets::world::SCUnitPointsPacket;
use crate::packets::world_objects::SCTargetChangedPacket;
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{now_ms, PlayerRef, WorldObject};

use super::super::geometry::Target;
use super::super::packets::{SCAiAggroPacket, SCUnitDeathPacket};
use super::super::packets_special::SCCharacterResurrectedPacket;
use super::super::types::{buff_constants, tags, BuffEventTriggerKind, KillReason};
use super::super::{broadcast, buffs, send_to_players};
use super::pvp::{award_pvp_honor, WAR_ZONE_HONOR_LOSS};
use super::{
    character_death_losses, clear_all_aggro, clear_incoming_aggro, compute_death_wait_time,
    death_penalty_applies, durability_targets, post_update_current_hp, set_in_battle,
    stop_auto_skill, try_claim_death,
};

/// `Unit.DoDie(killer, killReason)` with the `Npc` override around it.
///
/// The death is claimed first: a direct / scripted `DoDie` on a unit whose death already ran
/// for this life is refused (the C# ran the whole path again).
pub fn do_die(
    server: &GameServer,
    victim: &WorldObject,
    killer: Option<&WorldObject>,
    kill_reason: KillReason,
) {
    if !try_claim_death(victim) {
        // A scripted removal already ended this life; a direct DoDie on it is a quiet no-op
        // (the unit was scripted-removed, not killed twice).
        if victim
            .with_unit(|u| u.unit().scripted_removed)
            .unwrap_or(false)
        {
            return;
        }
        let name = victim.with_base_unit(|b| b.name().to_string());
        tracing::warn!(
            "DoDie refused for {name} ({}): death already handled for this life",
            victim.obj_id()
        );
        return;
    }
    do_die_after_claim(server, victim, killer, kill_reason);
}

/// `Unit.DoDie` body of the caller that owns the death claim (see [`try_claim_death`]).
pub(super) fn do_die_after_claim(
    server: &GameServer,
    victim: &WorldObject,
    killer: Option<&WorldObject>,
    kill_reason: KillReason,
) {
    // `DespawnOnCreatorDeath`: the dying unit's skill summons go with it (systems::npcs). This
    // runs before the Slave / house override returns below so every death path handles them.
    server
        .systems
        .skills
        .ext
        .npcs()
        .on_unit_died(server, victim);
    if crate::systems::housing::dominion::runtime::handle_tower_death(server, victim) {
        return;
    }
    // `Slave.DoDie` replaces the base implementation (vehicles subsystem).
    if crate::systems::vehicles::on_do_die(server, victim, killer, kill_reason) {
        return;
    }
    let npc_hooks = server.systems.skills.ext.npcs();
    if let Some(n) = victim.as_npc() {
        n.data().dead_time_ms = now_ms();
        npc_hooks.npc_before_death(server, n, killer);
    }
    super::skill::interrupt_skills(server, victim);
    // A dead unit no longer occupies a `once_one_*` doodad (cast or seat).
    crate::systems::npcs::doodad::occupancy::release_all_of(server, victim.obj_id());
    set_in_battle(server, victim, false);
    // Events.OnDeath (buff triggers listening to the victim, `Shipyard.OnDeath`)
    buffs::fire_unit_event(server, victim.obj_id(), BuffEventTriggerKind::Death, 0);
    // killerUnit?.Events.OnKill(this, { Killer, Victim }) (quest subscribers of a player; a
    // gimmick's kill goes to the character it acts for)
    if let Some(kp) = killer.and_then(|k| super::relation::credited_player(server, k)) {
        crate::systems::quests::events::on_kill(
            server,
            &kp,
            None,
            Some(&WorldObject::from(&kp)),
            Some(victim),
        );
    }
    // ParentWorld.Events.OnUnitKilled + killerUnit?.Events.OnKill (systems::instances).
    crate::systems::instances::events::on_do_die(server, victim, killer);
    // House.OnDeath
    crate::systems::housing::on_unit_died(server, victim);
    crate::systems::vehicles::on_death_event(server, victim);
    // Abusers that died leave the aggro tables they are in (OnAbuserDied).
    clear_incoming_aggro(server, victim);
    // A resurrection buff (`buffs.resurrection_health` / `resurrection_mana`; 1.2 buff 239
    // "Nui's Favor") brings the player back in place. It carries `remove_on_death` too, so it
    // is consumed here, before `RemoveEffectsOnDeath`; the resurrection itself runs after the
    // death losses and `DoDie` so it can refund this death's experience. A death without a
    // killer returns below before `DoDie` (the C# NullReference path), so it does not consume
    // the buff.
    let resurrection = if killer.is_some() && victim.as_player().is_some() {
        buffs::take_resurrection_buff(server, victim)
    } else {
        None
    };
    buffs::remove_effects_on_death(server, victim);

    let mut lost_exp = 0;
    let mut durability_loss = 0u8;
    let mut wait = 0;
    if let Some(p) = victim.as_player() {
        // Escalating respawn timer — runs BEFORE base.DoDie sends SCUnitDeathPacket
        compute_death_wait_time(server, p, now_ms());
        // A dead player cannot finish a trade; the client shows "Trade canceled." for the
        // generic reason (it only suppresses its own copy while loading).
        crate::systems::economy::trade::on_player_died(server, p);
        // Death penalties are PvE-only in 1.2. A kill by a `no_penalty` NPC (guards, siege
        // cannons, battlefield pets) and a PvP kill (another player, or a mate / slave owned by
        // one) take no exp and no death durability loss; the killing blow's attribution is the
        // killer object (a skill / DoT passes its original caster), so a lethal DoT counts.
        let penalty = death_penalty_applies(server, p, killer);
        (lost_exp, durability_loss) = character_death_losses(server, p, penalty);
        wait = p.data().rez_wait_duration;
    }
    if let Some(n) = victim.as_npc() {
        wait = npc_hooks.npc_respawn_time(server, n);
    }
    let Some(killer) = killer else {
        tracing::error!(
            "DoDie of {} without a killer (NullReferenceException in C#)",
            victim.obj_id()
        );
        if let Some(n) = victim.as_npc() {
            npc_hooks.npc_after_death(server, n, None);
        }
        return;
    };
    do_die_base(
        server,
        victim,
        killer,
        kill_reason,
        wait,
        lost_exp,
        durability_loss,
    );
    if let Some(n) = victim.as_npc() {
        npc_hooks.npc_after_death(server, n, Some(killer));
    }
    if let Some(p) = victim.as_player() {
        // ApplyDurabilityLossToEquipment(LastDurabilityLoss, All, -1f)
        if durability_loss > 0 {
            server.systems.skills.ext.items().durability_loss(
                server,
                p,
                i32::from(durability_loss),
                durability_targets::ALL,
                -1.0,
            );
        }
        character_death_tail(server, p, victim, killer);
        if let Some((health, mana, percent)) = resurrection {
            apply_resurrection_buff(server, p, victim, health, mana, percent, lost_exp);
        }
    }
}

/// The in-place self-resurrection of a `resurrection_health` / `resurrection_mana` buff on a
/// player death, after [`character_death_losses`] and `DoDie`: hp / mp restore, this death's
/// experience refund through the `RecoverExpEffect` path (no priest, no labor) and
/// `SCCharacterResurrectedPacket` at the corpse position, so the client leaves the
/// death-release state.
///
/// `percent == true` reads the values as percent of `max_hp` / `max_mp` (buff 239: 10 / 10);
/// absolute values (`percent == false`) are INFERRED (no 1.2 row uses them) and clamped to
/// the pools. The buff description says the decreased experience is recovered; the whole loss
/// of this death is refunded and the death's `recoverable_exp` entry is cleared, so a priest
/// cannot refund it a second time. The timing / animation of the self-resurrection is
/// capture-pending (no 1.2 capture): there is no extra delay.
fn apply_resurrection_buff(
    server: &GameServer,
    p: &PlayerRef,
    victim: &WorldObject,
    health: i32,
    mana: i32,
    percent: bool,
    lost_exp: i32,
) {
    let (hp, mp) = {
        let mut c = p.data();
        let max_hp = c.unit.max_hp;
        let max_mp = c.unit.max_mp;
        let restore = |value: i32, max: i32| {
            let value = if percent {
                (i64::from(max) * i64::from(value) / 100) as i32
            } else {
                value
            };
            value.clamp(0, max)
        };
        c.unit.hp = restore(health, max_hp);
        c.unit.mp = restore(mana, max_mp);
        // The flags `ApplyRevivalDebuffs` consumes on a temple resurrection: an in-place
        // resurrection has no death debuffs, and the next death must start with clean flags.
        c.died_in_pvp = false;
        c.died_in_pvp_war_zone = false;
        (c.unit.hp, c.unit.mp)
    };
    if hp <= 0 {
        // Only buff 239 has either column set in the 1.2 data (10 / 10). A row without
        // resurrection health cannot make the corpse alive; the buff is spent either way.
        tracing::warn!(
            "resurrection buff of {} restored no health (resurrection_health 0); not resurrecting",
            p.obj_id
        );
        return;
    }
    if lost_exp > 0 {
        {
            // Only this death's entry: `character_death_losses` overwrote the previous one.
            let mut c = p.data();
            c.recoverable_exp = 0;
        }
        p.send(super::progress::SCRecoverableExpPacket {
            obj_id: p.obj_id,
            recoverable_exp: 0,
            penaltied_exp: 0,
            reason: 1,
        });
        super::progress::add_exp(server, p, lost_exp, false);
    }
    let world_pos = match super::geometry::world_of(server, victim) {
        Some(w) => w.world_pos_rot(victim),
        None => victim.transform().local(),
    };
    broadcast(
        server,
        victim,
        &SCCharacterResurrectedPacket {
            unit_id: victim.obj_id(),
            x: world_pos.x,
            y: world_pos.y,
            z: world_pos.z,
            z_rot: world_pos.rot_z,
        },
        true,
    );
    broadcast(
        server,
        victim,
        &SCUnitPointsPacket {
            obj_id: victim.obj_id(),
            hp,
            mp,
        },
        true,
    );
    // Clears the death claim of the new life and runs the HP-change hooks of the resurrection.
    post_update_current_hp(server, victim, Some(victim), 0, hp, KillReason::Unknown);
}

/// The part of `Unit.DoDie` after the death events (the death packet, loot, target cleanup).
fn do_die_base(
    server: &GameServer,
    victim: &WorldObject,
    killer: &WorldObject,
    kill_reason: KillReason,
    wait: i32,
    lost_exp: i32,
    durability_loss: u8,
) {
    let killer_info = killer.is_unit().then(|| {
        (
            killer.obj_id(),
            killer.with_base_unit(|b| b.name().to_string()),
        )
    });
    broadcast(
        server,
        killer,
        &SCUnitDeathPacket {
            obj_id: victim.obj_id(),
            kill_reason,
            resurrect_wait_time: wait,
            lost_exp,
            durability_loss_rate: durability_loss,
            killer: killer_info,
        },
        true,
    );
    if killer.same(victim) {
        // DespawnMate(owner of the mate / the character) (vehicles subsystem)
        crate::systems::vehicles::mates::despawn_mate_on_death(server, victim);
        return;
    }
    // LootingContainer.GenerateLoot(killer) (NPC loot: npcs -> economy).
    if let Some(n) = victim.as_npc() {
        crate::systems::npcs::npc::death::generate_loot(server, n, Some(killer));
    }
    let current_target = victim.with_unit(|u| u.unit().current_target).unwrap_or(0);
    if current_target != 0 {
        send_to_players(
            server,
            &[victim.clone(), killer.clone()],
            &SCAiAggroPacket {
                npc_id: killer.obj_id(),
                count: 0,
                hostile_unit_id: 0,
                summarize_damage: 0,
                val2: 0,
                val3: 0,
            },
        );
        if killer.is_unit() {
            let killer_target = killer
                .with_unit(|u| {
                    u.unit_mut().summarize_damage = 0;
                    u.unit().current_target
                })
                .unwrap_or(0);
            if killer_target != 0 {
                if let Some(t) = server
                    .world
                    .object(killer_target)
                    .filter(WorldObject::is_unit)
                {
                    set_in_battle(server, &t, false);
                }
            }
            set_in_battle(server, killer, false);
        }
        broadcast(
            server,
            killer,
            &SCTargetChangedPacket {
                obj_id: killer.obj_id(),
                target_obj_id: 0,
            },
            true,
        );
        if let Some(p) = victim.as_player() {
            stop_auto_skill(p);
            set_in_battle(server, victim, false);
            p.data().dead_time = chrono::Utc::now().timestamp();
            crate::systems::vehicles::mates::despawn_mate_on_death(server, victim);
        }
        if let Some(kp) = killer.as_player() {
            let kt = kp.data().unit.current_target;
            if victim.as_player().is_some() && kt == victim.obj_id() {
                stop_auto_skill(kp);
                set_in_battle(server, killer, false);
            }
        }
        killer.with_unit(|u| u.unit_mut().current_target = 0);
    }
}

/// The quest `OnZoneKill` of a player killed by another player's hand (the
/// `QuestActObjZoneKill` PvP quests, `count_player_kill > 0`). The C# only raised it for NPC
/// deaths (`DoOnMonsterHuntEvents`), so those quests could not progress. Only the killer gets
/// the direct event; `team_share` acts hand it on to the killer's team.
fn raise_pvp_zone_kill(
    server: &GameServer,
    killer: &PlayerRef,
    victim: &WorldObject,
    zone_group_id: u32,
) {
    use crate::systems::quests::events::{kill_unit, with_quests, QuestEvent};
    let event = QuestEvent::ZoneKill {
        zone_group_id,
        credited_character_id: killer.character_id,
        killer: kill_unit(server, &WorldObject::from(killer.clone()), None),
        victim: kill_unit(server, victim, None),
    };
    let raised = with_quests(server, killer, None, |ctx| {
        if let Err(e) = ctx.dispatch(&event) {
            tracing::error!("Quest event ZoneKill threw: {}", e.0);
        }
    });
    if raised.is_none() {
        tracing::warn!(
            "PvP zone kill of {} not credited to {}: a quest operation is in progress",
            victim.obj_id(),
            killer.obj_id
        );
    }
}

/// `Character.DoDie` after `base.DoDie` (and the durability loss): PvP kill bookkeeping
/// (`HostileFactionKills`, `AwardPvpHonor`, the `DiedInPvp` / `DiedInPvpWarZone` flags,
/// `SCUnitPvPPointsChangedPacket`s, the War-zone honor loss of the victim) or crime evidence
/// for a friendly-fire kill (unless it was retaliation), `DropTradePackToFloor`,
/// `ClearAllAggro()`, the damage history reset and the arrest of a wanted character killed by
/// another character.
fn character_death_tail(
    server: &GameServer,
    victim_p: &PlayerRef,
    victim: &WorldObject,
    killer: &WorldObject,
) {
    use crate::data::world_data::conflict_type;
    // Resolve the victim's zone-conflict state once for both PvP-honor award and War-zone
    // honor-loss
    let zone_key = victim_p.data().unit.transform.zone_id;
    let victim_zone_group = server
        .game_data
        .zones
        .zone_by_key(zone_key)
        .map(|z| z.group_id);
    let conflict = victim_zone_group.and_then(|g| server.systems.social.conflicts.get(g));
    let zone_state = conflict
        .as_ref()
        .map_or(conflict_type::PEACE, |c| c.current_state);

    let relation = super::relation::relation_to(
        server,
        &Target::Obj(killer.clone()),
        &Target::Obj(victim.clone()),
    );
    // A gimmick's kill counts for the character it acts for (`relation::credited_player`).
    let arrestor = super::relation::credited_player(server, killer);
    if let Some(enemy) = &arrestor {
        if relation != super::types::RelationState::Friendly {
            enemy.data().hostile_faction_kills += 1;
            award_pvp_honor(
                server,
                victim_p,
                enemy,
                victim_zone_group,
                conflict.as_ref(),
                zone_state,
            );
            // Mark victim as PvP death (prevents Weakened Body debuff on temple-revive)
            {
                let mut c = victim_p.data();
                c.died_in_pvp = true;
                if zone_state == conflict_type::WAR {
                    c.died_in_pvp_war_zone = true;
                }
            }
            raise_pvp_zone_kill(server, enemy, victim, victim_zone_group.unwrap_or(0));
            // DEAD_BY_PVP (value1: the zone group the victim died in, 0 = any zone).
            crate::systems::report_achievement(
                server,
                victim_p,
                AchievementInput::Increment {
                    kind_id: char_record_kind::DEAD_BY_PVP,
                    amount: 1,
                    matcher: RecordMatch::ZoneGroup {
                        zone_group: victim_zone_group.unwrap_or(0),
                    },
                },
            );
            // Broadcast PvP stats: kind=0 → HonorGainedInCombat, kind=1 → HostileFactionKills
            let (honor, kills) = {
                let c = enemy.data();
                (c.honor_gained_in_combat, c.hostile_faction_kills)
            };
            let enemy_obj = WorldObject::from(enemy);
            for (kind, point) in [(0u8, honor), (1u8, kills)] {
                broadcast(
                    server,
                    &enemy_obj,
                    &crate::packets::g2c_misc::SCUnitPvPPointsChangedPacket {
                        unit_id: enemy.obj_id,
                        kind,
                        point: point as i32,
                    },
                    true,
                );
            }
            // Victim loses honor in War zone: the flat loss scaled by `HonorPointLoseMul`,
            // clamped at 0 and at the current honor (one lock inside the helper).
            if zone_state == conflict_type::WAR {
                let name = victim_p.data().unit.name.clone();
                let loss = super::progress::lose_honor_on_death(victim_p, WAR_ZONE_HONOR_LOSS);
                if loss > 0 {
                    tracing::debug!("PvP Death: {name} lost {loss} honor (War zone death)");
                }
            }
        } else {
            // Friendly-fire kill -> generate crime evidence (unless retaliation)
            let killer_owner = super::relation::owner_character(server, killer);
            if let Some(owner) = killer_owner {
                let retaliation = victim_p
                    .data()
                    .skill_char
                    .assaulted_by
                    .contains(&owner.character_id);
                if !retaliation {
                    server.systems.social.crimes.generate_evidence_from_kill(
                        server,
                        killer,
                        Some(&owner),
                        Some(victim_p),
                    );
                }
            }
        }
    }
    server
        .systems
        .skills
        .ext
        .items()
        .drop_trade_pack_to_floor(server, victim_p);
    clear_all_aggro(server, victim);
    // Clear damage history on death (heal history is intentionally kept)
    victim_p.data().skill_char.pvp_damage_history.clear();
    // Arrest if wanted
    let Some(arrestor) = arrestor else {
        return;
    };
    if !buffs::check_buff_tag(server, victim, tags::WANTED) {
        return;
    }
    // KILL_WANTED: the killer of a wanted victim, whether or not the arrest goes through.
    crate::systems::report_achievement(
        server,
        &arrestor,
        AchievementInput::Increment {
            kind_id: char_record_kind::KILL_WANTED,
            amount: 1,
            matcher: RecordMatch::Any,
        },
    );
    let contemptuous =
        victim.with_base_unit(|b| b.buffs().check_buff(buff_constants::CONTEMPTUOUS));
    let arrest = if !contemptuous {
        // If not a pirate arrest regardless
        true
    } else {
        // If a pirate, only arrest in faction zones
        let zones = &server.game_data.zones;
        let desperado = zones
            .zone_by_key(zone_key)
            .and_then(|z| zones.group(z.group_id))
            .is_some_and(|g| g.pirate_desperado);
        !desperado
    };
    if arrest
        && server
            .systems
            .social
            .trials
            .arrest_criminal(server, victim_p, Some(&arrestor))
            .is_some()
    {
        // The wanted player's accumulated bounty goes to the arresting player, and only when a
        // court case really started: `arrest_criminal` answers `None` when the region has no
        // court room or jail (or both parties are outside the justice system). An NPC, guard or
        // mob kill has no `arrestor` (it returned above) and a pirate killed where the arrest is
        // skipped pays nothing (BOUNTY-4).
        server
            .systems
            .social
            .bounties
            .settle_arrest(server, victim_p, &arrestor);
    }
}
