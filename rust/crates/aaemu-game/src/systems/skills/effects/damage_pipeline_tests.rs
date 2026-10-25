//! Damage pipeline regressions. Native 1.2 evidence: x2game12.c sub_398A7380
//! (float level range), sub_39898330 (integer terms, final min 1 and late AoE),
//! sub_397C8760/sub_397C7A40/sub_397C85B0 (damage/cast/caster wire fields).
//! Percent evaluation is a compatibility interpretation of actual 1.2 descriptors;
//! the authoritative server formula is unavailable. No 10.x factors are imported.

use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::data::world_data::SystemFaction;
use crate::data::GameData;
use crate::models::doodad::Doodad;
use crate::models::npc::Npc;
use crate::models::unit::{Bonus, BonusTemplate, UnitAttribute, UnitModifierType};
use crate::opcodes::sc;
use crate::test_support::TestClient;
use crate::world::{Entity, WorldObject};

use super::super::super as skills;
use super::super::EffectSource;
use super::*;

fn fixture_data() -> GameData {
    let mut data = GameData::empty();
    data.formulas = crate::systems::vehicles::stats::tests::formulas(
        // Character formulas use owner 0; owner 5 belongs to mates.
        &[(1, "100000", 14, 0), (2, "1000", 15, 0), (3, "100", 12, 0)],
        &[],
    );
    for id in [3, 4] {
        data.factions.add(SystemFaction {
            id,
            ..Default::default()
        });
    }
    data.systems.skills.add_buff(skills::data::BuffTemplate {
        id: 900,
        kind: 1,
        stack_rule: 1,
        melee_immune: true,
        ..Default::default()
    });
    data
}

fn args(caster: WorldObject, target: WorldObject, level: u8) -> EffectArgs<'static> {
    let skill = skills::state::SkillRun::new(Arc::new(skills::tests::skill_template(1)), level);
    EffectArgs {
        caster_caster: if caster.is_unit() {
            SkillCaster::Unit(caster.obj_id())
        } else {
            SkillCaster::Doodad(caster.obj_id())
        },
        caster: Some(caster),
        target_caster: skills::types::SkillCastTarget::Unit(target.obj_id()),
        target: Target::Obj(target),
        cast: CastAction::Skill {
            skill_id: 1,
            tl_id: 0,
        },
        source: EffectSource::from_skill(skill),
        skill_object: &skills::types::SkillObject::None,
    }
}

fn packet(client: &mut TestClient, caster: u32, target: u32) -> (i32, u16, Vec<u8>) {
    let frames: Vec<_> = client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == sc::SCUnitDamagedPacket)
        .collect();
    assert_eq!(frames.len(), 1);
    let body = frames.into_iter().next().unwrap().1;
    // Read in native 1.2 sub_397C8760 order; expected data is not encoded locally.
    let mut s = PacketStream::from_bytes(body.clone());
    assert_eq!(s.read_u8().unwrap(), 0);
    assert_eq!(s.read_u32().unwrap(), 1);
    assert_eq!(s.read_u16().unwrap(), 0);
    assert_eq!(s.read_u8().unwrap(), if caster == 0x300 { 4 } else { 0 });
    assert_eq!(s.read_bc().unwrap(), caster);
    assert_eq!(s.read_bc().unwrap(), caster);
    assert_eq!(s.read_bc().unwrap(), target);
    assert_eq!(s.read_u8().unwrap(), 0);
    let damage = s.read_pisc_w(3).unwrap();
    assert_eq!(damage[2], 0);
    assert_eq!(s.read_pisc_w(3).unwrap(), [0, 0, 0]);
    assert_eq!(s.read_u8().unwrap(), 0);
    let dice = s.read_u16().unwrap();
    assert_eq!(s.read_u8().unwrap(), 1);
    assert_eq!(s.read_u8().unwrap(), 1);
    assert_eq!(s.remaining(), 0);
    (damage[0] as i32, dice, body)
}

fn bonus(obj: &WorldObject, attribute: UnitAttribute, value: i32) {
    obj.with_unit(|u| {
        u.unit_mut().bonuses.add_bonus(
            attribute as u32,
            Bonus {
                template: BonusTemplate {
                    attribute,
                    modifier_type: UnitModifierType::Value,
                    value,
                    linear_level_bonus: 0,
                },
                value,
            },
        )
    });
}

#[test]
fn damage_pipeline_native_level_endpoints_interpolate_at_every_rank() {
    // Actual 1.2 damage effect 1007: level_md .5, VaStart 10, VaEnd 1.
    // sub_398A7380: +0.5, then truncation into each integer output.
    for (level, expected) in [(1, (45, 55)), (25, (47, 53)), (50, (50, 51))] {
        let (lo, hi) = level_damage_range(100.0, 0.5, level, 10, 1);
        assert_eq!((lo as i32, hi as i32), expected);
    }
    for (start, end) in [(10, 1), (1, 15), (10, 15)] {
        let ranges: Vec<_> = (1..=50)
            .map(|level| level_damage_range(100.0, 0.5, level, start, end))
            .collect();
        for pair in ranges.windows(2) {
            if start > end {
                assert!(pair[1].0 >= pair[0].0 && pair[1].1 <= pair[0].1);
            } else {
                assert!(pair[1].0 <= pair[0].0 && pair[1].1 >= pair[0].1);
            }
        }
    }
}

#[tokio::test]
async fn damage_pipeline_level_ranges_reach_hp_packets_and_threat() {
    for (level, range) in [(1, 45..=55), (25, 47..=53), (50, 50..=51)] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(fixture_data());
        let mut effect = skills::tests::damage_effect(1007, 0);
        effect.use_fixed_damage = false;
        effect.use_level_damage = true;
        effect.level_md = 0.5;
        effect.level_va_start = 10;
        effect.level_va_end = 1;
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), level),
            None,
        );
        let (dealt, dice, _) = packet(&mut client, player.obj_id, npc.obj_id);
        assert!(range.contains(&dealt));
        assert_eq!(dice & 0x1f, SkillHitType::MeleeHit as u16);
        let n = npc.data();
        assert_eq!(n.unit.hp, 1000 - dealt);
        assert_eq!(
            n.unit.skill_state.aggro_table[&player.obj_id].damage_aggro,
            dealt
        );
    }
}

#[tokio::test]
async fn damage_pipeline_authored_percent_and_current_health_bases() {
    // Actual compact flags/percentages: 4754 = 99% current, 5027 = 1% maximum,
    // 6047 = 50% maximum, 6198 enables fixed + level + percent together.
    // Weapon and level terms are reduced to deterministic values in this fixture.
    for (id, current, ratio, fixed, hp, maximum, expected) in [
        (4754, true, 99, false, 501, 1000, 495),
        (4754, true, 99, false, 901, 1000, 891),
        (5027, false, 1, false, 501, 1001, 10),
        (6047, false, 50, false, 901, 1001, 500),
        (6198, true, 10, true, 501, 1000, 150),
    ] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(fixture_data());
        let mut effect = skills::tests::damage_effect(id, if fixed { 100 } else { 0 });
        effect.use_fixed_damage = fixed;
        effect.use_percent_damage = true;
        effect.percent_min = ratio;
        effect.percent_max = ratio;
        effect.use_current_health = current;
        if id == 6198 {
            effect.use_level_damage = true;
            effect.level_md = 1.0;
        }
        {
            let mut n = npc.data();
            n.unit.hp = hp;
            n.unit.max_hp = maximum;
        }
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
        );
        let expected = expected + if id == 6198 { 100 } else { 0 };
        assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, expected);
        let n = npc.data();
        assert_eq!(n.unit.hp, hp - expected);
        assert_eq!(
            n.unit.skill_state.aggro_table[&player.obj_id].damage_aggro,
            expected
        );
    }
    // use_current_health alone does not enable percentage damage (actual row 4957).
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    let mut effect = skills::tests::damage_effect(4957, 100);
    effect.use_current_health = true;
    effect.percent_min = 90;
    effect.percent_max = 90;
    client.drain();
    damage(
        &server,
        &effect,
        &args((&player).into(), (&npc).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, 100);
}

#[tokio::test]
async fn damage_pipeline_scaled_threat_preserves_damage_and_kill_credit() {
    for multiplier in [0.0, 1.0, 3.0, 100.0] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(fixture_data());
        let mut effect = skills::tests::damage_effect(2180, 100);
        effect.aggro_multiplier = multiplier;
        let target = WorldObject::from(&npc);
        // Another player tagged first. Inflated threat must not steal its >50% credit.
        npc.data().unit.skill_state.tagging.tagger = 0x999;
        bonus(&WorldObject::from(&player), UnitAttribute::AggroMul, 50);
        bonus(&target, UnitAttribute::IncomingAggroMul, 100);
        client.drain();
        damage(&server, &effect, &args((&player).into(), target, 1), None);
        let (dealt, _, body) = packet(&mut client, player.obj_id, npc.obj_id);
        assert_eq!(dealt, 100);
        // Literal native 1.2 wire fixture (cast skill 1, unit 0x100 -> 0x200,
        // damage 100, absorbed/mana zero, melee hit). sub_397C8760, not a round trip.
        assert_eq!(
            body,
            [
                0, 1, 0, 0, 0, 0, 0, // cast
                0, 0, 1, 0, // unit caster
                0, 1, 0, 0, 2, 0, 0, // source, target, crime
                0, 100, 0, 0, // damage/absorbed/unused PISC
                0, 0, 0, 0, // mana PISC
                0, 0x21, 1, 1, 1, // holdable, dice, flags
            ]
        );
        let n = npc.data();
        assert_eq!(n.unit.hp, 900);
        assert_eq!(n.unit.skill_state.tagging.taggers, [(player.obj_id, 100)]);
        assert_eq!(n.unit.skill_state.tagging.tagger, 0x999);
        if multiplier == 0.0 {
            assert!(n.unit.skill_state.aggro_table.is_empty());
            assert!(!server
                .systems
                .skills
                .aggro_watch
                .lock()
                .unwrap()
                .contains_key(&player.obj_id));
        } else {
            assert_eq!(
                n.unit.skill_state.aggro_table[&player.obj_id].damage_aggro,
                (100.0 * multiplier * 1.5 * 2.0) as i32
            );
        }
    }
}

struct Armored;
impl skills::ext::NpcSkillHooks for Armored {
    fn npc_attribute(&self, _: &GameServer, _: &Npc, attr: StatKind) -> Option<f64> {
        (attr == StatKind::Armor).then_some(5300.0)
    }
}

#[tokio::test]
async fn damage_pipeline_minimum_and_late_aoe_apply_once_after_armor() {
    for (amount, aoe, expected) in [
        (7, None, 3),
        (7, Some(0.9), 2),
        (1, None, 1),
        (1, Some(0.1), 1),
        (0, None, 1),
        (-100, None, 1),
    ] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(fixture_data());
        server.systems.skills.ext.npcs.install(Arc::new(Armored));
        let effect = skills::tests::damage_effect(1, amount);
        client.drain();
        damage_with_aoe(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
            aoe,
        );
        assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, expected);
        assert_eq!(npc.data().unit.hp, 1000 - expected);
    }
}

#[tokio::test]
async fn damage_pipeline_percent_descriptor_multiplier_and_aoe_each_apply_once() {
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    let mut effect = skills::tests::damage_effect(5839, 0);
    effect.use_fixed_damage = false;
    effect.use_percent_damage = true;
    effect.percent_min = 70;
    effect.percent_max = 70;
    effect.multiplier = 0.5;
    client.drain();
    damage_with_aoe(
        &server,
        &effect,
        &args((&player).into(), (&npc).into(), 1),
        None,
        Some(0.9),
    );
    // 1000 max HP * 70% * descriptor .5 -> integer 350 * AoE .9 -> 315.
    assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, 315);
    assert_eq!(npc.data().unit.hp, 685);
    assert_eq!(
        npc.data().unit.skill_state.aggro_table[&player.obj_id].damage_aggro,
        315
    );
}

#[tokio::test]
async fn damage_pipeline_misses_and_immunity_stay_zero_with_or_without_aoe() {
    for aoe in [None, Some(0.1)] {
        for immune in [false, true] {
            let (server, mut client, player, npc) =
                skills::cast_validation_tests::fixture(fixture_data());
            let a = args((&player).into(), (&npc).into(), 1);
            if immune {
                buffs::apply_template(
                    &server,
                    server.game_data.systems.skills.buff(900).unwrap(),
                    a.caster.as_ref(),
                    &a.caster_caster,
                    &WorldObject::from(&npc),
                    None,
                );
            } else {
                a.source
                    .skill
                    .as_ref()
                    .unwrap()
                    .state()
                    .hit_types
                    .insert(npc.obj_id, SkillHitType::MeleeMiss);
            }
            client.drain();
            damage_with_aoe(
                &server,
                &skills::tests::damage_effect(1, 100),
                &a,
                None,
                aoe,
            );
            let (dealt, dice, _) = packet(&mut client, player.obj_id, npc.obj_id);
            assert_eq!(dealt, 0);
            assert_eq!(
                dice & 0x1f,
                if immune {
                    SkillHitType::Immune
                } else {
                    SkillHitType::MeleeMiss
                } as u16
            );
            let n = npc.data();
            assert_eq!(n.unit.hp, 1000);
            assert!(n.unit.skill_state.aggro_table.is_empty());
            assert!(n.unit.skill_state.tagging.taggers.is_empty());
        }
    }
}

#[tokio::test]
async fn damage_pipeline_nonunit_sources_and_hostile_target_authority() {
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    let mut d = Doodad::new(1);
    d.transform.set_xyz(100.0, 100.0, 100.0);
    let doodad = Entity::new(0x300, d);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&doodad, &server.view_ctx());
    let effect = skills::tests::damage_effect(1, 100);
    client.drain();
    damage(
        &server,
        &effect,
        &args((&doodad).into(), (&npc).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, doodad.obj_id, npc.obj_id).0, 100);
    assert_eq!(npc.data().unit.hp, 900);
    assert!(npc.data().unit.skill_state.tagging.taggers.is_empty());
    let mut percent = skills::tests::damage_effect(861, 0);
    percent.use_fixed_damage = false;
    percent.use_percent_damage = true;
    percent.percent_min = 35;
    percent.percent_max = 35;
    damage(
        &server,
        &percent,
        &args((&doodad).into(), (&npc).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, doodad.obj_id, npc.obj_id).0, 350);
    assert_eq!(npc.data().unit.hp, 550);
    player.data().unit.faction_id = 4;
    npc.data().unit.faction_id = 4;
    client.drain();
    damage(
        &server,
        &effect,
        &args((&player).into(), (&npc).into(), 1),
        None,
    );
    assert!(client
        .drain()
        .into_iter()
        .all(|(op, _)| op != sc::SCUnitDamagedPacket));
    assert_eq!(npc.data().unit.hp, 550);
    npc.data().unit.faction_id = 3;
    damage(
        &server,
        &effect,
        &args((&player).into(), (&npc).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, 100);
    assert_eq!(npc.data().unit.hp, 450);
    damage(
        &server,
        &effect,
        &args((&player).into(), (&doodad).into(), 1),
        None,
    );
    assert!(client
        .drain()
        .into_iter()
        .all(|(op, _)| op != sc::SCUnitDamagedPacket));
}

#[tokio::test]
async fn damage_pipeline_npc_percent_attacks_select_pre_hit_player_hp() {
    for (current, expected) in [(true, 495), (false, 990)] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(fixture_data());
        {
            let mut p = player.data();
            p.unit.hp = 1001;
            p.unit.max_hp = 2001;
        }
        let mut effect = skills::tests::damage_effect(4754, 0);
        effect.use_fixed_damage = false;
        effect.use_percent_damage = true;
        effect.use_current_health = current;
        effect.percent_min = 99;
        effect.percent_max = 99;
        // 5300 armor would halve melee damage; keep this sample at 0 armor, and
        // use 50% incoming damage to observe both bases without killing the player.
        bonus(
            &WorldObject::from(&player),
            UnitAttribute::IncomingMeleeDamageMul,
            -500,
        );
        client.drain();
        damage(
            &server,
            &effect,
            &args((&npc).into(), (&player).into(), 1),
            None,
        );
        assert_eq!(packet(&mut client, npc.obj_id, player.obj_id).0, expected);
        assert_eq!(player.data().unit.hp, 1001 - expected);
    }
}

#[tokio::test]
async fn damage_pipeline_new_threat_saturates_and_legacy_zero_callers_are_unchanged() {
    let (server, _client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    let target = WorldObject::from(&npc);
    let caster = WorldObject::from(&player);
    unit_combat::add_unit_aggro(&server, &target, AggroKind::Damage, &caster, 0);
    assert!(npc
        .data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&player.obj_id));
    unit_combat::add_damage_aggro(&server, &target, &caster, 10, i32::MAX);
    unit_combat::add_damage_aggro(&server, &target, &caster, 10, i32::MAX);
    let n = npc.data();
    assert_eq!(
        n.unit.skill_state.aggro_table[&player.obj_id].damage_aggro,
        i32::MAX
    );
    assert_eq!(n.unit.skill_state.tagging.taggers, [(player.obj_id, 20)]);
}

#[tokio::test]
async fn damage_pipeline_extreme_and_negative_inputs_never_heal_or_wrap() {
    for amount in [i32::MIN, i32::MAX] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(fixture_data());
        // Health theft clamps to the cached pool, populated during normal stat refresh.
        skills::stats::refresh_cached_stats(&server, &WorldObject::from(&player));
        player.data().unit.summarize_damage = i32::MAX;
        player.data().unit.hp = 50;
        let mut effect = skills::tests::damage_effect(1, amount);
        effect.health_steal_ratio = i32::MAX;
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
        );
        let dealt = packet(&mut client, player.obj_id, npc.obj_id).0;
        assert_eq!(dealt, amount.max(1));
        assert_eq!(npc.data().unit.hp, (1000_i32).saturating_sub(dealt).max(0));
        assert_eq!(player.data().unit.summarize_damage, i32::MAX);
        assert!(player.data().unit.hp >= 50);
    }
}

#[tokio::test]
async fn damage_pipeline_percent_uses_wide_arithmetic_and_never_emits_negative_damage() {
    for ratio in [i32::MIN, i32::MAX] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(fixture_data());
        npc.data().unit.max_hp = i32::MAX;
        let mut effect = skills::tests::damage_effect(1, 0);
        effect.use_fixed_damage = false;
        effect.use_percent_damage = true;
        effect.percent_min = ratio;
        effect.percent_max = ratio;
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
        );
        let expected = if ratio < 0 { 1 } else { i32::MAX };
        assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, expected);
        assert_eq!(npc.data().unit.hp, 1000_i32.saturating_sub(expected).max(0));
    }
}
