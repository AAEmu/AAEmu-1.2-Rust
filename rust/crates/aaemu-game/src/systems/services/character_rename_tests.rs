//! WP-AC2 tests: the `SCCharacterRenamed` wire body, the online rename side effects and
//! recipients, the refusal rules, the name-cache atomicity, and the offline MySQL round trip.

use std::sync::atomic::{AtomicBool, Ordering};

use aaemu_commons::PacketStream;

use crate::data::world_data::SystemFaction;
use crate::data::world_templates::WorldTemplate;
use crate::data::GameData;
use crate::managers::name::{normalize_name, NameManager};
use crate::models::char_data::FriendEntry;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::services::character_rename;
use crate::systems::social::expedition::{self, Expedition};
use crate::systems::social::family::{Family, FamilyMember};
use crate::test_support::{
    spawn_player, test_character, test_server, test_server_with_world, TestClient, TEST_ZONE,
};
use crate::world::{PlayerRef, WorldObject};

struct Pc {
    client: TestClient,
    p: PlayerRef,
}

fn pc(server: &GameServer, id: u32, name: &str, x: f32, y: f32) -> Pc {
    let client = TestClient::new(server, id, 1000 + id);
    let p = spawn_player(server, &client, test_character(id, name, x, y), 0x200 + id);
    let mut client = client;
    client.conn.active_char = Some(p.clone());
    client.drain();
    Pc { client, p }
}

/// The 1.2 serializer `FUN_397b7820`: `u32 charId`, then two length-prefixed strings.
fn rename_packet(old_name: &str, new_name: &str) -> Vec<u8> {
    let mut body = 1u32.to_le_bytes().to_vec();
    body.extend((old_name.len() as u16).to_le_bytes());
    body.extend(old_name.as_bytes());
    body.extend((new_name.len() as u16).to_le_bytes());
    body.extend(new_name.as_bytes());
    body
}

/// Reads it back the way the client handler `FUN_391df6e0` does: id, old name, new name.
fn read_rename_body(body: &[u8]) -> (u32, String, String) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_u32().expect("charId"),
        s.read_string().expect("oldName"),
        s.read_string().expect("newName"),
    )
}

fn renamed_frames(client: &mut TestClient) -> Vec<Vec<u8>> {
    client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == sc::SCCharacterRenamedPacket)
        .map(|(_, body)| body)
        .collect()
}

#[tokio::test]
async fn online_rename_updates_the_unit_and_reaches_viewers_friends_guild_and_family() {
    let server = test_server_with_world(
        GameData::empty(),
        WorldTemplate::synthetic(0, "main_world", 4, 4, TEST_ZONE),
    );
    let mut renamed = pc(&server, 1, "OldName", 100.0, 100.0);
    let mut viewer_friend = pc(&server, 2, "Friend", 150.0, 100.0);
    // Two cells away: a guild member who is not a viewer.
    let mut guildmate = pc(&server, 3, "Mate", 6000.0, 100.0);
    // Another far corner: a family member who is not a viewer.
    let mut family_member = pc(&server, 4, "Kin", 7500.0, 7500.0);
    for id in 1..=4 {
        server.names.add_character(
            id,
            ["OldName", "Friend", "Mate", "Kin"][id as usize - 1],
            1000 + id,
        );
    }
    let viewers = server
        .world
        .world(0)
        .expect("instance")
        .players_around(WorldObject::from(&renamed.p))
        .into_iter()
        .map(|p| p.character_id)
        .collect::<Vec<_>>();
    assert!(viewers.contains(&2), "the friend is a viewer");
    assert!(!viewers.contains(&3), "the guild mate is out of view");
    assert!(!viewers.contains(&4), "the family member is out of view");

    server.friends.add_to_all(FriendEntry {
        id: 1,
        friend_id: 1,
        owner: 2,
    });
    let members = vec![
        expedition::member_from_character(&server, 7, &renamed.p, true),
        expedition::member_from_character(&server, 7, &guildmate.p, false),
    ];
    server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 7,
            name: "Guild".into(),
            owner_id: 1,
            owner_name: "OldName".into(),
            ..SystemFaction::default()
        },
        members,
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    server.systems.social.families.insert(Family {
        id: 9,
        members: vec![
            FamilyMember {
                character: Some(renamed.p.clone()),
                id: 1,
                name: "OldName".into(),
                role: 1,
                title: String::new(),
            },
            FamilyMember {
                character: Some(family_member.p.clone()),
                id: 4,
                name: "Kin".into(),
                role: 0,
                title: String::new(),
            },
        ],
        removed_members: Vec::new(),
    });
    for c in [
        &mut renamed.client,
        &mut viewer_friend.client,
        &mut guildmate.client,
        &mut family_member.client,
    ] {
        c.drain();
    }

    character_rename::apply_renamed(&server, 1, "OldName", "NewName");

    assert_eq!(renamed.p.data().unit.name, "NewName");
    assert_eq!(server.names.get_character_id(&normalize_name("NewName")), 1);
    assert_eq!(server.names.get_character_id(&normalize_name("OldName")), 0);
    let expected = rename_packet("OldName", "NewName");
    for (what, client) in [
        ("renamed character", &mut renamed.client),
        ("viewer/friend", &mut viewer_friend.client),
        ("guild mate", &mut guildmate.client),
        ("family member", &mut family_member.client),
    ] {
        let frames = renamed_frames(client);
        assert_eq!(frames, vec![expected.clone()], "{what}");
        assert_eq!(
            read_rename_body(&frames[0]),
            (1, "OldName".into(), "NewName".into()),
            "{what} reader"
        );
    }
    let guild = server.expeditions.get(7).expect("guild");
    assert_eq!(guild.faction.owner_name, "NewName");
    assert_eq!(guild.member(1).expect("member").name, "NewName");
    let family = server.systems.social.families.family(9).expect("family");
    assert_eq!(
        family
            .members
            .iter()
            .find(|m| m.id == 1)
            .expect("member")
            .name,
        "NewName"
    );
}

#[tokio::test]
async fn rename_updates_the_in_memory_mail_nation_auction_and_crime_names() {
    use crate::systems::economy::auction::AuctionLot;
    use crate::systems::economy::mail::{BaseMail, MailHeader};
    use crate::systems::social::crime::{CrimeEvent, VictimInfo};
    use crate::systems::social::nation::Nation;

    let server = test_server();
    server.names.add_character(1, "OldName", 1001);

    let mail = BaseMail {
        id: 11,
        receiver_name: "OldName".into(),
        header: MailHeader {
            sender_id: 1,
            sender_name: "OldName".into(),
            receiver_id: 1,
            ..MailHeader::default()
        },
        ..BaseMail::default()
    };
    assert!(server.systems.economy.mail.state().all.add(11, mail));

    server.systems.social.nations.add(Nation {
        id: 77,
        expedition_id: 0,
        owner_character_id: 1,
        owner_name: "OldName".into(),
        name: "Nation".into(),
        created_at: 0,
        allow_change_name: 0,
        relations: Vec::new(),
        offline_members: Vec::new(),
    });

    let now = chrono::Utc::now();
    server.systems.economy.auction.add_auction_lot(AuctionLot {
        id: 5,
        duration: 1,
        item: None,
        end_time: now,
        world_id: 0,
        client_id: 1,
        client_name: "OldName".into(),
        start_money: 1,
        direct_money: 1,
        post_date: now,
        bid_world_id: 0,
        bidder_id: 1,
        bidder_name: "OldName".into(),
        bid_money: 0,
        extra: 0,
        dirty: false,
    });
    server.systems.social.crimes.set_victim_info(
        1,
        VictimInfo {
            name: "OldName".into(),
            faction_id: 0,
            expedition_id: 0,
        },
    );

    character_rename::apply_renamed(&server, 1, "OldName", "NewName");

    let mail = server
        .systems
        .economy
        .mail
        .get_mail_by_id(11)
        .expect("mail");
    assert_eq!(mail.header.sender_name, "NewName");
    assert_eq!(mail.receiver_name, "NewName");
    assert_eq!(
        server
            .systems
            .social
            .nations
            .get(77)
            .expect("nation")
            .owner_name,
        "NewName"
    );
    let lot = server.systems.economy.auction.lot(5).expect("lot");
    assert_eq!(
        (lot.client_name.as_str(), lot.bidder_name.as_str()),
        ("NewName", "NewName")
    );
    let event = CrimeEvent {
        victim: 1,
        ..CrimeEvent::default()
    };
    assert_eq!(
        server
            .systems
            .social
            .crimes
            .record_entry(&server, &event)
            .victim_name,
        "NewName"
    );
}

#[tokio::test]
async fn rename_refuses_invalid_duplicate_and_same_names_through_the_command() {
    use crate::systems::services::commands::scripts::harness;

    let server = harness::server();
    let mut gm = harness::pc(&server, 1, "Alice", 100.0, 100.0);
    server.names.add_character(1, "Alice", 1001);
    server.names.add_character(2, "Taken", 1002);

    let (_, out) = harness::run(&server, &gm.p, "rename Alice alice").await;
    assert_eq!(
        out,
        [harness::error(
            "rename",
            "the new name is the same as the old name"
        )]
    );
    let (_, out) = harness::run(&server, &gm.p, "rename Alice Taken").await;
    assert_eq!(
        out,
        [harness::error("rename", "the name is already in use")]
    );
    let (_, out) = harness::run(&server, &gm.p, "rename Alice bad!").await;
    assert_eq!(
        out,
        [harness::error(
            "rename",
            "the name contains invalid characters"
        )]
    );
    let (_, out) = harness::run(&server, &gm.p, "rename Alice").await;
    assert!(out[0].contains("/rename"), "help text: {:?}", out);

    assert_eq!(gm.p.data().unit.name, "Alice");
    assert_eq!(server.names.get_character_id(&normalize_name("Alice")), 1);
    assert_eq!(server.names.get_character_id(&normalize_name("Taken")), 2);
    assert!(renamed_frames(&mut gm.client).is_empty());
}

#[test]
fn the_name_cache_never_holds_neither_or_both_names_during_a_rename() {
    let names = NameManager::default();
    names.add_character(1, "First", 1001);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..2_000 {
                let target = if i % 2 == 0 { "Second" } else { "First" };
                assert!(names.rename_character(1, target));
            }
            stop.store(true, Ordering::Release);
        });
        while !stop.load(Ordering::Acquire) {
            let (ids, map) = names.snapshot();
            let current = ids.get(&1).expect("the character always keeps a name");
            assert_eq!(
                map.get(current),
                Some(&1),
                "the cached current name maps back to the character"
            );
            let owned = map.values().filter(|id| **id == 1).count();
            assert_eq!(owned, 1, "exactly one cached name belongs to the character");
        }
    });
}

/// The race hook of `CharacterManager::create` (`managers/character.rs:519-566`): one
/// `SaveManager::transaction_lock` spans the name check and the registration.
async fn create_registers_name(server: &GameServer, id: u32, name: &str) -> bool {
    let _transaction = server.save.transaction_lock().await;
    let name = normalize_name(name);
    if server.names.name_exists(&name) {
        return false;
    }
    let code = server.names.validate_character_name_with_blocks(
        &name,
        &server.game_data.characters.blocked_names,
        || false,
    );
    if code != crate::managers::name::CharacterCreateError::Ok {
        return false;
    }
    server.names.add_character(id, &name, 1000 + id);
    true
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_offline_rename_persists_and_updates_every_name_copy() {
    use crate::models::character::Character;
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use sqlx::Row;
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
    for table in [
        "characters",
        "accounts",
        "options",
        "abilities",
        "actabilities",
        "appellations",
        "character_active_buffs",
        "character_cooldowns",
        "portal_book_coords",
        "portal_visited_district",
        "friends",
        "blocked",
        "skills",
        "quests",
        "completed_quests",
        "character_records",
        "character_achievements",
        "character_crafts",
        "mails",
        "expedition_members",
        "expeditions",
        "family_members",
        "nations",
        "nation_relations",
        "nation_relation_offers",
        "shipyards",
        "auction_house",
        "character_rank_scores",
        "rank_period_payouts",
    ] {
        let (_, definition): (String, String) =
            sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(
            &crate::systems::social::achievement_persistence::temporary_shadow_definition(
                &definition,
            ),
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    let mut data = GameData::empty();
    data.items
        .templates
        .insert(1000, crate::data::items::ItemTemplate::plain(1000, 1000));
    let mut server = crate::test_support::test_server_with(data);
    server.db = pool.clone();
    let item = server
        .items
        .create(1000, 1, 0, 0, &server.game_data.items, &server.ids.item, 0)
        .expect("auction item");
    let mut offline = Character::new(1, 1, Default::default());
    offline.unit.name = "OldName".into();
    offline.created = chrono::Utc::now().timestamp();
    assert!(crate::db_character::save_directly_to_database(&pool, &mut offline).await);
    let mut deleted = Character::new(2, 2, Default::default());
    deleted.unit.name = "Deleted".into();
    deleted.created = chrono::Utc::now().timestamp();
    assert!(crate::db_character::save_directly_to_database(&pool, &mut deleted).await);
    sqlx::query("UPDATE characters SET deleted = 1 WHERE id = 2")
        .execute(&pool)
        .await
        .unwrap();
    server.names.add_character(1, "OldName", 1001);
    server.names.add_character(2, "Deleted", 1002);

    for sql in [
        "INSERT INTO expeditions (id, owner, owner_name, name, mother) VALUES (7, 1, 'OldName', 'Guild', 0)",
        "INSERT INTO expedition_members (character_id, expedition_id, name, level, role, ability1, ability2, ability3, memo) VALUES (1, 7, 'OldName', 1, 0, 0, 0, 0, '')",
        "INSERT INTO family_members (character_id, family_id, name, role, title) VALUES (1, 9, 'OldName', 1, '')",
        "INSERT INTO nations (id, expedition_id, owner_character_id, owner_name, name) VALUES (77, 7, 1, 'OldName', 'Nation')",
        "INSERT INTO shipyards (id, template_id, owner_id, owner_name, instance_id, x, y, z, spawned) VALUES (5, 1, 1, 'OldName', 0, 1, 2, 3, 0)",
        "INSERT INTO mails (id, type, status, title, text, sender_id, sender_name, attachment_count, receiver_id, receiver_name, open_date, send_date, received_date, returned, extra, money_amount_1, money_amount_2, money_amount_3) VALUES (11, 1, 0, 'title', 'text', 1, 'OldName', 0, 1, 'OldName', NOW(), NOW(), NOW(), 0, 0, 0, 0, 0)",
        "INSERT INTO character_rank_scores (rank_id, period_start, character_id, period_end, account_id, account, world_id, v1, v2, updated) VALUES (1, 100, 1, 200, 1001, 'OldName', 1, 10, 0, 0)",
    ] {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO auction_house (duration, item_id, stack_size, world_id, client_id, client_name, start_money, direct_money, bid_world_id, bidder_id, bidder_name, bid_money, extra) VALUES (1, ?, 1, 0, 1, 'OldName', 1, 1, 0, 1, 'OldName', 0, 0)",
    )
    .bind(item.id)
    .execute(&pool)
    .await
    .unwrap();

    server
        .systems
        .economy
        .mail
        .load(&server)
        .await
        .expect("mail load");
    server
        .systems
        .social
        .nations
        .load(&pool)
        .await
        .expect("nation load");
    assert!(server.systems.economy.auction.load(&server).await);
    server
        .systems
        .social
        .ranking
        .load(&pool)
        .await
        .expect("ranking load");

    let old = character_rename::rename_character(&server, 1, "NewName")
        .await
        .expect("offline rename");
    assert_eq!(old, "OldName");
    let new = normalize_name("NewName");
    let row = sqlx::query("SELECT name FROM characters WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("name"), new);
    for (sql, table) in [
        (
            "SELECT name FROM expedition_members WHERE character_id = 1",
            "expedition_members",
        ),
        (
            "SELECT owner_name FROM expeditions WHERE owner = 1",
            "expeditions",
        ),
        (
            "SELECT name FROM family_members WHERE character_id = 1",
            "family_members",
        ),
        (
            "SELECT owner_name FROM nations WHERE owner_character_id = 1",
            "nations",
        ),
        (
            "SELECT owner_name FROM shipyards WHERE owner_id = 1",
            "shipyards",
        ),
        (
            "SELECT sender_name FROM mails WHERE sender_id = 1",
            "mails.sender_name",
        ),
        (
            "SELECT receiver_name FROM mails WHERE receiver_id = 1",
            "mails.receiver_name",
        ),
        (
            "SELECT client_name FROM auction_house WHERE client_id = 1",
            "auction.client_name",
        ),
        (
            "SELECT bidder_name FROM auction_house WHERE bidder_id = 1",
            "auction.bidder_name",
        ),
        (
            "SELECT account FROM character_rank_scores WHERE character_id = 1",
            "character_rank_scores.account",
        ),
    ] {
        let value: String = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
        assert_eq!(value, new, "{table}");
    }
    assert_eq!(server.names.get_character_id(&new), 1);
    assert_eq!(server.names.get_character_id(&normalize_name("OldName")), 0);

    // The managers loaded these rows at start; the rename must update their cached names too.
    let mail = server
        .systems
        .economy
        .mail
        .get_mail_by_id(11)
        .expect("mail");
    assert_eq!(
        (
            mail.header.sender_name.as_str(),
            mail.receiver_name.as_str()
        ),
        (new.as_str(), new.as_str()),
        "in-memory mail"
    );
    assert_eq!(
        server
            .systems
            .social
            .nations
            .get(77)
            .expect("nation")
            .owner_name,
        new,
        "in-memory nation"
    );
    let lot = server.systems.economy.auction.lot(1).expect("auction lot");
    assert_eq!(
        (lot.client_name.as_str(), lot.bidder_name.as_str()),
        (new.as_str(), new.as_str()),
        "in-memory auction lot"
    );
    assert_eq!(
        server
            .systems
            .social
            .ranking
            .score_of(1, 1, 100)
            .expect("ranking score")
            .account,
        new,
        "in-memory ranking score"
    );

    // A later save of each manager writes its rows from memory; the cached names above must
    // survive it instead of the old ones being restored.
    server
        .systems
        .economy
        .mail
        .with_mail_mut(11, |m| m.is_dirty = true);
    let mut tx = pool.begin().await.unwrap();
    server.systems.economy.mail.save(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    server.systems.economy.mail.commit_save();
    let mut tx = pool.begin().await.unwrap();
    server
        .systems
        .social
        .nations
        .save(&server, &mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut lot = server.systems.economy.auction.lot(1).expect("auction lot");
    lot.dirty = true;
    server.systems.economy.auction.set_lot(lot);
    let mut tx = pool.begin().await.unwrap();
    server
        .systems
        .economy
        .auction
        .save(&server, &mut tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    server.systems.economy.auction.commit_save();
    let mut tx = pool.begin().await.unwrap();
    server.systems.social.ranking.save(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    for (sql, table) in [
        (
            "SELECT sender_name FROM mails WHERE sender_id = 1",
            "mails.sender_name after save",
        ),
        (
            "SELECT receiver_name FROM mails WHERE receiver_id = 1",
            "mails.receiver_name after save",
        ),
        (
            "SELECT owner_name FROM nations WHERE owner_character_id = 1",
            "nations.owner_name after save",
        ),
        (
            "SELECT client_name FROM auction_house WHERE client_id = 1",
            "auction.client_name after save",
        ),
        (
            "SELECT bidder_name FROM auction_house WHERE bidder_id = 1",
            "auction.bidder_name after save",
        ),
        (
            "SELECT account FROM character_rank_scores WHERE character_id = 1",
            "character_rank_scores.account after save",
        ),
    ] {
        let value: String = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
        assert_eq!(value, new, "{table}");
    }

    let error = character_rename::rename_character(&server, 2, "Other")
        .await
        .expect_err("deleted character");
    assert_eq!(error, character_rename::RenameError::Deleted);
    let name: String = sqlx::query_scalar("SELECT name FROM characters WHERE id = 2")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Deleted");
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; temporary tables only"]
async fn private_mysql_concurrent_rename_and_create_leave_exactly_one_winner() {
    use crate::models::character::Character;
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
    for table in [
        "characters",
        "accounts",
        "options",
        "abilities",
        "actabilities",
        "appellations",
        "character_active_buffs",
        "character_cooldowns",
        "portal_book_coords",
        "portal_visited_district",
        "friends",
        "blocked",
        "skills",
        "quests",
        "completed_quests",
        "character_records",
        "character_achievements",
        "character_crafts",
        "mails",
        "expedition_members",
        "expeditions",
        "family_members",
        "nations",
        "nation_relations",
        "nation_relation_offers",
        "shipyards",
        "auction_house",
        "character_rank_scores",
        "rank_period_payouts",
    ] {
        let (_, definition): (String, String) =
            sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(
            &crate::systems::social::achievement_persistence::temporary_shadow_definition(
                &definition,
            ),
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    let mut server = test_server();
    server.db = pool.clone();
    let mut character = Character::new(1, 1, Default::default());
    character.unit.name = "Alice".into();
    character.created = chrono::Utc::now().timestamp();
    assert!(crate::db_character::save_directly_to_database(&pool, &mut character).await);
    server.names.add_character(1, "Alice", 1001);

    let create = create_registers_name(&server, 99, "Contested");
    let rename = character_rename::rename_character(&server, 1, "Contested");
    // Poll the rename first: without the shared lock it reaches its database awaits before the
    // create's check, which is exactly the window the lock has to close.
    let (renamed, created) = tokio::join!(rename, create);
    assert!(
        created != renamed.is_ok(),
        "exactly one path may take the name (created={created}, renamed={renamed:?})"
    );
    if created {
        assert_eq!(
            server.names.get_character_id(&normalize_name("Contested")),
            99
        );
        assert_eq!(
            server.names.get_character_id(&normalize_name("Alice")),
            1,
            "the rename refused"
        );
    } else {
        assert_eq!(
            server.names.get_character_id(&normalize_name("Contested")),
            1
        );
        assert_eq!(server.names.get_character_id(&normalize_name("Alice")), 0);
        let name: String = sqlx::query_scalar("SELECT name FROM characters WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(name, normalize_name("Contested"));
    }
    pool.close().await;
}
