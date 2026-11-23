use super::*;

#[test]
fn phantom_message_names_branch_owner_and_request() {
    let req = MoveRequest {
        task: task_type::SWAP_ITEMS,
        from_item_id: 16_777_663,
        from_type: slot_type::INVENTORY,
        from_slot: 5,
        to_item_id: 0,
        to_type: slot_type::EQUIPMENT,
        to_slot: 17,
        count: 3,
    };
    let msg = phantom_item_message("from-holder", 16_777_663, 6, "Debil", &req, Some(false));
    // The original C# text stays greppable.
    assert!(msg.starts_with(
        "SplitOrMoveItem - ItemId 16777663 no longer exists, possibly a phantom item"
    ));
    assert!(msg.contains("(from-holder)"));
    assert!(msg.contains("character 6 (Debil)"));
    assert!(msg.contains(&format!("task {}", task_type::SWAP_ITEMS)));
    assert!(msg.contains(&format!("from item 16777663 {}:5", slot_type::INVENTORY)));
    assert!(msg.contains(&format!("to item 0 {}:17", slot_type::EQUIPMENT)));
    assert!(msg.contains("count 3"));
    assert!(msg.contains("known elsewhere on server: no"));
    assert!(!msg.contains('\n'));
    assert!(phantom_item_message("ex-decide", 1, 6, "", &req, None)
        .contains("known elsewhere on server: n/a"));
    assert!(
        phantom_item_message("to-holder", 1, 6, "", &req, Some(true))
            .contains("known elsewhere on server: yes")
    );
}

#[test]
fn move_rejection_message_names_request_and_item_facts() {
    let req = MoveRequest {
        task: task_type::SWAP_ITEMS,
        from_item_id: 100,
        from_type: slot_type::INVENTORY,
        from_slot: 5,
        to_item_id: 200,
        to_type: slot_type::INVENTORY,
        to_slot: 6,
        count: 0,
    };
    let from = Item {
        id: 100,
        template_id: 8001,
        slot: 5,
        count: 7,
        slot_type: slot_type::INVENTORY,
        ..Item::new(0)
    };
    let target = Item {
        id: 200,
        template_id: 8001,
        slot: 6,
        count: 90,
        slot_type: slot_type::INVENTORY,
        ..Item::new(0)
    };
    let facts = MoveFacts {
        effective_count: 7,
        from_item: Some(&from),
        target_item: Some(&target),
        max_count: 100,
        src_container: (42, slot_type::INVENTORY),
    };
    let msg = move_rejection_message(
        "SplitOrMoveItem Target Item stack does not have enough room to take source",
        "detail",
        6,
        "Debil",
        &req,
        &facts,
    );
    assert!(msg.starts_with(
        "SplitOrMoveItem Target Item stack does not have enough room to take source (detail):"
    ));
    assert!(msg.contains("character 6 (Debil)"));
    assert!(msg.contains("from item 100"));
    assert!(msg.contains("to item 200"));
    assert!(msg.contains("requested count 0, effective count 7"));
    assert!(msg.contains("source container 42"));
    assert!(msg.contains("source item template 8001 count 7"));
    assert!(msg.contains("target item template 8001 count 90"));
    assert!(msg.contains("source max_count 100"));
    assert!(!msg.contains('\n'));
    let none = MoveFacts {
        from_item: None,
        target_item: None,
        ..facts
    };
    assert!(move_rejection_message("p", "d", 1, "", &req, &none).contains("source item none"));
}
