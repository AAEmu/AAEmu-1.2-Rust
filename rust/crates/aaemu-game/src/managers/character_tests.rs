//! Port of AAEmu.UnitTests/Game/Core/Managers/CharacterManagerTests.cs
//!
//! `Constructor_DoesNotCallDeps` verifies the DI wiring of `CharacterManager`. The Rust
//! `CharacterManager` has no injected managers (its tables are `CharacterData`, its operations
//! free functions taking the server), so the port checks that construction has no side
//! effects: a new `CharacterData` is empty.

use crate::data::character_templates::CharacterData;
use crate::data::skills::{DefaultSkill, SkillTemplate};
use crate::data::GameData;
use crate::models::char_data::{read_action_slots, ActionSlot, ActionSlotType, MAX_ACTION_SLOTS};
use crate::models::character::Character;
use crate::test_support::test_server_with;

use super::{create, initialize_starting_skills, CreateCharacterRequest};

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
