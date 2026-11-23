use rand::Rng;

use crate::models::unit::SkillAttribute;
use crate::server::GameServer;
use crate::systems::instances::encounters::{hooks as encounter_hooks, registry as encounters};
use crate::world::WorldObject;

use super::super::data::{EffectKind, EffectTemplate, SkillEffect, SkillTemplate};
use super::super::effects::{EffectArgs, EffectSource};
use super::super::ext::TakenCastItem;
use super::super::geometry::{distance_to, is_front, world_of, world_pos, Target};
use super::super::packets::{CompressedGamePackets, SCSkillStartedPacket};
use super::super::relation::{filter_with_relation, protected_from, relation_to};
use super::super::state::{SkillRef, SkillRun};
use super::super::stats::{stats, StatKind};
use super::super::types::{
    skill_result, ApplicationMethod, CastAction, DamageType, RelationState, SkillCastTarget,
    SkillCaster, SkillHitType, SkillTargetRelation, SkillTargetSelection, SkillTargetType,
};
use super::super::{broadcast, buffs, effects, unit_reqs};
use super::targeting::append_hostile_ships_hit_by_siege_hull_aoe;
use super::{apply_skill_modifiers, stop, CastCtx, DIG_UP_TREASURE_SKILL};

/// The NPC a `KillNpcWithoutCorpseEffect` removes. A matching caster removes itself.
/// Other casts select the nearest matching NPC within the effect radius, when it has one.
pub(super) fn nearest_kill_npc(
    server: &GameServer,
    caster: &WorldObject,
    template_id: u32,
    radius: f32,
) -> Option<crate::world::NpcRef> {
    if let Some(npc) = caster
        .as_npc()
        .filter(|npc| npc.data().unit.template_id == template_id)
    {
        // Summoned NPCs can share a position. A self-destruct must remove its own caster
        // even when another matching NPC is exactly as close.
        return Some(npc.clone());
    }
    let world = world_of(server, caster)?;
    let from = Target::Obj(caster.clone());
    let mut best: Option<(f32, crate::world::NpcRef)> = None;
    for npc in world.all_npcs() {
        if npc.data().unit.template_id != template_id {
            continue;
        }
        let to = Target::Obj(WorldObject::from(npc.clone()));
        let distance = distance_to(server, &from, &to, true);
        if radius > 0.0 && distance > radius {
            continue;
        }
        if best.as_ref().is_none_or(|(best, _)| distance < *best) {
            best = Some((distance, npc));
        }
    }
    best.map(|(_, npc)| npc)
}

/// `Skill.RollCombatDice(attacker, target)`.
pub fn roll_combat_dice(
    server: &GameServer,
    skill: &SkillRun,
    attacker: &WorldObject,
    target: &WorldObject,
) -> SkillHitType {
    let damage_type = DamageType::from_raw(i64::from(skill.template.damage_type_id));
    let roll = || rand::thread_rng().gen::<f32>() * 100.0;
    let attacker_unit = attacker.is_unit();
    let target_unit = target.is_unit();
    let a = stats(
        server,
        attacker,
        &[
            StatKind::BullsEye,
            StatKind::MeleeAccuracy,
            StatKind::SpellAccuracy,
            StatKind::RangedAccuracy,
        ],
    );
    let mut always_hit = false;
    if attacker_unit {
        let bulls_eye_mod = a[0] as i32 as f32 / 1000.0 * 3.0 / 100.0;
        let ap = world_pos(server, &Target::Obj(attacker.clone()));
        let tp = world_pos(server, &Target::Obj(target.clone()));
        if !is_front(&ap, &tp) {
            always_hit = true;
        } else if target_unit {
            let tstat = stats(
                server,
                target,
                &[
                    StatKind::DodgeRate,
                    StatKind::BlockRate,
                    StatKind::MeleeParryRate,
                    StatKind::RangedParryRate,
                ],
            );
            if roll() < tstat[0] as f32 - bulls_eye_mod {
                match damage_type {
                    Some(DamageType::Melee) => return SkillHitType::MeleeDodge,
                    Some(DamageType::Ranged) => return SkillHitType::RangedDodge,
                    _ => {}
                }
            }
            if roll() < tstat[1] as f32 - bulls_eye_mod {
                match damage_type {
                    Some(DamageType::Melee) => return SkillHitType::MeleeBlock,
                    Some(DamageType::Ranged) => return SkillHitType::RangedBlock,
                    _ => {}
                }
            }
            if roll() < tstat[2] as f32 - bulls_eye_mod {
                if damage_type == Some(DamageType::Melee) {
                    return SkillHitType::MeleeParry;
                }
                if damage_type == Some(DamageType::Ranged) {
                    let dual = target.with_base_unit(|b| {
                        b.buffs()
                            .check_buff(super::types::buff_constants::EQUIP_DUALWIELD)
                            && b.buffs()
                                .check_buff(super::types::buff_constants::DUALWIELD_PROFICIENCY)
                    });
                    if dual {
                        return SkillHitType::MeleeParry;
                    }
                }
            }
            if roll() < tstat[3] as f32 - bulls_eye_mod && damage_type == Some(DamageType::Ranged) {
                return SkillHitType::RangedParry;
            }
        }
    }
    let _ = always_hit;
    match damage_type {
        Some(DamageType::Melee) => {
            if attacker_unit && roll() < a[1] as f32 {
                SkillHitType::MeleeHit
            } else {
                SkillHitType::MeleeMiss
            }
        }
        Some(DamageType::Magic) => {
            if attacker_unit && roll() < a[2] as f32 {
                SkillHitType::SpellHit
            } else {
                SkillHitType::SpellMiss
            }
        }
        Some(DamageType::Ranged) => {
            if attacker_unit && roll() < a[3] as f32 {
                SkillHitType::RangedHit
            } else {
                SkillHitType::RangedMiss
            }
        }
        Some(DamageType::Siege) => SkillHitType::RangedHit,
        _ => SkillHitType::Invalid,
    }
}

fn level_rule_hit(skill: &SkillRun) -> SkillHitType {
    match DamageType::from_raw(i64::from(skill.template.damage_type_id)) {
        Some(DamageType::Melee) => SkillHitType::MeleeHit,
        Some(DamageType::Magic | DamageType::Heal) => SkillHitType::SpellHit,
        Some(DamageType::Siege | DamageType::Ranged) => SkillHitType::RangedHit,
        None => SkillHitType::Invalid,
    }
}

/// Refunds the cast's taken source item unless the cast result was committed. Dropping the
/// guard refunds: an early refusal (missing reagent, failed build) or an effect that cancels
/// while nothing could be charged.
struct CastSourceDebit<'a> {
    server: &'a GameServer,
    taken: Option<TakenCastItem>,
}

impl Drop for CastSourceDebit<'_> {
    fn drop(&mut self) {
        if let Some(taken) = self.taken.take() {
            self.server
                .systems
                .skills
                .ext
                .items()
                .refund_cast_source_item(self.server, taken);
        }
    }
}

impl CastSourceDebit<'_> {
    fn commit(&mut self) {
        if let Some(taken) = self.taken.take() {
            self.server
                .systems
                .skills
                .ext
                .items()
                .commit_cast_source_item(self.server, taken);
        }
    }
}

/// The NPC template ids a skill's `target_npc_tag_id` effects accept (`tagged_npcs`), or `None`
/// when no effect restricts its targets by tag. A self-centred area cast with tagged effects
/// starts successfully even when no such NPC is in the area; the caller can use this to wait
/// instead of consuming a one-shot trigger.
pub fn npc_tag_targets(data: &super::Data, template: &SkillTemplate) -> Option<Vec<u32>> {
    let mut tagged = false;
    let mut ids = Vec::new();
    for effect in &template.effects {
        if effect.target_npc_tag_id > 0 {
            tagged = true;
            ids.extend_from_slice(data.npcs_by_tag(effect.target_npc_tag_id));
        }
    }
    tagged.then_some(ids)
}

/// `Skill.ApplyEffects(caster, casterCaster, targetSelf, targetCaster, skillObject)`.
pub fn apply_effects(server: &GameServer, skill: &SkillRef, ctx: &CastCtx) {
    let caster = &ctx.caster;
    if !caster.is_unit() || skill.cancelled() {
        tracing::debug!(
            skill_id = skill.id,
            caster_is_unit = caster.is_unit(),
            cancelled = skill.cancelled(),
            "Skill effects skipped"
        );
        return;
    }
    let t = skill.template.clone();
    let data = &server.game_data.systems.skills;
    let player = caster.as_player().cloned();
    let caster_t = Target::Obj(caster.clone());
    let target_self = &ctx.target;
    // Earthen Grip is marked Others, which the area filter interprets as neutral only.
    // Both it and Sorvell's barrel barrage select enemies that can receive their effects.
    let sorvell_area = encounters::hostile_area_skill(t.id) && caster.as_npc().is_some();
    if encounter_hooks::all()
        .iter()
        .any(|encounter| encounter.on_apply_effects(server, &t, caster))
    {
        return;
    }
    // Serialize the readiness check, phase change and reward or spawn. A
    // concurrent cast may finish after the first changed the doodad phase.
    let egg_cast = encounters::is_red_dragon_egg_skill(t.id);
    let cradle_chest = t.id == super::validation::CRADLE_CHEST_SKILL;
    let _doodad_operation =
        (egg_cast || cradle_chest).then(|| server.systems.economy.operation_scope());
    let ready = encounter_hooks::effects_ready(server, &t, caster, target_self);
    if let Err((result, _)) = ready {
        if let Some(player) = player.as_ref().filter(|_| result == skill_result::BAG_FULL) {
            player.send(crate::packets::world::SCErrorMsgPacket::new(
                crate::systems::economy::error_message::BAG_FULL,
                0,
                true,
            ));
        }
        skill.state().cancelled = true;
        return;
    }
    let selection = SkillTargetSelection::from_raw(i64::from(t.target_selection));
    // Native 1.2 sub_398A01B0 attribute 3 / sub_39420B30: radius is in meters.
    let area_radius = apply_skill_modifiers(
        server,
        skill,
        caster,
        SkillAttribute::AreaRadius,
        f64::from(t.target_area_radius),
    )
    .max(0.0)
    .min(f64::from(i32::MAX)) as f32;
    let mut possible: Vec<Target> = Vec::new();
    if t.target_siege
        && selection == Some(SkillTargetSelection::Source)
        && caster.as_slave().is_some()
    {
        possible.push(target_self.clone());
    } else if t.target_area_radius > 0 {
        let mut units = super::geometry::around_radius(server, target_self, area_radius, true);
        // `around_radius` compares only x/y: drop candidates far above or below the area centre
        // (a ground-target area uses the target point's z).
        let limit = super::geometry::area_vertical_limit(area_radius, t.target_valid_height);
        let origin_z = world_pos(server, target_self).z;
        units.retain(|o| (world_pos(server, &Target::Obj(o.clone())).z - origin_z).abs() <= limit);
        if selection == Some(SkillTargetSelection::Source) {
            if let Some(o) = target_self.as_obj() {
                units.push(o.clone());
            }
        } else if selection == Some(SkillTargetSelection::Target) {
            // `around_radius` excludes its origin. Keep the explicit target for
            // Concussive Arrow (11933) and area interactions such as Set Bomb (15724).
            if let Some(o) = target_self
                .as_obj()
                .filter(|o| o.is_unit() || o.as_doodad().is_some())
            {
                units.push(o.clone());
            }
        }
        let relation = if sorvell_area {
            SkillTargetRelation::Hostile
        } else {
            SkillTargetRelation::from_raw(i64::from(t.target_relation))
                .unwrap_or(SkillTargetRelation::Any)
        };
        let mut units = filter_with_relation(server, relation, &caster_t, units);
        // Corpses (or the living, for a resurrection) never take an area slot. A template with
        // neither flag set (57 radius skills) is left unfiltered rather than refusing everyone.
        if t.target_alive || t.target_dead {
            units.retain(|o| {
                !o.is_unit()
                    || super::geometry::alive_dead_allows(
                        t.target_alive,
                        t.target_dead,
                        o.with_unit(|u| u.unit().hp).unwrap_or(0),
                    )
            });
        }
        for encounter in encounter_hooks::all() {
            encounter.area_targets(server, caster, &t, &mut units);
        }
        possible.extend(units.into_iter().map(Target::Obj));
        if selection == Some(SkillTargetSelection::Source) && target_self.as_obj().is_none() {
            possible.push(target_self.clone());
        }
    } else {
        possible.push(target_self.clone());
    }
    append_hostile_ships_hit_by_siege_hull_aoe(
        server,
        &t,
        &caster_t,
        target_self,
        ctx,
        area_radius,
        &mut possible,
    );
    // Distinct
    let mut distinct: Vec<Target> = Vec::new();
    for p in possible {
        if !distinct.iter().any(|d| d.same(&p)) {
            distinct.push(p);
        }
    }
    let mut possible = distinct;
    // A hostile skill's unit targets drop the protected players (Peace, no-PvP instances, the
    // home-zone protection) so the combat dice / durability rolls below do not run on them. A
    // Neutral target keeps main's behaviour, and the caster itself stays so friendly-only Target
    // effects of a hostile skill still reach it.
    if t.target_type == SkillTargetType::Hostile as i32 {
        possible.retain(|target| {
            target.as_obj().is_none_or(|o| {
                !o.is_unit()
                    || o.obj_id() == caster_t.obj_id()
                    || !protected_from(server, &caster_t, target)
            })
        });
    }
    if t.target_area_radius > 0 && t.target_area_angle > 0 && t.target_area_angle < 360 {
        let origin = world_pos(server, target_self);
        possible.retain(|target| {
            super::geometry::in_angle(
                &origin,
                &world_pos(server, target),
                t.target_area_angle as f32,
            )
        });
    }
    // C# (`Skill.ApplyEffects`: "Add origin in case of no targets and using a target position
    // cast") pushed the caster unconditionally. The relation still decides whether the caster
    // is a target: a Hostile area never takes its caster (skill 27005, the Berserk Red Dragon
    // Roar meteor, is one of the 76 no-plot Hostile position skills whose effects carry both
    // `friendly` and `non_friendly`), and `Others` excludes it too. Any / Party / Friendly /
    // Raid keep the fallback (a Friendly area heal still reaches its caster).
    if possible.is_empty() && matches!(ctx.target_caster, SkillCastTarget::Position { .. }) {
        let relation = SkillTargetRelation::from_raw(i64::from(t.target_relation))
            .unwrap_or(SkillTargetRelation::Any);
        if super::relation::is_relation_valid(server, relation, &caster_t, &caster_t) {
            possible.push(caster_t.clone());
        }
    }
    if sorvell_area {
        possible.retain(|target| {
            target
                .as_obj()
                .is_some_and(|object| object.is_unit() && object.obj_id() != caster.obj_id())
        });
    }
    if encounters::hits_only_accepting_doodads(t.id) {
        // The thrown purifier destroys mushrooms via SkillHit. Nearby enemies and
        // other props must not use up its five target slots.
        let npcs = server.systems.skills.ext.npcs();
        possible.retain(|target| {
            target
                .as_obj()
                .is_some_and(|object| npcs.accepts_doodad_skill_hit(server, object, t.id))
        });
    }
    let mut over_count = t.target_area_count > 0 && possible.len() > t.target_area_count as usize;
    if over_count
        && caster.as_npc().is_some()
        && t.target_type == SkillTargetType::SelfTarget as i32
        && t.target_area_radius > 0
        && t.effects.is_empty()
    {
        // Encounter signals such as Raise Calleil's Water have no unit effects. Every prop
        // that responds to the signal receives it, and the area count does not apply: sixteen
        // props answer Calleil's ten-target cue.
        let npcs = server.systems.skills.ext.npcs();
        possible.retain(|target| {
            target
                .as_obj()
                .is_some_and(|object| npcs.accepts_doodad_skill_hit(server, object, t.id))
        });
        over_count = false;
    }
    if over_count {
        let mut with_d: Vec<(f32, Target)> = possible
            .into_iter()
            .map(|p| (distance_to(server, &p, target_self, false), p))
            .collect();
        with_d.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        possible = with_d
            .into_iter()
            .take(t.target_area_count as usize)
            .map(|(_, p)| p)
            .collect();
    }
    let hostile = t.target_type == SkillTargetType::Hostile as i32;
    for target in &possible {
        let Some(o) = target.as_obj() else { continue };
        if o.is_unit() && hostile {
            let mut dice = roll_combat_dice(server, skill, caster, o);
            if t.level_rule_no_consideration {
                dice = level_rule_hit(skill);
            }
            skill.state().hit_types.entry(o.obj_id()).or_insert(dice);
            if let Some(tp) = o.as_player() {
                let cfg = server.systems.skills.config();
                let items = server.systems.skills.ext.items();
                let mut rate = if caster.as_player().is_some() {
                    cfg.pvp_durability_loss_rate
                } else {
                    cfg.pve_durability_loss_rate
                };
                rate *= items.durability_decrement_chance(server);
                if rate > 0.0 {
                    use super::combat::durability_targets as d;
                    match dice {
                        SkillHitType::MeleeBlock | SkillHitType::RangedBlock => {
                            items.durability_loss(server, tp, 1, d::SHIELD, rate)
                        }
                        SkillHitType::RangedParry | SkillHitType::MeleeParry => {
                            items.durability_loss(server, tp, 1, d::ALL_MAIN_WEAPONS, rate)
                        }
                        _ => {}
                    }
                }
            }
        } else if let Some(doodad) = o.as_doodad() {
            // Both rooms' no-attack areas accept Melkina's original open cue.
            let hit_skill =
                encounters::doodad_hit_skill(t.id, || doodad.data().template_id).unwrap_or(t.id);
            server
                .systems
                .skills
                .ext
                .npcs()
                .doodad_skill_hit(server, o, caster, hit_skill);
        }
    }

    let caster_level = caster.with_unit(|u| u.unit().level).unwrap_or(0);
    let force_attack = caster.with_unit(|u| u.unit().force_attack).unwrap_or(false);
    let caster_pos = world_pos(server, &caster_t);
    let mut to_apply: Vec<(Target, &SkillEffect)> = Vec::new();
    let mut last_applied: Option<&SkillEffect> = None;
    for effect in &t.effects {
        // These scripted cues run once from the caster. Hieronimus casts Howl
        // while a separate effect places the ward. Godfrey's timed dialogue
        // continuation would multiply across his audience. The final-room
        // cinematic timer belongs to the invisible cue actor.
        let method = if encounters::applies_to_source(t.id, effect.effect_id) {
            Some(ApplicationMethod::Source)
        } else {
            ApplicationMethod::from_raw(i64::from(effect.application_method))
        };
        let effected: Vec<Target> = match method {
            Some(ApplicationMethod::Target | ApplicationMethod::SourceToPos) => possible.clone(),
            Some(ApplicationMethod::Source) => vec![caster_t.clone()],
            Some(ApplicationMethod::SourceOnce) => {
                let mate_owner_mark = matches!(ctx.caster_caster, SkillCaster::Mount { .. })
                    && target_self.as_obj().is_some_and(|o| o.as_mate().is_some());
                let slave = target_self.as_obj().is_some_and(|o| o.as_slave().is_some());
                if mate_owner_mark || slave {
                    possible.clone()
                } else {
                    vec![caster_t.clone()]
                }
            }
            None => {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    application_method = effect.application_method,
                    "Skill effect skipped: unknown application method"
                );
                Vec::new()
            }
        };
        for target in effected {
            let relation = relation_to(server, &caster_t, &target);
            if effect.start_level > caster_level || effect.end_level < caster_level {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    caster_level,
                    start_level = effect.start_level,
                    end_level = effect.end_level,
                    "Skill effect skipped: caster level outside the effect's level range"
                );
                continue;
            }
            if effect.friendly && !effect.non_friendly && relation != RelationState::Friendly {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    ?relation,
                    "Skill effect skipped: the effect is friendly-only"
                );
                continue;
            }
            // A non-friendly effect on a unit target skips a protected player (Peace, no-PvP,
            // home zones). A Neutral target keeps main's behaviour: the whole `can_attack`
            // also refused neutral NPCs, which lost interaction / trap / spawner effects.
            if !effect.friendly
                && effect.non_friendly
                && target.as_obj().is_some_and(|o| o.is_unit())
                && protected_from(server, &caster_t, &target)
            {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    "Skill effect skipped: the target is protected from hostile effects"
                );
                continue;
            }
            if !effect.friendly
                && effect.non_friendly
                && relation != RelationState::Hostile
                && ((relation == RelationState::Friendly && !force_attack)
                    || caster_t.obj_id() == target.obj_id())
            {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    ?relation,
                    "Skill effect skipped: the effect is hostile-only"
                );
                continue;
            }
            let tp = world_pos(server, &target);
            if effect.front && !effect.back && !is_front(&caster_pos, &tp) {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    "Skill effect skipped: the target is behind the caster"
                );
                continue;
            }
            if !effect.front && effect.back && is_front(&caster_pos, &tp) {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    "Skill effect skipped: the effect is back-only"
                );
                continue;
            }
            if effect.source_buff_tag_id > 0
                && !buffs::check_buffs_by_tag(server, caster, effect.source_buff_tag_id)
            {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    tag = effect.source_buff_tag_id,
                    "Skill effect skipped: the caster lacks the required buff tag"
                );
                continue;
            }
            if effect.source_no_buff_tag_id > 0
                && buffs::check_buffs_by_tag(server, caster, effect.source_no_buff_tag_id)
            {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    tag = effect.source_no_buff_tag_id,
                    "Skill effect skipped: the caster has the forbidden buff tag"
                );
                continue;
            }
            if let Some(o) = target.as_obj() {
                if effect.target_buff_tag_id > 0
                    && !buffs::check_buffs_by_tag(server, o, effect.target_buff_tag_id)
                {
                    tracing::debug!(
                        skill_id = t.id,
                        effect_id = effect.effect_id,
                        target = target.obj_id(),
                        tag = effect.target_buff_tag_id,
                        "Skill effect skipped: the target lacks the required buff tag"
                    );
                    continue;
                }
                if effect.target_no_buff_tag_id > 0
                    && buffs::check_buffs_by_tag(server, o, effect.target_no_buff_tag_id)
                {
                    tracing::debug!(
                        skill_id = t.id,
                        effect_id = effect.effect_id,
                        target = target.obj_id(),
                        tag = effect.target_no_buff_tag_id,
                        "Skill effect skipped: the target has the forbidden buff tag"
                    );
                    continue;
                }
            }
            if effect.target_npc_tag_id > 0 {
                let ok = target.as_obj().and_then(|o| o.as_npc()).is_some_and(|n| {
                    let tid = n.data().unit.template_id;
                    data.npcs_by_tag(effect.target_npc_tag_id).contains(&tid)
                });
                if !ok {
                    tracing::debug!(
                        skill_id = t.id,
                        effect_id = effect.effect_id,
                        target = target.obj_id(),
                        tag = effect.target_npc_tag_id,
                        "Skill effect skipped: the target NPC is not in the required tag"
                    );
                    continue;
                }
            }
            if !effects::chance_succeeds(effect.chance) {
                tracing::debug!(
                    skill_id = t.id,
                    effect_id = effect.effect_id,
                    target = target.obj_id(),
                    chance = effect.chance,
                    "Skill effect skipped: the chance roll failed"
                );
                continue;
            }
            if let (Some(last), Some((EffectKind::NpcSpawnerSpawnEffect, id))) =
                (last_applied, effect.template)
            {
                let spawner =
                    |e: &SkillEffect| match e.template.and_then(|(k, i)| data.effect(k, i)) {
                        Some(EffectTemplate::NpcSpawnerSpawn(s)) => Some(s.spawner_id),
                        _ => None,
                    };
                if last.effect_id == effect.effect_id && spawner(last) == spawner(effect) {
                    let _ = id;
                    continue;
                }
            }
            to_apply.push((target, effect));
            last_applied = Some(effect);
        }
    }

    let items = server.systems.skills.ext.items();
    let mut consumed_items: Vec<(u64, i32)> = Vec::new();
    let mut consumed_templates: Vec<(u32, i32)> = Vec::new();
    let paid_building = |target: &Target, effect: &SkillEffect| {
        player.is_some()
            && target.as_obj().and_then(WorldObject::as_house).is_some()
            && effect
                .template
                .and_then(|(kind, id)| data.effect(kind, id))
                .is_some_and(|template| {
                    crate::systems::housing::is_building_effect(server, template)
                })
    };
    // `SavePortal` creates an entry only after its own atomic reagent take in `misc.rs`; the
    // generic optional effect-item debit must not take a second one.
    let save_portal = t.effects.iter().any(|effect| {
        effect
            .template
            .and_then(|(kind, id)| data.effect(kind, id))
            .is_some_and(|template| {
                matches!(
                    template,
                    EffectTemplate::Special(s) if s.special_effect_type_id == 30
                )
            })
    });
    // The cast item is the source of the skill: consume it (or only re-verify it when the skill
    // consumes nothing) before the reagents, products and effects run, so an item that left the
    // bag during a cast time refuses the cast instead of firing for free.
    let mut owned_item_use: Option<(u64, u32, i32)> = None;
    if let SkillCaster::Item {
        item_id,
        item_template_id,
        ..
    } = ctx.caster_caster
    {
        let amount = last_applied.map_or(0, |last| {
            // Deviation: C# looked at the last applied effect only. Skills with mixed flags (a
            // consuming GainItem followed by an Interaction effect) never consumed the item, so
            // the last applied effect that carries `consume_source_item` / `consume_item_id`
            // wins.
            let applied = || to_apply.iter().map(|(_, e)| *e);
            if let Some(e) = applied().rev().find(|e| e.consume_source_item) {
                e.consume_item_count
            } else if items.use_skill_as_reagent(server, item_template_id) {
                last.consume_item_count
            } else {
                0
            }
        });
        owned_item_use = Some((item_id, item_template_id, amount));
    }
    if last_applied.is_some() {
        let applied = || to_apply.iter().map(|(_, e)| *e);
        let template_effect = applied()
            .rev()
            .find(|e| e.consume_item_id != 0 && e.consume_item_count > 0);
        if let Some(p) = &player {
            if let Some(e) = template_effect.filter(|e| {
                // SavePortal pays its effect items in its own atomic step; house construction
                // pays at the progress commit, including stale/rejected targets. Never fall
                // back to the optional debit for those.
                !save_portal
                    && !to_apply.iter().any(|(target, candidate)| {
                        std::ptr::eq(*candidate, *e) && paid_building(target, candidate)
                    })
            }) {
                // source item consumption, or enough of the item in the bag / equipment
                if e.consume_source_item
                    || items.item_count(server, p, e.consume_item_id) >= e.consume_item_count
                {
                    consumed_templates.push((e.consume_item_id, e.consume_item_count));
                }
            }
        }
    }
    // Debit the cast item before anything else can be granted: the owner, template and count
    // must still match and it must sit in the caster's bag or equipment. The item stays out of
    // its container until the result is committed, so no mover can reach it mid-cast; a missing
    // item refuses the whole result (no reagents, products or effects).
    let mut source_debit: Option<CastSourceDebit<'_>> = None;
    let mut source_item = None;
    let mut source_item_debited = false;
    if let (Some((item_id, template_id, amount)), Some(p)) = (owned_item_use, &player) {
        match items.take_cast_source_item(server, p, item_id, template_id, amount) {
            Some(Ok(taken)) => {
                source_item = Some(std::sync::Arc::new(taken.item.clone()));
                source_item_debited = taken.amount > 0;
                source_debit = Some(CastSourceDebit {
                    server,
                    taken: Some(taken),
                });
            }
            Some(Err(())) => {
                tracing::warn!(
                    skill_id = t.id,
                    item_id,
                    item_template_id = template_id,
                    amount,
                    "Skill cast cancelled: the source item is no longer in the caster's bag or equipment"
                );
                skill.state().cancelled = true;
                stop(server, skill, caster, 0);
                return;
            }
            None => {
                // Hooks that do not track items: keep the owner-check semantics with the read
                // API they provide.
                let ok = items
                    .owned_item(server, p, item_id)
                    .is_some_and(|(tid, count)| tid == template_id && count >= amount.max(0));
                if !ok {
                    tracing::warn!(
                        skill_id = t.id,
                        item_id,
                        item_template_id = template_id,
                        amount,
                        "Skill cast cancelled: the source item is not owned in the required template and count"
                    );
                    skill.state().cancelled = true;
                    stop(server, skill, caster, 0);
                    return;
                }
                source_item = server.items.item_by_id(item_id).map(std::sync::Arc::new);
            }
        }
    }
    let reagents = data.reagents_for(t.id);
    let products = data.products_for(t.id);
    if !save_portal && (!reagents.is_empty() || !products.is_empty()) {
        if let Some(p) = &player {
            if !reagents.is_empty() {
                let mut found = false;
                for r in &reagents {
                    if items.item_count(server, p, r.item_id) >= i32::from(r.amount) {
                        consumed_templates.push((r.item_id, i32::from(r.amount)));
                        found = true;
                        if t.first_reagent_only {
                            break;
                        }
                    } else if !t.first_reagent_only {
                        skill.state().cancelled = true;
                        return;
                    }
                }
                if !found {
                    skill.state().cancelled = true;
                    return;
                }
            }
            for prod in &products {
                items.acquire_item(server, p, prod.item_id, i32::from(prod.amount));
            }
        }
    }

    // Weighted selection
    let weighted_total: i32 = to_apply.iter().map(|(_, e)| e.weight).sum();
    let mut selected = -1i32;
    if weighted_total > 0 {
        selected = rand::thread_rng().gen_range(0..weighted_total);
    }
    let mut current_weight = 0;
    let mut packets = CompressedGamePackets::new();
    let mut deferred_application = None;
    let replaces_effects = encounters::is_combuster_self_destruct_skill(t.id);
    let mut effect_source = EffectSource::from_skill(skill.clone());
    if save_portal {
        effect_source.cast_target = target_self.as_obj().cloned();
    }
    effect_source.source_item = source_item.clone();
    effect_source.source_item_debited = source_item_debited;
    let tl = skill.tl_id();
    for (target, effect) in &to_apply {
        if effect.weight > 0 {
            if selected == -1 {
                continue;
            }
            current_weight += effect.weight;
            if selected >= current_weight {
                continue;
            }
            selected = -1;
        }
        let Some((kind, id)) = effect.template else {
            tracing::error!(
                "Template not found for Skill[{}] Effect[{}]",
                t.id,
                effect.effect_id
            );
            continue;
        };
        let deferred_effect = if replaces_effects {
            encounter_hooks::all().iter().find_map(|encounter| {
                encounter.deferred_effect_replacement(&t, kind, id, caster, target)
            })
        } else {
            None
        };
        let (kind, id) = deferred_effect.unwrap_or((kind, id));
        let Some(template) = data.effect(kind, id) else {
            tracing::debug!(
                skill_id = t.id,
                effect_id = effect.effect_id,
                ?kind,
                id,
                "Skill effect skipped: the effect template is missing from the game data"
            );
            continue;
        };
        // Position casts (ground / ballistic / summon placement) have no unit obj id: keep the
        // original target so effects can read the chosen coordinates and rotation.
        let this_target_caster = if matches!(
            ctx.target_caster,
            SkillCastTarget::Position { .. }
                | SkillCastTarget::Position2 { .. }
                | SkillCastTarget::Position3 { .. }
        ) || target.obj_id() == ctx.target_caster.obj_id()
        {
            ctx.target_caster
        } else {
            SkillCastTarget::Unit(target.obj_id())
        };
        let mut args = EffectArgs {
            caster: Some(caster.clone()),
            caster_caster: ctx.caster_caster,
            target: target.clone(),
            target_caster: this_target_caster,
            cast: CastAction::Skill {
                skill_id: t.id,
                tl_id: tl,
            },
            source: effect_source.clone(),
            skill_object: &ctx.skill_object,
        };
        args.source.always_hit = effect.always_hit;
        if paid_building(target, effect) {
            let Some(EffectTemplate::Craft {
                world_interaction, ..
            }) = effect.template.and_then(|(kind, id)| data.effect(kind, id))
            else {
                unreachable!("paid building requires a craft effect");
            };
            // This is the actual selected effect and target, rather than the router's
            // "handled" bool (which says nothing about whether construction succeeded).
            let p = player.as_ref().expect("paid building has a player");
            let house = target
                .as_obj()
                .and_then(WorldObject::as_house)
                .expect("paid building has a house");
            match crate::systems::housing::ops::craft_building_paid(
                server,
                p,
                house,
                t.id,
                effect.consume_item_id,
                effect.consume_item_count,
            ) {
                Ok(()) => {
                    let template_id = house.data().unit.template_id;
                    crate::systems::quests::events::on_world_interaction(
                        server,
                        p,
                        p,
                        template_id,
                        *world_interaction as i32,
                        0,
                    );
                }
                Err(error) => {
                    // The cast's own effect refused its payment: refund the source item.
                    skill.state().effect_cancelled = true;
                    if error == crate::systems::housing::ops::BuildActionError::MissingMaterial {
                        let mut packet = SCSkillStartedPacket::new(
                            skill.id,
                            tl,
                            ctx.caster_caster,
                            this_target_caster,
                            ctx.skill_object.clone(),
                        );
                        packet
                            .set_skill_result(unit_reqs::skill_result_for_key("skill_need_reagent"))
                            .set_result_uint(0);
                        p.send(packet);
                    }
                    stop(server, skill, caster, 0);
                    return;
                }
            }
        } else if let EffectTemplate::KillNpcWithoutCorpse(k) = template {
            let npc = nearest_kill_npc(server, caster, k.npc_id, k.radius);
            let effective = npc
                .clone()
                .or_else(|| target.as_obj().and_then(|o| o.as_npc().cloned()));
            if effective.as_ref().is_some_and(|n| n.data().unit.is_dead()) {
                // skip: KillNpcWithoutCorpse happens before death
            } else {
                if let Some(n) = npc {
                    args.caster = Some(WorldObject::from(n));
                }
                if deferred_effect.is_some() {
                    deferred_application = Some((kind, template, args));
                } else {
                    effects::apply(server, kind, template, &args, Some(&mut packets));
                }
            }
        } else {
            effects::apply(server, kind, template, &args, Some(&mut packets));
            // Dyeing rejects through the C# `Character.SkillCancelled` flag (the hook has no
            // `SkillRun` handle). `cast` clears that flag when it accepts the cast, so a true
            // value seen here was set by one of this cast's own effects; record it in this
            // run's state instead of consulting the character flag at refund time.
            if player
                .as_ref()
                .is_some_and(|p| p.data().skill_char.skill_cancelled)
            {
                skill.state().cancelled = true;
                skill.state().effect_cancelled = true;
            }
        }
        if effect.item_set_id > 0 {
            consumed_templates.extend(items.item_set(server, effect.item_set_id));
        }
    }
    if let SkillCaster::Item {
        item_id,
        item_template_id,
        ..
    } = ctx.caster_caster
    {
        let chance = caster
            .with_unit(|u| u.unit().skill_state.condition_chance)
            .unwrap_or(false);
        if chance {
            let Some(p) = &player else {
                return;
            };
            server
                .systems
                .skills
                .ext
                .quests()
                .item_use_template(server, p, item_template_id);
            if items.use_skill_as_reagent(server, item_template_id)
                && reagents.is_empty()
                && products.is_empty()
                && consumed_items.is_empty()
                && t.effects.is_empty()
            {
                consumed_items.push((item_id, 1));
            }
        }
    }
    if !packets.is_empty() {
        broadcast(server, caster, &packets, true);
    }
    if let Some((kind, template, args)) = deferred_application {
        effects::apply(server, kind, template, &args, None);
    }
    // Hack to consume TreasureMap items (as the C#).
    if let Some(p) = player.as_ref().filter(|_| t.id == DIG_UP_TREASURE_SKILL) {
        match unit_reqs::treasure_map_nearby(server, Some(p), unit_reqs::TREASURE_MAP_DIG_RANGE) {
            Some(map_id) => consumed_items.push((map_id, 1)),
            None => {
                let (name, id) = {
                    let c = p.data();
                    (c.unit.name.clone(), c.id)
                };
                tracing::error!(
                    "Unable to find a treasure map to take from user {name} ({id}) when digging up treasure"
                );
            }
        }
    }
    // An effect that cancels itself (invalid UCC/dye target, missing build material) refunds
    // the source item it could not charge for. A client / channel stop that lands while the
    // effects run (no effect cancelled) still pays for the effects that went out.
    // Only this cast's own run state decides the refund: the per-character flag can be left
    // set by another skill and must not make a successful cast refund its source.
    let effect_cancelled = skill.state().effect_cancelled;
    if let Some(mut debit) = source_debit {
        if !effect_cancelled {
            debit.commit();
        }
    }
    if !skill.cancelled() {
        if let Some(p) = &player {
            for (item, amount) in consumed_items {
                items.consume_item_by_id(server, p, item, amount);
            }
            for (tid, amount) in consumed_templates {
                items.consume_item_template(server, p, tid, amount);
            }
        }
    }
}
