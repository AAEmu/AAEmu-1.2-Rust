//! Regression scenarios for the native final-raw-attribute clamp. These use the
//! real buff lifecycle, gear hook, stat evaluators and XP/haste consumers. Native
//! numeric evidence and the literal compact fixture are in data::attribute_limits.

use std::sync::Arc;

use crate::data::attribute_limits::{tests::fixture as limits_fixture, AttributeLimit};
use crate::data::items::{EquipTemplate, ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::container::equip_slot;
use crate::models::item::Item;
use crate::models::unit::{
    Bonus, BonusTemplate, UnitAttribute as A, UnitBase, UnitModifierType as M,
};
use crate::systems::skills::{self, data::BuffTemplate, types::SkillCaster};
use crate::test_support::test_experience;
use crate::world::{Entity, WorldObject};

use super::{stat, stats, CharInputs, CharStats, StatKind as S};

fn data() -> GameData {
    let mut d = GameData::empty();
    let (unit, primary) = limits_fixture();
    d.characters.unit_attribute_limits = Arc::new(unit);
    d.characters.p_stat_limits = primary;
    d.experience = test_experience(55);
    d.formulas = crate::systems::vehicles::stats::tests::formulas(
        &[
            (1, "100", 22, 0),
            (2, "100", 23, 0),
            (3, "100", 24, 0),
            (4, "100", 25, 0),
            (5, "100", 26, 0),
            (6, "100", 27, 0),
            (7, "1000 + sta * 10", 14, 0),
            (8, "100", 15, 0),
            (9, "100", 18, 0),
        ],
        &[],
    );
    d
}

fn modifier(attr: A, kind: M, value: i32) -> BonusTemplate {
    BonusTemplate {
        attribute: attr,
        modifier_type: kind,
        value,
        linear_level_bonus: 0,
    }
}

fn boost(d: &mut GameData, id: u32, modifiers: Vec<BonusTemplate>) {
    d.systems.skills.add_buff(BuffTemplate {
        id,
        kind: 1,
        stack_rule: 1,
        bonuses: modifiers,
        ..Default::default()
    });
}

fn apply(server: &crate::GameServer, obj: &WorldObject, id: u32) {
    skills::buffs::apply_template(
        server,
        server.game_data.systems.skills.buff(id).unwrap(),
        Some(obj),
        &SkillCaster::Unit(obj.obj_id()),
        obj,
        None,
    );
    assert!(obj.with_unit(|u| u.unit().buffs.check_buff(id)).unwrap());
}

#[tokio::test]
async fn attribute_limits_equipment_and_stacked_buff_flow_caps_only_final_value() {
    let mut d = data();
    const ITEM: u32 = 60001;
    d.items.templates.insert(
        ITEM,
        ItemTemplate::new(ITEM, TemplateKind::Weapon(EquipTemplate::default())),
    );
    d.systems.skills.item_unit_modifiers.insert(
        ITEM,
        vec![
            modifier(A::MaxHealth, M::Value, 20_000_000),
            modifier(A::Armor, M::Value, 2_000_000),
        ],
    );
    boost(
        &mut d,
        60002,
        vec![
            modifier(A::MaxHealth, M::Value, -15_000_000),
            modifier(A::Armor, M::Value, -1_500_000),
        ],
    );
    boost(&mut d, 60003, vec![modifier(A::MaxHealth, M::Percent, 100)]);
    let (server, _client, p, _) = skills::cast_validation_tests::fixture(d);
    crate::systems::economy::bridges::install(&server);
    let obj = WorldObject::from(&p);
    assert_eq!(
        stats(&server, &obj, &[S::MaxHp, S::Armor]),
        vec![2_000.0, 100.0]
    );

    let t = server.game_data.items.template(ITEM).unwrap();
    let mut item = Item::create(42, t, 1, 0, 0, &server.game_data.items, 0);
    item.class = t.class_type();
    item.owner_id = u64::from(p.character_id);
    item.slot_type = crate::models::container::slot_type::EQUIPMENT;
    item.slot = equip_slot::MAINHAND;
    p.data()
        .inventory
        .equipment()
        .lock()
        .unwrap()
        .items
        .push(item.clone());
    let gear = server.systems.economy.hooks.gear.get().unwrap();
    gear.update_gear_bonuses(&server, &obj, Some(&item), None);
    assert_eq!(
        stats(&server, &obj, &[S::MaxHp, S::Armor]),
        vec![10_000_000.0, 1_000_000.0]
    );
    assert_eq!(p.data().unit.max_hp, 10_000_000);

    // The original +20M item modifier remains intact. Clamping that modifier or
    // the intermediate result would produce a different answer when -15M arrives.
    apply(&server, &obj, 60002);
    assert_eq!(
        stats(&server, &obj, &[S::MaxHp, S::Armor]),
        vec![5_002_000.0, 500_100.0]
    );
    apply(&server, &obj, 60003);
    assert_eq!(stat(&server, &obj, S::MaxHp), 10_000_000.0);
    skills::buffs::remove_buff(&server, &obj, 60003);
    assert_eq!(p.data().unit.max_hp, 5_002_000);
    skills::buffs::remove_buff(&server, &obj, 60002);
    assert_eq!(p.data().unit.max_hp, 10_000_000);
    p.data().inventory.equipment().lock().unwrap().items.clear();
    gear.update_gear_bonuses(&server, &obj, None, Some(&item));
    assert_eq!(
        stats(&server, &obj, &[S::MaxHp, S::Armor]),
        vec![2_000.0, 100.0]
    );
    assert_eq!(p.data().unit.max_hp, 2_000);
}

#[tokio::test]
async fn attribute_limits_extreme_buff_xp_haste_and_multiplier_units() {
    let mut d = data();
    let attrs = [
        A::MoveSpeedMul,
        A::MeleeSpeedMul,
        A::RangedSpeedMul,
        A::GlobalCooldownMul,
        A::CastingTimeMul,
        A::MeleeDamageMul,
        A::RangedDamageMul,
        A::SpellDamageMul,
        A::HealMul,
        A::ExpMul,
    ];
    for (id, value) in [(60010, i32::MAX), (60011, i32::MIN)] {
        boost(
            &mut d,
            id,
            attrs.map(|a| modifier(a, M::Value, value)).to_vec(),
        );
    }
    let (server, _client, p, _) = skills::cast_validation_tests::fixture(d);
    let obj = WorldObject::from(&p);
    let kinds = [
        S::MoveSpeedMul,
        S::GlobalCooldownMul,
        S::CastTimeMul,
        S::MeleeDamageMul,
        S::RangedDamageMul,
        S::SpellDamageMul,
        S::HealMul,
    ];
    assert_eq!(
        stats(&server, &obj, &kinds),
        vec![1.0, 100.0, 1.0, 1.0, 1.0, 1.0, 1.0]
    );
    p.data().experience = 10_000;
    let delay = |id| {
        skills::skill::attack_delay(
            &server,
            &skills::data::SkillTemplate {
                id,
                ..Default::default()
            },
            &obj,
            false,
            0.0,
        )
    };
    assert_eq!(delay(2), 1500.0);
    assert_eq!(delay(4), 1800.0);
    let base = skills::progress::add_exp_locked(&server, &mut p.data(), 100, false).unwrap();
    assert_eq!(base.exp, 100);

    apply(&server, &obj, 60010);
    assert_eq!(
        stats(&server, &obj, &kinds),
        vec![3.0, 20.0, 5.0, 6.0, 6.0, 6.0, 6.0]
    );
    for attr in [A::MeleeSpeedMul, A::RangedSpeedMul] {
        assert_eq!(p.data().unit.calculate_with_bonuses(0.0, attr, 0), 4000.0);
    }
    assert_eq!(delay(2), 60.0); // 1500 * GCD(.2) * haste(.2)
    assert_eq!(delay(4), 72.0);
    let gain = skills::progress::add_exp_locked(&server, &mut p.data(), 100, false).unwrap();
    assert_eq!(gain.exp, 500);
    skills::buffs::remove_buff(&server, &obj, 60010);

    apply(&server, &obj, 60011);
    let values = stats(&server, &obj, &kinds);
    for (value, expected) in values
        .into_iter()
        .zip([-10.0, 500.0, 0.4, 0.0, 0.0, 0.0, 0.2])
    {
        assert!((value - expected).abs() < 1e-6, "{value} != {expected}");
    }
    for attr in [A::MeleeSpeedMul, A::RangedSpeedMul] {
        assert_eq!(p.data().unit.calculate_with_bonuses(0.0, attr, 0), -800.0);
    }
    assert_eq!(delay(2), 37500.0); // 1500 * GCD(5) * haste(5)
    assert_eq!(delay(4), 45000.0);
    assert_eq!(
        skills::progress::add_exp_locked(&server, &mut p.data(), 100, false)
            .unwrap()
            .exp,
        0
    );
    // XP loss still bypasses bonuses; positive gain still saturates before i32 conversion.
    assert_eq!(
        skills::progress::add_exp_locked(&server, &mut p.data(), -10, false)
            .unwrap()
            .exp,
        -10
    );
    skills::buffs::remove_buff(&server, &obj, 60011);
    assert_eq!(
        stats(&server, &obj, &kinds),
        vec![1.0, 100.0, 1.0, 1.0, 1.0, 1.0, 1.0]
    );
}

#[tokio::test]
async fn attribute_limits_detached_xp_and_character_formulas_use_server_data() {
    let server = crate::test_support::test_server_with(data());
    let mut c = crate::test_support::test_character(1, "Detached", 0.0, 0.0);
    c.experience = 10_000;
    c.unit.bonuses.add_bonus(
        1,
        Bonus {
            template: modifier(A::ExpMul, M::Value, 10000),
            value: 10000,
        },
    );
    assert_eq!(
        skills::progress::add_exp_locked(&server, &mut c, 100, false)
            .unwrap()
            .exp,
        500
    );
    assert!(Arc::ptr_eq(
        &c.unit.bonuses.attribute_limits,
        &server.game_data.characters.unit_attribute_limits
    ));
    for (attr, value) in [
        (A::Str, 10_000),
        (A::Dex, -101),
        (A::MoveSpeedMul, 99999),
        (A::MaxHealth, 99999999),
    ] {
        c.unit.bonuses.add_bonus(
            2,
            Bonus {
                template: modifier(attr, M::Value, value),
                value,
            },
        );
    }
    let mut calc = CharStats::new(&server, &c, CharInputs::default(), 0);
    assert_eq!(calc.get(S::Str), 10_100.0); // pStat limits cannot cap equipment/buffs
    assert_eq!(calc.get(S::Dex), -1.0);
    assert_eq!(calc.get(S::MoveSpeedMul), 3.0);
    assert_eq!(calc.get(S::MaxHp), 10_000_000.0);

    // Explicit evaluator data must also work before any mutable path has bound
    // this unit, and must not reuse the limits from another GameServer.
    let mut other = data();
    Arc::make_mut(&mut other.characters.unit_attribute_limits)
        .attributes
        .insert(
            A::MaxHealth,
            AttributeLimit {
                minimum: 0,
                maximum: 5000,
            },
        );
    let other = crate::test_support::test_server_with(other);
    assert_eq!(
        CharStats::new(&other, &c, CharInputs::default(), 0).get(S::MaxHp),
        5000.0
    );
    assert_eq!(
        CharStats::new(&server, &c, CharInputs::default(), 0).get(S::MaxHp),
        10_000_000.0
    );
    assert_eq!(
        skills::progress::add_exp_locked(&server, &mut c, i32::MAX, false)
            .unwrap()
            .exp,
        i32::MAX
    );
    assert!(c.experience >= 10_000); // positive gain cannot wrap into negative XP
}

#[tokio::test]
async fn attribute_limits_detached_npc_and_vehicle_getters_widen_before_final_cap() {
    use crate::models::npc::Npc;
    use crate::systems::npcs::npc::{data::NpcTemplate, stats as npc_stats};
    use crate::systems::vehicles::stats::{refresh_pools, stats_of, VehicleKind};
    let mut d = data();
    // Synthetic extreme formulas make incorrect character-only 10000 caps and
    // overflow in the separate NPC/vehicle integer loops observable.
    let mut rows = vec![];
    let mut id = 1;
    for owner in [1, 2, 4, 5, 6] {
        for (formula, kind) in [("20000", 22), ("1000", 14), ("100", 18)] {
            rows.push((id, formula, kind, owner));
            id += 1;
        }
    }
    d.formulas = crate::systems::vehicles::stats::tests::formulas(&rows, &[]);
    d.systems.npcs.npc.templates.insert(
        1,
        NpcTemplate {
            id: 1,
            ..Default::default()
        },
    );
    let server = crate::test_support::test_server_with(d);
    let mut npc = Npc::new(1);
    let baseline = npc_stats::npc_stats(&server.game_data, &npc).unwrap();
    assert_eq!(
        (baseline.str_, baseline.max_hp, baseline.armor),
        (20000, 1000, 100)
    );
    for attr in [A::MaxHealth, A::Armor] {
        for index in 1..=2 {
            npc.unit.bonuses.add_bonus(
                index,
                Bonus {
                    template: modifier(attr, M::Value, i32::MAX),
                    value: i32::MAX,
                },
            );
        }
    }
    let boosted = npc_stats::npc_stats(&server.game_data, &npc).unwrap();
    assert_eq!(
        (boosted.str_, boosted.max_hp, boosted.armor),
        (20000, 10_000_000, 1_000_000)
    );
    npc_stats::refresh_cached_stats(&server.game_data, &mut npc);
    assert_eq!(npc.unit.max_hp, 10_000_000);
    assert!(Arc::ptr_eq(
        &npc.unit.bonuses.attribute_limits,
        &server.game_data.characters.unit_attribute_limits
    ));

    for kind in [
        VehicleKind::Mate { mate_kind_id: 1 },
        VehicleKind::Slave,
        VehicleKind::Transfer,
        VehicleKind::Shipyard,
    ] {
        let mut u = UnitBase::default();
        assert_eq!(
            stats_of(&server, kind, &u, &[S::Str, S::MaxHp]),
            vec![20000.0, 1000.0]
        );
        for index in 1..=2 {
            u.bonuses.add_bonus(
                index,
                Bonus {
                    template: modifier(A::MaxHealth, M::Value, i32::MAX),
                    value: i32::MAX,
                },
            );
        }
        assert_eq!(
            stats_of(&server, kind, &u, &[S::Str, S::MaxHp]),
            vec![20000.0, 10_000_000.0]
        );
        refresh_pools(&server, kind, &mut u);
        assert_eq!(u.max_hp, 10_000_000);
        assert!(Arc::ptr_eq(
            &u.bonuses.attribute_limits,
            &server.game_data.characters.unit_attribute_limits
        ));
        u.bonuses.statics.clear();
        for index in 1..=2 {
            u.bonuses.add_bonus(
                index,
                Bonus {
                    template: modifier(A::MaxHealth, M::Value, i32::MIN),
                    value: i32::MIN,
                },
            );
        }
        refresh_pools(&server, kind, &mut u);
        assert_eq!(u.max_hp, -2_000_000_000);
    }
}

#[test]
fn attribute_limits_dynamic_percent_order_and_extreme_stacks_remain_finite() {
    use crate::models::unit::{DynamicBonus, DynamicBonusTemplate, LinearFuncTemplate};
    let (limits, _) = limits_fixture();
    let mut u = UnitBase::default();
    u.bonuses.bind_attribute_limits(&Arc::new(limits));
    for (index, kind, value) in [
        (1, M::Percent, 100),
        (2, M::Value, 2000),
        (3, M::Value, -2500),
    ] {
        u.bonuses.add_bonus(
            index,
            Bonus {
                template: modifier(A::MoveSpeedMul, kind, value),
                value,
            },
        );
    }
    u.bonuses.add_dynamic_bonus(
        4,
        DynamicBonus {
            template: DynamicBonusTemplate {
                attribute: A::MoveSpeedMul,
                modifier_type: M::Percent,
                func_id: 1,
                func_type: "LinearFunc".into(),
            },
            source_buff_index: 4,
            source_start_ms: 0,
            source_duration: 1000,
            linear_func: Some(LinearFuncTemplate {
                id: 1,
                start_value: 100,
                end_value: 100,
            }),
            manual_values: None,
            source_tick_ms: 100.0,
        },
    );
    // (1000 + 2000 - 2500) * 2 * 2; no intermediate clamp despite reaching 3000.
    assert_eq!(
        u.calculate_with_bonuses(1000.0, A::MoveSpeedMul, 500),
        2000.0
    );
    for index in 5..150 {
        u.bonuses.add_bonus(
            index,
            Bonus {
                template: modifier(A::MoveSpeedMul, M::Percent, i32::MAX),
                value: i32::MAX,
            },
        );
    }
    assert_eq!(
        u.calculate_with_bonuses(1000.0, A::MoveSpeedMul, 500),
        3000.0
    );
    assert_eq!(
        u.calculate_with_bonuses(f64::NAN, A::MoveSpeedMul, 500),
        0.0
    );
}

#[tokio::test]
async fn attribute_limits_ai_movement_matches_stat_getter_at_raw_boundaries() {
    use crate::systems::ai::npc;

    // Literal native limits: complete MoveSpeedMul starts at 1000 and is bounded
    // to -10000..3000 before /1000. Adding 1 after clamping a zero-based bonus
    // made +5000 move at 4x while the stat getter correctly reported 3x.
    let cases = [
        (0, 0, 1.0),
        (500, 0, 1.5),
        (2000, 0, 3.0),
        (2001, 0, 3.0),
        (5000, 0, 3.0),
        (-500, 0, 0.5),
        (-1000, 0, 0.0),
        (-10999, 0, -9.999),
        (-11000, 0, -10.0),
        (-11001, 0, -10.0),
        (0, 50, 1.5),
        (500, 100, 3.0),
        (0, -50, 0.5),
    ];
    let mut d = data();
    for (index, (flat, percent, _)) in cases.iter().enumerate() {
        boost(
            &mut d,
            60100 + index as u32,
            vec![
                modifier(A::MoveSpeedMul, M::Value, *flat),
                modifier(A::MoveSpeedMul, M::Percent, *percent),
            ],
        );
    }
    let (server, _client, _player, n) = skills::cast_validation_tests::fixture(d);
    let obj = WorldObject::from(&n);
    for (index, (flat, percent, expected_mul)) in cases.into_iter().enumerate() {
        let buff_id = 60100 + index as u32;
        assert_eq!(npc::real_movement_speed(&obj, 4.5), 4.5);
        apply(&server, &obj, buff_id);
        for base in [0.0, 0.75, 4.5] {
            // Exercise the actual AI helper before evaluating the separate getter.
            let actual = npc::real_movement_speed(&obj, base);
            let expected = base * expected_mul;
            let stat_speed = base * stat(&server, &obj, S::MoveSpeedMul);
            assert!(
                (actual - expected).abs() < 1e-5,
                "flat {flat}, percent {percent}, base {base}: {actual} != {expected}"
            );
            assert!(
                (actual - stat_speed).abs() < 1e-5,
                "AI {actual} != stat-derived speed {stat_speed}"
            );
        }
        skills::buffs::remove_buff(&server, &obj, buff_id);
        assert_eq!(npc::real_movement_speed(&obj, 4.5), 4.5);
    }
}

#[tokio::test]
async fn attribute_limits_are_per_server_for_every_spawned_unit_kind() {
    let a = data();
    let mut b = data();
    Arc::make_mut(&mut b.characters.unit_attribute_limits)
        .attributes
        .insert(
            A::MoveSpeedMul,
            AttributeLimit {
                minimum: -500,
                maximum: 1250,
            },
        );
    let sa = crate::test_support::test_server_with(a);
    let sb = crate::test_support::test_server_with(b);
    use crate::models::{
        gimmick::Gimmick,
        house::House,
        mate::Mate,
        npc::Npc,
        shipyard::{Shipyard, ShipyardData},
        slave::Slave,
        transfer::Transfer,
    };
    let objects = vec![
        WorldObject::from(Entity::new(100, Npc::new(1))),
        WorldObject::from(Entity::new(101, Mate::new(1, 1))),
        WorldObject::from(Entity::new(102, Slave::new(1, 1))),
        WorldObject::from(Entity::new(103, House::new(1, 1))),
        WorldObject::from(Entity::new(104, Transfer::new(1))),
        WorldObject::from(Entity::new(105, Shipyard::new(ShipyardData::default()))),
        WorldObject::from(Entity::new(106, Gimmick::new(1))),
    ];
    for obj in objects {
        obj.with_unit(|u| {
            u.unit_mut().bonuses.add_bonus(
                1,
                Bonus {
                    template: modifier(A::MoveSpeedMul, M::Value, 5000),
                    value: 5000,
                },
            );
        });
        for (server, expected) in [(&sa, 3000.0), (&sb, 1250.0), (&sa, 3000.0)] {
            server
                .world
                .world(0)
                .unwrap()
                .spawn(&obj, &server.view_ctx());
            assert_eq!(
                obj.with_unit(|u| u.unit().calculate_with_bonuses(1000.0, A::MoveSpeedMul, 0)),
                Some(expected)
            );
            assert!(obj
                .with_unit(|u| Arc::ptr_eq(
                    &u.unit().bonuses.attribute_limits,
                    &server.game_data.characters.unit_attribute_limits
                ))
                .unwrap());
            server.world.world(0).unwrap().delete(&obj);
        }
    }
}
