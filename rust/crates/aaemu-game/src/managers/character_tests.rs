//! Port of AAEmu.UnitTests/Game/Core/Managers/CharacterManagerTests.cs
//!
//! `Constructor_DoesNotCallDeps` verifies the DI wiring of `CharacterManager`. The Rust
//! `CharacterManager` has no injected managers (its tables are `CharacterData`, its operations
//! free functions taking the server), so the port checks that construction has no side
//! effects: a new `CharacterData` is empty.

use crate::config::AppConfig;
use crate::data::character_templates::CharacterData;
use crate::data::skills::{DefaultSkill, SkillTemplate};
use crate::data::GameData;
use crate::models::char_data::{read_action_slots, ActionSlot, ActionSlotType, MAX_ACTION_SLOTS};
use crate::models::character::Character;
use crate::test_support::{test_server_with, test_server_with_config};

use super::{
    character_slot_limit, create, initialize_starting_skills, may_create_character,
    CreateCharacterRequest,
};

const STARTERS: [(u8, u32); 10] = [
    (1, 18132),
    (2, 14376),
    (3, 10399),
    (4, 16486),
    (5, 14810),
    (6, 16210),
    (7, 10752),
    (8, 18125),
    (9, 11973),
    (10, 10534),
];

fn starting_skill_data() -> GameData {
    let mut data = GameData::empty();
    for (ability_id, id) in STARTERS {
        data.skills.add_skill(SkillTemplate {
            id,
            ability_id,
            ability_level: 1,
            auto_learn: true,
            need_learn: true,
            show: true,
            ..SkillTemplate::default()
        });
    }
    for (slot, skill_id, add_to_slot) in [
        (1, 2, true),
        (2, 16064, true),
        (14, 14495, true),
        (21, 10594, true),
        (6, 20297, false),
    ] {
        data.skills.add_skill(SkillTemplate {
            id: skill_id,
            ..SkillTemplate::default()
        });
        data.skills.default_skills.push(DefaultSkill {
            skill_id,
            slot,
            add_to_slot,
        });
    }
    data.skills.finish();
    data
}

fn new_character(ability: u8) -> Character {
    let mut c = Character::new(1, 1, Default::default());
    c.unit.level = 1;
    c.ability1 = ability;
    c.ability2 = 11;
    c.ability3 = 11;
    c.abilities.set_ability(ability, 0);
    c
}

fn creation_data() -> GameData {
    use crate::data::character_templates::{
        AbilityItems, AbilitySupplyItem, CharacterTemplate, EquipItemsTemplate,
    };
    use crate::data::items::{EquipTemplate, ItemTemplate, TemplateKind};
    let mut data = starting_skill_data();
    data.experience = crate::test_support::test_experience(10);
    data.characters.templates.insert(
        17,
        CharacterTemplate {
            race: 1,
            gender: 1,
            creatable: true,
            num_inventory_slot: 50,
            num_bank_slot: 50,
            ..Default::default()
        },
    );
    let mut armor = ItemTemplate::plain(2000, 1);
    armor.kind = TemplateKind::Armor(EquipTemplate {
        slot_type_id: 1,
        ..Default::default()
    });
    data.items.templates.insert(2000, armor);
    for id in [2001, 2002] {
        data.items
            .templates
            .insert(id, ItemTemplate::plain(id, 1000));
    }
    data.characters.ability_items.insert(
        7,
        AbilityItems {
            items: EquipItemsTemplate {
                headgear: (2000, 0),
                ..Default::default()
            },
            supplies: vec![AbilitySupplyItem {
                id: 2001,
                amount: 2,
                grade: 0,
            }],
            ..Default::default()
        },
    );
    data.characters.ability_items.insert(
        0,
        AbilityItems {
            supplies: vec![AbilitySupplyItem {
                id: 2002,
                amount: 300,
                grade: 0,
            }],
            ..Default::default()
        },
    );
    data
}

fn creation_request(name: &str) -> CreateCharacterRequest {
    CreateCharacterRequest {
        name: name.into(),
        race: 1,
        gender: 1,
        body_items: [0; 7],
        model_params: Default::default(),
        ability1: 7,
        ability2: 11,
        ability3: 11,
        level: 1,
    }
}

async fn lobby_create(
    server: &crate::GameServer,
    client: &mut crate::test_support::TestClient,
    request: CreateCharacterRequest,
) -> crate::Result<()> {
    use crate::net::registry::ClientPacket;
    crate::packets::c2g_world::CSCreateCharacterPacket(request)
        .handle(server, &mut client.conn)
        .await
}

/// One pinned connection: all writes are shadowed by TEMPORARY tables and disappear at close.
async fn private_lobby_pool() -> Option<sqlx::MySqlPool> {
    use crate::systems::social::achievement_persistence::temporary_shadow_definition;
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;
    let url = std::env::var("AAEMU_TEST_MYSQL").ok()?;
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
        .unwrap();
    for table in [
        "items",
        "item_containers",
        "characters",
        "options",
        "abilities",
        "actabilities",
        "appellations",
        "character_active_buffs",
        "portal_book_coords",
        "portal_visited_district",
        "friends",
        "blocked",
        "skills",
        "quests",
        "completed_quests",
        "accounts",
        "character_records",
        "character_achievements",
        "character_crafts",
    ] {
        let (_, definition): (String, String) =
            sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(&temporary_shadow_definition(&definition))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("SET SESSION sql_mode='STRICT_ALL_TABLES'")
        .execute(&pool)
        .await
        .unwrap();
    Some(pool)
}

async fn persisted_lobby_count(pool: &sqlx::MySqlPool, account: u32) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM characters WHERE account_id = ? AND deleted = 0")
        .bind(account)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_lobby_slots_five_six_seven_parallel_rollback_retry_and_account_isolation() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut server = test_server_with(creation_data());
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000), (1008, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut seed = TestClient::new(&server, 1, 1007);
    for index in 0..5 {
        lobby_create(
            &server,
            &mut seed,
            creation_request(&format!("Seed{index}")),
        )
        .await
        .unwrap();
        assert!(seed
            .drain_ops()
            .contains(&crate::opcodes::sc::SCCreateCharacterResponsePacket));
    }
    assert_eq!(persisted_lobby_count(&pool, 1007).await, 5);
    let pending_id = seed.conn.characters[0].id;
    sqlx::query("UPDATE characters SET delete_request_time=UTC_TIMESTAMP(), delete_time=DATE_ADD(UTC_TIMESTAMP(), INTERVAL 1 DAY) WHERE id=?")
        .bind(pending_id).execute(&pool).await.unwrap();
    let inventory_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&pool)
        .await
        .unwrap();

    // Reproduce a later SQL failure at slot 5: the refused sixth character must roll back.
    // A required, unprovided column rejects the skill write after the character row; unlike
    // triggers or CHECK constraints this also works on MySQL TEMPORARY tables.
    sqlx::query("ALTER TABLE skills ADD COLUMN lobby_required INT NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    let mut retry = TestClient::new(&server, 2, 1007); // deliberately no loaded character list
    lobby_create(&server, &mut retry, creation_request("Retry"))
        .await
        .unwrap();
    assert_creation_failure(&mut retry, 2);
    assert_eq!(persisted_lobby_count(&pool, 1007).await, 5);
    assert!(!server.names.name_exists("Retry"));
    let after_failure: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after_failure, inventory_rows);
    sqlx::query("ALTER TABLE skills DROP COLUMN lobby_required")
        .execute(&pool)
        .await
        .unwrap();

    let mut parallel = TestClient::new(&server, 3, 1007);
    {
        let transaction = server.save.transaction_lock().await;
        let creates = async {
            tokio::join!(
                lobby_create(&server, &mut retry, creation_request("Retry")),
                lobby_create(&server, &mut parallel, creation_request("Parallel")),
            )
        };
        tokio::pin!(creates);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut creates)
                .await
                .is_err()
        );
        drop(transaction);
        let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(10), creates)
            .await
            .unwrap();
        a.unwrap();
        b.unwrap();
    }
    assert_eq!(
        retry.conn.characters.len() + parallel.conn.characters.len(),
        1
    );
    let frames = [retry.drain(), parallel.drain()];
    assert_eq!(
        frames
            .iter()
            .flatten()
            .filter(
                |(op, body)| *op == crate::opcodes::sc::SCCharacterCreationFailedPacket
                    && body == &[9]
            )
            .count(),
        1
    );
    assert_eq!(
        persisted_lobby_count(&pool, 1007).await,
        6,
        "pending deletion still consumes a slot"
    );
    lobby_create(&server, &mut seed, creation_request("Seventh"))
        .await
        .unwrap();
    assert_creation_failure(&mut seed, 9);

    // A full account never consumes a different account's slot or leaves its failed name.
    let mut other = TestClient::new(&server, 4, 1008);
    lobby_create(&server, &mut other, creation_request("Seventh"))
        .await
        .unwrap();
    assert_eq!(other.conn.characters.len(), 1);
    assert_eq!(persisted_lobby_count(&pool, 1008).await, 1);
    other.drain();
    let historical_id = other.conn.characters[0].id;
    // Model a pre-existing over-limit account (7 rows), independently of the cached lists.
    sqlx::query("UPDATE characters SET account_id=1007 WHERE id=?")
        .bind(historical_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(persisted_lobby_count(&pool, 1007).await, 7);
    lobby_create(&server, &mut seed, creation_request("Eighth"))
        .await
        .unwrap();
    assert_creation_failure(&mut seed, 9);

    // Fully deleted rows free slots; scheduled deletions above did not.
    sqlx::query("UPDATE characters SET deleted=1 WHERE id IN (?, ?)")
        .bind(historical_id)
        .bind(seed.conn.characters[1].id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(persisted_lobby_count(&pool, 1007).await, 5);
    lobby_create(&server, &mut seed, creation_request("Eighth"))
        .await
        .unwrap();
    assert!(seed
        .drain_ops()
        .contains(&crate::opcodes::sc::SCCreateCharacterResponsePacket));
    assert_eq!(persisted_lobby_count(&pool, 1007).await, 6);
    assert_eq!(
        server
            .names
            .get_character_account(server.names.get_character_id("Eighth")),
        1007
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_lobby_delete_changes_timestamps_only_for_current_owned_undeleted_row() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut server = test_server_with(creation_data());
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut client = TestClient::new(&server, 1, 1007);
    lobby_create(&server, &mut client, creation_request("Deletable"))
        .await
        .unwrap();
    client.drain();
    let id = client.conn.characters[0].id;
    for (account, deleted) in [(1008u32, 0), (1007, 1)] {
        sqlx::query("UPDATE characters SET account_id=?, deleted=? WHERE id=?")
            .bind(account)
            .bind(deleted)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        lobby_delete(&server, &mut client, id).await.unwrap();
        let frames = client.drain();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].0,
            crate::opcodes::sc::SCDeleteCharacterResponsePacket
        );
        let mut expected = id.to_le_bytes().to_vec();
        expected.extend_from_slice(&[0; 17]); // native status=0 + two zero i64 timestamps
        assert_eq!(frames[0].1, expected);
        assert_eq!(client.conn.characters[0].delete_request_time, 0);
        assert_eq!(client.conn.characters[0].delete_time, 0);
        assert_eq!(server.enter_world.character_owner(id), None);
    }
    sqlx::query("UPDATE characters SET account_id=1007, deleted=0 WHERE id=?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    lobby_delete(&server, &mut client, id).await.unwrap();
    let frames = client.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].0,
        crate::opcodes::sc::SCDeleteCharacterResponsePacket
    );
    assert_eq!(frames[0].1.len(), 21);
    assert_eq!(frames[0].1[4], 2);
    let persisted: (i64, i64) = sqlx::query_as(
        "SELECT CAST(UNIX_TIMESTAMP(delete_request_time) AS SIGNED), CAST(UNIX_TIMESTAMP(delete_time) AS SIGNED) FROM characters WHERE id=?",
    ).bind(id).fetch_one(&pool).await.unwrap();
    let c = &client.conn.characters[0];
    assert!(c.delete_request_time > 0);
    assert_eq!(persisted, (c.delete_request_time, c.delete_time));
    assert_eq!(server.enter_world.character_owner(id), None);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_lobby_parallel_names_have_one_owner_and_pending_names_use_native_reason() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut server = test_server_with(creation_data());
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000), (1008, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut a = TestClient::new(&server, 1, 1007);
    let mut b = TestClient::new(&server, 2, 1008);
    let (first, second) = tokio::join!(
        lobby_create(&server, &mut a, creation_request("Unique")),
        lobby_create(&server, &mut b, creation_request("uNIQUE")),
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(a.conn.characters.len() + b.conn.characters.len(), 1);
    let owner = if a.conn.characters.is_empty() {
        assert_creation_failure(&mut a, 4);
        b.conn.account_id()
    } else {
        assert_creation_failure(&mut b, 4);
        a.conn.account_id()
    };
    let id = server.names.get_character_id("Unique");
    assert_eq!(server.names.get_character_account(id), owner);
    assert_eq!(
        persisted_lobby_count(&pool, 1007).await + persisted_lobby_count(&pool, 1008).await,
        1
    );
    a.drain();
    b.drain();
    sqlx::query("UPDATE characters SET delete_request_time=UTC_TIMESTAMP() WHERE id=?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    lobby_create(&server, &mut a, creation_request("UNIQUE"))
        .await
        .unwrap();
    assert_creation_failure(&mut a, 6); // native 'character name is to be deleted'
    pool.close().await;
}

async fn lobby_delete(
    server: &crate::GameServer,
    client: &mut crate::test_support::TestClient,
    character_id: u32,
) -> crate::Result<()> {
    use crate::net::registry::ClientPacket;
    crate::packets::c2g_world::CSDeleteCharacterPacket { character_id }
        .handle(server, &mut client.conn)
        .await
}

fn assert_creation_failure(client: &mut crate::test_support::TestClient, reason: u8) {
    // Native 1.2 sub_397B3060 (x2game12.c:1749639): exactly one u8, no i32 enum.
    assert_eq!(
        client.drain(),
        vec![(
            crate::opcodes::sc::SCCharacterCreationFailedPacket,
            vec![reason]
        )]
    );
}

#[tokio::test]
async fn lobby_create_rejects_unavailable_and_aliased_race_gender_before_allocating_assets() {
    let mut data = creation_data();
    let mut unavailable = data.characters.template(1, 1).unwrap().clone();
    unavailable.race = 3;
    unavailable.creatable = false;
    data.characters.templates.insert(19, unavailable);
    let server = test_server_with(data);
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    for (race, gender) in [(3, 1), (1, 0), (1, 17), (17, 0), (255, 255)] {
        let mut req = creation_request("Unavailable");
        req.race = race;
        req.gender = gender;
        lobby_create(&server, &mut client, req).await.unwrap();
        assert_creation_failure(&mut client, 2); // native general server failure, not continent block
        assert!(client.conn.characters.is_empty());
        assert!(server.names.no_names_registered());
        assert!(server.items.all_containers().is_empty());
    }
    assert_eq!(server.ids.character.get_next_id().unwrap(), 1);
}

#[tokio::test]
async fn lobby_create_filters_invalid_blocked_and_case_variant_names() {
    use crate::data::character_templates::BlockedName;
    let mut data = creation_data();
    data.characters.blocked_names = vec![
        BlockedName {
            text: "Admin".into(),
            partial_match: true,
        },
        BlockedName {
            text: "GM".into(),
            partial_match: false,
        },
    ];
    let server = test_server_with(data);
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    for name in ["bad name", "", "Admin", "AdMiN", "TheAdMiNhero", "gM"] {
        lobby_create(&server, &mut client, creation_request(name))
            .await
            .unwrap();
        assert_creation_failure(&mut client, 5); // sub_391D3330: forbidden name
        assert!(server.names.no_names_registered());
        assert!(server.items.all_containers().is_empty());
    }
    for name in ["Accepted", "Павел", "GMhero"] {
        lobby_create(&server, &mut client, creation_request(name))
            .await
            .unwrap();
        // Reaches the closed DB, rather than being refused by the name filter.
        assert_creation_failure(&mut client, 2);
        assert!(server.names.no_names_registered());
        assert!(server.items.all_containers().is_empty());
    }
}

#[tokio::test]
async fn lobby_create_allowed_name_chars_whitelist_is_opt_in() {
    let data = || {
        let mut d = creation_data();
        // Shipped 1.2 data: lowercase Latin and Hangul syllables, no digits/Cyrillic.
        d.characters.allowed_name_chars = vec!['a', 'b', '가', '나'];
        d
    };

    // Flag off (default): the shipped CharacterNameRegex decides, digits and Cyrillic pass it.
    let server = test_server_with(data());
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    for name in ["Name1", "1", "Павел"] {
        lobby_create(&server, &mut client, creation_request(name))
            .await
            .unwrap();
        // Reaches the closed DB, rather than being refused by the name filter.
        assert_creation_failure(&mut client, 2);
        assert!(server.names.no_names_registered());
        assert!(server.items.all_containers().is_empty());
    }
    lobby_create(&server, &mut client, creation_request("Name!"))
        .await
        .unwrap();
    assert_creation_failure(&mut client, 5); // the regex still rejects it

    // Flag on: the allowed_name_chars whitelist overrides the regex and rejects them.
    let config = crate::config::AppConfig {
        id: 1,
        character_name_use_allowed_chars: true,
        ..Default::default()
    };
    let server = test_server_with_config(data(), config);
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    for name in ["Name1", "1", "Павел", "Name!", "힣"] {
        lobby_create(&server, &mut client, creation_request(name))
            .await
            .unwrap();
        assert_creation_failure(&mut client, 5); // sub_391D3330: forbidden name
        assert!(server.names.no_names_registered());
        assert!(server.items.all_containers().is_empty());
    }
    for name in ["Name", "aB"] {
        lobby_create(&server, &mut client, creation_request(name))
            .await
            .unwrap();
        assert_creation_failure(&mut client, 2);
        assert!(server.names.no_names_registered());
        assert!(server.items.all_containers().is_empty());
    }
}

fn lobby_character(client: &mut crate::test_support::TestClient, id: u32) {
    let mut c = Character::new(id, client.conn.account_id(), Default::default());
    c.unit.name = "Deletable".into();
    c.unit.level = 10;
    client.conn.characters.push(c);
}

fn assert_delete_error(client: &mut crate::test_support::TestClient, expected: &[u8]) {
    // Native 1.2 sub_397C3030 (x2game12.c:1761305): i16, i16, u32, bool.
    // isNotify=false is required for HandleErrorMessage's dedicated lobby-error events.
    assert_eq!(
        client.drain(),
        vec![(crate::opcodes::sc::SCErrorMsgPacket, expected.to_vec())]
    );
}

#[tokio::test]
async fn lobby_delete_refuses_online_and_inflight_select_on_any_connection() {
    let server = test_server_with(creation_data());
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    lobby_character(&mut client, 50);
    for owner in [1, 2] {
        assert_eq!(server.enter_world.reserve_character(50, owner), Ok(true));
        lobby_delete(&server, &mut client, 50).await.unwrap();
        assert_delete_error(&mut client, &[0x3f, 0x02, 0x3f, 0x02, 0, 0, 0, 0, 0]); // 575
        assert_eq!(server.enter_world.character_owner(50), Some(owner));
        assert_eq!(client.conn.characters[0].delete_request_time, 0);
        server.enter_world.release_character(50, owner);
    }
    // Also protect a live world object when no session reservation is present.
    let mut c = client.conn.characters[0].clone();
    c.obj_id = 500;
    let p = crate::world::Player::new(c, client.conn.handle());
    assert!(server.world.try_add_character(&p));
    lobby_delete(&server, &mut client, 50).await.unwrap();
    assert_delete_error(&mut client, &[0x3f, 0x02, 0x3f, 0x02, 0, 0, 0, 0, 0]);
    assert_eq!(server.enter_world.character_owner(50), None);
}

#[tokio::test]
async fn lobby_delete_uses_current_expedition_owner_and_releases_refused_reservations() {
    use crate::data::world_data::SystemFaction;
    use crate::systems::social::expedition::Expedition;
    let server = test_server_with(creation_data());
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    lobby_character(&mut client, 50);
    // The stale lobby character has expedition_id=0, but is now the authoritative owner.
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 700,
            owner_id: 50,
            ..Default::default()
        },
        members: vec![],
        policies: vec![],
        removed_members: vec![],
        is_disbanded: false,
    });
    lobby_delete(&server, &mut client, 50).await.unwrap();
    assert_delete_error(&mut client, &[0x4d, 0x01, 0x4d, 0x01, 0, 0, 0, 0, 0]); // 333
    assert_eq!(client.conn.characters[0].delete_request_time, 0);
    assert_eq!(server.enter_world.character_owner(50), None);

    // After ownership changes, the stale lobby expedition field must not block deletion.
    client.conn.characters[0].unit.expedition_id = 700;
    server
        .expeditions
        .with_mut(700, |e| e.faction.owner_id = 51)
        .unwrap();
    assert!(lobby_delete(&server, &mut client, 50).await.is_err()); // reaches the closed DB
    assert!(client.drain().is_empty());
    assert_eq!(client.conn.characters[0].delete_request_time, 0);
    assert_eq!(server.enter_world.character_owner(50), None);
    assert_eq!(server.enter_world.reserve_character(50, 2), Ok(true));
    server.enter_world.release_character(50, 2);
}

#[tokio::test]
async fn lobby_delete_cannot_mutate_another_accounts_lobby_character() {
    let server = test_server_with(creation_data());
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    lobby_character(&mut client, 50);
    client.conn.characters[0].account_id = 1008;
    lobby_delete(&server, &mut client, 50).await.unwrap();
    // sub_397B2F50 / sub_391EA8C0: id:u32, status:u8=0, two zero i64 timestamps.
    assert_eq!(
        client.drain(),
        vec![(
            crate::opcodes::sc::SCDeleteCharacterResponsePacket,
            vec![50, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        )]
    );
    assert_eq!(client.conn.characters[0].delete_request_time, 0);
    assert_eq!(server.enter_world.character_owner(50), None);
}

#[tokio::test]
async fn lobby_delete_blocks_select_while_waiting_and_releases_on_cancellation() {
    let server = test_server_with(creation_data());
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    lobby_character(&mut client, 50);
    let transaction = server.save.transaction_lock().await;
    let mut deletion = Box::pin(lobby_delete(&server, &mut client, 50));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut deletion)
            .await
            .is_err()
    );
    assert_eq!(server.enter_world.reserve_character(50, 2), Err(1));
    drop(deletion);
    assert_eq!(server.enter_world.reserve_character(50, 2), Ok(true));
    server.enter_world.release_character(50, 2);
    drop(transaction);
}

#[tokio::test]
async fn create_character_starting_kit_helpers_preserve_equipment_and_stacks() {
    use crate::data::character_templates::EquipItemsTemplate;
    use crate::data::items::{EquipTemplate, GradeTemplate, ItemTemplate, TemplateKind};
    use crate::models::character::Inventory;

    let mut data = GameData::empty();
    data.items.config.durability_const = 1.0;
    data.items.grades.insert(
        0,
        GradeTemplate {
            grade: 0,
            grade_order: 0,
            durability: 1.0,
        },
    );
    data.items.templates.insert(
        6001,
        ItemTemplate {
            kind: TemplateKind::Armor(EquipTemplate {
                slot_type_id: 3,
                coverage: 10,
                durability_ratio: 1.0,
                durability_multiplier: 100,
                ..Default::default()
            }),
            ..ItemTemplate::plain(6001, 1)
        },
    );
    data.items.templates.insert(
        7001,
        ItemTemplate {
            kind: TemplateKind::BodyPart {
                slot_type_id: 23,
                model_id: 1,
            },
            ..ItemTemplate::plain(7001, 1)
        },
    );
    data.items
        .templates
        .insert(8001, ItemTemplate::plain(8001, 10));
    let server = test_server_with(data);
    let inventory = Inventory::new(1, 50, 50, &server.items, &server.ids.container);
    let kit = EquipItemsTemplate {
        shirt: (6001, 0),
        ..Default::default()
    };
    let mut created_items = Vec::new();
    super::equip_pack(&server, &inventory, &kit, &mut |_| {}, &mut created_items);
    assert!(
        super::set_equip_item_template(&server, &inventory, 7001, 19, 0, &mut created_items)
            .is_some()
    );
    assert!(super::acquire_default_item_recorded(
        &server,
        inventory.bag(),
        8001,
        25,
        0,
        &mut created_items
    )
    .unwrap());

    let equipment = inventory.equipment().lock().unwrap();
    let chest = equipment.get_item_by_slot(2).unwrap();
    assert_eq!(chest.template_id, 6001);
    assert_eq!(chest.details.durability, 10);
    assert_eq!(equipment.get_item_by_slot(19).unwrap().template_id, 7001);
    drop(equipment);
    let mut stacks: Vec<_> = inventory
        .bag()
        .lock()
        .unwrap()
        .items
        .iter()
        .map(|item| (item.slot, item.template_id, item.count))
        .collect();
    stacks.sort_unstable();
    assert_eq!(stacks, vec![(0, 8001, 10), (1, 8001, 10), (2, 8001, 5)]);
}

#[tokio::test]
async fn new_character_setup_error_discards_inventory_before_id_reuse() {
    let mut data = creation_data();
    data.skills = Default::default(); // fails after containers, equipment and supplies exist
    let server = test_server_with(data);
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    assert!(
        create(&server, &mut client.conn, creation_request("Rejected"))
            .await
            .is_err()
    );
    assert!(
        server.items.all_containers().is_empty(),
        "rejected inventory remains globally saveable"
    );
    assert!(!server.names.name_exists("Rejected"));
    assert_eq!(server.ids.character.get_next_id().unwrap(), 1);
    assert_eq!(
        server.ids.item.get_next_id().unwrap(),
        crate::managers::id::ITEM_ID.first_id
    );
    assert_eq!(
        server.ids.container.get_next_id().unwrap(),
        crate::managers::id::CONTAINER_ID.first_id
    );
    assert!(client.conn.characters.is_empty());
}

#[tokio::test]
async fn new_character_registration_waits_for_world_save_lock() {
    let server = test_server_with(creation_data());
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    let transaction = server.save.transaction_lock().await;
    let creation = create(&server, &mut client.conn, creation_request("Waiting"));
    tokio::pin!(creation);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut creation)
            .await
            .is_err()
    );
    assert!(
        server.items.all_containers().is_empty(),
        "world save can see uncommitted starter items"
    );
    assert!(!server.names.name_exists("Waiting"));
    drop(transaction);
    tokio::time::timeout(std::time::Duration::from_secs(2), creation)
        .await
        .unwrap()
        .unwrap();
    assert!(server.items.all_containers().is_empty());
    assert!(!server.names.name_exists("Waiting"));
}

#[tokio::test]
async fn new_character_cleanup_restores_a_deletion_queued_after_setup_started() {
    use crate::models::character::Inventory;
    let server = test_server_with(creation_data());
    let unrelated = Inventory::new(99, 50, 50, &server.items, &server.ids.container);
    let deleted = server
        .items
        .create(2001, 4, 0, 1, &server.game_data.items, &server.ids.item, 0)
        .unwrap();
    let deleted_id = deleted.id;
    unrelated
        .bag()
        .lock()
        .unwrap()
        .add_new_item(deleted, 0, &server.game_data.items)
        .unwrap();
    let survivor = server
        .items
        .create(2002, 5, 0, 1, &server.game_data.items, &server.ids.item, 0)
        .unwrap();
    let survivor_id = survivor.id;
    unrelated
        .bag()
        .lock()
        .unwrap()
        .add_new_item(survivor, 1, &server.game_data.items)
        .unwrap();

    let _transaction = server.save.transaction_lock().await;
    let id = server.ids.character.get_next_id().unwrap();
    server.names.add_character(id, "Rejected", 1007);
    let mut creation = super::NewCharacter {
        server: &server,
        id,
        created_items: Vec::new(),
        committed: false,
    };
    let inventory = Inventory::new(id, 50, 50, &server.items, &server.ids.container);
    assert!(server.items.removed_items().is_empty());
    // Another owner's ItemTxn can queue/release an ID AFTER setup begins: the save lock
    // excludes database snapshots, not these synchronous item mutations.
    {
        let _economy = server.systems.economy.ops_lock();
        let mut bag = unrelated.bag().lock().unwrap();
        let index = bag.items.iter().position(|i| i.id == deleted_id).unwrap();
        bag.items.remove(index);
        server.items.release_id(deleted_id, &server.ids.item);
    }
    server.items.mark_for_db_deletion(0x0100_0099);
    super::acquire_default_item_recorded(
        &server,
        inventory.bag(),
        2001,
        2,
        0,
        &mut creation.created_items,
    )
    .unwrap();
    assert_eq!(
        creation.created_items,
        vec![(deleted_id, true)],
        "canceled deletion must be captured at allocation"
    );
    assert_eq!(server.items.removed_items(), vec![0x0100_0099]);
    drop(creation);

    let mut removed = server.items.removed_items();
    removed.sort_unstable();
    assert_eq!(removed, vec![deleted_id, 0x0100_0099]);
    assert!(inventory.bag().lock().unwrap().items.is_empty());
    assert!(server.items.item_by_id(deleted_id).is_none());
    assert_eq!(server.items.item_by_id(survivor_id).unwrap().count, 5);
    assert_eq!(server.items.prepare_save().item_ids(), vec![survivor_id]);
    assert!(server
        .items
        .all_containers()
        .iter()
        .all(|c| c.lock().unwrap().owner_id == 99));
    assert!(!server.names.name_exists("Rejected"));
    assert_eq!(server.ids.character.get_next_id().unwrap(), id);
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_new_character_later_item_failure_cleans_up_before_id_reuse() {
    use crate::models::character::Inventory;
    use crate::systems::social::achievement_persistence::temporary_shadow_definition;
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
        .unwrap();
    for table in [
        "items",
        "item_containers",
        "characters",
        "options",
        "abilities",
        "actabilities",
        "appellations",
        "character_active_buffs",
        "portal_book_coords",
        "portal_visited_district",
        "friends",
        "blocked",
        "skills",
        "quests",
        "completed_quests",
        "accounts",
        "character_records",
        "character_achievements",
        "character_crafts",
    ] {
        let (_, definition): (String, String) =
            sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(&temporary_shadow_definition(&definition))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("SET SESSION sql_mode='STRICT_ALL_TABLES'")
        .execute(&pool)
        .await
        .unwrap();
    // Equipment (count 1) is written first; a LATER starter item (count 300) fails.
    sqlx::query("ALTER TABLE items MODIFY `count` TINYINT UNSIGNED NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    let mut server = test_server_with(creation_data());
    server.db = pool.clone();
    let unrelated = Inventory::new(99, 50, 50, &server.items, &server.ids.container);
    let item = server
        .items
        .create(2001, 4, 0, 1, &server.game_data.items, &server.ids.item, 0)
        .unwrap();
    let unrelated_id = item.id;
    unrelated
        .bag()
        .lock()
        .unwrap()
        .add_new_item(item, 0, &server.game_data.items)
        .unwrap();
    let reused_deletion = u64::from(server.ids.item.get_next_id().unwrap());
    server.items.release_id(reused_deletion, &server.ids.item);
    server.items.mark_for_db_deletion(0x0100_0099);
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);

    create(&server, &mut client.conn, creation_request("Rejected"))
        .await
        .unwrap();
    assert!(client.conn.characters.is_empty());
    assert!(client
        .drain_ops()
        .contains(&crate::opcodes::sc::SCCharacterCreationFailedPacket));
    for table in [
        "characters",
        "items",
        "item_containers",
        "abilities",
        "skills",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM `{table}`"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "partial SQL rollback in {table}");
    }
    assert!(!server.names.name_exists("Rejected"));
    assert!(
        server
            .items
            .all_containers()
            .iter()
            .all(|c| c.lock().unwrap().owner_id == 99),
        "rejected inventory remains cached"
    );
    assert_eq!(server.items.prepare_save().item_ids(), vec![unrelated_id]);
    let mut removed = server.items.removed_items();
    removed.sort_unstable();
    assert_eq!(removed, vec![reused_deletion, 0x0100_0099]);
    assert_eq!(server.items.item_by_id(unrelated_id).unwrap().count, 4);

    sqlx::query("ALTER TABLE items MODIFY `count` INT NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    create(&server, &mut client.conn, creation_request("Accepted"))
        .await
        .unwrap();
    let c = &client.conn.characters[0];
    assert_eq!(c.id, 1, "failed character ID must be reused");
    let counts = |container: &crate::models::container::ContainerRef| {
        let mut items: Vec<_> = container
            .lock()
            .unwrap()
            .items
            .iter()
            .map(|i| (i.template_id, i.count))
            .collect();
        items.sort_unstable();
        items
    };
    assert_eq!(counts(c.inventory.bag()), vec![(2001, 2), (2002, 300)]);
    assert_eq!(counts(c.inventory.equipment()), vec![(2000, 1)]);
    let rows: Vec<(u32, i32)> =
        sqlx::query_as("SELECT template_id, `count` FROM items ORDER BY template_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![(2000, 1), (2001, 2), (2002, 300)]);
    assert!(server.items.prepare_save_for_owner(1).item_ids().is_empty());
    assert_eq!(server.items.prepare_save().item_ids(), vec![unrelated_id]);
    assert_eq!(server.items.removed_items(), vec![0x0100_0099]);
    // The next item save cannot resurrect the rejected character's inventory.
    let mut tx = pool.begin().await.unwrap();
    server.items.save(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    server.items.commit_save();
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 4);
    assert_eq!(counts(unrelated.bag()), vec![(2001, 4)]);
    pool.close().await;
}

#[test]
fn constructor_does_not_call_deps() {
    let manager = CharacterData::default();

    assert!(manager.templates.is_empty());
    assert!(manager.ability_items.is_empty());
    assert!(manager.appellations.is_empty());
    assert!(manager.actabilities.is_empty());
    assert!(manager.actability_categories.is_empty());
    assert!(manager.expert_limits.is_empty());
}

#[tokio::test]
async fn selected_ability_starter_is_first_and_survives_saved_hotbar_roundtrip() {
    let server = test_server_with(starting_skill_data());
    for (ability, skill_id) in STARTERS {
        let mut c = new_character(ability);
        c.set_action(10, ActionSlotType::ItemType, 4045);
        c.set_action(11, ActionSlotType::ItemType, 18791);
        initialize_starting_skills(&server, &mut c).unwrap();

        assert_eq!(c.skills.skills.len(), 1);
        assert_eq!(c.skills.skills[0].id, skill_id);
        assert_eq!(c.slots[1].kind, ActionSlotType::Spell);
        assert_eq!(c.slots[1].action_id, u64::from(skill_id));
        assert_eq!(c.slots[2].action_id, 16064, "ranged attack retained");
        assert_eq!(c.slots[3].action_id, 2, "melee attack retained");
        assert_eq!(c.slots[6].kind, ActionSlotType::None);
        assert_eq!(c.slots[10].action_id, 4045);
        assert_eq!(c.slots[10].kind, ActionSlotType::ItemType);
        assert_eq!(c.slots[11].action_id, 18791);
        assert_eq!(c.slots[14].action_id, 14495);
        assert_eq!(c.slots[21].action_id, 10594);

        let mut saved = vec![ActionSlot::default(); MAX_ACTION_SLOTS];
        read_action_slots(&mut saved, &c.action_slots_blob());
        assert_eq!(saved, c.slots);
    }
}

#[tokio::test]
async fn dense_shared_defaults_do_not_bury_sorcery_starter() {
    let mut data = starting_skill_data();
    data.skills.default_skills.clear();
    for slot in 1..=15 {
        let skill_id = 1000 + u32::from(slot);
        data.skills.add_skill(SkillTemplate {
            id: skill_id,
            ..SkillTemplate::default()
        });
        data.skills.default_skills.push(DefaultSkill {
            skill_id,
            slot,
            add_to_slot: true,
        });
    }
    let server = test_server_with(data);
    let mut c = new_character(7);
    initialize_starting_skills(&server, &mut c).unwrap();
    assert_eq!(c.slots[1].action_id, 10752);
    assert_eq!(c.slots[16].action_id, 1001);
    for slot in 2..=15 {
        assert_eq!(c.slots[slot].action_id, 1000 + slot as u64);
    }
}

#[tokio::test]
async fn additional_starting_spells_skip_occupied_utility_slots() {
    let mut data = starting_skill_data();
    data.skills.add_skill(SkillTemplate {
        id: 30000,
        ability_id: 7,
        ability_level: 1,
        auto_learn: true,
        need_learn: true,
        show: true,
        ..SkillTemplate::default()
    });
    data.skills.finish();
    let server = test_server_with(data);
    let mut c = new_character(7);
    c.set_action(4, ActionSlotType::ItemType, 18791);
    initialize_starting_skills(&server, &mut c).unwrap();
    assert_eq!(c.slots[1].action_id, 10752);
    assert_eq!(c.slots[4].action_id, 18791);
    assert_eq!(c.slots[4].kind, ActionSlotType::ItemType);
    assert_eq!(c.slots[5].action_id, 30000);
    assert_eq!(c.skills.skills.len(), 2);
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn starting_hotbar_real_compact_db() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut data = GameData::empty();
    data.skills = crate::data::SkillData::load(&conn).unwrap();
    let server = test_server_with(data);
    for (ability, skill_id) in STARTERS {
        assert_eq!(
            server.game_data.skills.start_ability_skills(ability),
            Some([skill_id].as_slice())
        );
        let mut c = new_character(ability);
        initialize_starting_skills(&server, &mut c).unwrap();
        assert_eq!(c.slots[1].action_id, u64::from(skill_id));
        assert_eq!(c.skills.skills[0].id, skill_id);
    }
}

#[test]
fn character_slot_limit_is_base_plus_purchased_slots() {
    assert_eq!(character_slot_limit(6, 0), 6);
    assert_eq!(character_slot_limit(6, 2), 8);
    assert_eq!(character_slot_limit(8, 2), 10);
    assert_eq!(character_slot_limit(255, 1), 255);
}

#[test]
fn create_cap_is_base_plus_purchased_slots() {
    // Base only: six full, and a seventh refused.
    assert!(may_create_character(6, 0, 5));
    assert!(!may_create_character(6, 0, 6));
    // Purchased slots extend the same limit up to the advertised total.
    assert!(may_create_character(6, 2, 6));
    assert!(may_create_character(6, 2, 7));
    assert!(!may_create_character(6, 2, 8));
    // A DB error on the extra-slots read gives 0, i.e. the base fallback.
    assert!(!may_create_character(6, 0, 6));
}

/// `Characters:MaxPerAccount` sets the base; `accounts.extra_character_slots` adds to it, and the
/// real create handler refuses the character past the advertised limit with native error 9.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_create_limit_is_config_base_plus_purchased_slots() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.characters.max_per_account = 2;
    let mut server = test_server_with_config(creation_data(), config);
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1010, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut client = TestClient::new(&server, 1, 1010);

    for index in 0..2 {
        lobby_create(
            &server,
            &mut client,
            creation_request(&format!("Base{index}")),
        )
        .await
        .unwrap();
        assert!(client
            .drain_ops()
            .contains(&crate::opcodes::sc::SCCreateCharacterResponsePacket));
    }
    lobby_create(&server, &mut client, creation_request("RefusedBase"))
        .await
        .unwrap();
    assert_creation_failure(&mut client, 9);

    sqlx::query("UPDATE accounts SET extra_character_slots = 2 WHERE account_id = 1010")
        .execute(&pool)
        .await
        .unwrap();
    for index in 0..2 {
        lobby_create(
            &server,
            &mut client,
            creation_request(&format!("Paid{index}")),
        )
        .await
        .unwrap();
        assert!(client
            .drain_ops()
            .contains(&crate::opcodes::sc::SCCreateCharacterResponsePacket));
    }
    lobby_create(&server, &mut client, creation_request("RefusedPaid"))
        .await
        .unwrap();
    assert_creation_failure(&mut client, 9);

    assert_eq!(persisted_lobby_count(&pool, 1010).await, 4);
    pool.close().await;
}

// ---- lobby character edit (CSEditCharacter, pre-select period) ----

fn edit_data() -> GameData {
    use crate::data::character_templates::AbilitySupplyItem;
    let mut data = creation_data();
    // A second creatable race, so a forged race change reaches the equality check.
    let mut other = data.characters.template(1, 1).unwrap().clone();
    other.race = 3;
    data.characters.templates.insert(19, other);
    // Ability 1's starting kit differs from ability 7's, so a rebuild is visible.
    data.characters.ability_items.insert(
        1,
        crate::data::character_templates::AbilityItems {
            supplies: vec![AbilitySupplyItem {
                id: 2003,
                amount: 1,
                grade: 0,
            }],
            ..Default::default()
        },
    );
    data.items
        .templates
        .insert(2003, crate::data::items::ItemTemplate::plain(2003, 1000));
    data
}

fn edit_ai_config() -> AppConfig {
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.characters.pre_select_period = true;
    config
}

fn appearance_params() -> crate::models::unit_model::UnitCustomModelParams {
    use crate::models::unit_model::{FaceModel, UnitCustomModelParams, UnitCustomModelType};
    UnitCustomModelParams {
        kind: UnitCustomModelType::FACE,
        hair_color_id: 7,
        skin_color_id: 3,
        model_id: 9,
        face: Some(FaceModel::default()),
    }
}

async fn lobby_edit(
    server: &crate::GameServer,
    client: &mut crate::test_support::TestClient,
    character_id: u32,
    request: CreateCharacterRequest,
) -> crate::Result<()> {
    use crate::net::registry::ClientPacket;
    crate::packets::c2g_world::CSEditCharacterPacket {
        character_id,
        request,
    }
    .handle(server, &mut client.conn)
    .await
}

/// The leading fields and the customizing blob of an `SCEditCharacterResponsePacket` body,
/// spelled out from the 1.2 `Character.Write` layout (`FUN_39620030`).
fn edit_response_appearance(body: &[u8]) -> (u32, String, u8, u8, u8, (u8, u32, u32, u32)) {
    let mut s = aaemu_commons::PacketStream::from_bytes(body.to_vec());
    let id = s.read_u32().unwrap();
    let name = s.read_string().unwrap();
    let race = s.read_u8().unwrap();
    let gender = s.read_u8().unwrap();
    let level = s.read_u8().unwrap();
    s.read_i32().unwrap(); // hp
    s.read_i32().unwrap(); // mp
    s.read_u32().unwrap(); // zone
    s.read_u32().unwrap(); // faction
    let _ = s.read_string().unwrap(); // faction name
    s.read_u32().unwrap(); // expedition
    s.read_u32().unwrap(); // family
    for _ in 0..28 {
        s.read_i32().unwrap(); // equipment
    }
    s.read_bytes(3).unwrap(); // abilities
    s.read_i64().unwrap(); // x
    s.read_i64().unwrap(); // y
    s.read_f32().unwrap(); // z
    let kind = s.read_u8().unwrap();
    let mut appearance = (kind, 0, 0, 0);
    if kind >= 1 {
        appearance.1 = s.read_u32().unwrap();
    }
    if kind >= 2 {
        appearance.2 = s.read_u32().unwrap();
        appearance.3 = s.read_u32().unwrap();
    }
    (id, name, race, gender, level, appearance)
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_lobby_edit_rebuilds_and_persists_the_appearance_name_and_ability() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut server = test_server_with_config(edit_data(), edit_ai_config());
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut client = TestClient::new(&server, 1, 1007);
    lobby_create(&server, &mut client, creation_request("Editable"))
        .await
        .unwrap();
    client.drain();
    let id = client.conn.characters[0].id;
    // A GM can mute a never-played character while it is offline; the edit rebuild must keep it.
    let mute_until = chrono::Utc::now().timestamp() + 1_800;
    sqlx::query("UPDATE characters SET chat_restricted_until = ? WHERE id = ?")
        .bind(crate::db::from_unix(mute_until))
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let old_items: Vec<u64> = sqlx::query_scalar("SELECT CAST(id AS UNSIGNED) FROM items")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(old_items.len(), 3, "head armor + two supplies");

    let mut req = creation_request("Renamed");
    req.model_params = appearance_params();
    req.ability1 = 1;
    lobby_edit(&server, &mut client, id, req).await.unwrap();
    let frames = client.drain();
    assert_eq!(frames.len(), 1, "exactly one response: {frames:?}");
    assert_eq!(
        frames[0].0,
        crate::opcodes::sc::SCEditCharacterResponsePacket
    );
    let (rid, name, race, gender, level, appearance) = edit_response_appearance(&frames[0].1);
    assert_eq!((rid, name.as_str()), (id, "Renamed"));
    assert_eq!((race, gender, level), (1, 1, 1));
    assert_eq!(appearance, (3, 7, 3, 9));

    // Persisted: the row, the name manager and the rebuilt starting kit.
    let (db_name, db_hair, db_ability, db_level): (String, i64, i64, i64) = sqlx::query_as(
        "SELECT name, ORD(SUBSTRING(unit_model_params, 2, 1)), CAST(ability1 AS SIGNED), CAST(level AS SIGNED) \
         FROM characters WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (db_name.as_str(), db_hair, db_ability, db_level),
        ("Renamed", 7, 1, 1)
    );
    let db_mute: chrono::NaiveDateTime =
        sqlx::query_scalar("SELECT chat_restricted_until FROM characters WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        crate::db::unix(Some(db_mute)),
        mute_until,
        "the rebuild keeps the persisted chat mute"
    );
    assert_eq!(server.names.get_character_id("Renamed"), id);
    assert!(!server.names.name_exists("Editable"));
    let templates: Vec<u32> = sqlx::query_scalar(
        "SELECT CAST(i.template_id AS UNSIGNED) FROM items i JOIN item_containers ic \
         ON ic.container_id = i.container_id WHERE ic.owner_id = ? ORDER BY i.template_id",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        templates,
        vec![2002, 2003],
        "ability 1's kit plus the general supplies replaced ability 7's"
    );
    for old in &old_items {
        let remaining_old: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE id = ?")
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining_old, 0, "the old item row {old} is gone");
    }
    let skills: Vec<u32> =
        sqlx::query_scalar("SELECT CAST(id AS UNSIGNED) FROM skills WHERE owner = ?")
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(skills.contains(&18132), "ability 1's starter: {skills:?}");
    assert!(!skills.contains(&10752), "ability 7's starter: {skills:?}");

    // The lobby copy is updated, so a relist carries the edit.
    use crate::net::registry::ClientPacket;
    crate::packets::c2g::CSListCharacterPacket {
        size: 0,
        data: Vec::new(),
    }
    .handle(&server, &mut client.conn)
    .await
    .unwrap();
    let listed = client
        .drain()
        .into_iter()
        .find(|(op, _)| *op == crate::opcodes::sc::SCCharacterListPacket)
        .expect("SCCharacterListPacket");
    assert!(listed.1.windows(7).any(|w| w == b"Renamed"));
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_lobby_edit_is_refused_outside_the_pre_select_period() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut config = edit_ai_config();
    config.characters.pre_select_period = false;
    let mut server = test_server_with_config(edit_data(), config);
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut client = TestClient::new(&server, 1, 1007);
    lobby_create(&server, &mut client, creation_request("Periodic"))
        .await
        .unwrap();
    client.drain();
    let id = client.conn.characters[0].id;

    let mut req = creation_request("Periodic");
    req.model_params = appearance_params();
    lobby_edit(&server, &mut client, id, req).await.unwrap();
    assert_eq!(client.drain(), vec![], "no reply outside the period");
    let (name, hair): (String, i64) = sqlx::query_as(
        "SELECT name, ORD(SUBSTRING(unit_model_params, 2, 1)) FROM characters WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((name.as_str(), hair), ("Periodic", 0));
    assert_eq!(client.conn.characters[0].unit.name, "Periodic");
    assert_eq!(client.conn.characters[0].unit.model_params.kind.0, 0);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_lobby_edit_refuses_a_character_of_another_account() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut server = test_server_with_config(edit_data(), edit_ai_config());
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000), (1008, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut owner = TestClient::new(&server, 1, 1007);
    lobby_create(&server, &mut owner, creation_request("Owned"))
        .await
        .unwrap();
    owner.drain();
    let id = owner.conn.characters[0].id;

    // A second account's connection with a stale/forged copy of the foreign character in its
    // own lobby list must not be able to edit it (the copy keeps account_id 1007).
    let mut other = TestClient::new(&server, 2, 1008);
    other.conn.characters.push(owner.conn.characters[0].clone());
    let mut req = creation_request("Owned");
    req.model_params = appearance_params();
    lobby_edit(&server, &mut other, id, req).await.unwrap();
    assert_eq!(other.drain(), vec![], "no reply for a foreign character");
    let (name, hair): (String, i64) = sqlx::query_as(
        "SELECT name, ORD(SUBSTRING(unit_model_params, 2, 1)) FROM characters WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((name.as_str(), hair), ("Owned", 0));
    assert_eq!(other.conn.characters[0].unit.model_params.kind.0, 0);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_lobby_edit_refuses_deleted_played_and_pending_deletion_characters() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut server = test_server_with_config(edit_data(), edit_ai_config());
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut client = TestClient::new(&server, 1, 1007);
    lobby_create(&server, &mut client, creation_request("Played"))
        .await
        .unwrap();
    client.drain();
    let id = client.conn.characters[0].id;
    async fn assert_unchanged(
        pool: &sqlx::MySqlPool,
        client: &mut crate::test_support::TestClient,
        id: u32,
    ) {
        let (name, hair): (String, i64) = sqlx::query_as(
            "SELECT name, ORD(SUBSTRING(unit_model_params, 2, 1)) FROM characters WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!((name.as_str(), hair), ("Played", 0));
        assert!(
            client.drain().is_empty(),
            "no reply for a refused lobby edit"
        );
    }

    // Not in the account's lobby list (a deleted character is not listed).
    let mut req = creation_request("Played");
    req.model_params = appearance_params();
    let removed = client.conn.characters.remove(0);
    lobby_edit(&server, &mut client, id, req.clone())
        .await
        .unwrap();
    assert_unchanged(&pool, &mut client, id).await;
    client.conn.characters.push(removed);
    assert_eq!(client.conn.characters[0].unit.name, "Played");

    // Pending deletion occupies the list until deleted=1 but cannot be edited. The persisted
    // row is authoritative; the cached lobby copy is not consulted for this.
    sqlx::query(
        "UPDATE characters SET delete_request_time = UTC_TIMESTAMP(),          delete_time = DATE_ADD(UTC_TIMESTAMP(), INTERVAL 1 DAY) WHERE id = ?",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    lobby_edit(&server, &mut client, id, req.clone())
        .await
        .unwrap();
    assert_unchanged(&pool, &mut client, id).await;
    sqlx::query("UPDATE characters SET delete_request_time = '0001-01-01', delete_time = '0001-01-01' WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

    // A character that has entered the world keeps its progress. The cached copy still says
    // online_time 0, so this only works with the row re-read under the save lock.
    sqlx::query("UPDATE characters SET online_time = 5 WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(client.conn.characters[0].online_time, 0, "stale lobby copy");
    lobby_edit(&server, &mut client, id, req.clone())
        .await
        .unwrap();
    assert_unchanged(&pool, &mut client, id).await;
    sqlx::query("UPDATE characters SET online_time = 0, leave_time = UTC_TIMESTAMP() WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    lobby_edit(&server, &mut client, id, req.clone())
        .await
        .unwrap();
    assert_unchanged(&pool, &mut client, id).await;
    sqlx::query("UPDATE characters SET leave_time = '0001-01-01' WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

    // A session that is in the world (or past the lobby state) cannot edit its own row.
    let player = crate::world::Player::new(
        client.conn.characters[0].clone(),
        crate::net::connection::ConnectionHandle::detached(
            98,
            "127.0.0.1:50002".parse().unwrap(),
            1007,
        ),
    );
    client.conn.active_char = Some(player);
    lobby_edit(&server, &mut client, id, req.clone())
        .await
        .unwrap();
    assert_unchanged(&pool, &mut client, id).await;
    client.conn.active_char = None;
    client.conn.state = crate::net::connection::GameState::World;
    lobby_edit(&server, &mut client, id, req.clone())
        .await
        .unwrap();
    assert_unchanged(&pool, &mut client, id).await;
    client.conn.state = crate::net::connection::GameState::Lobby;

    // A select/enter in flight owns the character.
    assert_eq!(server.enter_world.reserve_character(id, 99), Ok(true));
    lobby_edit(&server, &mut client, id, req.clone())
        .await
        .unwrap();
    assert_unchanged(&pool, &mut client, id).await;
    server.enter_world.release_character(id, 99);

    // A departure save still has the old row in flight.
    let departing = crate::world::Player::new(
        client.conn.characters[0].clone(),
        crate::net::connection::ConnectionHandle::detached(
            99,
            "127.0.0.1:50001".parse().unwrap(),
            1007,
        ),
    );
    server.save.remember_departing(&departing);
    lobby_edit(&server, &mut client, id, req.clone())
        .await
        .unwrap();
    assert_unchanged(&pool, &mut client, id).await;
    server.save.forget_departing(id);

    // Control: the same edit now succeeds.
    lobby_edit(&server, &mut client, id, req).await.unwrap();
    let frames = client.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].0,
        crate::opcodes::sc::SCEditCharacterResponsePacket
    );
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_lobby_edit_refuses_a_race_change_invalid_names_and_taken_names() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut server = test_server_with_config(edit_data(), edit_ai_config());
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut client = TestClient::new(&server, 1, 1007);
    lobby_create(&server, &mut client, creation_request("Edited"))
        .await
        .unwrap();
    client.drain();
    let id = client.conn.characters[0].id;
    lobby_create(&server, &mut client, creation_request("Taken"))
        .await
        .unwrap();
    client.drain();
    let taken_id = client.conn.characters[1].id;

    // Race is fixed in the edit UI; a forged race change is refused even for a creatable race.
    let mut req = creation_request("Edited");
    req.race = 3;
    lobby_edit(&server, &mut client, id, req).await.unwrap();
    assert!(client.drain().is_empty());
    let race: i64 = sqlx::query_scalar("SELECT CAST(race AS SIGNED) FROM characters WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(race, 1);

    // Names: create's rules apply, including another character's name.
    for name in ["bad name", "Taken"] {
        let mut req = creation_request(name);
        req.model_params = appearance_params();
        lobby_edit(&server, &mut client, id, req).await.unwrap();
        assert!(client.drain().is_empty(), "name {name:?} must be refused");
        let stored: String = sqlx::query_scalar("SELECT name FROM characters WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, "Edited");
    }
    assert_eq!(server.names.get_character_id("Taken"), taken_id);
    assert_eq!(server.names.get_character_id("Edited"), id);

    // Control: a valid new name is applied and swaps the name manager entries.
    let mut req = creation_request("Fresh");
    req.model_params = appearance_params();
    lobby_edit(&server, &mut client, id, req).await.unwrap();
    let frames = client.drain();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].0,
        crate::opcodes::sc::SCEditCharacterResponsePacket
    );
    assert_eq!(server.names.get_character_id("Fresh"), id);
    assert!(!server.names.name_exists("Edited"));
    pool.close().await;
}

/// The rebuild only touches the containers character creation fills (equipment, bag, bank).
/// A never-played character can already own mail attachments and mate/slave equipment; their
/// containers and items must survive an edit that succeeds (MySQL) or fails (this test, no DB).
#[tokio::test]
async fn lobby_edit_rebuild_keeps_mail_and_mate_containers_without_mysql() {
    use crate::models::character::Inventory;
    use crate::models::container::{slot_type, ContainerRef};

    let server = test_server_with_config(edit_data(), edit_ai_config());
    server.db.close().await;
    let owner = 7u32;
    let now = chrono::Utc::now().timestamp();
    let add = |slot: u8, mate: u32, template: u32, item_slot: i32| -> (ContainerRef, u64) {
        let c = server
            .items
            .container_for_character(owner, slot, mate, &server.ids.container);
        let item = server
            .items
            .create(
                template,
                1,
                0,
                1,
                &server.game_data.items,
                &server.ids.item,
                now,
            )
            .expect("item template");
        let id = item.id;
        c.lock()
            .expect("container poisoned")
            .add_new_item(item, item_slot, &server.game_data.items)
            .unwrap_or_else(|_| panic!("place item {template} in slot {slot}"));
        (c, id)
    };
    let (equipment, old_equipment) = add(slot_type::EQUIPMENT, 0, 2000, 0);
    let (_, old_bag) = add(slot_type::INVENTORY, 0, 2001, -1);
    let (_, old_bank) = add(slot_type::BANK, 0, 2001, -1);
    let (mail, mail_item) = add(slot_type::MAIL, 0, 2002, -1);
    let (mate_equipment, mate_item) = add(slot_type::EQUIPMENT_MATE, 55, 2000, 0);
    let (slave_equipment, slave_item) = add(slot_type::EQUIPMENT_SLAVE, 66, 2002, 0);

    let persistent = Inventory::new(owner, 50, 50, &server.items, &server.ids.container);
    let built = Inventory::standalone(owner, 50, 50);
    let built_item = server
        .items
        .create(
            2003,
            1,
            0,
            1,
            &server.game_data.items,
            &server.ids.item,
            now,
        )
        .expect("built item");
    let built_id = built_item.id;
    built
        .bag()
        .lock()
        .expect("container poisoned")
        .add_new_item(built_item, -1, &server.game_data.items)
        .expect("place built item");

    let batch = server.items.prepare_rebuilt_batch(&built, &persistent);
    assert_eq!(
        batch.container_count(),
        3,
        "only equipment, bag and bank are rewritten"
    );
    assert_eq!(batch.item_ids(), vec![built_id]);

    let removed = server.items.commit_rebuilt_containers(&built, &persistent);
    let mut expected = vec![old_equipment, old_bag, old_bank];
    expected.sort_unstable();
    let mut actual = removed.clone();
    actual.sort_unstable();
    assert_eq!(
        actual, expected,
        "only the creation containers' items are replaced"
    );
    assert!(!removed.contains(&mail_item));
    assert!(!removed.contains(&mate_item));
    assert!(!removed.contains(&slave_item));

    assert!(equipment
        .lock()
        .expect("container poisoned")
        .items
        .is_empty());
    assert_eq!(mail.lock().expect("container poisoned").items.len(), 1);
    assert_eq!(
        mate_equipment
            .lock()
            .expect("container poisoned")
            .items
            .len(),
        1
    );
    assert_eq!(
        slave_equipment
            .lock()
            .expect("container poisoned")
            .items
            .len(),
        1
    );
    assert!(server.items.item_by_id(mail_item).is_some());
    assert!(server.items.item_by_id(mate_item).is_some());
    assert!(server.items.item_by_id(slave_item).is_some());
    assert!(server.items.item_by_id(built_id).is_some());
}

/// A database that cannot be read refuses the edit without closing the connection (the
/// lobby copy stays untouched). The in-world, state and stale-cache guards are mutation-checked
/// by the MySQL test below, which needs a reachable row to tell a refusal from a failed read.
#[tokio::test]
async fn lobby_edit_survives_an_unavailable_database() {
    let server = test_server_with_config(edit_data(), edit_ai_config());
    server.db.close().await;
    let mut client = crate::test_support::TestClient::new(&server, 1, 1007);
    let mut character = crate::test_support::test_character(7, "Lobby", 1.0, 2.0);
    character.account_id = 1007;
    client.conn.characters.push(character);
    let request = creation_request("Lobby");

    lobby_edit(&server, &mut client, 7, request).await.unwrap();
    assert!(
        client.drain().is_empty(),
        "an unreadable row refuses the edit"
    );
    assert!(!client.conn.is_closing());
    assert_eq!(client.conn.characters[0].unit.name, "Lobby");
    assert_eq!(
        client.conn.characters[0].unit.model_params.kind.0, 0,
        "the lobby copy is untouched"
    );
}

/// A never-played character can already own mail attachments and mate equipment. The edit
/// rebuilds only the equipment/bag/bank rows; the mail and mate-equipment rows must survive.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_lobby_edit_keeps_mail_and_mate_equipment_rows() {
    use crate::test_support::TestClient;
    let Some(pool) = private_lobby_pool().await else {
        return;
    };
    let mut server = test_server_with_config(edit_data(), edit_ai_config());
    server.db = pool.clone();
    sqlx::query("INSERT INTO accounts (account_id, labor) VALUES (1007, 1000)")
        .execute(&pool)
        .await
        .unwrap();
    let mut client = TestClient::new(&server, 1, 1007);
    lobby_create(&server, &mut client, creation_request("Mailed"))
        .await
        .unwrap();
    client.drain();
    let id = client.conn.characters[0].id;

    // The creation save wrote the character's empty container rows, including MAIL.
    let mail_container: u64 = sqlx::query_scalar(
        "SELECT CAST(container_id AS UNSIGNED) FROM item_containers \
         WHERE owner_id = ? AND slot_type = ? AND mate_id = 0",
    )
    .bind(id)
    .bind(crate::models::container::slot_type::MAIL)
    .fetch_one(&pool)
    .await
    .unwrap();
    // A mail delivery to the offline character: the attachment row lives in the MAIL container.
    sqlx::query(
        "INSERT INTO items (id, `type`, template_id, container_id, slot_type, slot, count, \
         lifespan_mins, owner, flags) VALUES (900001, 'Item', 2001, ?, ?, 0, 1, 0, ?, 0)",
    )
    .bind(mail_container)
    .bind(crate::models::container::slot_type::MAIL)
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    // Mate equipment is not part of creation either; its container and item must survive.
    sqlx::query(
        "INSERT INTO item_containers (container_id, container_type, slot_type, container_size, \
         owner_id, mate_id) VALUES (900010, 'ItemContainer', ?, 28, ?, 55)",
    )
    .bind(crate::models::container::slot_type::EQUIPMENT_MATE)
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (id, `type`, template_id, container_id, slot_type, slot, count, \
         lifespan_mins, owner, flags) VALUES (900002, 'Item', 2000, 900010, ?, 0, 1, 0, ?, 0)",
    )
    .bind(crate::models::container::slot_type::EQUIPMENT_MATE)
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    let mut req = creation_request("Mailed");
    req.model_params = appearance_params();
    lobby_edit(&server, &mut client, id, req).await.unwrap();
    let frames = client.drain();
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(
        frames[0].0,
        crate::opcodes::sc::SCEditCharacterResponsePacket
    );

    let kept: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM items WHERE CAST(id AS UNSIGNED) IN (900001, 900002)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, 2, "mail attachment and mate equipment survive");
    let containers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM item_containers \
         WHERE CAST(container_id AS UNSIGNED) IN (?, 900010)",
    )
    .bind(mail_container)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        containers, 2,
        "mail and mate-equipment container rows survive"
    );
    let creation_containers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM item_containers WHERE owner_id = ? AND mate_id = 0 \
         AND slot_type IN (?, ?, ?)",
    )
    .bind(id)
    .bind(crate::models::container::slot_type::EQUIPMENT)
    .bind(crate::models::container::slot_type::INVENTORY)
    .bind(crate::models::container::slot_type::BANK)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        creation_containers, 3,
        "the creation containers were rewritten"
    );
    let hair: i64 = sqlx::query_scalar(
        "SELECT ORD(SUBSTRING(unit_model_params, 2, 1)) FROM characters WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(hair, 7, "the edit itself was applied");
    pool.close().await;
}

/// `services-misc#7` (WP-N1): the starter supplies follow `default_action_bar_actions`
/// (18791 -> 10, 18792 -> 11, 4045 -> 12 in the 1.2 data); the general supply without a row
/// (417) keeps the next free slot, 13.
#[tokio::test]
async fn new_character_default_action_bar_uses_authored_slots() {
    use crate::data::character_templates::{AbilityItems, AbilitySupplyItem};
    use crate::data::items::ItemTemplate;
    use crate::models::character::Inventory;

    let mut data = creation_data();
    // The shipped `character_supplies` has only ability 0 rows: the class supply list is empty.
    data.characters
        .ability_items
        .get_mut(&7)
        .unwrap()
        .supplies
        .clear();
    for id in [4045, 18791, 18792, 417] {
        data.items
            .templates
            .insert(id, ItemTemplate::plain(id, 1000));
    }
    data.characters.default_action_bar_actions = vec![(18791, 10), (18792, 11), (4045, 12)];
    data.characters.ability_items.insert(
        0,
        AbilityItems {
            supplies: [4045, 18791, 18792, 417]
                .into_iter()
                .map(|id| AbilitySupplyItem {
                    id,
                    amount: 1,
                    grade: 0,
                })
                .collect(),
            ..Default::default()
        },
    );
    let server = test_server_with(data);
    let template = server.game_data.characters.template(1, 1).unwrap().clone();
    let request = creation_request("ActionBar");
    let mut created_items = Vec::new();
    let c = super::build_lobby_character(
        super::BuildInputs {
            server: &server,
            name: "ActionBar",
            request: &request,
            template: &template,
            id: 1,
            account_id: 1001,
            connection_id: 1,
            ip: "127.0.0.1:12345".parse().unwrap(),
            access_level: 0,
            level: 1,
            created: None,
            labor_power: 0,
            labor_power_modified: 0,
            chat_restricted_until: 0,
            inventory: Inventory::new(1, 50, 50, &server.items, &server.ids.container),
        },
        &mut created_items,
    )
    .unwrap();
    for (slot, item) in [(10, 18791), (11, 18792), (12, 4045), (13, 417)] {
        assert_eq!(c.slots[slot].kind, ActionSlotType::ItemType, "slot {slot}");
        assert_eq!(c.slots[slot].action_id, item, "slot {slot}");
    }
}

/// The creation request is not trusted: the level field is ignored, only the first ability is
/// kept, and a body item that is not a body part of the race / gender model and slot is replaced
/// by the template default.
#[tokio::test]
async fn new_character_ignores_client_level_abilities_and_foreign_body_items() {
    use crate::data::items::{BodyPartTemplate, ItemTemplate, TemplateKind};
    use crate::models::character::Inventory;

    let mut data = creation_data();
    for (id, model_id, npc_only) in [(3001u32, 0u32, false), (3002, 99, false), (3003, 0, true)] {
        let mut t = ItemTemplate::plain(id, 1);
        t.kind = TemplateKind::BodyPart {
            slot_type_id: 23,
            model_id,
        };
        data.items.templates.insert(id, t);
        data.items.body_parts.insert(
            id,
            BodyPartTemplate {
                item_id: id,
                model_id,
                npc_only,
                slot_type_id: 23,
                beauty_shop_only: false,
            },
        );
    }
    data.items
        .templates
        .insert(3004, ItemTemplate::plain(3004, 1));
    data.characters.templates.get_mut(&17).unwrap().items[0] = 3001;
    let server = test_server_with(data);
    let template = server.game_data.characters.template(1, 1).unwrap().clone();
    let build = |body: u32| {
        let mut request = creation_request("Forged");
        request.ability2 = 5;
        request.ability3 = 6;
        request.level = 50;
        request.body_items[0] = body;
        let mut created_items = Vec::new();
        super::build_lobby_character(
            super::BuildInputs {
                server: &server,
                name: "Forged",
                request: &request,
                template: &template,
                id: 1,
                account_id: 1001,
                connection_id: 1,
                ip: "127.0.0.1:12345".parse().unwrap(),
                access_level: 0,
                level: 1,
                created: None,
                labor_power: 0,
                labor_power_modified: 0,
                chat_restricted_until: 0,
                inventory: Inventory::new(1, 50, 50, &server.items, &server.ids.container),
            },
            &mut created_items,
        )
        .unwrap()
    };
    let slot_item = |c: &Character| {
        c.inventory
            .equipment()
            .lock()
            .unwrap()
            .get_item_by_slot(19)
            .map(|i| i.template_id)
    };
    let c = build(3001);
    assert_eq!((c.ability2, c.ability3), (11, 11));
    assert_eq!(c.unit.level, 1);
    assert_eq!(slot_item(&c), Some(3001));
    // Another model's part, an NPC-only part and a non-body item fall back to the default.
    for bad in [3002, 3003, 3004, 2000] {
        assert_eq!(slot_item(&build(bad)), Some(3001), "item {bad}");
    }
}
