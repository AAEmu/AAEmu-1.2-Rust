//! Server-side cast restrictions, checked before GCD, buffs, plots or resources change.

use crate::server::GameServer;
use crate::world::WorldObject;

use super::geometry::{world_pos, Target};
use super::state::SkillRun;
use super::types::skill_result;
use super::unit_reqs::skill_result_for_key;
use crate::systems::economy::inventory::Inv;

/// Extra range for a doodad target, covering the distance from its origin to its edge.
pub(crate) const DOODAD_RANGE_ALLOWANCE: f64 = 15.0;
/// Extra range for a slave target (large ships are centred far from their hull edge).
const SLAVE_RANGE_ALLOWANCE: f64 = 40.0;
pub(super) const RED_DRAGON_EGG_SKILL: u32 = 23_150;
pub(super) const PORTABLE_HARPOON_MIN_RANGE: f64 = 4.0;
pub(super) const PORTABLE_HARPOON_MAX_RANGE: f64 = 45.0;

pub(super) fn is_portable_harpoon_shot(skill_id: u32) -> bool {
    matches!(skill_id, 17737 | 17738)
}

/// How far above the source and the destination the obstacle ray is cast. This was the Rust/C#-era
/// height (`+ 1.0` at both ends); 28286101a (2026-10-27) dropped it to feet level to follow the
/// client's "unit position height", and the feet-level ray then refused casts the 1.2 client
/// allowed and sent, because it grazes ground-level terrain and clutter on gentle slopes (65
/// `skill_obstacle` refusals on terrain in one run). The exact height the 1.2 client uses is not
/// verified.
pub(crate) const OBSTACLE_RAY_HEIGHT: f32 = 1.0;

fn fail(key: &str) -> Result<(), (u8, u32)> {
    Err((skill_result_for_key(key), 0))
}

/// The egg's authored skill uses Dancing Goddess's Hunt as an effect-level
/// cost. The bow is required for the whole cast, including its spawn effect.
pub(super) fn check_red_dragon_egg(
    server: &GameServer,
    caster: &WorldObject,
    target: &Target,
) -> Result<(), (u8, u32)> {
    const RED_DRAGON_EGG: u32 = 7056;
    const READY_PHASE: u32 = 18_936;
    const DANCING_GODDESS_HUNT: u32 = 29_301;

    let Some(player) = caster.as_player() else {
        return fail("skill_invalid_source");
    };
    let ready = target
        .as_obj()
        .and_then(WorldObject::as_doodad)
        .is_some_and(|egg| {
            let egg = egg.data();
            egg.template_id == RED_DRAGON_EGG && egg.func_group_id == READY_PHASE
        });
    if !ready {
        return fail("skill_invalid_target");
    }
    let items = server.systems.skills.ext.items();
    if items.item_count(server, player, DANCING_GODDESS_HUNT) > 0 {
        return Ok(());
    }
    let result = if Inv::of(player).items_count(DANCING_GODDESS_HUNT, -1) > 0 {
        skill_result::ITEM_LOCKED
    } else {
        skill_result_for_key("skill_need_reagent")
    };
    Err((result, DANCING_GODDESS_HUNT))
}

pub(super) const CRADLE_CHEST_SKILL: u32 = 19691;

/// The destruction device unlocks the hidden-room chests. Each can be used once.
pub(super) fn check_cradle_chest(server: &GameServer, target: &Target) -> Result<(), (u8, u32)> {
    let Some(chest) = target.as_obj().and_then(WorldObject::as_doodad) else {
        return fail("skill_invalid_target");
    };
    let ready = {
        let data = chest.data();
        (data.template_id, data.func_group_id) == (5550, 14303)
    };
    let opened = ready
        && server
            .world
            .world_of(&WorldObject::from(chest))
            .is_some_and(|world| {
                world.all_doodads().iter().any(|doodad| {
                    let data = doodad.data();
                    (data.template_id, data.func_group_id) == (5549, 14331)
                })
            });
    if opened {
        Ok(())
    } else {
        fail("skill_invalid_target")
    }
}

/// Item-caster tags belong to the owned item's server-side template, never the wire hint.
pub(super) fn check_item_zone(
    server: &GameServer,
    caster: &WorldObject,
    source: &super::types::SkillCaster,
) -> Result<(), (u8, u32)> {
    if let (Some(player), super::types::SkillCaster::Item { item_id, .. }) =
        (caster.as_player(), source)
    {
        if let Some((template, _)) = server
            .systems
            .skills
            .ext
            .items()
            .owned_item(server, player, *item_id)
        {
            if !crate::systems::economy::inventory::item_use_zone_allowed(server, player, template)
            {
                return fail("SKILL_CANNOT_USE_HERE");
            }
        }
    }
    Ok(())
}

/// The physics collider kind of a world object (matches the keys `ai::collision` registers).
fn collision_kind(obj: &WorldObject) -> crate::systems::ai::collision::world::ObjKind {
    use crate::systems::ai::collision::world::ObjKind;
    match obj {
        WorldObject::Doodad(_) => ObjKind::Doodad,
        WorldObject::House(_) => ObjKind::House,
        WorldObject::Slave(_) | WorldObject::Shipyard(_) => ObjKind::Ship,
        WorldObject::Gimmick(_) => ObjKind::Gimmick,
        _ => ObjKind::Unit,
    }
}

/// The pre-existing `source_not_swim` probe: 0.1 m above the caster. Kept apart from the
/// client-mirrored target rule below; the client's source behaviour for a wading caster is not
/// evidenced, and the target rule (0.1 m below plus a 1 m floor) newly allowed it.
const SOURCE_WATER_PROBE_HEIGHT: f32 = 0.1;

fn source_in_water(server: &GameServer, caster: &WorldObject) -> bool {
    if caster
        .with_unit(|u| u.unit().is_under_water)
        .unwrap_or(false)
    {
        return true;
    }
    let pos = world_pos(server, &Target::Obj(caster.clone()));
    crate::systems::ai::physics::is_water(
        server,
        caster.transform().instance_id,
        super::controllers::Vec3::new(pos.x, pos.y, pos.z + SOURCE_WATER_PROBE_HEIGHT),
    )
    .is_some()
}

/// The 1.2 client's water-only target check (`FUN_390b43b0`, `x2game.dll.c:148796`) probes
/// 0.1 m below the point (`DAT_3999ccf0`), requires water there (`probe.z <= level`) and then
/// queries the floor with I3DEngine vt+0x108 `(Vec3*, float)` - likely `GetBottomLevel`, a
/// downward ray that includes statics and answers -1e9 when nothing is hit, which passes the
/// `floor <= level - 1.0` test (`DAT_3999c488`). The server approximates that floor with the
/// terrain height only, so it is more lenient around static geometry. The client aims these
/// casts at the water surface (`FUN_3942b060` / `FUN_394322f0`, `x2game.dll.c:872033` /
/// `:877530`). Not ported: with `target_fishing` (target+0x194) set, a type-8 water entity
/// with flag bit 1 (`vt+0x150 & 1`, likely a NoFishing volume) is refused with 0x23
/// (`FUN_390b43b0`, `x2game.dll.c:148821-148834`).
const WATER_PROBE_DEPTH: f32 = 0.1;
/// `FUN_390b43b0`: `floor <= waterLevel - 1.0`.
const MIN_WATER_DEPTH: f32 = 1.0;

fn target_in_water(server: &GameServer, target: &Target) -> bool {
    if target
        .as_obj()
        .and_then(|o| o.with_unit(|u| u.unit().is_under_water))
        .unwrap_or(false)
    {
        return true;
    }
    let pos = world_pos(server, target);
    let instance = match target {
        Target::Obj(obj) => obj.transform().instance_id,
        Target::Pos(pos) => pos.instance_id,
    };
    let probe = super::controllers::Vec3::new(pos.x, pos.y, pos.z - WATER_PROBE_DEPTH);
    if crate::systems::ai::physics::is_water(server, instance, probe).is_none() {
        return false;
    }
    let Some(level) =
        crate::systems::ai::collision::water_level(server, instance, probe.x, probe.y, probe.z)
    else {
        return true;
    };
    // The client compares the probe against the water level: a level below the probe is not
    // water at that point, even when `is_water` matched another overlapping volume.
    if probe.z > level {
        return false;
    }
    // The terrain floor is unknown without the collision service; the water probe alone then
    // decides, as before the client check was mirrored.
    crate::systems::ai::collision::terrain_height(server, instance, probe.x, probe.y)
        .is_none_or(|floor| floor <= level - MIN_WATER_DEPTH)
}

pub(super) fn labor_cost(skill: &SkillRun, player: &crate::world::PlayerRef) -> i32 {
    let labor = skill.template.consume_labor_power.max(0);
    let step = player
        .data()
        .actability
        .actabilities
        .iter()
        .find(|ability| i64::from(ability.id) == i64::from(skill.template.actability_group_id))
        .map(|ability| ability.step);
    let cost = step.map_or(labor, |step| {
        (f64::from(labor) * f64::from(super::progress::labor_cost_multiplier(step)))
            .round_ties_even() as i32
    });
    if labor > 0 {
        cost.max(1)
    } else {
        0
    }
}

pub(super) fn check_resources(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    source: &super::types::SkillCaster,
) -> Result<(), (u8, u32)> {
    if super::skill::mana_cost(server, skill, caster, source)
        > caster.with_unit(|u| u.unit().mp).unwrap_or(0)
    {
        return fail("skill_lack_mana");
    }
    let Some(player) = caster.as_player() else {
        return Ok(());
    };
    if labor_cost(skill, player) > i32::from(player.data().labor_power) {
        return fail("skill_need_labor_power");
    }
    check_reagents(server, &skill.template, caster)
}

/// The `skill_reagents` requirement, checked before a cast and again when it ends. A player
/// caster without any one of the required items gets `NEED_REAGENT` with the missing item's
/// template id as the value: the 1.2 client's result handler (`FUN_39390900`, 0x24) puts an
/// item name into the text from that value.
pub(crate) fn check_reagents(
    server: &GameServer,
    template: &super::data::SkillTemplate,
    caster: &WorldObject,
) -> Result<(), (u8, u32)> {
    let Some(player) = caster.as_player() else {
        return Ok(());
    };
    let reagents = server.game_data.systems.skills.reagents_for(template.id);
    if reagents.is_empty() {
        return Ok(());
    }
    let items = server.systems.skills.ext.items();
    let enough = |reagent: &super::data::SkillItemAmount| {
        items.item_count(server, player, reagent.item_id) >= i32::from(reagent.amount)
    };
    let available = if template.first_reagent_only {
        reagents.iter().any(enough)
    } else {
        reagents.iter().all(enough)
    };
    if available {
        return Ok(());
    }
    let missing = reagents
        .iter()
        .find(|r| !enough(r))
        .map_or(0, |r| r.item_id);
    // The reagent exists but every stack of it is locked (`ItemFlag.Secure`): the 1.2 client
    // has a dedicated `SkillResult.ItemLocked` for this, instead of "need reagent".
    let locked_only = reagents
        .iter()
        .any(|r| !enough(r) && Inv::of(player).items_count(r.item_id, -1) >= i32::from(r.amount));
    if locked_only {
        return Err((skill_result::ITEM_LOCKED, missing));
    }
    Err((skill_result_for_key("skill_need_reagent"), missing))
}

/// Parity deviation (`PORTING.md`): the construction step of an unfinished house needs the
/// consume item of its step skill's effects (e.g. 1 Lumber for the Scarecrow Garden). The C#
/// consumed an effect-level item only when present and built for free otherwise; retail refuses
/// the cast with the reagent error. Every other skill keeps the soft behaviour.
pub(super) fn check_build_materials(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    target: &Target,
) -> Result<(), (u8, u32)> {
    let Some(player) = caster.as_player() else {
        return Ok(());
    };
    let Some(house) = target.as_obj().and_then(WorldObject::as_house) else {
        return Ok(());
    };
    let (finished, step) = {
        let h = house.data();
        (h.current_step < 0, h.current_step)
    };
    if finished {
        return Ok(());
    }
    let Some(template) = crate::systems::housing::manager::template_of(server, house) else {
        return Ok(());
    };
    if template.build_steps.get(&step).map(|s| s.skill_id) != Some(skill.id) {
        return Ok(());
    }
    let items = server.systems.skills.ext.items();
    let missing = skill.template.effects.iter().any(|e| {
        e.consume_item_id != 0
            && e.consume_item_count > 0
            && items.item_count(server, player, e.consume_item_id) < e.consume_item_count
    });
    if missing {
        return fail("skill_need_reagent");
    }
    Ok(())
}

/// The 1.2 client's cap-scale target check (`FUN_3939dc20`, SkillResult INVALID_TARGET
/// 0x22): a skill carrying the `ItemCapScale` (special 112) or `ItemCapScaleReset` (111)
/// effect refuses a target item whose template is in `item_cap_scale_forbids`
/// (`FUN_395a72d0`, the client's "scalable" property). Runs before the cast takes resources
/// or the source item, so the temper is kept.
pub(super) fn check_cap_scale_target(
    server: &GameServer,
    caster: &WorldObject,
    skill: &super::data::SkillTemplate,
    caster_caster: &super::types::SkillCaster,
    target_caster: &super::types::SkillCastTarget,
) -> Result<(), (u8, u32)> {
    if !matches!(caster_caster, super::types::SkillCaster::Item { .. }) {
        return Ok(());
    }
    let super::types::SkillCastTarget::Item { id, .. } = target_caster else {
        return Ok(());
    };
    let Some(player) = caster.as_player() else {
        return Ok(());
    };
    let cap_scale_effect = skill.effects.iter().any(|e| {
        e.template.is_some_and(|(kind, effect_id)| {
            matches!(
                server.game_data.systems.skills.effect(kind, effect_id),
                Some(super::data::EffectTemplate::Special(s))
                    if s.special_effect_type_id == 111 || s.special_effect_type_id == 112
            )
        })
    });
    if !cap_scale_effect {
        return Ok(());
    }
    let Some(item) = Inv::of(player).item_by_id(*id) else {
        return Ok(());
    };
    if server.game_data.items.cap_scale_forbidden(item.template_id) {
        return Err((skill_result::INVALID_TARGET, 0));
    }
    Ok(())
}

pub(super) fn check(
    server: &GameServer,
    skill: &SkillRun,
    caster: &WorldObject,
    target: &Target,
) -> Result<(), (u8, u32)> {
    let template = &skill.template;
    if target
        .as_obj()
        .is_some_and(|target| !target.same(caster) && super::buffs::nerta_bomb_drop_active(target))
    {
        return fail("skill_invalid_target");
    }
    if template.id == 18715
        && caster.as_player().is_none_or(|player| {
            crate::systems::npcs::doodad::interactions::equipped_powder_keg(player).is_none()
        })
    {
        return fail("skill_need_reagent");
    }
    if template.id == RED_DRAGON_EGG_SKILL {
        check_red_dragon_egg(server, caster, target)?;
    }
    // Like the other cast restrictions, this also applies to administrators. A GM's
    // cooldown override does not grant permission to use forbidden tags.
    if caster.as_player().is_some()
        && super::data::zone_rules::current_group(server, caster).is_some_and(|group| {
            let data = &server.game_data.systems.skills;
            data.zone_rules.bans_any(
                group,
                data.skill_tags.get(&skill.id).map_or(&[], Vec::as_slice),
            )
        })
    {
        // Native 1.2 x2game12.c:1983345: SkillResult 58 = ZONE_BANNED.
        return fail("SKILL_CANNOT_USE_HERE");
    }
    // Tosare's portal skill casts a Return effect directly, so it never runs Doodad.Use.
    // Require the machinery's open phase, where the client gets this skill.
    if template.id == 17_292
        && !target
            .as_obj()
            .and_then(WorldObject::as_doodad)
            .is_some_and(|doodad| {
                let d = doodad.data();
                d.template_id == 4499 && d.func_group_id == 10_996
            })
    {
        return fail("skill_invalid_target");
    }
    // Leave Burnt Castle Armory also applies Return directly. It is offered on the
    // entrance exit and on the rear exit only after skill 18024 reveals it.
    if template.id == crate::systems::instances::indun::ARMORY_EXIT_SKILL
        && !target
            .as_obj()
            .and_then(WorldObject::as_doodad)
            .is_some_and(|doodad| {
                let d = doodad.data();
                matches!(
                    (d.template_id, d.func_group_id),
                    (3673, 8794) | (4923, 12257)
                )
            })
    {
        return fail("skill_invalid_target");
    }
    if template.id == CRADLE_CHEST_SKILL {
        check_cradle_chest(server, target)?;
    }
    if let Some(doodad) = target.as_obj().and_then(WorldObject::as_doodad) {
        if !crate::systems::npcs::doodad::gathering::allows_skill(server, doodad, template, caster)
        {
            return fail("skill_invalid_target");
        }
        // The func's item requirement is enforced at the cast start too, not only when the
        // effect runs: the 1.2 client plays the cast bar and animation for a successful
        // `SCSkillStarted` and stops the cast on a non-zero result.
        if let Some(refusal) = crate::systems::npcs::doodad::item_gate::skill_item_refusal(
            server, doodad, template, caster,
        ) {
            return Err((refusal.result, refusal.value));
        }
    }
    let (hp, buff_ids) = caster
        .with_unit(|u| {
            let unit = u.unit();
            (
                unit.hp,
                unit.buffs
                    .effects
                    .iter()
                    .filter(|b| !b.is_ended())
                    .map(|b| b.template.id)
                    .collect::<Vec<_>>(),
            )
        })
        .ok_or((skill_result_for_key("skill_invalid_source"), 0))?;
    // A gimmick's collision skill (`GimmickTemplate.CollisionSkillId`) is cast by the server
    // when the gimmick touches a unit; the 1.2 client has no packet for it. A gimmick is a
    // physics prop with no health, so the source life flags cannot apply, the touch already is
    // the range, and a line-of-sight ray between touching objects is meaningless.
    let gimmick_source = caster.as_gimmick().is_some();
    if !gimmick_source && hp <= 0 && template.source_alive && !template.source_dead {
        return fail("skill_source_died");
    }
    if !gimmick_source && hp > 0 && template.source_dead && !template.source_alive {
        return fail("skill_source_alive");
    }
    if template.source_not_swim && source_in_water(server, caster) {
        return fail("skill_cannot_cast_in_swimming");
    }
    for id in buff_ids {
        let Some(buff) = server.game_data.systems.skills.buff(id) else {
            continue;
        };
        // The ship's "Anchor Lowered" and "Anchor" statuses use the generic stun
        // flag. Only the anchor and ship utilities can bypass these two statuses.
        let ship_utility = matches!(
            skill.id,
            13608 | 13609 | 17649 | 17949 | 17950 | 17957 | 17958 | 20704
        );
        let anchored_utility = matches!(id, 615 | 1983)
            && caster.as_slave().is_some()
            && (ship_utility || skill.id == 14766);
        // The client data permits these utility casts so the client will send
        // them while anchored. Keep the server's ordinary stun check even if
        // somebody uses that same compact database for their local server.
        let permits_stun = template.source_stun && !ship_utility;
        if !permits_stun && !anchored_utility && (buff.stun || buff.sleep || buff.knockdown) {
            return fail("skill_cannot_cast_in_stun");
        }
        if buff.blank_minded && !permits_stun {
            return fail("skill_blank_minded");
        }
    }
    // A skill cast from a corpse (`source_dead`, e.g. an `OnDeath` np_skill such as Muddy
    // Sarracenia's seed plot 11478) is aimed at the dying caster itself; its `target_alive`
    // flag describes the plot's later targets, so the corpse target is not held to it.
    // Harpoon Bomb and Deadly Toxin Shot start plots 355 and 356 on the living operator.
    // Their corpse-only flag does not describe the initial self target.
    let portable_harpoon = is_portable_harpoon_shot(skill.id);
    let scripted_self_cast = (template.source_dead || portable_harpoon)
        && target.as_obj().is_some_and(|o| o.same(caster));
    if let Some(target_hp) = target
        .as_obj()
        .filter(|_| !scripted_self_cast)
        .and_then(|o| o.with_unit(|u| u.unit().hp))
    {
        if target_hp <= 0 && template.target_alive && !template.target_dead {
            let can_recover = matches!(
                skill.id,
                super::skill::PET_DUAL_RECOVERY | super::skill::PET_DUAL_RECOVERY_ALT
            ) && target
                .as_obj()
                .and_then(WorldObject::as_mate)
                .is_some_and(|mate| mate.data().can_revive());
            if !can_recover {
                return fail("skill_target_died");
            }
        }
        if target_hp > 0 && template.target_dead && !template.target_alive {
            return fail("skill_target_alive");
        }
    }
    if crate::systems::vehicles::slaves::ship_repair_target_at_full_health(server, skill.id, target)
    {
        return fail("skill_urk_target_health_less_than");
    }
    if template.target_only_water && !target_in_water(server, target) {
        return fail("skill_invalid_location");
    }
    let source = world_pos(server, &Target::Obj(caster.clone()));
    let destination = world_pos(server, target);
    if ![
        source.x,
        source.y,
        source.z,
        destination.x,
        destination.y,
        destination.z,
    ]
    .iter()
    .all(|v| v.is_finite())
    {
        return fail("skill_invalid_target");
    }
    if template.target_angle > 0
        && !super::geometry::in_angle(&source, &destination, template.target_angle as f32)
    {
        return fail("skill_outof_angle");
    }
    let distance = f64::from(super::geometry::distance_to(
        server,
        &Target::Obj(caster.clone()),
        target,
        true,
    ));
    let mut min_range = f64::from(template.min_range);
    let mut max_range = super::skill::apply_skill_modifiers(
        server,
        skill,
        caster,
        crate::models::unit::SkillAttribute::Range,
        f64::from(template.max_range),
    );
    let aimed_harpoon = portable_harpoon && matches!(target, Target::Pos(_));
    if aimed_harpoon {
        // The self-targeted data uses zero for both ranges. A confirmed aim
        // has a real muzzle clearance and the 45 m range from the launch plot.
        let horizontal = (destination.x - source.x).hypot(destination.y - source.y);
        if f64::from(horizontal) < PORTABLE_HARPOON_MIN_RANGE {
            return fail("skill_too_close_range");
        }
        max_range = PORTABLE_HARPOON_MAX_RANGE;
    } else if portable_harpoon {
        min_range = 0.0;
    }
    if template.target_type == super::types::SkillTargetType::Doodad as i32
        && template.min_range >= 100
    {
        min_range /= 100.0;
    }
    // A mate (battle pet) wields no weapon: its skills keep their own range instead of the
    // weaponless 0-3 m fallback.
    if template.weapon_slot_for_range_id > 0 && caster.as_mate().is_none() {
        let ranges = server
            .systems
            .skills
            .ext
            .items()
            .weapon_holdable(server, caster, template.weapon_slot_for_range_id)
            .map_or((0.0, 3.0), |h| (h.min_range, h.max_range));
        min_range = f64::from(ranges.0);
        max_range = f64::from(ranges.1);
    }
    if distance < min_range {
        return fail("skill_too_close_range");
    }
    // A position-targeted skill is cast at a spot on the ground; its template may carry
    // max_range 0 because the client decides where the placement is legal (10.x
    // `unboundedPlacement`; summon skill 15802 rejected every boat at 3.4 m without it).
    let unbounded_placement =
        matches!(target, Target::Pos(_)) && template.max_range <= 0 && !aimed_harpoon;
    // Doodads have no model radius and a ship's origin is far from its hull, so these targets
    // get an allowance on top of the skill's range (C# exempted them entirely).
    let extent = target.as_obj().map_or(0.0, |o| {
        if o.as_doodad().is_some() {
            DOODAD_RANGE_ALLOWANCE
        } else if o.as_slave().is_some() {
            SLAVE_RANGE_ALLOWANCE
        } else {
            0.0
        }
    });
    if !gimmick_source && distance > max_range + extent && !unbounded_placement {
        return fail("skill_too_far_range");
    }
    // The 1.2 client never runs the obstacle ray for a doodad target: the cast target check for
    // wire type 4 (`FUN_3939E030`, IDA `sub_3939E030`, `x2game12.c:765943`) only looks the doodad
    // up (result 9 when it is gone), while the one producer of skill result 0xC (`FUN_390B5540`)
    // is reached only for unit (`FUN_390B5C60`) and position (`FUN_390B5A30`) targets, and unit
    // ids never resolve to doodads. A doodad's origin also sits on or inside static map
    // geometry (workbench stalls, house fixtures, objects baked into a brush), so the ray
    // always hit that geometry and refused the interaction ("Too far from target"). The
    // retail zone does not check obstacles either and the C# tree had no check at all.
    // A harpoon bomb follows an arc and explodes on the first collision. A
    // straight ray from the operator to the selected surface rejects valid aim.
    if !gimmick_source
        && !aimed_harpoon
        && template.check_obstacle
        && !target
            .as_obj()
            .is_some_and(|o| o.same(caster) || o.as_doodad().is_some())
    {
        // 1.2 client `FUN_390B5650` (x2game.dll.c:149951): shorten the end point by the
        // target radius and cast no ray when `dist <= radius`; `FUN_390B5540`
        // (x2game.dll.c:149877) uses objtypes 7 (0x107 with `check_terrain`), ray flags
        // 0x2048F (collision class 2) and one hit, mapping a hit to skill result 0xC.
        // Unit targets (`FUN_390B5C60` -> `FUN_3989C0D0`) use the target radius at target
        // +0x80 and skip the source and target colliders; position targets
        // (`FUN_390B5A30` -> `FUN_390B5790`) use the constant 0.5 (DAT_3999C47C) and skip
        // only the source.
        let dx = destination.x - source.x;
        let dy = destination.y - source.y;
        let dz = destination.z - source.z;
        let len = (dx * dx + dy * dy + dz * dz).sqrt();
        let (target_radius, skip_target) = match target {
            Target::Pos(_) => (0.5, false),
            Target::Obj(_) => (super::geometry::model_size(server, target), true),
        };
        if len > target_radius {
            let mut to = [destination.x, destination.y, destination.z];
            if target_radius > 0.0 {
                to[0] -= dx / len * target_radius;
                to[1] -= dy / len * target_radius;
                to[2] -= dz / len * target_radius;
            }
            // Raising both ends by the same amount keeps the direction, so the radius gate and
            // the shortening above hold unchanged.
            let from = [source.x, source.y, source.z + OBSTACLE_RAY_HEIGHT];
            to[2] += OBSTACLE_RAY_HEIGHT;
            let mut ignored = vec![(collision_kind(caster), caster.obj_id())];
            if skip_target {
                if let Some(obj) = target.as_obj() {
                    ignored.push((collision_kind(obj), obj.obj_id()));
                }
            }
            let objtypes = if template.check_terrain {
                crate::systems::ai::collision::query::mask::SOLID_TERRAIN
            } else {
                crate::systems::ai::collision::query::mask::SOLID
            };
            if let Some(Some(hit)) = crate::systems::ai::collision::raycast_ignoring_objects(
                server,
                caster.transform().instance_id,
                from,
                to,
                objtypes,
                crate::systems::ai::collision::query::ray_flags::SKILL_OBSTACLE,
                &ignored,
            ) {
                // NPC AI retries blocked casts, so only player rejections are warnings.
                if let Some(player) = caster.as_player() {
                    let character = player.data().unit.name.clone();
                    tracing::warn!(
                        character = %character,
                        skill = template.id,
                        target = target.obj_id(),
                        result_key = "skill_obstacle",
                        check_terrain = template.check_terrain,
                        ?from,
                        ?to,
                        ?hit,
                        "Skill line of sight blocked"
                    );
                } else {
                    tracing::debug!(
                        caster = caster.obj_id(),
                        skill = template.id,
                        target = target.obj_id(),
                        ?from,
                        ?to,
                        ?hit,
                        "Skill line of sight blocked"
                    );
                }
                return fail("skill_obstacle");
            }
        }
    }
    Ok(())
}
