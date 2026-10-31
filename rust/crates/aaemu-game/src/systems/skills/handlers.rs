//! C->S packets of the skills subsystem: `CSStartSkillPacket`, `CSStopCastingPacket`,
//! `CSRemoveBuffPacket`, `CSLearnSkillPacket`, `CSLearnBuffPacket`, `CSResetSkillsPacket`,
//! `CSSwapAbilityPacket`, `CSSkillControllerStatePacket`, `CSCreateSkillControllerPacket`,
//! `CSSetForceAttackPacket`, `CSChangeTargetPacket`, `CSBuyPriestBuffPacket`.

use aaemu_commons::PacketStream;
use futures::future::BoxFuture;

use crate::error::Result;
use crate::net::connection::GameConnection;
use crate::net::registry::ClientPacket;
use crate::packets::world_objects::SCTargetChangedPacket;
use crate::server::GameServer;
use crate::world::WorldObject;

use super::packets::{
    SCPlotCastingStoppedPacket, SCPlotChannelingStoppedPacket, SCSkillStartedPacket,
};
use super::state::{SkillRef, SkillRun, SkillTaskKind};
use super::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};
use super::{broadcast, buffs, combat, progress, skill};

/// `CSStartSkillPacket`.
#[derive(Debug, Clone, PartialEq)]
pub struct CSStartSkillPacket {
    pub skill_id: u32,
    pub caster: SkillCaster,
    pub target: SkillCastTarget,
    pub flag: u8,
    pub skill_object: SkillObject,
}

impl ClientPacket for CSStartSkillPacket {
    const NAME: &'static str = "CSStartSkillPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        let skill_id = s.read_u32()?;
        let ct = s.read_u8()?;
        let caster = SkillCaster::read(ct, s)?;
        let tt = s.read_u8()?;
        let target = SkillCastTarget::read(tt, s)?;
        let flag = s.read_u8()?;
        let flag_type = flag & 15;
        let skill_object = if flag_type > 0 {
            SkillObject::read(flag_type, s)?
        } else {
            SkillObject::None
        };
        Ok(Self {
            skill_id,
            caster,
            target,
            flag,
            skill_object,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if conn.active_char.is_none() {
                return Ok(());
            }
            // "Will delay for 150 Milliseconds to eliminate the hanging of the skill" (100 ms)
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            start_skill(server, conn, self);
            Ok(())
        })
    }
}

fn template_skill(server: &GameServer, id: u32) -> Option<SkillRef> {
    match server.game_data.systems.skills.skill(id).cloned() {
        Some(t) => Some(SkillRun::new(t, 1)),
        None => {
            tracing::error!("StartSkill: skill {id} has no template (NullReferenceException)");
            None
        }
    }
}

/// The C# bind-on-pickup exception (`CSStartSkillPacket`, commit bfdc3268 / PR #615) was added
/// for the house fireplace's `SavePortal`: the client sends a bind-on-pickup item as the
/// `SkillItem` caster while the skill is the doodad's own `DoodadFuncUse`. Keep only that case:
/// the packet must target a doodad within the skill's range whose current func group has a
/// `DoodadFuncUse` for this skill. Any other mismatched skill id is refused.
fn bind_on_pickup_doodad_use(
    server: &GameServer,
    player: &crate::world::PlayerRef,
    skill: &SkillRef,
    target: SkillCastTarget,
) -> bool {
    use super::geometry::Target;
    use crate::systems::npcs::doodad::templates::FuncTemplate;

    let SkillCastTarget::Doodad(obj_id) = target else {
        return false;
    };
    let me = WorldObject::from(player);
    let Some(world) = super::geometry::world_of(server, &me) else {
        return false;
    };
    let Some(doodad) = world.object(obj_id).and_then(|o| o.as_doodad().cloned()) else {
        return false;
    };
    let data = &server.game_data.systems.npcs.doodad;
    let group = doodad.data().func_group_id;
    let Some(func) = data.func_for_skill(group, skill.template.id) else {
        return false;
    };
    let is_use = func.skill_id == skill.template.id
        && matches!(
            data.func_template(func.func_id, &func.func_type),
            Some(FuncTemplate::Use(_))
        );
    if !is_use {
        return false;
    }
    let target = Target::Obj(WorldObject::from(&doodad));
    super::geometry::distance_to(server, &Target::Obj(me), &target, false)
        <= skill.template.max_range.max(0) as f32
}

/// Minimum time between two WARN lines for the same item-cast refusal of the same character
/// (`Ctx::log_refusal`'s window: a modded client can send bogus item ids in a loop).
const ITEM_CAST_REFUSAL_REPEAT_MS: i64 = 60_000;

/// Whether a refusal of `reason` for `character_id` should be logged now. Repeats inside
/// [`ITEM_CAST_REFUSAL_REPEAT_MS`] are dropped; like the quest layer's `refusal_log`, the map
/// stays bounded when many characters or reasons pass through.
fn should_log_item_cast_refusal(character_id: u32, reason: &'static str) -> bool {
    static LOGGED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(u32, &'static str), i64>>,
    > = std::sync::OnceLock::new();
    let mut logged = LOGGED
        .get_or_init(Default::default)
        .lock()
        .expect("item cast refusal log poisoned");
    if logged.len() > 1024 {
        logged.clear();
    }
    let now = crate::world::now_ms();
    if logged
        .get(&(character_id, reason))
        .is_some_and(|&t| now.saturating_sub(t) < ITEM_CAST_REFUSAL_REPEAT_MS)
    {
        return false;
    }
    logged.insert((character_id, reason), now);
    true
}

/// The 1.2 linked-item rule (`FUN_393925c0`): a glider skill needs the equipped back-slot
/// glider (`item_backpacks.backpack_type_id = 2`), and an `Item` cast must be that item. The
/// client converts the default glider skill 14901 into the equipped item's use skill, so a unit
/// cast is only legitimate for the glider's own use skill or a linked glider UI skill.
fn glider_cast_refused(
    server: &GameServer,
    player: &crate::world::PlayerRef,
    skill: &super::data::SkillTemplate,
    caster: SkillCaster,
) -> bool {
    use super::glider;
    use crate::data::items::backpack_type;
    if !glider::is_glider_skill(skill) {
        return false;
    }
    // A fold ends the current flight, so it stays legitimate when the glider item already
    // expired or was destroyed while the gliding buff is still active.
    if glider::is_glider_fold_skill(skill)
        && glider::has_gliding_buff(server, &crate::world::WorldObject::from(player))
    {
        return false;
    }
    let Some(glider) = glider::equipped_glider(server, player) else {
        return true;
    };
    match caster {
        SkillCaster::Item { item_id, .. } => item_id != glider.id,
        _ => {
            let own_skill = server
                .game_data
                .items
                .template(glider.template_id)
                .map(|t| t.use_skill_id);
            skill.link_backpack_type_id != backpack_type::GLIDER && own_skill != Some(skill.id)
        }
    }
}

/// `CSStartSkillPacket.Read` behaviour.
pub fn start_skill(server: &GameServer, conn: &mut GameConnection, mut p: CSStartSkillPacket) {
    let Some(player) = conn.active_char.clone() else {
        return;
    };
    let me = WorldObject::from(&player);
    let mut target = p.target;
    let skill_id = p.skill_id;
    tracing::info!(
        "StartSkill: Id {skill_id}, flag {}, caster={}, target={}",
        p.flag,
        p.caster.obj_id(),
        target.obj_id()
    );
    let result;
    let mut value = 0u32;
    let data = &server.game_data;
    // A skill reached only through an item use must come with that item: the plain unit cast
    // would otherwise run it (and consume nothing) anywhere. The Item caster branch below
    // validates the item's own use skill and consumes the reagent item.
    if !p.caster.is_item() && data.skills.is_item_only_skill(skill_id) {
        tracing::warn!(
            character = %player.data().unit.name,
            skill_id,
            "Client cast of an item-only skill without its item refused"
        );
        send_cast_refused(&player, skill_id, p.caster, target, p.skill_object);
        return;
    }
    // 1.2 glider skills: the back slot must hold a glider, and an item cast must be that
    // equipped item (`FUN_393925c0`; failure raises `must_equip_proper_item` 326 client-side).
    // The client resolves the default glider skill 14901 into the equipped item's use skill.
    if data
        .systems
        .skills
        .skill(skill_id)
        .is_some_and(|t| glider_cast_refused(server, &player, t, p.caster))
    {
        tracing::warn!(
            character = %player.data().unit.name,
            skill_id,
            caster = p.caster.obj_id(),
            "Client glider cast refused: no matching glider in the backpack slot"
        );
        crate::systems::economy::inventory::send_error_message(
            &player,
            crate::systems::economy::error_message::MUST_EQUIP_PROPER_ITEM,
            0,
            true,
        );
        send_cast_refused(&player, skill_id, p.caster, target, p.skill_object);
        return;
    }
    let is_default = data.skills.is_default_skill(skill_id);
    let is_common = data.skills.is_common_skill(skill_id);
    let auto = player.data().unit.skill_state.auto_attack.clone();
    let is_auto = player.data().unit.is_auto_attack;
    if let SkillCaster::Mount { obj_id, .. } = p.caster {
        // Mount / slave skill
        let Some(skill) = template_skill(server, skill_id) else {
            return;
        };
        let Some(caster) = super::geometry::world_of(server, &me).and_then(|w| w.object(obj_id))
        else {
            return;
        };
        if !super::mount_authority::may_cast_as(server, &player, &caster) {
            // Mount / slave skills are cast as a mate the player may use or a slave the player
            // owns or is attached to; anything else is a forged packet.
            tracing::warn!(
                character = %player.data().unit.name,
                skill_id,
                caster = p.caster.obj_id(),
                "Client cast as an object it does not own or ride refused"
            );
            send_cast_refused(&player, skill_id, p.caster, target, p.skill_object);
            return;
        }
        if !super::mount_authority::may_use_vehicle_skill(server, &caster, skill_id) {
            // A vehicle caster authorizes only the skills on its own bar (and those its buffs
            // grant); any other id, such as an NPC / boss / GM skill, is not a mount cast.
            tracing::warn!(
                character = %player.data().unit.name,
                skill_id,
                caster = p.caster.obj_id(),
                "Client cast of a skill outside the vehicle's bar refused"
            );
            send_cast_refused(&player, skill_id, p.caster, target, p.skill_object);
            return;
        }
        // StormShipLogic.TryBlockMountOrSlaveSkill (vehicles + ai storm buff)
        if let Some(slave) = caster.as_slave() {
            if crate::systems::vehicles::ship_physics::storm_blocks_slave_skill(
                server, slave, skill_id,
            ) {
                // Otherwise the skill press can feel like "nothing happens" client-side.
                player.send(crate::packets::world::SCErrorMsgPacket::new(
                    crate::systems::economy::error_message::NOT_READY,
                    0,
                    true,
                ));
                let mut pkt =
                    SCSkillStartedPacket::new(skill_id, 0, p.caster, target, p.skill_object);
                pkt.set_skill_result(skill_result::NO_PERM)
                    .set_result_uint(0);
                player.send(pkt);
                return;
            }
        }
        let (r, _v) = skill::use_skill(
            server,
            &skill,
            &caster,
            p.caster,
            &mut target,
            Some(p.skill_object.clone()),
            false,
        );
        // Harpoon rope controller + MateGameData.GetMountAttachedSkills (vehicles subsystem)
        let mount_attached_skill = crate::systems::vehicles::on_mount_skill_used(
            server,
            &player,
            &caster,
            skill_id,
            r == skill_result::SUCCESS,
            &target,
        );
        // If no rider/operator skill is linked, we can stop here
        if mount_attached_skill == 0 {
            return;
        }
        // Use player's currently selected target for the rider/operator skill
        let current = player.data().unit.current_target;
        let rider_target = super::geometry::world_of(server, &me)
            .and_then(|w| w.object(current))
            .filter(WorldObject::is_unit)
            .unwrap_or_else(|| me.clone());
        result = skill::unit_use_skill(server, &me, mount_attached_skill, &rider_target);
    } else if is_auto
        && auto
            .as_ref()
            .is_some_and(|a| a.skill.template.id == skill_id)
    {
        result = skill_result::SUCCESS;
    } else if !data.skills.is_item_only_skill(skill_id)
        && (is_default || (is_common && !p.caster.is_item()))
    {
        let Some(skill) = template_skill(server, skill_id) else {
            return;
        };
        let (r, v) = skill::use_skill(
            server,
            &skill,
            &me,
            p.caster,
            &mut target,
            Some(p.skill_object.clone()),
            false,
        );
        result = r;
        value = v;
        if result == skill_result::SUCCESS && skill_id < 5000 && p.caster.obj_id() == player.obj_id
        {
            player.data().unit.is_auto_attack = true;
            skill::start_auto_skill(server, &me, &skill);
        }
    } else if let SkillCaster::Item {
        item_id,
        item_template_id: wire_template_id,
        ..
    } = p.caster
    {
        let items = server.systems.skills.ext.items();
        // Resolve the template from the owned item: the wire template id is a client hint and
        // must never decide the use skill, the reagent or the consumed template.
        let Some((template_id, _)) = items.owned_item(server, &player, item_id) else {
            // A stale item id (a stack the server already merged, consumed or destroyed) is
            // the usual cause; a real client sends the id of a stack it still owns.
            if should_log_item_cast_refusal(player.character_id, "not owned") {
                tracing::warn!(
                    character = %player.data().unit.name,
                    skill_id,
                    item_id,
                    wire_template_id,
                    "Client item cast refused: the item is not in the character's containers"
                );
            }
            return;
        };
        let Some((use_skill_id, bind_on_pickup)) = items.item_use_skill(server, template_id) else {
            if should_log_item_cast_refusal(player.character_id, "no use skill") {
                tracing::warn!(
                    character = %player.data().unit.name,
                    skill_id,
                    item_id,
                    item_template_id = template_id,
                    "Client item cast refused: the item template has no use skill"
                );
            }
            return;
        };
        let Some(skill) = template_skill(server, skill_id) else {
            return;
        };
        if skill_id != use_skill_id
            && !(bind_on_pickup && bind_on_pickup_doodad_use(server, &player, &skill, target))
        {
            // The item's own use skill is the only general authorization; item-only skills
            // never take the legacy exception, and the exception itself is limited to the
            // fireplace's real DoodadFuncUse interaction.
            if should_log_item_cast_refusal(player.character_id, "not the use skill") {
                tracing::warn!(
                    character = %player.data().unit.name,
                    skill_id,
                    use_skill_id,
                    item_id,
                    item_template_id = template_id,
                    "Client item cast refused: the skill is not the item template's use skill"
                );
            }
            if data.skills.is_item_only_skill(skill_id) {
                send_cast_refused(&player, skill_id, p.caster, target, p.skill_object);
            }
            return;
        }
        // Continue with the server-resolved template: `skill::use_skill` validates and consumes
        // through it (item id, template and count), never through the client's hint.
        p.caster = match p.caster {
            SkillCaster::Item {
                obj_id,
                item_id,
                type1,
                type2,
                ..
            } => SkillCaster::Item {
                obj_id,
                item_id,
                item_template_id: template_id,
                type1,
                type2,
            },
            caster => caster,
        };
        let (r, v) = skill::use_skill(
            server,
            &skill,
            &me,
            p.caster,
            &mut target,
            Some(p.skill_object.clone()),
            false,
        );
        result = r;
        value = v;
    } else if let Some(denied) = class_skill_denial(server, &player, skill_id) {
        // Ability-tree skills of an inactive tree, or never learned: the 1.2 client refuses
        // these locally (`URK_TRAINED_SKILL`), so only another client can send them.
        tracing::warn!(
            character = %player.data().unit.name,
            skill_id,
            result = denied,
            "Client cast of an inactive or unlearned ability skill refused"
        );
        result = denied;
    } else if player.data().skills.skills.iter().any(|s| s.id == skill_id) {
        let Some(t) = server.game_data.systems.skills.skill(skill_id).cloned() else {
            return;
        };
        let skill = skill::new_skill_for(server, t, Some(&me));
        let (r, v) = skill::use_skill(
            server,
            &skill,
            &me,
            p.caster,
            &mut target,
            Some(p.skill_object.clone()),
            false,
        );
        result = r;
        value = v;
    } else if skill_id > 0 && is_variant_of_skill(server, &player, skill_id) {
        let Some(skill) = template_skill(server, skill_id) else {
            return;
        };
        let (r, v) = skill::use_skill(
            server,
            &skill,
            &me,
            p.caster,
            &mut target,
            Some(p.skill_object.clone()),
            false,
        );
        result = r;
        value = v;
    } else {
        // Doodad / quest-interaction skills are neither learned nor item skills: the normal
        // path for them (a missing template is reported by `template_skill`). Only skills the
        // static data hands to players (doodad interactions, crafts, NPC interactions, slash
        // commands, buff-granted and quest skills, ...) are accepted: NPC / boss / GM skills
        // are never a client cast.
        tracing::debug!("StartSkill: Id {skill_id}, undefined use type");
        let Some(skill) = template_skill(server, skill_id) else {
            return;
        };
        if !data.skills.is_client_sourced_skill(skill_id) {
            tracing::warn!(
                character = %player.data().unit.name,
                skill_id,
                "Client cast of a skill that has no player source refused"
            );
            send_cast_refused(&player, skill_id, p.caster, target, p.skill_object);
            return;
        }
        let (r, v) = skill::use_skill(
            server,
            &skill,
            &me,
            p.caster,
            &mut target,
            Some(p.skill_object.clone()),
            false,
        );
        result = r;
        value = v;
    }
    if result == skill_result::SUCCESS
        && data
            .systems
            .skills
            .skill(skill_id)
            .is_some_and(|t| super::glider::is_glider_fold_skill(t))
    {
        // A fold ends the flight. This also covers a glider that expired / was destroyed
        // mid-flight: the client falls cleanly instead of the fold being refused.
        super::glider::end_flight(server, &me);
    }
    if result != skill_result::SUCCESS {
        tracing::debug!(skill_id, result, value, "Client skill failed");
        let mut pkt = SCSkillStartedPacket::new(skill_id, 0, p.caster, target, p.skill_object);
        pkt.set_skill_result(result).set_result_uint(value);
        player.send(pkt);
    }
}

/// Answers a refused client cast with the started-skill packet carrying `NO_PERM`.
fn send_cast_refused(
    player: &crate::world::PlayerRef,
    skill_id: u32,
    caster: SkillCaster,
    target: SkillCastTarget,
    object: SkillObject,
) {
    let mut pkt = SCSkillStartedPacket::new(skill_id, 0, caster, target, object);
    pkt.set_skill_result(skill_result::NO_PERM)
        .set_result_uint(0);
    player.send(pkt);
}

#[cfg(test)]
#[path = "class_skill_authority_tests.rs"]
mod class_skill_authority_tests;

#[cfg(test)]
#[path = "ucc_item_authority_tests.rs"]
mod ucc_item_authority_tests;

/// Why a client `CSStartSkill` of an ability-tree skill is refused (`None` = allowed).
///
/// A skill that belongs to an ability (`ability_id != 0`) can only be cast while that ability
/// is one of the character's three active ones (`INACTIVE_ABILITY`), and a skill that must be
/// learned (`need_learn`) only after the character learned it or a skill of the same ability
/// level (`INVALID_SKILL`). Skills with no ability (doodad / quest / item-driven and
/// buff-granted ones) are not ability-tree skills and stay unrestricted; item, mount and
/// default / common skills never reach this check, and GM commands cast through the server
/// skill API instead of this handler.
pub(super) fn class_skill_denial(
    server: &GameServer,
    player: &crate::world::PlayerRef,
    skill_id: u32,
) -> Option<u8> {
    let t = server.game_data.systems.skills.skill(skill_id)?;
    if t.ability_id == 0 {
        return None;
    }
    let (active, learned) = {
        let c = player.data();
        (
            [c.ability1, c.ability2, c.ability3].contains(&t.ability_id),
            c.skills.skills.iter().any(|s| s.id == skill_id),
        )
    };
    if !active {
        return Some(skill_result::INACTIVE_ABILITY);
    }
    if t.need_learn && !learned && !is_variant_of_skill(server, player, skill_id) {
        return Some(skill_result::INVALID_SKILL);
    }
    None
}

/// `CharacterSkills.IsVariantOfSkill(skillId)`.
fn is_variant_of_skill(
    server: &GameServer,
    player: &crate::world::PlayerRef,
    skill_id: u32,
) -> bool {
    let data = &server.game_data.systems.skills;
    let Some(t) = data.skill(skill_id) else {
        return false;
    };
    let learned: Vec<u32> = player.data().skills.skills.iter().map(|s| s.id).collect();
    learned.into_iter().any(|id| {
        data.skill(id)
            .is_some_and(|s| s.ability_id == t.ability_id && s.ability_level == t.ability_level)
    })
}

/// `CSStopCastingPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSStopCastingPacket {
    pub tl_id: u16,
    pub plot_tl_id: u16,
    pub obj_id: u32,
}

impl ClientPacket for CSStopCastingPacket {
    const NAME: &'static str = "CSStopCastingPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            tl_id: s.read_u16()?,
            plot_tl_id: s.read_u16()?,
            obj_id: s.read_bc()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            stop_casting(server, conn, self);
            Ok(())
        })
    }
}

fn stop_casting(server: &GameServer, conn: &mut GameConnection, p: CSStopCastingPacket) {
    let Some(player) = conn.active_char.clone() else {
        tracing::error!("CSStopCastingPacket without an active character (NullReferenceException)");
        return;
    };
    if player.obj_id != p.obj_id {
        tracing::warn!(
            "Player (ObjId {}) is trying to stop casting a skill on object {} using TlId {} and plotTlId {}",
            player.obj_id,
            p.obj_id,
            p.tl_id,
            p.plot_tl_id
        );
        return;
    }
    let me = WorldObject::from(&player);
    let mut plot_cancel = false;
    let plot = player.data().unit.skill_state.active_plot.clone();
    if p.plot_tl_id != 0 {
        if let Some(plot) = plot {
            if plot.0.skill.tl_id() == p.plot_tl_id {
                plot.0.request_cancellation();
                plot_cancel = true;
            } else {
                conn.send(&SCPlotCastingStoppedPacket {
                    tl: p.plot_tl_id,
                    duration: 0,
                    last_event: 1,
                });
                conn.send(&SCPlotChannelingStoppedPacket {
                    tl: p.plot_tl_id,
                    duration: 0,
                    last_event: 1,
                });
            }
        }
    }
    let task = player.data().unit.skill_state.skill_task.clone();
    let Some(task) = task else {
        if !plot_cancel {
            if p.tl_id == 0 && p.plot_tl_id != 0 {
                // The client repeats a cancel for a plot that has already ended (a burst of
                // ~30 per plot); the C# warned for each of them.
                tracing::debug!(
                    "Stop requested for an ended plot. Pid: {}, objId: {}",
                    p.plot_tl_id,
                    p.obj_id
                );
            } else {
                tracing::warn!(
                    "Stop requested, but no skill active? Tl: {}, Pid: {}, objId: {}",
                    p.tl_id,
                    p.plot_tl_id,
                    p.obj_id
                );
            }
        }
        return;
    };
    let active = task.skill.tl_id();
    let matches = if p.tl_id != 0 {
        active == p.tl_id || active == p.plot_tl_id
    } else {
        p.plot_tl_id != 0 && active == p.plot_tl_id
    };
    if !matches {
        tracing::warn!(
            "Stop requested for another skill? Tl: {}, Pid: {}, ActiveTl: {active}",
            p.tl_id,
            p.plot_tl_id
        );
        return;
    }
    task.cancel.cancel();
    match task.kind {
        SkillTaskKind::EndChanneling { channel_doodad } => {
            skill::stop(server, &task.skill, &me, channel_doodad)
        }
        SkillTaskKind::Cast => skill::stop(server, &task.skill, &me, 0),
    }
}

/// `CSRemoveBuffPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSRemoveBuffPacket {
    pub obj_id: u32,
    pub buff_index: u32,
    pub reason: u8,
}

impl ClientPacket for CSRemoveBuffPacket {
    const NAME: &'static str = "CSRemoveBuffPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            obj_id: s.read_bc()?,
            buff_index: s.read_u32()?,
            reason: s.read_u8()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(player) = conn.active_char.clone() else {
                return Ok(());
            };
            let me = WorldObject::from(&player);
            let remove = |o: &WorldObject| -> bool {
                match buffs::get_buff(o, self.buff_index) {
                    None => false,
                    Some(b) => {
                        if buffs::full_template(server, &b.template).kind == 1 {
                            buffs::exit(server, o, b.index, false);
                        }
                        true
                    }
                }
            };
            if player.obj_id == self.obj_id && remove(&me) {
                return Ok(());
            }
            // Pets (`MateManager.GetActiveMates(...).FirstOrDefault(x => x.Id == objId)`) and
            // vehicles (`SlaveManager.GetSlaveByObjId`): owned mates / slaves in the world.
            if let Some(o) =
                super::geometry::world_of(server, &me).and_then(|w| w.object(self.obj_id))
            {
                if o.as_mate()
                    .is_some_and(|m| m.data().owner_obj_id == player.obj_id)
                    || o.as_slave()
                        .is_some_and(|s| s.data().summoner_id == player.character_id)
                {
                    remove(&o);
                }
            }
            Ok(())
        })
    }
}

macro_rules! simple_handler {
    ($(#[$m:meta])* $name:ident { $($field:ident : $ty:ty = $read:ident),* $(,)? } => |$server:ident, $conn:ident, $p:ident| $body:block) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub struct $name {
            $(pub $field: $ty,)*
        }
        impl ClientPacket for $name {
            const NAME: &'static str = stringify!($name);
            fn read(s: &mut PacketStream) -> Result<Self> {
                Ok(Self { $($field: s.$read()?,)* })
            }
            fn handle<'a>(self, $server: &'a GameServer, $conn: &'a mut GameConnection) -> BoxFuture<'a, Result<()>> {
                let $p = self;
                Box::pin(async move { $body Ok(()) })
            }
        }
    };
}

simple_handler!(
    /// `CSLearnSkillPacket`.
    CSLearnSkillPacket { skill_id: u32 = read_u32 } => |server, conn, p| {
        if let Some(c) = conn.active_char.clone() {
            progress::learn_skill(server, &c, p.skill_id);
        }
    }
);

simple_handler!(
    /// `CSLearnBuffPacket`.
    CSLearnBuffPacket { buff_id: u32 = read_u32 } => |server, conn, p| {
        if let Some(c) = conn.active_char.clone() {
            progress::learn_buff(server, &c, p.buff_id);
        }
    }
);

simple_handler!(
    /// `CSResetSkillsPacket`.
    CSResetSkillsPacket { ability_id: u8 = read_u8, ausp: bool = read_bool } => |server, conn, p| {
        if let Some(c) = conn.active_char.clone() {
            progress::reset_skills(server, &c, p.ability_id);
        }
    }
);

simple_handler!(
    /// `CSSwapAbilityPacket`.
    CSSwapAbilityPacket { obj_id: u32 = read_bc, old_ability: u8 = read_u8, ability: u8 = read_u8, auap: bool = read_bool } => |server, conn, p| {
        if let Some(c) = conn.active_char.clone() {
            progress::swap_ability(server, &c, p.old_ability, p.ability);
        }
    }
);

simple_handler!(
    /// `CSSetForceAttackPacket`.
    CSSetForceAttackPacket { on: bool = read_bool } => |server, conn, p| {
        if let Some(c) = conn.active_char.clone() {
            combat::set_force_attack(server, &WorldObject::from(&c), p.on);
        }
    }
);

simple_handler!(
    /// `CSCreateSkillControllerPacket` (logged only, as in the C#).
    CSCreateSkillControllerPacket { obj_id: u32 = read_bc, sc_type: u8 = read_u8, fall_damage_immune: bool = read_bool } => |_server, _conn, p| {
        tracing::debug!("CSCreateSkillController objId={} scType={} fallDamageImmune={}", p.obj_id, p.sc_type, p.fall_damage_immune);
    }
);

simple_handler!(
    /// `CSBuyPriestBuffPacket` (logged only, as in the C#).
    CSBuyPriestBuffPacket { priest_buff_id: u32 = read_u32, npc_unit_id: u32 = read_bc } => |_server, _conn, p| {
        tracing::warn!("BuyPriestBuff, PriestBuffId: {}, NpcUnitId: {}", p.priest_buff_id, p.npc_unit_id);
    }
);

/// `CSSkillControllerStatePacket` (`scType == 0`: rope length / teared / cutouted).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CSSkillControllerStatePacket {
    pub obj_id: u32,
    pub sc_type: u8,
    pub rope: Option<(f32, bool, bool)>,
}

impl ClientPacket for CSSkillControllerStatePacket {
    const NAME: &'static str = "CSSkillControllerStatePacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        let obj_id = s.read_bc()?;
        let sc_type = s.read_u8()?;
        let rope = if sc_type == 0 {
            Some((s.read_f32()?, s.read_bool()?, s.read_bool()?))
        } else {
            None
        };
        Ok(Self {
            obj_id,
            sc_type,
            rope,
        })
    }

    fn handle<'a>(
        self,
        _server: &'a GameServer,
        _conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::debug!(
                "CSSkillControllerState objId={} scType={} {:?}",
                self.obj_id,
                self.sc_type,
                self.rope
            );
            // ShipHarpoonRopeController.TryApplySkillControllerState (vehicles subsystem)
            if let (Some((len, teared, cutouted)), Some(p)) = (self.rope, _conn.active_char.clone())
            {
                crate::systems::vehicles::harpoon::try_apply_skill_controller_state(
                    _server,
                    &p,
                    self.obj_id,
                    len,
                    teared,
                    cutouted,
                );
            }
            Ok(())
        })
    }
}

/// `CSChangeTargetPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSChangeTargetPacket {
    pub target_id: u32,
}

impl ClientPacket for CSChangeTargetPacket {
    const NAME: &'static str = "CSChangeTargetPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            target_id: s.read_bc()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(c) = conn.active_char.clone() else {
                return Ok(());
            };
            let me = WorldObject::from(&c);
            let target = if self.target_id > 0 {
                super::geometry::world_of(server, &me)
                    .and_then(|w| w.unit(self.target_id))
                    .map_or(0, |u| u.obj_id())
            } else {
                0
            };
            c.data().unit.current_target = target;
            broadcast(
                server,
                &me,
                &SCTargetChangedPacket {
                    obj_id: c.obj_id,
                    target_obj_id: target,
                },
                true,
            );
            Ok(())
        })
    }
}
