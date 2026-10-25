//! Modifier application regressions. Rows below retain the 1.2 compact IDs/units;
//! reduced channel times keep the scheduler tests short. Packet fields are read in native
//! 1.2 order (D:/aa-re/client12/decomp/x2game12.c): 397C6580 (XP), 397CA390
//! (fired), 397C8760 (damage), 397C8EF0 (heal). No encoder generates expected packets.

use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::PacketStream;

use crate::data::GameData;
use crate::models::unit::{
    BonusTemplate, BuffAttribute, BuffModifier, SkillAttribute, SkillModifier, UnitAttribute,
    UnitModifierType,
};
use crate::opcodes::sc;
use crate::systems::skills;
use crate::test_support::{test_experience, TestClient};
use crate::world::{NpcRef, PlayerRef, WorldObject};

use skills::data::effects::HealEffect;
use skills::data::{BuffTemplate, EffectKind, EffectTemplate, SkillTemplate, TickEffect};
use skills::state::{AutoAttackSlot, CancelToken, SkillRef, SkillRun};
use skills::types::{SkillCastTarget, SkillCaster, SkillObject};

fn data() -> GameData {
    let mut data = GameData::empty();
    data.experience = test_experience(55);
    data.formulas = crate::systems::vehicles::stats::tests::formulas(
        &[(1, "100000", 14, 0), (2, "1000", 15, 0)],
        &[],
    );
    data
}

fn bonus(attribute: UnitAttribute, value: i32) -> BonusTemplate {
    BonusTemplate {
        attribute,
        modifier_type: UnitModifierType::Value,
        value,
        linear_level_bonus: 0,
    }
}

fn add_boost(data: &mut GameData, id: u32, bonuses: Vec<BonusTemplate>) {
    data.systems.skills.add_buff(BuffTemplate {
        id,
        kind: 1,
        stack_rule: 1,
        bonuses,
        ..Default::default()
    });
}

fn add_skill_modifier(
    data: &mut GameData,
    owner: u32,
    skill_id: u32,
    attr: SkillAttribute,
    kind: UnitModifierType,
    value: i32,
) {
    add_boost(data, owner, vec![]);
    data.systems
        .skills
        .skill_modifiers
        .entry(owner)
        .or_default()
        .push(SkillModifier {
            id: owner,
            owner_id: owner,
            tag_id: 0,
            skill_attribute: attr,
            unit_modifier_type: kind,
            value,
            skill_id,
            synergy: false,
        });
}

fn apply_boost(server: &crate::GameServer, player: &PlayerRef, id: u32) {
    let me = WorldObject::from(player);
    skills::buffs::apply_template(
        server,
        server.game_data.systems.skills.buff(id).unwrap(),
        Some(&me),
        &SkillCaster::Unit(player.obj_id),
        &me,
        None,
    );
    assert!(player.data().unit.buffs.check_buff(id));
}

fn cast(server: &crate::GameServer, player: &PlayerRef, target: u32, skill: &SkillRef) {
    assert_eq!(
        super::use_skill(
            server,
            skill,
            &WorldObject::from(player),
            SkillCaster::Unit(player.obj_id),
            &mut SkillCastTarget::Unit(target),
            Some(SkillObject::None),
            true,
        )
        .0,
        0,
    );
}

// Native 1.2 sub_397C7A40 (cast union), sub_397C85B0 (caster), and the readers above.
fn read_effect(s: &mut PacketStream, op: u16) -> (u32, i64) {
    match s.read_u8().unwrap() {
        0 => {
            s.read_u32().unwrap();
            s.read_u16().unwrap();
        }
        2 => {
            s.read_u32().unwrap();
            s.read_bc().unwrap();
            s.read_u32().unwrap();
            s.read_bool().unwrap();
            s.read_bool().unwrap();
        }
        other => panic!("unexpected cast union {other}"),
    }
    assert_eq!(s.read_u8().unwrap(), 0); // unit caster
    assert_eq!(s.read_bc().unwrap(), 0x100);
    if op == sc::SCUnitDamagedPacket {
        assert_eq!(s.read_bc().unwrap(), 0x100); // caster ID
        let target = s.read_bc().unwrap();
        s.read_u8().unwrap(); // crime
        let damage = s.read_pisc_w(3).unwrap();
        s.read_pisc_w(3).unwrap(); // mana burn
        s.read_u8().unwrap(); // holdable
        s.read_u16().unwrap(); // dice
        s.read_u8().unwrap();
        s.read_u8().unwrap();
        (target, damage[0])
    } else {
        assert_eq!(op, sc::SCUnitHealedPacket);
        let target = s.read_bc().unwrap();
        assert_eq!(s.read_u8().unwrap(), 0); // health
        assert_eq!(s.read_u8().unwrap(), 13); // noncritical heal
        let value = s.read_i32().unwrap();
        assert_eq!(s.read_i32().unwrap(), 0); // overheal field
        assert!(s.read_bool().unwrap());
        (target, i64::from(value))
    }
}

fn effects(client: &mut TestClient) -> Vec<(u32, i64)> {
    let mut out = vec![];
    for (op, body) in client.drain() {
        if op & 0x8000 != 0 {
            let raw = skills::packets::inflate_stored(&body).unwrap();
            let mut s = PacketStream::from_bytes(raw);
            for _ in 0..(op & 0x7fff) {
                assert_eq!(s.read_u16().unwrap(), 0);
                let op = s.read_u16().unwrap();
                out.push(read_effect(&mut s, op));
            }
            assert!(s.unread().is_empty());
        } else if op == sc::SCUnitDamagedPacket || op == sc::SCUnitHealedPacket {
            out.push(read_effect(&mut PacketStream::from_bytes(body), op));
        }
    }
    out
}

#[tokio::test]
async fn modifier_exp_buff_boosts_positive_xp_and_active_abilities_only() {
    let mut data = data();
    // compact unit_modifiers: buff 3084, ExpMul, Value +20 (percentage points).
    add_boost(&mut data, 3084, vec![bonus(UnitAttribute::ExpMul, 20)]);
    let (server, mut client, player, _) = skills::cast_validation_tests::fixture(data);
    {
        let mut c = player.data();
        c.experience = 10_000;
        [c.ability1, c.ability2, c.ability3] = [1, 2, 3];
        for a in c.abilities.abilities.iter_mut().take(3) {
            a.exp = 10_000;
        }
    }
    client.drain();
    skills::progress::add_exp(&server, &player, 100, true);
    // Native 1.2 sub_397C6580: BC unit ID, signed i32 XP delta, boolean ability flag.
    assert!(client
        .drain()
        .contains(&(sc::SCExpChangedPacket, vec![0, 1, 0, 100, 0, 0, 0, 1])));
    apply_boost(&server, &player, 3084);
    client.drain();
    skills::progress::add_exp(&server, &player, 100, true);
    assert!(client
        .drain()
        .contains(&(sc::SCExpChangedPacket, vec![0, 1, 0, 120, 0, 0, 0, 1])));
    assert_eq!(player.data().experience, 10_220);
    for a in &player.data().abilities.abilities {
        assert_eq!(a.exp, if a.id <= 3 { 10_220 } else { 0 });
    }
    skills::progress::add_exp(&server, &player, -30, true);
    assert_eq!(player.data().experience, 10_190); // death/ordinary loss unchanged
    skills::progress::add_exp(&server, &player, 100, false);
    assert_eq!(player.data().experience, 10_310);
    assert_eq!(player.data().abilities.abilities[0].exp, 10_190);
    skills::buffs::remove_buff(&server, &WorldObject::from(&player), 3084);
    skills::progress::add_exp(&server, &player, 100, true);
    assert_eq!(player.data().experience, 10_410);
}

#[tokio::test]
async fn modifier_exp_config_composes_and_large_gains_preserve_level_caps() {
    let mut data = data();
    add_boost(&mut data, 3084, vec![bonus(UnitAttribute::ExpMul, 20)]);
    let server = crate::test_support::test_server_with(data);
    server.config.update(|c| c.world.exp_rate = 2.0);
    let mut c = crate::test_support::test_character(1, "XP", 100.0, 100.0);
    c.experience = 10_000;
    c.ability1 = 1;
    c.unit.bonuses.add_bonus(
        1,
        crate::models::unit::Bonus {
            template: bonus(UnitAttribute::ExpMul, 20),
            value: 20,
        },
    );
    assert_eq!(
        skills::progress::add_exp_locked(&server, &mut c, 100, true)
            .unwrap()
            .exp,
        240
    );
    skills::progress::add_exp_locked(&server, &mut c, i32::MAX, true).unwrap();
    assert_eq!(c.unit.level, 55);
    assert_eq!(c.experience, 55_000);
    assert_eq!(c.abilities.abilities[0].exp, 55_000);
    server.config.update(|c| c.world.exp_rate = -1.0);
    assert_eq!(
        skills::progress::add_exp_locked(&server, &mut c, 100, true)
            .unwrap()
            .exp,
        0
    );
    assert_eq!(
        skills::progress::add_exp_locked(&server, &mut c, -10, false)
            .unwrap()
            .exp,
        -10
    );
}

struct Weapons;
impl skills::ext::ItemSkillHooks for Weapons {
    fn weapon_holdable(
        &self,
        _: &crate::GameServer,
        _: &WorldObject,
        slot: i32,
    ) -> Option<skills::ext::HoldableInfo> {
        Some(skills::ext::HoldableInfo {
            speed: match slot {
                15 => 1500.0,
                16 => 2000.0,
                17 => 1800.0,
                _ => return None,
            },
            slot_type_id: skills::stats::slot_type::ONE_HANDED,
            max_range: 30.0,
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn modifier_attack_delay_uses_the_weapon_and_matching_haste_once() {
    let mut data = data();
    // compact buff 933: MeleeSpeedMul +300; ranged should be unchanged.
    add_boost(
        &mut data,
        933,
        vec![bonus(UnitAttribute::MeleeSpeedMul, 300)],
    );
    add_boost(
        &mut data,
        1447,
        vec![bonus(UnitAttribute::RangedSpeedMul, -200)],
    );
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(skills::tests::damage_effect(1, 100))),
        1,
    );
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
    server.systems.skills.ext.items.install(Arc::new(Weapons));
    let me = WorldObject::from(&player);
    for (id, slot, base, boosted) in [
        (2, 15, 1500.0, 1500.0 / 1.3),
        (3, 16, 2000.0, 2000.0 / 1.3),
        (4, 17, 1800.0, 1800.0),
    ] {
        let t = SkillTemplate {
            weapon_slot_for_auto_attack_id: slot,
            effects: vec![skills::tests::skill_effect(1, EffectKind::DamageEffect, 1)],
            ..skills::tests::skill_template(id)
        };
        assert_eq!(super::attack_delay(&server, &t, &me, true, 1000.0), base);
        apply_boost(&server, &player, 933);
        assert!((super::attack_delay(&server, &t, &me, true, 1000.0) - boosted).abs() < 0.001);
        let skill = SkillRun::new(Arc::new(t), 1);
        let cancel = CancelToken::new();
        {
            let mut c = player.data();
            c.unit.current_target = npc.obj_id;
            c.unit.skill_state.auto_attack = Some(AutoAttackSlot {
                skill: skill.clone(),
                cancel: cancel.clone(),
            });
        }
        client.drain();
        assert_eq!(
            super::auto_attack_tick(&server, &player, &skill, &cancel),
            Some(boosted as i64)
        );
        assert_eq!(effects(&mut client), vec![(npc.obj_id, 100)]);
        skills::buffs::remove_buff(&server, &me, 933);
    }
    apply_boost(&server, &player, 1447);
    let t = skills::tests::skill_template(4);
    assert_eq!(super::attack_delay(&server, &t, &me, true, 1000.0), 2250.0);
    // Haste and GCD are distinct attributes; the GCD factor is included once.
    player.data().unit.bonuses.add_bonus(
        999,
        crate::models::unit::Bonus {
            template: bonus(UnitAttribute::GlobalCooldownMul, 1000),
            value: 1000,
        },
    );
    assert_eq!(super::attack_delay(&server, &t, &me, true, 1000.0), 1125.0);
}

fn heal_effect() -> HealEffect {
    HealEffect {
        id: 1,
        use_fixed_heal: true,
        fixed_min: 100,
        fixed_max: 100,
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
    }
}

#[tokio::test]
async fn modifier_heal_buff_changes_actual_healing_and_packet_once() {
    for fixed in [false, true] {
        heal_with_modifier(fixed).await;
    }
}

async fn heal_with_modifier(fixed: bool) {
    let mut data = data();
    // compact row 412: buff 5254, Fervent Healing 14929, Heal +7 percent.
    add_skill_modifier(
        &mut data,
        5254,
        14929,
        SkillAttribute::Heal,
        UnitModifierType::Percent,
        7,
    );
    let mut effect = heal_effect();
    effect.use_fixed_heal = fixed;
    effect.dps_multiplier = 1.0;
    data.systems
        .skills
        .add_effect(EffectKind::HealEffect, EffectTemplate::Heal(effect), 1);
    add_boost(&mut data, 999, vec![bonus(UnitAttribute::HealDps, 100_000)]);
    let (server, mut client, player, _) = skills::cast_validation_tests::fixture(data);
    apply_boost(&server, &player, 999);
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            target_type: 0,
            effects: vec![skills::tests::skill_effect(1, EffectKind::HealEffect, 1)],
            ..skills::tests::skill_template(14929)
        }),
        1,
    );
    for (boosted, expected) in [(false, 100), (true, 107), (false, 100)] {
        if boosted {
            apply_boost(&server, &player, 5254);
        } else {
            skills::buffs::remove_buff(&server, &WorldObject::from(&player), 5254);
        }
        player.data().unit.hp = 500;
        client.drain();
        cast(&server, &player, player.obj_id, &skill);
        assert_eq!(player.data().unit.hp, 500 + expected);
        assert_eq!(
            effects(&mut client),
            vec![(player.obj_id, i64::from(expected))]
        );
    }
}

#[tokio::test]
async fn modifier_existing_skill_damage_is_still_applied_once() {
    let mut data = data();
    add_skill_modifier(
        &mut data,
        900,
        10001,
        SkillAttribute::Damage,
        UnitModifierType::Percent,
        10,
    );
    add_boost(
        &mut data,
        901,
        // Keep both f32 weapon-damage endpoints exact before floor/ceil, so
        // random sampling cannot obscure whether the modifier was applied once.
        vec![bonus(UnitAttribute::MainhandDps, 80_000)],
    );
    let mut damage = skills::tests::damage_effect(1, 0);
    damage.use_fixed_damage = false;
    damage.use_mainhand_weapon = true;
    damage.dps_multiplier = 1.0;
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(damage)),
        1,
    );
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
    apply_boost(&server, &player, 901);
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            effects: vec![skills::tests::skill_effect(1, EffectKind::DamageEffect, 1)],
            ..skills::tests::skill_template(10001)
        }),
        1,
    );
    client.drain();
    cast(&server, &player, npc.obj_id, &skill);
    assert_eq!(effects(&mut client), vec![(npc.obj_id, 80)]);
    assert_eq!(npc.data().unit.hp, 920);
    apply_boost(&server, &player, 900);
    client.drain();
    cast(&server, &player, npc.obj_id, &skill);
    assert_eq!(effects(&mut client), vec![(npc.obj_id, 88)]);
    assert_eq!(npc.data().unit.hp, 832);
    skills::buffs::remove_buff(&server, &WorldObject::from(&player), 900);
    client.drain();
    cast(&server, &player, npc.obj_id, &skill);
    assert_eq!(effects(&mut client), vec![(npc.obj_id, 80)]);
    assert_eq!(npc.data().unit.hp, 752);
}

#[tokio::test]
async fn modifier_native_channel_upgrade_adds_four_seconds() {
    let mut data = data();
    // Unscaled compact row 470: Protective Wings 10714, buff 5326, +4000ms.
    add_skill_modifier(
        &mut data,
        5326,
        10714,
        SkillAttribute::ChannelingTime,
        UnitModifierType::Value,
        4000,
    );
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            channeling_time: 18000,
            channeling_tick: 1000,
            ..skills::tests::skill_template(10714)
        }),
        1,
    );
    for (boosted, expected) in [(false, 1810), (true, 2210)] {
        if boosted {
            apply_boost(&server, &player, 5326);
        }
        client.drain();
        cast(&server, &player, npc.obj_id, &skill);
        let fired = client
            .drain()
            .into_iter()
            .find(|(op, _)| *op == sc::SCSkillFiredPacket)
            .unwrap()
            .1;
        assert_eq!(
            i16::from_le_bytes(fired[17..19].try_into().unwrap()),
            expected
        );
        super::stop(&server, &skill, &WorldObject::from(&player), 0);
    }
}

#[tokio::test]
async fn modifier_buff_damage_matches_buff_id_or_tag_and_disappears_on_removal() {
    for by_tag in [false, true] {
        let mut data = data();
        let owner = if by_tag { 5263 } else { 356 };
        add_boost(&mut data, owner, vec![]);
        data.systems.skills.buff_game_data.buff_modifiers.insert(
            owner,
            vec![BuffModifier {
                id: if by_tag { 95 } else { 2 },
                owner_id: owner,
                buff_id: if by_tag { 0 } else { 121 },
                tag_id: if by_tag { 36 } else { 0 },
                buff_attribute: BuffAttribute::Damage,
                unit_modifier_type: UnitModifierType::Percent,
                value: 10,
                synergy: false,
            }],
        );
        data.systems.skills.buff_tags.insert(121, vec![36]);
        data.systems.skills.add_buff(BuffTemplate {
            id: 121,
            kind: 2,
            duration: 1000,
            tick: 1000.0,
            stack_rule: 1,
            tick_effects: vec![TickEffect {
                effect_id: 1,
                target_buff_tag_id: 0,
                target_no_buff_tag_id: 0,
            }],
            ..Default::default()
        });
        data.systems.skills.add_effect(
            EffectKind::DamageEffect,
            EffectTemplate::Damage(Box::new(skills::tests::damage_effect(1, 100))),
            1,
        );
        let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
        let me = WorldObject::from(&player);
        let target = WorldObject::from(&npc);
        for (boosted, expected) in [(false, 100), (true, 110), (false, 100)] {
            if boosted {
                apply_boost(&server, &player, owner);
            } else {
                skills::buffs::remove_buff(&server, &me, owner);
            }
            skills::buffs::apply_template(
                &server,
                server.game_data.systems.skills.buff(121).unwrap(),
                Some(&me),
                &SkillCaster::Unit(player.obj_id),
                &target,
                None,
            );
            let index = npc
                .data()
                .unit
                .buffs
                .effects
                .iter()
                .find(|b| b.template.id == 121)
                .unwrap()
                .index;
            let before = npc.data().unit.hp;
            client.drain();
            // Drive the real buff tick synchronously; do not duplicate its EffectSource setup.
            skills::buffs::schedule_effect(&server, &target, index, false);
            assert_eq!(npc.data().unit.hp, before - expected);
            assert_eq!(
                effects(&mut client),
                vec![(npc.obj_id, i64::from(expected))]
            );
            skills::buffs::remove_buff(&server, &target, 121);
        }
    }
}

#[tokio::test]
async fn modifier_area_buff_expands_real_target_selection_in_meters() {
    let mut data = data();
    // compact row 371: buff 5206 adds 5m to skill 11395's 5m radius.
    add_skill_modifier(
        &mut data,
        5206,
        11395,
        SkillAttribute::AreaRadius,
        UnitModifierType::Value,
        5,
    );
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(skills::tests::damage_effect(1, 100))),
        1,
    );
    let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
    player.data().unit.transform.set_xyz(100.0, 90.0, 100.0);
    let world = server.world.world(0).unwrap();
    let mut other = crate::models::npc::Npc::new(1);
    other.unit.hp = 1000;
    other.unit.max_hp = 1000;
    other.unit.transform.set_xyz(100.0, 112.0, 100.0); // 7m from original target
    let other: NpcRef = crate::world::Entity::new(0x201, other);
    world.spawn(&other, &server.view_ctx());
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            target_area_radius: 5,
            target_relation: 0,
            target_selection: 1,
            effects: vec![skills::tests::skill_effect(1, EffectKind::DamageEffect, 1)],
            ..skills::tests::skill_template(11395)
        }),
        1,
    );
    for (boosted, count) in [(false, 1), (true, 2), (false, 1)] {
        if boosted {
            apply_boost(&server, &player, 5206);
        } else {
            skills::buffs::remove_buff(&server, &WorldObject::from(&player), 5206);
        }
        client.drain();
        let before = other.data().unit.hp;
        cast(&server, &player, npc.obj_id, &skill);
        let packets = effects(&mut client);
        assert_eq!(packets.len(), count);
        assert!(packets.contains(&(npc.obj_id, 100)));
        assert_eq!(other.data().unit.hp, before - if boosted { 100 } else { 0 });
    }
}

#[tokio::test(start_paused = true)]
async fn modifier_channel_packet_and_ticks_share_a_snapshot_of_duration() {
    for boosted in [false, true] {
        let mut data = data();
        // Same Value/millisecond semantics as compact 5326 (+4000ms). Small values
        // let this test observe expiry and the ticks beyond the original duration.
        add_skill_modifier(
            &mut data,
            5326,
            10714,
            SkillAttribute::ChannelingTime,
            UnitModifierType::Value,
            200,
        );
        data.systems.skills.add_effect(
            EffectKind::DamageEffect,
            EffectTemplate::Damage(Box::new(skills::tests::damage_effect(1, 10))),
            1,
        );
        let (server, mut client, player, npc) = skills::cast_validation_tests::fixture(data);
        if boosted {
            apply_boost(&server, &player, 5326);
        }
        let skill = SkillRun::new(
            Arc::new(SkillTemplate {
                channeling_time: 200,
                channeling_tick: 50,
                effects: vec![skills::tests::skill_effect(1, EffectKind::DamageEffect, 1)],
                ..skills::tests::skill_template(10714)
            }),
            1,
        );
        client.drain();
        cast(&server, &player, npc.obj_id, &skill);
        let fired = client
            .drain()
            .into_iter()
            .find(|(op, _)| *op == sc::SCSkillFiredPacket)
            .unwrap()
            .1;
        // Native sub_397CA390: ID, timeline, caster, target, object, two msec fields.
        // Pin the current wire's duration field independently of the packet encoder.
        let wire_duration = i16::from_le_bytes(fired[17..19].try_into().unwrap());
        assert_eq!(wire_duration, if boosted { 50 } else { 30 });
        skills::buffs::remove_buff(&server, &WorldObject::from(&player), 5326);
        // Install the spawned timers, then visit each deadline. Wall-clock sleeps
        // can skip a tick under unrelated machine load and don't test this snapshot.
        tokio::task::yield_now().await;
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(20)).await;
        tokio::task::yield_now().await;
        assert_eq!(player.data().unit.skill_state.skill_task.is_some(), boosted);
        for step in [30, 50, 50, 50] {
            tokio::time::advance(Duration::from_millis(step)).await;
            tokio::task::yield_now().await;
        }
        assert!(player.data().unit.skill_state.skill_task.is_none());
        let packets = effects(&mut client);
        assert_eq!(packets.len(), if boosted { 7 } else { 3 });
        assert!(packets.iter().all(|p| *p == (npc.obj_id, 10)));
        assert_eq!(npc.data().unit.hp, if boosted { 930 } else { 970 });
    }
}
