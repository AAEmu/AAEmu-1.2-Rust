//! Normal client item-cast capability checks for the 1.2 UCC effects.

use std::sync::Arc;

use crate::data::items::{bind_type, ItemTemplate};
use crate::models::container::slot_type;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::tests::{give, item_at, test_data, World, ARMOR};
use crate::systems::skills::data::{EffectKind, EffectTemplate, SkillEffect, SkillTemplate};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};

use super::{CSStartSkillPacket, SkillCastTarget, SkillCaster, SkillObject};

const TORCH: u32 = 13758;
const NON_UCC_SKILL: u32 = 12345;

fn effect(kind: EffectKind, id: u32) -> SkillEffect {
    SkillEffect {
        effect_id: id,
        template: Some((kind, id)),
        weight: 0,
        start_level: 0,
        end_level: 255,
        friendly: true,
        non_friendly: true,
        target_buff_tag_id: 0,
        target_no_buff_tag_id: 0,
        source_buff_tag_id: 0,
        source_no_buff_tag_id: 0,
        chance: 100,
        front: false,
        back: false,
        target_npc_tag_id: 0,
        application_method: 1,
        consume_source_item: false,
        consume_item_id: 0,
        consume_item_count: 1,
        always_hit: true,
        item_set_id: 0,
        interaction_success_hit: false,
    }
}

fn fixture() -> World {
    let mut data = test_data();
    for (id, skill, reagent) in [
        (17662, 14433, true),
        (15175, 12698, false),
        (19780, 16046, true),
        (23705, 16046, true),
        (TORCH, 0, true),
    ] {
        let mut item = ItemTemplate::plain(id, 10);
        item.bind_type = bind_type::BIND_ON_PICKUP;
        item.use_skill_id = skill;
        item.use_skill_as_reagent = reagent;
        data.items.templates.insert(id, item);
    }
    for (skill, kind, template) in [
        (
            14433,
            EffectKind::ImprintUccEffect,
            EffectTemplate::ImprintUcc { id: 5, item_id: 0 },
        ),
        (
            12698,
            EffectKind::ImprintUccEffect,
            EffectTemplate::ImprintUcc { id: 4, item_id: 0 },
        ),
        (
            16046,
            EffectKind::CleanupUccEffect,
            EffectTemplate::CleanupUcc { id: 1 },
        ),
    ] {
        let id = template.id();
        data.systems.skills.add_effect(kind, template, 0);
        data.systems.skills.add_skill(SkillTemplate {
            id: skill,
            target_type: 9,
            target_selection: 2,
            max_range: 4,
            // Keep this packet/consumption fixture synchronous rather than waiting
            // for the authentic skills' cosmetic cast times.
            effects: vec![effect(kind, id)],
            ..SkillTemplate::default()
        });
    }
    data.systems.skills.add_skill(SkillTemplate {
        id: NON_UCC_SKILL,
        target_type: 9,
        max_range: 4,
        ..SkillTemplate::default()
    });
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::economy::bridges::install(&server);
    let mut a = TestClient::new(&server, 1, 1001);
    let mut b = TestClient::new(&server, 2, 1002);
    let pa = spawn_player(
        &server,
        &a,
        test_character(1, "Stamper", 100.0, 100.0),
        0x100,
    );
    let pb = spawn_player(&server, &b, test_character(2, "Other", 101.0, 100.0), 0x101);
    a.conn.active_char = Some(pa.clone());
    b.conn.active_char = Some(pb.clone());
    server.items.add_container(Inv::of(&pa).bag());
    server.items.add_container(Inv::of(&pb).bag());
    a.drain();
    b.drain();
    World {
        server,
        a,
        pa,
        b,
        pb,
    }
}

fn operands(w: &World, source_template: u32) -> (u64, u64) {
    let source = give(w, &w.pa, slot_type::INVENTORY, source_template, 2, 0);
    let target = give(w, &w.pa, slot_type::INVENTORY, ARMOR, 1, 1);
    let bag = Inv::of(&w.pa).bag();
    let mut bag = bag.lock().unwrap();
    bag.get_item_by_id_mut(source).unwrap().set_ucc(0xBEEF);
    bag.get_item_by_id_mut(target).unwrap().set_ucc(0x1234);
    (source, target)
}

async fn cast(w: &mut World, skill: u32, source: u64, source_template: u32, target: u64) {
    CSStartSkillPacket {
        skill_id: skill,
        caster: SkillCaster::Item {
            obj_id: w.pa.obj_id,
            item_id: source,
            item_template_id: source_template,
            type1: 0,
            type2: 0,
        },
        target: SkillCastTarget::Item {
            obj_id: w.pa.obj_id,
            id: target,
            type1: 0,
            type2: 0,
        },
        flag: 0,
        skill_object: SkillObject::None,
    }
    .handle(&w.server, &mut w.a.conn)
    .await
    .unwrap();
}

#[tokio::test]
async fn bind_on_pickup_torch_cannot_authorize_ucc_imprint_or_cleanup() {
    for skill in [14433, 12698, 16046] {
        let mut w = fixture();
        let (source, target) = operands(&w, TORCH);
        let before_source = item_at(&w.pa, slot_type::INVENTORY, 0).unwrap();
        let before_target = item_at(&w.pa, slot_type::INVENTORY, 1).unwrap();
        // The source id, ownership and client template id are all truthful. Only
        // its capability is wrong: this bound torch has use_skill_id=0.
        cast(&mut w, skill, source, TORCH, target).await;
        assert_eq!(
            item_at(&w.pa, slot_type::INVENTORY, 0).unwrap(),
            before_source
        );
        assert_eq!(
            item_at(&w.pa, slot_type::INVENTORY, 1).unwrap(),
            before_target
        );
        assert_eq!(w.pa.data().unit.skill_last_used_ms, 0);
        assert!(w.a.drain().is_empty());
        assert!(w.b.drain().is_empty());
    }
}

#[tokio::test]
async fn configured_imprint_and_both_bleaches_cast_and_consume_their_real_sources() {
    for (source_template, skill, cleanup, remaining) in [
        (17662, 14433, false, 1),
        (15175, 12698, false, 2),
        (19780, 16046, true, 1),
        (23705, 16046, true, 1),
    ] {
        let mut w = fixture();
        let (source, target) = operands(&w, source_template);
        cast(&mut w, skill, source, source_template, target).await;
        assert_eq!(
            item_at(&w.pa, slot_type::INVENTORY, 0).unwrap().count,
            remaining
        );
        assert_eq!(
            item_at(&w.pa, slot_type::INVENTORY, 1).unwrap().ucc_id,
            if cleanup { 0 } else { 0xBEEF }
        );
        assert!(w.pa.data().unit.skill_last_used_ms > 0);
        assert!(w.a.drain_ops().contains(&sc::SCItemUccDataChangedPacket));
        // Ordinary cast telemetry can be visible to the nearby player; the
        // private inventory/UCC notifications must remain with the owner.
        let other = w.b.drain_ops();
        assert!(!other.contains(&sc::SCItemUccDataChangedPacket));
        assert!(!other.contains(&sc::SCItemTaskSuccessPacket));
    }
}

#[tokio::test]
async fn bind_on_pickup_item_cannot_authorize_a_non_ucc_client_skill() {
    // The legacy C# exception was only for the house fireplace's doodad interaction. A BoP
    // item with no use skill cannot authorize an arbitrary client skill id.
    let mut w = fixture();
    let (source, target) = operands(&w, TORCH);
    let before_source = item_at(&w.pa, slot_type::INVENTORY, 0).unwrap();
    cast(&mut w, NON_UCC_SKILL, source, TORCH, target).await;
    assert_eq!(w.pa.data().unit.skill_last_used_ms, 0);
    assert_eq!(
        item_at(&w.pa, slot_type::INVENTORY, 0).unwrap(),
        before_source
    );
    assert!(w.a.drain().is_empty());
    assert!(w.b.drain().is_empty());
}

#[tokio::test]
async fn cancelled_ucc_cast_refunds_the_debited_source_item_exactly() {
    // A stackable reagent ink is taken before the effect runs; the effect then rejects a target
    // that belongs to another player and the skill cancels. The item must come back unchanged:
    // same id, slot, count, grade and UCC data.
    let mut w = fixture();
    let (source, _own_target) = operands(&w, 17662);
    let foreign = give(&w, &w.pb, slot_type::INVENTORY, ARMOR, 1, 1);
    let before = item_at(&w.pa, slot_type::INVENTORY, 0).unwrap();
    let before_state = (
        before.id,
        before.slot,
        before.slot_type,
        before.count,
        before.grade,
        before.ucc_id,
        before.dirty,
    );
    cast(&mut w, 14433, source, 17662, foreign).await;
    let after = item_at(&w.pa, slot_type::INVENTORY, 0).expect("the ink is back");
    assert_eq!(
        (
            after.id,
            after.slot,
            after.slot_type,
            after.count,
            after.grade,
            after.ucc_id,
            after.dirty,
        ),
        before_state
    );
    assert!(!w.a.drain_ops().contains(&sc::SCItemUccDataChangedPacket));
}

fn item_task_body(action: u8, tasks: Vec<crate::models::item_task::ItemTask>) -> Vec<u8> {
    use crate::systems::economy::item_packets::SCItemTaskSuccessPacket;
    use aaemu_commons::{Marshal, PacketStream};
    let mut s = PacketStream::new();
    SCItemTaskSuccessPacket {
        action,
        tasks,
        force_remove: Vec::new(),
    }
    .write(&mut s);
    s.into_bytes()
}

#[tokio::test]
async fn cancelled_full_ucc_cast_removes_then_restores_the_item_at_its_slot() {
    use crate::models::item_task::{task_type, ItemTask};

    let mut w = fixture();
    // One-unit source: the take removes the whole item, so the refund must re-add it.
    let source = give(&w, &w.pa, slot_type::INVENTORY, 17662, 1, 0);
    let foreign = give(&w, &w.pb, slot_type::INVENTORY, ARMOR, 1, 1);
    let before = item_at(&w.pa, slot_type::INVENTORY, 0).expect("ink");
    w.a.drain();
    cast(&mut w, 14433, source, 17662, foreign).await;
    let after = item_at(&w.pa, slot_type::INVENTORY, 0).expect("ink is back");
    assert_eq!(
        (
            after.id,
            after.slot,
            after.count,
            after.template_id,
            after.ucc_id
        ),
        (
            before.id,
            before.slot,
            before.count,
            before.template_id,
            before.ucc_id
        )
    );
    let bodies: Vec<Vec<u8>> =
        w.a.drain()
            .into_iter()
            .filter(|(op, _)| *op == sc::SCItemTaskSuccessPacket)
            .map(|(_, body)| body)
            .collect();
    assert_eq!(
        bodies,
        vec![
            item_task_body(
                task_type::SKILL_REAGENTS,
                vec![ItemTask::remove_slot(&before)]
            ),
            item_task_body(task_type::SKILL_REAGENTS, vec![ItemTask::add(&before)]),
        ],
        "the client removes the ink on the debit and gets it back on the refund"
    );
}
