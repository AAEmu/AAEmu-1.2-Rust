//! Authored 1.2 breaker relations. IDs/tags below are pinned again by the ignored compact
//! regression. No packet round trips: BuffRemoved's native reader is 397C6360, handler
//! 391D5E90 (x2game12.c:1763754-1763769 / 382144-382148): 3-byte owner, u32 buff index.
//! Relation loaders: native 1.2 395D17B0 (x2game12.c:1256462-1256496), 10.x 39B41DE0
//! (x2game-dev_dedicate.c:2323838-2323904). Those prove ownership/direction only. Traced
//! 10.x ZoneBuffMan::Create 39450740 and its tick 3944FC90 do not establish a breaker-set
//! consumer. Immediate accepted-state reconciliation is the server compatibility policy;
//! exact retail apply-vs-tick timing remains a native-parity limitation.

use std::sync::Arc;
use std::time::Duration;

use crate::data::GameData;
use crate::models::buff::Buff;
use crate::models::npc::Npc;
use crate::models::unit::{
    BonusTemplate, BuffAttribute, BuffModifier, CombatBuffTemplate, SkillAttribute, SkillModifier,
    UnitAttribute, UnitModifierType,
};
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{now_ms, Entity, MateRef, WorldObject};

use super::super::controllers::{self, ScState};
use super::super::data::effects::{BuffEffect, DispelEffect};
use super::super::data::{
    BuffTemplate, BuffTolerance, BuffTriggerTemplate, EffectKind, EffectTemplate, SkillTemplate,
    TickEffect,
};
use super::super::state::{
    CancelToken, SkillRef, SkillRun, SkillTaskKind, SkillTaskSlot, ToleranceCounter,
};
use super::super::types::{BuffEventTriggerKind, SkillCaster};

const SONGS: [u32; 4] = [656, 662, 667, 778];
const GLIDER: u32 = 1029;
const STUN: u32 = 93;
const SLEEP: u32 = 84; // Leech: Sleep tag 97, despite its stun flag in the export.
const SILENCE: u32 = 245;
const IRRELEVANT: u32 = 60_001;
const IMMUNITY: u32 = 60_002;
const ORIGINAL_TAG: u32 = 60_030;
const MARKER: u32 = 60_031;
const MARKER_TAG: u32 = 60_032;
const TRIGGER_EFFECT: u32 = 60_033;
const MODIFIER_TARGET: u32 = 60_034;

fn data() -> GameData {
    let mut data = GameData::empty();
    let d = &mut data.systems.skills;
    for id in SONGS {
        d.add_buff(BuffTemplate {
            id,
            kind: 3,
            duration: 15_000,
            tick: 500.0,
            stack_rule: 1,
            max_stack: 1,
            bonuses: vec![BonusTemplate {
                attribute: UnitAttribute::Str,
                modifier_type: UnitModifierType::Value,
                value: 100,
                linear_level_bonus: 0,
            }],
            ..BuffTemplate::default()
        });
        d.buff_breakers
            .insert(id, vec![6, 12, 13, 97, 99, 107, 150, 346]);
    }
    d.add_buff(BuffTemplate {
        id: GLIDER,
        kind: 1,
        duration: 180_000,
        gliding: true,
        stack_rule: 1,
        max_stack: 1,
        ..BuffTemplate::default()
    });
    d.buff_breakers
        .insert(GLIDER, vec![6, 12, 13, 27, 97, 99, 107, 150, 346]);
    d.buff_tags.insert(GLIDER, vec![10, 294]);
    for (id, duration) in [(431, 15_000), (2170, 10_000)] {
        d.add_buff(BuffTemplate {
            id,
            kind: 1,
            duration,
            ..BuffTemplate::default()
        });
        d.buff_breakers.insert(id, vec![294]);
    }
    for (id, tag, duration, stun, silence) in [
        (STUN, 6, 4000, true, false),
        (SLEEP, 97, 5000, true, false),
        (SILENCE, 99, 3500, false, true),
        (IRRELEVANT, 60_000, 10_000, true, false),
    ] {
        d.add_buff(BuffTemplate {
            id,
            kind: 2,
            duration,
            stun,
            silence,
            stack_rule: 1,
            max_stack: 1,
            ..BuffTemplate::default()
        });
        d.buff_tags.insert(id, vec![tag]);
        d.tagged_buffs.insert(tag, vec![id]);
    }
    data
}

fn fixture(data: GameData) -> (Arc<GameServer>, TestClient, WorldObject) {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let owner = spawn_player(
        &server,
        &client,
        test_character(1, "Bard", 100.0, 100.0),
        0x300,
    );
    // Visibility serialization can add its synthetic state buffs. Start this fixture's
    // bookkeeping at index 2 so the native removal-body expectations remain literal.
    owner.data().unit.buffs = crate::models::buff::Buffs::default();
    (server, client, WorldObject::from(&owner))
}

fn add(server: &GameServer, owner: &WorldObject, id: u32) {
    super::add_buff_by_id(server, owner, id, owner, 0);
}

fn has(owner: &WorldObject, id: u32) -> bool {
    owner.with_base_unit(|u| u.buffs().check_buff(id))
}

fn removed(client: &mut TestClient) -> Vec<Vec<u8>> {
    client
        .drain()
        .into_iter()
        .filter_map(|(op, body)| (op == sc::SCBuffRemovedPacket).then_some(body))
        .collect()
}

// Unit-caster Created prefix from native 397CA670 -> 397C85B0 / 397C9070:
// caster kind+bc, caster DB id, owner bc, index u32, buff id u32. Removed: owner bc,index.
// Read the emitted notices; expected IDs/order below are independent literals.
fn notices(client: &mut TestClient) -> Vec<(u16, u32, u32)> {
    client
        .drain()
        .into_iter()
        .filter_map(|(op, body)| {
            if op == sc::SCBuffCreatedPacket {
                assert_eq!(body[0], 0, "fixture uses unit casters");
                Some((
                    op,
                    u32::from_le_bytes(body[15..19].try_into().unwrap()),
                    u32::from_le_bytes(body[11..15].try_into().unwrap()),
                ))
            } else if op == sc::SCBuffRemovedPacket {
                assert_eq!(body.len(), 7);
                Some((op, 0, u32::from_le_bytes(body[3..7].try_into().unwrap())))
            } else {
                None
            }
        })
        .collect()
}

fn trigger(d: &mut GameData, kind: BuffEventTriggerKind, effect: (EffectKind, u32), no_tag: u32) {
    d.systems
        .skills
        .buff_triggers
        .entry(SONGS[0])
        .or_default()
        .push(BuffTriggerTemplate {
            id: effect.1,
            kind: kind as u16,
            effect_on_source: false,
            effect,
            use_damage_amount: false,
            use_original_source: false,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: no_tag,
            synergy: false,
        });
}

fn trigger_buff(
    d: &mut GameData,
    kind: BuffEventTriggerKind,
    effect_id: u32,
    buff_id: u32,
    no_tag: u32,
) {
    d.systems.skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: effect_id,
            buff_id: Some(buff_id),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        0,
    );
    trigger(d, kind, (EffectKind::BuffEffect, effect_id), no_tag);
}

fn reentrant_data() -> GameData {
    let mut d = data();
    let skills = &mut d.systems.skills;
    let original = Arc::make_mut(skills.buffs.get_mut(&SONGS[0]).unwrap());
    original.stun = true; // Would unmount/interrupt if the removed Started hook resumed.
    original.gliding = true; // Would start a controller for non-character owners.
    original.tick = 10.0;
    original.tick_effects = vec![TickEffect {
        effect_id: TRIGGER_EFFECT + 1,
        target_buff_tag_id: 0,
        target_no_buff_tag_id: 0,
    }];
    skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: TRIGGER_EFFECT + 1,
            buff_id: Some(MARKER),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        TRIGGER_EFFECT + 1,
    );
    Arc::make_mut(skills.buffs.get_mut(&IRRELEVANT).unwrap()).stun = false;
    skills.buff_tags.insert(IRRELEVANT, vec![6]);
    skills.tagged_buffs.insert(6, vec![STUN, IRRELEVANT]);
    skills.buff_tags.insert(SONGS[0], vec![ORIGINAL_TAG]);
    skills.tagged_buffs.insert(ORIGINAL_TAG, vec![SONGS[0]]);
    skills.add_buff(BuffTemplate {
        id: MARKER,
        kind: 1,
        ..Default::default()
    });
    skills.buff_tags.insert(MARKER, vec![MARKER_TAG]);
    skills.tagged_buffs.insert(MARKER_TAG, vec![MARKER]);
    skills.skill_modifiers.insert(
        SONGS[0],
        vec![SkillModifier {
            id: MODIFIER_TARGET,
            owner_id: SONGS[0],
            tag_id: 0,
            skill_attribute: SkillAttribute::CastTime,
            unit_modifier_type: UnitModifierType::Value,
            value: 17,
            skill_id: MODIFIER_TARGET,
            synergy: false,
        }],
    );
    skills.buff_game_data.buff_modifiers.insert(
        SONGS[0],
        vec![BuffModifier {
            id: MODIFIER_TARGET,
            owner_id: SONGS[0],
            tag_id: 0,
            buff_attribute: BuffAttribute::Duration,
            unit_modifier_type: UnitModifierType::Value,
            value: 23,
            buff_id: MODIFIER_TARGET,
            synergy: false,
        }],
    );
    skills.combat_buffs.insert(
        SONGS[0],
        vec![CombatBuffTemplate {
            id: MODIFIER_TARGET,
            hit_skill_id: 0,
            hit_type: 1,
            req_skill_id: 0,
            buff_id: MARKER,
            buff_from_source: false,
            buff_to_source: false,
            req_buff_id: SONGS[0],
            is_heal_spell: false,
        }],
    );
    trigger_buff(
        &mut d,
        BuffEventTriggerKind::Started,
        TRIGGER_EFFECT,
        IRRELEVANT,
        MARKER_TAG,
    );
    d
}

fn channel(server: &GameServer, owner: &WorldObject, matching: bool) -> (SkillRef, CancelToken) {
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            id: MODIFIER_TARGET,
            channeling_buff_id: if matching {
                SONGS[0]
            } else {
                MODIFIER_TARGET + 1
            },
            channeling_time: 30_000,
            ..Default::default()
        }),
        1,
    );
    skill.state().tl_id = server
        .systems
        .skills
        .tl_ids
        .next_id(owner.obj_id(), skill.template.id);
    let cancel = CancelToken::new();
    owner.with_unit(|u| {
        u.unit_mut().skill_state.skill_task = Some(SkillTaskSlot {
            kind: SkillTaskKind::EndChanneling { channel_doodad: 0 },
            skill: skill.clone(),
            cancel: cancel.clone(),
        });
    });
    (skill, cancel)
}

fn queue_original(server: &GameServer, owner: &WorldObject, skill: Option<&SkillRef>) -> Buff {
    let template = server.game_data.systems.skills.buff(SONGS[0]).unwrap();
    let mut b = super::new_buff(
        server,
        template,
        Some(owner),
        &SkillCaster::Unit(owner.obj_id()),
        skill.map(|s| s.as_ref()),
        now_ms(),
    );
    b.duration = template.duration;
    owner.with_base_unit(|u| u.buffs_mut().add_buff_with_duration(b, 0, now_ms()));
    super::get_buff(owner, 2).unwrap()
}

fn assert_no_original_state(server: &GameServer, owner: &WorldObject, original: &Buff) {
    assert!(!has(owner, SONGS[0]));
    assert!(!super::check_buff_tag(server, owner, ORIGINAL_TAG));
    assert!(controllers::active(owner).is_none());
    assert!(!owner.with_base_unit(|u| u.buffs().has_events()));
    let (skill_mod, buff_mod, bonus, combat) = owner
        .with_unit(|u| {
            let u = u.unit();
            (
                u.skill_modifiers
                    .apply(MODIFIER_TARGET, &[], SkillAttribute::CastTime, 0.0),
                u.buff_modifiers
                    .apply(MODIFIER_TARGET, &[], BuffAttribute::Duration, 0.0),
                u.bonuses.calculate(0.0, UnitAttribute::Str, now_ms()),
                u.combat_buffs
                    .by_hit_type
                    .values()
                    .map(Vec::len)
                    .sum::<usize>(),
            )
        })
        .unwrap();
    assert_eq!((skill_mod, buff_mod, bonus, combat), (0.0, 0.0, 0.0, 0));
    assert!(!server
        .systems
        .skills
        .triggers
        .lock()
        .unwrap()
        .iter()
        .any(|s| { s.owner_obj == owner.obj_id() && s.buff_instance_id == original.instance_id }));
    super::dispel_task(server, owner.obj_id(), original.index, original.task_gen);
    assert!(!has(owner, SONGS[0]));
}

fn npc_owner(server: &GameServer) -> WorldObject {
    let mut npc = Npc::new(1);
    npc.unit.hp = 100;
    npc.unit.max_hp = 100;
    npc.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let owner = WorldObject::from(Entity::new(0x400, npc));
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&owner, &server.view_ctx());
    owner
}

fn mount_data(d: &mut GameData) {
    use crate::data::items::{ItemTemplate, TemplateKind};
    use crate::systems::npcs::npc::data::NpcTemplate;
    d.experience = crate::test_support::test_experience(55);
    d.formulas = crate::systems::vehicles::stats::tests::formulas(
        &[(1, "1000", 14, 5), (2, "100", 15, 5)],
        &[],
    );
    d.systems.npcs.npc.templates.insert(
        60_090,
        NpcTemplate {
            id: 60_090,
            name: "Horse".into(),
            level: 1,
            model_id: 77,
            scale: 1.0,
            ..Default::default()
        },
    );
    d.items.templates.insert(
        60_091,
        ItemTemplate {
            kind: TemplateKind::SummonMate { npc_id: 60_090 },
            ..ItemTemplate::plain(60_091, 1)
        },
    );
}

fn mount(server: &GameServer, owner: &WorldObject) -> MateRef {
    use crate::models::container::slot_type;
    use crate::models::item::Item;
    use crate::systems::vehicles::{data::attach_point, mates};
    let player = owner.as_player().unwrap();
    let template = server.game_data.items.template(60_091).unwrap();
    let mut item = Item::create(0x7001, template, 1, 0, 1, &server.game_data.items, 0);
    item.slot_type = slot_type::INVENTORY;
    item.slot = 0;
    let bag = player
        .data()
        .inventory
        .get(slot_type::INVENTORY)
        .cloned()
        .unwrap();
    bag.lock().unwrap().items.push(item);
    mates::spawn_mount(server, player, 0x7001);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(player.character_id)
        .pop()
        .unwrap();
    let tl = u32::from(mate.data().unit.tl_id);
    mates::mount_mate(server, player, tl, attach_point::DRIVER, 1);
    assert!(player.data().is_riding);
    mate
}

#[tokio::test]
async fn buff_breakers_bard_songs_end_on_real_stun_sleep_and_silence_tags() {
    for cc in [STUN, SLEEP, SILENCE] {
        for song in SONGS {
            let (server, mut client, owner) = fixture(data());
            add(&server, &owner, song);
            let song_buff = owner.with_base_unit(|u| u.buffs().effects[0].clone());
            client.drain();
            add(&server, &owner, cc);
            assert!(!has(&owner, song), "song {song}, CC {cc}");
            assert!(has(&owner, cc));
            assert_eq!(removed(&mut client), vec![vec![0, 3, 0, 2, 0, 0, 0]]);
            assert_eq!(
                owner.with_unit(|u| u
                    .unit()
                    .bonuses
                    .calculate(0.0, UnitAttribute::Str, now_ms())),
                Some(0.0)
            );
            // A queued song timer may no longer tick, resurrect, or remove twice.
            super::dispel_task(&server, owner.obj_id(), song_buff.index, song_buff.task_gen);
            assert!(removed(&mut client).is_empty());
            assert!(!has(&owner, song));
        }
    }
}

#[tokio::test]
async fn buff_breakers_glider_controller_stops_through_normal_dispel() {
    let (server, _client, _player) = fixture(data());
    let mut npc = Npc::new(1);
    npc.unit.hp = 100;
    npc.unit.max_hp = 100;
    npc.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let npc = Entity::new(0x400, npc);
    let owner = WorldObject::from(&npc);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&owner, &server.view_ctx());
    add(&server, &owner, GLIDER);
    let controller = controllers::active(&owner).unwrap();
    assert_eq!(controller.source_buff_id(), GLIDER);
    assert_eq!(controller.state(), ScState::Running);
    add(&server, &owner, SILENCE);
    assert!(!has(&owner, GLIDER));
    assert_eq!(controller.state(), ScState::Ended);
    assert!(controllers::active(&owner).is_none());
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert_eq!(npc.data().unit.transform.z, 100.0);
}

#[tokio::test]
async fn buff_breakers_ignore_irrelevant_tags_and_rejected_incoming_buffs() {
    let mut d = data();
    d.systems.skills.add_buff(BuffTemplate {
        id: IMMUNITY,
        kind: 1,
        immune_buff_tag_id: 99,
        ..BuffTemplate::default()
    });
    let (server, mut client, owner) = fixture(d);
    add(&server, &owner, SONGS[0]);
    add(&server, &owner, IRRELEVANT);
    assert!(has(&owner, SONGS[0]), "CC flags alone are not breaker tags");
    add(&server, &owner, IMMUNITY);
    client.drain();
    let silence = server.game_data.systems.skills.buff(SILENCE).unwrap();
    super::apply_template(
        &server,
        silence,
        Some(&owner),
        &SkillCaster::Unit(owner.obj_id()),
        &owner,
        None,
    );
    assert!(has(&owner, SONGS[0]));
    assert!(!has(&owner, SILENCE));
    assert!(removed(&mut client).is_empty());

    let mut d = data();
    Arc::make_mut(d.systems.skills.buffs.get_mut(&SILENCE).unwrap()).require_buff_id = 60_004;
    let (server, mut client, owner) = fixture(d);
    add(&server, &owner, SONGS[0]);
    client.drain();
    let rejected = server.game_data.systems.skills.buff(SILENCE).unwrap();
    super::apply_template(
        &server,
        rejected,
        Some(&owner),
        &SkillCaster::Unit(owner.obj_id()),
        &owner,
        None,
    );
    assert!(has(&owner, SONGS[0]));
    assert!(!has(&owner, SILENCE));
    assert!(removed(&mut client).is_empty());
}

#[tokio::test]
async fn buff_breakers_tolerance_rejection_does_not_end_song() {
    let mut d = data();
    d.systems.skills.buff_game_data.tolerances.insert(
        6,
        BuffTolerance {
            id: 1,
            buff_tag_id: 6,
            step_duration: 60,
            final_step_buff_id: IMMUNITY,
            character_time_reduction: 0,
            steps: vec![(1, 100, 0)],
        },
    );
    let (server, mut client, owner) = fixture(d);
    owner.with_unit(|u| {
        u.unit_mut().skill_state.tolerance_counters.insert(
            1,
            ToleranceCounter {
                tolerance_id: 1,
                current_step: (1, 100, 0),
                last_step_ms: now_ms(),
            },
        );
    });
    add(&server, &owner, SONGS[0]);
    client.drain();
    add(&server, &owner, STUN); // No next tolerance step: existing rejection path.
    assert!(!has(&owner, STUN));
    assert!(has(&owner, SONGS[0]));
    assert!(removed(&mut client).is_empty());
}

#[tokio::test]
async fn buff_breakers_cancel_only_the_corresponding_channel_without_reentering_removal() {
    let mut d = data();
    // Snare tag 27 is authored for gliders, without the generic stun/silence interrupt.
    Arc::make_mut(d.systems.skills.buffs.get_mut(&IRRELEVANT).unwrap()).stun = false;
    d.systems.skills.buff_tags.insert(IRRELEVANT, vec![27]);
    let (server, mut client, owner) = fixture(d);
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            id: 60_010,
            channeling_buff_id: GLIDER,
            channeling_time: 10_000,
            ..SkillTemplate::default()
        }),
        1,
    );
    skill.state().tl_id = server
        .systems
        .skills
        .tl_ids
        .next_id(owner.obj_id(), skill.template.id);
    let cancel = CancelToken::new();
    owner.with_unit(|u| {
        u.unit_mut().skill_state.skill_task = Some(SkillTaskSlot {
            kind: SkillTaskKind::EndChanneling { channel_doodad: 0 },
            skill: skill.clone(),
            cancel: cancel.clone(),
        });
    });
    let template = server.game_data.systems.skills.buff(GLIDER).unwrap();
    let buff = super::new_buff(
        &server,
        template,
        Some(&owner),
        &SkillCaster::Unit(owner.obj_id()),
        Some(&skill),
        now_ms(),
    );
    super::add_buff(&server, &owner, buff, 0, 0);
    client.drain();
    add(&server, &owner, IRRELEVANT);
    assert!(!has(&owner, GLIDER));
    assert!(cancel.is_cancelled());
    assert!(skill.cancelled());
    assert!(owner
        .with_unit(|u| u.unit().skill_state.skill_task.is_none())
        .unwrap());
    assert_eq!(server.systems.skills.tl_ids.in_use(), 0);
    assert_eq!(removed(&mut client), vec![vec![0, 3, 0, 2, 0, 0, 0]]);
}

#[tokio::test]
async fn buff_breakers_timer_applied_cc_uses_the_same_acceptance_path() {
    let mut d = data();
    d.systems.skills.add_buff(BuffTemplate {
        id: 60_020,
        kind: 3,
        duration: 10_000,
        tick: 10.0,
        tick_effects: vec![TickEffect {
            effect_id: 60_021,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
        }],
        ..BuffTemplate::default()
    });
    d.systems.skills.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: 60_021,
            buff_id: Some(SILENCE),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        60_021,
    );
    let (server, mut client, owner) = fixture(d);
    add(&server, &owner, SONGS[0]);
    add(&server, &owner, 60_020);
    client.drain();
    for _ in 0..40 {
        if !has(&owner, SONGS[0]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(has(&owner, SILENCE));
    assert!(!has(&owner, SONGS[0]));
    assert_eq!(removed(&mut client), vec![vec![0, 3, 0, 2, 0, 0, 0]]);
}

#[tokio::test]
async fn buff_breakers_multiple_tags_remove_each_song_once_in_index_order() {
    let mut d = data();
    d.systems.skills.buff_tags.insert(STUN, vec![99, 6, 97, 6]);
    let (server, mut client, owner) = fixture(d);
    for song in SONGS {
        add(&server, &owner, song);
    }
    client.drain();
    add(&server, &owner, STUN);
    for song in SONGS {
        assert!(!has(&owner, song));
    }
    assert_eq!(
        removed(&mut client),
        vec![
            vec![0, 3, 0, 2, 0, 0, 0],
            vec![0, 3, 0, 3, 0, 0, 0],
            vec![0, 3, 0, 4, 0, 0, 0],
            vec![0, 3, 0, 5, 0, 0, 0],
        ]
    );
}

#[tokio::test]
async fn buff_breakers_mutual_batch_removal_does_not_start_a_stale_buff_event() {
    let mut d = data();
    for id in [SONGS[0], GLIDER] {
        d.systems.skills.buff_tags.insert(id, vec![6]);
    }
    let (server, mut client, owner) = fixture(d);
    for id in [SONGS[0], GLIDER] {
        let template = server.game_data.systems.skills.buff(id).unwrap();
        let mut buff = super::new_buff(
            &server,
            template,
            Some(&owner),
            &SkillCaster::Unit(owner.obj_id()),
            None,
            now_ms(),
        );
        buff.duration = template.duration;
        owner.with_base_unit(|u| u.buffs_mut().add_buff_with_duration(buff, 0, now_ms()));
    }
    client.drain();
    crate::systems::dispatch_buff_events(&server, &owner);
    assert!(!has(&owner, SONGS[0]));
    assert!(!has(&owner, GLIDER));
    assert_eq!(
        removed(&mut client),
        vec![vec![0, 3, 0, 2, 0, 0, 0], vec![0, 3, 0, 3, 0, 0, 0]]
    );
    assert!(controllers::active(&owner).is_none());
}

#[tokio::test]
async fn buff_breakers_existing_cc_prevents_restarting_a_song() {
    let (server, _client, owner) = fixture(data());
    add(&server, &owner, SILENCE);
    add(&server, &owner, SONGS[0]);
    assert!(has(&owner, SILENCE));
    assert!(!has(&owner, SONGS[0]));
    assert_eq!(
        owner.with_unit(|u| u
            .unit()
            .bonuses
            .calculate(0.0, UnitAttribute::Str, now_ms())),
        Some(0.0)
    );
}

#[tokio::test]
async fn buff_breakers_gliding_tag_ends_frigid_tracks_and_deadeye() {
    let (server, mut client, owner) = fixture(data());
    add(&server, &owner, 431);
    add(&server, &owner, 2170);
    client.drain();
    add(&server, &owner, GLIDER);
    assert!(!has(&owner, 431));
    assert!(!has(&owner, 2170));
    assert!(has(&owner, GLIDER));
    assert_eq!(
        removed(&mut client),
        vec![vec![0, 3, 0, 2, 0, 0, 0], vec![0, 3, 0, 3, 0, 0, 0]]
    );
}

#[test]
fn buff_breakers_loader_preserves_direction_and_deduplicates_tags() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE buff_breakers (buff_id, buff_tag_id); INSERT INTO buff_breakers VALUES (656,99),(656,6),(656,6),(1029,27);").unwrap();
    let breakers = super::super::data::load_buff_breakers(&conn).unwrap();
    assert_eq!(breakers[&656], vec![6, 99]);
    assert_eq!(breakers[&1029], vec![27]);
    assert!(!breakers.contains_key(&6));
}

#[tokio::test]
async fn buff_breakers_started_trigger_cc_does_not_resume_removed_buff_or_disturb_mount() {
    use crate::systems::vehicles::data::attach_point;
    for matching in [false, true] {
        let mut d = reentrant_data();
        mount_data(&mut d);
        let (server, mut client, owner) = fixture(d);
        let mate = mount(&server, &owner);
        // Mount visibility may serialize state buffs. Keep this buff transaction isolated.
        owner.with_base_unit(|u| *u.buffs_mut() = Default::default());
        let (skill, cancel) = channel(&server, &owner, matching);
        let original = queue_original(&server, &owner, Some(&skill));
        client.drain();
        crate::systems::dispatch_buff_events(&server, &owner);
        assert_no_original_state(&server, &owner, &original);
        assert!(has(&owner, IRRELEVANT));
        assert!(!has(&owner, MARKER));
        assert_eq!(cancel.is_cancelled(), matching);
        assert_eq!(skill.cancelled(), matching);
        assert_eq!(server.systems.skills.tl_ids.in_use(), u32::from(!matching));
        assert_eq!(
            owner.with_unit(|u| u.unit().skill_state.skill_task.is_none()),
            Some(matching)
        );
        let player = owner.as_player().unwrap();
        assert!(
            player.data().is_riding,
            "removed buff must not reach its unmount step"
        );
        assert_eq!(player.data().unit.transform.parent_obj_id, mate.obj_id);
        assert_eq!(
            mate.data().passenger(attach_point::DRIVER).unwrap().obj_id,
            owner.obj_id()
        );
        assert_eq!(
            notices(&mut client),
            vec![
                (sc::SCBuffCreatedPacket, IRRELEVANT, 3),
                (sc::SCBuffRemovedPacket, 0, 2),
            ]
        );
        if !matching {
            super::super::skill::stop(&server, &skill, &owner, 0);
        }
        assert_eq!(server.systems.skills.tl_ids.in_use(), 0);
    }
}

#[tokio::test]
async fn buff_breakers_started_trigger_cc_cleans_npc_controller_and_timer_paths() {
    let (server, mut client, _player) = fixture(reentrant_data());
    let owner = npc_owner(&server);
    let (skill, cancel) = channel(&server, &owner, true);
    let original = queue_original(&server, &owner, Some(&skill));
    client.drain();
    crate::systems::dispatch_buff_events(&server, &owner);
    assert_no_original_state(&server, &owner, &original);
    assert!(cancel.is_cancelled());
    assert_eq!(server.systems.skills.tl_ids.in_use(), 0);
    assert_eq!(
        notices(&mut client),
        vec![
            (sc::SCBuffCreatedPacket, IRRELEVANT, 3),
            (sc::SCBuffRemovedPacket, 0, 2),
        ]
    );
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert!(!has(&owner, MARKER), "removed tick must not run");
    assert_eq!(owner.transform().z, 100.0);
    assert!(controllers::active(&owner).is_none());
    assert!(notices(&mut client).is_empty());
}

#[tokio::test]
async fn buff_breakers_started_self_dispel_finishes_even_without_a_breaker_relation() {
    let mut d = reentrant_data();
    // An unknown faction is attackable even against itself, so a friendly self-cure
    // needs the same known faction as a live character.
    d.factions.add(crate::data::world_data::SystemFaction {
        id: 101,
        ..Default::default()
    });
    d.systems.skills.buff_breakers.clear();
    d.systems.skills.buff_triggers.clear();
    // Also cover the permanent/non-ticking start branch, which must not turn a deferred
    // explicit exit back into ACTING after its own Started trigger dispels it.
    let original = Arc::make_mut(d.systems.skills.buffs.get_mut(&SONGS[0]).unwrap());
    original.kind = 2; // Model the friendly self-cure fixture as a bad buff.
    original.duration = 0;
    original.tick = 0.0;
    original.tick_effects.clear();
    d.systems.skills.add_effect(
        EffectKind::DispelEffect,
        EffectTemplate::Dispel(DispelEffect {
            id: TRIGGER_EFFECT,
            dispel_count: 0,
            cure_count: 1,
            buff_tag_id: ORIGINAL_TAG,
        }),
        0,
    );
    trigger(
        &mut d,
        BuffEventTriggerKind::Started,
        (EffectKind::DispelEffect, TRIGGER_EFFECT),
        0,
    );
    let (server, mut client, owner) = fixture(d);
    let (skill, cancel) = channel(&server, &owner, false);
    let original = queue_original(&server, &owner, Some(&skill));
    client.drain();
    crate::systems::dispatch_buff_events(&server, &owner);
    assert_no_original_state(&server, &owner, &original);
    assert!(
        !cancel.is_cancelled(),
        "self-dispel must not resume the stun interrupt"
    );
    assert!(!has(&owner, IRRELEVANT));
    assert_eq!(notices(&mut client), vec![(sc::SCBuffRemovedPacket, 0, 2)]);
    super::super::skill::stop(&server, &skill, &owner, 0);
}

#[tokio::test]
async fn buff_breakers_started_mutual_breakers_keep_dispelled_callback_and_cleanup_order() {
    let mut d = reentrant_data();
    d.systems
        .skills
        .buff_breakers
        .insert(IRRELEVANT, vec![ORIGINAL_TAG]);
    Arc::make_mut(d.systems.skills.buffs.get_mut(&IRRELEVANT).unwrap()).gliding = true;
    trigger_buff(
        &mut d,
        BuffEventTriggerKind::Dispelled,
        TRIGGER_EFFECT + 2,
        MARKER,
        0,
    );
    let (server, mut client, _player) = fixture(d);
    let owner = npc_owner(&server);
    let original = queue_original(&server, &owner, None);
    client.drain();
    crate::systems::dispatch_buff_events(&server, &owner);
    assert_no_original_state(&server, &owner, &original);
    assert!(!has(&owner, IRRELEVANT));
    assert!(
        has(&owner, MARKER),
        "Dispelled callback still runs during nested removal"
    );
    assert!(!super::check_buff_tag(&server, &owner, 6));
    assert_eq!(
        notices(&mut client),
        vec![
            (sc::SCBuffCreatedPacket, IRRELEVANT, 3),
            (sc::SCBuffCreatedPacket, MARKER, 4),
            (sc::SCBuffRemovedPacket, 0, 2),
            (sc::SCBuffRemovedPacket, 0, 3),
        ]
    );
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert_eq!(owner.transform().z, 100.0);
    assert!(controllers::active(&owner).is_none());
    assert!(notices(&mut client).is_empty());
}

#[tokio::test]
async fn buff_breakers_started_channel_callback_reuses_index_without_resuming_old_lifetime() {
    let mut d = reentrant_data();
    d.systems
        .skills
        .buff_breakers
        .insert(IRRELEVANT, vec![ORIGINAL_TAG]);
    let (server, mut client, owner) = fixture(d);
    let (skill, cancel) = channel(&server, &owner, true);
    let weak = Arc::downgrade(&server);
    let callback_owner = owner.clone();
    *skill.callback.lock().unwrap() = Some(Box::new(move || {
        let server = weak.upgrade().unwrap();
        super::remove_buff(&server, &callback_owner, IRRELEVANT);
        // Replace the later mutual-breaker candidate at its index too. The outer removal
        // snapshot must not remove this new lifetime. Its tag gates the new song's trigger.
        let marker = super::new_buff(
            &server,
            server.game_data.systems.skills.buff(MARKER).unwrap(),
            Some(&callback_owner),
            &SkillCaster::Unit(callback_owner.obj_id()),
            None,
            now_ms(),
        );
        super::add_buff(&server, &callback_owner, marker, 3, 0);
        let t = server.game_data.systems.skills.buff(SONGS[0]).unwrap();
        let b = super::new_buff(
            &server,
            t,
            Some(&callback_owner),
            &SkillCaster::Unit(callback_owner.obj_id()),
            None,
            now_ms(),
        );
        super::add_buff(&server, &callback_owner, b, 2, 0);
    }));
    let original = queue_original(&server, &owner, Some(&skill));
    client.drain();
    crate::systems::dispatch_buff_events(&server, &owner);
    let replacement = super::get_buff(&owner, 2).unwrap();
    assert_eq!(replacement.template.id, original.template.id);
    assert!(!replacement.same_instance(&original));
    assert!(
        has(&owner, MARKER),
        "later candidate's replacement survives"
    );
    assert_eq!(replacement.state, crate::models::buff::effect_state::ACTING);
    assert_ne!(replacement.task_gen, 0);
    assert!(cancel.is_cancelled());
    assert!(owner
        .with_unit(|u| u.unit().skill_state.skill_task.is_none())
        .unwrap());
    assert_eq!(server.systems.skills.tl_ids.in_use(), 0);
    assert_eq!(
        owner.with_unit(|u| (
            u.unit()
                .skill_modifiers
                .apply(MODIFIER_TARGET, &[], SkillAttribute::CastTime, 0.0),
            u.unit()
                .buff_modifiers
                .apply(MODIFIER_TARGET, &[], BuffAttribute::Duration, 0.0),
            u.unit()
                .combat_buffs
                .by_hit_type
                .values()
                .map(Vec::len)
                .sum::<usize>(),
        )),
        Some((17.0, 23.0, 1)),
        "replacement gets exactly one set of modifiers"
    );
    let subscriptions: Vec<_> = server
        .systems
        .skills
        .triggers
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.owner_obj == owner.obj_id() && s.buff_index == 2)
        .map(|s| s.buff_instance_id)
        .collect();
    assert_eq!(subscriptions, vec![replacement.instance_id]);
    super::dispel_task(&server, owner.obj_id(), original.index, original.task_gen);
    assert!(
        has(&owner, SONGS[0]),
        "old lifetime's timer cannot remove replacement"
    );
    assert_eq!(
        notices(&mut client),
        vec![
            (sc::SCBuffCreatedPacket, IRRELEVANT, 3),
            (sc::SCBuffRemovedPacket, 0, 2),
            (sc::SCBuffRemovedPacket, 0, 3),
            (sc::SCBuffCreatedPacket, MARKER, 3),
            (sc::SCBuffCreatedPacket, SONGS[0], 2),
        ]
    );
    super::exit(&server, &owner, 2, false);
    assert_no_original_state(&server, &owner, &replacement);
}

#[tokio::test]
async fn buff_breakers_accepted_refresh_reconciles_restored_active_buff_against_cc() {
    let mut d = data();
    d.systems.skills.add_buff(BuffTemplate {
        id: MARKER,
        kind: 1,
        ..Default::default()
    });
    trigger_buff(
        &mut d,
        BuffEventTriggerKind::Started,
        TRIGGER_EFFECT,
        MARKER,
        0,
    );
    let (server, mut client, owner) = fixture(d);
    let cc = super::new_buff(
        &server,
        server.game_data.systems.skills.buff(SILENCE).unwrap(),
        Some(&owner),
        &SkillCaster::Unit(owner.obj_id()),
        None,
        now_ms(),
    );
    super::add_buff(&server, &owner, cc, 99, 0);
    // Reconstruct pre-hook/restored state through bookkeeping + lifecycle only. Drain
    // Started so this regression can fail independently of Started reconciliation.
    let original = queue_original(&server, &owner, None);
    owner.with_base_unit(|u| u.buffs_mut().take_events());
    super::schedule_effect(&server, &owner, original.index, false);
    let acting = super::get_buff(&owner, original.index).unwrap();
    assert!(acting.in_use && acting.same_instance(&original));
    assert!(!has(&owner, MARKER));
    client.drain();
    add(&server, &owner, SONGS[0]); // REFRESH keeps the index; must still reconcile.
    assert!(!has(&owner, SONGS[0]));
    assert!(
        !has(&owner, MARKER),
        "refresh must not fire Started triggers"
    );
    assert!(has(&owner, SILENCE));
    assert_eq!(
        notices(&mut client),
        vec![
            (sc::SCBuffCreatedPacket, SONGS[0], 2),
            (sc::SCBuffRemovedPacket, 0, 2),
        ]
    );
    super::dispel_task(&server, owner.obj_id(), acting.index, acting.task_gen);
    assert!(notices(&mut client).is_empty());
}

#[tokio::test]
async fn buff_breakers_valid_refresh_keeps_identity_and_single_modifier_set() {
    let mut d = reentrant_data();
    d.systems.skills.buff_triggers.clear();
    Arc::make_mut(d.systems.skills.buffs.get_mut(&SONGS[0]).unwrap()).stun = false;
    let (server, mut client, owner) = fixture(d);
    add(&server, &owner, SONGS[0]);
    let original = super::get_buff(&owner, 2).unwrap();
    client.drain();
    super::add_buff_by_id(&server, &owner, SONGS[0], &owner, 30_000);
    let refreshed = super::get_buff(&owner, 2).unwrap();
    assert!(refreshed.same_instance(&original));
    assert_ne!(refreshed.task_gen, original.task_gen);
    assert_eq!(refreshed.duration, 30_000);
    assert_eq!(
        owner.with_unit(|u| (
            u.unit()
                .skill_modifiers
                .apply(MODIFIER_TARGET, &[], SkillAttribute::CastTime, 0.0),
            u.unit()
                .combat_buffs
                .by_hit_type
                .values()
                .map(Vec::len)
                .sum::<usize>(),
        )),
        Some((17.0, 1))
    );
    assert_eq!(
        notices(&mut client),
        vec![(sc::SCBuffCreatedPacket, SONGS[0], 2)]
    );
    super::dispel_task(&server, owner.obj_id(), original.index, original.task_gen);
    assert!(has(&owner, SONGS[0]));
    super::exit(&server, &owner, 2, false);
    assert_no_original_state(&server, &owner, &refreshed);
}

#[tokio::test]
async fn buff_breakers_started_self_refresh_retains_lifetime_and_installs_modifiers_once() {
    let mut d = reentrant_data();
    d.systems.skills.buff_triggers.clear();
    trigger_buff(
        &mut d,
        BuffEventTriggerKind::Started,
        TRIGGER_EFFECT,
        SONGS[0],
        0,
    );
    let (server, mut client, owner) = fixture(d);
    let original = queue_original(&server, &owner, None);
    client.drain();
    crate::systems::dispatch_buff_events(&server, &owner);
    let active = super::get_buff(&owner, 2).unwrap();
    assert!(active.same_instance(&original));
    assert_eq!(active.state, crate::models::buff::effect_state::ACTING);
    assert_eq!(
        owner.with_unit(|u| (
            u.unit()
                .skill_modifiers
                .apply(MODIFIER_TARGET, &[], SkillAttribute::CastTime, 0.0),
            u.unit()
                .buff_modifiers
                .apply(MODIFIER_TARGET, &[], BuffAttribute::Duration, 0.0),
            u.unit()
                .combat_buffs
                .by_hit_type
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            u.unit().bonuses.bonuses(UnitAttribute::Str).len(),
        )),
        Some((17.0, 23.0, 1, 1))
    );
    // Preserve existing order: the nested refresh publishes its update before the outer
    // start completes, and both refer to the same effect. No Removed notice is valid.
    assert_eq!(
        notices(&mut client),
        vec![
            (sc::SCBuffCreatedPacket, SONGS[0], 2),
            (sc::SCBuffCreatedPacket, SONGS[0], 2),
        ]
    );
    super::exit(&server, &owner, 2, false);
    assert_no_original_state(&server, &owner, &active);
}

#[test]
#[ignore = "requires AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn buff_breakers_real_compact_db_song_glider_and_cc_relations() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let breakers = super::super::data::load_buff_breakers(&conn).unwrap();
    assert_eq!(breakers.len(), 92);
    assert_eq!(breakers.values().map(Vec::len).sum::<usize>(), 661);
    for song in SONGS {
        assert_eq!(breakers[&song], vec![6, 12, 13, 97, 99, 107, 150, 346]);
    }
    assert_eq!(
        breakers[&GLIDER],
        vec![6, 12, 13, 27, 97, 99, 107, 150, 346]
    );
    for (id, tag) in [(STUN, 6), (SLEEP, 97), (SILENCE, 99)] {
        let count: i32 = conn
            .query_row(
                "SELECT count(*) FROM tagged_buffs WHERE buff_id=?1 AND tag_id=?2",
                [id, tag],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
    // Frigid Tracks and Deadeye break on Gliding, rather than arbitrary CC flags.
    assert_eq!(breakers[&431], vec![294]);
    assert_eq!(breakers[&2170], vec![294]);
}
