use std::sync::Arc;

use crate::models::unit::SkillAttribute;
use crate::server::GameServer;
use crate::world::{now_ms, WorldObject};

use super::super::packets::SCSkillFiredPacket;
use super::super::state::{CancelToken, SkillRef, SkillTaskKind, SkillTaskSlot};
use super::super::types::SkillCaster;
use super::super::{broadcast, buffs, combat as unit_combat};
use super::{apply_effects, apply_skill_modifiers, end_skill, stop, CastCtx};

/// `Skill.StartChanneling`.
pub fn start_channeling(server: &GameServer, skill: &SkillRef, ctx: &CastCtx) {
    skill.state().channel_target = ctx
        .target
        .as_obj()
        .map(|target| (target.obj_id(), target.core() as *const _ as usize));
    let caster = &ctx.caster;
    let t = &skill.template;
    // Native 1.2 sub_398A01B0, attribute 11: milliseconds, independent of CastTime.
    // Keep the server timer in milliseconds; the fired packet clamps its own wire field.
    let channeling_time = apply_skill_modifiers(
        server,
        skill,
        caster,
        SkillAttribute::ChannelingTime,
        f64::from(t.channeling_time),
    )
    .max(0.0) as i32;
    let data = &server.game_data.systems.skills;
    if t.channeling_buff_id != 0 {
        if let Some(b) = data.buff(t.channeling_buff_id).cloned() {
            buffs::apply_template(
                server,
                &b,
                Some(caster),
                &ctx.caster_caster,
                caster,
                Some(skill),
            );
        }
    }
    if t.channeling_target_buff_id != 0 {
        if let Some(b) = data.buff(t.channeling_target_buff_id).cloned() {
            if let Some(target) = ctx.target.as_obj() {
                buffs::apply_template(
                    server,
                    &b,
                    Some(caster),
                    &ctx.caster_caster,
                    target,
                    Some(skill),
                );
            }
        }
    }
    let doodad = if t.channeling_doodad_id > 0 {
        server
            .systems
            .skills
            .ext
            .npcs()
            .spawn_channel_doodad(server, caster, t.channeling_doodad_id)
            .unwrap_or(0)
    } else {
        0
    };
    let tl = skill.tl_id();
    broadcast(
        server,
        caster,
        &SCSkillFiredPacket {
            id: skill.id,
            tl,
            caster: ctx.caster_caster,
            target: ctx.target_caster,
            skill_object: ctx.skill_object.clone(),
            computed_delay: 0,
            channeling_time,
            fire_anim_id: t.fire_anim_id,
        },
        true,
    );
    let cancel = CancelToken::new();
    caster.with_unit(|u| {
        u.unit_mut().skill_state.skill_task = Some(SkillTaskSlot {
            kind: SkillTaskKind::EndChanneling {
                channel_doodad: doodad,
            },
            skill: skill.clone(),
            cancel: cancel.clone(),
            extend_ms: 0,
        })
    });
    if t.channeling_tick > 0 {
        schedule_channel_tick(
            server,
            skill.clone(),
            ctx.clone(),
            cancel.clone(),
            t.channeling_tick,
            channeling_time,
        );
    }
    let skill2 = skill.clone();
    let ctx2 = ctx.clone();
    let scheduled = super::tasks::schedule(server, i64::from(channeling_time), move |srv| {
        if cancel.is_cancelled() {
            return;
        }
        if !ctx2.has_live_handles(&srv) {
            stop(&srv, &skill2, &ctx2.caster, doodad);
            return;
        }
        if skill2.template.channeling_tick <= 0 {
            apply_effects(&srv, &skill2, &ctx2);
        }
        end_channeling(
            &srv,
            &skill2,
            &ctx2.caster,
            doodad,
            &ctx2.caster_caster,
            true,
        );
    });
    if !scheduled {
        // No scheduler: close the channel now instead of leaving the task slot (and the
        // timeline id) set forever.
        stop(server, skill, caster, doodad);
    }
}

fn schedule_channel_tick(
    server: &GameServer,
    skill: SkillRef,
    ctx: CastCtx,
    cancel: CancelToken,
    elapsed: i32,
    duration: i32,
) {
    let interval = skill.template.channeling_tick;
    if interval <= 0 || elapsed >= duration {
        return;
    }
    super::tasks::schedule(server, i64::from(interval), move |server| {
        if cancel.is_cancelled() || skill.cancelled() {
            return;
        }
        let active = ctx
            .caster
            .with_unit(|u| u.unit().skill_state.skill_task.clone())
            .flatten();
        if !active.is_some_and(|task| std::sync::Arc::ptr_eq(&task.skill, &skill)) {
            return;
        }
        if !ctx.has_live_handles(&server) {
            let doodad = ctx
                .caster
                .with_unit(|u| u.unit().skill_state.skill_task.clone())
                .flatten()
                .and_then(|task| match task.kind {
                    SkillTaskKind::EndChanneling { channel_doodad } => Some(channel_doodad),
                    _ => None,
                })
                .unwrap_or(0);
            stop(&server, &skill, &ctx.caster, doodad);
            return;
        }
        let cost = skill.template.channeling_mana.max(0);
        let mp = ctx.caster.with_unit(|u| u.unit().mp).unwrap_or(0);
        if cost > mp {
            let doodad = ctx
                .caster
                .with_unit(|u| u.unit().skill_state.skill_task.clone())
                .flatten()
                .and_then(|task| match task.kind {
                    SkillTaskKind::EndChanneling { channel_doodad } => Some(channel_doodad),
                    _ => None,
                })
                .unwrap_or(0);
            stop(&server, &skill, &ctx.caster, doodad);
            return;
        }
        if cost > 0 {
            unit_combat::reduce_current_mp(&server, &ctx.caster, cost);
        }
        apply_effects(&server, &skill, &ctx);
        schedule_channel_tick(
            &server,
            skill.clone(),
            ctx.clone(),
            cancel,
            elapsed.saturating_add(interval),
            duration,
        );
    });
}

/// `Skill.EndChanneling(caster, channelDoodad, casterCaster)`. `completed`: the channel ran to
/// its end (not stopped or cancelled), so the channeling target buff times out.
pub fn end_channeling(
    server: &GameServer,
    skill: &SkillRef,
    caster: &WorldObject,
    channel_doodad: u32,
    caster_caster: &SkillCaster,
    completed: bool,
) {
    if !caster.is_unit() {
        return;
    }
    let active = caster
        .with_unit(|u| {
            let slot = &mut u.unit_mut().skill_state.skill_task;
            if slot
                .as_ref()
                .is_some_and(|task| Arc::ptr_eq(&task.skill, skill))
            {
                slot.take()
            } else {
                None
            }
        })
        .flatten();
    if let Some(task) = active {
        task.cancel.cancel();
    }
    let t = &skill.template;
    if t.channeling_buff_id != 0 && super::geometry::is_live_object(server, caster) {
        buffs::remove_effect_by_skill(server, caster, t.channeling_buff_id, t.id);
    }
    let channel_target = skill.state().channel_target.take();
    if t.channeling_target_buff_id != 0 {
        if let Some(target) = channel_target.and_then(|(id, identity)| {
            server
                .world
                .object(id)
                .filter(|target| target.core() as *const _ as usize == identity)
        }) {
            if completed {
                // The channel ran to its end: the target buff ends with it, which is its
                // Timeout (quest items such as the Earth Crystal are granted by that trigger).
                buffs::finish_effect_by_skill(server, &target, t.channeling_target_buff_id, t.id);
            } else {
                buffs::remove_effect_by_skill(server, &target, t.channeling_target_buff_id, t.id)
            }
        }
    }
    if channel_doodad != 0 {
        server
            .systems
            .skills
            .ext
            .npcs()
            .delete_doodad(server, channel_doodad);
    }
    // The 1.2 client also refreshes `LastCast` when a channel ends (`FUN_39397ff0` @ 0x39397ff0,
    // DEC 748451: `channeling_time > 0` and not `keep_mana_regen`), on top of the cast start.
    // The window is `LastCast + 5 s` (`X2::Unit::UpdateManaRegen`, `FUN_390b63f0`), so it covers
    // the first 5 s of a longer channel and the 5 s after the channel's end.
    if t.channeling_time > 0 && !t.keep_mana_regen {
        if let Some(p) = caster.as_player() {
            let mut c = p.data();
            c.skill_char.last_cast_ms = now_ms();
            c.skill_char.is_in_post_cast = true;
        }
    }
    end_skill(server, skill, caster);
    if !skill.cancelled() {
        if let (
            SkillCaster::Item {
                item_id,
                item_template_id,
                ..
            },
            Some(p),
        ) = (caster_caster, caster.as_player())
        {
            if *item_template_id > 0 {
                server
                    .systems
                    .skills
                    .ext
                    .quests()
                    .item_use(server, p, *item_id);
            }
        }
    }
}
