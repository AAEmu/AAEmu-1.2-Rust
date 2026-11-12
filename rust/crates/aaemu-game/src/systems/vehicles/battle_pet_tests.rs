//! Battle pets: the 1.2 client runs the pet AI (follow, auto target, auto-cast) and sends the
//! pet's skills as `CSStartSkill` with the mate as a plain `Unit` caster (the pet bar clicks
//! too). The server casts them as the mate, with the mate's own range / GCD / cooldowns, and
//! accepts only the mate's base skill, its bar skills and the skills its level allows.

use super::super::data::{MountSkills, NpcMountSkills};
use super::*;
use crate::models::npc::Npc;
use crate::systems::skills::data::effects::{EffectTemplate, HealEffect, SpecialEffect};
use crate::systems::skills::data::EffectKind;
use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
use crate::systems::skills::tests::{damage_effect, skill_effect};
use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};
use crate::world::{Entity, NpcRef};

const BASIC: u32 = 300_201;
const BAR_HIT: u32 = 300_202;
const HIGH_LEVEL: u32 = 300_203;
const BAR_HEAL: u32 = 300_204;
const NOT_ON_BAR: u32 = 300_205;
const TARGET_OBJ: u32 = 0x0200;
const POTION_ITEM: u32 = 18649;
const POTION_SKILL: u32 = 15220;
const POTION_EFFECT: u32 = 12554;

fn pet_skill(id: u32, hostile: bool) -> crate::systems::skills::data::SkillTemplate {
    let mut t = crate::systems::skills::tests::skill_template(id);
    t.ability_level = 1;
    if hostile {
        t.max_range = 25;
        // The weaponless 0-3 m fallback must not apply to a mate caster.
        t.weapon_slot_for_range_id = 15;
        t.effects
            .push(skill_effect(900_001, EffectKind::DamageEffect, 1));
    } else {
        t.target_type = 0; // Self
        t.target_selection = 1;
        t.damage_type_id = 0;
        t.max_range = 30;
        t.effects
            .push(skill_effect(900_002, EffectKind::HealEffect, 2));
    }
    t
}

fn pet_data() -> GameData {
    let mut d = data();
    d.systems
        .npcs
        .npc
        .templates
        .get_mut(&MATE_NPC)
        .expect("mate template")
        .base_skill_id = BASIC as i32;
    let s = &mut d.systems.skills;
    s.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(damage_effect(1, 100))),
        900_001,
    );
    s.add_effect(
        EffectKind::HealEffect,
        EffectTemplate::Heal(HealEffect {
            id: 2,
            use_fixed_heal: true,
            fixed_min: 150,
            fixed_max: 150,
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
        900_002,
    );
    s.add_skill(pet_skill(BASIC, true));
    s.add_skill(pet_skill(BAR_HIT, true));
    let mut high = pet_skill(HIGH_LEVEL, true);
    high.ability_level = 30;
    s.add_skill(high);
    s.add_skill(pet_skill(BAR_HEAL, false));
    s.add_skill(pet_skill(NOT_ON_BAR, true));
    let v = &mut d.systems.vehicles;
    v.npc_mount_skills.clear();
    for (i, (row, skill)) in [(31, BAR_HIT), (32, HIGH_LEVEL), (33, BAR_HEAL)]
        .into_iter()
        .enumerate()
    {
        v.mount_skills.push(MountSkills {
            id: row,
            name: "pet".into(),
            skill_id: skill,
        });
        v.npc_mount_skills.push(NpcMountSkills {
            id: 40 + i as u32,
            npc_id: MATE_NPC,
            mount_skill_id: row,
        });
    }
    // Item 18649 "Pet Dual-Recovery Potion" -> skill 15220 -> effect 12554 -> SpecialEffect
    // 3461 (type 56 HealPet, 20 / 20).
    d.items.templates.insert(
        POTION_ITEM,
        ItemTemplate {
            use_skill_id: POTION_SKILL,
            use_skill_as_reagent: true,
            ..ItemTemplate::plain(POTION_ITEM, 1)
        },
    );
    let s = &mut d.systems.skills;
    s.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: POTION_EFFECT,
            special_effect_type_id: 56,
            value1: 20,
            value2: 20,
            value3: 0,
            value4: 0,
        }),
        POTION_EFFECT,
    );
    let mut potion = crate::systems::skills::tests::skill_template(POTION_SKILL);
    potion.target_type = 1; // Friendly
    potion.target_selection = 2; // Target
    potion.target_alive = true;
    potion.max_range = 3;
    potion.casting_time = 2000;
    potion.cooldown_time = 5000;
    potion.effects = vec![skill_effect(
        10130,
        EffectKind::SpecialEffect,
        POTION_EFFECT,
    )];
    s.add_skill(potion);
    d
}

struct Owner {
    client: TestClient,
    player: PlayerRef,
    mate: MateRef,
}

/// Player `n` at `x` with a summoned (not mounted) level-`level` mate standing at `mate_x`.
fn owner_with_pet(server: &Arc<GameServer>, n: u32, x: f32, mate_x: f32, level: u8) -> Owner {
    let mut client = TestClient::new(server, n, n);
    let (player, item_id) = player_with_item(server, &client, n, PA + n, (x, 100.0), MATE_ITEM);
    {
        let mut c = player.data();
        c.unit.level = 30;
        c.unit.hp = 5_000;
        c.unit.max_hp = 5_000;
    }
    client.conn.active_char = Some(player.clone());
    let now = chrono::Utc::now().naive_utc();
    server.systems.vehicles.mates.character(n, |cm| {
        cm.mates.push(mates::MateDb {
            id: 700 + n,
            item_id,
            name: "Pet".into(),
            xp: 0,
            level: 1,
            mileage: 0,
            hp: 10_000,
            mp: 10_000,
            owner: n,
            updated_at: now,
            created_at: now,
        });
    });
    mates::spawn_mount(server, &player, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(n)
        .pop()
        .expect("the mate spawned");
    {
        let mut m = mate.data();
        m.unit.level = level;
        m.unit.max_hp = 1_000;
        m.unit.hp = 500;
        m.unit.max_mp = 1_000;
        m.unit.mp = 1_000;
        m.unit.transform.set_xyz(mate_x, 100.0, 100.0);
    }
    client.drain();
    Owner {
        client,
        player,
        mate,
    }
}

fn hostile(server: &Arc<GameServer>, x: f32) -> NpcRef {
    let mut n = Npc::new(1234);
    n.unit.name = "Dummy".into();
    n.unit.level = 5;
    n.unit.hp = 1_000;
    n.unit.max_hp = 1_000;
    n.unit.transform.set_xyz(x, 100.0, 100.0);
    let npc = Entity::new(TARGET_OBJ, n);
    server
        .world
        .world(0)
        .expect("world")
        .spawn(&npc, &server.view_ctx());
    npc
}

fn pet_cast(caster: SkillCaster, skill_id: u32, target: SkillCastTarget) -> CSStartSkillPacket {
    CSStartSkillPacket {
        skill_id,
        caster,
        target,
        flag: 0,
        skill_object: SkillObject::None,
    }
}

async fn settle() {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(1_500)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
}

/// Casts and returns the `SCSkillStarted` result (if any) and the frames.
async fn cast(
    server: &Arc<GameServer>,
    o: &mut Owner,
    p: CSStartSkillPacket,
) -> (Option<u8>, Vec<(u16, Vec<u8>)>) {
    let skill = p.skill_id;
    start_skill(server, &mut o.client.conn, p);
    settle().await;
    let frames = o.client.drain();
    (started_result(&frames, skill), frames)
}

#[tokio::test(start_paused = true)]
async fn pet_unit_caster_basic_attack_hits_as_the_mate_when_the_owner_is_out_of_range() {
    let server = server_with(pet_data());
    crate::systems::skills::bind(&server);
    // The owner stands 60 m away, the pet 10 m from the target.
    let mut o = owner_with_pet(&server, 1, 100.0, 150.0, 5);
    let npc = hostile(&server, 160.0);
    let packet = pet_cast(
        SkillCaster::Unit(o.mate.obj_id),
        BASIC,
        SkillCastTarget::Unit(TARGET_OBJ),
    );
    let (result, frames) = cast(&server, &mut o, packet).await;
    assert_eq!(result, None, "{:x?}", ops(&frames));
    assert_eq!(npc.data().unit.hp, 900, "the pet's hit landed");
    assert!(
        !o.player.data().unit.is_auto_attack,
        "a pet cast never starts the owner's auto attack"
    );
    // The owner's own GCD was not used up by the pet's cast.
    assert_eq!(o.player.data().unit.skill_last_used_ms, 0);
}

#[tokio::test(start_paused = true)]
async fn pet_unit_caster_bar_skill_is_accepted_and_off_bar_skill_is_refused() {
    let server = server_with(pet_data());
    crate::systems::skills::bind(&server);
    let mut o = owner_with_pet(&server, 1, 100.0, 150.0, 5);
    let npc = hostile(&server, 160.0);
    let packet = pet_cast(
        SkillCaster::Unit(o.mate.obj_id),
        BAR_HIT,
        SkillCastTarget::Unit(TARGET_OBJ),
    );
    let (result, frames) = cast(&server, &mut o, packet).await;
    assert_eq!(result, None, "{:x?}", ops(&frames));
    assert_eq!(npc.data().unit.hp, 900);
    let packet = pet_cast(
        SkillCaster::Unit(o.mate.obj_id),
        NOT_ON_BAR,
        SkillCastTarget::Unit(TARGET_OBJ),
    );
    let (result, _) = cast(&server, &mut o, packet).await;
    assert_eq!(result, Some(skill_result::NO_PERM));
    assert_eq!(npc.data().unit.hp, 900, "the refused skill did nothing");
}

#[tokio::test(start_paused = true)]
async fn pet_unit_caster_of_someone_elses_mate_is_refused() {
    let server = server_with(pet_data());
    crate::systems::skills::bind(&server);
    let mut a = owner_with_pet(&server, 1, 100.0, 150.0, 5);
    let b = owner_with_pet(&server, 2, 120.0, 152.0, 5);
    let npc = hostile(&server, 160.0);
    let packet = pet_cast(
        SkillCaster::Unit(b.mate.obj_id),
        BASIC,
        SkillCastTarget::Unit(TARGET_OBJ),
    );
    let (result, _) = cast(&server, &mut a, packet).await;
    assert_eq!(result, Some(skill_result::NO_PERM));
    assert_eq!(npc.data().unit.hp, 1_000);
}

#[tokio::test(start_paused = true)]
async fn pet_skill_above_the_mate_level_is_refused_for_both_caster_types() {
    let server = server_with(pet_data());
    crate::systems::skills::bind(&server);
    let mut o = owner_with_pet(&server, 1, 100.0, 150.0, 5);
    let npc = hostile(&server, 160.0);
    let packet = pet_cast(
        SkillCaster::Unit(o.mate.obj_id),
        HIGH_LEVEL,
        SkillCastTarget::Unit(TARGET_OBJ),
    );
    let (result, _) = cast(&server, &mut o, packet).await;
    assert_eq!(result, Some(skill_result::NO_PERM));
    let mount = SkillCaster::Mount {
        obj_id: o.mate.obj_id,
        mount_skill_template_id: 32,
    };
    let packet = pet_cast(mount, HIGH_LEVEL, SkillCastTarget::Unit(TARGET_OBJ));
    let (result, _) = cast(&server, &mut o, packet).await;
    assert_eq!(result, Some(skill_result::NO_PERM));
    assert_eq!(npc.data().unit.hp, 1_000);
    // Once the pet reaches the level the same cast goes through.
    o.mate.data().unit.level = 30;
    let packet = pet_cast(
        SkillCaster::Unit(o.mate.obj_id),
        HIGH_LEVEL,
        SkillCastTarget::Unit(TARGET_OBJ),
    );
    let (result, frames) = cast(&server, &mut o, packet).await;
    assert_eq!(result, None, "{:x?}", ops(&frames));
    assert_eq!(npc.data().unit.hp, 900);
}

#[tokio::test(start_paused = true)]
async fn pet_heal_skill_heals_the_pet_for_both_caster_types() {
    let server = server_with(pet_data());
    crate::systems::skills::bind(&server);
    let mut o = owner_with_pet(&server, 1, 100.0, 150.0, 5);
    let packet = pet_cast(
        SkillCaster::Unit(o.mate.obj_id),
        BAR_HEAL,
        SkillCastTarget::Unit(o.mate.obj_id),
    );
    let (result, frames) = cast(&server, &mut o, packet).await;
    assert_eq!(result, None, "{:x?}", ops(&frames));
    assert_eq!(o.mate.data().unit.hp, 650, "Unit caster");
    assert_eq!(
        o.player.data().unit.hp,
        5_000,
        "the owner is not the caster"
    );
    let mount = SkillCaster::Mount {
        obj_id: o.mate.obj_id,
        mount_skill_template_id: 33,
    };
    let packet = pet_cast(mount, BAR_HEAL, SkillCastTarget::Unit(o.mate.obj_id));
    let (result, frames) = cast(&server, &mut o, packet).await;
    assert_eq!(result, None, "{:x?}", ops(&frames));
    assert_eq!(o.mate.data().unit.hp, 800, "Mount caster");
}

#[tokio::test(start_paused = true)]
async fn mate_state_values_from_four_up_are_ignored() {
    let server = server_with(pet_data());
    let o = owner_with_pet(&server, 1, 100.0, 150.0, 5);
    let tl = u32::from(o.mate.data().unit.tl_id);
    mates::change_state_mate(&server, &o.player, tl, 3);
    assert_eq!(o.mate.data().user_state, 3);
    mates::change_state_mate(&server, &o.player, tl, 4);
    mates::change_state_mate(&server, &o.player, tl, 255);
    assert_eq!(o.mate.data().user_state, 3);
    mates::change_state_mate(&server, &o.player, tl, 0);
    assert_eq!(o.mate.data().user_state, 0);
}

/// The real Brown Wolfhound (npc 10814, base skill 2) with its real skill rows: Health Regen
/// 17702 (`HealEffect` 290, `target_type` 0, level 25) heals the pet through both caster types,
/// and the basic attack 2 hits from the mate with the owner far away.
#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_wolfhound_health_regen_heals_and_basic_attack_hits() {
    let Ok(db) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    const WOLFHOUND: u32 = 10_814;
    const HEALTH_REGEN: u32 = 17_702;
    let server = super::mate_dash_skill_tests::real_pet_server(&db, WOLFHOUND);
    let mut o = owner_with_pet(&server, 1, 100.0, 150.0, 30);
    let regen_row = server
        .game_data
        .systems
        .vehicles
        .mount_skill_id_for_skill(HEALTH_REGEN);
    assert_ne!(regen_row, 0);
    println!("mate skills {:?}", o.mate.data().skills);
    let mate_id = o.mate.obj_id;
    for (what, caster) in [
        (
            "Mount",
            SkillCaster::Mount {
                obj_id: mate_id,
                mount_skill_template_id: regen_row,
            },
        ),
        ("Unit", SkillCaster::Unit(mate_id)),
    ] {
        o.mate.data().unit.hp = 100;
        o.mate.data().unit.max_hp = 1_000;
        // The skill's 18 s cooldown is on the mate: forget the first cast.
        {
            let mut m = o.mate.data();
            m.unit.cooldowns = Default::default();
            m.unit.skill_last_used_ms = 0;
            m.unit.global_cooldown_ms = 0;
        }
        let packet = pet_cast(caster, HEALTH_REGEN, SkillCastTarget::Unit(mate_id));
        let (result, frames) = cast(&server, &mut o, packet).await;
        assert_eq!(result, None, "{what}: {:x?}", ops(&frames));
        assert!(
            o.mate.data().unit.hp > 100,
            "{what}: the pet was healed: {:x?}",
            ops(&frames)
        );
    }
    let npc = hostile(&server, 160.0);
    {
        let mut m = o.mate.data();
        m.unit.skill_last_used_ms = 0;
        m.unit.global_cooldown_ms = 0;
    }
    let packet = pet_cast(
        SkillCaster::Unit(mate_id),
        2,
        SkillCastTarget::Unit(TARGET_OBJ),
    );
    let (result, frames) = cast(&server, &mut o, packet).await;
    assert_eq!(result, None, "{:x?}", ops(&frames));
    assert!(npc.data().unit.hp < 1_000, "the pet's basic attack landed");
}

/// Item 18649 "Pet Dual-Recovery Potion" cast through the real item -> skill -> effect chain
/// heals the owner's summoned mate (value1/value2 = 20 % of the pools), mounted or not, and
/// broadcasts the mate's `SCUnitPoints`. A cast aimed at the owner itself is a valid use of the
/// skill but has no mate to heal: the effect does nothing (the skill is Friendly / Target, and
/// only the mate carries the HealPet effect's target kind).
#[tokio::test(start_paused = true)]
async fn pet_dual_recovery_potion_heals_the_summoned_mate() {
    let mut case = 0u64;
    for mounted in [false, true] {
        for target_self in [false, true] {
            let server = server_with(pet_data());
            crate::systems::skills::bind(&server);
            crate::systems::economy::bridges::install(&server);
            let mut o = owner_with_pet(&server, 1, 100.0, 101.0, 5);
            // The item caster resolves its source through the item manager's containers.
            server
                .items
                .add_container(crate::systems::economy::inventory::Inv::of(&o.player).bag());
            {
                let mut m = o.mate.data();
                m.unit.hp = 500;
                m.unit.max_hp = 1_000;
                m.unit.mp = 500;
                m.unit.max_mp = 1_000;
            }
            if mounted {
                let tl = u32::from(o.mate.data().unit.tl_id);
                mates::mount_mate(&server, &o.player, tl, attach_point::DRIVER, 1);
                assert!(o.player.data().is_riding);
            }
            let potion = give_item(&server, &o.player, 0x7200 + case, POTION_ITEM);
            case += 1;
            o.client.drain();
            let target = if target_self {
                SkillCastTarget::Unit(o.player.obj_id)
            } else {
                SkillCastTarget::Unit(o.mate.obj_id)
            };
            let packet = pet_cast(
                SkillCaster::Item {
                    obj_id: o.player.obj_id,
                    item_id: potion,
                    item_template_id: POTION_ITEM,
                    type1: 0,
                    type2: 0,
                },
                POTION_SKILL,
                target,
            );
            let (result, mut frames) = cast(&server, &mut o, packet).await;
            // The potion's real 2 s cast: the first settle only covers 1.5 s.
            settle().await;
            frames.extend(o.client.drain());
            let case = format!("mounted {mounted}, target_self {target_self}");
            assert_eq!(result, None, "{case}: {:x?}", ops(&frames));
            assert!(
                ops(&frames).contains(&sc::SCSkillFiredPacket),
                "{case}: the cast finished: {:x?}",
                ops(&frames)
            );
            let (hp, mp) = {
                let m = o.mate.data();
                (m.unit.hp, m.unit.mp)
            };
            if target_self {
                assert_eq!((hp, mp), (500, 500), "{case}: the owner is not a mate");
                assert!(
                    !ops(&frames).contains(&sc::SCUnitPointsPacket),
                    "{case}: no mate points packet: {:x?}",
                    ops(&frames)
                );
                continue;
            }
            assert_eq!((hp, mp), (700, 700), "{case}");
            let expected = crate::packets::world::SCUnitPointsPacket {
                obj_id: o.mate.obj_id,
                hp: 700,
                mp: 700,
            };
            assert_eq!(
                body(&frames, sc::SCUnitPointsPacket),
                crate::net::packet::encode(&expected)[6..].to_vec(),
                "{case}: the mate's SCUnitPoints carries the healed pools"
            );
        }
    }
}
