//! Offline goldens of the three native appearance operations of the 1.2 client: `CSConvertItemLook`
//! (CS049, base + image item + 9 powder), Skinize (skill 23741, powder 29880) and Cleanup (skill 23457,
//! consumable 29704). The numbers are the real catalog's.
//!
//! The live counterparts are `rust/verify/look-*.vtest` (request bytes, wall-clock timing and database
//! rows against a running lane). These tests pin the same server behaviour without a stack, through the
//! real handlers: `start_skill` runs the whole cast (Started, effects, Fired, Ended) in one step when no
//! scheduler is bound, so every packet of a cast can be asserted in order and byte for byte.
//!
//! * The default tests run every scenario on synthetic rows built from the tables below, which carry
//!   exactly the catalog's numbers.
//! * `catalog_runs_every_scenario` (ignored, `AAEMU_COMPACT_DB=.../compact.sqlite3`) checks the same
//!   tables against the real catalog and runs the same scenarios on the real data.
//!
//! Everything asserted here is the server's CURRENT behaviour, known gaps included (the double
//! consumption of the source item, the partial powder count, the 15 powder of cloaks, gliders refused,
//! base == image destroying the item, item class not policed). None of it is a policy proposal.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use aaemu_commons::{Marshal, PacketStream};

use crate::data::items::{backpack_type, bind_type, EquipTemplate, ItemTemplate, TemplateKind};
use crate::data::GameData;
use crate::models::container::{equip_slot, slot_type};
use crate::models::item::{Item, ITEM_FLAG_SKINIZED};
use crate::models::item_task::{task_type, ItemTask};
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::economy::item_handlers::CSSwapItemsPacket;
use crate::systems::economy::store::CSConvertItemLookPacket;
use crate::systems::economy::store_data::ItemLookConvert;
use crate::systems::economy::tests::{item_at, task_body, test_data};
use crate::systems::skills::data::effects::SpecialEffect;
use crate::systems::skills::data::{EffectKind, EffectTemplate, SkillEffect, SkillTemplate};
use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
use crate::systems::skills::packets::SCSkillStartedPacket;
use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::PlayerRef;

const BASE: u32 = 26060; // one-handed sword, level 50, grade 3, bind on pickup
const DONOR: u32 = 26118; // one-handed sword, level 48, bind on pickup
const POWDER: u32 = 29880; // use skill 23741 (also its reagent), 9 per conversion
const CLEANER: u32 = 29704; // use skill 23457 (not a reagent; the effect row consumes it)
const NO_RECIPE: u32 = 6256; // armor of wearable slot type 4: no ItemLookConvert
const CLOAK: u32 = 14361; // armor of wearable slot type 9
const CHEST: u32 = 2691; // armor of wearable slot type 3
const GLIDER: u32 = 16385; // backpack, item_impl 22
const SKINIZE: u32 = 23741;
const CLEANUP: u32 = 23457;
/// `task_type::SKILL_REAGENTS`: the reason of the generic source / reagent consumption.
const REAGENTS: u8 = task_type::SKILL_REAGENTS; // 40
const INV: u8 = slot_type::INVENTORY;

// ---------------------------------------------------------------------------------------------
// The catalog rows (one table feeds the synthetic data and the real-catalog check)
// ---------------------------------------------------------------------------------------------

enum Kind {
    /// A weapon of this holdable id.
    Weapon(u32),
    /// An armor of this wearable slot type.
    Armor(u32),
    Glider,
    Plain,
}

struct ItemRow {
    id: u32,
    kind: Kind,
    bind: u32,
    max: i32,
    use_skill: u32,
    reagent: bool,
    level: i32,
    grade: i32,
    impl_id: i32,
}

const fn row(id: u32, kind: Kind, bind: u32, max: i32, impl_id: i32) -> ItemRow {
    ItemRow {
        id,
        kind,
        bind,
        max,
        use_skill: 0,
        reagent: false,
        level: 0,
        grade: -1,
        impl_id,
    }
}

const ITEMS: &[ItemRow] = &[
    ItemRow {
        level: 50,
        grade: 3,
        ..row(BASE, Kind::Weapon(1), bind_type::BIND_ON_PICKUP, 1, 1)
    },
    ItemRow {
        level: 48,
        ..row(DONOR, Kind::Weapon(6), bind_type::BIND_ON_PICKUP, 1, 1)
    },
    ItemRow {
        use_skill: SKINIZE,
        reagent: true,
        ..row(POWDER, Kind::Plain, bind_type::NORMAL, 100, 0)
    },
    ItemRow {
        use_skill: CLEANUP,
        ..row(CLEANER, Kind::Plain, bind_type::NORMAL, 100, 0)
    },
    row(NO_RECIPE, Kind::Armor(4), bind_type::NORMAL, 1, 2),
    ItemRow {
        // the catalog sets "use skill as reagent" on cloaks although they have no use skill
        reagent: true,
        ..row(CLOAK, Kind::Armor(9), bind_type::NORMAL, 1, 2)
    },
    row(CHEST, Kind::Armor(3), bind_type::NORMAL, 1, 2),
    ItemRow {
        use_skill: 19230,
        ..row(GLIDER, Kind::Glider, bind_type::BIND_ON_PICKUP, 1, 22)
    },
];

/// `item_look_convert_required_items`: (convert id, powder count).
const CONVERTS: &[(u32, i32)] = &[(1, 9), (19, 6), (25, 4), (27, 15)];
/// `item_look_convert_holdables`: (holdable id, convert id).
const HOLDABLE_CONVERTS: &[(u32, u32)] = &[(1, 1), (6, 1)];
/// `item_look_convert_wearables` as the server keeps them (first row per slot of the raw scan):
/// (wearable slot type, convert id). Slot 9 has two rows in the catalog, the server uses the
/// 15-powder one (convert 27); the native recipe group of cloaks is convert 25 (4 powder).
const WEARABLE_CONVERTS: &[(u32, u32)] = &[(3, 19), (9, 27)];

struct SkillRow {
    id: u32,
    casting: i32,
    cooldown: i32,
    max_range: i32,
    mana_level_md: f32,
    delayable: bool,
    effect_id: u32,
    special_id: u32,
    special_type: i32,
    consume_source: bool,
}

const SKILLS: &[SkillRow] = &[
    SkillRow {
        id: SKINIZE,
        casting: 5000,
        cooldown: 0,
        max_range: 4,
        mana_level_md: 1.0,
        delayable: true,
        effect_id: 34375,
        special_id: 14162,
        special_type: 108,
        consume_source: false,
    },
    SkillRow {
        id: CLEANUP,
        casting: 3000,
        cooldown: 1000,
        max_range: 15,
        mana_level_md: 0.0,
        delayable: false,
        effect_id: 33333,
        special_id: 13681,
        special_type: 103,
        consume_source: true,
    },
];

fn equip(slot_type_id: u32) -> EquipTemplate {
    EquipTemplate {
        slot_type_id,
        durability_ratio: 1.0,
        durability_multiplier: 100,
        ..EquipTemplate::default()
    }
}

/// Synthetic rows carrying the catalog numbers of the tables above.
fn fixture_data() -> GameData {
    let mut d = test_data();
    for r in ITEMS {
        let kind = match r.kind {
            // one-handed swords: holdable slot type 17 (main hand or off hand)
            Kind::Weapon(_) => TemplateKind::Weapon(equip(17)),
            Kind::Armor(slot) => TemplateKind::Armor(equip(slot)),
            Kind::Glider => TemplateKind::Backpack {
                backpack_type: backpack_type::GLIDER,
            },
            Kind::Plain => TemplateKind::Item,
        };
        d.items.templates.insert(
            r.id,
            ItemTemplate {
                bind_type: r.bind,
                max_count: r.max,
                use_skill_id: r.use_skill,
                use_skill_as_reagent: r.reagent,
                level_requirement: r.level,
                fixed_grade: r.grade,
                impl_id: r.impl_id,
                ..ItemTemplate::new(r.id, kind)
            },
        );
    }
    let store = &mut d.systems.economy.store;
    for r in ITEMS {
        match r.kind {
            Kind::Weapon(holdable) => {
                store.weapon_holdables.insert(r.id, holdable);
            }
            Kind::Armor(slot) => {
                store.armor_slot_types.insert(r.id, slot);
            }
            Kind::Glider | Kind::Plain => {}
        }
    }
    for &(id, count) in CONVERTS {
        store.item_look_converts.insert(
            id,
            ItemLookConvert {
                id,
                slot_id: 0,
                required_item_id: POWDER,
                required_item_count: count,
            },
        );
    }
    store
        .holdable_item_look_converts
        .extend(HOLDABLE_CONVERTS.iter().copied());
    store
        .wearable_item_look_converts
        .extend(WEARABLE_CONVERTS.iter().copied());
    for s in SKILLS {
        d.systems.skills.add_effect(
            EffectKind::SpecialEffect,
            EffectTemplate::Special(SpecialEffect {
                id: s.special_id,
                special_effect_type_id: s.special_type,
                value1: 0,
                value2: 0,
                value3: 0,
                value4: 0,
            }),
            s.effect_id,
        );
        d.systems.skills.add_skill(SkillTemplate {
            id: s.id,
            casting_time: s.casting,
            cooldown_time: s.cooldown,
            target_type: 9,
            target_selection: 1,
            max_range: s.max_range,
            mana_level_md: s.mana_level_md,
            casting_delayable: s.delayable,
            default_gcd: true,
            source_alive: true,
            target_alive: true,
            timing_id: 1,
            category_id: 1,
            level_step: 1,
            fire_anim_id: 100,
            combat_dice_id: 4,
            target_water: true,
            effects: vec![SkillEffect {
                effect_id: s.effect_id,
                template: Some((EffectKind::SpecialEffect, s.special_id)),
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
                consume_source_item: s.consume_source,
                consume_item_id: 0,
                consume_item_count: 1,
                always_hit: false,
                item_set_id: 0,
                interaction_success_hit: false,
            }],
            ..SkillTemplate::default()
        });
    }
    d
}

/// The real catalog's item and system tables (no formulas: casts cost no mana in these tests).
fn catalog_data(path: &str) -> GameData {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open compact.sqlite3");
    let app = std::path::Path::new(path)
        .parent()
        .and_then(std::path::Path::parent)
        .expect("<app>/Data/compact.sqlite3");
    let files = crate::data::client_files::ClientFileManager::from_sources(&[], app);
    let mut d = GameData::empty();
    d.items = crate::data::items::ItemData::load(&conn).expect("items");
    d.systems = crate::systems::SystemsData::load(&crate::systems::DataLoadCtx {
        conn: &conn,
        app_path: app,
        files: &files,
        language: "en_us",
    })
    .expect("systems");
    d
}

// ---------------------------------------------------------------------------------------------
// The environment: one server, any number of actors (every cast gets its own, so no global cooldown
// of an earlier cast is in the way)
// ---------------------------------------------------------------------------------------------

struct Env {
    server: Arc<GameServer>,
    next: AtomicU32,
}

struct Actor {
    c: TestClient,
    p: PlayerRef,
}

type Frame = (u16, Vec<u8>);

impl Env {
    fn with(data: GameData) -> Self {
        let server = Arc::new(test_server_with(data));
        server.set_self();
        // The economy side of the skills' item hooks; the skills scheduler stays unbound on
        // purpose, so a cast runs to its end inside `start_skill`.
        crate::systems::economy::bridges::install(&server);
        Self {
            server,
            next: AtomicU32::new(1),
        }
    }

    fn fixture() -> Self {
        Self::with(fixture_data())
    }

    fn actor(&self) -> Actor {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let mut c = TestClient::new(&self.server, n, 1000 + n);
        let p = spawn_player(
            &self.server,
            &c,
            test_character(n, &format!("Look{n}"), 100.0, 100.0),
            0x100 + n,
        );
        c.conn.active_char = Some(p.clone());
        self.server.items.add_container(Inv::of(&p).bag());
        {
            // both skills need a living source (`source_alive`), and a full pool pays any mana cost
            let mut d = p.data();
            d.unit.max_hp = 1000;
            d.unit.hp = 1000;
            d.unit.max_mp = 1000;
            d.unit.mp = 1000;
        }
        c.drain();
        Actor { c, p }
    }

    /// A new item of `template` x `count` at (`st`, `slot`) of the actor, no packets.
    fn give(&self, a: &Actor, st: u8, template: u32, count: i32, slot: i32) -> u64 {
        let item = self
            .server
            .items
            .create(
                template,
                count,
                0,
                1,
                &self.server.game_data.items,
                &self.server.ids.item,
                0,
            )
            .expect("item template");
        let id = item.id;
        let container = Inv::of(&a.p).get(st).expect("container");
        assert!(container
            .lock()
            .unwrap()
            .add_new_item(item, slot, &self.server.game_data.items)
            .is_ok());
        id
    }
}

impl Actor {
    fn at(&self, slot: u8) -> Option<Item> {
        item_at(&self.p, INV, slot)
    }

    fn item(&self, slot: u8) -> Item {
        self.at(slot).expect("item in the bag slot")
    }

    fn count(&self, slot: u8) -> i32 {
        self.at(slot).map_or(0, |i| i.count)
    }

    fn with_item(&self, id: u64, f: impl FnOnce(&mut Item)) {
        let bag = Inv::of(&self.p).bag();
        let mut bag = bag.lock().unwrap();
        f(bag.get_item_by_id_mut(id).expect("item"));
    }

    fn caster(&self, source: &Item) -> SkillCaster {
        SkillCaster::Item {
            obj_id: self.p.obj_id,
            item_id: source.id,
            item_template_id: source.template_id,
            type1: 0,
            type2: 0,
        }
    }

    fn target(&self, item_id: u64) -> SkillCastTarget {
        SkillCastTarget::Item {
            obj_id: self.p.obj_id,
            id: item_id,
            type1: 0,
            type2: 0,
        }
    }
}

/// `CSStartSkillPacket` of an item cast, handled up to the end of the cast; the actor's frames.
fn cast(
    env: &Env,
    a: &mut Actor,
    skill: u32,
    caster: SkillCaster,
    target: SkillCastTarget,
) -> Vec<Frame> {
    a.c.drain();
    start_skill(
        &env.server,
        &mut a.c.conn,
        CSStartSkillPacket {
            skill_id: skill,
            caster,
            target,
            flag: 0,
            skill_object: SkillObject::None,
        },
    );
    a.c.drain()
}

/// `CSConvertItemLookPacket(base, image)` through the packet reader and handler; the actor's frames.
fn convert(env: &Env, a: &mut Actor, base: u64, image: u64) -> Vec<Frame> {
    a.c.drain();
    let mut s = PacketStream::new();
    s.write_u64(base).write_u64(image);
    let p = CSConvertItemLookPacket::read(&mut PacketStream::from_bytes(s.into_bytes()))
        .expect("CS049 body is two u64");
    futures::executor::block_on(p.handle(&env.server, &mut a.c.conn)).expect("handle");
    a.c.drain()
}

// ---------------------------------------------------------------------------------------------
// Packet expectations
// ---------------------------------------------------------------------------------------------

/// What a flow is expected to send, in order. Started / Fired / Ended carry a timeline id that is
/// only checked for consistency, so their bodies are taken from the actual frames.
#[derive(Clone)]
enum Ex {
    Started,
    Fired,
    Ended,
    Task(u8, Vec<ItemTask>),
    Err(i16, u32),
}

const RELEVANT: [u16; 5] = [
    sc::SCSkillStartedPacket,
    sc::SCSkillFiredPacket,
    sc::SCSkillEndedPacket,
    sc::SCErrorMsgPacket,
    sc::SCItemTaskSuccessPacket,
];

fn relevant(frames: &[Frame]) -> Vec<Frame> {
    frames
        .iter()
        .filter(|(op, _)| RELEVANT.contains(op))
        .cloned()
        .collect()
}

fn err_frame(code: i16, param: u32) -> Frame {
    let mut s = PacketStream::new();
    SCErrorMsgPacket::new(code, param, true).write(&mut s);
    (sc::SCErrorMsgPacket, s.into_bytes())
}

fn describe(frames: &[Frame]) -> Vec<String> {
    frames
        .iter()
        .map(|(op, b)| match *op {
            sc::SCItemTaskSuccessPacket => format!(
                "task reason {} rows {} first action {:?}",
                b[0],
                b[1],
                b.get(2)
            ),
            sc::SCErrorMsgPacket => format!(
                "error {} param {}",
                i16::from_le_bytes([b[0], b[1]]),
                u32::from_le_bytes([b[4], b[5], b[6], b[7]])
            ),
            other => format!("op {other:#05x}"),
        })
        .collect()
}

fn assert_flow(what: &str, frames: &[Frame], seq: Vec<Ex>) {
    let got = relevant(frames);
    let want: Vec<Frame> = seq
        .into_iter()
        .enumerate()
        .map(|(i, e)| {
            let echo = |op: u16| {
                got.get(i)
                    .filter(|f| f.0 == op)
                    .map(|f| f.1.clone())
                    .unwrap_or_default()
            };
            match e {
                Ex::Started => (sc::SCSkillStartedPacket, echo(sc::SCSkillStartedPacket)),
                Ex::Fired => (sc::SCSkillFiredPacket, echo(sc::SCSkillFiredPacket)),
                Ex::Ended => (sc::SCSkillEndedPacket, echo(sc::SCSkillEndedPacket)),
                Ex::Task(reason, tasks) => (sc::SCItemTaskSuccessPacket, task_body(reason, tasks)),
                Ex::Err(code, param) => err_frame(code, param),
            }
        })
        .collect();
    assert_eq!(describe(&got), describe(&want), "{what}: packet sequence");
    assert_eq!(got, want, "{what}: packet bytes");
}

/// The SCSkillStarted frame equals the request echoed back with the catalog's cast time (the
/// timeline id is whatever the server allocated, but never 0).
fn assert_started(
    frames: &[Frame],
    skill: u32,
    caster: SkillCaster,
    target: SkillCastTarget,
    ms: u16,
) {
    let got = frames
        .iter()
        .find(|f| f.0 == sc::SCSkillStartedPacket)
        .expect("SCSkillStarted");
    let tl = u16::from_le_bytes([got.1[4], got.1[5]]);
    assert_ne!(tl, 0, "a running cast has a timeline id");
    let mut p = SCSkillStartedPacket::new(skill, tl, caster, target, SkillObject::None);
    p.base_cast_time_div10 = ms / 10;
    p.real_cast_time_div10 = ms / 10;
    let mut s = PacketStream::new();
    p.write(&mut s);
    assert_eq!(
        got.1,
        s.into_bytes(),
        "SCSkillStarted echo + cast time {ms} ms"
    );
}

/// The receipts of one consumed source item, in the order the server sends them (G7, the C#
/// behaviour): the effect takes one with `reason`, then the generic reagent step takes one more
/// with reason 40. A consumption that empties the stack is a Seize followed by an empty receipt.
fn double_consumption(reason: u8, n: i32, stack: &Item) -> Vec<Ex> {
    let take = |r: u8| Ex::Task(r, vec![ItemTask::count_update(stack, -1)]);
    let seize = |r: u8| Ex::Task(r, vec![ItemTask::remove_slot(stack)]);
    let empty = |r: u8| Ex::Task(r, vec![]);
    match n {
        1 => vec![seize(reason), empty(reason)],
        2 => vec![take(reason), seize(REAGENTS), empty(REAGENTS)],
        _ => vec![take(reason), take(REAGENTS)],
    }
}

// ---------------------------------------------------------------------------------------------
// Scenarios (each runs on the synthetic rows and, ignored, on the real catalog)
// ---------------------------------------------------------------------------------------------

mod scenarios {
    use super::*;

    /// The rows the other scenarios rely on, checked against whatever data the environment has.
    pub fn rows_match_the_pinned_numbers(env: &Env) {
        let data = &env.server.game_data;
        for r in ITEMS {
            let t = data
                .items
                .template(r.id)
                .unwrap_or_else(|| panic!("item {}", r.id));
            assert_eq!(t.bind_type, r.bind, "bind type of {}", r.id);
            assert_eq!(t.max_count, r.max, "stack size of {}", r.id);
            assert_eq!(t.use_skill_id, r.use_skill, "use skill of {}", r.id);
            assert_eq!(t.use_skill_as_reagent, r.reagent, "as reagent of {}", r.id);
            assert_eq!(
                t.level_requirement, r.level,
                "level requirement of {}",
                r.id
            );
            assert_eq!(t.fixed_grade, r.grade, "fixed grade of {}", r.id);
            assert_eq!(t.impl_id, r.impl_id, "item_impl of {}", r.id);
            match r.kind {
                Kind::Weapon(_) => {
                    assert!(t.is_equip() && matches!(t.kind, TemplateKind::Weapon(_)))
                }
                Kind::Armor(_) => assert!(t.is_equip() && matches!(t.kind, TemplateKind::Armor(_))),
                Kind::Glider => assert!(
                    !t.is_equip() && matches!(t.kind, TemplateKind::Backpack { .. }),
                    "a glider is a backpack, not equipment"
                ),
                Kind::Plain => assert!(!t.is_equip()),
            }
        }
        let store = &data.systems.economy.store;
        let recipe = |id: u32| {
            store
                .item_look_convert(id)
                .map(|c| (c.required_item_id, c.required_item_count))
        };
        assert_eq!(recipe(BASE), Some((POWDER, 9)));
        assert_eq!(recipe(DONOR), Some((POWDER, 9)));
        assert_eq!(recipe(CHEST), Some((POWDER, 6)));
        assert_eq!(
            recipe(CLOAK),
            Some((POWDER, 15)),
            "cloaks: the server's 15, not the native 4"
        );
        assert_eq!(
            recipe(NO_RECIPE),
            None,
            "wearable slot type 4 has no convert row"
        );
        assert_eq!(recipe(GLIDER), None);

        let skills = &data.systems.skills;
        for s in SKILLS {
            let t = skills
                .skill(s.id)
                .unwrap_or_else(|| panic!("skill {}", s.id));
            assert_eq!(t.casting_time, s.casting, "casting time of {}", s.id);
            assert_eq!(t.cooldown_time, s.cooldown, "cooldown of {}", s.id);
            assert_eq!(t.max_range, s.max_range, "range of {}", s.id);
            assert_eq!(
                t.mana_level_md, s.mana_level_md,
                "mana_level_md of {}",
                s.id
            );
            assert_eq!(t.target_type, 9, "an item target");
            assert!(t.default_gcd, "{} starts the global cooldown", s.id);
            assert_eq!(t.effects.len(), 1);
            let e = &t.effects[0];
            assert_eq!(e.effect_id, s.effect_id);
            assert_eq!(e.template, Some((EffectKind::SpecialEffect, s.special_id)));
            assert_eq!(
                e.consume_source_item, s.consume_source,
                "consume_source_item of {}",
                s.id
            );
            match skills.effect(EffectKind::SpecialEffect, s.special_id) {
                Some(EffectTemplate::Special(sp)) => {
                    assert_eq!(sp.special_effect_type_id, s.special_type)
                }
                other => panic!("special effect {}: {other:?}", s.special_id),
            }
        }
    }

    /// Skinize with a powder stack of 1 / 2 / 3: the flag bit, then the powder TWICE (G7).
    pub fn skinize_takes_the_powder_twice(env: &Env) {
        for n in 1..=3 {
            let mut a = env.actor();
            env.give(&a, INV, DONOR, 1, 0);
            env.give(&a, INV, POWDER, n, 1);
            let (donor, powder) = (a.item(0), a.item(1));
            assert_eq!(donor.flags & ITEM_FLAG_SKINIZED, 0);
            let (caster, target) = (a.caster(&powder), a.target(donor.id));
            let frames = cast(env, &mut a, SKINIZE, caster, target);
            let after = a.item(0);
            assert_eq!(after.flags, donor.flags | ITEM_FLAG_SKINIZED, "n={n}");
            let mut seq = vec![
                Ex::Started,
                Ex::Fired,
                Ex::Task(task_type::SKINIZE, vec![ItemTask::update_bits(&after)]),
            ];
            seq.extend(double_consumption(task_type::SKINIZE, n, &powder));
            seq.push(Ex::Ended);
            assert_flow(&format!("skinize n={n}"), &frames, seq);
            assert_started(&frames, SKINIZE, caster, target, 5000);
            assert_eq!(a.count(1), (n - 2).max(0), "the stack ends at max(n-2, 0)");
        }
    }

    /// Skinize on an already skinized item: the cast runs, 739 at fire time, nothing consumed.
    pub fn skinize_refused_when_already_skinized(env: &Env) {
        let mut a = env.actor();
        let donor_id = env.give(&a, INV, DONOR, 1, 0);
        env.give(&a, INV, POWDER, 3, 1);
        a.with_item(donor_id, |i| i.flags |= ITEM_FLAG_SKINIZED);
        let (donor, powder) = (a.item(0), a.item(1));
        let (caster, target) = (a.caster(&powder), a.target(donor.id));
        let frames = cast(env, &mut a, SKINIZE, caster, target);
        assert_flow(
            "skinize twice",
            &frames,
            vec![Ex::Started, Ex::Fired, Ex::Err(739, 0), Ex::Ended],
        );
        assert_eq!(a.item(0).flags, donor.flags);
        assert_eq!(a.count(1), 3);
    }

    /// G6: the effect polices neither the item class nor the owner's intent: any owned item is
    /// flagged, here the cleanup consumable.
    pub fn skinize_flags_any_owned_item(env: &Env) {
        let mut a = env.actor();
        env.give(&a, INV, CLEANER, 1, 0);
        env.give(&a, INV, POWDER, 3, 1);
        let (stranger, powder) = (a.item(0), a.item(1));
        let (caster, target) = (a.caster(&powder), a.target(stranger.id));
        let frames = cast(env, &mut a, SKINIZE, caster, target);
        let after = a.item(0);
        assert_eq!(after.flags, stranger.flags | ITEM_FLAG_SKINIZED);
        let mut seq = vec![
            Ex::Started,
            Ex::Fired,
            Ex::Task(task_type::SKINIZE, vec![ItemTask::update_bits(&after)]),
        ];
        seq.extend(double_consumption(task_type::SKINIZE, 3, &powder));
        seq.push(Ex::Ended);
        assert_flow("skinize a non-equipment item", &frames, seq);
    }

    /// Cleanup of a converted base with a consumable stack of 1 / 2 / 3: image 0, then the
    /// consumable TWICE (G7: the effect row consumes it and the generic step takes one more).
    pub fn cleanup_takes_the_consumable_twice(env: &Env) {
        for n in 1..=3 {
            let mut a = env.actor();
            let base_id = env.give(&a, INV, BASE, 1, 0);
            env.give(&a, INV, CLEANER, n, 1);
            a.with_item(base_id, |i| i.details.image_item_template_id = DONOR);
            let (base, cleaner) = (a.item(0), a.item(1));
            let (caster, target) = (a.caster(&cleaner), a.target(base.id));
            let frames = cast(env, &mut a, CLEANUP, caster, target);
            let after = a.item(0);
            assert_eq!(after.details.image_item_template_id, 0, "n={n}");
            let mut seq = vec![
                Ex::Started,
                Ex::Fired,
                Ex::Task(task_type::CONVERT_ITEM_LOOK, vec![ItemTask::update(&after)]),
            ];
            seq.extend(double_consumption(
                task_type::CONVERT_ITEM_LOOK,
                n,
                &cleaner,
            ));
            seq.push(Ex::Ended);
            assert_flow(&format!("cleanup n={n}"), &frames, seq);
            assert_started(&frames, CLEANUP, caster, target, 3000);
            assert_eq!(a.count(1), (n - 2).max(0), "the stack ends at max(n-2, 0)");
        }
    }

    /// A second Cleanup right behind the first is refused up front: `SCSkillStarted` with timeline
    /// id 0 and result 8 (the skill's own 1000 ms cooldown, the global cooldown behind it), no
    /// effect, nothing consumed. The live suite `look-silent` measures the same with the wall clock.
    pub fn second_cleanup_inside_the_cooldown_is_refused(env: &Env) {
        let mut a = env.actor();
        let first = env.give(&a, INV, BASE, 1, 0);
        let second = env.give(&a, INV, DONOR, 1, 1);
        env.give(&a, INV, CLEANER, 6, 2);
        for id in [first, second] {
            a.with_item(id, |i| i.details.image_item_template_id = CHEST);
        }
        let cleaner = a.item(2);
        let (caster, target) = (a.caster(&cleaner), a.target(first));
        let frames = cast(env, &mut a, CLEANUP, caster, target);
        assert_eq!(
            a.item(0).details.image_item_template_id,
            0,
            "the first cast ran"
        );
        assert_eq!(a.count(2), 4);
        assert!(relevant(&frames).len() > 1, "{:?}", describe(&frames));

        let target = a.target(second);
        let frames = cast(env, &mut a, CLEANUP, caster, target);
        let mut refusal = SCSkillStartedPacket::new(CLEANUP, 0, caster, target, SkillObject::None);
        refusal
            .set_skill_result(skill_result::COOLDOWN_TIME)
            .set_result_uint(0);
        let mut s = PacketStream::new();
        refusal.write(&mut s);
        assert_eq!(
            relevant(&frames),
            vec![(sc::SCSkillStartedPacket, s.into_bytes())],
            "{:?}",
            describe(&frames)
        );
        assert_eq!(a.item(1).details.image_item_template_id, CHEST);
        assert_eq!(a.count(2), 4);
    }

    /// Cleanup of an item without an image: the cast runs, 737 at fire time, nothing consumed.
    pub fn cleanup_refused_without_an_image(env: &Env) {
        let mut a = env.actor();
        env.give(&a, INV, BASE, 1, 0);
        env.give(&a, INV, CLEANER, 3, 1);
        let (base, cleaner) = (a.item(0), a.item(1));
        let (caster, target) = (a.caster(&cleaner), a.target(base.id));
        let frames = cast(env, &mut a, CLEANUP, caster, target);
        assert_flow(
            "cleanup without image",
            &frames,
            vec![Ex::Started, Ex::Fired, Ex::Err(737, 0), Ex::Ended],
        );
        assert_eq!(a.count(1), 3);
    }

    /// CS049 with 9 / 10 / 12 powder: the recipe is paid with reason 40 (a Seize + empty receipt
    /// when the stack is used up), the base takes the donor's own template as image (reason 111,
    /// no acknowledgement), the donor is seized. The base keeps id, slot, grade and flags.
    pub fn convert_pays_the_recipe_and_gives_the_donors_image(env: &Env) {
        for n in [9, 10, 12] {
            let mut a = env.actor();
            env.give(&a, INV, BASE, 1, 0);
            env.give(&a, INV, DONOR, 1, 1);
            env.give(&a, INV, POWDER, n, 2);
            let (base, donor, powder) = (a.item(0), a.item(1), a.item(2));
            let frames = convert(env, &mut a, base.id, donor.id);
            let after = a.item(0);
            assert_eq!(after.details.image_item_template_id, DONOR, "n={n}");
            assert_eq!(
                (after.id, after.slot, after.grade, after.flags),
                (base.id, 0, base.grade, base.flags)
            );
            let paid = if n == 9 {
                vec![
                    Ex::Task(REAGENTS, vec![ItemTask::remove_slot(&powder)]),
                    Ex::Task(REAGENTS, vec![]),
                ]
            } else {
                vec![Ex::Task(
                    REAGENTS,
                    vec![ItemTask::count_update(&powder, -9)],
                )]
            };
            let mut seq = paid;
            seq.push(Ex::Task(
                task_type::CONVERT_ITEM_LOOK,
                vec![ItemTask::update(&after)],
            ));
            seq.push(Ex::Task(REAGENTS, vec![ItemTask::remove_slot(&donor)]));
            assert_flow(&format!("convert n={n}"), &frames, seq);
            assert!(a.at(1).is_none(), "the donor is consumed");
            assert_eq!(a.count(2), n - 9);
        }
    }

    /// The refusals CS049 answers with a packet, and the one it answers with nothing.
    pub fn convert_refusals(env: &Env) {
        let mut a = env.actor();
        env.give(&a, INV, BASE, 1, 0);
        env.give(&a, INV, DONOR, 1, 1);
        env.give(&a, INV, POWDER, 9, 2);
        let (base, donor, powder) = (a.item(0), a.item(1), a.item(2));
        // 207: an unknown id, either side
        for (b, i) in [
            (123_456_789, 987_654_321),
            (base.id, 987_654_321),
            (123_456_789, donor.id),
        ] {
            let frames = convert(env, &mut a, b, i);
            assert_flow("unknown ids", &frames, vec![Ex::Err(207, 0)]);
        }
        // 737: the "base" is not equipment (a powder stack), nothing consumed
        let frames = convert(env, &mut a, powder.id, donor.id);
        assert_flow("non-equipment base", &frames, vec![Ex::Err(737, 0)]);
        assert_eq!((a.count(2), a.at(1).map(|i| i.id)), (9, Some(donor.id)));

        // 722 with the powder template: no powder at all
        let mut b = env.actor();
        env.give(&b, INV, BASE, 1, 0);
        env.give(&b, INV, DONOR, 1, 1);
        let (base, donor) = (b.item(0), b.item(1));
        let frames = convert(env, &mut b, base.id, donor.id);
        assert_flow("no powder", &frames, vec![Ex::Err(722, POWDER)]);
        assert_eq!(b.item(0).details.image_item_template_id, 0);
        assert!(b.at(1).is_some());

        // silence: the base template has no ItemLookConvert (the handler only logs)
        let mut c = env.actor();
        env.give(&c, INV, NO_RECIPE, 1, 0);
        env.give(&c, INV, NO_RECIPE, 1, 1);
        env.give(&c, INV, POWDER, 9, 2);
        let (base, other) = (c.item(0), c.item(1));
        let frames = convert(env, &mut c, base.id, other.id);
        assert!(
            frames.is_empty(),
            "no packet at all: {:?}",
            describe(&frames)
        );
        assert_eq!((c.count(2), c.at(1).is_some()), (9, true));
        assert_eq!(c.item(0).details.image_item_template_id, 0);

        // G3: a glider is a backpack for the server, so it is refused like any non-equipment
        let mut d = env.actor();
        let glider = env.give(&d, slot_type::EQUIPMENT, GLIDER, 1, equip_slot::BACKPACK);
        env.give(&d, INV, DONOR, 1, 0);
        env.give(&d, INV, POWDER, 9, 1);
        let donor = d.item(0);
        let frames = convert(env, &mut d, glider, donor.id);
        assert_flow("glider base", &frames, vec![Ex::Err(737, 0)]);
        assert_eq!(d.count(1), 9);
    }

    /// Expected-current gaps of CS049, pinned so a later fix flips a test on purpose.
    pub fn convert_gaps_current_behaviour(env: &Env) {
        let rows = |powder: &Item, after: &Item, donor: &Item, seized_powder: bool| {
            let mut seq = if seized_powder {
                vec![
                    Ex::Task(REAGENTS, vec![ItemTask::remove_slot(powder)]),
                    Ex::Task(REAGENTS, vec![]),
                ]
            } else {
                vec![]
            };
            seq.push(Ex::Task(
                task_type::CONVERT_ITEM_LOOK,
                vec![ItemTask::update(after)],
            ));
            seq.push(Ex::Task(REAGENTS, vec![ItemTask::remove_slot(donor)]));
            seq
        };

        // G1: 8 of the 9 powder convert and take all 8 (C# parity; the native UI asks for 9)
        let mut a = env.actor();
        env.give(&a, INV, BASE, 1, 0);
        env.give(&a, INV, DONOR, 1, 1);
        env.give(&a, INV, POWDER, 8, 2);
        let (base, donor, powder) = (a.item(0), a.item(1), a.item(2));
        let frames = convert(env, &mut a, base.id, donor.id);
        let after = a.item(0);
        assert_eq!(after.details.image_item_template_id, DONOR);
        assert_flow(
            "G1 partial powder",
            &frames,
            rows(&powder, &after, &donor, true),
        );
        assert_eq!(a.count(2), 0);

        // G2: cloaks cost 15 powder (the native recipe group is 4)
        let mut a = env.actor();
        env.give(&a, INV, CLOAK, 1, 0);
        env.give(&a, INV, CLOAK, 1, 1);
        env.give(&a, INV, POWDER, 20, 2);
        let (base, donor, powder) = (a.item(0), a.item(1), a.item(2));
        let frames = convert(env, &mut a, base.id, donor.id);
        let after = a.item(0);
        let mut seq = vec![Ex::Task(
            REAGENTS,
            vec![ItemTask::count_update(&powder, -15)],
        )];
        seq.extend(rows(&powder, &after, &donor, false));
        assert_flow("G2 cloak", &frames, seq);
        assert_eq!(a.count(2), 5);

        // G4: base == image takes the powder, gives the base its own template as image and then
        // seizes the "donor", which is the base: the item is destroyed
        let mut a = env.actor();
        env.give(&a, INV, BASE, 1, 0);
        env.give(&a, INV, POWDER, 9, 1);
        let (base, powder) = (a.item(0), a.item(1));
        let frames = convert(env, &mut a, base.id, base.id);
        let mut imaged = base.clone();
        imaged.details.image_item_template_id = BASE;
        assert_flow(
            "G4 base == image",
            &frames,
            rows(&powder, &imaged, &base, true),
        );
        assert!(a.at(0).is_none(), "the base is gone");
        assert_eq!(a.count(1), 0);

        // G5: the image item may be armor, or not equipment at all
        for (donor_tpl, label) in [
            (CHEST, "G5a armor donor"),
            (CLEANER, "G5b non-equipment donor"),
        ] {
            let mut a = env.actor();
            env.give(&a, INV, BASE, 1, 0);
            env.give(&a, INV, donor_tpl, 1, 1);
            env.give(&a, INV, POWDER, 9, 2);
            let (base, donor, powder) = (a.item(0), a.item(1), a.item(2));
            let frames = convert(env, &mut a, base.id, donor.id);
            let after = a.item(0);
            assert_eq!(after.details.image_item_template_id, donor_tpl, "{label}");
            assert_flow(label, &frames, rows(&powder, &after, &donor, true));
            assert!(a.at(1).is_none());
        }
    }

    /// Which item casts the server accepts and which it drops without a single packet, and what
    /// it does with a target that is not the caster's.
    pub fn item_cast_admission(env: &Env) {
        let mut a = env.actor();
        env.give(&a, INV, DONOR, 1, 0);
        env.give(&a, INV, POWDER, 3, 1);
        env.give(&a, INV, CLEANER, 3, 2);
        let (donor, powder, cleaner) = (a.item(0), a.item(1), a.item(2));
        let mut foreign = env.actor();
        env.give(&foreign, INV, POWDER, 3, 0);
        let foreign_powder = foreign.item(0);

        let silent = |frames: Vec<Frame>, what: &str| {
            assert!(
                relevant(&frames).is_empty(),
                "{what}: {:?}",
                describe(&frames)
            );
        };
        // Cleanup's skill id with the Skinize powder, and the other way round
        let (c, t) = (a.caster(&powder), a.target(donor.id));
        silent(cast(env, &mut a, CLEANUP, c, t), "skill id of another item");
        let (c, t) = (a.caster(&cleaner), a.target(donor.id));
        silent(cast(env, &mut a, SKINIZE, c, t), "skill id of another item");
        // a source item nobody owns, and one that belongs to another character
        let ghost = SkillCaster::Item {
            obj_id: a.p.obj_id,
            item_id: 123_456_789,
            item_template_id: POWDER,
            type1: 0,
            type2: 0,
        };
        let t = a.target(donor.id);
        silent(cast(env, &mut a, SKINIZE, ghost, t), "unknown source item");
        let c = a.caster(&foreign_powder);
        silent(cast(env, &mut a, SKINIZE, c, t), "foreign source item");
        // nothing was consumed or flagged
        assert_eq!(
            (a.count(1), a.count(2), a.item(0).flags),
            (3, 3, donor.flags)
        );
        assert_eq!(foreign.count(0), 3);
        foreign.c.drain();

        // The item target carries another object id: the cast starts and ends, nothing happens.
        let mut b = env.actor();
        env.give(&b, INV, DONOR, 1, 0);
        env.give(&b, INV, POWDER, 3, 1);
        let (donor, powder) = (b.item(0), b.item(1));
        let other_object = SkillCastTarget::Item {
            obj_id: 1,
            id: donor.id,
            type1: 0,
            type2: 0,
        };
        let c = b.caster(&powder);
        let frames = cast(env, &mut b, SKINIZE, c, other_object);
        assert_flow(
            "target of another object",
            &frames,
            vec![Ex::Started, Ex::Fired, Ex::Ended],
        );
        assert_started(&frames, SKINIZE, c, other_object, 5000);
        assert_eq!((b.count(1), b.item(0).flags), (3, donor.flags));

        // The target item does not exist: same.
        let mut b = env.actor();
        env.give(&b, INV, POWDER, 3, 0);
        let powder = b.item(0);
        let c = b.caster(&powder);
        let t = b.target(123_456_789);
        let frames = cast(env, &mut b, SKINIZE, c, t);
        assert_flow(
            "target item missing",
            &frames,
            vec![Ex::Started, Ex::Fired, Ex::Ended],
        );
        assert_started(&frames, SKINIZE, c, t, 5000);
        assert_eq!(b.count(0), 3);
    }

    /// Equipping a converted base into an occupied main hand: the owner gets swap receipts, an
    /// onlooker gets the equipment event whose item carries the image template, and the image
    /// survives the move (what the Bevy actor refresh has to react to).
    pub fn equip_of_a_converted_base_keeps_and_broadcasts_its_image(env: &Env) {
        let mut a = env.actor();
        let mut b = env.actor();
        let worn = env.give(&a, slot_type::EQUIPMENT, DONOR, 1, equip_slot::MAINHAND);
        let base_id = env.give(&a, INV, BASE, 1, 3);
        a.with_item(base_id, |i| i.details.image_item_template_id = DONOR);
        a.c.drain();
        b.c.drain();

        let mut s = PacketStream::new();
        s.write_u64(base_id)
            .write_u64(worn)
            .write_u8(INV)
            .write_u8(3)
            .write_u8(slot_type::EQUIPMENT)
            .write_u8(equip_slot::MAINHAND as u8);
        let p = CSSwapItemsPacket::read(&mut PacketStream::from_bytes(s.into_bytes()))
            .expect("swap body");
        futures::executor::block_on(p.handle(&env.server, &mut a.c.conn)).expect("handle");

        let equipped =
            item_at(&a.p, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8).expect("equipped");
        assert_eq!(equipped.id, base_id);
        assert_eq!(
            equipped.details.image_item_template_id, DONOR,
            "the image survives the equip"
        );
        assert_eq!(a.item(3).id, worn, "the worn sword went to the bag slot");

        let own = a.c.drain();
        assert!(own
            .iter()
            .any(|(op, b)| *op == sc::SCItemTaskSuccessPacket && b[0] == task_type::SWAP_ITEMS));
        assert!(
            !own.iter().any(|(op, _)| *op == sc::SCErrorMsgPacket),
            "{:?}",
            describe(&own)
        );

        let mut item_bytes = PacketStream::new();
        equipped.write(&mut item_bytes);
        let item_bytes = item_bytes.into_bytes();
        let events: Vec<Frame> =
            b.c.drain()
                .into_iter()
                .filter(|(op, _)| *op == sc::SCUnitEquipmentsChangedPacket)
                .collect();
        assert_eq!(
            events.len(),
            2,
            "the worn item leaves and the base enters the same slot"
        );
        for (_, body) in &events {
            let mut r = PacketStream::from_bytes(body.clone());
            assert_eq!(r.read_bc().unwrap(), a.p.obj_id);
            assert_eq!(r.read_u8().unwrap(), 1);
            assert_eq!(r.read_u8().unwrap(), equip_slot::MAINHAND as u8);
        }
        assert!(
            events.iter().any(|(_, body)| body
                .windows(item_bytes.len())
                .any(|w| w == item_bytes.as_slice())),
            "the equipment event carries the converted base with its image"
        );
    }

    /// Converting and cleaning a WORN base updates the worn item in place: the owner gets the usual
    /// receipts with the equipment slot in the update row. Nothing is sent to onlookers about the
    /// image change (current behaviour: the equipment event only exists for equip / unequip).
    pub fn operations_on_a_worn_base_update_it_in_place(env: &Env) {
        let mut a = env.actor();
        let mut b = env.actor();
        env.give(&a, slot_type::EQUIPMENT, BASE, 1, equip_slot::MAINHAND);
        env.give(&a, INV, DONOR, 1, 0);
        env.give(&a, INV, POWDER, 9, 1);
        env.give(&a, INV, CLEANER, 3, 2);
        let worn = |a: &Actor| {
            item_at(&a.p, slot_type::EQUIPMENT, equip_slot::MAINHAND as u8).expect("worn base")
        };
        let (base, donor, powder, cleaner) = (worn(&a), a.item(0), a.item(1), a.item(2));
        b.c.drain();

        let frames = convert(env, &mut a, base.id, donor.id);
        let converted = worn(&a);
        assert_eq!(converted.details.image_item_template_id, DONOR);
        assert_eq!(
            (converted.id, converted.slot_type, converted.slot),
            (base.id, slot_type::EQUIPMENT, equip_slot::MAINHAND)
        );
        assert_flow(
            "convert a worn base",
            &frames,
            vec![
                Ex::Task(REAGENTS, vec![ItemTask::remove_slot(&powder)]),
                Ex::Task(REAGENTS, vec![]),
                Ex::Task(
                    task_type::CONVERT_ITEM_LOOK,
                    vec![ItemTask::update(&converted)],
                ),
                Ex::Task(REAGENTS, vec![ItemTask::remove_slot(&donor)]),
            ],
        );
        let seen = b.c.drain();
        assert!(
            !seen
                .iter()
                .any(|(op, _)| *op == sc::SCUnitEquipmentsChangedPacket),
            "onlookers get no equipment event for an in-place image change: {:?}",
            describe(&seen)
        );

        let (caster, target) = (a.caster(&cleaner), a.target(base.id));
        let frames = cast(env, &mut a, CLEANUP, caster, target);
        let cleaned = worn(&a);
        assert_eq!(cleaned.details.image_item_template_id, 0);
        let mut seq = vec![
            Ex::Started,
            Ex::Fired,
            Ex::Task(
                task_type::CONVERT_ITEM_LOOK,
                vec![ItemTask::update(&cleaned)],
            ),
        ];
        seq.extend(double_consumption(
            task_type::CONVERT_ITEM_LOOK,
            3,
            &cleaner,
        ));
        seq.push(Ex::Ended);
        assert_flow("cleanup a worn base", &frames, seq);
        assert!(!b
            .c
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCUnitEquipmentsChangedPacket));
    }
}

macro_rules! scenarios {
    ($($name:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $name() {
                scenarios::$name(&Env::fixture());
            }
        )*
        const ALL: &[(&str, fn(&Env))] = &[$((stringify!($name), scenarios::$name)),*];
    };
}

scenarios!(
    rows_match_the_pinned_numbers,
    skinize_takes_the_powder_twice,
    skinize_refused_when_already_skinized,
    skinize_flags_any_owned_item,
    cleanup_takes_the_consumable_twice,
    second_cleanup_inside_the_cooldown_is_refused,
    cleanup_refused_without_an_image,
    convert_pays_the_recipe_and_gives_the_donors_image,
    convert_refusals,
    convert_gaps_current_behaviour,
    item_cast_admission,
    equip_of_a_converted_base_keeps_and_broadcasts_its_image,
    operations_on_a_worn_base_update_it_in_place,
);

/// `AAEMU_COMPACT_DB=D:/.../Data/compact.sqlite3 cargo test -p aaemu-game look_appearance -- --ignored`
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn catalog_runs_every_scenario() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let env = Env::with(catalog_data(&path));
    // Facts only the real catalog can show: cloaks have two convert rows, the server keeps the
    // 15-powder one, the native recipe group is the 4-powder one.
    let store = &env.server.game_data.systems.economy.store;
    assert_eq!(store.wearable_item_look_converts.get(&9), Some(&27));
    assert_eq!(store.item_look_converts[&25].required_item_count, 4);
    assert_eq!(store.item_look_converts[&27].required_item_count, 15);
    // Every scenario runs, so one pass lists all the differences between the synthetic rows and
    // the catalog (each panic message is printed as it happens).
    let mut differing = Vec::new();
    for (name, scenario) in ALL {
        eprintln!("catalog scenario {name}");
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scenario(&env))).is_err() {
            differing.push(*name);
        }
    }
    assert!(
        differing.is_empty(),
        "scenarios that differ on the real catalog: {differing:?}"
    );
}
