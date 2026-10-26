//! Provisional server AoE policy regressions, not retail parity claims. Native clients
//! expose descriptors/diagnostics, but the authoritative world-server counter is unavailable.
//! These tests drive real plot effects and combat; expected ranks implement the reviewed
//! compatibility policy in PORTING.md. No client packet format is introduced or changed.

use super::*;

use crate::data::world_data::SystemFaction;
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::models::unit::{Bonus, BonusTemplate, SkillModifier, UnitAttribute, UnitModifierType};
use crate::test_support::TestClient;
use crate::world::{Entity, NpcRef, PlayerRef};

use super::super::data::effects::{BuffEffect, HealEffect};
use super::super::data::misc::AreaShape;
use super::super::data::plots::{PlotEventEffect, PlotNextEvent, PlotNode};
use super::super::data::{BuffTemplate, SkillTemplate};
use super::super::state::SkillRun;
use super::super::types::{SkillCastTarget, SkillCaster, SkillHitType, SkillObject};

const PLOT: u32 = 1;
const SKILL: u32 = 11939;
const DAMAGE: u32 = 3172;
const DAMAGE_EVENT: u32 = 2723;
const RESET_EVENT: u32 = 246;
const UNFLAGGED_EVENT: u32 = 900_001;
const OTHER_FLAGGED_EVENT: u32 = 900_002;
const NON_DAMAGE_EVENT: u32 = 900_003;
const HP: i32 = 100_000;

fn plot_effect(actual_type: &str, actual_id: u32) -> PlotEventEffect {
    PlotEventEffect {
        position: 1,
        source_id: 1,
        target_id: 4,
        actual_id,
        actual_type: actual_type.into(),
    }
}

fn event(id: u32, diminishing: bool, effects: Vec<PlotEventEffect>) -> PlotEventTemplate {
    PlotEventTemplate {
        id,
        plot_id: PLOT,
        position: 1,
        source_update_method_id: 1,
        target_update_method_id: 4,
        aoe_diminishing: diminishing,
        effects,
        ..Default::default()
    }
}

fn data() -> GameData {
    let mut data = GameData::empty();
    data.formulas = crate::systems::vehicles::stats::tests::formulas(
        &[(1, "100000", 14, 0), (2, "1000", 15, 0)],
        &[],
    );
    for id in [3, 4] {
        data.factions.add(SystemFaction {
            id,
            ..Default::default()
        });
    }
    let skills = &mut data.systems.skills;
    skills.plots.aoe_diminishings = [
        (1, 100.0),
        (2, 90.0),
        (3, 80.0),
        (4, 70.0),
        (5, 55.0),
        (6, 40.0),
        (7, 25.0),
        (8, 10.0),
        (9, 10.0),
        (10, 10.0),
    ]
    .into();
    skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(super::super::tests::damage_effect(DAMAGE, 1000))),
        1,
    );
    skills.add_effect(
        EffectKind::ResetAoeDiminishingEffect,
        EffectTemplate::ResetAoeDiminishing { id: 2 },
        2,
    );
    for (id, diminishing) in [
        (DAMAGE_EVENT, true),
        (UNFLAGGED_EVENT, false),
        (OTHER_FLAGGED_EVENT, true),
    ] {
        skills.plots.events.insert(
            id,
            event(id, diminishing, vec![plot_effect("DamageEffect", DAMAGE)]),
        );
    }
    // Native plot_effects row 5680: event 246, descriptor 2, source 1, target 3.
    let mut reset = plot_effect("ResetAoeDiminishingEffect", 2);
    reset.target_id = 3;
    skills
        .plots
        .events
        .insert(RESET_EVENT, event(RESET_EVENT, false, vec![reset]));
    data
}

fn fixture(data: GameData, count: u32) -> (Arc<GameServer>, TestClient, PlayerRef, Vec<NpcRef>) {
    let (server, client, player, _) = super::super::cast_validation_tests::fixture(data);
    player.data().unit.faction_id = 4;
    super::super::stats::refresh_cached_stats(&server, &WorldObject::from(&player));
    let targets = (0..count)
        .map(|i| {
            let mut npc = Npc::new(1);
            npc.unit.hp = HP;
            npc.unit.max_hp = HP;
            npc.unit.faction_id = 3;
            npc.unit
                .transform
                .set_xyz(100.0 + i as f32 * 0.01, 100.0, 100.0);
            let npc = Entity::new(0x300 + i, npc);
            server
                .world
                .world(0)
                .unwrap()
                .spawn(&npc, &server.view_ctx());
            npc
        })
        .collect();
    (server, client, player, targets)
}

fn state(caster: &WorldObject, target: &NpcRef, skill: Option<SkillRef>) -> Arc<PlotState> {
    let skill = skill.unwrap_or_else(|| {
        SkillRun::new(
            Arc::new(SkillTemplate {
                plot_id: Some(PLOT),
                plot_only: true,
                target_alive: true,
                target_relation: 4,
                ..super::super::tests::skill_template(SKILL)
            }),
            1,
        )
    });
    PlotState::new(
        skill,
        CastCtx {
            caster: caster.clone(),
            caster_caster: SkillCaster::Unit(caster.obj_id()),
            target: Target::Obj(WorldObject::from(target)),
            target_caster: SkillCastTarget::Unit(target.obj_id),
            skill_object: SkillObject::None,
        },
    )
}

fn node(event_id: u32) -> PlotNode {
    PlotNode {
        event_id,
        parent: None,
        parent_next_event: None,
        children: vec![],
    }
}

/// Real PlotNode -> apply_plot_effect -> effect dispatch -> damage/heal/buff execution.
fn fire(server: &GameServer, state: &Arc<PlotState>, event_id: u32, targets: &[NpcRef]) {
    let mut ti = PlotTargetInfo::new(
        Some(Target::Obj(state.caster().clone())),
        Some(Target::Obj(WorldObject::from(&targets[0]))),
    );
    ti.effected = targets
        .iter()
        .map(|t| Target::Obj(WorldObject::from(t)))
        .collect();
    execute_node(
        server,
        &PlotTree {
            plot_id: PLOT,
            nodes: vec![node(event_id)],
        },
        0,
        state,
        &ti,
    );
}

fn damage(
    server: &GameServer,
    state: &Arc<PlotState>,
    event_id: u32,
    targets: &[NpcRef],
) -> Vec<i32> {
    let before: Vec<_> = targets.iter().map(|t| t.data().unit.hp).collect();
    fire(server, state, event_id, targets);
    before
        .into_iter()
        .zip(targets)
        .map(|(hp, t)| hp - t.data().unit.hp)
        .collect()
}

#[tokio::test]
async fn aoe_policy_plot_damage_targets_one_through_twelve_and_reuses_floor_targets() {
    let (server, _client, player, targets) = fixture(data(), 12);
    let state = state(&WorldObject::from(&player), &targets[0], None);
    assert_eq!(
        damage(&server, &state, DAMAGE_EVENT, &targets),
        [1000, 900, 800, 700, 550, 400, 250, 100, 100, 100, 100, 100]
    );
    assert_eq!(state.inner().aoe_diminishing_ranks.len(), 10);
    // Stored early identities retain their rank, and unstored late identities retain the floor.
    let reverse: Vec<_> = targets.into_iter().rev().collect();
    assert_eq!(
        damage(&server, &state, OTHER_FLAGGED_EVENT, &reverse),
        [100, 100, 100, 100, 100, 250, 400, 550, 700, 800, 900, 1000]
    );
    assert_eq!(state.inner().aoe_diminishing_ranks.len(), 10);
}

#[tokio::test]
async fn aoe_policy_unflagged_damage_neither_diminishes_nor_consumes_a_rank() {
    let (server, _client, player, targets) = fixture(data(), 3);
    let state = state(&WorldObject::from(&player), &targets[0], None);
    assert_eq!(damage(&server, &state, DAMAGE_EVENT, &targets[..1]), [1000]);
    assert_eq!(
        damage(&server, &state, UNFLAGGED_EVENT, &targets[1..]),
        [1000, 1000]
    );
    assert_eq!(state.inner().aoe_diminishing_ranks.len(), 1);
    assert_eq!(
        damage(&server, &state, OTHER_FLAGGED_EVENT, &targets[2..]),
        [900]
    );
    assert_eq!(damage(&server, &state, DAMAGE_EVENT, &targets[..1]), [1000]);
}

#[tokio::test]
async fn aoe_policy_reset_is_local_to_plot_even_with_reused_skill_and_other_casters() {
    let (server, _client, player, targets) = fixture(data(), 4);
    let caster = WorldObject::from(&player);
    let first = state(&caster, &targets[0], None);
    // Deliberately reuse the same SkillRun across two plot instances: context must remain explicit.
    let second = state(&caster, &targets[0], Some(first.skill.clone()));
    let other = state(&WorldObject::from(&targets[3]), &targets[0], None);
    assert_eq!(
        damage(&server, &first, DAMAGE_EVENT, &targets[..2]),
        [1000, 900]
    );
    assert_eq!(
        damage(&server, &second, DAMAGE_EVENT, &targets[..2]),
        [1000, 900]
    );
    // This caster has no faction, so the ordinary can_attack null-faction rule permits the hit.
    targets[3].data().unit.faction_id = 0;
    assert_eq!(damage(&server, &other, DAMAGE_EVENT, &targets[..1]), [1000]);
    fire(&server, &first, RESET_EVENT, &targets[..1]);
    assert!(first.inner().aoe_diminishing_ranks.is_empty());
    assert_eq!(second.inner().aoe_diminishing_ranks.len(), 2);
    assert_eq!(other.inner().aoe_diminishing_ranks.len(), 1);
    assert_eq!(
        damage(&server, &first, DAMAGE_EVENT, &targets[2..3]),
        [1000]
    );
    assert_eq!(
        damage(&server, &second, DAMAGE_EVENT, &targets[2..3]),
        [800]
    );
    assert_eq!(damage(&server, &other, DAMAGE_EVENT, &targets[1..2]), [900]);
    let weak = Arc::downgrade(&first);
    drop(first);
    assert!(
        weak.upgrade().is_none(),
        "no plot context may be retained in its SkillRun or caster"
    );
    let fresh = state(&caster, &targets[0], Some(second.skill.clone()));
    assert_eq!(
        damage(&server, &fresh, DAMAGE_EVENT, &targets[2..3]),
        [1000]
    );
}

#[tokio::test]
async fn aoe_policy_repeat_waves_reuse_ranks_until_authored_reset_effect() {
    let (server, _client, player, targets) = fixture(data(), 3);
    let state = state(&WorldObject::from(&player), &targets[0], None);
    for wave in 0..3 {
        let mut order = targets.clone();
        order.rotate_left(wave);
        assert_eq!(
            damage(&server, &state, DAMAGE_EVENT, &order[..2]),
            [1000, 900]
        );
        assert_eq!(damage(&server, &state, DAMAGE_EVENT, &order[2..]), [800]);
        assert_eq!(
            damage(&server, &state, OTHER_FLAGGED_EVENT, &order[..2]),
            [1000, 900]
        );
        fire(&server, &state, RESET_EVENT, &targets[..1]);
        assert!(state.inner().aoe_diminishing_ranks.is_empty());
    }
}

fn next(id: u32, event_id: u32, per_target: bool, delay: i32, speed: i32) -> PlotNextEvent {
    PlotNextEvent {
        id,
        event_id,
        position: 1,
        per_target,
        delay,
        speed,
        casting: false,
        channeling: false,
        casting_inc: 0,
        add_anim_cs_time: false,
        casting_delayable: false,
        casting_cancelable: false,
        cancel_on_big_hit: false,
        use_exe_time: false,
        fail: false,
    }
}

#[tokio::test]
async fn aoe_policy_queued_per_target_projectiles_share_one_plot_wave() {
    let mut data = data();
    data.systems.skills.area_shapes.insert(
        1,
        AreaShape {
            id: 1,
            kind: 1,
            value1: 10.0,
            value2: 0.0,
            value3: 0.0,
        },
    );
    let mut area = event(3, false, vec![]);
    area.target_update_method_id = 5;
    area.target_update_method_params = [1, 12, 0, 0, 0, 0, 0, 4, 2];
    // Actual Searing Rain fan-out: next 3195, event 3 -> 2723, per_target, speed 20.
    // Targets are within 11cm of the caster to keep real projectile delays short.
    area.next_events.push(next(3195, DAMAGE_EVENT, true, 0, 20));
    data.systems.skills.plots.events.insert(3, area);
    let mut root = node(3);
    root.children = vec![1];
    let mut child = node(DAMAGE_EVENT);
    child.parent = Some(0);
    child.parent_next_event = Some((3, 0));
    data.systems.skills.plots.trees.insert(
        PLOT,
        PlotTree {
            plot_id: PLOT,
            nodes: vec![root, child],
        },
    );
    let (server, _client, player, targets) = fixture(data, 12);
    let state = state(&WorldObject::from(&player), &targets[0], None);
    execute_tree(&server, PLOT, &state).await;
    // World iteration decides which target is first. Do not impose a new sort on production.
    let mut dealt: Vec<_> = targets.iter().map(|t| HP - t.data().unit.hp).collect();
    dealt.sort_unstable();
    assert_eq!(
        dealt,
        [100, 100, 100, 100, 100, 250, 400, 550, 700, 800, 900, 1000]
    );
    assert_eq!(state.inner().aoe_diminishing_ranks.len(), 10);
    assert_eq!(state.inner().tickets[&DAMAGE_EVENT], 12);
}

#[tokio::test]
async fn aoe_policy_misses_and_immunity_occupy_and_reuse_ranks() {
    let mut data = data();
    data.systems.skills.add_buff(BuffTemplate {
        id: 900,
        kind: 1,
        stack_rule: 1,
        melee_immune: true,
        ..Default::default()
    });
    let (server, _client, player, targets) = fixture(data, 3);
    let caster = WorldObject::from(&player);
    let state = state(&caster, &targets[0], None);
    state
        .skill
        .state()
        .hit_types
        .insert(targets[0].obj_id, SkillHitType::MeleeMiss);
    super::super::buffs::apply_template(
        &server,
        server.game_data.systems.skills.buff(900).unwrap(),
        Some(&caster),
        &SkillCaster::Unit(player.obj_id),
        &WorldObject::from(&targets[1]),
        None,
    );
    assert!(super::super::buffs::check_damage_immune(
        &server,
        &WorldObject::from(&targets[1]),
        Some(super::super::types::DamageType::Melee),
    ));
    assert_eq!(damage(&server, &state, DAMAGE_EVENT, &targets), [0, 0, 800]);
    assert_eq!(state.inner().aoe_diminishing_ranks.len(), 3);
    state
        .skill
        .state()
        .hit_types
        .insert(targets[0].obj_id, SkillHitType::MeleeHit);
    super::super::buffs::remove_buff(&server, &WorldObject::from(&targets[1]), 900);
    assert_eq!(
        damage(&server, &state, OTHER_FLAGGED_EVENT, &targets[..2]),
        [1000, 900]
    );
}

#[tokio::test]
async fn aoe_policy_flagged_heals_and_buffs_do_not_consume_ranks_or_scale() {
    let mut data = data();
    let skills = &mut data.systems.skills;
    skills.add_buff(BuffTemplate {
        id: 901,
        kind: 2,
        stack_rule: 1,
        stun: true,
        ..Default::default()
    });
    skills.buff_tags.insert(901, vec![6]); // Native 1.2 Stun tag.
    skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: 8,
            buff_id: Some(901),
            chance: 100,
            stack: 1,
            ab_level: 1,
        }),
        8,
    );
    skills.add_effect(
        EffectKind::HealEffect,
        EffectTemplate::Heal(HealEffect {
            id: 9,
            use_fixed_heal: true,
            fixed_min: 1000,
            fixed_max: 1000,
            use_level_heal: false,
            level_md: 0.0,
            level_va_start: 0,
            level_va_end: 0,
            percent: false,
            use_charged_buff: false,
            charged_buff_id: 0,
            charged_mul: 0.0,
            slave_applicable: false,
            ignore_heal_aggro: true,
            dps_multiplier: 0.0,
            actability_group_id: 0,
            actability_step: 0,
            actability_mul: 0.0,
            actability_add: 0.0,
        }),
        9,
    );
    skills.plots.events.insert(
        NON_DAMAGE_EVENT,
        event(
            NON_DAMAGE_EVENT,
            true,
            vec![plot_effect("HealEffect", 9), plot_effect("BuffEffect", 8)],
        ),
    );
    let (server, _client, player, targets) = fixture(data, 3);
    let state = state(&WorldObject::from(&player), &targets[0], None);
    assert_eq!(damage(&server, &state, DAMAGE_EVENT, &targets[..1]), [1000]);
    for target in &targets {
        target.data().unit.hp = 50_000;
    }
    fire(&server, &state, NON_DAMAGE_EVENT, &targets);
    for target in &targets {
        let unit = &target.data().unit;
        assert_eq!(unit.hp, 51_000);
        assert!(unit.buffs.check_buff(901));
    }
    assert_eq!(state.inner().aoe_diminishing_ranks.len(), 1);
    assert_eq!(damage(&server, &state, DAMAGE_EVENT, &targets[2..]), [900]);
}

fn add_bonus(unit: &mut crate::models::unit::UnitBase, attribute: UnitAttribute, value: i32) {
    unit.bonuses.add_bonus(
        999,
        Bonus {
            template: BonusTemplate {
                attribute,
                modifier_type: UnitModifierType::Value,
                value,
                linear_level_bonus: 0,
            },
            value,
        },
    );
}

struct Armored;

impl super::super::ext::NpcSkillHooks for Armored {
    fn npc_attribute(&self, _: &GameServer, _: &Npc, attr: StatKind) -> Option<f64> {
        // Exercise the real damage reduction without requiring NPC reference-data loaders.
        (attr == StatKind::Armor).then_some(5300.0)
    }
}

#[tokio::test]
async fn aoe_policy_late_fractional_rate_preserves_physical_fixed_and_modifier_order() {
    let mut data = data();
    data.systems.skills.plots.aoe_diminishings.insert(2, 77.5);
    let mut physical = super::super::tests::damage_effect(10, 0);
    physical.use_fixed_damage = false;
    physical.use_mainhand_weapon = true;
    physical.dps_multiplier = 1.0;
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(physical)),
        10,
    );
    data.systems.skills.plots.events.insert(
        OTHER_FLAGGED_EVENT,
        event(
            OTHER_FLAGGED_EVENT,
            true,
            vec![plot_effect("DamageEffect", 10)],
        ),
    );
    let (server, _client, player, targets) = fixture(data, 2);
    server.systems.skills.ext.npcs.install(Arc::new(Armored));
    {
        let unit = &mut player.data().unit;
        // Exactly one DPS avoids random floor/ceil widening in the weapon-damage base.
        add_bonus(unit, UnitAttribute::MainhandDps, 1000);
        unit.skill_modifiers.add_modifiers(&[SkillModifier {
            id: 1,
            owner_id: 1,
            tag_id: 0,
            skill_attribute: SkillAttribute::Damage,
            unit_modifier_type: UnitModifierType::Value,
            value: 109,
            skill_id: SKILL,
            synergy: false,
        }]);
    }
    let state = state(&WorldObject::from(&player), &targets[0], None);
    // Physical 1 -> skill modifier +109 -> armor 55 -> authored target-2 rate 77.5% -> 42.
    assert_eq!(
        damage(&server, &state, OTHER_FLAGGED_EVENT, &targets),
        [55, 42]
    );
    // Fixed 1000 still replaces skill-modified base damage before the existing armor reduction.
    assert_eq!(damage(&server, &state, DAMAGE_EVENT, &targets), [500, 387]);
    assert_eq!(
        damage(&server, &state, UNFLAGGED_EVENT, &targets),
        [500, 500]
    );
}

#[tokio::test]
async fn aoe_policy_minimum_applies_after_integer_damage_truncation_and_never_heals() {
    let mut data = data();
    let mut tiny = super::super::tests::damage_effect(DAMAGE, 7);
    tiny.health_steal_ratio = 100;
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(tiny)),
        1,
    );
    data.systems.skills.plots.aoe_diminishings.insert(2, 90.0);
    data.systems.skills.plots.aoe_diminishings.insert(3, -100.0);
    let (server, _client, player, targets) = fixture(data, 3);
    server.systems.skills.ext.npcs.install(Arc::new(Armored));
    let state = state(&WorldObject::from(&player), &targets[0], None);
    // Armor leaves 3.5 -> integer 3, then rank-2 90% -> 2. Multiplying before that
    // first truncation would incorrectly produce 3. A negative authored rate cannot heal.
    player.data().unit.hp = 100;
    assert_eq!(damage(&server, &state, DAMAGE_EVENT, &targets), [3, 2, 1]);
    assert_eq!(player.data().unit.hp, 106);
}

#[tokio::test]
async fn aoe_policy_negative_damage_cannot_raise_target_health() {
    let mut data = data();
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(super::super::tests::damage_effect(DAMAGE, -10))),
        1,
    );
    let (server, _client, player, targets) = fixture(data, 1);
    let state = state(&WorldObject::from(&player), &targets[0], None);
    assert_eq!(damage(&server, &state, DAMAGE_EVENT, &targets), [1]);
}

#[tokio::test]
async fn aoe_policy_rank_storage_is_bounded_and_missing_table_retains_no_state() {
    let (server, _client, player, targets) = fixture(data(), 1);
    let state = state(&WorldObject::from(&player), &targets[0], None);
    let rates = &server.game_data.systems.skills.plots.aoe_diminishings;
    for id in 1..=10_000 {
        state.aoe_damage_multiplier(rates, id);
    }
    assert_eq!(state.inner().aoe_diminishing_ranks.len(), 10);
    assert_eq!(state.aoe_damage_multiplier(rates, 1), 1.0);
    assert_eq!(state.aoe_damage_multiplier(rates, 10_000), 0.1);
    fire(&server, &state, RESET_EVENT, &targets);
    assert!(state.inner().aoe_diminishing_ranks.is_empty());
    assert_eq!(state.aoe_damage_multiplier(&HashMap::new(), 1), 1.0);
    assert!(state.inner().aoe_diminishing_ranks.is_empty());
}
