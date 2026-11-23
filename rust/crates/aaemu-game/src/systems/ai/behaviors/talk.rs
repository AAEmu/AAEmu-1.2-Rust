use crate::models::movement::{GameStanceType, MoveTypeAlertness};
use crate::systems::npcs::npc::packets::SCNpcChatMessagePacket;
use crate::systems::skills::controllers::Vec3;
use crate::world::{now_ms, WorldObject};

use super::super::combat::can_attack;
use super::super::framework::{AiCtx, BehaviorKind};
use super::super::skills as ai_skills;

use super::npc;
use super::support::{template_scalars, throttle};

use BehaviorKind as B;

/// `BaseCombatBehavior.GetPlayersInRange(owner, range, fovScale, greeted, cooldown)`.
fn players_in_range(
    c: &AiCtx<'_>,
    range: f32,
    fov: f64,
    k_greeted: &std::collections::BTreeMap<u32, i64>,
    cooldown_ms: i64,
) -> Vec<WorldObject> {
    let now = now_ms();
    let origin = npc::world_pos_and_rot(c.server, &c.obj);
    npc::units_around(c.server, &c.obj, range, true)
        .into_iter()
        .filter(|(o, _)| o.as_player().is_some())
        .filter(|(_, p)| npc::is_front_at(&origin, p, fov))
        .filter(|(p, _)| {
            k_greeted
                .get(&p.obj_id())
                .is_none_or(|last| now - last >= cooldown_ms)
        })
        .map(|(o, _)| o)
        .collect()
}

const GREET_RANGE: f32 = 5.0;
const GREET_FOV: f64 = 0.6667;
const GREET_COOLDOWN_MS: i64 = 5 * 60 * 1000;

/// `CheckForTalk()` of Idle / HoldPosition.
pub(super) fn check_for_talk(c: &mut AiCtx<'_>, k: BehaviorKind) {
    if c.ai.current_behavior() == Some(B::Talk) {
        return;
    }
    let _profile = super::profile::step(super::profile::Step::Talk);
    let now = now_ms();
    let greeted = c.st(k).greeted.clone();
    let players = players_in_range(c, GREET_RANGE, GREET_FOV, &greeted, GREET_COOLDOWN_MS);
    let ids: Vec<u32> = players.iter().map(WorldObject::obj_id).collect();
    let s = c.st(k);
    for id in &ids {
        if s.greeted
            .get(id)
            .is_none_or(|last| now - last >= GREET_COOLDOWN_MS)
        {
            s.greeted.insert(*id, now);
        }
    }
    s.greeted
        .retain(|id, t| ids.contains(id) || now - *t < GREET_COOLDOWN_MS);
    if !ids.is_empty() {
        c.go_to_talk();
    }
}

pub(super) fn talk_enter(c: &mut AiCtx<'_>, k: BehaviorKind) {
    ai_skills::interrupt_skills(c);
    npc::stop_movement(c.server, &c.npc);
    npc::set_game_stance(&c.npc, GameStanceType::RELAXED);
    npc::set_alertness(&c.npc, MoveTypeAlertness::IDLE);
    c.st(k).initialized = true;
}

pub(super) fn talk_tick(c: &mut AiCtx<'_>, k: BehaviorKind) {
    if !c.st(k).initialized {
        tracing::warn!(
            "TalkBehavior.Tick called before initialization for unit {}",
            c.npc.obj_id
        );
        return;
    }
    if !throttle(c, k) {
        return;
    }
    let now = now_ms();
    let greeted = c.st(k).greeted.clone();
    let players = players_in_range(c, GREET_RANGE, GREET_FOV, &greeted, GREET_COOLDOWN_MS);
    for p in &players {
        let id = p.obj_id();
        let fresh = c
            .st(k)
            .greeted
            .get(&id)
            .is_none_or(|last| now - last >= GREET_COOLDOWN_MS);
        if fresh {
            send_greeting(c, p);
            spawn_spyglass_if_needed(c);
            c.st(k).greeted.insert(id, now);
        }
    }
    let ids: Vec<u32> = players.iter().map(WorldObject::obj_id).collect();
    c.st(k)
        .greeted
        .retain(|id, t| ids.contains(id) || now - *t < GREET_COOLDOWN_MS);
    if players.is_empty() {
        c.go_to_default_behavior();
    }
}

/// `TalkBehavior.SendGreeting(player)`.
fn send_greeting(c: &mut AiCtx<'_>, player: &WorldObject) {
    let Some(p) = player.as_player() else { return };
    let npc_template_id = c.npc.data().unit.template_id as i32;
    let Some((event_name, weight)) = determine_event_type(c, player) else {
        return;
    };
    if event_name.is_empty() {
        // `eventType == null` is never true for the C# tuple ("" falls through); GetEvent with
        // an empty name finds nothing.
        return;
    }
    let data = &c.server.game_data.systems.ai.ai;
    let Some(selected) = data.event(npc_template_id, &event_name, weight).cloned() else {
        return;
    };
    let Some(bubble) = data.chat_bubble(selected.id).cloned() else {
        return;
    };
    let (npc_name, faction, template_id) = {
        let n = c.npc.data();
        (n.unit.name.clone(), n.unit.faction_id, n.unit.template_id)
    };
    let msg = SCNpcChatMessagePacket {
        chat_type: 0, // ChatType.White
        faction_id: faction,
        npc_obj_id: c.npc.obj_id,
        npc_name: crate::systems::npcs::npc::packets::npc_chat_name(
            c.server,
            &npc_name,
            template_id,
        ),
        character_obj_id: p.obj_id,
        kind: 1,
        type_: bubble.id as u32,
        message: String::new(),
    };
    let snd = super::packets::SCPlaySoundPacket {
        kind: 2,
        bubble_id: bubble.id as u32,
        npc_obj_id: c.npc.obj_id,
    };
    crate::systems::skills::broadcast(c.server, player, &msg, true);
    crate::systems::skills::broadcast(c.server, player, &snd, true);
    if selected.skill_id > 0 {
        crate::systems::skills::skill::unit_use_skill(
            c.server,
            &c.obj,
            selected.skill_id as u32,
            player,
        );
    }
    tracing::debug!(
        "NPC {npc_template_id} sent {event_name}:{weight} #{} to {}",
        bubble.id,
        p.obj_id
    );
}

/// `TalkBehavior.DetermineEventType(player)`.
fn determine_event_type(c: &AiCtx<'_>, player: &WorldObject) -> Option<(String, f32)> {
    let npc_id = c.npc.data().unit.template_id as i32;
    let lvl = player.as_player().map_or(0, |p| p.data().unit.level);
    let data = &c.server.game_data.systems.ai.ai;
    // 1. Enemy check
    let relation = crate::systems::skills::relation::relation_to(
        c.server,
        &crate::systems::skills::geometry::Target::Obj(c.obj.clone()),
        &crate::systems::skills::geometry::Target::Obj(player.clone()),
    );
    use crate::systems::skills::types::RelationState as R;
    let friendly = matches!(relation, R::Friendly | R::Neutral);
    if can_attack(c.server, &c.obj, player)
        && !friendly
        && !data.events(npc_id, "OnEnemySeen").is_empty()
    {
        return Some(("OnEnemySeen".into(), 500.0));
    }
    // 2. All records for NPC
    let mut records: Vec<(&str, f32)> = Vec::new();
    for name in ["OnClientGreeting", "OnFriendNearSeen"] {
        for ev in data.events(npc_id, name) {
            records.push((name, ev.weight));
        }
    }
    // 3. Determine maximum allowed weight based on level
    let max_weight = match lvl {
        l if l < 15 => 500.0,
        l if l < 30 => 300.0,
        l if l < 45 => 200.0,
        _ => 60.0,
    };
    // 4. Get the entry with the highest weight not exceeding maxWeight (first of equals)
    let mut best: Option<(&str, f32)> = None;
    for r in records.into_iter().filter(|r| r.1 <= max_weight) {
        if best.is_none_or(|b| r.1 > b.1) {
            best = Some(r);
        }
    }
    Some(best.map_or((String::new(), 0.0), |(n, w)| (n.to_string(), w)))
}

/// `TalkBehavior.SpawnSpyglassIfNeeded()`.
fn spawn_spyglass_if_needed(c: &mut AiCtx<'_>) {
    const VEHICLE_NICK_ID: i32 = 22;
    const SPYGLASS_ID: u32 = 6129;
    const SPYGLASS_DIST: f32 = 1.5;
    if template_scalars(c).npc_nickname_id != VEHICLE_NICK_ID {
        return;
    }
    let t = c.obj.transform();
    // pos = Transform.CloneDetached(); pos.Local.AddDistanceToFront(1.5)
    let (fx, fy) = crate::systems::skills::controllers::add_distance_to_front(
        SPYGLASS_DIST,
        t.x,
        t.y,
        t.rot_z + std::f32::consts::FRAC_PI_2,
    );
    // yaw = CalculateAngleFrom(pos, Owner.Transform) (degrees, used as is)
    let yaw = crate::systems::skills::controllers::calculate_angle_from(
        Vec3::new(fx, fy, t.z),
        Vec3::new(t.x, t.y, t.z),
    ) as f32;
    super::hooks::spawn_spyglass(c.server, &c.obj, SPYGLASS_ID, fx, fy, t.z, yaw);
}
