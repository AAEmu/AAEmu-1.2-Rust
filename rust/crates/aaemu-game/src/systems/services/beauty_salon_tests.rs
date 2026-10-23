//! Salon packet flows through the real economy bridge (no MySQL).

use std::sync::Arc;

use super::*;
use crate::data::items::{ItemTemplate, TemplateKind};
use crate::models::container::slot_type;
use crate::models::unit_model::UnitCustomModelType;
use crate::systems::economy::bridges::EconomyForServices;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::tests::{give, item_at, test_data, World};
use crate::systems::services::item_ops::CharacterItemOps;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

const OLD_HAIR: u32 = 24127;
const NEW_HAIR: u32 = 24128;

fn fixture(old_hair: Option<u32>, certificates: i32) -> World {
    let mut data = test_data();
    for id in [OLD_HAIR, NEW_HAIR] {
        data.items.templates.insert(
            id,
            ItemTemplate {
                kind: TemplateKind::BodyPart {
                    slot_type_id: 24, // EquipmentItemSlotType.Hair
                    model_id: 10,
                },
                ..ItemTemplate::plain(id, 1)
            },
        );
    }
    data.items.templates.insert(
        SALON_CERTIFICATE,
        ItemTemplate::plain(SALON_CERTIFICATE, 20),
    );
    let server = Arc::new(test_server_with(data));
    server.set_self();
    CHARACTER_ITEM_OPS.install(Arc::new(EconomyForServices));
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1002);
    let pa = spawn_player(&server, &a, test_character(1, "Salon", 100.0, 100.0), 0x100);
    let pb = spawn_player(
        &server,
        &b,
        test_character(2, "Observer", 101.0, 100.0),
        0x101,
    );
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    let mut w = World {
        server,
        a,
        pa,
        b,
        pb,
    };
    for container in [Inv::of(&w.pa).equipment(), Inv::of(&w.pa).bag()] {
        w.server.items.add_container(container);
    }
    if let Some(hair) = old_hair {
        give(&w, &w.pa, slot_type::EQUIPMENT, hair, 1, equip_slot::HAIR);
    }
    if certificates > 0 {
        give(
            &w,
            &w.pa,
            slot_type::INVENTORY,
            SALON_CERTIFICATE,
            certificates,
            0,
        );
    }
    w.a.drain();
    w.b.drain();
    w
}

fn edited_model() -> UnitCustomModelParams {
    UnitCustomModelParams {
        kind: UnitCustomModelType::FACE,
        hair_color_id: 728,
        skin_color_id: 1,
        model_id: 10,
        face: Some(Default::default()),
    }
}

async fn apply(w: &mut World, hair: u32) {
    CSBeautyshopDataPacket {
        hair,
        model: edited_model(),
    }
    .handle(&w.server, &mut w.a.conn)
    .await
    .unwrap();
}

fn assert_receipt_and_payment(w: &mut World, hair: u32) {
    assert_eq!(w.pa.data().unit.model_params, edited_model());
    assert_eq!(
        EconomyForServices.items_count(&w.server, &w.pa, SALON_CERTIFICATE),
        1
    );
    let mut expected = PacketStream::new();
    expected.write_u32(w.pa.character_id).write_u32(hair);
    edited_model().write(&mut expected);
    expected.write_u32(0);
    let expected = expected.into_bytes();
    let owner = w.a.drain();
    let observer = w.b.drain();
    for frames in [&owner, &observer] {
        let receipts: Vec<_> = frames
            .iter()
            .filter(|(op, _)| *op == sc::SCCharacterGenderAndModelModifiedPacket)
            .map(|(_, body)| body)
            .collect();
        assert_eq!(receipts, vec![&expected]);
    }
    let payments: Vec<_> = owner
        .iter()
        .enumerate()
        .filter(|(_, (op, body))| {
            *op == sc::SCItemTaskSuccessPacket && body.first() == Some(&item_task::EDIT_COSMETIC)
        })
        .collect();
    assert_eq!(payments.len(), 1);
    let receipt = owner
        .iter()
        .position(|(op, _)| *op == sc::SCCharacterGenderAndModelModifiedPacket)
        .unwrap();
    assert!(
        receipt < payments[0].0,
        "receipt precedes certificate consumption, as in C#"
    );
    assert!(!observer
        .iter()
        .any(|(op, _)| *op == sc::SCItemTaskSuccessPacket));
}

async fn successful_hair_change(old_hair: Option<u32>) {
    let mut w = fixture(old_hair, 2);
    let before = Inv::of(&w.pa).equipped(equip_slot::HAIR);
    apply(&mut w, NEW_HAIR).await;
    let after = Inv::of(&w.pa)
        .equipped(equip_slot::HAIR)
        .expect("equipped hair");
    assert_eq!(after.template_id, NEW_HAIR);
    assert_eq!(after.slot_type, slot_type::EQUIPMENT);
    assert_eq!(after.slot, equip_slot::HAIR);
    assert_eq!(after.owner_id, u64::from(w.pa.character_id));
    assert_eq!(after.count, 1);
    assert_eq!(w.server.items.item_by_id(after.id), Some(after.clone()));
    if old_hair == Some(NEW_HAIR) {
        assert_eq!(Some(after), before, "unchanged hair is not recreated");
    } else {
        assert_eq!(after.made_unit_id, w.pa.character_id);
    }
    assert_receipt_and_payment(&mut w, NEW_HAIR);
}

#[tokio::test]
async fn absent_hair_is_created_and_received_by_owner_and_observer() {
    successful_hair_change(None).await;
}

#[tokio::test]
async fn same_hair_is_kept_while_model_changes_and_one_certificate_is_consumed() {
    successful_hair_change(Some(NEW_HAIR)).await;
}

#[tokio::test]
async fn changed_hair_is_replaced_and_received_by_owner_and_observer() {
    successful_hair_change(Some(OLD_HAIR)).await;
}

#[tokio::test]
async fn no_certificate_leaves_hair_and_model_unchanged_without_receipt() {
    for old_hair in [None, Some(OLD_HAIR)] {
        let mut w = fixture(old_hair, 0);
        let before = Inv::of(&w.pa).equipped(equip_slot::HAIR);
        let model = w.pa.data().unit.model_params.clone();
        apply(&mut w, NEW_HAIR).await;
        assert_eq!(Inv::of(&w.pa).equipped(equip_slot::HAIR), before);
        assert_eq!(w.pa.data().unit.model_params, model);
        assert_eq!(
            EconomyForServices.items_count(&w.server, &w.pa, SALON_CERTIFICATE),
            0
        );
        assert!(w.a.drain().is_empty());
        assert!(w.b.drain().is_empty());
    }
}

#[tokio::test]
async fn failed_hair_creation_preserves_csharp_receipt_and_consumption_behavior() {
    for old_hair in [None, Some(OLD_HAIR)] {
        let mut w = fixture(old_hair, 2);
        // Invalid template: the existing swap removes old hair before attempting creation.
        apply(&mut w, u32::MAX).await;
        assert!(Inv::of(&w.pa).equipped(equip_slot::HAIR).is_none());
        assert_eq!(item_at(&w.pa, slot_type::INVENTORY, 0).unwrap().count, 1);
        // Model edits still apply; the receipt reports actual equipment, never requested hair.
        assert_receipt_and_payment(&mut w, 0);
    }
}

#[tokio::test]
async fn failed_certificate_consumption_does_not_undo_the_appearance_receipt() {
    struct TakeCertificates;
    impl crate::systems::economy::hooks::QuestItemHooks for TakeCertificates {
        fn on_items_acquired(
            &self,
            server: &GameServer,
            player: &PlayerRef,
            template: u32,
            _: i32,
        ) {
            if template == NEW_HAIR {
                // Deterministically remove the certificates between the availability check
                // and the final consume, using the real item callbacks and inventory path.
                assert_eq!(
                    EconomyForServices.consume_item(
                        server,
                        player,
                        crate::models::item_task::task_type::INVALID,
                        SALON_CERTIFICATE,
                        2,
                    ),
                    2
                );
            }
        }
    }
    let mut w = fixture(Some(OLD_HAIR), 2);
    w.server
        .systems
        .economy
        .hooks
        .quests
        .install(Arc::new(TakeCertificates));
    apply(&mut w, NEW_HAIR).await;
    assert_eq!(hair_template(&w.pa), Some(NEW_HAIR));
    assert_eq!(w.pa.data().unit.model_params, edited_model());
    assert_eq!(
        EconomyForServices.items_count(&w.server, &w.pa, SALON_CERTIFICATE),
        0
    );
    let expected = crate::net::packet::encode(&SCCharacterGenderAndModelModifiedPacket {
        character_id: w.pa.character_id,
        hair_template_id: NEW_HAIR,
        model_params: edited_model(),
    })[6..]
        .to_vec();
    for frames in [w.a.drain(), w.b.drain()] {
        let receipts: Vec<_> = frames
            .iter()
            .filter(|(op, _)| *op == sc::SCCharacterGenderAndModelModifiedPacket)
            .map(|(_, body)| body)
            .collect();
        assert_eq!(receipts, vec![&expected]);
        assert!(!frames
            .iter()
            .any(|(op, body)| *op == sc::SCItemTaskSuccessPacket
                && body.first() == Some(&item_task::EDIT_COSMETIC)));
    }
}
