use crate::models::unit::{SkillAttribute, UnitCooldowns};
use crate::server::GameServer;
use crate::world::{now_ms, PlayerRef, WorldObject};

use super::super::packets::{SCCooldownsPacket, SCSkillCooldownResetPacket};
use super::super::relation::owner_character;
use super::super::state::{SkillRef, SkillRun};
use super::super::stats::{stat, StatKind};
use super::{apply_skill_modifiers, RECALL_SKILL};

/// Sets the caster's global cooldown from the skill template (`default_gcd` / `custom_gcd`
/// scaled by the caster's `GlobalCooldownMul`). Skipped when the skill bypasses the GCD.
///
/// `cast` starts it, except for a player's timed cast, which `use_skill` starts at acceptance
/// because the 1.2 client counts the GCD from the cast start (also when the cast is cancelled).
pub(super) fn start_global_cooldown(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    now: i64,
) {
    if skill.state().bypass_gcd {
        return;
    }
    let t = &skill.template;
    let mut gcd = t.custom_gcd;
    if t.default_gcd {
        gcd = if caster.as_npc().is_some() {
            1500
        } else {
            1000
        };
    }
    let mul = stat(server, caster, StatKind::GlobalCooldownMul) as f32;
    let ms = (gcd as f32 * (mul / 100.0)) as i64;
    caster.with_unit(|u| u.unit_mut().global_cooldown_ms = now + ms);
}

/// A gimmick (a thrown bomb, a rolling rock) is a projectile: `owner_character` maps it to the
/// character that threw it for the PvP and credit rules, but it is not a unit that character
/// controls, so it has no skill cooldowns of its own (a contact gimmick that keeps rolling hits
/// every unit it touches, whatever the skill's `cooldown_time`).
pub(super) fn cooldown_exempt(caster: &WorldObject) -> bool {
    caster.as_gimmick().is_some()
}

fn cooldown_ignored(server: &GameServer, caster: &WorldObject) -> bool {
    caster
        .as_player()
        .cloned()
        .or_else(|| owner_character(server, caster))
        .is_some_and(|owner| owner.data().skill_char.ignore_skill_cooldowns)
}

pub(super) fn on_skill_cooldown(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    now: i64,
) -> bool {
    if cooldown_exempt(caster) {
        return false;
    }
    // Preserve the existing server-controlled NPC policy: only a character or its
    // controlled unit is subject to skill cooldown admission, on that unit's own store.
    let owner = caster
        .as_player()
        .cloned()
        .or_else(|| owner_character(server, caster));
    let Some(owner) = owner else {
        return false;
    };
    if owner.data().skill_char.ignore_skill_cooldowns {
        return false;
    }
    caster
        .with_unit(|u| {
            u.unit_mut().cooldowns.check_skill_cooldown(
                skill.template.id,
                skill.template.cooldown_tag_id.max(0) as u32,
                now,
            )
        })
        .unwrap_or(false)
}

pub(super) fn start_skill_cooldown(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    now: i64,
) {
    if cooldown_exempt(caster) || cooldown_ignored(server, caster) {
        return;
    }
    // Native 1.2 `3989A360` -> `3989F900`, attribute 1: modify the started skill's
    // own cooldown, not each member's authored duration. Never cast a signed duration to u32.
    let duration = apply_skill_modifiers(
        server,
        skill,
        caster,
        SkillAttribute::Cooldown,
        f64::from(skill.template.cooldown_time),
    )
    .clamp(0.0, f64::from(u32::MAX)) as u32;
    if duration == 0 {
        return;
    }
    let tag = skill.template.cooldown_tag_id.max(0) as u32;
    // Native `3939b010` (OnSkillFired) starts the client-side cooldown itself when the packet's
    // f bit0 (`+0x154`, DEC 750636) and trailing flag bit1 (`+0x160`, DEC 750694) are 0 and the
    // template has `toggle_buff_id == 0` (`+0x1c0`, compared with the zero-initialised
    // DAT_3a17770c) and `timing_id != 0` (`+0x1bc`). Toggle skills' cooldown is started by
    // `OnBuffRemoved` (`39389b70` -> `39368f90`). A `plot_only` cast with a plot never sends
    // `SCSkillFired`, so the client cannot start it and the server must send the update.
    // Sending an entry the client already appended would duplicate it: `39899d40` does not
    // de-duplicate and drops additions once the list holds 150.
    let fires = !(skill.template.plot_only && skill.template.plot_id.is_some());
    let client_starts_cooldown =
        fires && (skill.template.toggle_buff_id != 0 || skill.template.timing_id != 0);
    let packet = caster.with_unit(|u| {
        let cooldowns = &mut u.unit_mut().cooldowns;
        cooldowns.add_cooldown(skill.template.id, duration, now);
        cooldowns.add_tag_cooldown(tag, duration, now);
        // Native 1.2 397B2350 keeps these lists separate; 3989A290 takes
        // max(individual, tag). Never manufacture individual entries for other members.
        // A client-started skill needs no packet at all: `39899f90` writes the individual entry
        // or the tag entry from the client's own template, so sending either one duplicates it.
        let mut affected = UnitCooldowns::default();
        if !client_starts_cooldown {
            affected
                .cooldowns
                .insert(skill.template.id, cooldowns.cooldowns[&skill.template.id]);
            if tag != 0 {
                affected
                    .tag_cooldowns
                    .insert(tag, cooldowns.tag_cooldowns[&tag]);
            }
        }
        SCCooldownsPacket::from_cooldowns(&affected, now)
    });
    if let Some(packet) = packet.filter(|p| !p.is_empty()) {
        // OnCooldowns (native 391D43C0) addresses the local character's manager;
        // unlike SkillCooldownReset, the packet has no subject object ID. A mate's
        // shared timers must not populate its owner's character cooldown store.
        if let Some(player) = caster.as_player() {
            player.send(packet);
        }
    }
}

/// Replaces the client-side Recall timer after a teleport using the remaining server duration.
pub(crate) fn send_recall_cooldown(player: &PlayerRef) {
    let mut cooldowns = UnitCooldowns::default();
    if let Some(entry) = player
        .data()
        .unit
        .cooldowns
        .cooldowns
        .get(&RECALL_SKILL)
        .copied()
    {
        cooldowns.cooldowns.insert(RECALL_SKILL, entry);
    }
    let packet = SCCooldownsPacket::from_cooldowns(&cooldowns, now_ms());
    if !packet.is_empty() {
        player.send(SCSkillCooldownResetPacket {
            obj_id: player.obj_id,
            skill_id: RECALL_SKILL,
            tag_id: 0,
            gcd: false,
        });
        player.send(packet);
    }
}
