//! The shared doodad item gate for the funcs the 1.2 client does not pre-check: `RemoveItem`,
//! `BuyFish` and `ConvertFish` (the `ConsumeChanger` packet flow is in
//! `skills::doodad_item_gate_tests`).

use std::sync::Arc;

use crate::data::items::{ItemTemplate, TemplateKind};
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::Item;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::npcs::doodad::manager;
use crate::systems::npcs::doodad::templates::*;
use crate::test_support::*;
use crate::world::{DoodadRef, Entity, PlayerRef, WorldObject};

use super::skill_item_refusal;

const PLAYER_OBJ: u32 = 0x100;
const DOODAD: u32 = 9000;
const DOODAD_OBJ: u32 = 0x200;
const GROUP: u32 = 9001;
const BACKPACK: u32 = 9002;
const REQUIRED: u32 = 9003;

struct Fixture {
    server: Arc<GameServer>,
    player: PlayerRef,
    doodad: DoodadRef,
    skill_id: u32,
}

fn fixture(func_type: &'static str, template: FuncTemplate, skill_id: u32) -> Fixture {
    let mut data = crate::systems::economy::tests::test_data();
    data.items.templates.insert(
        BACKPACK,
        ItemTemplate {
            kind: TemplateKind::Backpack { backpack_type: 1 },
            ..ItemTemplate::plain(BACKPACK, 1)
        },
    );
    data.items
        .templates
        .insert(REQUIRED, ItemTemplate::plain(REQUIRED, 10));
    let doodads = &mut data.systems.npcs.doodad;
    doodads.templates.insert(
        DOODAD,
        DoodadTemplate {
            id: DOODAD,
            func_groups: vec![DoodadFuncGroups {
                id: GROUP,
                almighty: DOODAD,
                group_kind_id: doodad_func_group_kind::START,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let func_id = template.id();
    doodads.funcs_by_groups.insert(
        GROUP,
        vec![DoodadFunc {
            group_id: GROUP,
            func_id,
            func_key: 1,
            func_type: func_type.into(),
            next_phase: -1,
            skill_id,
            ..Default::default()
        }],
    );
    doodads
        .func_templates
        .entry(func_type)
        .or_default()
        .insert(func_id, template);
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::npcs::bridges::install(&server);
    let client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Tester", 100.0, 100.0),
        PLAYER_OBJ,
    );
    let instance = server.world.world(0).unwrap();
    let doodad =
        manager::create(&server, &instance, DOODAD_OBJ, DOODAD, None, true).expect("doodad");
    instance.spawn(&doodad, &server.view_ctx());
    Fixture {
        server,
        player,
        doodad,
        skill_id,
    }
}

impl Fixture {
    fn refusal(&self) -> Option<super::SkillItemRefusal> {
        let skill = crate::systems::skills::tests::skill_template(self.skill_id);
        skill_item_refusal(
            &self.server,
            &self.doodad,
            &skill,
            &WorldObject::from(&self.player),
        )
    }

    fn give(&self, template_id: u32, st: u8, slot: i32) {
        let template = self.server.game_data.items.template(template_id).unwrap();
        let mut item = Item::create(
            u64::from(PLAYER_OBJ) * 100 + template_id as u64,
            template,
            1,
            0,
            1,
            &self.server.game_data.items,
            0,
        );
        item.owner_id = u64::from(self.player.character_id);
        let container = Inv::of(&self.player).get(st).unwrap();
        container
            .lock()
            .unwrap()
            .add_new_item(item, slot, &self.server.game_data.items)
            .unwrap();
        self.server.items.add_container(container);
    }
}

/// A `RemoveItem` func needs the item in the bag already at the cast start.
#[tokio::test]
async fn remove_item_without_the_item_is_refused() {
    let f = fixture(
        "DoodadFuncRemoveItem",
        FuncTemplate::RemoveItem(DoodadFuncRemoveItem {
            id: 1,
            item_id: REQUIRED,
            count: 2,
        }),
        17452,
    );
    let refusal = f.refusal().expect("refused");
    assert_eq!(
        refusal.result,
        crate::systems::skills::types::skill_result::NEED_REAGENT
    );
    assert_eq!(refusal.value, REQUIRED);
    assert_eq!(refusal.error, None);
}

#[tokio::test]
async fn remove_item_with_the_item_is_allowed() {
    let f = fixture(
        "DoodadFuncRemoveItem",
        FuncTemplate::RemoveItem(DoodadFuncRemoveItem {
            id: 1,
            item_id: REQUIRED,
            count: 1,
        }),
        17452,
    );
    f.give(REQUIRED, slot_type::INVENTORY, 0);
    assert_eq!(f.refusal(), None);
}

/// `BuyFish` / `ConvertFish` need an equipped backpack (the fish pack) before the cast.
#[tokio::test]
async fn buy_fish_without_a_backpack_is_refused() {
    let f = fixture(
        "DoodadFuncBuyFish",
        FuncTemplate::BuyFish(DoodadFuncBuyFish {
            id: 1,
            item_id: REQUIRED,
        }),
        21904,
    );
    let refusal = f.refusal().expect("refused");
    assert_eq!(
        refusal.result,
        crate::systems::skills::types::skill_result::NEED_REAGENT
    );
    assert_eq!(refusal.value, REQUIRED);
    assert_eq!(
        refusal.error,
        Some(crate::systems::npcs::doodad::funcs::error_message::STORE_BACKPACK_NOGOODS)
    );
}

#[tokio::test]
async fn convert_fish_with_an_equipped_backpack_is_allowed() {
    let f = fixture(
        "DoodadFuncConvertFish",
        FuncTemplate::ConvertFish(DoodadFuncConvertFish { id: 1 }),
        22201,
    );
    assert!(f.refusal().is_some());
    f.give(BACKPACK, slot_type::EQUIPMENT, equip_slot::BACKPACK);
    assert_eq!(f.refusal(), None);
}

/// The fixture ids above are the shipped 1.2 rows: the storage func 7481, the eco-fuel mount
/// chain 60 -> 190 -> 20291 / 20292, and the two accepted fuel items.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
async fn real_compact_db_ids_match_the_gate_fixtures() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let slot: (u32, String, u32) = conn
        .query_row(
            "SELECT id, dos.doodad_func_group_id, slot_id FROM doodad_func_consume_changers dc              JOIN doodad_funcs dos ON dos.actual_func_id = dc.id              WHERE dos.id = 7481 AND dos.actual_func_type = 'DoodadFuncConsumeChanger'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(slot, (2, "8264".to_string(), 26));
    let next: (i32, u32) = conn
        .query_row(
            "SELECT next_phase, func_skill_id FROM doodad_funcs WHERE id = 7481",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(next, (8265, 15307));
    let pack: u32 = conn
        .query_row(
            "SELECT item_id FROM doodad_func_consume_changer_items              WHERE doodad_func_consume_changer_id = 2 AND item_id = 18469",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pack, 18469);
    let mount: u32 = conn
        .query_row(
            "SELECT ms.skill_id FROM slave_mount_skills sms              JOIN mount_skills ms ON ms.id = sms.mount_skill_id WHERE sms.slave_id = 60",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mount, 20291);
    let rider: u32 = conn
        .query_row(
            "SELECT skill_id FROM mount_attached_skills              WHERE mount_skill_id = 190 AND attach_point_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rider, 20292);
    let fuels: Vec<u32> = conn
        .prepare("SELECT item_id FROM skill_reagents WHERE skill_id = 20292 ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fuels, vec![34583, 26548]);
    // The same shape on a ship bar: row 345 -> mount skill 22287, rider 25632, 10 fuels.
    let ship: (u32, u32, u32) = conn
        .query_row(
            "SELECT mount_skill_id, attach_point_id, skill_id FROM mount_attached_skills              WHERE id = 345",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(ship, (345, 1, 25632));
    let ship_mount: u32 = conn
        .query_row(
            "SELECT skill_id FROM mount_skills WHERE id = 345",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ship_mount, 22287);
    let ship_fuel: Vec<(u32, i16)> = conn
        .prepare("SELECT item_id, amount FROM skill_reagents WHERE skill_id = 25632 ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ship_fuel, vec![(34583, 10), (26548, 10)]);
    let with_reagents: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT mas.id) FROM mount_attached_skills mas              JOIN skill_reagents r ON r.skill_id = mas.skill_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        with_reagents >= 45,
        "only {with_reagents} rider skills with reagents"
    );
    let link: (i32, u32) = conn
        .query_row(
            "SELECT link_equip_slot_id, link_backpack_type_id FROM skills WHERE id = 15307",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(link, (-1, 1));
}
/// An NPC caster is not a character: the item gate leaves it alone.
#[tokio::test]
async fn npc_caster_is_not_gated() {
    let f = fixture(
        "DoodadFuncConsumeChanger",
        FuncTemplate::ConsumeChanger(DoodadFuncConsumeChanger {
            id: 1,
            slot_id: equip_slot::BACKPACK as u32,
            count: 1,
        }),
        15307,
    );
    let npc = Entity::new(0x400, Npc::new(1));
    let skill = crate::systems::skills::tests::skill_template(15307);
    assert_eq!(
        skill_item_refusal(&f.server, &f.doodad, &skill, &WorldObject::from(&npc)),
        None
    );
}
