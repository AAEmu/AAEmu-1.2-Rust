//! Transform buffs (`buffs.transform_buff_id`): the application that reaches `max_stack`
//! replaces the stacks with the transform buff. 1.2 tooltips (compact `localized_texts`,
//! `tbl_name='buffs'`): 1757 "Two stacks turn it into Nymph's Flame" (`max_stack` 2), 2214
//! "Stacks up to 3 times. A 4th stack Stuns" (`max_stack` 4), 2407 "Petrifies upon reaching 5
//! stacks" (`max_stack` 5). All 66 data rows are stack rule 4 with `max_stack >= 2`; chains
//! exist (4152 -> 4752 -> 4101 and 5378 -> 5379 -> 5380).

use crate::data::GameData;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::test_support::TestClient;
use crate::world::{now_ms, WorldObject};

use super::super::cast_validation_tests::fixture;
use super::super::data::BuffTemplate;
use super::super::types::SkillCaster;

const SOURCE: u32 = 71_001;
const TRANSFORM: u32 = 71_002;
const CHAIN_A: u32 = 71_010;
const CHAIN_B: u32 = 71_011;
const CHAIN_C: u32 = 71_012;
const PLAIN: u32 = 71_020;
const MISSING: u32 = 71_030;
const MISSING_TARGET: u32 = 71_099;
const SELF: u32 = 71_040;
const SOURCE_SKILL: u32 = 71_050;

fn stacking(id: u32, max_stack: i32, transform_buff_id: u32) -> BuffTemplate {
    BuffTemplate {
        id,
        kind: 2,
        duration: 10_000,
        stack_rule: 4,
        max_stack,
        transform_buff_id,
        ..BuffTemplate::default()
    }
}

fn data() -> GameData {
    let mut data = GameData::empty();
    let d = &mut data.systems.skills;
    d.add_buff(stacking(SOURCE, 4, TRANSFORM));
    d.add_buff(BuffTemplate {
        id: TRANSFORM,
        kind: 2,
        duration: 1_500,
        ..BuffTemplate::default()
    });
    d.add_buff(stacking(CHAIN_A, 2, CHAIN_B));
    d.add_buff(stacking(CHAIN_B, 2, CHAIN_C));
    d.add_buff(BuffTemplate {
        id: CHAIN_C,
        kind: 2,
        duration: 8_000,
        ..BuffTemplate::default()
    });
    d.add_buff(stacking(PLAIN, 3, 0));
    d.add_buff(stacking(MISSING, 2, MISSING_TARGET));
    d.add_buff(stacking(SELF, 1, SELF));
    data
}

fn count(owner: &WorldObject, id: u32) -> i32 {
    owner.with_base_unit(|b| b.buffs().buff_count_by_id(id))
}

fn apply(server: &GameServer, owner: &WorldObject, caster: &WorldObject, id: u32, times: u32) {
    for _ in 0..times {
        super::add_buff_by_id(server, owner, id, caster, 0);
    }
}

/// Created `(opcode, buff id, index)` / removed `(opcode, 0, index)` notices, decoded from the
/// native 1.2 layout (unit caster prefix; removed body is owner bc + index).
fn notices(client: &mut TestClient) -> Vec<(u16, u32, u32)> {
    client
        .drain()
        .into_iter()
        .filter_map(|(op, body)| {
            if op == sc::SCBuffCreatedPacket {
                Some((
                    op,
                    u32::from_le_bytes(body[15..19].try_into().unwrap()),
                    u32::from_le_bytes(body[11..15].try_into().unwrap()),
                ))
            } else if op == sc::SCBuffRemovedPacket {
                Some((op, 0, u32::from_le_bytes(body[3..7].try_into().unwrap())))
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn reaching_max_stack_transforms_and_removes_the_stacks() {
    let (server, mut client, player, npc) = fixture(data());
    let caster = WorldObject::from(&player);
    let owner = WorldObject::from(&npc);
    apply(&server, &owner, &caster, SOURCE, 3);
    assert_eq!(count(&owner, SOURCE), 3);
    assert_eq!(count(&owner, TRANSFORM), 0);
    let stacks: Vec<u32> = npc
        .data()
        .unit
        .buffs
        .effects
        .iter()
        .filter(|b| b.template.id == SOURCE)
        .map(|b| b.index)
        .collect();
    client.drain();

    apply(&server, &owner, &caster, SOURCE, 1);

    assert_eq!(count(&owner, SOURCE), 0);
    assert_eq!(count(&owner, TRANSFORM), 1);
    let events = notices(&mut client);
    let created: Vec<u32> = events
        .iter()
        .filter(|(op, _, _)| *op == sc::SCBuffCreatedPacket)
        .map(|(_, id, _)| *id)
        .collect();
    assert!(
        created.contains(&TRANSFORM),
        "the transform buff is announced to the client: {created:?}"
    );
    let removed: Vec<u32> = events
        .iter()
        .filter(|(op, _, _)| *op == sc::SCBuffRemovedPacket)
        .map(|(_, _, index)| *index)
        .collect();
    assert_eq!(removed.len(), 4, "all four stacks are removed: {removed:?}");
    for index in stacks {
        assert!(
            removed.contains(&index),
            "stack {index} removed: {removed:?}"
        );
    }
}

#[tokio::test]
async fn below_max_stack_only_stacks() {
    let (server, _client, player, npc) = fixture(data());
    let caster = WorldObject::from(&player);
    let owner = WorldObject::from(&npc);
    apply(&server, &owner, &caster, SOURCE, 3);
    assert_eq!(count(&owner, SOURCE), 3);
    assert_eq!(count(&owner, TRANSFORM), 0);
    apply(&server, &owner, &caster, PLAIN, 2);
    assert_eq!(count(&owner, PLAIN), 2);
}

#[tokio::test]
async fn a_transform_that_reaches_its_own_cap_transforms_again() {
    let (server, _client, player, npc) = fixture(data());
    let caster = WorldObject::from(&player);
    let owner = WorldObject::from(&npc);
    // 4152 -> 4752 -> 4101: the second breath application replaces the stacks with 4752,
    // which already has one stack and therefore reaches its own cap in the same call.
    apply(&server, &owner, &caster, CHAIN_B, 1);
    apply(&server, &owner, &caster, CHAIN_A, 2);
    assert_eq!(count(&owner, CHAIN_A), 0);
    assert_eq!(count(&owner, CHAIN_B), 0);
    assert_eq!(count(&owner, CHAIN_C), 1);
}

#[tokio::test]
async fn a_missing_transform_id_falls_back_to_refresh_at_cap() {
    let (server, _client, player, npc) = fixture(data());
    let caster = WorldObject::from(&player);
    let owner = WorldObject::from(&npc);
    apply(&server, &owner, &caster, MISSING, 2);
    assert_eq!(count(&owner, MISSING), 2);
    // Make the cap refresh deterministic (the new application must have more time left).
    npc.data().unit.buffs.effects.iter_mut().for_each(|b| {
        b.start_time_ms -= 5_000;
    });
    apply(&server, &owner, &caster, MISSING, 1);
    assert_eq!(count(&owner, MISSING), 2);
}

#[tokio::test]
async fn a_self_transform_chain_is_bounded() {
    let (server, _client, player, npc) = fixture(data());
    let caster = WorldObject::from(&player);
    let owner = WorldObject::from(&npc);
    apply(&server, &owner, &caster, SELF, 1);
    assert_eq!(count(&owner, SELF), 1);
}

#[tokio::test]
async fn the_transform_keeps_the_caster_and_skill_of_the_triggering_application() {
    let (server, _client, player, npc) = fixture(data());
    let owner = WorldObject::from(&npc);
    // The first three stacks come from the NPC itself, the fourth from the player.
    apply(&server, &owner, &owner, SOURCE, 3);
    let caster = WorldObject::from(&player);
    let template = server
        .game_data
        .systems
        .skills
        .buff(SOURCE)
        .expect("source template")
        .clone();
    let mut buff = super::new_buff(
        &server,
        &template,
        Some(&caster),
        &SkillCaster::Unit(caster.obj_id()),
        None,
        now_ms(),
    );
    buff.skill_id = SOURCE_SKILL;
    buff.skill_level = 3;
    buff.ab_level = 7;
    super::add_buff(&server, &owner, buff, 0, 0);

    let transform = npc
        .data()
        .unit
        .buffs
        .effects
        .iter()
        .find(|b| b.template.id == TRANSFORM)
        .cloned()
        .expect("transform buff");
    assert_eq!(transform.caster_obj_id, caster.obj_id());
    assert_eq!(
        transform.caster,
        Some((player.character_id, player.data().unit.level))
    );
    assert_eq!(transform.skill_id, SOURCE_SKILL);
    assert_eq!(transform.skill_level, 3);
    assert_eq!(transform.ab_level, 7);
}
