//! Private system chat for skill casts accepted by the server.

use std::sync::{Arc, Mutex, Weak};

use crate::server::GameServer;
use crate::systems::social::chat::chat_type;
use crate::world::{session, Player, PlayerRef, WorldObject};

use super::geometry::{raw_distance, world_of, Target};
use super::skill::CastCtx;
use super::state::SkillRun;
use super::types::{SkillCastTarget, SkillCaster};

const RANGE_METERS: f32 = 100.0;

#[derive(Debug, Default)]
pub(crate) struct CastDebug {
    subscribers: Mutex<Vec<Weak<Player>>>,
}

impl CastDebug {
    pub(crate) fn toggle(&self, player: &PlayerRef) -> bool {
        let mut subscribers = self.subscribers.lock().expect("skill debug subscribers");
        let player = Arc::downgrade(player);
        if let Some(index) = subscribers.iter().position(|p| p.ptr_eq(&player)) {
            subscribers.swap_remove(index);
            false
        } else {
            subscribers.push(player);
            true
        }
    }

    fn subscribers(&self) -> Vec<PlayerRef> {
        let mut subscribers = self.subscribers.lock().expect("skill debug subscribers");
        let mut players = Vec::with_capacity(subscribers.len());
        subscribers.retain(|p| {
            if let Some(player) = p.upgrade() {
                players.push(player);
                true
            } else {
                false
            }
        });
        players
    }
}

pub(super) fn report(
    server: &GameServer,
    skill: &SkillRun,
    ctx: &CastCtx,
    tl: u16,
    cast_time_ms: i32,
) {
    let mut recipients = server.systems.skills.debug_casts.subscribers();
    if recipients.is_empty() {
        return;
    }
    let Some(world) = world_of(server, &ctx.caster) else {
        return;
    };
    let origin = world.world_pos_rot(&ctx.caster);
    recipients.retain(|player| {
        player.region().is_some_and(|(id, _, _)| id == world.id)
            && raw_distance(
                &origin,
                &world.world_pos_rot(&WorldObject::from(player)),
                true,
            ) <= RANGE_METERS
    });
    if recipients.is_empty() {
        return;
    }

    let caster = describe_object(&ctx.caster);
    let source = match ctx.caster_caster {
        SkillCaster::Item {
            item_id,
            item_template_id,
            ..
        } => format!(" via item {item_id} (template {item_template_id})"),
        SkillCaster::Mount {
            mount_skill_template_id,
            ..
        } => format!(" via mount skill {mount_skill_template_id}"),
        SkillCaster::Doodad(id) => format!(" via doodad {id}"),
        _ => String::new(),
    };
    let target = match (&ctx.target, ctx.target_caster) {
        (_, SkillCastTarget::Item { id, .. }) => format!("item {id}"),
        (Target::Obj(obj), _) => describe_object(obj),
        (Target::Pos(p), _) => format!("({:.1}, {:.1}, {:.1})", p.pos.x, p.pos.y, p.pos.z),
    };
    let timing = match skill.template.plot_id {
        Some(plot) if skill.template.plot_only => format!("plot {plot}"),
        Some(plot) => format!("cast {cast_time_ms} ms, plot {plot}"),
        None => format!("cast {cast_time_ms} ms"),
    };
    let message = format!(
        "[SkillDebug] Skill {} (TL {tl}, {timing}) {caster}{source} -> {target}",
        skill.id
    );
    for player in recipients {
        session::send_message(&player, chat_type::SYSTEM, &message);
    }
}

fn describe_object(obj: &WorldObject) -> String {
    let kind = obj.kind();
    let id = obj.obj_id();
    obj.with_base_unit(|unit| {
        let name = unit.name();
        let template = unit.template_id();
        if name.is_empty() {
            format!("{kind:?} {id} (template {template})")
        } else {
            format!("{name} ({kind:?} {id}, template {template})")
        }
    })
}
