//! Entry / leave session guards: a character may have one world session at a time, the
//! reservation is taken before the select's MySQL load and kept until the departure save has
//! finished, and a failed world registration releases it again.

use std::sync::{Arc, Barrier};

use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::GameData;
use crate::managers::account::AccountState;
use crate::managers::save::capture_financial_snapshot;
use crate::net::connection::GameState;
use crate::net::connection::{AccountPayment, PaymentMethodType};
use crate::systems::services::premium::{PcBangBuff, PremiumGrade};
use crate::systems::vehicles::mates::MateDb;
use crate::test_support::{
    spawn_player, test_character, test_server, test_server_with, TestClient,
};
use crate::world::session::{self, SelectDbData};

async fn assert_select_preserves_live_session(selected_id: u32) {
    let server = test_server();
    // Any attempted load must fail immediately, without reaching a real database.
    server.db.close().await;
    let mut client = TestClient::new(&server, 1, 1007);
    let character = test_character(7, "Live", 100.0, 100.0);
    let mut sibling = test_character(8, "Other", 200.0, 200.0);
    sibling.account_id = character.account_id;
    client.conn.characters = vec![character.clone(), sibling];
    assert_eq!(server.enter_world.reserve_character(7, 1), Ok(true));
    session::select_loaded(
        &server,
        &mut client.conn,
        character,
        SelectDbData::default(),
    )
    .expect("initial select");
    let player = client.conn.active_char.clone().unwrap();
    {
        let mut c = player.data();
        c.money = 4321;
        c.money2 = 8765;
        c.labor_power_unsaved = -25;
        c.unit.hp = 37;
        c.unit.mp = 19;
        c.is_online = true;
    }
    let world = server.world.world(0).unwrap();
    world.spawn(&player, &server.view_ctx());
    client.conn.state = GameState::World;
    let region = player.region().expect("spawned player's region");

    server.systems.vehicles.mates.character(7, |mates| {
        mates.mates.push(MateDb {
            id: 70,
            item_id: 700,
            name: "Mount".into(),
            xp: 100,
            level: 2,
            mileage: 3,
            hp: 41,
            mp: 23,
            owner: 7,
            updated_at: Default::default(),
            created_at: Default::default(),
        });
        // Progress and deletions accumulated since the original load must survive a select.
        mates.get_mut(700).unwrap().xp += 300;
        mates.removed.push(71);
    });
    let mates = server.systems.vehicles.mates.character_mates(7).unwrap();
    client.drain();

    for _ in 0..2 {
        session::select_character(&server, &mut client.conn, selected_id)
            .await
            .expect("an in-world select is ignored before any database access");
        assert!(Arc::ptr_eq(
            client.conn.active_char.as_ref().unwrap(),
            &player
        ));
        assert_eq!(client.conn.state, GameState::World);
        assert!(!client.conn.is_closing());
        let live = server.world.character_by_id(7).unwrap();
        assert!(Arc::ptr_eq(&live, &player));
        let spawned = world.character_by_obj_id(player.obj_id).unwrap();
        assert!(Arc::ptr_eq(&spawned, &player));
        assert_eq!(server.world.all_characters().len(), 1);
        assert!(server.world.character_by_id(8).is_none());
        assert_eq!(player.region(), Some(region));
        assert!(player.is_visible());
        assert_eq!(server.enter_world.character_owner(7), Some(1));
        assert_eq!(server.enter_world.character_owner(8), None);
        assert!(server.save.pending_departure(7).is_none());
        {
            let c = player.data();
            assert_eq!(
                (c.money, c.money2, c.labor_power_unsaved),
                (4321, 8765, -25)
            );
            assert_eq!((c.unit.hp, c.unit.mp), (37, 19));
            assert!(c.is_online);
        }
        assert_eq!(
            server.systems.vehicles.mates.character_mates(7),
            Some(mates.clone())
        );
        assert_eq!(
            client.conn.characters[0].money, 0,
            "the lobby copy stays stale"
        );
        assert!(
            client.drain().is_empty(),
            "no second select's initial packets"
        );
        assert!(client.commands().is_empty());
    }
}

#[tokio::test]
async fn repeated_select_preserves_the_live_session() {
    assert_select_preserves_live_session(7).await;
}

#[tokio::test]
async fn selecting_another_character_preserves_the_live_session() {
    assert_select_preserves_live_session(8).await;
}

#[tokio::test]
async fn select_without_an_active_character_still_attempts_the_load() {
    let server = test_server();
    server.db.close().await;
    let mut client = TestClient::new(&server, 1, 1007);
    client.conn.characters = vec![test_character(7, "Lobby", 100.0, 100.0)];

    let error = session::select_character(&server, &mut client.conn, 7)
        .await
        .expect_err("an initial select must reach the database");
    assert!(
        matches!(error, crate::GameError::Db(sqlx::Error::PoolClosed)),
        "{error}"
    );
    assert!(client.conn.active_char.is_none());
    assert_eq!(server.enter_world.character_owner(7), None);
    assert!(server.world.all_characters().is_empty());
}

#[tokio::test]
async fn a_stale_active_character_does_not_block_selecting_after_departure() {
    let server = test_server();
    server.db.close().await;
    let mut old = TestClient::new(&server, 1, 1007);
    let mut sibling = test_character(8, "Other", 100.0, 100.0);
    sibling.account_id = 1007;
    old.conn.characters = vec![sibling];
    let stale = spawn_player(
        &server,
        &old,
        test_character(7, "Returner", 100.0, 100.0),
        0x107,
    );
    old.conn.active_char = Some(stale.clone());
    let world = server.world.world(0).unwrap();
    world.delete(&stale);
    assert!(server.world.try_remove_character(stale.obj_id));

    // A connection keeps active_char after departure. Even if the object id is reused by a
    // newer session, only the exact Player still registered in the world triggers the guard.
    let newer = TestClient::new(&server, 2, 1007);
    let replacement = spawn_player(
        &server,
        &newer,
        test_character(7, "Returner", 100.0, 100.0),
        stale.obj_id,
    );
    assert_eq!(server.enter_world.reserve_character(7, 2), Ok(true));
    let error = session::select_character(&server, &mut old.conn, 8)
        .await
        .expect_err("a departed active_char must not bypass the next select's load");
    assert!(
        matches!(error, crate::GameError::Db(sqlx::Error::PoolClosed)),
        "{error}"
    );
    assert!(Arc::ptr_eq(old.conn.active_char.as_ref().unwrap(), &stale));
    assert!(Arc::ptr_eq(
        &server.world.character_by_id(7).unwrap(),
        &replacement
    ));
    assert_eq!(server.enter_world.character_owner(7), Some(2));
    assert_eq!(server.enter_world.character_owner(8), None);
    assert!(server.save.pending_departure(7).is_none());
}

#[tokio::test]
async fn force_release_retains_departing_state_for_the_next_financial_snapshot() {
    let server = test_server();
    server.db.close().await;
    let departing_client = TestClient::new(&server, 1, 1007);
    let other_client = TestClient::new(&server, 2, 1008);
    let departing = spawn_player(
        &server,
        &departing_client,
        test_character(7, "Departing", 100.0, 100.0),
        0x107,
    );
    let other = spawn_player(
        &server,
        &other_client,
        test_character(8, "Staying", 110.0, 110.0),
        0x108,
    );
    {
        let mut c = departing.data();
        c.money = 4321;
        c.money2 = 8765;
        c.labor_power_unsaved = -25;
        c.unit.hp = 37;
    }
    other.data().money = 99;
    assert_eq!(server.enter_world.reserve_character(7, 1), Ok(true));
    // An abandoned select can leave a reservation without a corresponding world entry.
    assert_eq!(server.enter_world.reserve_character(9, 1), Ok(true));
    assert_eq!(server.enter_world.reserve_character(8, 2), Ok(true));
    let weak = Arc::downgrade(&departing);
    let obj_id = departing.obj_id;
    let region = other.region().unwrap();
    drop(departing);

    // There need not be an active_char on the failed connection for fallback cleanup to work.
    assert!(departing_client.conn.active_char.is_none());
    session::force_release_connection(&server, 1);
    let retained = weak
        .upgrade()
        .expect("the departure set must retain the removed Player");
    let pending = server.save.pending_departure(7).expect("pending departure");
    assert!(Arc::ptr_eq(&pending, &retained));
    assert!(server.world.character_by_id(7).is_none());
    assert!(server.world.character_by_obj_id(obj_id).is_none());
    let world = server.world.world(0).unwrap();
    assert!(world.object(obj_id).is_none());
    assert!(retained.region().is_none());
    assert!(!retained.is_visible());
    assert_eq!(server.enter_world.character_owner(7), None);
    assert_eq!(server.enter_world.character_owner(9), None);
    assert!(server.save.pending_departure(9).is_none());

    // Repeated fallback cleanup and a connection with no active player are harmless.
    session::force_release_connection(&server, 1);
    session::force_release_connection(&server, 999);
    assert!(Arc::ptr_eq(
        &server.world.character_by_id(8).unwrap(),
        &other
    ));
    assert!(Arc::ptr_eq(
        &world.character_by_obj_id(other.obj_id).unwrap(),
        &other
    ));
    assert_eq!(server.world.all_characters().len(), 1);
    assert_eq!(server.enter_world.character_owner(8), Some(2));
    assert_eq!(other.region(), Some(region));
    assert!(other.is_visible());
    assert_eq!(other.data().money, 99);
    assert!(server.save.pending_departure(8).is_none());

    let snapshot = capture_financial_snapshot(&server);
    assert_eq!(
        snapshot.characters.len(),
        2,
        "each live/departing wallet once"
    );
    let (player, saved) = snapshot
        .characters
        .iter()
        .find(|(p, _)| p.character_id == 7)
        .expect("force-removed player in the next save");
    assert!(Arc::ptr_eq(player, &retained));
    assert_eq!(
        (saved.money, saved.money2, saved.labor_power_unsaved),
        (4321, 8765, -25)
    );
    assert_eq!(saved.unit.hp, 37);
    let (player, saved) = snapshot
        .characters
        .iter()
        .find(|(p, _)| p.character_id == 8)
        .expect("unrelated player still saved");
    assert!(Arc::ptr_eq(player, &other));
    assert_eq!(saved.money, 99);
}

#[tokio::test]
async fn the_same_character_cannot_enter_the_world_twice() {
    let server = test_server();
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1001);
    let c = test_character(7, "Twin", 100.0, 100.0);
    a.conn.characters = vec![c.clone()];
    b.conn.characters = vec![c.clone()];

    // `select_character` reserves the character before its MySQL load; `select_loaded` is the
    // world half once that load is done.
    assert_eq!(server.enter_world.reserve_character(7, 1), Ok(true));
    session::select_loaded(&server, &mut a.conn, c.clone(), SelectDbData::default())
        .expect("first select");
    let first = a.conn.active_char.clone().expect("first active char");
    assert_eq!(server.enter_world.character_owner(7), Some(1));

    let error = session::select_loaded(&server, &mut b.conn, c, SelectDbData::default())
        .expect_err("a second session must not load the same character");
    assert!(
        error.to_string().contains("world entry"),
        "unexpected error: {error}"
    );
    assert!(b.conn.active_char.is_none());
    // The failed attempt must not release the first session's reservation or entry.
    assert_eq!(server.enter_world.character_owner(7), Some(1));
    let live = server
        .world
        .character_by_id(7)
        .expect("the character stays in the world");
    assert!(Arc::ptr_eq(&live, &first));
    assert_eq!(server.world.all_characters().len(), 1);
}

/// Two connections enter the same character at the same instant: the reservation is atomic, so
/// exactly one wins and the world holds one player (before the fix both entries succeeded, the
/// second silently replacing nothing but running a second live copy).
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_entries_of_one_character_leave_exactly_one_player() {
    let server = Arc::new(test_server());
    let barrier = Arc::new(Barrier::new(2));
    let mut threads = Vec::new();
    for connection_id in [1u32, 2] {
        let server = server.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let mut client = TestClient::new(&server, connection_id, 1001);
            let character = test_character(7, "Twin", 100.0, 100.0);
            barrier.wait();
            match server.enter_world.reserve_character(7, connection_id) {
                Ok(true) => session::select_loaded(
                    &server,
                    &mut client.conn,
                    character,
                    SelectDbData::default(),
                )
                .map(|()| {
                    let obj_id = client
                        .conn
                        .active_char
                        .as_ref()
                        .expect("active char")
                        .obj_id;
                    (connection_id, obj_id)
                }),
                Ok(false) => Err(crate::GameError::Protocol(
                    "reservation already owned".into(),
                )),
                Err(owner) => Err(crate::GameError::Protocol(format!(
                    "character 7 is already in the world (session {owner})"
                ))),
            }
        }));
    }
    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("enter thread"))
        .collect();
    let winners: Vec<_> = results.iter().filter(|result| result.is_ok()).collect();
    assert_eq!(winners.len(), 1, "results: {results:?}");
    let (winner_connection, _) = *winners[0].as_ref().expect("winner");
    assert_eq!(
        server.enter_world.character_owner(7),
        Some(winner_connection)
    );
    assert_eq!(server.world.all_characters().len(), 1);
    assert_eq!(
        server.world.character_by_id(7).unwrap().conn.id,
        winner_connection
    );
}

#[tokio::test]
async fn select_loaded_refuses_an_occupied_world_entry() {
    let server = test_server();
    let client = TestClient::new(&server, 3, 1003);
    let existing = spawn_player(
        &server,
        &client,
        test_character(8, "Ghost", 100.0, 100.0),
        0x108,
    );
    // Same obj id as the entry the new session would use.
    server.world.remember_obj_id(8, 0x108);
    assert_eq!(server.enter_world.character_owner(8), None);

    // The select path reserved the character for the new session before the load; the caller's
    // `CharacterReservation` guard releases it when `select_loaded` fails.
    assert_eq!(server.enter_world.reserve_character(8, 4), Ok(true));
    let mut other = TestClient::new(&server, 4, 1004);
    let error = session::select_loaded(
        &server,
        &mut other.conn,
        test_character(8, "Ghost", 0.0, 0.0),
        SelectDbData::default(),
    )
    .expect_err("an occupied world entry must be refused");
    assert!(
        error.to_string().contains("world entry"),
        "unexpected error: {error}"
    );
    assert!(other.conn.active_char.is_none());
    server.enter_world.release_character(8, 4);
    assert_eq!(server.enter_world.character_owner(8), None);
    let live = server
        .world
        .character_by_id(8)
        .expect("the existing entry stays");
    assert!(Arc::ptr_eq(&live, &existing));
    assert_eq!(server.world.all_characters().len(), 1);
}

#[tokio::test]
async fn leave_releases_the_character_only_after_the_departure_save() {
    let server = test_server();
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1001);
    let c = test_character(9, "Returner", 100.0, 100.0);
    a.conn.characters = vec![c.clone()];
    b.conn.characters = vec![c.clone()];
    assert_eq!(server.enter_world.reserve_character(9, 1), Ok(true));
    session::select_loaded(&server, &mut a.conn, c.clone(), SelectDbData::default()).unwrap();
    let first = a.conn.active_char.clone().unwrap();

    // While the first session is live the character cannot be loaded again.
    assert!(
        session::select_loaded(&server, &mut b.conn, c.clone(), SelectDbData::default()).is_err()
    );
    assert_eq!(server.enter_world.character_owner(9), Some(1));

    // The leave runs the departure save (it fails against the closed test DB, which keeps it in
    // `pending_departure` for the next select's retry) and only then frees the reservation.
    session::leave_world_task(
        &server,
        None,
        session::leave_target::CHARACTER_SELECT,
        Some(first.clone()),
    )
    .await;
    assert_eq!(server.enter_world.character_owner(9), None);
    assert!(server.world.character_by_id(9).is_none());
    assert!(server.save.pending_departure(9).is_some());

    assert_eq!(server.enter_world.reserve_character(9, 2), Ok(true));
    session::select_loaded(&server, &mut b.conn, c, SelectDbData::default())
        .expect("re-entry after the leave finished");
    let second = b.conn.active_char.clone().unwrap();
    assert!(
        !Arc::ptr_eq(&second, &first),
        "a fresh Player must be created"
    );
    let live = server.world.character_by_id(9).unwrap();
    assert!(Arc::ptr_eq(&live, &second));
}

#[tokio::test]
async fn select_character_refuses_a_reserved_character_before_loading() {
    let server = test_server();
    let mut a = TestClient::new(&server, 1, 1001);
    let c = test_character(7, "Twin", 100.0, 100.0);
    assert_eq!(server.enter_world.reserve_character(7, 1), Ok(true));
    session::select_loaded(&server, &mut a.conn, c.clone(), SelectDbData::default())
        .expect("first entry");

    let mut b = TestClient::new(&server, 2, 1001);
    b.conn.characters = vec![c];
    // No MySQL is reachable in the test server: reaching the load would fail with a database
    // error instead of the reservation refusal.
    let error = session::select_character(&server, &mut b.conn, 7)
        .await
        .expect_err("the select must be refused before the load");
    assert!(
        error.to_string().contains("already in the world"),
        "unexpected error: {error}"
    );
    assert!(b.conn.active_char.is_none());
}

/// The departure save commits before the reservation is released, and the re-entry reads the
/// saved row: the money the leaving session had is what the next session loads.
///
/// Private MySQL 3317 only (`AAEMU_TEST_MYSQL`); every table the save/load path touches is
/// shadowed by a connection-local temporary copy, so no persistent row changes.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_reentry_loads_the_state_the_departure_save_committed() {
    use std::str::FromStr;

    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};

    use crate::systems::social::achievement_persistence::temporary_shadow_definition;

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
        "housings",
        "dominions",
        "dominion_siege_participants",
        "mails",
        "items",
        "item_containers",
        "auction_house",
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
        assert!(definition.starts_with("CREATE TABLE "));
        sqlx::query(&temporary_shadow_definition(&definition))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO `accounts` (`account_id`, `labor`) VALUES (1, 1000)")
        .execute(&pool)
        .await
        .unwrap();

    let mut data = crate::data::GameData::empty();
    data.experience = crate::test_support::test_experience(55);
    let mut server = crate::test_support::test_server_with(data);
    server.db = pool.clone();
    let client = TestClient::new(&server, 1, 1);
    let mut character = test_character(1, "Reloader", 100.0, 100.0);
    character.account_id = 1;
    character.money = 1234;
    character.is_online = true;
    let player = spawn_player(&server, &client, character, 0x101);
    assert_eq!(server.enter_world.reserve_character(1, 1), Ok(true));

    session::leave_world_task(
        &server,
        None,
        session::leave_target::CHARACTER_SELECT,
        Some(player),
    )
    .await;
    assert!(
        server.save.pending_departure(1).is_none(),
        "the departure save committed"
    );
    assert_eq!(server.enter_world.character_owner(1), None);

    // The re-entry loads the row the departure save wrote.
    let (loaded, _) = crate::db_character::load_character(&server, 1, 1)
        .await
        .expect("load")
        .expect("the character row");
    assert_eq!(loaded.money, 1234);
    assert_eq!(loaded.unit.name, "Reloader");
    assert_eq!(server.enter_world.reserve_character(1, 2), Ok(true));
    let mut other = TestClient::new(&server, 2, 1);
    session::select_loaded(&server, &mut other.conn, loaded, SelectDbData::default())
        .expect("re-entry");
    let reentered = other
        .conn
        .active_char
        .clone()
        .expect("re-entered character");
    assert_eq!(reentered.data().money, 1234);
    assert_eq!(server.enter_world.character_owner(1), Some(2));
    pool.close().await;
}

/// The offline infamy crossed the threshold while the character was away: the select path
/// applies `Character.SetFaction(Pirate)` once (the faction in the later spawn state is then
/// already Pirate; the player is not in the world regions during the select, so nobody else is
/// notified yet).
#[tokio::test]
async fn select_applies_the_pirate_faction_at_the_threshold_once() {
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::world_data::SystemFaction;
    use crate::data::GameData;
    use crate::opcodes::sc;
    use crate::systems::social::packets::body;
    use crate::systems::social::packets::faction::SCUnitFactionChangedPacket;

    let mut data = GameData::empty();
    for id in [3710u32, 4832] {
        data.skills
            .buffs
            .insert(id, BuffTemplate::simple(id, BuffKind::Good));
    }
    for (id, mother) in [(101u32, 0u32), (161, 0)] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            ..Default::default()
        });
    }
    let server = crate::test_support::test_server_with(data);
    let mut observer_client = TestClient::new(&server, 2, 1008);
    let observer = spawn_player(
        &server,
        &observer_client,
        test_character(8, "Observer", 105.0, 100.0),
        0x108,
    );
    observer_client.drain();

    let mut client = TestClient::new(&server, 1, 1007);
    let mut character = test_character(7, "Crook", 100.0, 100.0);
    character.infamy_point = 3_000;
    session::select_loaded(
        &server,
        &mut client.conn,
        character,
        SelectDbData::default(),
    )
    .expect("select");
    let player = client.conn.active_char.clone().expect("selected");
    assert_eq!(player.data().unit.faction_id, 161);

    let expected = SCUnitFactionChangedPacket {
        obj_id: player.obj_id,
        unit_name: "Crook".into(),
        id: 101,
        id2: 161,
        temp: false,
    };
    let find = |frames: &[(u16, Vec<u8>)]| -> Vec<Vec<u8>> {
        frames
            .iter()
            .filter(|f| f.0 == sc::SCUnitFactionChangedPacket)
            .map(|f| f.1.clone())
            .collect()
    };
    assert_eq!(
        find(&client.drain()),
        vec![body(&expected)],
        "the pirate is told exactly once (the 1.2 client shows \"$1 became pirate.\")"
    );
    // The select path adds the character to the world table but does not spawn it in a region
    // yet, so there is no around-player broadcast here (the spawn's SCUnitState carries 161).
    assert!(find(&observer_client.drain()).is_empty());
    assert_eq!(observer.data().unit.faction_id, 101);
}

/// A character whose offline infamy crossed the threshold is converted at select, before
/// `CSNotifyInGamePacket` runs `TeamManager.UpdateAtLogin`; the party removal must therefore
/// find the team by membership instead of relying on `InParty` (still false at that point),
/// otherwise `UpdateAtLogin` re-joins the new pirate to the team.
#[tokio::test]
async fn select_releases_a_converted_pirate_from_its_team() {
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::world_data::SystemFaction;
    use crate::data::GameData;
    use crate::test_support::test_server_with;

    let mut data = GameData::empty();
    for id in [3710u32, 4832] {
        data.skills
            .buffs
            .insert(id, BuffTemplate::simple(id, BuffKind::Good));
    }
    for (id, mother) in [(101u32, 0u32), (161, 0)] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            ..Default::default()
        });
    }
    let server = test_server_with(data);
    let mut owner_client = TestClient::new(&server, 8, 1008);
    let owner = spawn_player(
        &server,
        &owner_client,
        test_character(8, "Owner", 105.0, 100.0),
        0x108,
    );
    let mut other_client = TestClient::new(&server, 9, 1009);
    let other = spawn_player(
        &server,
        &other_client,
        test_character(9, "Other", 106.0, 100.0),
        0x109,
    );
    let mut member_client = TestClient::new(&server, 7, 1007);
    let member = spawn_player(
        &server,
        &member_client,
        test_character(7, "Crook", 100.0, 100.0),
        0x107,
    );
    for p in [&owner, &other, &member] {
        p.data().is_online = true;
    }
    let teams = &server.systems.social.teams;
    teams.ask_to_join(&server, &owner, "", 0, true, Some(&member));
    teams.reply_to_join_team(
        &server,
        &member,
        0,
        true,
        owner.character_id,
        false,
        "",
        false,
    );
    teams.ask_to_join(&server, &owner, "", 0, true, Some(&other));
    teams.reply_to_join_team(
        &server,
        &other,
        0,
        true,
        owner.character_id,
        false,
        "",
        false,
    );
    assert!(member.data().in_party && owner.data().in_party && other.data().in_party);
    assert!(teams.active_team_by_unit(member.character_id).is_some());

    // End the old session without the team / world cleanup, then select the character again
    // with the infamy that crossed the threshold while it was away.
    server.world.try_remove_character(member.obj_id);
    let mut character = test_character(7, "Crook", 100.0, 100.0);
    character.infamy_point = 3_000;
    session::select_loaded(
        &server,
        &mut member_client.conn,
        character,
        SelectDbData::default(),
    )
    .expect("select");
    let player = member_client.conn.active_char.clone().expect("selected");
    assert_eq!(player.data().unit.faction_id, 161);
    assert!(
        teams.active_team_by_unit(player.character_id).is_none(),
        "the new pirate is not in the team after select"
    );
    assert!(!player.data().in_party);

    // `UpdateAtLogin` must not bring the pirate back into the other faction's team.
    crate::systems::social::session::on_notify_in_game_team(&server, &player);
    assert!(teams.active_team_by_unit(player.character_id).is_none());
    assert!(!player.data().in_party);
    let team = teams
        .active_team_by_unit(owner.character_id)
        .expect("the remaining party survives");
    assert_eq!(team.members_count(), 2);
    assert!(team.is_member(owner.character_id) && team.is_member(other.character_id));
    owner_client.drain();
    other_client.drain();
}

/// WP-B: character selection applies the `pcbang_buffs` rows of the account's premium grade;
/// the VIP buff 8000011 is no longer force-added to every character.
#[tokio::test]
async fn select_applies_the_premium_grade_buff_only_for_premium_accounts() {
    let mut data = GameData::empty();
    data.systems.services.premium.grades = vec![
        PremiumGrade {
            id: 1,
            grade_id: 1,
            point: 0,
        },
        PremiumGrade {
            id: 2,
            grade_id: 2,
            point: 1,
        },
    ];
    data.systems.services.premium.pcbang_buffs = vec![PcBangBuff {
        id: 8_000_001,
        buff_id: 8_000_011,
        active: true,
        kind_id: 3,
        premium_grade_id: 2,
    }];
    data.skills
        .buffs
        .insert(8_000_011, BuffTemplate::simple(8_000_011, BuffKind::Good));
    let server = test_server_with(data);
    let now = chrono::Utc::now().timestamp();

    for (account, premium) in [(1007u32, true), (1008u32, false)] {
        let mut client = TestClient::new(&server, account - 1000, account);
        server.accounts.set_state(
            account,
            AccountState {
                payment: AccountPayment {
                    method: if premium {
                        PaymentMethodType::Premium
                    } else {
                        PaymentMethodType::None
                    },
                    location: 1,
                    start_time: 0,
                    end_time: if premium { now + 3_600 } else { 0 },
                },
                premium_point: i32::from(premium),
            },
        );
        let character = test_character(account - 1000, "Tester", 100.0, 100.0);
        session::select_loaded(
            &server,
            &mut client.conn,
            character,
            SelectDbData::default(),
        )
        .expect("select");
        let player = client.conn.active_char.clone().expect("active char");
        assert_eq!(
            player.data().unit.buffs.check_buff(8_000_011),
            premium,
            "account {account}"
        );

        if premium {
            // Relog: the leave removes the session buff, the next select applies it again.
            session::leave_world_task(
                &server,
                None,
                session::leave_target::CHARACTER_SELECT,
                Some(player.clone()),
            )
            .await;
            assert!(
                !player.data().unit.buffs.check_buff(8_000_011),
                "leave must remove the premium grade buff"
            );
            let again = test_character(account - 1000, "Tester2", 120.0, 120.0);
            session::select_loaded(&server, &mut client.conn, again, SelectDbData::default())
                .expect("select after leave");
            let relogged = client.conn.active_char.clone().expect("active char");
            assert!(
                relogged.data().unit.buffs.check_buff(8_000_011),
                "select must re-apply the premium grade buff"
            );
        }
    }
}

/// During the pre-select period the client's Start button edits the character instead of
/// entering the world; a forged `CSSelectCharacter` must not enter either.
#[tokio::test]
async fn select_is_refused_during_the_pre_select_period() {
    let mut config = crate::config::AppConfig {
        id: 1,
        ..Default::default()
    };
    config.characters.pre_select_period = true;
    let server = crate::test_support::test_server_with_config(GameData::empty(), config);
    server.db.close().await;
    let mut client = TestClient::new(&server, 1, 1007);
    let mut character = test_character(9, "Pre", 1.0, 2.0);
    character.account_id = 1007;
    client.conn.characters.push(character);
    session::select_character(&server, &mut client.conn, 9)
        .await
        .expect("refused without entering");
    assert!(client.conn.active_char.is_none());
    assert_eq!(client.conn.state, GameState::Lobby);
    assert_eq!(server.enter_world.character_owner(9), None);
    assert!(client.drain().is_empty());
}

#[tokio::test]
async fn enter_world_reports_mail_that_arrived_offline() {
    use crate::opcodes::sc;
    use crate::systems::economy::mail::{mail_status, mail_type, now, BaseMail};
    let server = test_server();
    let add = |id: i64, mt: u8, status: u8, receiver: u32, recv_date: i64| {
        let mut m = BaseMail::new();
        m.id = id;
        m.mail_type = mt;
        m.header.status = status;
        m.header.receiver_id = receiver;
        m.body.recv_date = recv_date;
        assert!(server.systems.economy.mail.state().all.add(id, m));
    };
    let past = now() - 10;
    add(1, mail_type::NORMAL, mail_status::UNREAD, 7, past);
    add(2, mail_type::BILLING, mail_status::UNPAID, 7, past);
    add(3, mail_type::NORMAL, mail_status::READ, 7, past);
    add(4, mail_type::NORMAL, mail_status::UNREAD, 7, now() + 3600);
    add(5, mail_type::NORMAL, mail_status::UNREAD, 8, past);

    let mut client = TestClient::new(&server, 1, 1007);
    let character = test_character(7, "Offline", 100.0, 100.0);
    client.conn.characters = vec![character.clone()];
    assert_eq!(server.enter_world.reserve_character(7, 1), Ok(true));
    session::select_loaded(
        &server,
        &mut client.conn,
        character,
        SelectDbData::default(),
    )
    .expect("select");
    let player = client.conn.active_char.clone().unwrap();
    let unread = player.data().unread_mail;
    assert_eq!(
        (
            unread.received,
            unread.mia_received,
            unread.commercial_received
        ),
        (2, 0, 0)
    );
    let mut want = aaemu_commons::packet_stream::PacketStream::new();
    for v in [0, 2, 0, 0] {
        want.write_i32(v);
    }
    let sent: Vec<_> = client
        .drain()
        .into_iter()
        .filter(|f| f.0 == sc::SCCountUnreadMailPacket)
        .collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, want.into_bytes());
}
