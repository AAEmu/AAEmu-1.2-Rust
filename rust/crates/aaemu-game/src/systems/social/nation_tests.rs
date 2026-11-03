//! Player nations: independence, relation consumers, id allocation, persistence and the GM
//! command (PORTING.md, NATION-1..4).

use super::faction;
use super::nation::{self, Nation, NationManager, NationRelation};
use crate::data::items::{backpack_type, ItemTemplate, TemplateKind};
use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::data::GameData;
use crate::models::container::equip_slot;
use crate::models::item::Item;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::housing::dominion::{DominionState, TowerIdentity};
use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::effects::{special, EffectArgs, EffectSource};
use crate::systems::skills::geometry::Target;
use crate::systems::skills::state::SkillRun;
use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};
use crate::systems::social::expedition::{self, Expedition};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{PlayerRef, WorldObject};

const EXPEDITION: u32 = 1000;
const ZONE_GROUP: u32 = 25;
const CASTLE_CLAIM_PACK: u32 = 16_493;

/// Factions and the faction-166 relation template (`system_factions` /
/// `system_faction_relations` subset; the real table has 46 rows).
fn data() -> GameData {
    let mut data = GameData::empty();
    for (id, mother) in [
        (1, 0),
        (2, 0),
        (3, 0),
        (101, 148),
        (148, 0),
        (149, 0),
        (161, 0),
        (165, 0),
        (166, 0),
        (177, 0),
    ] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            name: format!("Faction{id}"),
            ..SystemFaction::default()
        });
    }
    for (id, id2, state) in [
        (101, 166, 1),
        (148, 166, 1),
        (149, 166, 1),
        (161, 166, 1),
        (165, 166, 3),
        (166, 177, 3),
    ] {
        data.factions.relations.push(FactionRelation {
            id,
            id2,
            state,
            exp_time: 0,
        });
    }
    data.systems.skills.add_skill(SkillTemplate {
        id: nation::DECLARE_INDEPENDENCE_SKILL,
        ..SkillTemplate::default()
    });
    data.items.templates.insert(
        CASTLE_CLAIM_PACK,
        ItemTemplate {
            kind: TemplateKind::Backpack {
                backpack_type: backpack_type::CASTLE_CLAIM,
            },
            ..ItemTemplate::plain(CASTLE_CLAIM_PACK, 1)
        },
    );
    data
}

fn give_dominion(server: &GameServer, expedition_id: u32) {
    let mut state = DominionState::unowned(
        ZONE_GROUP,
        TowerIdentity {
            house_id: 500,
            instance_id: 0,
            guard_tower_setting_id: 6,
        },
    );
    state.owner_expedition_id = expedition_id;
    state.ownership_generation = 1;
    state.reign_start_at = 1_000;
    server
        .systems
        .housing
        .dominions
        .restore(vec![state])
        .expect("dominion fixture");
}

fn add_expedition(server: &GameServer, leader: &PlayerRef, member: &PlayerRef) {
    let faction = SystemFaction {
        id: EXPEDITION,
        mother_id: 101,
        name: "Kingdom".into(),
        owner_id: leader.character_id,
        owner_name: leader.data().unit.name.clone(),
        unit_owner_type: 0,
        political_system: 1,
        created: 0,
        ..SystemFaction::default()
    };
    server.expeditions.add(Expedition {
        faction,
        members: vec![
            expedition::member_from_character(server, EXPEDITION, leader, true),
            expedition::member_from_character(server, EXPEDITION, member, false),
        ],
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    leader.data().unit.expedition_id = EXPEDITION;
    member.data().unit.expedition_id = EXPEDITION;
    // The fixture adds the expedition directly; tell the allocator so it cannot hand the same
    // id to the nation.
    server.ids.expedition.initialize_with(&[EXPEDITION]);
}

struct Fixture {
    server: GameServer,
    leader_client: TestClient,
    member_client: TestClient,
    leader: PlayerRef,
    member: PlayerRef,
}

impl Fixture {
    fn new(with_dominion: bool) -> Self {
        let server = test_server_with(data());
        crate::systems::social::install_hooks(&server);
        let mut leader_client = TestClient::new(&server, 1, 1001);
        let leader = spawn_player(
            &server,
            &leader_client,
            test_character(1, "King", 100.0, 100.0),
            0x100,
        );
        let mut member_client = TestClient::new(&server, 2, 1002);
        let member = spawn_player(
            &server,
            &member_client,
            test_character(2, "Member", 100.0, 100.0),
            0x101,
        );
        add_expedition(&server, &leader, &member);
        if with_dominion {
            give_dominion(&server, EXPEDITION);
        }
        leader_client.drain();
        member_client.drain();
        Self {
            server,
            leader_client,
            member_client,
            leader,
            member,
        }
    }
}

fn drain_ops(client: &mut TestClient) -> Vec<u16> {
    client.drain_ops()
}

#[tokio::test]
async fn independence_creates_the_nation_and_moves_the_members() {
    let mut f = Fixture::new(true);
    let created = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");

    // King and expedition membership.
    assert_eq!(created.owner_character_id, 1);
    assert_eq!(created.owner_name, "King");
    assert_eq!(created.expedition_id, EXPEDITION);
    let expedition = f.server.expeditions.get(EXPEDITION).expect("expedition");
    assert_eq!(expedition.faction.mother_id, created.id);
    assert_eq!(f.leader.data().unit.faction_id, created.id);
    assert_eq!(f.member.data().unit.faction_id, created.id);

    // The copied faction-166 template (NATION-2): hostile to Nuia/Haranya/pirates, friendly
    // to 165 and 177.
    assert_eq!(created.relations.len(), 6);
    assert_eq!(
        created
            .relations
            .iter()
            .find(|r| r.other_faction_id == 148)
            .map(|r| r.state),
        Some(1)
    );
    assert_eq!(
        created
            .relations
            .iter()
            .find(|r| r.other_faction_id == 165)
            .map(|r| r.state),
        Some(3)
    );

    // The client handlers: independence, the relation rows and unit faction changes.
    let leader_ops = drain_ops(&mut f.leader_client);
    assert!(leader_ops.contains(&sc::SCFactionIndependencePacket));
    assert!(leader_ops.contains(&sc::SCFactionRelationListPacket));
    assert!(leader_ops.contains(&sc::SCUnitFactionChangedPacket));
    let member_ops = drain_ops(&mut f.member_client);
    assert!(member_ops.contains(&sc::SCFactionIndependencePacket));
    assert!(member_ops.contains(&sc::SCUnitFactionChangedPacket));

    // The nation id came from the expedition allocator (next id is past it).
    let next = f.server.ids.expedition.get_next_id().expect("next id");
    assert!(
        next > created.id,
        "the shared allocator must already hold {}, got {next}",
        created.id
    );
}

#[tokio::test]
async fn independence_requires_the_owner_and_a_dominion() {
    let f = Fixture::new(true);
    let error = nation::declare_independence(&f.server, &f.member, EXPEDITION, "Kingdom", true)
        .expect_err("a member is not the owner");
    assert_eq!(error, 40);

    let f = Fixture::new(false);
    let error = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect_err("no dominion");
    assert_eq!(error, 40);
    assert!(f.server.systems.social.nations.all().is_empty());
}

#[tokio::test]
async fn independence_skill_effect_needs_the_accepted_cast_and_the_claim_backpack() {
    let f = Fixture::new(true);
    // CASTLE_CLAIM backpack in slot 26 (skill 18540's link_backpack_type_id).
    let item = Item::create(
        100,
        f.server
            .game_data
            .items
            .template(CASTLE_CLAIM_PACK)
            .expect("pack template"),
        1,
        0,
        1,
        &f.server.game_data.items,
        0,
    );
    let equipment = Inv::of(&f.leader).equipment();
    equipment
        .lock()
        .unwrap()
        .add_new_item(item, equip_slot::BACKPACK, &f.server.game_data.items)
        .unwrap();
    f.server.items.add_container(equipment);

    let skill = SkillRun::new(
        f.server
            .game_data
            .systems
            .skills
            .skill(nation::DECLARE_INDEPENDENCE_SKILL)
            .expect("skill")
            .clone(),
        1,
    );
    {
        let mut state = skill.state();
        state.generation = 1;
        state.tl_id = 5;
        state.initial_target = f.leader.obj_id;
    }
    let object = SkillObject::None;
    let args = EffectArgs {
        caster: Some(WorldObject::from(&f.leader)),
        caster_caster: SkillCaster::Unit(f.leader.obj_id),
        target: Target::Obj(WorldObject::from(&f.leader)),
        target_caster: SkillCastTarget::Unit(f.leader.obj_id),
        cast: CastAction::Skill {
            skill_id: nation::DECLARE_INDEPENDENCE_SKILL,
            tl_id: 5,
        },
        source: EffectSource::from_skill(skill.clone()),
        skill_object: &object,
    };
    special::execute(&f.server, 71, &args, [0; 4]);
    assert_eq!(f.server.systems.social.nations.all().len(), 1);
    assert_eq!(
        f.server
            .expeditions
            .get(EXPEDITION)
            .unwrap()
            .faction
            .mother_id,
        f.server.systems.social.nations.all()[0].id
    );

    // A forged (unaccepted) cast does nothing.
    let f = Fixture::new(true);
    let item = Item::create(
        100,
        f.server
            .game_data
            .items
            .template(CASTLE_CLAIM_PACK)
            .expect("pack template"),
        1,
        0,
        1,
        &f.server.game_data.items,
        0,
    );
    let equipment = Inv::of(&f.leader).equipment();
    equipment
        .lock()
        .unwrap()
        .add_new_item(item, equip_slot::BACKPACK, &f.server.game_data.items)
        .unwrap();
    f.server.items.add_container(equipment);
    let skill = SkillRun::new(
        f.server
            .game_data
            .systems
            .skills
            .skill(nation::DECLARE_INDEPENDENCE_SKILL)
            .expect("skill")
            .clone(),
        1,
    );
    let object = SkillObject::None;
    let args = EffectArgs {
        caster: Some(WorldObject::from(&f.leader)),
        caster_caster: SkillCaster::Unit(f.leader.obj_id),
        target: Target::Obj(WorldObject::from(&f.leader)),
        target_caster: SkillCastTarget::Unit(f.leader.obj_id),
        cast: CastAction::Skill {
            skill_id: nation::DECLARE_INDEPENDENCE_SKILL,
            tl_id: 0,
        },
        source: EffectSource::from_skill(skill),
        skill_object: &object,
    };
    special::execute(&f.server, 71, &args, [0; 4]);
    assert!(f.server.systems.social.nations.all().is_empty());
}

#[tokio::test]
async fn nation_relations_drive_combat_party_and_ai_checks() {
    let mut data = data();
    let server = test_server_with({
        data.systems.social.relations = faction::FactionRelations::from_rows(&[
            (101, 166, 1),
            (148, 166, 1),
            (161, 166, 1),
            (165, 166, 3),
        ])
        .expect("social relations");
        data
    });
    let mut client = TestClient::new(&server, 10, 1010);
    let nation_player = spawn_player(
        &server,
        &client,
        test_character(10, "Citizen", 100.0, 100.0),
        0x200,
    );
    let nuia_client = TestClient::new(&server, 11, 1011);
    let nuia_player = spawn_player(
        &server,
        &nuia_client,
        test_character(11, "Nuian", 120.0, 100.0),
        0x201,
    );
    let friend_client = TestClient::new(&server, 12, 1012);
    let friend_player = spawn_player(
        &server,
        &friend_client,
        test_character(12, "Friend", 140.0, 100.0),
        0x202,
    );
    let nation_id = 9_000;
    server.systems.social.nations.add(Nation {
        id: nation_id,
        expedition_id: EXPEDITION,
        owner_character_id: 10,
        owner_name: "Citizen".into(),
        name: "Kingdom".into(),
        created_at: 1,
        allow_change_name: 0,
        relations: vec![
            NationRelation {
                other_faction_id: 148,
                state: 1,
                expire_at: 0,
                next_state: 0,
            },
            NationRelation {
                other_faction_id: 165,
                state: 3,
                expire_at: 0,
                next_state: 0,
            },
        ],
        offline_members: Vec::new(),
    });
    nation_player.data().unit.faction_id = nation_id;
    friend_player.data().unit.faction_id = 165;

    use crate::systems::skills::relation::{can_attack, faction_relation};
    use crate::systems::skills::types::RelationState;
    assert_eq!(
        faction_relation(&server, nation_id, 148),
        RelationState::Hostile
    );
    assert_eq!(
        faction_relation(&server, 148, nation_id),
        RelationState::Hostile
    );
    assert_eq!(
        faction::relation_state_live(&server, nation_id, 148),
        faction::relation_state::HOSTILE
    );
    assert_eq!(
        faction::player_relation(&server, &nation_player, &nuia_player),
        faction::relation_state::HOSTILE
    );
    assert_eq!(
        faction::player_relation(&server, &nation_player, &friend_player),
        faction::relation_state::FRIENDLY
    );

    let me = WorldObject::from(&nation_player);
    let nuian = WorldObject::from(&nuia_player);
    let friend = WorldObject::from(&friend_player);
    assert!(can_attack(
        &server,
        &Target::Obj(me.clone()),
        &Target::Obj(nuian.clone())
    ));
    assert!(!can_attack(
        &server,
        &Target::Obj(me.clone()),
        &Target::Obj(me.clone())
    ));
    assert!(!can_attack(
        &server,
        &Target::Obj(me.clone()),
        &Target::Obj(friend)
    ));
    // The AI aggro helper is the same `CanAttack` path.
    assert!(crate::systems::ai::combat::can_attack(&server, &me, &nuian));

    // Party invites refuse the hostile Nuian and allow the friendly nation member.
    client.drain();
    server.systems.social.teams.ask_to_join(
        &server,
        &nation_player,
        "Nuian",
        0,
        true,
        Some(&nuia_player),
    );
    let frames = client.drain();
    let refused = frames.iter().any(|(op, body)| {
        *op == sc::SCErrorMsgPacket
            && body.len() >= 2
            && i16::from_le_bytes([body[0], body[1]]) == 80
    });
    assert!(refused, "hostile invite must send error 80");
}

#[tokio::test]
async fn gm_command_creates_a_nation_from_the_callers_expedition() {
    let mut data = data();
    data.systems.social.relations =
        faction::FactionRelations::from_rows(&[(148, 166, 1)]).expect("social relations");
    use crate::systems::services::commands::scripts::harness;
    let server = harness::server_with(data);
    let gm = harness::pc(&server, 20, "Gm", 100.0, 100.0);
    let member = harness::pc(&server, 21, "Member", 100.0, 100.0);
    add_expedition(&server, &gm.p, &member.p);
    give_dominion(&server, EXPEDITION);
    let (handled, out) = harness::run(&server, &gm.p, "nation create").await;
    assert!(handled);
    assert!(
        out.iter()
            .any(|message| message.contains("Nation Kingdom created")),
        "{out:?}"
    );
    let nations = server.systems.social.nations.all();
    assert_eq!(nations.len(), 1);
    assert_eq!(nations[0].name, "Kingdom");
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_nation_persistence_round_trip_and_offline_member() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;
    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    for statement in [
        "CREATE TEMPORARY TABLE nations (id INT UNSIGNED NOT NULL, expedition_id INT UNSIGNED \
         NOT NULL, owner_character_id INT UNSIGNED NOT NULL, owner_name VARCHAR(128) NOT NULL, \
         name VARCHAR(128) NOT NULL, created_at BIGINT NOT NULL, allow_change_name TINYINT \
         UNSIGNED NOT NULL, PRIMARY KEY (id))",
        "CREATE TEMPORARY TABLE nation_relations (nation_id INT UNSIGNED NOT NULL, \
         other_faction_id INT UNSIGNED NOT NULL, state TINYINT UNSIGNED NOT NULL, expire_at \
         BIGINT NOT NULL, next_state TINYINT UNSIGNED NOT NULL, \
         PRIMARY KEY (nation_id, other_faction_id))",
        "CREATE TEMPORARY TABLE expeditions (id INT NOT NULL, mother INT NOT NULL)",
        "CREATE TEMPORARY TABLE characters (id INT NOT NULL, faction_id INT NOT NULL)",
        "INSERT INTO expeditions (id, mother) VALUES (1000, 101)",
        "INSERT INTO characters (id, faction_id) VALUES (42, 101)",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    let mut server = test_server_with(GameData::empty());
    server.db = pool.clone();
    let nation = Nation {
        id: 1001,
        expedition_id: 1000,
        owner_character_id: 1,
        owner_name: "King".into(),
        name: "Kingdom".into(),
        created_at: 123,
        allow_change_name: 0,
        relations: vec![NationRelation {
            other_faction_id: 148,
            state: 1,
            expire_at: 0,
            next_state: 0,
        }],
        offline_members: vec![42],
    };
    nation::save_creation(&server, &nation).await.unwrap();
    let (mother, faction): (i64, i64) = sqlx::query_as(
        "SELECT CAST(e.mother AS SIGNED), CAST(c.faction_id AS SIGNED) FROM expeditions e, \
         characters c WHERE e.id = 1000 AND c.id = 42",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((mother, faction), (1001, 1001));

    let loaded = NationManager::default();
    loaded.load(&pool).await.unwrap();
    let loaded = loaded.get(1001).expect("loaded nation");
    assert_eq!(
        (
            loaded.id,
            loaded.expedition_id,
            loaded.owner_character_id,
            loaded.owner_name.as_str(),
            loaded.name.as_str(),
            loaded.created_at,
        ),
        (
            nation.id,
            nation.expedition_id,
            nation.owner_character_id,
            nation.owner_name.as_str(),
            nation.name.as_str(),
            nation.created_at,
        )
    );
    assert_eq!(loaded.relations, nation.relations);
    // The offline list is runtime-only (the characters were already rewritten).
    assert!(loaded.offline_members.is_empty());
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_relation_template_is_faction_166() {
    use crate::data::world_data::{FactionData, Localization};
    use rusqlite::{Connection, OpenFlags};
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let loc = Localization::load(&conn, "en_us").unwrap();
    let mut data = GameData::empty();
    data.factions = FactionData::load(&conn, &loc).unwrap();
    let server = test_server_with(data);
    let rows = nation::template_relations(&server);
    assert_eq!(rows.len(), 46, "faction 166 has 46 relation rows in 1.2");
    assert!(rows
        .iter()
        .any(|r| r.other_faction_id == 148 && r.state == 1));
    assert!(rows
        .iter()
        .any(|r| r.other_faction_id == 149 && r.state == 1));
    assert!(rows
        .iter()
        .any(|r| r.other_faction_id == 161 && r.state == 1));
    assert!(rows
        .iter()
        .any(|r| r.other_faction_id == 116 && r.state == 2));
    assert!(rows
        .iter()
        .any(|r| r.other_faction_id == 165 && r.state == 3));
    assert!(rows
        .iter()
        .any(|r| r.other_faction_id == 177 && r.state == 3));
    assert!(rows.iter().all(|r| r.other_faction_id != 166));
}

#[test]
fn nation_relation_chunks_carry_the_copied_rows() {
    let manager = NationManager::default();
    manager.add(Nation {
        id: 1001,
        expedition_id: 1000,
        owner_character_id: 1,
        owner_name: "King".into(),
        name: "Kingdom".into(),
        created_at: 1,
        allow_change_name: 0,
        relations: vec![NationRelation {
            other_faction_id: 148,
            state: 1,
            expire_at: 0,
            next_state: 0,
        }],
        offline_members: Vec::new(),
    });
    let factions = manager.faction_chunks();
    assert_eq!(factions.len(), 1);
    assert_eq!(factions[0][0].id, 1001);
    assert_eq!(factions[0][0].mother_id, 0);
    assert!(factions[0][0].diplomacy_target);
    let rows = manager.relation_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].id, rows[0].id2, rows[0].state), (1001, 148, 1));
}
