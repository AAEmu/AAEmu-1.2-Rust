//! Player nations: independence, relation consumers, id allocation, persistence and the GM
//! command (PORTING.md, NATION-1..4).

use aaemu_commons::PacketStream;
use chrono::Utc;

use super::faction;
use super::nation::{self, Nation, NationManager, NationRelation, PendingInvite};
use super::packets::faction::{
    CSFactionDeclareHostilePacket, CSFactionImmigrateToOriginPacket,
    CSFactionImmigrationInvitePacket, CSFactionImmigrationInviteReplyPacket,
    CSFactionKickToOriginPacket,
};
use super::packets::misc::invitation_canceled_kind;
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
use crate::systems::social::duel::{FACTION_BLUE_TEAM, FACTION_RED_TEAM};
use crate::systems::social::expedition::{self, Expedition};
use crate::systems::social::packets::duel::{CSChallengeDuelPacket, CSStartDuelPacket};
use crate::systems::social::packets::expedition::CSCreateExpeditionPacket;
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
        (102, 148),
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
            diplomacy_target: matches!(id, 148 | 149),
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
async fn nation_founding_follows_the_era_profile() {
    use crate::config::ServerProfile;

    // Default profile (EuAuroria) and EuLaunch: no player nations; the refusal is the same
    // one a failed ownership precondition uses (40).
    for profile in [ServerProfile::EuAuroria, ServerProfile::EuLaunch] {
        let f = Fixture::new(true);
        f.server.config.update(|c| {
            c.world.profile = profile;
            c.social.player_nations = None;
        });
        assert_eq!(
            nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
                .expect_err("the era has no player nations"),
            crate::systems::economy::error_message::FACTION_SEPARATE_RIGHTS
        );
        assert!(f.server.systems.social.nations.all().is_empty());
    }

    // Kr12: founding works.
    let f = Fixture::new(true);
    f.server.config.update(|c| {
        c.world.profile = ServerProfile::Kr12;
        c.social.player_nations = None;
    });
    assert!(
        nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true).is_ok()
    );

    // An explicit config value still wins over the profile, in both directions.
    let f = Fixture::new(true);
    f.server.config.update(|c| {
        c.world.profile = ServerProfile::EuAuroria;
        c.social.player_nations = Some(true);
    });
    assert!(
        nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true).is_ok()
    );

    let f = Fixture::new(true);
    f.server.config.update(|c| {
        c.world.profile = ServerProfile::Kr12;
        c.social.player_nations = Some(false);
    });
    assert_eq!(
        nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
            .expect_err("explicit false"),
        crate::systems::economy::error_message::FACTION_SEPARATE_RIGHTS
    );
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
        "CREATE TEMPORARY TABLE nation_relation_offers (nation_id INT UNSIGNED NOT NULL, other_faction_id INT UNSIGNED NOT NULL, last_offer_at BIGINT NOT NULL, PRIMARY KEY (nation_id, other_faction_id))",
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
    let mut server = test_server_with(data());
    server.db = pool.clone();
    // The offline member's house is loaded in the manager too (all `housings` rows load at
    // startup); it must move with the offline member, not only its DB row.
    let offline_house = character_house(&server, 42);
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
    // Production adds the nation before saving it (the housing faction check resolves it).
    server.systems.social.nations.add(nation.clone());
    nation::save_creation(&server, &nation).await.unwrap();
    let (mother, faction, house): (i64, i64, i64) = sqlx::query_as(
        "SELECT CAST(e.mother AS SIGNED), CAST(c.faction_id AS SIGNED),          CAST(h.faction_id AS SIGNED) FROM expeditions e, characters c, housings h          WHERE e.id = 1000 AND c.id = 42 AND h.id = 13",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((mother, faction, house), (1001, 1001, 1001));
    assert_eq!(
        offline_house.data().unit.faction_id,
        1001,
        "the loaded HouseRef must move with the offline member"
    );

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
    // Alive: a guild invitation to a dead character is refused.
    character.unit.hp = 100;
    character.unit.max_hp = 100;
    spawn_player(server, client, character, obj_id)
}

fn character_house(server: &GameServer, character_id: u32) -> HouseRef {
    let world = server.world.world(0).expect("instance 0");
    let house = crate::systems::housing::manager::create(server, 300, 101, &world, 0, 0)
        .expect("house template");
    {
        let mut h = house.data();
        h.id = 900 + character_id;
        h.account_id = 900 + character_id;
        h.unit.owner_id = character_id;
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
    let house = character_house(&f.server, candidate.character_id);
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

/// An unanswered immigration invitation is swept after 60 s and closes the invitee's popup with
/// kind 7 (`SCFactionImmigrateInvite`, popup type 0x29).
#[tokio::test]
async fn expired_immigration_invite_sends_kind_seven() {
    let mut f = Fixture::new(false);
    let _nation = declare(&f);
    let mut candidate_client = TestClient::new(&f.server, 3, 1003);
    let candidate = spawn_candidate(&f.server, &candidate_client, 3, "Citizen", 0x102);
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
    assert_eq!(
        frames_with(
            &candidate_client.drain(),
            sc::SCFactionImmigrateInvitePacket
        )
        .len(),
        1
    );

    nation::sweep_expired_invites(
        &f.server,
        super::util::now_ms() + nation::INVITE_TIMEOUT_MS + 1,
    );
    assert_eq!(
        frames_with(&candidate_client.drain(), sc::SCInvitationCanceledPacket),
        vec![vec![invitation_canceled_kind::NATION]]
    );
    assert_eq!(
        f.server.systems.social.nations.pending_invite_count(),
        0,
        "the expired invitation is removed"
    );
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
    for (name, expected) in [("King", 736), ("kInG", 736), ("Nobody", 50)] {
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

    // A king on the invitee's block list is dropped silently (BLOCK-LIST-1).
    candidate
        .data()
        .blocked
        .push((candidate.character_id, f.leader.character_id));
    f.leader_client.drain();
    CSFactionImmigrationInvitePacket {
        invitee: "Citizen".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert!(error_code(&f.leader_client.drain()).is_none());
    assert!(frames_with(
        &candidate_client.drain(),
        sc::SCFactionImmigrateInvitePacket
    )
    .is_empty());
    candidate.data().blocked.clear();

    // A repeated invite while one is pending is dropped, not sent again.
    f.leader_client.drain();
    CSFactionImmigrationInvitePacket {
        invitee: "Citizen".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert_eq!(
        frames_with(
            &candidate_client.drain(),
            sc::SCFactionImmigrateInvitePacket
        )
        .len(),
        1
    );
    f.leader_client.drain();
    CSFactionImmigrationInvitePacket {
        invitee: "Citizen".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert!(frames_with(
        &candidate_client.drain(),
        sc::SCFactionImmigrateInvitePacket
    )
    .is_empty());

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
    let house = character_house(&f.server, candidate.character_id);
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
    let house = character_house(&f.server, candidate.character_id);
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
         TINYINT UNSIGNED NOT NULL, gender TINYINT UNSIGNED NOT NULL, deleted TINYINT UNSIGNED NOT NULL DEFAULT 0, PRIMARY KEY (id))",
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
    // Every `housings` row is loaded at startup, so the offline owner has a live HouseRef.
    let offline_house = character_house(&f.server, 4242);
    offline_house.data().unit.faction_id = nation.id;
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
    assert_eq!(
        offline_house.data().unit.faction_id,
        101,
        "the loaded HouseRef must move with the offline kick, not only the DB row"
    );
    assert!(f.server.world.character_by_name("Offline").is_none());

    // A deleted row is not a kick target.
    sqlx::query(
        "INSERT INTO characters (id, name, faction_id, expedition_id, race, gender, deleted)          VALUES (4343, 'Deleted', ?, 0, 1, 1, 1)",
    )
    .bind(nation.id)
    .execute(&pool)
    .await
    .unwrap();
    f.leader_client.drain();
    CSFactionKickToOriginPacket {
        name: "Deleted".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert_eq!(
        kick_result_error(&f.leader_client.drain()),
        Some(704),
        "a deleted character is not a target"
    );

    // The login race: a target that is in the world is reported back for the online path.
    let online_client = TestClient::new(&f.server, 6, 1006);
    let online = spawn_candidate(&f.server, &online_client, 6, "Online", 0x105);
    assert!(matches!(
        nation::kick_offline(&f.server, online.character_id, nation.id, 101).await,
        Ok(nation::OfflineKick::TargetOnline)
    ));
    // A stored faction that is no longer the nation is not overwritten.
    sqlx::query(
        "INSERT INTO characters (id, name, faction_id, expedition_id, race, gender, deleted)          VALUES (4444, 'Changed', 101, 0, 1, 1, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        nation::kick_offline(&f.server, 4444, nation.id, 101).await,
        Ok(nation::OfflineKick::FactionChanged)
    ));
    let changed: i64 =
        sqlx::query_scalar("SELECT CAST(faction_id AS SIGNED) FROM characters WHERE id = 4444")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(changed, 101);
    pool.close().await;
}

#[tokio::test]
async fn temporary_duel_factions_cannot_immigrate_or_leave_the_nation() {
    let mut f = Fixture::new(false);
    let nation = declare(&f);
    let mut challenger_client = TestClient::new(&f.server, 3, 1003);
    let challenger = spawn_candidate(&f.server, &challenger_client, 3, "Pirate", 0x102);
    challenger.data().unit.faction_id = crate::db_character::FACTION_PIRATE;
    let mut challenged_client = TestClient::new(&f.server, 4, 1004);
    let challenged = spawn_candidate(&f.server, &challenged_client, 4, "Civilian", 0x103);
    for p in [&challenger, &challenged] {
        p.data().unit.hp = 1000;
    }
    challenger_client.conn.active_char = Some(challenger.clone());
    challenged_client.conn.active_char = Some(challenged.clone());
    f.leader_client.conn.active_char = Some(f.leader.clone());
    challenger_client.drain();
    challenged_client.drain();
    f.leader_client.drain();

    // Drive the real duel accept: both participants get a temporary team faction.
    CSChallengeDuelPacket {
        challenged_id: challenged.character_id,
    }
    .handle(&f.server, &mut challenger_client.conn)
    .await
    .unwrap();
    CSStartDuelPacket {
        challenger_id: challenger.character_id,
        error_message: 0,
    }
    .handle(&f.server, &mut challenged_client.conn)
    .await
    .unwrap();
    assert_eq!(challenger.data().unit.faction_id, FACTION_RED_TEAM);
    assert!(challenger.data().unit.faction_temporary);
    assert_eq!(challenged.data().unit.faction_id, FACTION_BLUE_TEAM);
    assert!(challenged.data().unit.faction_temporary);

    // The pirate is still a pirate: return to origin and a king's invite are refused with the
    // pirate code, and the temporary faction stays.
    challenger_client.drain();
    CSFactionImmigrateToOriginPacket {}
        .handle(&f.server, &mut challenger_client.conn)
        .await
        .unwrap();
    assert_eq!(error_code(&challenger_client.drain()), Some(694));
    assert_eq!(challenger.data().unit.faction_id, FACTION_RED_TEAM);
    assert!(challenger.data().unit.faction_temporary);
    f.leader_client.drain();
    CSFactionImmigrationInvitePacket {
        invitee: "Pirate".into(),
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .unwrap();
    assert_eq!(error_code(&f.leader_client.drain()), Some(694));
    assert!(frames_with(
        &challenger_client.drain(),
        sc::SCFactionImmigrateInvitePacket
    )
    .is_empty());

    // A non-nation player in a duel cannot "return to origin": the general refusal wins and
    // the duel team faction is untouched (the pirate escape / duel break are closed).
    challenged_client.drain();
    CSFactionImmigrateToOriginPacket {}
        .handle(&f.server, &mut challenged_client.conn)
        .await
        .unwrap();
    assert_eq!(error_code(&challenged_client.drain()), Some(693));
    assert_eq!(challenged.data().unit.faction_id, FACTION_BLUE_TEAM);
    assert!(challenged.data().unit.faction_temporary);

    // The guild rule reads the saved faction: the duelling civilian (saved 101) may still join
    // a guild of the Nuia alliance, while the duelling pirate (saved 161) may not.
    let mut owner_client = TestClient::new(&f.server, 5, 1005);
    let owner = spawn_player(
        &f.server,
        &owner_client,
        test_character(5, "Owner", 100.0, 100.0),
        0x104,
    );
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
    f.server.expeditions.add(Expedition {
        faction,
        members: vec![expedition::member_from_character(
            &f.server, 7_000, &owner, true,
        )],
        policies: expedition::default_policies(&f.server, 7_000),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    owner.data().unit.expedition_id = 7_000;
    owner_client.drain();
    challenger_client.drain();
    challenged_client.drain();

    expedition::invite(&f.server, &owner, "Pirate");
    assert!(
        frames_with(&challenger_client.drain(), sc::SCExpeditionInvitationPacket).is_empty(),
        "a duelling pirate may not join a non-pirate guild"
    );
    expedition::invite(&f.server, &owner, "Civilian");
    assert_eq!(
        frames_with(&challenged_client.drain(), sc::SCExpeditionInvitationPacket).len(),
        1,
        "a duelling Nuian may join a Nuian guild by their saved faction"
    );
    expedition::reply_invite(&f.server, &challenged, 7_000, 0, true).await;
    assert_eq!(challenged.data().unit.expedition_id, 7_000);
    assert_eq!(f.server.expeditions.get(7_000).unwrap().members.len(), 2);
    let _ = nation;
}

#[tokio::test]
async fn guild_membership_uses_the_saved_top_level_faction() {
    let server = test_server_with(data());
    crate::systems::social::install_hooks(&server);
    let mut owner_client = TestClient::new(&server, 30, 1030);
    let owner = spawn_player(
        &server,
        &owner_client,
        test_character(30, "Owner", 100.0, 100.0),
        0x300,
    );
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

    // Race 102 is the same alliance as 101: still invited and accepted.
    let mut ally_client = TestClient::new(&server, 31, 1031);
    let ally = spawn_player(
        &server,
        &ally_client,
        test_character(31, "Ally", 120.0, 100.0),
        0x301,
    );
    ally.data().unit.faction_id = 102;
    ally.data().unit.hp = 100;
    ally.data().unit.max_hp = 100;
    owner_client.drain();
    ally_client.drain();
    expedition::invite(&server, &owner, "Ally");
    assert_eq!(
        frames_with(&ally_client.drain(), sc::SCExpeditionInvitationPacket).len(),
        1,
        "a faction-102 player belongs to the guild's Nuia alliance"
    );
    expedition::reply_invite(&server, &ally, 7_000, 0, true).await;
    assert_eq!(ally.data().unit.expedition_id, 7_000);
    assert_eq!(server.expeditions.get(7_000).unwrap().members.len(), 2);

    // A pirate belongs to no alliance and is refused by the non-pirate guild (silently: there
    // is no 1.2 error for it, NATION-13).
    let mut pirate_client = TestClient::new(&server, 32, 1032);
    let pirate = spawn_player(
        &server,
        &pirate_client,
        test_character(32, "Pirate", 140.0, 100.0),
        0x302,
    );
    pirate.data().unit.faction_id = crate::db_character::FACTION_PIRATE;
    pirate_client.drain();
    expedition::invite(&server, &owner, "Pirate");
    assert!(frames_with(&pirate_client.drain(), sc::SCExpeditionInvitationPacket).is_empty());
    assert_eq!(pirate.data().unit.expedition_id, 0);
}

/// Five level-10 characters with the given factions, joined into one party (the create
/// requirements are level 5, a party of 5 and 10 000 money paid by the caller).
fn spawn_party(
    server: &GameServer,
    first_id: u32,
    factions: &[u32],
) -> (Vec<TestClient>, Vec<PlayerRef>) {
    let mut clients = Vec::new();
    let mut players = Vec::new();
    for (i, faction) in factions.iter().enumerate() {
        let id = first_id + i as u32;
        let mut client = TestClient::new(server, id, 1_000 + id);
        let p = spawn_player(
            server,
            &client,
            test_character(id, &format!("Party{id}"), 100.0 + i as f32, 100.0),
            0x100 + id,
        );
        p.data().unit.faction_id = *faction;
        p.data().is_online = true;
        client.drain();
        clients.push(client);
        players.push(p);
    }
    for i in 1..players.len() {
        let name = players[i].data().unit.name.clone();
        server
            .systems
            .social
            .teams
            .ask_to_join(server, &players[0], &name, 0, true, None);
        server.systems.social.teams.reply_to_join_team(
            server,
            &players[i],
            0,
            true,
            players[0].character_id,
            false,
            "",
            false,
        );
    }
    assert_eq!(
        server
            .systems
            .social
            .teams
            .team_characters_of(players[0].character_id)
            .map(|m| m.len()),
        Some(factions.len())
    );
    (clients, players)
}

#[tokio::test]
async fn guild_creation_requires_one_top_level_faction() {
    let server = test_server_with(data());
    crate::systems::social::install_hooks(&server);
    // Owner 101 + members 102/101/102/101: one Nuia guild.
    let (mut clients, players) = spawn_party(&server, 40, &[101, 102, 101, 102, 101]);
    let owner = players[0].clone();
    owner.data().money = 20_000;
    clients[0].conn.active_char = Some(owner.clone());
    CSCreateExpeditionPacket {
        name: "SameAlliance".into(),
        id: 0,
    }
    .handle(&server, &mut clients[0].conn)
    .await
    .unwrap();
    let created = server
        .expeditions
        .all()
        .into_iter()
        .find(|e| e.faction.name == "SameAlliance")
        .expect("the same-alliance guild is created");
    assert_eq!(created.faction.mother_id, 101);
    assert_eq!(created.members.len(), 5);

    // One friend (165) is not Nuia: the creation is refused with 320 and no guild is created.
    let (mut clients, players) = spawn_party(&server, 50, &[101, 101, 101, 101, 165]);
    let owner = players[0].clone();
    owner.data().money = 20_000;
    clients[0].conn.active_char = Some(owner.clone());
    clients[0].drain();
    CSCreateExpeditionPacket {
        name: "OtherAlliance".into(),
        id: 0,
    }
    .handle(&server, &mut clients[0].conn)
    .await
    .unwrap();
    assert_eq!(error_code(&clients[0].drain()), Some(320));
    assert!(!server
        .expeditions
        .all()
        .iter()
        .any(|e| e.faction.name == "OtherAlliance"));
}

// ---- Stage 3: diplomacy (hostile declaration, friendly treaty mail, relation expiry) ----

fn spawn_side_player(
    server: &GameServer,
    id: u32,
    name: &str,
    faction: u32,
    obj_id: u32,
) -> (TestClient, PlayerRef) {
    let client = TestClient::new(server, id, 2000 + id);
    let p = spawn_player(
        server,
        &client,
        test_character(id, name, 100.0, 100.0),
        obj_id,
    );
    p.data().unit.faction_id = faction;
    (client, p)
}

fn relation_states(frames: &[(u16, Vec<u8>)], nation_id: u32, other: u32) -> Vec<(u8, i64, u8)> {
    frames
        .iter()
        .filter(|(op, body)| *op == sc::SCFactionSetRelationStatePacket && body.len() >= 18)
        .filter(|(_, body)| {
            u32::from_le_bytes([body[0], body[1], body[2], body[3]]) == nation_id
                && u32::from_le_bytes([body[4], body[5], body[6], body[7]]) == other
        })
        .map(|(_, body)| {
            let state = body[8];
            let expire = i64::from_le_bytes(body[9..17].try_into().unwrap());
            let next = body[17];
            (state, expire, next)
        })
        .collect()
}

#[tokio::test]
async fn hostile_declaration_updates_the_relation_broadcasts_and_allows_attack() {
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::relation::can_attack;
    use crate::systems::skills::types::RelationState;
    use crate::world::WorldObject;

    let mut f = Fixture::new(true);
    let nation = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");
    let (mut haranya_client, haranya) = spawn_side_player(&f.server, 3, "Haranya", 149, 0x102);
    // The template makes a fresh nation hostile to Haranya; a treaty changes that first.
    assert!(f.server.systems.social.nations.set_relation(
        nation.id,
        149,
        faction::relation_state::FRIENDLY,
        0,
        0,
    ));
    f.leader_client.drain();
    f.member_client.drain();
    haranya_client.drain();
    let me = WorldObject::from(&f.member);
    let target = WorldObject::from(&haranya);
    assert!(!can_attack(
        &f.server,
        &Target::Obj(me.clone()),
        &Target::Obj(target.clone())
    ));

    // Drive the real packet handler (0x019) with the king's connection.
    f.leader_client.conn.active_char = Some(f.leader.clone());
    CSFactionDeclareHostilePacket { id: 149 }
        .handle(&f.server, &mut f.leader_client.conn)
        .await
        .expect("hostile declaration");

    let row = f
        .server
        .systems
        .social
        .nations
        .relation_row(nation.id, 149)
        .expect("relation row");
    assert_eq!(row.state, faction::relation_state::HOSTILE);
    assert!(row.expire_at > Utc::now().timestamp());
    assert_eq!(row.next_state, faction::relation_state::HOSTILE);

    // The king gets the result; both sides get the new state.
    let leader_frames = f.leader_client.drain();
    let result = leader_frames
        .iter()
        .find(|(op, _)| *op == sc::SCFactionDeclareHostileResultPacket)
        .expect("declare hostile result");
    assert_eq!(
        result.1,
        [
            vec![1],
            nation.id.to_le_bytes().to_vec(),
            149u32.to_le_bytes().to_vec()
        ]
        .concat()
    );
    assert!(!relation_states(&leader_frames, nation.id, 149).is_empty());
    assert!(!relation_states(&f.member_client.drain(), nation.id, 149).is_empty());
    assert!(!relation_states(&haranya_client.drain(), nation.id, 149).is_empty());

    // The combat path sees the new state at once.
    assert_eq!(
        crate::systems::skills::relation::faction_relation(&f.server, nation.id, 149),
        RelationState::Hostile
    );
    assert!(can_attack(
        &f.server,
        &Target::Obj(me),
        &Target::Obj(target)
    ));
}

#[tokio::test]
async fn hostile_declaration_refusals_follow_the_client_check_order() {
    let f = Fixture::new(true);
    let nation = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");
    // 1. The client's rights check runs first (FUN_398a4750 -> 0x20).
    assert_eq!(
        nation::declare_hostile(&f.server, &f.member, 149).unwrap_err(),
        crate::systems::economy::error_message::FACTION_RIGHTS
    );
    // A non-king is refused with 32 even for a target that is also invalid.
    f.server.systems.social.nations.set_relation(
        nation.id,
        149,
        faction::relation_state::FRIENDLY,
        0,
        0,
    );
    // 2. Only is_diplomacy_tgt system factions are subjects (FUN_398a4750 -> 0x2ce = 718).
    assert_eq!(
        nation::declare_hostile(&f.server, &f.leader, 101).unwrap_err(),
        crate::systems::economy::error_message::FACTION_DIPLOMACY_INVALID_SUBJECT
    );
    assert_eq!(
        nation::declare_hostile(&f.server, &f.leader, 999).unwrap_err(),
        crate::systems::economy::error_message::FACTION_DIPLOMACY_INVALID_SUBJECT
    );
    // 3. Already hostile: the client's own relation check (FUN_398a3660) also answers 718.
    assert_eq!(
        nation::declare_hostile(&f.server, &f.leader, 148).unwrap_err(),
        crate::systems::economy::error_message::FACTION_DIPLOMACY_INVALID_SUBJECT
    );
    nation::declare_hostile(&f.server, &f.leader, 149).expect("first declaration");
    assert_eq!(
        nation::declare_hostile(&f.server, &f.leader, 149).unwrap_err(),
        crate::systems::economy::error_message::FACTION_DIPLOMACY_INVALID_SUBJECT
    );
}

#[tokio::test]
async fn relation_expiry_moves_to_the_next_state_and_notifies_both_sides() {
    let mut f = Fixture::new(true);
    let nation = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");
    let (mut haranya_client, _haranya) = spawn_side_player(&f.server, 3, "Haranya", 149, 0x102);
    let now = Utc::now().timestamp();
    f.server.systems.social.nations.set_relation(
        nation.id,
        149,
        faction::relation_state::FRIENDLY,
        now - 1,
        faction::relation_state::HOSTILE,
    );
    f.leader_client.drain();
    f.member_client.drain();
    haranya_client.drain();

    nation::run_periodic(&f.server);

    let row = f
        .server
        .systems
        .social
        .nations
        .relation_row(nation.id, 149)
        .expect("relation row");
    assert_eq!(row.state, faction::relation_state::HOSTILE);
    assert_eq!(row.expire_at, 0);
    assert_eq!(row.next_state, faction::relation_state::HOSTILE);

    for (name, frames) in [
        ("king", f.leader_client.drain()),
        ("nation member", f.member_client.drain()),
        ("Haranya", haranya_client.drain()),
    ] {
        let states = relation_states(&frames, nation.id, 149);
        assert_eq!(
            states,
            vec![(
                faction::relation_state::HOSTILE,
                0,
                faction::relation_state::HOSTILE
            )],
            "{name} must receive the new relation state"
        );
        let expired = frames.iter().any(|(op, body)| {
            *op == sc::SCFactionRelationExpiredPacket
                && body.len() >= 10
                && u32::from_le_bytes([body[0], body[1], body[2], body[3]]) == nation.id
                && u32::from_le_bytes([body[4], body[5], body[6], body[7]]) == 149
                && body[8] == faction::relation_state::FRIENDLY
                && body[9] == faction::relation_state::HOSTILE
        });
        assert!(expired, "{name} must receive the expiry notification");
    }
}

#[tokio::test]
async fn treaty_mail_is_created_and_paid_into_a_friendly_relation() {
    use crate::systems::economy::mail::{mail_status, mail_type};

    let mut f = Fixture::new(true);
    let nation = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");
    f.server.names.add_character(1, "King", 1001);
    let (mut haranya_client, _haranya) = spawn_side_player(&f.server, 3, "Haranya", 149, 0x102);
    let now = Utc::now().timestamp();
    f.server.systems.social.nations.set_relation(
        nation.id,
        149,
        faction::relation_state::FRIENDLY,
        now + 47 * 3600,
        faction::relation_state::HOSTILE,
    );
    // Keep the other alliance out of the due list.
    f.server.systems.social.nations.set_relation(
        nation.id,
        148,
        faction::relation_state::HOSTILE,
        now + 3600,
        faction::relation_state::HOSTILE,
    );
    f.leader_client.drain();
    haranya_client.drain();

    assert_eq!(nation::send_due_treaty_mails(&f.server, now), 1);
    // A second tick does not duplicate the open request.
    assert_eq!(nation::send_due_treaty_mails(&f.server, now), 0);

    let (mail_id, mail) = f
        .server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .find(|(_, m)| m.mail_type == mail_type::NATION_REQUEST_FRIEND)
        .map(|(id, m)| (id, m.clone()))
        .expect("treaty request mail");
    assert_eq!(mail.header.receiver_id, 1);
    assert_eq!(mail.header.status, mail_status::UNPAID);
    assert_eq!(mail.header.extra, 149);
    assert_eq!(mail.body.billing_amount, 2_000_000);

    f.leader.data().money = 2_000_000;
    assert!(nation::pay_friend_treaty(&f.server, &f.leader, mail_id));
    let row = f
        .server
        .systems
        .social
        .nations
        .relation_row(nation.id, 149)
        .expect("relation row");
    assert_eq!(row.state, faction::relation_state::FRIENDLY);
    assert!(row.expire_at >= now + 168 * 3600);
    assert_eq!(row.next_state, faction::relation_state::HOSTILE);
    assert_eq!(f.leader.data().money, 0);
    assert!(f
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(mail_id)
        .is_none());

    let leader_frames = f.leader_client.drain();
    assert!(leader_frames
        .iter()
        .any(|(op, _)| *op == sc::SCChargeMoneyPaidPacket));
    assert!(leader_frames
        .iter()
        .any(|(op, _)| *op == sc::SCMailDeletedPacket));
    assert!(!relation_states(&leader_frames, nation.id, 149).is_empty());
    assert!(!relation_states(&haranya_client.drain(), nation.id, 149).is_empty());
}

#[tokio::test]
async fn the_charge_money_packet_pays_the_treaty_mail() {
    use crate::systems::economy::mail::mail_type;
    use crate::systems::economy::mail_packets::CSPayChargeMoneyPacket;

    let mut f = Fixture::new(true);
    let nation = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");
    f.server.names.add_character(1, "King", 1001);
    let now = Utc::now().timestamp();
    f.server.systems.social.nations.set_relation(
        nation.id,
        149,
        faction::relation_state::HOSTILE,
        0,
        faction::relation_state::HOSTILE,
    );
    f.server.systems.social.nations.set_relation(
        nation.id,
        148,
        faction::relation_state::HOSTILE,
        now + 3600,
        faction::relation_state::HOSTILE,
    );
    assert_eq!(nation::send_due_treaty_mails(&f.server, now), 1);
    let mail_id = f
        .server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .find(|(_, m)| m.mail_type == mail_type::NATION_REQUEST_FRIEND)
        .map(|(id, _)| id)
        .expect("treaty request mail");
    f.leader.data().money = 2_000_000;
    f.leader_client.conn.active_char = Some(f.leader.clone());
    f.leader_client.drain();
    CSPayChargeMoneyPacket {
        mail_id,
        auto_use_aa_point: false,
    }
    .handle(&f.server, &mut f.leader_client.conn)
    .await
    .expect("charge money packet");
    assert_eq!(
        f.server
            .systems
            .social
            .nations
            .relation_row(nation.id, 149)
            .unwrap()
            .state,
        faction::relation_state::FRIENDLY
    );
    assert_eq!(f.leader.data().money, 0);
    assert!(f
        .leader_client
        .drain()
        .iter()
        .any(|(op, _)| *op == sc::SCChargeMoneyPaidPacket));
}

#[tokio::test]
async fn a_treaty_mail_cannot_be_paid_twice_or_without_money() {
    use crate::systems::economy::mail::mail_type;

    let f = Fixture::new(true);
    let nation = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");
    f.server.names.add_character(1, "King", 1001);
    let now = Utc::now().timestamp();
    f.server.systems.social.nations.set_relation(
        nation.id,
        149,
        faction::relation_state::HOSTILE,
        0,
        faction::relation_state::HOSTILE,
    );
    f.server.systems.social.nations.set_relation(
        nation.id,
        148,
        faction::relation_state::HOSTILE,
        now + 3600,
        faction::relation_state::HOSTILE,
    );
    assert_eq!(nation::send_due_treaty_mails(&f.server, now), 1);
    let mail_id = f
        .server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .find(|(_, m)| m.mail_type == mail_type::NATION_REQUEST_FRIEND)
        .map(|(id, _)| id)
        .expect("treaty request mail");

    // No money: no relation change and the mail stays payable.
    assert!(!nation::pay_friend_treaty(&f.server, &f.leader, mail_id));
    assert_eq!(
        f.server
            .systems
            .social
            .nations
            .relation_row(nation.id, 149)
            .unwrap()
            .state,
        faction::relation_state::HOSTILE
    );
    assert!(f
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(mail_id)
        .is_some());

    f.leader.data().money = 2_000_000;
    assert!(nation::pay_friend_treaty(&f.server, &f.leader, mail_id));
    // The second attempt has no mail and no relation change.
    assert!(!nation::pay_friend_treaty(&f.server, &f.leader, mail_id));
    assert_eq!(f.leader.data().money, 0);
}

#[tokio::test]
async fn diplomacy_link_id_mirrors_the_alliance_relation() {
    let mut data = data();
    data.factions.add(SystemFaction {
        id: 167,
        name: "Nuia guard".into(),
        diplomacy_link_id: 148,
        ..SystemFaction::default()
    });
    let server = test_server_with(data);
    let nation_id = 9_100;
    server.systems.social.nations.add(Nation {
        id: nation_id,
        expedition_id: 0,
        owner_character_id: 1,
        owner_name: "King".into(),
        name: "Kingdom".into(),
        created_at: 1,
        allow_change_name: 0,
        relations: vec![NationRelation {
            other_faction_id: 148,
            state: faction::relation_state::FRIENDLY,
            expire_at: 0,
            next_state: 0,
        }],
        offline_members: Vec::new(),
    });
    // No explicit nation -> 167 row: the guard follows the relation to 148.
    assert_eq!(
        nation::relation_between(&server, nation_id, 167),
        Some(faction::relation_state::FRIENDLY)
    );
    assert_eq!(
        nation::relation_between(&server, 167, nation_id),
        Some(faction::relation_state::FRIENDLY)
    );
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_hostile_relation_persists() {
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
        "CREATE TEMPORARY TABLE nation_relation_offers (nation_id INT UNSIGNED NOT NULL, other_faction_id INT UNSIGNED NOT NULL, last_offer_at BIGINT NOT NULL, PRIMARY KEY (nation_id, other_faction_id))",
        "CREATE TEMPORARY TABLE nation_relations (nation_id INT UNSIGNED NOT NULL, \
         other_faction_id INT UNSIGNED NOT NULL, state TINYINT UNSIGNED NOT NULL, expire_at \
         BIGINT NOT NULL, next_state TINYINT UNSIGNED NOT NULL, \
         PRIMARY KEY (nation_id, other_faction_id))",
        "CREATE TEMPORARY TABLE expeditions (id INT NOT NULL, mother INT NOT NULL)",
        "CREATE TEMPORARY TABLE characters (id INT NOT NULL, faction_id INT NOT NULL)",
        "CREATE TEMPORARY TABLE housings (id INT NOT NULL, owner INT NOT NULL, faction_id INT NOT NULL)",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    let mut server = test_server_with(data());
    server.db = pool.clone();
    let nation = Nation {
        id: 1001,
        expedition_id: 0,
        owner_character_id: 1,
        owner_name: "King".into(),
        name: "Kingdom".into(),
        created_at: 123,
        allow_change_name: 0,
        relations: vec![NationRelation {
            other_faction_id: 149,
            state: faction::relation_state::HOSTILE,
            expire_at: 1_700_000_000,
            next_state: faction::relation_state::HOSTILE,
        }],
        offline_members: Vec::new(),
    };
    server.systems.social.nations.add(nation.clone());
    let mut tx = pool.begin().await.unwrap();
    server
        .systems
        .social
        .nations
        .save(&server, &mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // The diplomacy action writes its row immediately as well (INSERT ... ON DUPLICATE KEY).
    nation::persist_relation(&server, 1001, 149, 3, 42, 0)
        .await
        .unwrap();
    nation::persist_offer(&server, 1001, 149, 12345)
        .await
        .unwrap();

    let loaded = NationManager::default();
    loaded.load(&pool).await.unwrap();
    assert_eq!(loaded.last_offer_at(1001, 149), Some(12345));
    let loaded = loaded.get(1001).expect("loaded nation");
    assert_eq!(
        loaded
            .relations
            .iter()
            .find(|r| r.other_faction_id == 149)
            .copied(),
        Some(NationRelation {
            other_faction_id: 149,
            state: 3,
            expire_at: 42,
            next_state: 0,
        })
    );

    // The expiry tick writes the moved state immediately too.
    nation::persist_expiries(
        &server,
        &[nation::RelationExpiry {
            nation_id: 1001,
            other_faction_id: 149,
            prev_state: 3,
            state: 2,
            next_state: 0,
        }],
    )
    .await
    .unwrap();
    let reloaded = NationManager::default();
    reloaded.load(&pool).await.unwrap();
    let row = reloaded
        .get(1001)
        .expect("reloaded nation")
        .relations
        .into_iter()
        .find(|r| r.other_faction_id == 149)
        .expect("relation");
    assert_eq!((row.state, row.expire_at, row.next_state), (2, 0, 0));
    pool.close().await;
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_nation_diplomacy_settings() {
    use rusqlite::{Connection, OpenFlags};
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let settings = nation::NationSettings::load(&conn).unwrap();
    assert_eq!(settings.friend_period_hours, 168);
    assert_eq!(settings.hostile_period_hours, 21);
    assert_eq!(settings.treaty_lead_hours, 48);
    assert_eq!(settings.friend_cost_copper, 2_000_000);
}

// ---- Stage 3 follow-up: offer cooldown, restart tracking, stale payments, linked guards ----

fn type25_mail_ids(server: &GameServer) -> Vec<i64> {
    server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .filter(|(_, m)| {
            m.mail_type == crate::systems::economy::mail::mail_type::NATION_REQUEST_FRIEND
        })
        .map(|(id, _)| id)
        .collect()
}

async fn tracked_offer_fixture() -> (Fixture, u32, i64) {
    let f = Fixture::new(true);
    let nation = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");
    f.server.names.add_character(1, "King", 1001);
    let now = Utc::now().timestamp();
    f.server.systems.social.nations.set_relation(
        nation.id,
        149,
        faction::relation_state::HOSTILE,
        0,
        faction::relation_state::HOSTILE,
    );
    f.server.systems.social.nations.set_relation(
        nation.id,
        148,
        faction::relation_state::HOSTILE,
        now + 24 * 3600,
        faction::relation_state::HOSTILE,
    );
    assert_eq!(nation::send_due_treaty_mails(&f.server, now), 1);
    let mail_id = type25_mail_ids(&f.server)[0];
    (f, nation.id, mail_id)
}

#[tokio::test]
async fn template_hostile_rows_do_not_trigger_treaty_mails() {
    let f = Fixture::new(true);
    let _nation = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");
    f.server.names.add_character(1, "King", 1001);
    let now = Utc::now().timestamp();
    assert_eq!(nation::send_due_treaty_mails(&f.server, now), 0);
    assert!(type25_mail_ids(&f.server).is_empty());
}

#[tokio::test]
async fn a_deleted_offer_is_not_resent_within_the_cooldown() {
    let (f, nation_id, mail_id) = tracked_offer_fixture().await;
    let now = Utc::now().timestamp();
    let lead = i64::from(f.server.game_data.systems.social.nation.treaty_lead_hours) * 3600;
    f.server
        .systems
        .economy
        .mail
        .delete_mail(&f.server, mail_id);
    assert_eq!(nation::send_due_treaty_mails(&f.server, now + 60), 0);
    assert_eq!(nation::send_due_treaty_mails(&f.server, now + lead - 1), 0);
    assert!(f
        .server
        .systems
        .social
        .nations
        .take_treaty(nation_id, 149)
        .is_none());
    assert_eq!(
        nation::send_due_treaty_mails(&f.server, now + lead + 1),
        1,
        "a fresh offer is possible after the cooldown"
    );
}

#[tokio::test]
async fn restart_rebuilds_tracking_and_removes_stale_mails() {
    let (f, nation_id, mail_id) = tracked_offer_fixture().await;
    let now = Utc::now().timestamp();
    // Start-up memory is empty; the stored mail must be found again.
    f.server.systems.social.nations.take_treaty(nation_id, 149);
    nation::rebuild_treaty_mail_tracking(&f.server)
        .await
        .unwrap();
    assert_eq!(
        f.server
            .systems
            .social
            .nations
            .treaty(nation_id, 149)
            .map(|p| p.mail_id),
        Some(mail_id)
    );
    assert_eq!(nation::send_due_treaty_mails(&f.server, now), 0);

    // A mail left over from before a hostile declaration is stale on restart.
    f.server.systems.social.nations.take_treaty(nation_id, 149);
    f.server.systems.social.nations.set_relation(
        nation_id,
        149,
        faction::relation_state::HOSTILE,
        now + 3600,
        faction::relation_state::HOSTILE,
    );
    nation::rebuild_treaty_mail_tracking(&f.server)
        .await
        .unwrap();
    assert!(f
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(mail_id)
        .is_none());
    assert!(f
        .server
        .systems
        .social
        .nations
        .treaty(nation_id, 149)
        .is_none());
}

#[tokio::test]
async fn friendly_expiry_does_not_duplicate_the_lead_mail() {
    let f = Fixture::new(true);
    let nation = nation::declare_independence(&f.server, &f.leader, EXPEDITION, "Kingdom", true)
        .expect("declaration");
    f.server.names.add_character(1, "King", 1001);
    let now = Utc::now().timestamp();
    f.server.systems.social.nations.set_relation(
        nation.id,
        149,
        faction::relation_state::FRIENDLY,
        now + 1,
        faction::relation_state::HOSTILE,
    );
    f.server.systems.social.nations.set_relation(
        nation.id,
        148,
        faction::relation_state::HOSTILE,
        now + 24 * 3600,
        faction::relation_state::HOSTILE,
    );
    assert_eq!(nation::send_due_treaty_mails(&f.server, now), 1);
    assert_eq!(type25_mail_ids(&f.server).len(), 1);

    // Let the applied period run out; the tick must delete the lead mail and not replace it
    // with a second one in the same pass.
    f.server.systems.social.nations.set_relation(
        nation.id,
        149,
        faction::relation_state::FRIENDLY,
        now - 1,
        faction::relation_state::HOSTILE,
    );
    nation::run_periodic(&f.server);
    let row = f
        .server
        .systems
        .social
        .nations
        .relation_row(nation.id, 149)
        .expect("relation");
    assert_eq!(row.state, faction::relation_state::HOSTILE);
    assert_eq!(row.next_state, faction::relation_state::HOSTILE);
    assert!(type25_mail_ids(&f.server).is_empty());
    assert_eq!(nation::send_due_treaty_mails(&f.server, now + 2), 0);
}

#[tokio::test]
async fn stale_or_untracked_treaty_mails_cannot_be_paid() {
    let (f, nation_id, mail_id) = tracked_offer_fixture().await;
    let now = Utc::now().timestamp();
    // A hostile declaration during the offer's life makes it stale.
    f.server.systems.social.nations.set_relation(
        nation_id,
        149,
        faction::relation_state::HOSTILE,
        now + 3600,
        faction::relation_state::HOSTILE,
    );
    f.leader.data().money = 2_000_000;
    assert!(!nation::pay_friend_treaty(&f.server, &f.leader, mail_id));
    assert_eq!(f.leader.data().money, 2_000_000, "nothing may be charged");
    assert!(f
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(mail_id)
        .is_none());
    assert!(f
        .server
        .systems
        .social
        .nations
        .treaty(nation_id, 149)
        .is_none());

    // An untracked mail (memory lost / forged) is refused and deleted. The hostile window has
    // ended, so only the tracking check can refuse it.
    f.server.systems.social.nations.set_relation(
        nation_id,
        149,
        faction::relation_state::HOSTILE,
        0,
        faction::relation_state::HOSTILE,
    );
    let Some(untracked) = nation::send_treaty_mail(&f.server, nation_id, 149) else {
        panic!("GM treaty mail");
    };
    f.server.systems.social.nations.take_treaty(nation_id, 149);
    assert!(!nation::pay_friend_treaty(&f.server, &f.leader, untracked));
    assert_eq!(f.leader.data().money, 2_000_000);
    assert!(f
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(untracked)
        .is_none());

    // A non-king cannot pay the king's mail.
    let Some(king_mail) = nation::send_treaty_mail(&f.server, nation_id, 149) else {
        panic!("GM treaty mail");
    };
    f.member.data().money = 2_000_000;
    assert!(!nation::pay_friend_treaty(&f.server, &f.member, king_mail));
    assert_eq!(f.member.data().money, 2_000_000);
    assert!(f
        .server
        .systems
        .economy
        .mail
        .get_mail_by_id(king_mail)
        .is_some());
}

#[tokio::test]
async fn guards_of_the_treaty_alliance_become_friendly() {
    let mut data = data();
    for (id, link) in [
        (134, 149),
        (135, 148),
        (157, 149),
        (167, 148),
        (168, 149),
        (171, 148),
        (173, 149),
    ] {
        data.factions.add(SystemFaction {
            id,
            name: format!("Guard{id}"),
            diplomacy_link_id: link,
            ..SystemFaction::default()
        });
    }
    let server = test_server_with(data);
    let nation_id = 9_200;
    let guards = [134, 135, 157, 167, 168, 171, 173];
    server.systems.social.nations.add(Nation {
        id: nation_id,
        expedition_id: 0,
        owner_character_id: 1,
        owner_name: "King".into(),
        name: "Kingdom".into(),
        created_at: 1,
        allow_change_name: 0,
        relations: [
            vec![
                NationRelation {
                    other_faction_id: 148,
                    state: faction::relation_state::HOSTILE,
                    expire_at: 0,
                    next_state: 0,
                },
                NationRelation {
                    other_faction_id: 149,
                    state: faction::relation_state::FRIENDLY,
                    expire_at: 0,
                    next_state: 0,
                },
            ],
            guards
                .iter()
                .map(|id| NationRelation {
                    other_faction_id: *id,
                    state: faction::relation_state::HOSTILE,
                    expire_at: 0,
                    next_state: 0,
                })
                .collect(),
        ]
        .concat(),
        offline_members: Vec::new(),
    });
    // 134 / 157 / 168 / 173 follow 149 (friendly); 135 / 167 / 171 follow 148 (hostile),
    // even though the template stored an explicit hostile row for every guard.
    for id in [134, 157, 168, 173] {
        assert_eq!(
            nation::relation_between(&server, nation_id, id),
            Some(faction::relation_state::FRIENDLY),
            "guard {id}"
        );
        assert_eq!(
            nation::relation_between(&server, id, nation_id),
            Some(faction::relation_state::FRIENDLY),
            "guard {id} reverse"
        );
    }
    for id in [135, 167, 171] {
        assert_eq!(
            nation::relation_between(&server, nation_id, id),
            Some(faction::relation_state::HOSTILE),
            "guard {id}"
        );
    }
}

fn offline_nation(id: u32, members: Vec<u32>) -> Nation {
    Nation {
        id,
        expedition_id: 0,
        owner_character_id: 1,
        owner_name: "King".into(),
        name: "Kingdom".into(),
        created_at: 0,
        allow_change_name: 0,
        relations: Vec::new(),
        offline_members: members,
    }
}

#[test]
fn committed_save_prunes_only_the_written_offline_members() {
    let nations = NationManager::default();
    nations.add(offline_nation(7, vec![1, 2, 3]));
    // The save wrote 1 and 2; 3 joined the list afterwards.
    *nations.saved_offline.lock().unwrap() = vec![(7, vec![1, 2])];
    nations.commit_save();
    assert_eq!(nations.get(7).unwrap().offline_members, vec![3]);
    // A second commit has nothing left to prune.
    nations.commit_save();
    assert_eq!(nations.get(7).unwrap().offline_members, vec![3]);
}

#[test]
fn a_kick_removes_the_member_from_the_offline_list() {
    let nations = NationManager::default();
    nations.add(offline_nation(7, vec![1, 2]));
    nations.remove_offline_member(2);
    assert_eq!(nations.get(7).unwrap().offline_members, vec![1]);
}
