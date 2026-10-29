//! Quest 44 "Temporary Measure" on the real 1.2 data. The quest hands out the Lizard Stun Hammer
//! (item 5265, skill 11537) and Afindelle's Vial (item 5263, skill 11536); the hammer puts buff
//! 579 "Stunned Lizard" (8 s) on a Lilyut Crested Lizard (NPC 668), the vial draws blood only
//! from a lizard that carries buff 579 (quest objective `QuestActObjItemGather` 297, 5 bloods).
//! When the stun times out the lizard gets buff 580 "Angry Lizard" (30 s), which is immune to the
//! buffs tagged 124 (buff 579): "Won't stun angered lizards" is data, not code.
//!
//! The tests run the client path (`CSStartSkillPacket` with an item caster) against the real
//! tables. Gated by `AAEMU_COMPACT_DB` (path to `compact.sqlite3`):
//! `AAEMU_COMPACT_DB=... cargo test -p aaemu-game lizard_stun -- --ignored`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::data::client_files::ClientFileManager;
use crate::data::GameData;
use crate::models::unit::{Bonus, BonusTemplate, UnitAttribute, UnitModifierType};
use crate::opcodes::sc;
use crate::systems::economy::inventory::Inv;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{NpcRef, PlayerRef, WorldObject};

use super::handlers::{start_skill, CSStartSkillPacket};
use super::types::{SkillCastTarget, SkillCaster, SkillObject};

const QUEST: u32 = 44;
const LIZARD_NPC: u32 = 668;
const STUN_HAMMER: u32 = 5265;
const STUN_SKILL: u32 = 11537;
const STUNNED_LIZARD_BUFF: u32 = 579;
const ANGRY_LIZARD_BUFF: u32 = 580;
const BLOOD_VIAL: u32 = 5263;
const BLOOD_SKILL: u32 = 11536;
const BLOOD: u32 = 5264;
const LIZARD_OBJ: u32 = 0x200;

/// The tables the quest needs, loaded from the real database.
fn real_data() -> Option<GameData> {
    let path = std::env::var("AAEMU_COMPACT_DB").ok()?;
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("compact db");
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    let files = ClientFileManager::from_sources(&[], &app);
    let localization = crate::data::Localization::load(&conn, "en_us").unwrap();
    let mut data = GameData::empty();
    data.experience = crate::test_support::test_experience(55);
    data.items = crate::data::items::ItemData::load(&conn).unwrap();
    data.factions = crate::data::FactionData::load(&conn, &localization).unwrap();
    data.skills = crate::data::skills::SkillData::load(&conn).unwrap();
    data.formulas = crate::data::formulas::FormulaManager::load(&conn).unwrap();
    data.systems = crate::systems::SystemsData::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: &app,
        files: &files,
        language: "en_us",
    })
    .unwrap();
    Some(data)
}

struct Fx {
    server: Arc<crate::GameServer>,
    client: TestClient,
    player: PlayerRef,
    lizard: NpcRef,
}

/// A level 15 player with quest 44 started and its two quest items in the bag, next to a lizard.
/// The player's melee accuracy is pushed to 100 %: the stun is an ordinary melee-typed skill
/// whose miss roll (about 4 % at level 15) is not what these tests are about.
fn fixture(data: GameData) -> Fx {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::social::install_hooks(&server);
    crate::systems::quests::install_hooks(&server);
    crate::systems::economy::wiring::install(&server);
    crate::systems::economy::bridges::install(&server);
    let mut client = TestClient::new(&server, 1, 1001);
    let mut c = test_character(1, "Lizardhunter", 100.0, 100.0);
    c.unit.level = 15;
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    c.unit.mp = 1000;
    c.unit.max_mp = 1000;
    c.skill_char.ignore_skill_cooldowns = true;
    c.unit.bonuses.add_bonus(
        1,
        Bonus {
            template: BonusTemplate {
                attribute: UnitAttribute::MeleeAntiMiss,
                modifier_type: UnitModifierType::Value,
                value: 1_000_000,
                linear_level_bonus: 0,
            },
            value: 1_000_000,
        },
    );
    let player = spawn_player(&server, &client, c, 0x100);
    client.conn.active_char = Some(player.clone());
    server.items.add_container(Inv::of(&player).bag());
    let started = crate::systems::quests::events::add_quest(&server, &player, QUEST, true);
    assert_eq!(started, Some(true), "quest 44 must start");
    for template in [BLOOD_VIAL, STUN_HAMMER] {
        let item = server
            .items
            .create(
                template,
                1,
                0,
                1,
                &server.game_data.items,
                &server.ids.item,
                0,
            )
            .expect("quest item template");
        Inv::of(&player)
            .bag()
            .lock()
            .unwrap()
            .add_new_item(item, 0, &server.game_data.items)
            .expect("bag add");
    }
    let lizard = crate::systems::npcs::npc::manager::create(&server, LIZARD_OBJ, LIZARD_NPC)
        .expect("npc 668 template");
    {
        let mut n = lizard.data();
        n.unit.transform.set_xyz(100.0, 103.0, 100.0);
        n.unit.hp = 1000;
        n.unit.max_hp = 1000;
    }
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&lizard, &server.view_ctx());
    client.drain();
    Fx {
        server,
        client,
        player,
        lizard,
    }
}

impl Fx {
    /// The id of the stack of quest item `template` in the player's bag.
    fn bag_item(&self, template: u32) -> u64 {
        let bag = Inv::of(&self.player).bag();
        let bag = bag.lock().unwrap();
        let (ids, count) = bag.items_by_template(template, -1);
        assert!(count > 0, "item {template} must be in the bag");
        ids[0]
    }

    /// The client's cast of an item skill on the lizard (`CSStartSkillPacket`, item caster).
    fn use_item(&mut self, template: u32, skill: u32) {
        let item_id = self.bag_item(template);
        self.player.data().unit.skill_last_used_ms = 0;
        self.player.data().unit.global_cooldown_ms = 0;
        start_skill(
            &self.server,
            &mut self.client.conn,
            CSStartSkillPacket {
                skill_id: skill,
                caster: SkillCaster::Item {
                    obj_id: self.player.obj_id,
                    item_id,
                    item_template_id: template,
                    type1: 0,
                    type2: 0,
                },
                target: SkillCastTarget::Unit(LIZARD_OBJ),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    }

    fn lizard_has_buff(&self, buff: u32) -> bool {
        WorldObject::from(&self.lizard).with_base_unit(|b| b.buffs().check_buff(buff))
    }

    /// How many lizard bloods (quest objective item 5264) the player holds.
    fn blood(&self) -> i32 {
        let bag = Inv::of(&self.player).bag();
        let bag = bag.lock().unwrap();
        bag.items_by_template(BLOOD, -1).1
    }
}

/// `SCBuffCreatedPacket` owner and buff id (`SkillCaster`, `u32` caster, `bc` owner, `u32`
/// index, `u32` buff).
fn buff_created(body: &[u8]) -> (u32, u32) {
    let mut s = aaemu_commons::PacketStream::from_bytes(body.to_vec());
    let kind = s.read_u8().unwrap();
    SkillCaster::read(kind, &mut s).unwrap();
    s.read_u32().unwrap();
    let owner = s.read_bc().unwrap();
    s.read_u32().unwrap();
    (owner, s.read_u32().unwrap())
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_44_hammer_stuns_the_lizard_and_the_vial_draws_its_blood() {
    let Some(data) = real_data() else {
        return;
    };
    let mut fx = fixture(data);
    // The vial only works on a stunned lizard: refused before the stun.
    fx.use_item(BLOOD_VIAL, BLOOD_SKILL);
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(fx.blood(), 0, "blood from an unstunned lizard");
    fx.client.drain();

    fx.use_item(STUN_HAMMER, STUN_SKILL);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        fx.lizard_has_buff(STUNNED_LIZARD_BUFF),
        "the hammer must put buff 579 on the lizard"
    );
    let told: Vec<(u32, u32)> = fx
        .client
        .drain()
        .into_iter()
        .filter(|(op, _)| *op == sc::SCBuffCreatedPacket)
        .map(|(_, body)| buff_created(&body))
        .collect();
    assert!(
        told.contains(&(LIZARD_OBJ, STUNNED_LIZARD_BUFF)),
        "the client must be told about the stun: {told:?}"
    );
    // The hammer stays in the bag: one quest hammer serves every lizard.
    assert!(fx.bag_item(STUN_HAMMER) > 0);

    fx.use_item(BLOOD_VIAL, BLOOD_SKILL);
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(
        fx.blood(),
        1,
        "the vial must draw blood from the stunned lizard"
    );
}

#[tokio::test(start_paused = true)]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn angry_lizard_cannot_be_stunned_until_the_anger_ends() {
    let Some(data) = real_data() else {
        return;
    };
    let mut fx = fixture(data);
    fx.use_item(STUN_HAMMER, STUN_SKILL);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(fx.lizard_has_buff(STUNNED_LIZARD_BUFF));

    // 8 s after it started the stun times out and leaves the lizard angry for 30 s.
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(!fx.lizard_has_buff(STUNNED_LIZARD_BUFF));
    assert!(fx.lizard_has_buff(ANGRY_LIZARD_BUFF));

    // "Won't stun angered lizards": the hammer cast is accepted and the buff is refused.
    fx.use_item(STUN_HAMMER, STUN_SKILL);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !fx.lizard_has_buff(STUNNED_LIZARD_BUFF),
        "an angry lizard is immune to the stun"
    );
    assert!(fx.lizard_has_buff(ANGRY_LIZARD_BUFF));

    // Once the anger has worn off the lizard can be stunned again.
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert!(!fx.lizard_has_buff(ANGRY_LIZARD_BUFF));
    fx.use_item(STUN_HAMMER, STUN_SKILL);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(fx.lizard_has_buff(STUNNED_LIZARD_BUFF));
}
