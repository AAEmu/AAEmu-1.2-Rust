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
use crate::server::GameServer;
use crate::systems::skills::buffs;
use crate::systems::skills::data::effects::{DamageEffect, HealEffect};
use crate::test_support::{spawn_player, test_character, TestClient};
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

fn threshold_data() -> GameData {
    let mut data = fixture_data();
    data.systems.skills.add_buff(skills::data::BuffTemplate {
        id: 275,
        kind: 1,
        stack_rule: 1,
        melee_immune: true,
        immune_damage: 50,
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
    packet_with_skill(client, caster, target, 1)
}

fn packet_with_skill(
    client: &mut TestClient,
    caster: u32,
    target: u32,
    cast_skill_id: u32,
) -> (i32, u16, Vec<u8>) {
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
    assert_eq!(s.read_u32().unwrap(), cast_skill_id);
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
async fn damage_pipeline_immune_damage_threshold_ignores_only_hits_up_to_the_limit() {
    // Physical Resist Potion (275, 50) data shape: melee flag plus a positive
    // `buffs.immune_damage` threshold. Hits below and at the threshold are ignored;
    // a larger melee hit and a non-melee hit still land.
    for (amount, damage_type, immune) in
        [(40, 1, true), (50, 1, true), (60, 1, false), (40, 2, false)]
    {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(threshold_data());
        let target = WorldObject::from(&npc);
        let a = args((&player).into(), target.clone(), 1);
        buffs::apply_template(
            &server,
            server.game_data.systems.skills.buff(275).unwrap(),
            a.caster.as_ref(),
            &a.caster_caster,
            &target,
            None,
        );
        let mut effect = skills::tests::damage_effect(1, amount);
        effect.damage_type = damage_type;
        client.drain();
        damage_with_aoe(&server, &effect, &a, None, None);
        let (dealt, dice, _) = packet(&mut client, player.obj_id, npc.obj_id);
        if immune {
            assert_eq!(dealt, 0, "hit {amount} type {damage_type} must be ignored");
            assert_eq!(dice & 0x1f, SkillHitType::Immune as u16);
        } else {
            assert_eq!(dealt, amount, "hit {amount} type {damage_type} must land");
        }
        assert_eq!(npc.data().unit.hp, 1000 - dealt);
    }
}

#[tokio::test]
async fn damage_pipeline_immune_damage_zero_flag_still_ignores_every_hit() {
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    let target = WorldObject::from(&npc);
    let a = args((&player).into(), target.clone(), 1);
    buffs::apply_template(
        &server,
        server.game_data.systems.skills.buff(900).unwrap(),
        a.caster.as_ref(),
        &a.caster_caster,
        &target,
        None,
    );
    client.drain();
    damage_with_aoe(
        &server,
        &skills::tests::damage_effect(1, 1_000_000),
        &a,
        None,
        None,
    );
    let (dealt, dice, _) = packet(&mut client, player.obj_id, npc.obj_id);
    assert_eq!(dealt, 0);
    assert_eq!(dice & 0x1f, SkillHitType::Immune as u16);
    assert_eq!(npc.data().unit.hp, 1000);
}

/// The real 1.2 rows behind the threshold rule: the six flagged buffs carry a positive
/// `immune_damage`, and buff 131 (Invincible) is the zero-threshold full immunity.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn real_compact_db_separates_threshold_and_full_damage_immunity() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let buffs = skills::data::templates::load_buffs(&conn).unwrap();
    let get = |id: u32| buffs.iter().find(|b| b.id == id).unwrap();
    for (id, melee, spell, ranged, siege, threshold) in [
        (176, false, true, false, false, 137),
        (275, true, false, false, false, 50),
        (276, false, true, false, false, 100),
        (2071, true, true, true, true, 2000),
        (3615, true, true, false, false, 1_000_000),
        (4609, true, true, true, true, 9_999_999),
    ] {
        let b = get(id);
        assert_eq!(
            (
                b.melee_immune,
                b.spell_immune,
                b.ranged_immune,
                b.siege_immune,
                b.immune_damage
            ),
            (melee, spell, ranged, siege, threshold),
            "buff {id}"
        );
    }
    let invincible = get(131);
    assert_eq!(invincible.immune_damage, 0);
    assert!(
        invincible.melee_immune
            && invincible.spell_immune
            && invincible.ranged_immune
            && invincible.siege_immune
    );
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

/// The 2026-11-08 bot run's buff 399 ticks: a level heal (HealEffect 61, `level_md` 2) cast by an
/// NPC whose native `LevelDps` is -8 (ability level 0) computed a negative value and sent an
/// `SCUnitHealedPacket` of -1, removing one HP per tick. A heal without positive health must
/// send nothing and change no HP; a positive heal still takes the normal path.
#[tokio::test]
async fn negative_level_heal_sends_no_packet_and_changes_no_hp() {
    let negative_dps = |mut data: GameData| {
        data.formulas = crate::systems::vehicles::stats::tests::formulas(
            &[(1, "100000", 14, 0), (2, "1000", 15, 0), (3, "-8", 12, 0)],
            &[],
        );
        data
    };
    let effect = crate::systems::skills::data::effects::HealEffect {
        id: 61,
        use_fixed_heal: false,
        fixed_min: 0,
        fixed_max: 0,
        use_level_heal: true,
        level_md: 2.0,
        level_va_start: 0,
        level_va_end: 0,
        percent: false,
        use_charged_buff: false,
        charged_buff_id: 0,
        charged_mul: 0.0,
        slave_applicable: false,
        ignore_heal_aggro: false,
        dps_multiplier: 1.0,
        actability_group_id: 0,
        actability_step: 0,
        actability_mul: 0.0,
        actability_add: 0.0,
    };

    let (server, mut client, player, npc) =
        skills::cast_validation_tests::fixture(negative_dps(fixture_data()));
    let a = args((&player).into(), (&npc).into(), 1);
    let hp_before = npc.data().unit.hp;
    client.drain();
    super::heal(&server, &effect, &a, None);
    assert_eq!(healed_values(&mut client), Vec::<i32>::new());
    assert_eq!(npc.data().unit.hp, hp_before);

    // Positive `LevelDps` (the character formula, 100 in `fixture_data`) still heals.
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    let a = args((&player).into(), (&npc).into(), 1);
    npc.data().unit.hp = 500; // room below MaxHp for the heal
    let hp_before = npc.data().unit.hp;
    client.drain();
    super::heal(&server, &effect, &a, None);
    let values = healed_values(&mut client);
    assert_eq!(values.len(), 1, "{values:?}");
    assert!(values[0] > 0, "{values:?}");
    assert!(npc.data().unit.hp > hp_before);
}

/// The 2026-11-08 bot run's `RestoreManaEffect 68: min > max` errors: NPC 21787 casting skill
/// 18501 has the native `LevelDps` -8, and effect 68 (`level_md` 3, `level_va_start/end` 10)
/// makes the "max" more negative than the "min". Not swapped columns; the caster has no positive
/// range, so the effect is a no-op logged at debug. A player's positive range still restores.
#[tokio::test]
async fn negative_level_restore_mana_is_a_no_op_without_an_error() {
    let effect = crate::systems::skills::data::effects::RestoreManaEffect {
        id: 68,
        use_fixed_value: false,
        fixed_min: 0,
        fixed_max: 0,
        use_level_value: true,
        level_md: 3.0,
        level_va_start: 10,
        level_va_end: 10,
        percent: false,
    };

    let mut data = fixture_data();
    data.formulas = crate::systems::vehicles::stats::tests::formulas(
        &[(1, "100000", 14, 0), (2, "1000", 15, 0), (3, "-8", 12, 0)],
        &[],
    );
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
    let a = args((&player).into(), (&npc).into(), 1);
    client.drain();
    let (_, events) =
        crate::test_support::capture_events(|| super::restore_mana(&server, &effect, &a));
    assert_eq!(healed_values(&mut client), Vec::<i32>::new());
    assert!(
        events
            .iter()
            .all(|(level, message)| *level != tracing::Level::ERROR
                || !message.contains("RestoreManaEffect")),
        "{events:?}"
    );

    // Positive `LevelDps` restores mana normally.
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    let a = args((&player).into(), (&npc).into(), 1);
    client.drain();
    super::restore_mana(&server, &effect, &a);
    let values = healed_values(&mut client);
    assert_eq!(values.len(), 1, "{values:?}");
    assert!(values[0] > 0, "{values:?}");
}

/// The `value` of every `SCUnitHealedPacket` the client received (health or mana).
fn healed_values(client: &mut TestClient) -> Vec<i32> {
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == sc::SCUnitHealedPacket)
        .map(|(_, body)| {
            // CastAction::Skill (0 + u32 id + u16 tl), unit caster (0 + BC), target BC,
            // heal type, heal hit type, then the i32 value.
            i32::from_le_bytes(body[16..20].try_into().expect("heal value"))
        })
        .collect()
}

/// An authored inverted range is a data error even when the computed maximum is <= 0, so the
/// `min > max` error must not be swallowed by the negative-LevelDps no-op (fixed_min 100,
/// fixed_max -200: `max <= 0 < min`, and the inversion is in the columns).
#[tokio::test]
async fn authored_inverted_restore_range_still_reports_the_error() {
    let effect = crate::systems::skills::data::effects::RestoreManaEffect {
        id: 69,
        use_fixed_value: true,
        fixed_min: 100,
        fixed_max: -200,
        use_level_value: false,
        level_md: 1.0,
        level_va_start: 0,
        level_va_end: 0,
        percent: false,
    };
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    let a = args((&player).into(), (&npc).into(), 1);
    client.drain();
    let (_, events) =
        crate::test_support::capture_events(|| super::restore_mana(&server, &effect, &a));
    assert_eq!(healed_values(&mut client), Vec::<i32>::new());
    assert!(
        events
            .iter()
            .any(|(level, message)| *level == tracing::Level::ERROR
                && message.contains("RestoreManaEffect")),
        "{events:?}"
    );
}

/// The guards must work through the real NPC stats path, which produced the 2026-11-08 bug: an
/// NPC caster's `LevelDps` comes from `npc_stats` (formula 29 evaluated with `ab_level = 0`), not
/// from the player formula the other tests use.
#[tokio::test]
async fn npc_caster_with_negative_level_dps_gets_no_heal_and_no_restore() {
    use crate::systems::npcs::npc::data::NpcTemplate;

    let mut data = fixture_data();
    data.formulas = crate::systems::vehicles::stats::tests::formulas(
        &[
            (1, "100000", 14, 0),
            (2, "1000", 15, 0),
            (3, "100", 12, 0),
            (
                29,
                "( floor( ( ab_level + 2 ) / 3 ) * 24 - 8 ) * ( npc_template * npc_kind * npc_grade )",
                12,
                1,
            ),
        ],
        &[(29, 2, 1, 1.0), (29, 3, 1, 1.0), (29, 4, 1, 1.0)],
    );
    data.systems.npcs.npc.templates.insert(
        1,
        NpcTemplate {
            id: 1,
            npc_template_id: 1,
            npc_kind_id: 1,
            npc_grade_id: 1,
            ..Default::default()
        },
    );
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
    crate::systems::npcs::install_hooks(&server);
    let npc_obj = WorldObject::from(&npc);
    assert_eq!(
        crate::systems::skills::stats::stat(
            &server,
            &npc_obj,
            crate::systems::skills::stats::StatKind::LevelDps
        ),
        -8.0
    );

    let a = args(npc_obj, (&player).into(), 1);
    let heal = crate::systems::skills::data::effects::HealEffect {
        id: 61,
        use_fixed_heal: false,
        fixed_min: 0,
        fixed_max: 0,
        use_level_heal: true,
        level_md: 2.0,
        level_va_start: 0,
        level_va_end: 0,
        percent: false,
        use_charged_buff: false,
        charged_buff_id: 0,
        charged_mul: 0.0,
        slave_applicable: false,
        ignore_heal_aggro: false,
        dps_multiplier: 1.0,
        actability_group_id: 0,
        actability_step: 0,
        actability_mul: 0.0,
        actability_add: 0.0,
    };
    let player_hp = player.data().unit.hp;
    client.drain();
    super::heal(&server, &heal, &a, None);
    assert_eq!(healed_values(&mut client), Vec::<i32>::new());
    assert_eq!(player.data().unit.hp, player_hp);

    let restore = crate::systems::skills::data::effects::RestoreManaEffect {
        id: 68,
        use_fixed_value: false,
        fixed_min: 0,
        fixed_max: 0,
        use_level_value: true,
        level_md: 3.0,
        level_va_start: 10,
        level_va_end: 10,
        percent: false,
    };
    let (_, events) =
        crate::test_support::capture_events(|| super::restore_mana(&server, &restore, &a));
    assert_eq!(healed_values(&mut client), Vec::<i32>::new());
    assert!(
        events
            .iter()
            .all(|(level, message)| *level != tracing::Level::ERROR
                || !message.contains("RestoreManaEffect")),
        "{events:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Fixed-damage DoT ticks.

const DOT_BUFF: u32 = 9_100;
const DOT_EFFECT: u32 = 9_101;
const HEAL_DOT_BUFF: u32 = 9_102;
const HEAL_DOT_EFFECT: u32 = 9_103;

fn dot_data(effect: DamageEffect, duration: i32, tick: f64) -> GameData {
    let mut data = fixture_data();
    let skills = &mut data.systems.skills;
    skills.add_effect(
        skills::data::EffectKind::DamageEffect,
        skills::data::EffectTemplate::Damage(Box::new(effect)),
        DOT_EFFECT,
    );
    skills.add_buff(skills::data::BuffTemplate {
        id: DOT_BUFF,
        duration,
        tick,
        tick_effects: vec![skills::data::TickEffect {
            effect_id: DOT_EFFECT,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
        }],
        ..Default::default()
    });
    data
}

fn fixed_dot_data(total: i32) -> GameData {
    dot_data(
        skills::tests::damage_effect(DOT_EFFECT, total),
        11_000,
        1_000.0,
    )
}

fn percent_dot_data() -> GameData {
    let mut effect = skills::tests::damage_effect(DOT_EFFECT, 0);
    effect.use_fixed_damage = false;
    effect.use_percent_damage = true;
    effect.percent_min = 10;
    effect.percent_max = 10;
    dot_data(effect, 10_000, 1_000.0)
}

fn fixed_heal_dot_data(total: i32) -> GameData {
    let mut data = fixture_data();
    let skills = &mut data.systems.skills;
    skills.add_effect(
        skills::data::EffectKind::HealEffect,
        skills::data::EffectTemplate::Heal(HealEffect {
            id: HEAL_DOT_EFFECT,
            use_fixed_heal: true,
            fixed_min: total,
            fixed_max: total,
            use_level_heal: false,
            level_md: 0.0,
            level_va_start: 0,
            level_va_end: 0,
            percent: false,
            use_charged_buff: false,
            charged_buff_id: 0,
            charged_mul: 0.0,
            slave_applicable: false,
            ignore_heal_aggro: false,
            dps_multiplier: 0.0,
            actability_group_id: 0,
            actability_step: 0,
            actability_mul: 0.0,
            actability_add: 0.0,
        }),
        HEAL_DOT_EFFECT,
    );
    skills.add_buff(skills::data::BuffTemplate {
        id: HEAL_DOT_BUFF,
        duration: 11_000,
        tick: 1_000.0,
        tick_effects: vec![skills::data::TickEffect {
            effect_id: HEAL_DOT_EFFECT,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
        }],
        ..Default::default()
    });
    data
}

/// Drive the real buff path: `apply_template` schedules the CREATED buff, and each
/// `schedule_effect` reaches `time_to_time_apply` -> the effect. Returns the buff index.
fn apply_tick_buff(
    server: &GameServer,
    caster: &WorldObject,
    target: &WorldObject,
    buff_id: u32,
) -> u32 {
    apply_tick_buff_from_skill(server, caster, target, buff_id, None)
}

/// [`apply_tick_buff`] with the applying skill recorded on the live buff (the real cast path).
fn apply_tick_buff_from_skill(
    server: &GameServer,
    caster: &WorldObject,
    target: &WorldObject,
    buff_id: u32,
    skill_id: Option<u32>,
) -> u32 {
    let skill = skill_id
        .map(|id| skills::state::SkillRun::new(Arc::new(skills::tests::skill_template(id)), 1));
    buffs::apply_template(
        server,
        server.game_data.systems.skills.buff(buff_id).unwrap(),
        Some(caster),
        &SkillCaster::Unit(caster.obj_id()),
        target,
        skill.as_deref(),
    );
    target
        .with_base_unit(|b| {
            b.buffs()
                .effects
                .iter()
                .find(|e| e.template.id == buff_id && !e.is_ended())
                .map(|e| e.index)
        })
        .expect("buff applied")
}

fn tick_damage(server: &GameServer, target: &WorldObject, index: u32) -> i32 {
    let before = target.with_unit(|u| u.unit().hp).unwrap();
    buffs::schedule_effect(server, target, index, false);
    before - target.with_unit(|u| u.unit().hp).unwrap()
}

fn tick_heal(server: &GameServer, target: &WorldObject, index: u32) -> i32 {
    let before = target.with_unit(|u| u.unit().hp).unwrap();
    buffs::schedule_effect(server, target, index, false);
    target.with_unit(|u| u.unit().hp).unwrap() - before
}

/// 1.2 DamageEffect `fixed_min`/`fixed_max` are the **total** over the buff duration
/// (client macro sub_398CFD40/sub_398CFFC0: `total_min_damage` is the fixed term,
/// `min_damage` is that term / (duration / tick)). Buff 3910 ("Metal Poisoning",
/// 11000 ms, 1000 ms tick, DamageEffect 3427 1800-2200) must therefore tick
/// 1800-2200 x 1000/11000 = 164-200, not the whole range per tick.
#[tokio::test]
async fn damage_pipeline_fixed_dot_spreads_its_total_over_the_ticks() {
    let (server, mut client, player, npc) =
        skills::cast_validation_tests::fixture(fixed_dot_data(1_100));
    {
        let mut n = npc.data();
        n.unit.hp = 5_000;
        n.unit.max_hp = 5_000;
    }
    let caster = WorldObject::from(&player);
    let target = WorldObject::from(&npc);
    let index = apply_tick_buff(&server, &caster, &target, DOT_BUFF);
    client.drain();
    let ticks: Vec<i32> = (0..11)
        .map(|_| tick_damage(&server, &target, index))
        .collect();
    assert_eq!(
        ticks,
        vec![100; 11],
        "a fixed DoT tick is the total spread over the duration, not the full total"
    );
    assert_eq!(npc.data().unit.hp, 5_000 - 1_100);
}

/// The same tick scaling that the current non-fixed path applies must stay in place.
#[tokio::test]
async fn damage_pipeline_percent_dot_tick_keeps_the_existing_scale() {
    let (server, mut client, player, npc) =
        skills::cast_validation_tests::fixture(percent_dot_data());
    let caster = WorldObject::from(&player);
    let target = WorldObject::from(&npc);
    let index = apply_tick_buff(&server, &caster, &target, DOT_BUFF);
    client.drain();
    let ticks: Vec<i32> = (0..10)
        .map(|_| tick_damage(&server, &target, index))
        .collect();
    assert_eq!(ticks, vec![10; 10]);
    assert_eq!(npc.data().unit.hp, 1_000 - 100);
}

/// The heal path applies the buff tick modifier after the fixed heal override
/// (`HealEffect.Apply`); a fixed-damage DoT tick must produce the same share of its total.
#[tokio::test]
async fn damage_pipeline_fixed_dot_tick_matches_the_fixed_heal_tick() {
    let (server, mut client, player, npc) =
        skills::cast_validation_tests::fixture(fixed_heal_dot_data(1_100));
    {
        let mut n = npc.data();
        n.unit.hp = 3_000;
        n.unit.max_hp = 5_000;
    }
    let caster = WorldObject::from(&player);
    let target = WorldObject::from(&npc);
    let index = apply_tick_buff(&server, &caster, &target, HEAL_DOT_BUFF);
    client.drain();
    let ticks: Vec<i32> = (0..11)
        .map(|_| tick_heal(&server, &target, index))
        .collect();
    assert_eq!(ticks, vec![100; 11]);
    assert_eq!(npc.data().unit.hp, 3_000 + 1_100);
}

/// A buff trigger is a one-shot event, not one tick of the buff: its standalone fixed damage
/// keeps the authored value even when the source buff carries tick effects (the previous
/// fixed-override order had the same result). Only periodic buff ticks spread the total.
#[tokio::test]
async fn damage_pipeline_fixed_trigger_keeps_its_full_value() {
    let (server, mut client, player, npc) =
        skills::cast_validation_tests::fixture(fixed_dot_data(500));
    let caster = WorldObject::from(&player);
    let target = WorldObject::from(&npc);
    let buff = server
        .game_data
        .systems
        .skills
        .buff(DOT_BUFF)
        .unwrap()
        .clone();
    let mut source = EffectSource::from_buff(buff);
    source.is_trigger = true;
    let args = EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: SkillCaster::Unit(player.obj_id),
        target: Target::Obj(target.clone()),
        target_caster: skills::types::SkillCastTarget::Unit(npc.obj_id),
        cast: CastAction::Buff {
            buff_id: DOT_BUFF,
            owner_obj_id: npc.obj_id,
            index: 0,
        },
        source,
        skill_object: &skills::types::SkillObject::None,
    };
    let effect = skills::tests::damage_effect(DOT_EFFECT, 500);
    client.drain();
    damage(&server, &effect, &args, None);
    assert_eq!(npc.data().unit.hp, 1_000 - 500);
}

// ---------------------------------------------------------------------------------------------
// Player -> NPC level damage bonus (`formulas` row 20, research-data-rules.md §1).

const LEVEL_BONUS_ROW: u32 = 20;
const LEVEL_BONUS_FORMULA: &str =
    "if_negative(pc_level - npc_level, 1, 1 + (pc_level - npc_level) * 0.04)";

fn level_bonus_data(formula: &str) -> GameData {
    let mut data = fixture_data();
    data.formulas.insert_formula(LEVEL_BONUS_ROW, formula);
    data
}

/// A non-fixed, non-percent row with a deterministic 100 base: level damage (`LevelDps` is the
/// constant 100 in the fixture) with no variance, so the level bonus is the only variable.
fn level_damage_effect(id: u32) -> DamageEffect {
    let mut effect = skills::tests::damage_effect(id, 0);
    effect.use_fixed_damage = false;
    effect.use_level_damage = true;
    effect.level_md = 1.0;
    effect.level_va_start = 0;
    effect.level_va_end = 0;
    effect
}

/// A percent-of-maximum-HP row (10% of the fixture's 1000 HP = 100), which the level bonus
/// must leave untouched.
fn percent_damage_effect(percent: i32) -> DamageEffect {
    let mut effect = skills::tests::damage_effect(1, 0);
    effect.use_fixed_damage = false;
    effect.use_percent_damage = true;
    effect.percent_min = percent;
    effect.percent_max = percent;
    effect
}

fn level_dot_data() -> GameData {
    let mut data = level_bonus_data(LEVEL_BONUS_FORMULA);
    let mut effect = level_damage_effect(DOT_EFFECT);
    effect.damage_type = 2;
    let skills = &mut data.systems.skills;
    skills.add_effect(
        skills::data::EffectKind::DamageEffect,
        skills::data::EffectTemplate::Damage(Box::new(effect)),
        DOT_EFFECT,
    );
    skills.add_buff(skills::data::BuffTemplate {
        id: DOT_BUFF,
        duration: 1_000,
        tick: 1_000.0,
        tick_effects: vec![skills::data::TickEffect {
            effect_id: DOT_EFFECT,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
        }],
        ..Default::default()
    });
    data
}

/// Row 20 (`damage_bonus_multiplier_by_level`): +4% per level for a player at or above the
/// NPC's level, for every damage type; row 21 is the constant x1 below it (no penalty).
#[tokio::test]
async fn damage_pipeline_level_bonus_scales_player_hits_on_lower_level_npcs() {
    for (player_level, npc_level, damage_type, expected) in [
        (50, 40, 1, 140),
        (50, 40, 2, 140),
        (50, 40, 4, 140),
        (50, 40, 3, 140),
        (50, 50, 1, 100),
        (40, 50, 1, 100),
    ] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(level_bonus_data(LEVEL_BONUS_FORMULA));
        player.data().unit.level = player_level;
        npc.data().unit.level = npc_level;
        let mut effect = level_damage_effect(1);
        effect.damage_type = damage_type;
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
        );
        assert_eq!(
            packet(&mut client, player.obj_id, npc.obj_id).0,
            expected,
            "player {player_level} vs NPC {npc_level}, damage type {damage_type}"
        );
        assert_eq!(npc.data().unit.hp, 1000 - expected);
    }
}

/// The rate is the `formulas` row's text, not a hard-coded 0.04; without the row the hit is
/// unchanged (no hidden fallback).
#[tokio::test]
async fn damage_pipeline_level_bonus_rate_comes_from_the_formula_row() {
    for (formula, expected) in [
        (LEVEL_BONUS_FORMULA, 140),
        (
            "if_negative(pc_level - npc_level, 1, 1 + (pc_level - npc_level) * 0.10)",
            200,
        ),
    ] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(level_bonus_data(formula));
        player.data().unit.level = 50;
        npc.data().unit.level = 40;
        let effect = level_damage_effect(1);
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
        );
        assert_eq!(
            packet(&mut client, player.obj_id, npc.obj_id).0,
            expected,
            "formula {formula}"
        );
    }

    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    player.data().unit.level = 50;
    npc.data().unit.level = 40;
    let effect = skills::tests::damage_effect(1, 100);
    client.drain();
    damage(
        &server,
        &effect,
        &args((&player).into(), (&npc).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, 100);
}

/// The variables name a player character and an NPC: PvP keeps its own battle-resist path.
#[tokio::test]
async fn damage_pipeline_level_bonus_does_not_apply_to_pvp() {
    let (server, _client, player, _npc) =
        skills::cast_validation_tests::fixture(level_bonus_data(LEVEL_BONUS_FORMULA));
    player.data().unit.level = 50;
    let mut victim_client = TestClient::new(&server, 2, 2);
    let mut character = test_character(2, "Victim", 100.0, 105.0);
    character.unit.hp = 1000;
    character.unit.max_hp = 1000;
    let victim = spawn_player(&server, &victim_client, character, 0x102);
    victim.data().unit.level = 10;
    victim_client.drain();
    let effect = level_damage_effect(1);
    damage(
        &server,
        &effect,
        &args((&player).into(), (&victim).into(), 1),
        None,
    );
    assert_eq!(
        packet(&mut victim_client, player.obj_id, victim.obj_id).0,
        100
    );
    assert_eq!(victim.data().unit.hp, 900);
}

/// The variables name a player character and an NPC: an NPC caster (even a higher-level one)
/// and a player target are outside the row, so the multiplier itself is x1.
#[tokio::test]
async fn damage_pipeline_level_bonus_only_applies_to_a_player_caster_and_npc_target() {
    let (server, _client, player, npc) =
        skills::cast_validation_tests::fixture(level_bonus_data(LEVEL_BONUS_FORMULA));
    player.data().unit.level = 50;
    npc.data().unit.level = 40;
    // A second, level-10 NPC so an NPC caster can be higher than an NPC target.
    let mut other = Npc::new(1);
    other.unit.hp = 1_000;
    other.unit.max_hp = 1_000;
    other.unit.level = 10;
    other.unit.transform.set_xyz(100.0, 110.0, 100.0);
    let other = Entity::new(0x201, other);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&other, &server.view_ctx());
    let player = WorldObject::from(&player);
    let npc = WorldObject::from(&npc);
    let other = WorldObject::from(&other);
    assert_eq!(super::level_damage_multiplier(&server, &player, &npc), 1.4);
    assert_eq!(super::level_damage_multiplier(&server, &npc, &player), 1.0);
    assert_eq!(
        super::level_damage_multiplier(&server, &player, &player),
        1.0
    );
    // Higher-level NPC -> lower-level NPC must be x1 too (only a player caster qualifies).
    assert_eq!(super::level_damage_multiplier(&server, &npc, &other), 1.0);
}

/// Authored fixed values and percent-of-HP rows keep their exact value; the bonus scales only
/// the weapon / level damage base (row 4754 "99% of current HP" stays 99%).
#[tokio::test]
async fn damage_pipeline_level_bonus_keeps_fixed_and_percent_rows() {
    for (label, effect, expected) in [
        ("fixed", skills::tests::damage_effect(1, 100), 100),
        ("percent", percent_damage_effect(10), 100),
    ] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(level_bonus_data(LEVEL_BONUS_FORMULA));
        player.data().unit.level = 50;
        npc.data().unit.level = 40;
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
        );
        assert_eq!(
            packet(&mut client, player.obj_id, npc.obj_id).0,
            expected,
            "{label} row must keep its authored value"
        );
    }
}

/// Siege-weapon NPCs (`NpcKindType.SiegeWeapon`, kind 9: battlefield turrets / cannons and
/// their practice copies, compact levels 20-50) are destructible structures, not PvE mobs.
#[tokio::test]
async fn damage_pipeline_level_bonus_skips_siege_weapon_npcs() {
    let mut data = level_bonus_data(LEVEL_BONUS_FORMULA);
    data.systems.npcs.npc.templates.insert(
        1,
        crate::systems::npcs::npc::data::NpcTemplate {
            id: 1,
            npc_kind_id: 9,
            ..Default::default()
        },
    );
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
    player.data().unit.level = 50;
    npc.data().unit.level = 40;
    let effect = level_damage_effect(1);
    client.drain();
    damage(
        &server,
        &effect,
        &args((&player).into(), (&npc).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, 100);
}

/// Training dummies (`system_factions` 170 "scarecrow": 8 templates at levels 1-50) are practice
/// structures too; a level-1 dummy must not take x2.96 from a level-50 player.
#[tokio::test]
async fn damage_pipeline_level_bonus_skips_training_dummies() {
    let (server, mut client, player, npc) =
        skills::cast_validation_tests::fixture(level_bonus_data(LEVEL_BONUS_FORMULA));
    player.data().unit.level = 50;
    npc.data().unit.level = 1;
    npc.data().unit.faction_id = 170;
    let effect = level_damage_effect(1);
    client.drain();
    damage(
        &server,
        &effect,
        &args((&player).into(), (&npc).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, 100);
}

/// DoT ticks run through `damage_with_aoe` with the buff's caster, so a player's DoT on an NPC
/// gets the level bonus like a direct hit (the row is a damage multiplier).
#[tokio::test]
async fn damage_pipeline_level_bonus_applies_to_buff_tick_damage() {
    let (server, mut client, player, npc) =
        skills::cast_validation_tests::fixture(level_dot_data());
    player.data().unit.level = 50;
    npc.data().unit.level = 40;
    let caster = WorldObject::from(&player);
    let target = WorldObject::from(&npc);
    let index = apply_tick_buff(&server, &caster, &target, DOT_BUFF);
    client.drain();
    assert_eq!(tick_damage(&server, &target, index), 140);
}

/// Heals are not damage (`combat::heal` never calls `damage_with_aoe`), so they keep their
/// authored value even with row 20 present and a high-level caster.
#[tokio::test]
async fn damage_pipeline_level_bonus_does_not_scale_heals() {
    let mut data = fixed_heal_dot_data(1_100);
    data.formulas
        .insert_formula(LEVEL_BONUS_ROW, LEVEL_BONUS_FORMULA);
    let (server, mut client, player, _npc) = skills::cast_validation_tests::fixture(data);
    player.data().unit.level = 50;
    player.data().unit.hp = 500;
    let caster = WorldObject::from(&player);
    let target = caster.clone();
    let index = apply_tick_buff(&server, &caster, &target, HEAL_DOT_BUFF);
    client.drain();
    assert_eq!(tick_heal(&server, &target, index), 100);
}

// ---------------------------------------------------------------------------------------------
// Archery close-range penalty (`formulas` row 12, `damage_multiplier_by_range`).

const RANGE_PENALTY_ROW: u32 = 12;
const RANGE_PENALTY_FORMULA: &str = "if_negative(range - 8, 0.8, 1)";
const ENDLESS_ARROWS: u32 = 14_835;
const UNLISTED_SKILL: u32 = 90_001;

fn range_penalty_data(skill_id: u32) -> GameData {
    let mut data = fixture_data();
    data.formulas
        .insert_formula(RANGE_PENALTY_ROW, RANGE_PENALTY_FORMULA);
    data.systems.skills.range_penalty_skills.insert(skill_id);
    data
}

fn args_with_skill(
    caster: WorldObject,
    target: WorldObject,
    level: u8,
    skill_id: u32,
) -> EffectArgs<'static> {
    let skill =
        skills::state::SkillRun::new(Arc::new(skills::tests::skill_template(skill_id)), level);
    EffectArgs {
        caster_caster: if caster.is_unit() {
            SkillCaster::Unit(caster.obj_id())
        } else {
            SkillCaster::Doodad(caster.obj_id())
        },
        caster: Some(caster),
        target_caster: skills::types::SkillCastTarget::Unit(target.obj_id()),
        target: Target::Obj(target),
        cast: CastAction::Skill { skill_id, tl_id: 0 },
        source: EffectSource::from_skill(skill),
        skill_object: &skills::types::SkillObject::None,
    }
}

/// Endless Arrows (skill 14835, "Damage decreases -20% if target is within 8m") is x0.8
/// under 8 m and x1 from 8 m on; the formula itself makes exactly 8 m neutral
/// (`if_negative(range - 8, ...)`).
#[tokio::test]
async fn damage_pipeline_range_penalty_scales_listed_skills_under_8m() {
    for (distance, expected) in [(5.0, 80), (8.0, 100), (10.0, 100)] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(range_penalty_data(ENDLESS_ARROWS));
        npc.data()
            .unit
            .transform
            .set_xyz(100.0 + distance, 100.0, 100.0);
        let effect = level_damage_effect(1);
        client.drain();
        damage(
            &server,
            &effect,
            &args_with_skill((&player).into(), (&npc).into(), 1, ENDLESS_ARROWS),
            None,
        );
        assert_eq!(
            packet_with_skill(&mut client, player.obj_id, npc.obj_id, ENDLESS_ARROWS).0,
            expected,
            "distance {distance} m"
        );
        assert_eq!(npc.data().unit.hp, 1000 - expected);
    }
}

/// The distance is the skill-range measure: 3D, so standing directly above the target at 7 m
/// is under the threshold and at 9 m is not (a horizontal-only distance would be 0).
#[tokio::test]
async fn damage_pipeline_range_penalty_uses_the_3d_distance() {
    for (height, expected) in [(7.0, 80), (9.0, 100)] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(range_penalty_data(ENDLESS_ARROWS));
        npc.data()
            .unit
            .transform
            .set_xyz(100.0, 100.0, 100.0 + height);
        let effect = level_damage_effect(1);
        client.drain();
        damage(
            &server,
            &effect,
            &args_with_skill((&player).into(), (&npc).into(), 1, ENDLESS_ARROWS),
            None,
        );
        assert_eq!(
            packet_with_skill(&mut client, player.obj_id, npc.obj_id, ENDLESS_ARROWS).0,
            expected,
            "height {height} m"
        );
    }
}

/// The penalty is a property of the skill, not of its damage type: a listed skill is scaled
/// even when its damage is magic, while an unlisted skill keeps its damage at any type.
#[tokio::test]
async fn damage_pipeline_range_penalty_is_per_skill_not_damage_type() {
    for (label, skill_id, damage_type, expected) in [
        ("listed skill with magic damage", ENDLESS_ARROWS, 2, 80),
        ("unlisted skill with melee damage", UNLISTED_SKILL, 1, 100),
        ("unlisted skill with magic damage", UNLISTED_SKILL, 2, 100),
    ] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(range_penalty_data(ENDLESS_ARROWS));
        let mut effect = level_damage_effect(1);
        effect.damage_type = damage_type;
        client.drain();
        damage(
            &server,
            &effect,
            &args_with_skill((&player).into(), (&npc).into(), 1, skill_id),
            None,
        );
        assert_eq!(
            packet_with_skill(&mut client, player.obj_id, npc.obj_id, skill_id).0,
            expected,
            "{label} at 5 m"
        );
    }
}

/// The factor is the `formulas` row's text, not a hard-coded 0.8; without the row the hit is
/// unchanged.
#[tokio::test]
async fn damage_pipeline_range_penalty_rate_comes_from_the_formula_row() {
    for (formula, distance, expected) in [
        (RANGE_PENALTY_FORMULA, 5.0, 80),
        ("if_negative(range - 10, 0.5, 1)", 9.0, 50),
    ] {
        let mut data = fixture_data();
        data.formulas.insert_formula(RANGE_PENALTY_ROW, formula);
        data.systems
            .skills
            .range_penalty_skills
            .insert(ENDLESS_ARROWS);
        let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
        npc.data()
            .unit
            .transform
            .set_xyz(100.0 + distance, 100.0, 100.0);
        let effect = level_damage_effect(1);
        client.drain();
        damage(
            &server,
            &effect,
            &args_with_skill((&player).into(), (&npc).into(), 1, ENDLESS_ARROWS),
            None,
        );
        assert_eq!(
            packet_with_skill(&mut client, player.obj_id, npc.obj_id, ENDLESS_ARROWS).0,
            expected,
            "formula {formula} at {distance} m"
        );
    }

    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture({
        let mut data = fixture_data();
        data.systems
            .skills
            .range_penalty_skills
            .insert(ENDLESS_ARROWS);
        data
    });
    let effect = level_damage_effect(1);
    client.drain();
    damage(
        &server,
        &effect,
        &args_with_skill((&player).into(), (&npc).into(), 1, ENDLESS_ARROWS),
        None,
    );
    assert_eq!(
        packet_with_skill(&mut client, player.obj_id, npc.obj_id, ENDLESS_ARROWS).0,
        100
    );
}

/// Authored fixed values and percent-of-HP rows are excluded, as with the level bonus.
#[tokio::test]
async fn damage_pipeline_range_penalty_keeps_fixed_and_percent_rows() {
    for (label, effect) in [
        ("fixed", skills::tests::damage_effect(1, 100)),
        ("percent", percent_damage_effect(10)),
    ] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(range_penalty_data(ENDLESS_ARROWS));
        client.drain();
        damage(
            &server,
            &effect,
            &args_with_skill((&player).into(), (&npc).into(), 1, ENDLESS_ARROWS),
            None,
        );
        assert_eq!(
            packet_with_skill(&mut client, player.obj_id, npc.obj_id, ENDLESS_ARROWS).0,
            100,
            "{label} row at 5 m must keep its authored value"
        );
    }
}

/// A buff tick knows the skill that applied the buff (`Buff.skill_id`): a DoT of a listed skill
/// is scaled per tick, while a buff applied without a skill (or by an unlisted skill) is not.
#[tokio::test]
async fn damage_pipeline_range_penalty_applies_to_listed_skill_dot_ticks() {
    for (label, skill_id, expected) in [
        ("listed skill", Some(ENDLESS_ARROWS), 80),
        ("unlisted skill", Some(UNLISTED_SKILL), 100),
        ("no skill", None, 100),
    ] {
        let (server, mut client, player, npc) = skills::cast_validation_tests::fixture({
            let mut data = dot_data(level_damage_effect(DOT_EFFECT), 1_000, 1_000.0);
            data.formulas
                .insert_formula(RANGE_PENALTY_ROW, RANGE_PENALTY_FORMULA);
            data.systems
                .skills
                .range_penalty_skills
                .insert(ENDLESS_ARROWS);
            data
        });
        let caster = WorldObject::from(&player);
        let target = WorldObject::from(&npc);
        let index = apply_tick_buff_from_skill(&server, &caster, &target, DOT_BUFF, skill_id);
        client.drain();
        assert_eq!(tick_damage(&server, &target, index), expected, "{label}");
    }
}

/// The multiplier is captured when the DoT buff is applied (the tooltip describes the shot):
/// applying at 20 m and moving into melee leaves the ticks at x1, while applying at 5 m and
/// moving away keeps them at x0.8.
#[tokio::test]
async fn damage_pipeline_range_penalty_dot_uses_the_application_distance() {
    for (apply_distance, tick_distance, expected) in [(20.0, 2.0, 100), (5.0, 20.0, 80)] {
        let (server, mut client, player, npc) = skills::cast_validation_tests::fixture({
            let mut data = dot_data(level_damage_effect(DOT_EFFECT), 1_000, 1_000.0);
            data.formulas
                .insert_formula(RANGE_PENALTY_ROW, RANGE_PENALTY_FORMULA);
            data.systems
                .skills
                .range_penalty_skills
                .insert(ENDLESS_ARROWS);
            data
        });
        npc.data()
            .unit
            .transform
            .set_xyz(100.0, 100.0 + apply_distance, 100.0);
        let caster = WorldObject::from(&player);
        let target = WorldObject::from(&npc);
        let index =
            apply_tick_buff_from_skill(&server, &caster, &target, DOT_BUFF, Some(ENDLESS_ARROWS));
        npc.data()
            .unit
            .transform
            .set_xyz(100.0, 100.0 + tick_distance, 100.0);
        client.drain();
        assert_eq!(
            tick_damage(&server, &target, index),
            expected,
            "applied at {apply_distance} m, ticked at {tick_distance} m"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Height damage multiplier (`formulas` row 11, `damage_multiplier_by_height`).

const HEIGHT_ROW: u32 = 11;
const HEIGHT_FORMULA: &str = "if_negative(range - 1, 1, 1.05 + (min(range, 100)/100))";

fn height_data() -> GameData {
    let mut data = fixture_data();
    data.formulas.insert_formula(HEIGHT_ROW, HEIGHT_FORMULA);
    data
}

/// The level-damage row gives a deterministic 100 base; only the effect's own
/// `adjust_damage_by_height` flag scopes formula row 11.
fn height_damage_effect(id: u32) -> DamageEffect {
    let mut effect = level_damage_effect(id);
    effect.adjust_damage_by_height = true;
    effect
}

/// Attacker 10 m above the target: `1.05 + 10/100` = x1.15 (formula row 11). Below 1 m and
/// below the target the row's `if_negative(range - 1, ...)` keeps x1, for every damage type.
#[tokio::test]
async fn damage_pipeline_height_bonus_scales_attacks_from_higher_ground() {
    for (damage_type, height, expected) in [
        (1, 10.0, 115),
        (2, 10.0, 115),
        (4, 10.0, 115),
        (1, 0.5, 100),
        (1, -10.0, 100),
        (1, 100.0, 205),
    ] {
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(height_data());
        player
            .data()
            .unit
            .transform
            .set_xyz(100.0, 100.0, 100.0 + height);
        let mut effect = height_damage_effect(1);
        effect.damage_type = damage_type;
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
        );
        assert_eq!(
            packet(&mut client, player.obj_id, npc.obj_id).0,
            expected,
            "damage type {damage_type} at {height} m height difference"
        );
        assert_eq!(npc.data().unit.hp, 1000 - expected);
    }
}

/// The factor is the `formulas` row's text, not a hard-coded 1.05 / 100; without the row the
/// hit is unchanged (a failed lookup returns x1).
#[tokio::test]
async fn damage_pipeline_height_bonus_rate_comes_from_the_formula_row() {
    for (formula, expected) in [
        (HEIGHT_FORMULA, 115),
        ("if_negative(range - 5, 1, 1 + (min(range, 100)/10))", 200),
    ] {
        let mut data = fixture_data();
        data.formulas.insert_formula(HEIGHT_ROW, formula);
        let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
        player.data().unit.transform.set_xyz(100.0, 100.0, 110.0);
        let effect = height_damage_effect(1);
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
        );
        assert_eq!(
            packet(&mut client, player.obj_id, npc.obj_id).0,
            expected,
            "formula {formula}"
        );
    }

    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(fixture_data());
    player.data().unit.transform.set_xyz(100.0, 100.0, 110.0);
    let effect = height_damage_effect(1);
    client.drain();
    damage(
        &server,
        &effect,
        &args((&player).into(), (&npc).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, 100);
}

/// The 34 `adjust_damage_by_height = 'f'` rows are the exception: the same higher ground does
/// not scale a row without the flag.
#[tokio::test]
async fn damage_pipeline_height_bonus_honours_the_effect_flag() {
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(height_data());
    player.data().unit.transform.set_xyz(100.0, 100.0, 110.0);
    let effect = level_damage_effect(1);
    client.drain();
    damage(
        &server,
        &effect,
        &args((&player).into(), (&npc).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, player.obj_id, npc.obj_id).0, 100);
}

/// Authored fixed values and percent-of-HP rows are excluded, as with the level bonus and the
/// archery range penalty.
#[tokio::test]
async fn damage_pipeline_height_bonus_keeps_fixed_and_percent_rows() {
    for (label, mut effect) in [
        ("fixed", skills::tests::damage_effect(1, 100)),
        ("percent", percent_damage_effect(10)),
    ] {
        effect.adjust_damage_by_height = true;
        let (server, mut client, player, npc) =
            skills::cast_validation_tests::fixture(height_data());
        player.data().unit.transform.set_xyz(100.0, 100.0, 110.0);
        client.drain();
        damage(
            &server,
            &effect,
            &args((&player).into(), (&npc).into(), 1),
            None,
        );
        assert_eq!(
            packet(&mut client, player.obj_id, npc.obj_id).0,
            100,
            "{label} row 10 m above must keep its authored value"
        );
    }
}

/// Every caster kind qualifies (the row names no player / NPC variable): an NPC attacking a
/// player from 10 m above gets x1.15 like a player would. The NPC `LevelDps` is supplied by the
/// same hook the NPC subsystem uses, so the base is the deterministic 100.
struct NpcLevelDps;
impl skills::ext::NpcSkillHooks for NpcLevelDps {
    fn npc_attribute(&self, _: &GameServer, _: &Npc, attr: StatKind) -> Option<f64> {
        (attr == StatKind::LevelDps).then_some(100.0)
    }
}

#[tokio::test]
async fn damage_pipeline_height_bonus_applies_to_npc_casters() {
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(height_data());
    server
        .systems
        .skills
        .ext
        .npcs
        .install(Arc::new(NpcLevelDps));
    npc.data().unit.transform.set_xyz(100.0, 100.0, 110.0);
    let effect = height_damage_effect(1);
    client.drain();
    damage(
        &server,
        &effect,
        &args((&npc).into(), (&player).into(), 1),
        None,
    );
    assert_eq!(packet(&mut client, npc.obj_id, player.obj_id).0, 115);
    assert_eq!(player.data().unit.hp, 1000 - 115);
}

/// A DoT tick uses the height factor of the moment the buff was applied, like the archery
/// close-range factor: applying 10 m above ticks at x1.15 after the caster descends, and
/// applying level then rising does not add the bonus to later ticks.
#[tokio::test]
async fn damage_pipeline_height_bonus_dot_uses_the_application_height() {
    for (apply_height, tick_height, expected) in [(10.0, 0.0, 115), (0.0, 10.0, 100)] {
        let (server, mut client, player, npc) = skills::cast_validation_tests::fixture({
            let mut data = dot_data(height_damage_effect(DOT_EFFECT), 1_000, 1_000.0);
            data.formulas.insert_formula(HEIGHT_ROW, HEIGHT_FORMULA);
            data
        });
        player
            .data()
            .unit
            .transform
            .set_xyz(100.0, 100.0, 100.0 + apply_height);
        let caster = WorldObject::from(&player);
        let target = WorldObject::from(&npc);
        let index = apply_tick_buff(&server, &caster, &target, DOT_BUFF);
        player
            .data()
            .unit
            .transform
            .set_xyz(100.0, 100.0, 100.0 + tick_height);
        client.drain();
        assert_eq!(
            tick_damage(&server, &target, index),
            expected,
            "applied {apply_height} m above, ticked {tick_height} m above"
        );
    }
}
