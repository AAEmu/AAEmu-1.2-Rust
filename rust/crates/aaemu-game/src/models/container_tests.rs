//! Port of AAEmu.UnitTests/Game/Models/Game/Items/Containers/InventoryTests.cs
//!
//! Includes the test helpers it uses: `Utils/InventoryTestUtils.cs` (`MockItem`),
//! `Utils/Mocks/ItemMock.cs` and `Utils/Mocks/CharacterMock.cs`. The C# mocks carry their
//! `ItemTemplate` on the item; the Rust container resolves templates through `ItemData`, so the
//! mock template is registered there.

use crate::data::items::{bind_type, ItemData, ItemTemplate};
use crate::models::container::{slot_type, ContainerKind, ItemContainer};
use crate::models::item::Item;
use crate::models::item_task::{task_type, ItemFx, ItemFxList, ItemTask};

/// `CharacterMock` (a `Character` with no connection; only its id is used here).
struct CharacterMock {
    id: u32,
}

impl CharacterMock {
    fn new() -> Self {
        // `new CharacterMock()`: `Character(null)` leaves `Id` at 0.
        Self { id: 0 }
    }
}

/// `ItemMock(id, template, count = 1)`: a base `Item` with the given template.
fn item_mock(id: u64, template: &ItemTemplate, count: i32) -> Item {
    Item::with_template(1, id, template, count)
}

/// `InventoryTestUtils.MockItem(id, templateId)`: a `BindType.Normal` template. Returns the
/// template too (registered in `ItemData` by the caller).
fn mock_item(id: u64, template_id: u32) -> (Item, ItemTemplate) {
    let template = ItemTemplate {
        bind_type: bind_type::NORMAL,
        ..ItemTemplate::new(template_id, crate::data::items::TemplateKind::Item)
    };
    (item_mock(id, &template, 1), template)
}

#[test]
fn inventory_adds_item() {
    let mock_character = CharacterMock::new();
    // new ItemContainer(mockCharacter.Id, SlotType.Inventory, false, mockCharacter)
    let mut container = ItemContainer::new(
        ContainerKind::Item,
        mock_character.id,
        slot_type::INVENTORY,
        0,
    );
    container.has_owner = true;
    let (item, template) = mock_item(1, 1);
    let mut data = ItemData::default();
    data.templates.insert(template.id, template);

    let mut fx = ItemFxList::default();
    assert_eq!(
        container.add_item(task_type::GM, item, 1, &data, &mut fx),
        Ok(true)
    );

    let found: Vec<&Item> = container
        .items
        .iter()
        .filter(|it| it.template_id == 1)
        .collect();
    assert_eq!(found.len(), 1, "SingleOrDefault");
}

fn consumption_fixture(count: i32, equipment: bool) -> (crate::GameServer, ItemContainer) {
    let mut data = crate::data::GameData::empty();
    let mut template = ItemTemplate::plain(1, 100);
    if equipment {
        template.kind =
            crate::data::items::TemplateKind::Armor(crate::data::items::EquipTemplate {
                slot_type_id: 3, // chest
                ..Default::default()
            });
        template.max_count = 1;
    }
    data.items.templates.insert(1, template);
    let server = crate::test_support::test_server_with(data);
    let (kind, st, slot) = if equipment {
        (ContainerKind::Equipment, slot_type::EQUIPMENT, 2)
    } else {
        (ContainerKind::Item, slot_type::INVENTORY, 0)
    };
    let mut container = ItemContainer::new(kind, 7, st, 100);
    container.has_owner = true;
    let item = Item::create(
        1,
        server.game_data.items.template(1).unwrap(),
        count,
        0,
        1,
        &server.game_data.items,
        0,
    );
    assert_eq!(
        container.add_new_item(item, slot, &server.game_data.items),
        Ok(true)
    );
    (server, container)
}

fn consumed(fx: &ItemFxList) -> Vec<(u32, u32, i32)> {
    fx.0.iter()
        .filter_map(|fx| match fx {
            ItemFx::Consumed {
                owner,
                template_id,
                count,
            } => Some((*owner, *template_id, *count)),
            _ => None,
        })
        .collect()
}

fn tasks(fx: &ItemFxList) -> Vec<ItemTask> {
    fx.0.iter()
        .filter_map(|fx| match fx {
            ItemFx::Tasks { tasks, .. } => Some(tasks.iter().cloned()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn full_stack_consumption(preferred: Option<u64>) {
    let (server, mut container) = consumption_fixture(3, false);
    let before = container.get_item_by_id(1).unwrap().clone();
    let mut env = crate::systems::economy::inventory::server_env(&server);
    assert_eq!(
        container.consume_item(
            task_type::SKILL_EFFECT_CONSUMPTION,
            1,
            3,
            preferred,
            &mut env
        ),
        3
    );
    assert!(container.items.is_empty());
    assert_eq!(consumed(&env.fx), vec![(7, 1, 3)]);
    assert_eq!(tasks(&env.fx), vec![ItemTask::remove_slot(&before)]);
    assert_eq!(
        env.fx
            .0
            .iter()
            .filter(|fx| matches!(fx, ItemFx::ReleaseId(1)))
            .count(),
        1
    );
}

#[tokio::test]
async fn consumption_full_stack_reports_actual_count_once() {
    full_stack_consumption(None);
}

#[tokio::test]
async fn consumption_preferred_full_stack_reports_actual_count_once() {
    full_stack_consumption(Some(1));
}

#[tokio::test]
async fn consumption_partial_stack_reports_delta_instead_of_remaining_count() {
    for preferred in [None, Some(1)] {
        let (server, mut container) = consumption_fixture(3, false);
        let before = container.get_item_by_id(1).unwrap().clone();
        let mut env = crate::systems::economy::inventory::server_env(&server);
        assert_eq!(
            container.consume_item(
                task_type::SKILL_EFFECT_CONSUMPTION,
                1,
                1,
                preferred,
                &mut env
            ),
            1
        );
        assert_eq!(container.get_item_by_id(1).unwrap().count, 2);
        assert_eq!(consumed(&env.fx), vec![(7, 1, 1)]);
        assert_eq!(tasks(&env.fx), vec![ItemTask::count_update(&before, -1)]);
        assert!(!env.fx.0.iter().any(|fx| matches!(fx, ItemFx::ReleaseId(_))));
    }
}

#[tokio::test]
async fn consumption_multiple_stacks_reports_each_actual_delta_once() {
    for preferred in [None, Some(2)] {
        let (server, mut container) = consumption_fixture(2, false);
        let first = container.get_item_by_id(1).unwrap().clone();
        let second = Item::create(
            2,
            server.game_data.items.template(1).unwrap(),
            3,
            0,
            1,
            &server.game_data.items,
            0,
        );
        assert_eq!(
            container.add_new_item(second, 1, &server.game_data.items),
            Ok(true)
        );
        let second = container.get_item_by_id(2).unwrap().clone();
        let mut env = crate::systems::economy::inventory::server_env(&server);
        assert_eq!(
            container.consume_item(
                task_type::SKILL_EFFECT_CONSUMPTION,
                1,
                4,
                preferred,
                &mut env
            ),
            4
        );
        if preferred.is_some() {
            assert_eq!(consumed(&env.fx), vec![(7, 1, 3), (7, 1, 1)]);
            assert_eq!(container.get_item_by_id(1).unwrap().count, 1);
            assert_eq!(
                tasks(&env.fx),
                vec![
                    ItemTask::remove_slot(&second),
                    ItemTask::count_update(&first, -1)
                ]
            );
        } else {
            assert_eq!(consumed(&env.fx), vec![(7, 1, 2), (7, 1, 2)]);
            assert_eq!(container.get_item_by_id(2).unwrap().count, 1);
            assert_eq!(
                tasks(&env.fx),
                vec![
                    ItemTask::remove_slot(&first),
                    ItemTask::count_update(&second, -2)
                ]
            );
        }
    }
}

#[tokio::test]
async fn consumption_equipment_notifies_consumption_before_equip_leave() {
    for preferred in [None, Some(1)] {
        let (server, mut container) = consumption_fixture(1, true);
        let before = container.get_item_by_id(1).unwrap().clone();
        let mut env = crate::systems::economy::inventory::server_env(&server);
        assert_eq!(
            container.consume_item(
                task_type::SKILL_EFFECT_CONSUMPTION,
                1,
                1,
                preferred,
                &mut env
            ),
            1
        );
        assert_eq!(consumed(&env.fx), vec![(7, 1, 1)]);
        let consumed_at = env
            .fx
            .0
            .iter()
            .position(|fx| matches!(fx, ItemFx::Consumed { .. }))
            .unwrap();
        let leave_at = env
            .fx
            .0
            .iter()
            .position(|fx| matches!(fx, ItemFx::EquipLeave { .. }))
            .unwrap();
        assert!(consumed_at < leave_at);
        assert_eq!(tasks(&env.fx), vec![ItemTask::remove_slot(&before)]);
    }
}

#[tokio::test]
async fn consumption_direct_removal_retains_the_original_stack_count_and_task() {
    let (server, mut container) = consumption_fixture(3, false);
    let before = container.get_item_by_id(1).unwrap().clone();
    let mut env = crate::systems::economy::inventory::server_env(&server);
    assert_eq!(
        container.remove_item_task(task_type::DESTROY, 1, true, &mut env),
        Some(before.clone())
    );
    assert_eq!(consumed(&env.fx), vec![(7, 1, 3)]);
    assert_eq!(tasks(&env.fx), vec![ItemTask::remove_slot(&before)]);
    assert!(container.items.is_empty());
}
