//! Player nations: independence, relation consumers, id allocation, persistence and the GM
//! command (PORTING.md, NATION-1..4).

use aaemu_commons::PacketStream;

use super::faction;
use super::nation::{self, Nation, NationManager, NationRelation, PendingInvite};
use super::packets::faction::{
    CSFactionImmigrateToOriginPacket, CSFactionImmigrationInvitePacket,
    CSFactionImmigrationInviteReplyPacket, CSFactionKickToOriginPacket,
};
use crate::data::character_templates::CharacterTemplate;
use crate::data::items::{backpack_type, ItemTemplate, TemplateKind};
use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::data::GameData;
use crate::models::container::equip_slot;
use crate::models::item::Item;
use crate::net::registry::ClientPacket;
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
use crate::world::{HouseRef, PlayerRef, WorldObject};

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
    // The race-1 start faction (`test_character` uses race 1 / gender 1), the origin of every
    // return / kick (PORTING.md, NATION-2 is unrelated: this is `character_templates`).
    data.characters.templates.insert(
        16 + 1,
        CharacterTemplate {
            race: 1,
            gender: 1,
            faction_id: 101,
            ..CharacterTemplate::default()
        },
    );
    // One house template so the housing-faction hook can be exercised.
    data.systems
        .housing
        .add_template(crate::systems::housing::data::HousingTemplate {
            id: 300,
            hp: 100,
            ..Default::default()
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
async fn a_disbanded_founding_guild_does_not_capture_a_reused_id() {
    let f = Fixture::new(false);
    let first = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", false)
        .expect("first declaration");
    assert_eq!(
        f.server
            .systems
            .social
            .nations
            .get(first.id)
            .unwrap()
            .expedition_id,
        EXPEDITION
    );

    // The founding guild disbands; the nation stays kingless (NATION-5) and drops the id.
    assert!(expedition::disband(&f.server, &f.leader).await);
    assert_eq!(
        f.server
            .systems
            .social
            .nations
            .get(first.id)
            .unwrap()
            .expedition_id,
        0
    );

    // The next guild takes the freed id. It must neither be captured by the stale nation nor
    // be refused independence. The owner name is read in its own statement: a `leader.data()`
    // temporary inside the nested struct literal would live until the end of the `let`.
    let owner_name = f.leader.data().unit.name.clone();
    let faction = SystemFaction {
        id: EXPEDITION,
        mother_id: 101,
        name: "NewGuild".into(),
        owner_id: f.leader.character_id,
        owner_name,
        unit_owner_type: 0,
        political_system: 1,
        created: 0,
        ..SystemFaction::default()
    };
    let replacement = Expedition {
        faction,
        members: vec![
            expedition::member_from_character(&f.server, EXPEDITION, &f.leader, true),
            expedition::member_from_character(&f.server, EXPEDITION, &f.member, false),
        ],
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    };
    f.server
        .expeditions
        .with_mut(EXPEDITION, |e| *e = replacement);
    f.leader.data().unit.expedition_id = EXPEDITION;
    f.member.data().unit.expedition_id = EXPEDITION;
    let second = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "NewGuild", false)
        .expect("a reused guild id is independent again");
    assert_ne!(second.id, first.id);
    assert_eq!(second.expedition_id, EXPEDITION);
    assert_eq!(
        f.server
            .expeditions
            .get(EXPEDITION)
            .unwrap()
            .faction
            .mother_id,
        second.id
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
        "CREATE TEMPORARY TABLE housings (id INT NOT NULL, owner INT NOT NULL, faction_id INT NOT NULL)",
        "INSERT INTO expeditions (id, mother) VALUES (1000, 101)",
        "INSERT INTO characters (id, faction_id) VALUES (42, 101)",
        "INSERT INTO housings (id, owner, faction_id) VALUES (13, 42, 101)",
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
    let (mother, faction, house): (i64, i64, i64) = sqlx::query_as(
        "SELECT CAST(e.mother AS SIGNED), CAST(c.faction_id AS SIGNED),          CAST(h.faction_id AS SIGNED) FROM expeditions e, characters c, housings h          WHERE e.id = 1000 AND c.id = 42 AND h.id = 13",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((mother, faction, house), (1001, 1001, 1001));

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

    // A new guild reuses the freed id with its own mother. The periodic nation save must not
    // rewrite it (the old per-save UPDATE captured the new guild).
    sqlx::query("UPDATE expeditions SET mother = 101 WHERE id = 1000")
        .execute(&pool)
        .await
        .unwrap();
    let manager = NationManager::default();
    manager.add(nation.clone());
    let mut tx = pool.begin().await.unwrap();
    manager.save(&server, &mut tx).await.unwrap();
    tx.commit().await.unwrap();
    let reused: i64 =
        sqlx::query_scalar("SELECT CAST(mother AS SIGNED) FROM expeditions WHERE id = 1000")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        reused, 101,
        "the periodic save must not move a reused guild"
    );

    // Loading reconciles the stale stored id (the nation stays; the reused guild does not).
    let reloaded = NationManager::default();
    reloaded.load(&pool).await.unwrap();
    assert_eq!(reloaded.get(1001).unwrap().expedition_id, 0);
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

// ---- Immigration (stage 2): invite / reply / return to origin / kick ----

/// Declares the fixture's expedition independent (no dominion: `require_dominion = false`).
fn declare(f: &Fixture) -> Nation {
    nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", false)
        .expect("declaration")
}

/// A level-50 candidate at the origin faction (race 1 -> 101), as `test_character` builds one.
fn spawn_candidate(
    server: &GameServer,
    client: &TestClient,
    id: u32,
    name: &str,
    obj_id: u32,
) -> PlayerRef {
    let mut character = test_character(id, name, 160.0, 100.0);
    character.unit.level = nation::IMMIGRATE_MIN_LEVEL;
    spawn_player(server, client, character, obj_id)
}

fn citizen_house(server: &GameServer, citizen: &PlayerRef) -> HouseRef {
    let world = server.world.world(0).expect("instance 0");
    let house = crate::systems::housing::manager::create(server, 300, 101, &world, 0, 0)
        .expect("house template");
    {
        let mut h = house.data();
        h.id = 900;
        h.account_id = 900;
        h.unit.owner_id = citizen.character_id;
        h.unit.name = "Cottage".into();
    }
    assert!(server.systems.housing.manager.insert(&house));
    house
}

fn error_code(frames: &[(u16, Vec<u8>)]) -> Option<i16> {
    frames.iter().find_map(|(op, body)| {
        (*op == sc::SCErrorMsgPacket).then(|| i16::from_le_bytes([body[0], body[1]]))
    })
}

fn frames_with(frames: &[(u16, Vec<u8>)], opcode: u16) -> Vec<Vec<u8>> {
    frames
        .iter()
        .filter(|(op, _)| *op == opcode)
        .map(|(_, body)| body.clone())
        .collect()
}

/// Invites "Citizen" through the packet handler with `configure` applied first; returns the
/// error code the king received.
async fn invite_error(configure: impl FnOnce(&GameServer, &PlayerRef, u32)) -> i16 {
    let mut f = Fixture::new(false);
    let nation = declare(&f);
    let candidate_client = TestClient::new(&f.server, 3, 1003);
    let candidate = spawn_candidate(&f.server, &candidate_client, 3, "Citizen", 0x102);
    configure(&f.server, &candidate, nation.id);
    f.leader_client.conn.active_char = Some(f.leader.clone());
    f.leader_client.drain();
    CSFactionImmigrationInvitePacket {
        invitee: "Citizen".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    error_code(&f.leader_client.drain()).expect("an error to the king")
}

#[tokio::test]
async fn invite_accept_moves_the_citizen_and_the_house() {
    let mut f = Fixture::new(false);
    let nation = declare(&f);
    let mut candidate_client = TestClient::new(&f.server, 3, 1003);
    let candidate = spawn_candidate(&f.server, &candidate_client, 3, "Citizen", 0x102);
    let house = citizen_house(&f.server, &candidate);
    f.leader_client.conn.active_char = Some(f.leader.clone());
    candidate_client.conn.active_char = Some(candidate.clone());
    f.leader_client.drain();
    candidate_client.drain();

    CSFactionImmigrationInvitePacket {
        invitee: "Citizen".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    let frames = candidate_client.drain();
    let invites = frames_with(&frames, sc::SCFactionImmigrateInvitePacket);
    assert_eq!(invites.len(), 1, "the invitee must receive the invitation");
    let mut body = PacketStream::from_bytes(invites[0].clone());
    assert_eq!(body.read_u32().unwrap(), nation.id);
    assert_eq!(
        body.read_u32().unwrap(),
        candidate.character_id,
        "id2 is the invitee-id nonce (NATION-11)"
    );
    assert!(error_code(&f.leader_client.drain()).is_none());

    CSFactionImmigrationInviteReplyPacket {
        id: nation.id,
        id2: candidate.character_id,
        answer: true,
    }
    .handle(&f.server, &mut candidate_client.conn)
    .await
    .unwrap();

    assert_eq!(candidate.data().unit.faction_id, nation.id);
    assert_eq!(house.data().unit.faction_id, nation.id);
    let frames = candidate_client.drain();
    let results = frames_with(&frames, sc::SCFactionImmigrateInviteResultPacket);
    assert_eq!(results.len(), 1);
    let mut body = PacketStream::from_bytes(results[0].clone());
    assert_eq!(body.read_u32().unwrap(), nation.id);
    assert_eq!(body.read_string().unwrap(), "Citizen");
    assert!(body.read_bool().unwrap());
    let frames = f.leader_client.drain();
    let results = frames_with(&frames, sc::SCFactionImmigrateInviteResultPacket);
    assert_eq!(results.len(), 1, "the inviter receives the same result");
    let mut body = PacketStream::from_bytes(results[0].clone());
    assert_eq!(body.read_u32().unwrap(), nation.id);
    assert_eq!(body.read_string().unwrap(), "Citizen");
    assert!(body.read_bool().unwrap());
}

#[tokio::test]
async fn immigration_refusals_follow_the_client_check_order() {
    // 1. in combat (also pirate and level 1: combat wins).
    let code = invite_error(|_server, p, _nation| {
        p.data().unit.is_in_battle = true;
        p.data().unit.faction_id = crate::db_character::FACTION_PIRATE;
        p.data().unit.level = 1;
    })
    .await;
    assert_eq!(code, 770);

    // 2. pirate (also an expedition and level 1: the pirate check wins).
    let code = invite_error(|_server, p, _nation| {
        p.data().unit.faction_id = crate::db_character::FACTION_PIRATE;
        p.data().unit.expedition_id = 12345;
        p.data().unit.level = 1;
    })
    .await;
    assert_eq!(code, 694);

    // 3. unknown faction (also an expedition and level 1).
    let code = invite_error(|_server, p, _nation| {
        p.data().unit.faction_id = 9_999;
        p.data().unit.expedition_id = 12345;
        p.data().unit.level = 1;
    })
    .await;
    assert_eq!(code, 693);

    // 4. already a citizen (also an expedition and level 1).
    let code = invite_error(|_server, p, nation_id| {
        p.data().unit.faction_id = nation_id;
        p.data().unit.expedition_id = 12345;
        p.data().unit.level = 1;
    })
    .await;
    assert_eq!(code, 730);

    // 5. owns a user faction (also an expedition and level 1).
    let code = invite_error(|server, p, _nation| {
        server.systems.social.nations.add(Nation {
            id: 5_000,
            expedition_id: 0,
            owner_character_id: p.character_id,
            owner_name: "Citizen".into(),
            name: "Other".into(),
            created_at: 1,
            allow_change_name: 0,
            relations: Vec::new(),
            offline_members: Vec::new(),
        });
        p.data().unit.expedition_id = 12345;
        p.data().unit.level = 1;
    })
    .await;
    assert_eq!(code, 731);

    // 6. in an expedition (also level 1).
    let code = invite_error(|_server, p, _nation| {
        p.data().unit.expedition_id = 12345;
        p.data().unit.level = 1;
    })
    .await;
    assert_eq!(code, 732);

    // 7. level below content_configs 49.
    let code = invite_error(|_server, p, _nation| {
        p.data().unit.level = nation::IMMIGRATE_MIN_LEVEL - 1;
    })
    .await;
    assert_eq!(code, 728);
}

#[tokio::test]
async fn invite_permission_self_offline_and_forged_or_expired_replies() {
    let mut f = Fixture::new(false);
    let nation = declare(&f);
    let mut candidate_client = TestClient::new(&f.server, 3, 1003);
    let candidate = spawn_candidate(&f.server, &candidate_client, 3, "Citizen", 0x102);

    // Not the king -> 697.
    f.member_client.conn.active_char = Some(f.member.clone());
    f.member_client.drain();
    CSFactionImmigrationInvitePacket {
        invitee: "Citizen".into(),
    }
    .handle(&f.server, &mut f.member_client.conn)
    .await
    .unwrap();
    assert_eq!(error_code(&f.member_client.drain()), Some(697));

    // The king cannot invite himself -> 736; an offline name -> 50.
    f.leader_client.conn.active_char = Some(f.leader.clone());
    for (name, expected) in [("King", 736), ("Nobody", 50)] {
        f.leader_client.drain();
        CSFactionImmigrationInvitePacket {
            invitee: name.into(),
        }
        .handle(&f.server, &mut f.leader_client.conn)
        .await
        .unwrap();
        assert_eq!(error_code(&f.leader_client.drain()), Some(expected));
    }

    candidate_client.conn.active_char = Some(candidate.clone());
    // A reply without an invitation is refused (no pending) and changes nothing.
    candidate_client.drain();
    CSFactionImmigrationInviteReplyPacket {
        id: nation.id,
        id2: candidate.character_id,
        answer: true,
    }
    .handle(&f.server, &mut candidate_client.conn)
    .await
    .unwrap();
    assert_eq!(error_code(&candidate_client.drain()), Some(693));
    assert_ne!(candidate.data().unit.faction_id, nation.id);

    // An expired pending invite is refused.
    let now = super::util::now_ms();
    f.server.systems.social.nations.add_invite(
        PendingInvite {
            nation_id: nation.id,
            inviter_character_id: f.leader.character_id,
            invitee_character_id: candidate.character_id,
            expires_at_ms: now - 1,
        },
        now,
    );
    candidate_client.drain();
    CSFactionImmigrationInviteReplyPacket {
        id: nation.id,
        id2: candidate.character_id,
        answer: true,
    }
    .handle(&f.server, &mut candidate_client.conn)
    .await
    .unwrap();
    assert_eq!(error_code(&candidate_client.drain()), Some(693));

    // A wrong nonce is refused.
    f.server.systems.social.nations.add_invite(
        PendingInvite {
            nation_id: nation.id,
            inviter_character_id: f.leader.character_id,
            invitee_character_id: candidate.character_id,
            expires_at_ms: now + nation::INVITE_TIMEOUT_MS,
        },
        now,
    );
    candidate_client.drain();
    CSFactionImmigrationInviteReplyPacket {
        id: nation.id,
        id2: candidate.character_id + 1,
        answer: true,
    }
    .handle(&f.server, &mut candidate_client.conn)
    .await
    .unwrap();
    assert_eq!(error_code(&candidate_client.drain()), Some(693));

    // The real invitation works once; the replayed reply is refused.
    f.leader_client.drain();
    CSFactionImmigrationInvitePacket {
        invitee: "Citizen".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    candidate_client.drain();
    for expected_ok in [true, false] {
        candidate_client.drain();
        CSFactionImmigrationInviteReplyPacket {
            id: nation.id,
            id2: candidate.character_id,
            answer: true,
        }
        .handle(&f.server, &mut candidate_client.conn)
        .await
        .unwrap();
        assert_eq!(
            error_code(&candidate_client.drain()),
            if expected_ok { None } else { Some(693) }
        );
    }
    assert_eq!(candidate.data().unit.faction_id, nation.id);
    assert_eq!(f.server.systems.social.nations.pending_invite_count(), 0);
}

#[tokio::test]
async fn pending_invites_are_bounded() {
    let f = Fixture::new(false);
    let nation = declare(&f);
    let now = super::util::now_ms();
    for id in 1..=(nation::MAX_PENDING_INVITES as u32 + 25) {
        f.server.systems.social.nations.add_invite(
            PendingInvite {
                nation_id: nation.id,
                inviter_character_id: f.leader.character_id,
                invitee_character_id: 10_000 + id,
                expires_at_ms: now + nation::INVITE_TIMEOUT_MS,
            },
            now,
        );
    }
    assert_eq!(
        f.server.systems.social.nations.pending_invite_count(),
        nation::MAX_PENDING_INVITES
    );
}

#[tokio::test]
async fn return_to_origin_refusals_follow_the_client_check_order() {
    let mut f = Fixture::new(false);
    let nation = declare(&f);
    let mut candidate_client = TestClient::new(&f.server, 3, 1003);
    let candidate = spawn_candidate(&f.server, &candidate_client, 3, "Citizen", 0x102);
    faction::set_faction(&f.server, &candidate, nation.id).await;
    candidate_client.conn.active_char = Some(candidate.clone());

    // 1. combat wins over the expedition check.
    candidate.data().unit.is_in_battle = true;
    candidate.data().unit.expedition_id = 12345;
    candidate_client.drain();
    CSFactionImmigrateToOriginPacket {}
        .handle(&f.server, &mut candidate_client.conn)
        .await
        .unwrap();
    assert_eq!(error_code(&candidate_client.drain()), Some(770));
    candidate.data().unit.is_in_battle = false;

    // 2. pirate wins over the expedition check.
    candidate.data().unit.faction_id = crate::db_character::FACTION_PIRATE;
    candidate_client.drain();
    CSFactionImmigrateToOriginPacket {}
        .handle(&f.server, &mut candidate_client.conn)
        .await
        .unwrap();
    assert_eq!(error_code(&candidate_client.drain()), Some(694));
    candidate.data().unit.faction_id = nation.id;

    // 3. a player still at the race origin is already a member -> 733.
    let mut origin_client = TestClient::new(&f.server, 4, 1004);
    let origin = spawn_candidate(&f.server, &origin_client, 4, "Origin", 0x103);
    origin_client.conn.active_char = Some(origin.clone());
    origin_client.drain();
    CSFactionImmigrateToOriginPacket {}
        .handle(&f.server, &mut origin_client.conn)
        .await
        .unwrap();
    assert_eq!(error_code(&origin_client.drain()), Some(733));

    // 4. the nation king cannot leave -> 734.
    f.leader_client.conn.active_char = Some(f.leader.clone());
    f.leader_client.drain();
    CSFactionImmigrateToOriginPacket {}
        .handle(&f.server, &mut f.leader_client.conn)
        .await
        .unwrap();
    assert_eq!(error_code(&f.leader_client.drain()), Some(734));

    // 5. an expedition member cannot leave -> 735.
    candidate.data().unit.expedition_id = 12345;
    candidate_client.drain();
    CSFactionImmigrateToOriginPacket {}
        .handle(&f.server, &mut candidate_client.conn)
        .await
        .unwrap();
    assert_eq!(error_code(&candidate_client.drain()), Some(735));
}

#[tokio::test]
async fn return_to_origin_moves_the_citizen_and_the_house() {
    let mut f = Fixture::new(false);
    let nation = declare(&f);
    let mut candidate_client = TestClient::new(&f.server, 3, 1003);
    let candidate = spawn_candidate(&f.server, &candidate_client, 3, "Citizen", 0x102);
    let house = citizen_house(&f.server, &candidate);
    faction::set_faction(&f.server, &candidate, nation.id).await;
    assert_eq!(house.data().unit.faction_id, nation.id);
    candidate_client.conn.active_char = Some(candidate.clone());
    candidate_client.drain();
    f.leader_client.drain();

    CSFactionImmigrateToOriginPacket {}
        .handle(&f.server, &mut candidate_client.conn)
        .await
        .unwrap();
    assert_eq!(candidate.data().unit.faction_id, 101);
    assert_eq!(house.data().unit.faction_id, 101);
    let frames = candidate_client.drain();
    let results = frames_with(&frames, sc::SCFactionImmigrateToOriginResultPacket);
    assert_eq!(results.len(), 1);
    let mut body = PacketStream::from_bytes(results[0].clone());
    assert_eq!(body.read_string().unwrap(), "Citizen");
    assert_eq!(body.read_u32().unwrap(), nation.id);
    // The remaining nation members see the leave too.
    let frames = f.leader_client.drain();
    assert_eq!(
        frames_with(&frames, sc::SCFactionImmigrateToOriginResultPacket).len(),
        1
    );
}

fn kick_result_error(frames: &[(u16, Vec<u8>)]) -> Option<i16> {
    let bodies = frames_with(frames, sc::SCFactionKickToOriginResultPacket);
    let body = bodies.first()?;
    let mut s = PacketStream::from_bytes(body.clone());
    s.read_string().ok()?;
    s.read_u32().ok()?;
    s.read_u32().ok()?;
    s.read_i16().ok()
}

#[tokio::test]
async fn kick_refusals_follow_the_client_check_order() {
    let mut f = Fixture::new(false);
    let nation = declare(&f);
    let candidate_client = TestClient::new(&f.server, 3, 1003);
    let candidate = spawn_candidate(&f.server, &candidate_client, 3, "Citizen", 0x102);
    faction::set_faction(&f.server, &candidate, nation.id).await;
    let outsider_client = TestClient::new(&f.server, 4, 1004);
    let outsider = spawn_candidate(&f.server, &outsider_client, 4, "Outsider", 0x103);

    // A non-king -> 64.
    f.member_client.conn.active_char = Some(f.member.clone());
    f.member_client.drain();
    CSFactionKickToOriginPacket {
        name: "Citizen".into(),
    }
    .handle(&f.server, &mut f.member_client.conn)
    .await
    .unwrap();
    assert_eq!(kick_result_error(&f.member_client.drain()), Some(64));

    // The king cannot kick himself -> 65.
    f.leader_client.conn.active_char = Some(f.leader.clone());
    f.leader_client.drain();
    CSFactionKickToOriginPacket {
        name: "King".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert_eq!(kick_result_error(&f.leader_client.drain()), Some(65));

    // Unknown target -> 704.
    f.leader_client.drain();
    CSFactionKickToOriginPacket {
        name: "Nobody".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert_eq!(kick_result_error(&f.leader_client.drain()), Some(704));

    // The founding guild's member is an expedition member -> 726.
    f.leader_client.drain();
    CSFactionKickToOriginPacket {
        name: "Member".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert_eq!(kick_result_error(&f.leader_client.drain()), Some(726));

    // A player of another faction -> 705, and nothing changes.
    f.leader_client.drain();
    CSFactionKickToOriginPacket {
        name: "Outsider".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert_eq!(kick_result_error(&f.leader_client.drain()), Some(705));
    assert_eq!(outsider.data().unit.faction_id, 101);
    assert_eq!(candidate.data().unit.faction_id, nation.id);
}

#[tokio::test]
async fn kick_moves_an_online_citizen_back_to_the_origin() {
    let mut f = Fixture::new(false);
    let nation = declare(&f);
    let mut candidate_client = TestClient::new(&f.server, 3, 1003);
    let candidate = spawn_candidate(&f.server, &candidate_client, 3, "Citizen", 0x102);
    let house = citizen_house(&f.server, &candidate);
    faction::set_faction(&f.server, &candidate, nation.id).await;
    candidate_client.conn.active_char = Some(candidate.clone());
    f.leader_client.conn.active_char = Some(f.leader.clone());
    f.leader_client.drain();
    candidate_client.drain();

    CSFactionKickToOriginPacket {
        name: "Citizen".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert_eq!(candidate.data().unit.faction_id, 101);
    assert_eq!(house.data().unit.faction_id, 101);
    let frames = f.leader_client.drain();
    let results = frames_with(&frames, sc::SCFactionKickToOriginResultPacket);
    assert_eq!(results.len(), 1);
    let mut body = PacketStream::from_bytes(results[0].clone());
    assert_eq!(body.read_string().unwrap(), "Citizen");
    assert_eq!(body.read_u32().unwrap(), candidate.character_id);
    assert_eq!(body.read_u32().unwrap(), nation.id);
    assert_eq!(body.read_i16().unwrap(), 0);
    // The kicked player sees `id == my char id` -> kicked_expel.
    let frames = candidate_client.drain();
    let results = frames_with(&frames, sc::SCFactionKickToOriginResultPacket);
    assert_eq!(results.len(), 1);
    let mut body = PacketStream::from_bytes(results[0].clone());
    assert_eq!(body.read_string().unwrap(), "Citizen");
    assert_eq!(body.read_u32().unwrap(), candidate.character_id);
    assert_eq!(body.read_u32().unwrap(), nation.id);
    assert_eq!(body.read_i16().unwrap(), 0);
}

#[tokio::test]
async fn a_citizen_cannot_join_a_guild_of_another_mother() {
    let server = test_server_with(data());
    crate::systems::social::install_hooks(&server);
    let mut owner_client = TestClient::new(&server, 30, 1030);
    let owner = spawn_player(
        &server,
        &owner_client,
        test_character(30, "Owner", 100.0, 100.0),
        0x300,
    );
    let mut citizen_client = TestClient::new(&server, 31, 1031);
    let citizen = spawn_candidate(&server, &citizen_client, 31, "Citizen", 0x301);
    // A guild whose mother is the Nuia race faction 101 (top-level 148).
    let faction = SystemFaction {
        id: 7_000,
        mother_id: 101,
        name: "NuiaGuild".into(),
        owner_id: owner.character_id,
        owner_name: "Owner".into(),
        unit_owner_type: 0,
        political_system: 1,
        created: 0,
        ..SystemFaction::default()
    };
    server.expeditions.add(Expedition {
        faction,
        members: vec![expedition::member_from_character(
            &server, 7_000, &owner, true,
        )],
        policies: expedition::default_policies(&server, 7_000),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    owner.data().unit.expedition_id = 7_000;
    // The citizen belongs to nation 5_000.
    server.systems.social.nations.add(Nation {
        id: 5_000,
        expedition_id: 0,
        owner_character_id: 999,
        owner_name: "King".into(),
        name: "Kingdom".into(),
        created_at: 1,
        allow_change_name: 0,
        relations: Vec::new(),
        offline_members: Vec::new(),
    });
    owner_client.drain();
    citizen_client.drain();

    // A citizen of another nation is not invited.
    citizen.data().unit.faction_id = 5_000;
    expedition::invite(&server, &owner, "Citizen");
    assert!(
        frames_with(&citizen_client.drain(), sc::SCExpeditionInvitationPacket).is_empty(),
        "a citizen of another nation must not be invited"
    );

    // Even an invitation obtained while still at the origin faction cannot be accepted after
    // the citizen joins a nation.
    citizen.data().unit.faction_id = 101;
    expedition::invite(&server, &owner, "Citizen");
    assert_eq!(
        frames_with(&citizen_client.drain(), sc::SCExpeditionInvitationPacket).len(),
        1
    );
    citizen.data().unit.faction_id = 5_000;
    expedition::reply_invite(&server, &citizen, 7_000, 0, true).await;
    assert_eq!(citizen.data().unit.expedition_id, 0);
    assert_eq!(server.expeditions.get(7_000).unwrap().members.len(), 1);

    // Two citizens of different nations cannot found a guild together either (the create path
    // compares the live top-level factions, NATION-13).
    let second_client = TestClient::new(&server, 32, 1032);
    let second = spawn_candidate(&server, &second_client, 32, "Second", 0x302);
    server.systems.social.nations.add(Nation {
        id: 5_001,
        expedition_id: 0,
        owner_character_id: 999,
        owner_name: "King".into(),
        name: "OtherKingdom".into(),
        created_at: 1,
        allow_change_name: 0,
        relations: Vec::new(),
        offline_members: Vec::new(),
    });
    second.data().unit.faction_id = 5_001;
    assert_ne!(
        nation::top_level_faction(&server, 5_000),
        nation::top_level_faction(&server, 5_001)
    );
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_immigration_persists_and_kicks_offline() {
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
        "CREATE TEMPORARY TABLE characters (id INT UNSIGNED NOT NULL, name VARCHAR(128) NOT \
         NULL, faction_id INT UNSIGNED NOT NULL, expedition_id INT UNSIGNED NOT NULL, race \
         TINYINT UNSIGNED NOT NULL, gender TINYINT UNSIGNED NOT NULL, PRIMARY KEY (id))",
        "CREATE TEMPORARY TABLE housings (id INT UNSIGNED NOT NULL, owner INT UNSIGNED NOT \
         NULL, faction_id INT UNSIGNED NOT NULL, PRIMARY KEY (id))",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }

    let mut f = Fixture::new(false);
    f.server.db = pool.clone();
    let nation = declare(&f);
    let mut candidate_client = TestClient::new(&f.server, 3, 1003);
    let candidate = spawn_candidate(&f.server, &candidate_client, 3, "Citizen", 0x102);
    sqlx::query(
        "INSERT INTO characters (id, name, faction_id, expedition_id, race, gender) \
         VALUES (?, 'Citizen', 101, 0, 1, 1)",
    )
    .bind(candidate.character_id)
    .execute(&pool)
    .await
    .unwrap();

    // Invite -> accept persists `characters.faction_id`.
    f.leader_client.conn.active_char = Some(f.leader.clone());
    candidate_client.conn.active_char = Some(candidate.clone());
    f.leader_client.drain();
    candidate_client.drain();
    CSFactionImmigrationInvitePacket {
        invitee: "Citizen".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    candidate_client.drain();
    CSFactionImmigrationInviteReplyPacket {
        id: nation.id,
        id2: candidate.character_id,
        answer: true,
    }
    .handle(&f.server, &mut candidate_client.conn)
    .await
    .unwrap();
    let saved: i64 =
        sqlx::query_scalar("SELECT CAST(faction_id AS SIGNED) FROM characters WHERE id = ?")
            .bind(candidate.character_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(saved, i64::from(nation.id));

    // An offline citizen is kicked by name: characters + houses move together.
    sqlx::query(
        "INSERT INTO characters (id, name, faction_id, expedition_id, race, gender) \
         VALUES (4242, 'Offline', ?, 0, 1, 1)",
    )
    .bind(nation.id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO housings (id, owner, faction_id) VALUES (13, 4242, ?)")
        .bind(nation.id)
        .execute(&pool)
        .await
        .unwrap();
    f.leader_client.drain();
    CSFactionKickToOriginPacket {
        name: "Offline".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    let frames = f.leader_client.drain();
    assert_eq!(kick_result_error(&frames), Some(0));
    let (faction, house): (i64, i64) = sqlx::query_as(
        "SELECT CAST(c.faction_id AS SIGNED), CAST(h.faction_id AS SIGNED) \
         FROM characters c, housings h WHERE c.id = 4242 AND h.id = 13",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((faction, house), (101, 101));
    assert!(f.server.world.character_by_name("Offline").is_none());
    pool.close().await;
}
