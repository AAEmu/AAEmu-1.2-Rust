//! The repairman's destroyed-item update: only the player's own containers are consulted, so
//! an item that vanished between the scan and the update is refunded instead of repaired from
//! the global item store.

use super::repair_destroyed_slave_items;
use crate::data::items::{ItemData, ItemTemplate, TemplateKind};
use crate::models::container::{slot_type, ContainerKind, ItemContainer};
use crate::models::item::Item;

fn slave_item(id: u64, data: &ItemData, destroyed: bool) -> Item {
    let template = ItemTemplate {
        kind: TemplateKind::SummonSlave { slave_id: 1 },
        ..ItemTemplate::plain(5001, 1)
    };
    let mut item = Item::create(id, &template, 1, 0, 1, data, 0);
    item.details.slave_is_destroyed = u8::from(destroyed);
    item.slot_type = slot_type::INVENTORY;
    item
}

#[test]
fn repair_updates_only_items_still_in_the_given_containers_and_refunds_the_rest() {
    let data = crate::data::GameData::empty().items;
    let bag = ItemContainer::new(ContainerKind::Item, 1, slot_type::INVENTORY, 1).into_ref();
    bag.lock().unwrap().items.push(slave_item(11, &data, true));
    bag.lock().unwrap().items.push(slave_item(12, &data, false));

    // 11 is in the bag and destroyed; 12 is in the bag but already repaired; 13 was traded
    // away after the scan (not in any given container).
    let (tasks, refund) = repair_destroyed_slave_items(
        std::slice::from_ref(&bag),
        &[(11, 0, 50), (12, 0, 70), (13, 0, 30)],
    );
    assert_eq!(tasks.len(), 1);
    assert_eq!(refund, 70 + 30);
    let bag = bag.lock().unwrap();
    assert_eq!(
        bag.get_item_by_id(11).unwrap().details.slave_is_destroyed,
        0
    );
    assert_eq!(
        bag.get_item_by_id(12).unwrap().details.slave_is_destroyed,
        0
    );
}
