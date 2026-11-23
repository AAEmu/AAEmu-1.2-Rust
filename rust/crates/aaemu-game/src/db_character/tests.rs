use super::*;
use crate::systems::social::achievement_persistence::temporary_shadow_definition;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use std::str::FromStr;

/// Every table `save_character` writes or `load_character` reads. A test must shadow all of
/// them: the save unconditionally deletes from `character_active_buffs` and rewrites both
/// achievement tables even when its fixture has no buffs or progress.
const CHARACTER_SAVE_TABLES: &[&str] = &[
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
];

/// Replaces each live table with a connection-local copy of its actual schema, so the test
/// never touches another session's rows on the shared MySQL3317. Temporary tables cannot
/// carry foreign keys, so [`temporary_shadow_definition`] drops them.
async fn shadow_tables(pool: &MySqlPool, tables: &[&str]) {
    for table in tables {
        let (_, definition): (String, String) =
            sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
                .fetch_one(pool)
                .await
                .unwrap();
        assert!(definition.starts_with("CREATE TABLE "));
        sqlx::query(&temporary_shadow_definition(&definition))
            .execute(pool)
            .await
            .unwrap();
    }
}

/// A duelling / battlefield pirate keeps the saved faction 161 while showing Red / Blue /
/// corps. The infamy threshold must read the saved faction: testing the live one would
/// re-fire `SetFaction(Pirate)` (end the temporary faction and kick the party / duel).
#[tokio::test]
async fn a_temporary_faction_does_not_rearm_the_pirate_change() {
    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::GameData;
    use crate::test_support::test_server_with;

    let mut data = GameData::empty();
    for id in [BUFF_WANTED, BUFF_CONTEMPTUOUS] {
        data.skills
            .buffs
            .insert(id, BuffTemplate::simple(id, BuffKind::Good));
    }
    let server = test_server_with(data);
    let mut duellist = Character::new(1, 1, UnitCustomModelParams::default());
    duellist.unit.faction_id = 160; // Red team while the saved faction is Pirate.
    duellist.unit.origin_faction_id = FACTION_PIRATE;
    duellist.unit.faction_temporary = true;
    duellist.infamy_point = PIRATE_CRIME_POINT_THRESHOLD;
    assert!(
        !duellist.check_wanted_threshold(&server, 0).became_pirate,
        "a saved pirate in a temporary faction is not converted again"
    );
    assert_eq!(duellist.unit.faction_id, 160);

    // A plain non-pirate at the threshold still converts.
    let mut criminal = Character::new(2, 1, UnitCustomModelParams::default());
    criminal.unit.faction_id = 101;
    criminal.infamy_point = PIRATE_CRIME_POINT_THRESHOLD;
    assert!(criminal.check_wanted_threshold(&server, 0).became_pirate);
}

#[test]
fn a_temporary_faction_is_never_persisted() {
    let mut c = Character::new(1, 1, UnitCustomModelParams::default());
    c.unit.faction_id = 101;
    assert_eq!(persisted_faction(&c), 101);
    // A duel / battlefield change: faction 160, origin 101.
    c.unit.faction_id = 160;
    c.unit.origin_faction_id = 101;
    c.unit.faction_temporary = true;
    assert_eq!(persisted_faction(&c), 101);
    // Restored: the live faction is the saved one again.
    c.unit.faction_id = 101;
    c.unit.faction_temporary = false;
    assert_eq!(persisted_faction(&c), 101);
    // A permanent change is saved as-is.
    c.unit.faction_id = 161;
    assert_eq!(persisted_faction(&c), 161);
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_character_save_preserves_both_bot_report_counters() {
    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;
    let mut character = Character::new(1, 1, UnitCustomModelParams::default());
    character.unit.name = "BotCounterFixture".into();
    character.created = Utc::now().timestamp();
    character.bot_reported_count = 9;
    character.reported_as_bot_count = 7;
    character.offline_guilty_region = 6;
    for expected in [(9, 7), (11, 12)] {
        character.bot_reported_count = expected.0;
        character.reported_as_bot_count = expected.1;
        let mut transaction = pool.begin().await.unwrap();
        save_character(&mut transaction, &mut character, Utc::now().timestamp())
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        let saved: (i64, i64, i64) = sqlx::query_as(
            "SELECT CAST(bot_reported_count AS SIGNED), CAST(reported_as_bot_count AS SIGNED), \
                 CAST(offline_guilty_region AS SIGNED) FROM characters WHERE id=1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(saved, (i64::from(expected.0), i64::from(expected.1), 6));
    }
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_chat_restriction_survives_character_reload() {
    use crate::test_support::{test_character, test_server_with};

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;

    let mut server = test_server_with(crate::data::GameData::empty());
    server.db = pool.clone();

    let until = Utc::now().timestamp() + 3_600;
    let mut character = test_character(1, "ChatRestrict", 100.0, 100.0);
    character.account_id = 1;
    character.created = Utc::now().timestamp();
    character.chat_restricted_until = until;
    let mut transaction = pool.begin().await.unwrap();
    save_character(&mut transaction, &mut character, Utc::now().timestamp())
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let saved: chrono::NaiveDateTime =
        sqlx::query_scalar("SELECT `chat_restricted_until` FROM `characters` WHERE `id` = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unix(Some(saved)), until);

    let (loaded, _) = load_character(&server, 1, 1)
        .await
        .unwrap()
        .expect("saved character");
    assert_eq!(loaded.chat_restricted_until, until);

    character.chat_restricted_until = 0;
    let mut transaction = pool.begin().await.unwrap();
    save_character(&mut transaction, &mut character, Utc::now().timestamp())
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let (loaded, _) = load_character(&server, 1, 1)
        .await
        .unwrap()
        .expect("saved character");
    assert_eq!(loaded.chat_restricted_until, 0);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_learned_crafts_survive_character_reload() {
    use crate::data::character_templates::CharacterTemplate;
    use crate::data::GameData;
    use crate::test_support::test_server_with;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;

    let mut data = GameData::empty();
    data.characters.templates.insert(
        17,
        CharacterTemplate {
            race: 1,
            gender: 1,
            creatable: true,
            ..Default::default()
        },
    );
    let mut server = test_server_with(data);
    server.db = pool.clone();

    let mut character = Character::new(1, 1, UnitCustomModelParams::default());
    character.race = 1;
    character.gender = 1;
    character.unit.name = "CraftReload".into();
    character.created = Utc::now().timestamp();
    assert!(character.learn_craft(987));
    assert!(character.learn_craft(480));
    assert_eq!(character.learned_crafts_unsaved.len(), 2);
    let mut transaction = pool.begin().await.unwrap();
    save_character(&mut transaction, &mut character, Utc::now().timestamp())
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let mut loaded = Character::new(1, 1, UnitCustomModelParams::default());
    loaded.race = 1;
    loaded.gender = 1;
    load_character_details(&server, &mut loaded).await.unwrap();
    assert_eq!(
        loaded.learned_crafts,
        std::collections::HashSet::from([987u32, 480])
    );
    assert!(loaded.learned_crafts_unsaved.is_empty());
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_direct_save_failure_keeps_receipts_and_bookkeeping() {
    use crate::systems::social::achievement_persistence::load_achievement_state;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;

    // The previously saved character: one paid completion (achievement 1) and one record.
    let earned = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp");
    let mut character = Character::new(1, 1, UnitCustomModelParams::default());
    character.unit.name = "DirectSaveRollback".into();
    character.created = Utc::now().timestamp();
    character.money = 100;
    character.achievement_state.restore_record(10, 50);
    character
        .achievement_state
        .restore_progress(1, 50, Some(earned));
    assert!(character.achievement_state.mark_reward_delivered(1));
    assert!(save_directly_to_database(&pool, &mut character).await);
    let saved_updated = character.updated;

    // The next snapshot moves the wallet, the record and the amount and queues removed
    // entries, while the completion (and its receipt) stays as it is.
    character.money = 200;
    character.removed_friends = vec![7];
    character.removed_blocked = vec![8];
    character.skills.removed = vec![9];
    character.quests.removed = vec![10];
    character.achievement_state.restore_record(10, 60);
    character
        .achievement_state
        .restore_progress(1, 60, Some(earned));
    assert!(character
        .achievement_state
        .progress(1)
        .expect("row")
        .reward_delivered());

    // The snapshot's amount violates this constraint, so the achievement INSERT fails after
    // the `characters` REPLACE and the achievement DELETEs ran in the transaction.
    sqlx::query(
        "ALTER TABLE `character_achievements` \
             ADD CONSTRAINT `chk_direct_save_rollback` CHECK (`amount` <= 50)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(!save_directly_to_database(&pool, &mut character).await);

    // The failed transaction is rolled back: the rows are still the ones the previous save
    // wrote, and the paid completion keeps its receipt.
    let money: i64 =
        sqlx::query_scalar("SELECT CAST(money AS SIGNED) FROM `characters` WHERE `id` = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(money, 100);
    let state = load_achievement_state(&pool, 1).await.unwrap();
    assert_eq!(state.record_value(10), 50);
    assert_eq!(state.progress_snapshot().len(), 1);
    assert_eq!(state.amount(1), 50);
    assert_eq!(state.completed_at(1), Some(earned));
    assert!(state.progress(1).expect("row").reward_delivered());
    assert!(!state.has_pending_reward(1));
    // The passed character keeps its bookkeeping: the failed save neither advanced `updated`
    // nor cleared the removed lists on it.
    assert_eq!(character.updated, saved_updated);
    assert_eq!(character.removed_friends, [7]);
    assert_eq!(character.removed_blocked, [8]);
    assert_eq!(character.skills.removed, [9]);
    assert_eq!(character.quests.removed, [10]);

    // The retry saves the same snapshot and applies it to the character: `updated` moves on,
    // the removed lists are cleared and the paid completion is rewritten exactly once.
    while Utc::now().timestamp() <= saved_updated {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    sqlx::query("ALTER TABLE `character_achievements` DROP CHECK `chk_direct_save_rollback`")
        .execute(&pool)
        .await
        .unwrap();
    assert!(save_directly_to_database(&pool, &mut character).await);
    assert!(character.updated > saved_updated);
    assert!(character.removed_friends.is_empty());
    assert!(character.removed_blocked.is_empty());
    assert!(character.skills.removed.is_empty());
    assert!(character.quests.removed.is_empty());

    let money: i64 =
        sqlx::query_scalar("SELECT CAST(money AS SIGNED) FROM `characters` WHERE `id` = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(money, 200);
    let state = load_achievement_state(&pool, 1).await.unwrap();
    assert_eq!(state.record_value(10), 60);
    assert_eq!(
        state.progress_snapshot().len(),
        1,
        "no duplicate receipt row"
    );
    assert_eq!(state.amount(1), 60);
    assert_eq!(state.completed_at(1), Some(earned));
    assert!(state.progress(1).expect("row").reward_delivered());
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_spent_labor_survives_reload() {
    use crate::systems::quests::bridges::GameQuestHooks;
    use crate::systems::quests::hooks::QuestHooks;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    // `accounts` is shadowed too, so the reload reads the fixture's labor.
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;
    sqlx::query("INSERT INTO `accounts` (`account_id`, `labor`) VALUES (1, 1000)")
        .execute(&pool)
        .await
        .unwrap();

    let server = crate::test_support::test_server();
    let mut character = Character::new(1, 1, UnitCustomModelParams::default());
    character.unit.name = "SpentLaborFixture".into();
    character.created = Utc::now().timestamp();
    character.labor_power = 1000;
    // Spend 100 labor in play (skills path) and 5 more through the quest bridge
    // (`GameQuestHooks::change_labor` with no connection, like a quest script spend).
    assert!(crate::systems::skills::progress::change_labor_locked(
        &server,
        &mut character,
        -100,
        0
    )
    .is_some());
    GameQuestHooks.change_labor(&server, None, &mut character, -5, 0);
    assert_eq!(character.labor_power, 895);

    let mut transaction = pool.begin().await.unwrap();
    save_character(&mut transaction, &mut character, Utc::now().timestamp())
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let reloaded = crate::db::get_account_details(&pool, &server.config, 1).await;
    assert_eq!(
        reloaded.labor, 895,
        "labor spent in play must survive the reload"
    );

    // Second save with a spend racing the capture: the snapshot flushes only the captured
    // delta, and `commit_financial_snapshot` subtracts it from the live character so a later
    // spend stays pending (the save itself runs on the captured clone).
    character.change_labor_power(-7);
    let ack_labor = character.labor_power_unsaved;
    let mut captured = character.clone();
    let mut transaction = pool.begin().await.unwrap();
    save_character(&mut transaction, &mut captured, Utc::now().timestamp())
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    // The live character spends 7 more between the capture and the save ack.
    character.change_labor_power(-7);
    character.labor_power_unsaved -= ack_labor;
    assert_eq!(captured.labor_power_unsaved, 0);
    assert_eq!(character.labor_power, 881);
    assert_eq!(character.labor_power_unsaved, -7);
    assert_eq!(
        accounts_labor(&pool).await,
        888,
        "the captured save flushed only the captured delta"
    );
    assert_eq!(
        accounts_labor(&pool).await + i64::from(character.labor_power_unsaved),
        i64::from(character.labor_power),
        "saved labor + pending delta must match the live cache"
    );

    // Offline path (`do_add_labor` without a live character): `add_labor_capped` adds below
    // the cap, clamps the result to it, then makes an at-cap row a no-op.
    assert_eq!(
        crate::db::add_labor_capped(&pool, 1, 10, 2000)
            .await
            .unwrap(),
        1
    );
    assert_eq!(accounts_labor(&pool).await, 898);
    assert_eq!(
        crate::db::add_labor_capped(&pool, 1, 1500, 2000)
            .await
            .unwrap(),
        1
    );
    assert_eq!(accounts_labor(&pool).await, 2000);
    assert_eq!(
        crate::db::add_labor_capped(&pool, 1, 10, 2000)
            .await
            .unwrap(),
        0
    );
    assert_eq!(accounts_labor(&pool).await, 2000);
    pool.close().await;
}

/// The death penalty's recoverable experience survives the relog: `save_character` writes it
/// and `load_character` reads it back, so the priest prayer after a new session restores the
/// exp lost before the logout (the prayer flow itself is in
/// `systems::skills::exp_recovery_tests`).
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_recoverable_exp_survives_reload() {
    use crate::test_support::{test_character, test_experience, test_server_with};

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;
    sqlx::query("INSERT INTO `accounts` (`account_id`, `labor`) VALUES (1, 100)")
        .execute(&pool)
        .await
        .unwrap();

    let mut data = crate::data::GameData::empty();
    data.experience = test_experience(55);
    let mut server = test_server_with(data);
    server.db = pool.clone();
    let mut character = test_character(1, "Recoverable", 100.0, 100.0);
    character.account_id = 1;
    character.unit.level = 10;
    character.experience = 10_450; // level 10 death: 50 lost, 40 recoverable
    character.recoverable_exp = 40;
    character.created = Utc::now().timestamp();
    let mut transaction = pool.begin().await.unwrap();
    save_character(&mut transaction, &mut character, Utc::now().timestamp())
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let (loaded, _) = load_character(&server, 1, 1)
        .await
        .unwrap()
        .expect("saved character");
    assert_eq!(loaded.experience, 10_450);
    assert_eq!(
        loaded.recoverable_exp, 40,
        "the death penalty's recoverable exp must survive the relog"
    );
    assert_eq!(loaded.labor_power, 100);
    pool.close().await;
}

/// The real labor save path (`SaveManager` snapshot capture/write/commit and the departing
/// save) against one online character: a rolled-back snapshot must not move the wallet, a
/// committed retry must flush only the captured delta while a spend races it, and the
/// departure must flush what is left.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_labor_snapshot_rollback_retry_inflight_and_departure() {
    use crate::managers::save::{
        capture_financial_snapshot, commit_financial_snapshot, write_financial_snapshot,
    };
    use crate::test_support::{spawn_player, test_character, test_server, TestClient};
    use std::sync::Arc;

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
    // The financial tables the snapshot path touches, on top of the character save's.
    shadow_tables(
        &pool,
        &[
            "housings",
            "dominions",
            "dominion_siege_participants",
            "mails",
            "items",
            "item_containers",
            "auction_house",
        ],
    )
    .await;
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;
    sqlx::query("INSERT INTO `accounts` (`account_id`, `labor`) VALUES (1, 1000)")
        .execute(&pool)
        .await
        .unwrap();

    let mut server = test_server();
    server.db = pool.clone();
    let server = Arc::new(server);
    server.set_self();
    let client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "SnapshotLaborer", 500.0, 500.0);
    c.account_id = 1;
    c.labor_power = 1000;
    c.is_online = true;
    let p = spawn_player(&server, &client, c, 0x101);
    assert_eq!(accounts_labor(&pool).await, 1000);

    // 1. A rolled-back snapshot: the wallet write is undone and the live cache keeps its
    //    pending spend.
    crate::systems::skills::progress::change_labor(&server, &p, -100, 0);
    assert_eq!(p.data().labor_power, 900);
    let snapshot = capture_financial_snapshot(&server);
    let mut transaction = pool.begin().await.unwrap();
    write_financial_snapshot(&server, &mut transaction, snapshot)
        .await
        .unwrap();
    transaction.rollback().await.unwrap();
    assert_eq!(accounts_labor(&pool).await, 1000);
    assert_eq!(p.data().labor_power_unsaved, -100);

    // 2. The retry commits the captured -100 while the player spends 7 more in flight: the
    //    ack flushes only the captured delta and leaves the later spend pending.
    let snapshot = capture_financial_snapshot(&server);
    crate::systems::skills::progress::change_labor(&server, &p, -7, 0);
    let mut transaction = pool.begin().await.unwrap();
    let receipt = write_financial_snapshot(&server, &mut transaction, snapshot)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    commit_financial_snapshot(&server, receipt);
    assert_eq!(accounts_labor(&pool).await, 900);
    assert_eq!(p.data().labor_power, 893);
    assert_eq!(p.data().labor_power_unsaved, -7);

    // 3. The departure (`GameConnection.SaveAndRemoveFromWorld`): the pending -7 reaches the
    //    wallet, the cache is clean and the departing entry is dropped.
    server.save.remember_departing(&p);
    if server
        .world
        .character_by_obj_id(p.obj_id)
        .is_some_and(|live| Arc::ptr_eq(&live, &p))
    {
        server.world.try_remove_character(p.obj_id);
    }
    let _transaction = server.save.transaction_lock().await;
    assert!(server.save.save_departing_locked(&server, &p).await);
    assert_eq!(accounts_labor(&pool).await, 893);
    assert_eq!(p.data().labor_power_unsaved, 0);
    assert!(server.save.pending_departure(p.character_id).is_none());

    // 4. What the next character load (`Character.Load` -> account details) reads.
    let reloaded = crate::db::get_account_details(&pool, &server.config, 1).await;
    assert_eq!(reloaded.labor, 893);
    pool.close().await;
}

/// Character creation writes the starting supplies together with the `characters` row: a
/// stop before the next world save must not keep the character but lose its items.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_new_character_items_are_saved_with_the_character() {
    use crate::test_support::test_character;

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
    shadow_tables(&pool, &["items", "item_containers"]).await;
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;
    sqlx::query("INSERT INTO `accounts` (`account_id`, `labor`) VALUES (1007, 1000)")
        .execute(&pool)
        .await
        .unwrap();

    let mut data = crate::data::GameData::empty();
    let mut template = crate::data::items::ItemTemplate::plain(1000, 20);
    template.name = "Starter".into();
    data.items.templates.insert(1000, template);
    let mut server = crate::test_support::test_server_with(data);
    server.db = pool.clone();
    let mut c = test_character(7, "Newbie", 500.0, 500.0);
    c.inventory = Inventory::new(7, 50, 50, &server.items, &server.ids.container);
    let item = server
        .items
        .create(1000, 1, 0, 1, &server.game_data.items, &server.ids.item, 0)
        .expect("item");
    let item_id = item.id;
    c.inventory
        .bag()
        .lock()
        .unwrap()
        .add_new_item(item, 0, &server.game_data.items)
        .expect("fits");

    let _transaction = server.save.transaction_lock().await;
    assert!(save_new_character_locked(&server, &mut c).await);

    let rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM items i JOIN item_containers ic              ON ic.container_id = i.container_id WHERE ic.owner_id = 7 AND i.id = ?",
        )
        .bind(item_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    // The rows are marked clean, so the next world save does not rewrite them.
    assert!(server.items.prepare_save_for_owner(7).item_ids().is_empty());
    pool.close().await;
}

#[tokio::test]
async fn learn_skill_load_refuses_a_skill_above_the_ability_level() {
    // A stored attribute-less ability_level 99 row (as a pre-fix server could write) is not
    // re-learned at load either: the C# recomputed a wrapped level and kept failing to save.
    let mut data = crate::data::GameData::empty();
    data.experience = crate::test_support::test_experience(55);
    data.skills.add_skill(crate::data::skills::SkillTemplate {
        id: 10385,
        ability_id: 0,
        ability_level: 99,
        level_step: 3,
        skill_points: 1,
        ..Default::default()
    });
    let server = crate::test_support::test_server_with(data);
    let mut c = crate::test_support::test_character(1, "Stored", 100.0, 100.0);
    c.unit.level = 55;
    learn_skill(&server, &mut c, 10385);
    assert!(c.skills.skills.is_empty());
}

#[tokio::test]
async fn stored_skill_cleanup_keeps_class_and_default_skills_only() {
    // Load-time filter: an ability-less NPC / GM skill a forged CSLearnSkill stored is
    // dropped; class skills of an active ability and server-granted defaults stay.
    let mut data = crate::data::GameData::empty();
    for (id, ability_id, need_learn) in [
        (23071, 0, true), // NPC / GM skill
        (40, 1, true),    // class skill, active ability
        (41, 2, true),    // class skill, inactive ability
        (42, 0, false),   // server-granted default
    ] {
        data.skills.add_skill(crate::data::skills::SkillTemplate {
            id,
            ability_id,
            need_learn,
            ..Default::default()
        });
    }
    data.skills
        .default_skills
        .push(crate::data::skills::DefaultSkill {
            skill_id: 42,
            slot: 0,
            add_to_slot: false,
        });
    let server = crate::test_support::test_server_with(data);
    let mut c = crate::test_support::test_character(1, "Stored", 100.0, 100.0);
    c.ability1 = 1;
    assert!(!stored_skill_allowed(&server, &c, 23071));
    assert!(stored_skill_allowed(&server, &c, 40));
    assert!(!stored_skill_allowed(&server, &c, 41));
    assert!(stored_skill_allowed(&server, &c, 42));
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_retired_skill_level_is_savable() {
    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;

    // Skill 10385: ability_level 99, level_step 3, ability level 55 (the cap). The old
    // formula stored level 243, MySQL rejects it (1264) and the whole save rolled back.
    let mut data = crate::data::GameData::empty();
    data.skills.add_skill(crate::data::skills::SkillTemplate {
        id: 10385,
        ability_id: 0,
        ability_level: 99,
        level_step: 3,
        skill_points: 1,
        ..Default::default()
    });
    let mut character = Character::new(1, 1, UnitCustomModelParams::default());
    character.unit.name = "RetiredSkill".into();
    character.created = Utc::now().timestamp();
    character.skills.add_skill_template(&data.skills, 10385, 55);
    assert_eq!(character.skills.skills[0].level, 1);

    assert!(save_directly_to_database(&pool, &mut character).await);
    let (id, level): (i64, i64) = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), CAST(`level` AS SIGNED) FROM `skills` WHERE `owner` = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((id, level), (10385, 1));
    pool.close().await;
}

/// `save_character` -> `load_character_details` must keep running skill and shared-tag
/// cooldowns (absolute expiries) and drop the expired ones, so Recall / Escape / a long
/// cooldown consumable cannot be recast right after a relog.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_cooldowns_survive_relog_and_expired_rows_are_dropped() {
    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;

    let now = Utc::now().timestamp_millis();
    let mut character = Character::new(1, 1, UnitCustomModelParams::default());
    character.unit.name = "CooldownFixture".into();
    character.created = Utc::now().timestamp();
    character.unit.cooldowns.add_cooldown(14849, 60_000, now); // Recall
    character.unit.cooldowns.add_tag_cooldown(144, 120_000, now); // shared potion group
    character.unit.cooldowns.cooldowns.insert(20297, now - 1); // Escape: already over

    assert!(save_directly_to_database(&pool, &mut character).await);
    let rows = fetch_character_cooldowns(&pool, 1).await;
    assert_eq!(rows.len(), 2, "expired Escape row is not written");
    assert!(rows
        .iter()
        .any(|r| r.kind == COOLDOWN_TYPE_SKILL && r.id == 14849 && r.expires_at_ms > now));
    assert!(rows
        .iter()
        .any(|r| r.kind == COOLDOWN_TYPE_TAG && r.id == 144 && r.expires_at_ms > now));

    let mut reloaded = Character::new(1, 1, UnitCustomModelParams::default());
    reloaded.id = 1;
    apply_cooldown_rows(&mut reloaded, &rows, Utc::now().timestamp_millis());
    assert!(reloaded.unit.cooldowns.cooldowns.contains_key(&14849));
    assert!(reloaded.unit.cooldowns.tag_cooldowns.contains_key(&144));
    assert!(!reloaded.unit.cooldowns.cooldowns.contains_key(&20297));

    // A save with nothing running clears the table (the rows are rewritten, not appended).
    character.unit.cooldowns = crate::models::unit::UnitCooldowns::default();
    assert!(save_directly_to_database(&pool, &mut character).await);
    assert!(fetch_character_cooldowns(&pool, 1).await.is_empty());
    pool.close().await;
}

/// Offline infamy reduction (a quest reward / crime change saved while the character was
/// away) leaves the faction to the select path, exactly like the pirate conversion. The
/// select sees saved faction 161 with infamy 0 and pardons through the shared helper; a
/// following save persists the home faction.
#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses only temporary tables"]
async fn private_mysql_select_pardons_an_offline_pirate_at_zero_infamy() {
    use crate::data::character_templates::CharacterTemplate;
    use crate::data::GameData;
    use crate::systems::social::achievement::char_record_kind;
    use crate::systems::social::achievement_flow_tests::one_achievement;
    use crate::test_support::{test_character, test_experience, test_server_with, TestClient};
    use crate::world::session::{select_loaded, SelectDbData};

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("private MySQL");
    shadow_tables(&pool, CHARACTER_SAVE_TABLES).await;

    let mut data = GameData::empty();
    data.experience = test_experience(55);
    data.characters.templates.insert(
        0x11,
        CharacterTemplate {
            race: 1,
            gender: 1,
            creatable: true,
            faction_id: 101,
            ..Default::default()
        },
    );
    data.systems.social.achievements = one_achievement(
        1380,
        1,
        0,
        2401,
        char_record_kind::MY_DISHONORABLE_POINT,
        0,
        0,
    );
    let mut server = test_server_with(data);
    server.db = pool.clone();
    let server = std::sync::Arc::new(server);
    server.set_self();
    crate::systems::social::install_hooks(&server);

    let now = Utc::now().timestamp();
    let mut character = test_character(1, "OfflinePirate", 100.0, 100.0);
    character.unit.faction_id = FACTION_PIRATE;
    character.crime_point = 0;
    character.infamy_point = 0;
    character.created = now;
    let mut transaction = pool.begin().await.unwrap();
    save_character(&mut transaction, &mut character, now)
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let (loaded, _) = load_character(&server, 1, character.account_id)
        .await
        .unwrap()
        .expect("saved character");
    assert_eq!(loaded.unit.faction_id, FACTION_PIRATE);

    let mut client = TestClient::new(&server, 1, character.account_id);
    select_loaded(&server, &mut client.conn, loaded, SelectDbData::default()).unwrap();
    let player = client.conn.active_char.clone().expect("selected");
    {
        let c = player.data();
        assert_eq!(c.unit.faction_id, 101, "the select pardoned the pirate");
        assert_eq!(persisted_faction(&c), 101);
        assert_eq!(
            c.achievement_state.record_value(2401),
            1,
            "repentant (kind 49) credited at select"
        );
    }

    let mut saved = player.data().clone();
    let mut transaction = pool.begin().await.unwrap();
    save_character(&mut transaction, &mut saved, now)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let faction: i64 =
        sqlx::query_scalar("SELECT CAST(faction_id AS SIGNED) FROM `characters` WHERE `id` = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(faction, 101, "the next save persists the home faction");
    pool.close().await;
}

async fn accounts_labor(pool: &MySqlPool) -> i64 {
    sqlx::query_scalar("SELECT CAST(labor AS SIGNED) FROM `accounts` WHERE `account_id` = 1")
        .fetch_one(pool)
        .await
        .unwrap()
}
