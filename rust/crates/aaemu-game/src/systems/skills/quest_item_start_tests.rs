//! Quest-starter items (`QuestActConAcceptItem`) end to end through the client cast path.
//!
//! The 1.2 client uses a quest-starter item with `CSStartSkill` (skill `items.use_skill_id`,
//! `SkillCaster::Item`); skill 11141's `AcceptQuestEffect` has to start the quest the item is
//! bound to (`item_accept_quests`). The fixture test uses the real ids (item 1459, skill 11141,
//! effect 52, quest 72) and the real `QuestActConAcceptItem` start act; the ignored test runs all
//! five starter items on `compact.sqlite3`:
//! `AAEMU_COMPACT_DB=... cargo test -p aaemu-game quest_item_start -- --ignored`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sqlx::mysql::MySqlPoolOptions;

use crate::config::AppConfig;
use crate::data::client_files::ClientFileManager;
use crate::data::items::{bind_type, item_impl, ItemTemplate};
use crate::data::skills::{BuffKind, BuffTemplate};
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::opcodes::sc;
use crate::systems::economy::inventory::Inv;
use crate::systems::quests::data::QuestData;
use crate::systems::quests::model::kind;
use crate::systems::quests::templates::{ActKind, QuestComponentTemplate, QuestTemplate};
use crate::systems::skills::data::{EffectKind, EffectTemplate, SkillEffect, SkillTemplate};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::unit_state::UNIT_STATE_FORCED_BUFFS;
use crate::world::PlayerRef;
use crate::GameServer;

use super::handlers::{start_skill, CSStartSkillPacket};
use super::types::{SkillCastTarget, SkillCaster, SkillObject};

const STARTER_SKILL: u32 = 11141;
const STARTER_ITEM: u32 = 1459;
const STARTER_QUEST: u32 = 72;
/// `accept_quest_effects` 52 (`effects` row 6750 of skill 11141).
const ACCEPT_QUEST_EFFECT: u32 = 52;
const ACCEPT_QUEST_EFFECT_QUEST: u32 = 28;

/// The five real starter items, the quest their `AcceptQuestEffect` starts and the character
/// faction to play it with: children of the mother faction the start component requires
/// (`unit_reqs` kind 0x2A), 101 -> 148 and 108 -> 149.
const STARTERS: [(u32, u32, u32); 5] = [
    (1459, 72, 101),
    (14739, 1587, 101),
    (14709, 1596, 101),
    (14784, 1607, 101),
    (25781, 3303, 108),
];

/// The tables the cast needs, loaded from the real database.
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

/// A server on the real tables with the subsystem hooks installed like `systems::start_all`.
fn real_server(mut data: GameData) -> Arc<GameServer> {
    let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
    data.worlds = WorldTemplates::from_templates(
        vec![WorldTemplate::synthetic(0, "main_world", 2, 2, 100)],
        Vec::new(),
    )
    .expect("world");
    for id in UNIT_STATE_FORCED_BUFFS {
        let mut t = BuffTemplate::simple(id, BuffKind::Good);
        t.duration = 0;
        data.skills.buffs.insert(id, t);
    }
    let mut config = AppConfig {
        id: 1,
        ..AppConfig::default()
    };
    config.app_path = app;
    config.connections.mysql_provider.host = "127.0.0.1".into();
    config.connections.mysql_provider.port = 1;
    let pool = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy_with(config.connections.mysql_provider.connect_options());
    let server = Arc::new(GameServer::new(config, pool, data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::social::install_hooks(&server);
    crate::systems::quests::install_hooks(&server);
    crate::systems::economy::wiring::install(&server);
    crate::systems::economy::bridges::install(&server);
    crate::systems::ai::hooks::install(&server);
    server
}

struct Fx {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
}

impl Fx {
    /// Adds one unit of the real starter item to the bag and returns its item id.
    fn give_starter(&self, template_id: u32) -> u64 {
        self.give(template_id, self.player.clone())
    }

    fn give(&self, template_id: u32, owner: PlayerRef) -> u64 {
        let item = self
            .server
            .items
            .create(
                template_id,
                1,
                0,
                1,
                &self.server.game_data.items,
                &self.server.ids.item,
                0,
            )
            .expect("starter item");
        let id = item.id;
        let bag = Inv::of(&owner).bag();
        let mut bag = bag.lock().unwrap();
        bag.add_new_item(item, -1, &self.server.game_data.items)
            .expect("bag space");
        id
    }

    /// Adds an item whose template is absent from the game data (a data gap): the handler's
    /// `item_use_skill` lookup then has no template to read the use skill from. The item is
    /// placed like the quest tests' `give_item`, because `ItemManager::create` needs the
    /// template the fixture deliberately does not register.
    fn give_unregistered_item(&self, template_id: u32) -> u64 {
        use crate::data::items::ItemData;
        use crate::models::container::slot_type;
        use crate::models::item::Item;

        let template = ItemTemplate {
            impl_id: item_impl::ACCEPT_QUEST,
            use_skill_id: STARTER_SKILL,
            ..ItemTemplate::plain(template_id, 1)
        };
        let id = u64::from(self.server.ids.item.get_next_id().expect("item id"));
        let mut item = Item::create(id, &template, 1, 0, 1, &ItemData::default(), 0);
        item.owner_id = u64::from(self.player.character_id);
        item.slot_type = slot_type::INVENTORY;
        let bag = Inv::of(&self.player).bag();
        let mut bag = bag.lock().unwrap();
        item.slot = bag.get_unused_slot(-1);
        bag.items.push(item);
        bag.update_free_slot_count();
        id
    }

    /// The client's item cast (`CSStartSkillPacket`, `SkillCaster::Item`, self as target).
    fn cast_starter(&mut self, skill_id: u32, item_id: u64, template_id: u32) {
        self.player.data().unit.skill_last_used_ms = 0;
        self.player.data().unit.global_cooldown_ms = 0;
        start_skill(
            &self.server,
            &mut self.client.conn,
            CSStartSkillPacket {
                skill_id,
                caster: SkillCaster::Item {
                    obj_id: self.player.obj_id,
                    item_id,
                    item_template_id: template_id,
                    type1: 0,
                    type2: 0,
                },
                target: SkillCastTarget::Unit(self.player.obj_id),
                flag: 0,
                skill_object: SkillObject::None,
            },
        );
    }
}

/// The hand-built fixture: real ids and the real data relations
/// (`items.impl_id = ACCEPT_QUEST`, `items.use_skill_id`, `item_accept_quests`,
/// `QuestActConAcceptItem` start act, `AcceptQuestEffect`).
fn fixture_data() -> GameData {
    let mut data = GameData::empty();
    let item = ItemTemplate {
        impl_id: item_impl::ACCEPT_QUEST,
        use_skill_id: STARTER_SKILL,
        bind_type: bind_type::BIND_ON_PICKUP,
        ..ItemTemplate::plain(STARTER_ITEM, 1)
    };
    data.items.templates.insert(STARTER_ITEM, item);

    let effect = SkillEffect {
        effect_id: 5601,
        template: Some((EffectKind::AcceptQuestEffect, ACCEPT_QUEST_EFFECT)),
        weight: 0,
        start_level: 1,
        end_level: 99,
        friendly: true,
        non_friendly: true,
        target_buff_tag_id: 0,
        target_no_buff_tag_id: 0,
        source_buff_tag_id: 0,
        source_no_buff_tag_id: 0,
        chance: 100,
        front: true,
        back: true,
        target_npc_tag_id: 0,
        application_method: 1,
        consume_source_item: false,
        consume_item_id: 0,
        consume_item_count: 1,
        always_hit: false,
        item_set_id: 0,
        interaction_success_hit: false,
    };
    data.systems.skills.add_effect(
        EffectKind::AcceptQuestEffect,
        EffectTemplate::AcceptQuest {
            id: ACCEPT_QUEST_EFFECT,
            quest_id: ACCEPT_QUEST_EFFECT_QUEST,
        },
        0,
    );
    data.systems.skills.add_skill(SkillTemplate {
        id: STARTER_SKILL,
        target_type: 0,
        target_selection: 1,
        max_range: 4,
        effects: vec![effect],
        ..SkillTemplate::default()
    });

    let mut quests = QuestData::default();
    quests.add_template(QuestTemplate {
        id: STARTER_QUEST,
        level: 10,
        ..QuestTemplate::default()
    });
    quests.add_component(QuestComponentTemplate {
        id: 830,
        quest_id: STARTER_QUEST,
        kind_id: kind::START,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    });
    quests.add_act(
        830,
        991,
        18,
        0,
        ActKind::ConAcceptItem {
            item_id: STARTER_ITEM,
            cleanup: true,
            drop_when_destroy: true,
            destroy_when_drop: false,
        },
    );
    // The real quest's Progress step gathers another item, so the started quest stays active.
    quests.add_component(QuestComponentTemplate {
        id: 11220,
        quest_id: STARTER_QUEST,
        kind_id: kind::PROGRESS,
        npc_ai_id: 1,
        ..QuestComponentTemplate::default()
    });
    quests.add_act(
        11220,
        16217,
        1060,
        1,
        ActKind::ObjItemGather {
            item_id: 9999,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
            use_alias: false,
            alias_id: 0,
            cleanup: false,
            drop_when_destroy: false,
            destroy_when_drop: false,
        },
    );
    // `item_accept_quests` 16: the item binds to the quest, not to the effect's quest 28.
    quests.starter_quests.insert(STARTER_ITEM, STARTER_QUEST);
    quests.finish();
    data.systems.quests = quests;
    data
}

fn fixture() -> Fx {
    fixture_for(1, |_| {})
}

/// The fixture on a character with `character_id`. The refusal log is throttled per character
/// and reason, so tests asserting a WARN use distinct character ids to stay independent of the
/// other tests in this process.
fn fixture_for(character_id: u32, build: impl FnOnce(&mut GameData)) -> Fx {
    let mut data = fixture_data();
    build(&mut data);
    let server = real_server(data);
    let mut client = TestClient::new(&server, 1, 1001);
    let mut c = test_character(character_id, "Starter", 100.0, 100.0);
    c.unit.level = 10;
    c.unit.faction_id = 101;
    let player = spawn_player(&server, &client, c, 0x100);
    client.conn.active_char = Some(player.clone());
    server.items.add_container(Inv::of(&player).bag());
    client.drain();
    Fx {
        server,
        client,
        player,
    }
}

// ----- refusal logging -----

/// Captures the formatted log lines of the current thread (fields included).
#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl LogCapture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Runs `f` and returns the WARN lines it logged.
fn warnings(f: impl FnOnce()) -> Vec<String> {
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(capture.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    capture
        .text()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

// ----- tests -----

#[tokio::test]
async fn starter_item_cast_starts_the_quest() {
    let mut fx = fixture();
    let item_id = fx.give_starter(STARTER_ITEM);
    fx.client.drain();
    assert!(!crate::systems::quests::events::has_quest(
        &fx.player,
        STARTER_QUEST
    ));

    fx.cast_starter(STARTER_SKILL, item_id, STARTER_ITEM);

    assert!(
        crate::systems::quests::events::has_quest(&fx.player, STARTER_QUEST),
        "the starter item must start quest {STARTER_QUEST}"
    );
    assert!(
        fx.client
            .drain_ops()
            .contains(&sc::SCQuestContextStartedPacket),
        "the client must get SCQuestContextStartedPacket"
    );
}

#[tokio::test]
async fn an_unowned_item_id_is_refused_with_the_reason() {
    let mut fx = fixture_for(2, |_| {});
    fx.client.drain();

    let lines = warnings(|| fx.cast_starter(STARTER_SKILL, 999_999, STARTER_ITEM));

    assert!(
        !crate::systems::quests::events::has_quest(&fx.player, STARTER_QUEST),
        "an unowned item id must not start a quest"
    );
    let line = lines
        .iter()
        .find(|l| l.contains("the item is not in the character's containers"))
        .unwrap_or_else(|| panic!("no refusal WARN, got {lines:?}"));
    assert!(
        line.contains("999999"),
        "the WARN must name the item id: {line}"
    );
    assert!(
        line.contains("wire_template_id=1459"),
        "the WARN must include the client's template hint: {line}"
    );
}

#[tokio::test]
async fn an_item_with_no_use_skill_is_refused_with_the_reason() {
    // `item_use_skill` returns None only when the owned item's template is missing from the
    // game data; an item template with `use_skill_id = 0` still returns `Some((0, ..))` and
    // takes the "skill is not the use skill" branch instead.
    let mut fx = fixture_for(3, |data| {
        data.items.templates.remove(&STARTER_ITEM);
    });
    let item_id = fx.give_unregistered_item(STARTER_ITEM);
    fx.client.drain();

    let lines = warnings(|| fx.cast_starter(STARTER_SKILL, item_id, STARTER_ITEM));

    assert!(
        !crate::systems::quests::events::has_quest(&fx.player, STARTER_QUEST),
        "an item whose template is not in the data must not start a quest"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("the item template has no use skill")
                && l.contains("item_template_id=1459")),
        "no refusal WARN, got {lines:?}"
    );
}

#[tokio::test]
async fn a_foreign_skill_is_refused_with_the_reason() {
    // A known skill that is not the item's use skill: the Item caster cannot run it.
    let mut fx = fixture_for(4, |data| {
        data.systems.skills.add_skill(SkillTemplate {
            id: 12345,
            target_type: 0,
            target_selection: 1,
            max_range: 4,
            ..SkillTemplate::default()
        });
    });
    let item_id = fx.give_starter(STARTER_ITEM);
    fx.client.drain();

    let lines = warnings(|| fx.cast_starter(12345, item_id, STARTER_ITEM));

    assert!(
        !crate::systems::quests::events::has_quest(&fx.player, STARTER_QUEST),
        "a foreign skill must not start the item's quest"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("the skill is not the item template's use skill")),
        "no refusal WARN, got {lines:?}"
    );
}

#[tokio::test]
async fn a_bogus_item_id_is_logged_once_per_character_and_reason() {
    let mut fx = fixture_for(5, |_| {});
    fx.client.drain();

    let lines = warnings(|| {
        for _ in 0..100 {
            fx.cast_starter(STARTER_SKILL, 999_999, STARTER_ITEM);
        }
    });

    let count = lines
        .iter()
        .filter(|l| l.contains("the item is not in the character's containers"))
        .count();
    assert_eq!(
        count, 1,
        "100 bogus item casts must log one WARN, got {lines:?}"
    );
}

/// Runs a `kill`-free character through the five real starter items on `compact.sqlite3`.
/// The character factions are children of the mother factions the start components require
/// (`unit_reqs` kind 0x2A), taken from the data: 101 (mother 148) and 108 (mother 149).
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn quest_starter_items_start_their_quests() {
    for (template_id, quest_id, faction_id) in STARTERS {
        let Some(data) = real_data() else {
            return;
        };
        let quests = &data.systems.quests;
        let item = data.items.template(template_id).expect("starter item");
        assert_eq!(item.impl_id, item_impl::ACCEPT_QUEST);
        assert_eq!(item.use_skill_id, STARTER_SKILL);
        assert_eq!(quests.quest_id_from_starter_item(template_id), quest_id);

        let server = real_server(data);
        let mut client = TestClient::new(&server, 1, 1001);
        let mut c = test_character(1, "Starter", 100.0, 100.0);
        c.unit.level = 55;
        c.unit.faction_id = faction_id;
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
        c.unit.mp = 1000;
        c.unit.max_mp = 1000;
        c.skill_char.ignore_skill_cooldowns = true;
        let player = spawn_player(&server, &client, c, 0x100);
        client.conn.active_char = Some(player.clone());
        server.items.add_container(Inv::of(&player).bag());
        client.drain();
        let mut fx = Fx {
            server,
            client,
            player,
        };
        let item_id = fx.give_starter(template_id);
        fx.client.drain();
        assert!(
            !crate::systems::quests::events::has_quest(&fx.player, quest_id),
            "quest {quest_id} must not be active before the cast"
        );

        fx.cast_starter(STARTER_SKILL, item_id, template_id);

        assert!(
            crate::systems::quests::events::has_quest(&fx.player, quest_id),
            "item {template_id} must start quest {quest_id}"
        );
        assert!(
            fx.client
                .drain_ops()
                .contains(&sc::SCQuestContextStartedPacket),
            "item {template_id} must send SCQuestContextStartedPacket for quest {quest_id}"
        );
    }
}
