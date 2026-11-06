//! Salon packet flows through the real economy bridge (no MySQL).

use std::sync::Arc;

use super::*;
use crate::data::character_templates::{CharacterData, CharacterTemplate};
use crate::data::items::{BodyPartTemplate, ItemData, ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::container::slot_type;
use crate::models::unit_model::UnitCustomModelType;
use crate::systems::economy::bridges::EconomyForServices;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::tests::{give, test_data, World};
use crate::systems::services::item_ops::CharacterItemOps;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

const OLD_HAIR: u32 = 24127;
const NEW_HAIR: u32 = 24128;
/// A hair for model 11 (the fixture character is model 10).
const OTHER_MODEL_HAIR: u32 = 24129;
/// A hair flagged `npc_only` for the character's own model.
const NPC_ONLY_HAIR: u32 = 24130;

fn hair_part(item_id: u32, model_id: u32, npc_only: bool) -> BodyPartTemplate {
    BodyPartTemplate {
        item_id,
        model_id,
        npc_only,
        slot_type_id: 24, // EquipmentItemSlotType.Hair
        beauty_shop_only: true,
    }
}

fn fixture_data() -> GameData {
    let mut data = test_data();
    for (id, model_id, npc_only) in [
        (OLD_HAIR, 10, false),
        (NEW_HAIR, 10, false),
        (OTHER_MODEL_HAIR, 11, false),
        (NPC_ONLY_HAIR, 10, true),
    ] {
        data.items.templates.insert(
            id,
            ItemTemplate {
                kind: TemplateKind::BodyPart {
                    slot_type_id: 24,
                    model_id,
                },
                ..ItemTemplate::plain(id, 1)
            },
        );
        data.items
            .body_parts
            .insert(id, hair_part(id, model_id, npc_only));
    }
    data.items.templates.insert(
        SALON_CERTIFICATE,
        ItemTemplate::plain(SALON_CERTIFICATE, 20),
    );
    data.characters.templates.insert(
        17, // 16 * gender + race for (race 1, gender 1)
        CharacterTemplate {
            race: 1,
            gender: 1,
            model_id: 10,
            ..CharacterTemplate::default()
        },
    );
    data
}

fn fixture(old_hair: Option<u32>, certificates: i32) -> World {
    fixture_with_data(fixture_data(), old_hair, certificates)
}

fn fixture_with_data(data: GameData, old_hair: Option<u32>, certificates: i32) -> World {
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

/// The error body of a refused salon request.
fn expected_refusal() -> Vec<u8> {
    let mut s = PacketStream::new();
    SCErrorMsgPacket::new(error_message::FAILED_TO_CHANGE_ITEM_LOOK, 0, false).write(&mut s);
    s.into_bytes()
}

/// A refused request changes no appearance, hair item, certificate, or money, and sends the
/// owner exactly one error. Observers see nothing.
async fn refused(w: &mut World, hair: u32) {
    let model = w.pa.data().unit.model_params.clone();
    let equipped = Inv::of(&w.pa).equipped(equip_slot::HAIR);
    let certificates = EconomyForServices.items_count(&w.server, &w.pa, SALON_CERTIFICATE);
    let money = w.pa.data().money;
    apply(w, hair).await;
    assert_eq!(w.pa.data().unit.model_params, model, "appearance changed");
    assert_eq!(
        Inv::of(&w.pa).equipped(equip_slot::HAIR),
        equipped,
        "hair item changed"
    );
    assert_eq!(
        EconomyForServices.items_count(&w.server, &w.pa, SALON_CERTIFICATE),
        certificates,
        "certificate consumed"
    );
    assert_eq!(w.pa.data().money, money, "money changed");
    assert_eq!(
        w.a.drain(),
        vec![(sc::SCErrorMsgPacket, expected_refusal())],
        "one salon refusal error"
    );
    assert!(w.b.drain().is_empty(), "observers see nothing");
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
async fn arbitrary_item_template_is_refused_without_changes() {
    let mut w = fixture(Some(OLD_HAIR), 2);
    refused(&mut w, SALON_CERTIFICATE).await;
}

#[tokio::test]
async fn npc_only_hair_is_refused_without_changes() {
    let mut w = fixture(Some(OLD_HAIR), 2);
    refused(&mut w, NPC_ONLY_HAIR).await;
}

#[tokio::test]
async fn hair_of_another_model_is_refused_without_changes() {
    let mut w = fixture(Some(OLD_HAIR), 2);
    refused(&mut w, OTHER_MODEL_HAIR).await;
}

#[tokio::test]
async fn no_certificate_sends_the_refusal_error_and_changes_nothing() {
    for old_hair in [None, Some(OLD_HAIR)] {
        let mut w = fixture(old_hair, 0);
        refused(&mut w, NEW_HAIR).await;
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

/// The shipped `item_body_parts` data decides: race 1 gender 1 is model 10, item 25355 is a
/// playable model-10 hair, 4085 is `npc_only`, 24024 is the model-11 (female) hair, and 1287
/// is a weapon that has no `item_body_parts` row.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
async fn real_compact_db_accepts_job_hair_and_refuses_other_templates() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut data = test_data();
    data.items = ItemData::load(&conn).unwrap();
    data.characters = CharacterData::load(
        &conn,
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app"),
        |_, _| 0,
        0,
    )
    .unwrap();
    let mut w = fixture_with_data(data, None, 2);
    apply(&mut w, 25355).await;
    assert_eq!(hair_template(&w.pa), Some(25355));
    assert_eq!(
        EconomyForServices.items_count(&w.server, &w.pa, SALON_CERTIFICATE),
        1
    );
    w.a.drain();
    w.b.drain();
    for wrong in [4085, 24024, 1287] {
        refused(&mut w, wrong).await;
    }
    assert_eq!(hair_template(&w.pa), Some(25355));
}
