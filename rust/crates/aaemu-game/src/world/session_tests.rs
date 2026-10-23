//! Entry / leave session guards: a character may have one world session at a time, the
//! reservation is taken before the select's MySQL load and kept until the departure save has
//! finished, and a failed world registration releases it again.

use std::sync::{Arc, Barrier};

use crate::managers::save::capture_financial_snapshot;
use crate::net::connection::GameState;
use crate::systems::vehicles::mates::MateDb;
use crate::test_support::{spawn_player, test_character, test_server, TestClient};
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
