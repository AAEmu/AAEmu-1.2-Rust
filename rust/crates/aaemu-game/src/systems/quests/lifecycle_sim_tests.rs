//! "Play every quest" simulator (an audit tool, not a regression test).
//!
//! For every quest of the real `compact.sqlite3` that has a Start step, a fresh character is
//! fitted to the quest's component requirements, the quest is accepted the way its Start
//! acceptor dictates, every objective act is satisfied by delivering the gameplay event it
//! subscribes to (through the public `events::*` entry points other subsystems use), the
//! evaluation queue is drained, the quest is reported and the result is checked
//! (completed flag set, quest not active any more, rewards delivered through the hooks).
//!
//! Run (see the final report of the task for the exact command):
//! `AAEMU_COMPACT_DB=.../compact.sqlite3 AAEMU_QUEST_SIM_OUT=out.json cargo test -p aaemu-game
//! --lib real_compact_db_quest_lifecycles -- --ignored --nocapture`.
//!
//! Optional: `AAEMU_QUEST_SIM_ONLY=1,2,3` (only these quest ids), `AAEMU_QUEST_SIM_LIMIT=n`.
//!
//! Every quest is run in isolation: a fresh character with a unique id, a panic is caught and
//! recorded, and the whole server is rebuilt after a panic (poisoned locks). The harness
//! never fixes anything: a quest that does not complete is recorded with the step, the
//! pending acts (objective counts against the required counts) and the engine log lines.
//! `harness_unsupported` marks quests whose pending act the harness cannot synthesize an event
//! for; those are not engine findings.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde_json::{json, Value};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::Layer;

use super::data::QuestData;
use super::engine::{Ctx, EnvView, OwnerView};
use super::events::{self, QuestEvent};
use super::hooks::QuestHooks;
use super::model::{acceptor, kind, ItemCreationDefinition};
use super::spheres::SphereQuest;
use super::state::{ActPath, QuestState};
use super::templates::{ActKind, QuestActTemplate};
use super::unit_reqs::{req_kind, validate, UnitReq};
use crate::data::experience::{ExperienceTable, LevelTemplate};
use crate::data::items::{ItemData, ItemTemplate};
use crate::data::world_data::{FactionData, Localization, ZoneData};
use crate::data::GameData;
use crate::models::char_data::Skill;
use crate::models::character::Character;
use crate::models::item::Item;
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{DoodadRef, Entity, NpcRef, PlayerRef, WorldObject};

/// Iteration cap of one quest (evaluation rounds).
const MAX_ITERATIONS: usize = 80;
/// Event cap of one act delivery.
const MAX_EVENTS_PER_ACT: usize = 3000;

// ----- log / panic capture -----

thread_local! {
    static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static LAST_PANIC: RefCell<String> = const { RefCell::new(String::new()) };
}

struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

/// Records the WARN / ERROR events of the quest code into [`LOG`].
struct LogCapture;

impl<S: tracing::Subscriber> Layer<S> for LogCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: LayerContext<'_, S>) {
        let mut v = MessageVisitor(String::new());
        event.record(&mut v);
        let line = format!("{} {}", event.metadata().level(), v.0);
        LOG.with(|l| {
            let mut l = l.borrow_mut();
            if l.len() < 400 {
                l.push(line);
            }
        });
    }
}

fn take_log() -> Vec<String> {
    LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

/// The last lines of the log, without the per-run noise (duplicates and the removal notice).
fn log_tail(lines: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for l in lines {
        if l.contains("] for player:") && l.contains("removed.") {
            continue;
        }
        let short: String = l.chars().take(300).collect();
        if seen.insert(short.clone()) {
            out.push(short);
        }
    }
    let skip = out.len().saturating_sub(8);
    out.split_off(skip)
}

// ----- hooks -----

#[derive(Default, Clone)]
struct RewardLog {
    exp: i64,
    copper: i64,
    /// Items acquired through `acquire_default_item` / the backpack hook: (template, count).
    items: Vec<(u32, i32)>,
    /// Reward mails: (item count, copper).
    mails: Vec<(usize, i32)>,
    mailed_items: Vec<(u32, i32)>,
    consumed: Vec<(u32, i32)>,
    points: i64,
    labor: i64,
}

#[derive(Default)]
struct SimHooks {
    log: Mutex<RewardLog>,
}

impl SimHooks {
    fn reset(&self) {
        *self.log.lock().unwrap() = RewardLog::default();
    }
    fn snapshot(&self) -> RewardLog {
        self.log.lock().unwrap().clone()
    }
}

static NEXT_ITEM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000);

fn give_item_to(c: &mut Character, container: u8, template_id: u32, count: i32) {
    let t = ItemTemplate::plain(template_id, 1_000_000);
    let id = NEXT_ITEM.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut item = Item::create(id, &t, count, 0, 1, &ItemData::default(), 0);
    let Some(cont) = c.inventory.get(container).cloned() else {
        return;
    };
    let mut g = cont.lock().unwrap();
    item.slot = g.get_unused_slot(-1);
    g.items.push(item);
    g.update_free_slot_count();
}

fn give_item(c: &mut Character, template_id: u32, count: i32) {
    give_item_to(
        c,
        crate::models::container::slot_type::INVENTORY,
        template_id,
        count,
    );
}

fn take_items(c: &mut Character, template_id: u32, count: i32) -> i32 {
    let bag = c.inventory.bag().clone();
    let mut g = bag.lock().unwrap();
    let mut left = count;
    for i in g.items.iter_mut().filter(|i| i.template_id == template_id) {
        let n = left.min(i.count);
        i.count -= n;
        left -= n;
    }
    g.items.retain(|i| i.count > 0);
    g.update_free_slot_count();
    count - left
}

impl QuestHooks for SimHooks {
    fn add_exp(&self, _: &GameServer, _: Option<&PlayerRef>, c: &mut Character, exp: i32, _: bool) {
        c.experience += exp;
        self.log.lock().unwrap().exp += i64::from(exp);
    }
    fn change_money(&self, _: &GameServer, _: Option<&PlayerRef>, c: &mut Character, amount: i32) {
        c.money += i64::from(amount);
        self.log.lock().unwrap().copper += i64::from(amount);
    }
    fn change_game_points(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        _: &mut Character,
        _: u8,
        amount: i32,
    ) {
        self.log.lock().unwrap().points += i64::from(amount);
    }
    fn change_labor(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        _: &mut Character,
        amount: i16,
        _: i32,
    ) {
        self.log.lock().unwrap().labor += i64::from(amount);
    }
    #[allow(clippy::too_many_arguments)]
    fn acquire_default_item(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        c: &mut Character,
        _: u8,
        template_id: u32,
        count: i32,
        _: u8,
    ) -> bool {
        give_item(c, template_id, count);
        self.log.lock().unwrap().items.push((template_id, count));
        true
    }
    #[allow(clippy::too_many_arguments)]
    fn try_equip_new_backpack(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        _: &mut Character,
        _: u8,
        template_id: u32,
        count: i32,
        _: u8,
    ) -> bool {
        self.log.lock().unwrap().items.push((template_id, count));
        true
    }
    #[allow(clippy::too_many_arguments)]
    fn consume_item(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        c: &mut Character,
        _: u8,
        template_id: u32,
        count: i32,
        _: Option<u64>,
    ) -> i32 {
        let removed = take_items(c, template_id, count);
        self.log
            .lock()
            .unwrap()
            .consumed
            .push((template_id, removed));
        removed
    }
    fn send_quest_reward_mails(
        &self,
        _: &GameServer,
        _: Option<&PlayerRef>,
        _: &mut Character,
        _: u32,
        items: &[ItemCreationDefinition],
        copper: i32,
    ) -> usize {
        let mut l = self.log.lock().unwrap();
        l.mails.push((items.len(), copper));
        l.mailed_items
            .extend(items.iter().map(|i| (i.template_id, i.count)));
        0
    }
    fn summon_mate_level(&self, _: &GameServer, _: &Item) -> Option<u8> {
        Some(u8::MAX)
    }
    fn npc_aggro_rating_percent(&self, _: &GameServer, _: &NpcRef, _: u32) -> f32 {
        0.0
    }
}

// ----- the simulator -----

struct Sim {
    server: GameServer,
    hooks: Arc<SimHooks>,
    client: RefCell<TestClient>,
    zone_key_by_group: HashMap<u32, u32>,
    zone_key_by_id: HashMap<u32, u32>,
    faction_by_mother: HashMap<u32, u32>,
    /// `npcs.engage_combat_give_quest_id` -> npc template (quests started by engaging the npc).
    engage_npc_by_quest: HashMap<u32, u32>,
    next_obj: Cell<u32>,
    next_char: Cell<u32>,
}

impl Sim {
    fn new(conn: &Connection) -> Sim {
        let files = crate::data::client_files::ClientFileManager::new();
        let quests = QuestData::load_from(conn, &files).expect("quest tables load");
        let mut gd = GameData::empty();
        gd.systems.quests = quests;
        gd.zones = ZoneData::load(conn, 0).expect("zones");
        let loc = Localization::load(conn, "en_us").expect("localization");
        gd.factions = FactionData::load(conn, &loc).expect("factions");
        let mut stmt = conn
            .prepare("SELECT id, total_exp, total_mate_exp, skill_points FROM levels ORDER BY id")
            .expect("levels");
        let levels: Vec<LevelTemplate> = stmt
            .query_map([], |r| {
                Ok(LevelTemplate {
                    level: r.get::<_, i64>(0)? as u8,
                    total_exp: r.get::<_, i64>(1)? as i32,
                    total_mate_exp: r.get::<_, i64>(2)? as i32,
                    skill_points: r.get::<_, i64>(3)? as i32,
                })
            })
            .expect("levels")
            .map(|r| r.expect("level row"))
            .collect();
        gd.experience = ExperienceTable::from_templates(levels).expect("experience");

        let mut zone_key_by_group = HashMap::new();
        let mut zone_key_by_id = HashMap::new();
        let mut zones: Vec<_> = gd.zones.zones.values().collect();
        zones.sort_by_key(|z| z.zone_key);
        for z in zones {
            zone_key_by_group.entry(z.group_id).or_insert(z.zone_key);
            zone_key_by_id.entry(z.id).or_insert(z.zone_key);
        }
        let mut faction_by_mother = HashMap::new();
        for f in &gd.factions.factions {
            faction_by_mother.entry(f.mother_id).or_insert(f.id);
        }
        let mut engage_npc_by_quest = HashMap::new();
        let mut stmt = conn
            .prepare(
                "SELECT engage_combat_give_quest_id, id FROM npcs                  WHERE engage_combat_give_quest_id > 0 ORDER BY id DESC",
            )
            .expect("engage-combat npcs");
        for row in stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u32))
            })
            .expect("engage-combat npcs")
        {
            let (quest, npc) = row.expect("engage-combat row");
            engage_npc_by_quest.insert(quest, npc);
        }
        let server = test_server_with(gd);
        let hooks = Arc::new(SimHooks::default());
        server.systems.quests.hooks.install(hooks.clone());
        let client = TestClient::new(&server, 1, 1001);
        Sim {
            server,
            hooks,
            client: RefCell::new(client),
            zone_key_by_group,
            zone_key_by_id,
            faction_by_mother,
            engage_npc_by_quest,
            next_obj: Cell::new(0x2000_0000),
            next_char: Cell::new(200_000),
        }
    }

    fn data(&self) -> &QuestData {
        &self.server.game_data.systems.quests
    }

    fn obj_id(&self) -> u32 {
        let v = self.next_obj.get();
        self.next_obj.set(v + 1);
        v
    }

    fn char_id(&self) -> u32 {
        let v = self.next_char.get();
        self.next_char.set(v + 1);
        v
    }

    fn drain_queue(&self) {
        for _ in 0..200 {
            if self.server.systems.quests.queued_count() == 0 {
                break;
            }
            self.server
                .systems
                .quests
                .run_queued_evaluations(&self.server);
        }
    }

    /// Fits a character to the requirements of the quest's components. Returns notes for
    /// requirements the harness cannot satisfy.
    fn fit_character(&self, qid: u32, c: &mut Character) -> Vec<String> {
        let d = self.data();
        let mut notes = Vec::new();
        let Some(t) = d.template(qid) else {
            return notes;
        };
        let mut lower = 0u32;
        let mut upper = 255u32;
        for cid in &t.components {
            let Some(ct) = d.component(*cid) else {
                continue;
            };
            let reqs = d.unit_reqs.requirements("QuestComponent", *cid);
            if ct.or_unit_reqs {
                let mut ok = reqs.is_empty();
                let mut last = String::new();
                for r in reqs {
                    match self.apply_req(r, c, &mut lower, &mut upper) {
                        Ok(()) => {
                            ok = true;
                            break;
                        }
                        Err(e) => last = e,
                    }
                }
                if !ok {
                    notes.push(format!("component {cid} (OR): {last}"));
                }
            } else {
                for r in reqs {
                    if let Err(e) = self.apply_req(r, c, &mut lower, &mut upper) {
                        notes.push(format!("component {cid}: {e}"));
                    }
                }
            }
        }
        if super::engine::has_quest_completed(&c.quests.completed, qid) {
            // A component that requires the quest itself to be completed (unsatisfiable).
            super::engine::set_completed_quest_flag(&mut c.quests.completed, qid, false);
            notes.push("[self] a component requires this quest to be completed already".into());
        }
        if lower > 0 {
            c.unit.level = lower.min(255) as u8;
        }
        if lower > upper {
            notes.push(format!("conflicting level requirements {lower}..{upper}"));
        } else if u32::from(c.unit.level) > upper {
            c.unit.level = upper as u8;
        }
        notes
    }

    fn apply_req(
        &self,
        r: &UnitReq,
        c: &mut Character,
        lower: &mut u32,
        upper: &mut u32,
    ) -> Result<(), String> {
        let (v1, v2) = (r.value1, r.value2);
        let desc = || format!("req kind {:#x} v1={v1} v2={v2}", r.kind);
        match r.kind {
            req_kind::LEVEL => {
                *lower = (*lower).max(v1);
                if v2 > 0 {
                    *upper = (*upper).min(v2);
                }
            }
            req_kind::MAX_LEVEL => *upper = (*upper).min(v1),
            req_kind::ABILITY => {
                let exp = self
                    .server
                    .game_data
                    .experience
                    .exp_for_level(v2.min(255) as u8, false);
                if let Some(a) = c
                    .abilities
                    .abilities
                    .iter_mut()
                    .find(|a| u32::from(a.id) == v1)
                {
                    a.exp = a.exp.max(exp);
                }
            }
            req_kind::RACE => c.race = v1 as u8,
            req_kind::GENDER => c.gender = v1 as u8,
            req_kind::OWN_ITEM => give_item(c, v1, 1),
            req_kind::EQUIP_ITEM => {
                give_item_to(c, crate::models::container::slot_type::EQUIPMENT, v1, 1)
            }
            req_kind::TRAINED_SKILL => c.skills.skills.push(Skill { id: v1, level: 1 }),
            req_kind::COMPLETE_QUEST_CONTEXT => {
                super::engine::set_completed_quest_flag(&mut c.quests.completed, v1, true);
            }
            req_kind::HONOR_POINT => c.honor_point = c.honor_point.max(v1 as i32),
            req_kind::JURY_POINT => c.jury_point = c.jury_point.max(v1 as i32),
            req_kind::LIVING_POINT => c.vocation_point = c.vocation_point.max(v1 as i32),
            req_kind::ACT_ABILITY_POINT => {
                use crate::models::char_data::Actability;
                match c.actability.actabilities.iter_mut().find(|a| a.id == v1) {
                    Some(a) => a.point = a.point.max(v2 as i32),
                    None => c.actability.actabilities.push(Actability {
                        id: v1,
                        point: v2 as i32,
                        step: 0,
                    }),
                }
            }
            req_kind::APPELLATION => {
                if !c.appellations.appellations.contains(&v1) {
                    c.appellations.appellations.push(v1);
                }
            }
            req_kind::IN_ZONE => match self.zone_key_by_id.get(&v1) {
                Some(k) => c.unit.transform.zone_id = *k,
                None => return Err(format!("{}: no zone with id {v1}", desc())),
            },
            req_kind::FACTION_MATCH | req_kind::FACTION_MATCH_ONLY => c.unit.faction_id = v1,
            req_kind::MOTHER_FACTION | req_kind::MOTHER_FACTION_ONLY => {
                match self.faction_by_mother.get(&v1) {
                    Some(f) => c.unit.faction_id = *f,
                    None => return Err(format!("{}: no faction with mother {v1}", desc())),
                }
            }
            req_kind::EXPEDITION_MEMBER => c.unit.expedition_id = 1,
            req_kind::LABOR_POWER_MARGIN => c.labor_power = 0,
            req_kind::TOD => self.server.time.set((v1 as f32 + 1.0) / 100.0),
            req_kind::PROGRESS_QUEST_CONTEXT
            | req_kind::READY_QUEST_CONTEXT
            | req_kind::PRE_COMPLETE_QUEST_CONTEXT => {
                return Err(format!("{}: needs quest {v1} active", desc()));
            }
            req_kind::BUFF => return Err(format!("{}: needs buff {v1}", desc())),
            req_kind::EQUIP_SLOT
            | req_kind::EQUIP_SHIELD
            | req_kind::EQUIP_RANGED
            | req_kind::DOODAD_RANGE
            | req_kind::AREA_SPHERE
            | req_kind::TARGET_DOODAD
            | req_kind::TARGET_BUFF
            | req_kind::TARGET_BUFF_TAG
            | req_kind::TARGET_HEALTH_LESS_THAN
            | req_kind::EXPEDITION_OWNER => {
                return Err(format!("{}: not satisfiable by the harness", desc()));
            }
            _ => {}
        }
        Ok(())
    }
}

// ----- one quest run -----

/// What the harness found pending in the current step.
struct Pend {
    act: usize,
    have: i32,
}

struct Snap {
    active: bool,
    completed: bool,
    step: u8,
    status: u8,
    component_id: u32,
    objectives: [i32; 5],
    acceptor: (u8, u32),
    pending: Vec<Pend>,
    /// Inactive components of the current step with their failing requirements.
    inactive: Vec<(u32, Vec<String>)>,
}

enum Delivered {
    Events(usize),
    /// The harness has no way to synthesize this act's event.
    Unsupported(String),
    /// The act's data can never be satisfied (no member NPC, no zone, ...).
    DataGap(String),
    /// Nothing to send: satisfied through other acts / evaluation.
    Indirect,
}

#[derive(Default)]
struct Outcome {
    result: &'static str,
    accept_via: String,
    notes: Vec<String>,
    unsupported: BTreeSet<String>,
    data_gaps: BTreeSet<String>,
    events_sent: usize,
    iterations: usize,
    forced_evaluation: bool,
    reward_warnings: Vec<String>,
    reload: Vec<Value>,
    skip_reason: String,
    final_snap: Option<Value>,
    reject: Option<Value>,
}

struct Run<'a> {
    sim: &'a Sim,
    qid: u32,
    player: PlayerRef,
    char_id: u32,
    spawned: Vec<WorldObject>,
    npcs: HashMap<(u32, u8, u32, u32), NpcRef>,
    doodads: HashMap<u32, DoodadRef>,
    out: Outcome,
}

impl<'a> Run<'a> {
    fn server(&self) -> &'a GameServer {
        &self.sim.server
    }

    fn data(&self) -> &'a QuestData {
        self.sim.data()
    }

    fn npc(&mut self, template: u32, level: u8, faction: u32, zone_key: u32) -> NpcRef {
        let key = (template, level, faction, zone_key);
        if let Some(n) = self.npcs.get(&key) {
            return n.clone();
        }
        let mut npc = Npc::new(template);
        npc.unit.transform.set_xyz(102.0, 100.0, 100.0);
        npc.unit.level = level;
        npc.unit.faction_id = faction;
        let e = Entity::new(self.sim.obj_id(), npc);
        let world = self.server().world.world(0).expect("instance 0");
        world.spawn(&e, &self.server().view_ctx());
        e.data().unit.transform.zone_id = zone_key;
        self.spawned.push(WorldObject::from(&e));
        self.npcs.insert(key, e.clone());
        e
    }

    fn plain_npc(&mut self, template: u32) -> NpcRef {
        self.npc(template, 10, 0, TEST_ZONE)
    }

    fn doodad(&mut self, template: u32) -> DoodadRef {
        if let Some(d) = self.doodads.get(&template) {
            return d.clone();
        }
        let mut doodad = crate::models::doodad::Doodad::new(template);
        doodad.transform.set_xyz(103.0, 100.0, 100.0);
        let e = Entity::new(self.sim.obj_id(), doodad);
        let world = self.server().world.world(0).expect("instance 0");
        world.spawn(&e, &self.server().view_ctx());
        self.spawned.push(WorldObject::from(&e));
        self.doodads.insert(template, e.clone());
        e
    }

    fn target(&self, obj_id: u32) {
        self.player.data().unit.current_target = obj_id;
    }

    // ----- accepting -----

    fn starters(&self) -> Vec<(String, Starter)> {
        let d = self.data();
        let mut v: Vec<(String, Starter)> = Vec::new();
        let Some(t) = d.template(self.qid) else {
            return v;
        };
        for c in t.components_of(&d.components, kind::START) {
            for &a in &c.acts {
                let at = &d.acts[a];
                let s = match at.kind {
                    ActKind::ConAcceptNpc { npc_id }
                    | ActKind::ConAcceptNpcKill { npc_id }
                    | ActKind::ConAcceptNpcEmotion { npc_id, .. } => Starter::Npc(npc_id),
                    ActKind::ConAcceptDoodad { doodad_id } => Starter::Doodad(doodad_id),
                    ActKind::ConAcceptItem { item_id, .. } => Starter::Item(item_id, 1),
                    ActKind::ConAcceptItemGain { item_id } => {
                        Starter::ItemGain(item_id, at.count.max(1))
                    }
                    ActKind::ConAcceptItemEquip { item_id } => Starter::ItemEquip(item_id),
                    ActKind::ConAcceptSphere { sphere_id } => Starter::Sphere(sphere_id),
                    ActKind::ConAcceptSkill { skill_id } => Starter::Skill(skill_id),
                    ActKind::ConAcceptBuff { buff_id } => Starter::Buff(buff_id),
                    ActKind::ConAcceptLevelUp { level } => Starter::Level(level),
                    ActKind::ConAutoComplete => Starter::Auto,
                    ActKind::ConAcceptComponent { .. } => Starter::Component,
                    _ => continue,
                };
                let name = format!("{}({})", at.kind.class_name(), s.arg());
                if !v.iter().any(|(n, _)| *n == name) {
                    v.push((name, s));
                }
            }
        }
        if v.is_empty() {
            v.push(("no starter act (add_quest)".into(), Starter::Auto));
        }
        // A quest only reachable through a component / auto start is, in the live game, given
        // by engaging its npc (`NpcTemplate.EngageCombatGiveQuestId` -> `AddQuestFromNpc`),
        // which makes the npc the acceptor (`QuestActObjAggro` needs that).
        if v.iter()
            .all(|(_, s)| matches!(s, Starter::Auto | Starter::Component))
        {
            if let Some(&npc) = self.sim.engage_npc_by_quest.get(&self.qid) {
                let s = Starter::Npc(npc);
                v.insert(0, (format!("engage-combat npc({npc})"), s));
            }
        }
        // Gameplay acceptors first, the acceptor-less ones (auto / component) last.
        v.sort_by_key(|(_, s)| matches!(s, Starter::Auto | Starter::Component));
        v
    }

    fn attempt(&mut self, s: &Starter) -> bool {
        let (server, p, qid) = (self.server(), self.player.clone(), self.qid);
        match *s {
            Starter::Npc(npc_id) => {
                let npc = self.plain_npc(npc_id);
                events::add_quest_from_npc(server, &p, qid, npc.obj_id);
            }
            Starter::Doodad(doodad_id) => {
                let d = self.doodad(doodad_id);
                events::add_quest_from_doodad(server, &p, qid, d.obj_id);
            }
            Starter::Item(item, count) => {
                give_item(&mut p.data(), item, count);
                let bound = self.data().quest_id_from_starter_item(item);
                if bound != qid {
                    self.out.notes.push(format!(
                        "starter item {item} is bound to quest {bound}, not {qid}"
                    ));
                }
                events::add_quest_from_item(server, &p, qid, item);
            }
            Starter::ItemGain(item, count) => {
                // The item-gain path: the bag receives the items, the inventory raises the
                // acquired event, which starts the quest (no `add_quest_from_item`).
                give_item(&mut p.data(), item, count);
                events::on_items_acquired(server, &p, item, count);
            }
            Starter::ItemEquip(item) => {
                give_item_to(
                    &mut p.data(),
                    crate::models::container::slot_type::EQUIPMENT,
                    item,
                    1,
                );
                events::add_quest_from_item(server, &p, qid, item);
            }
            Starter::Sphere(sphere) => {
                events::on_enter_quest_starter_sphere(server, &p, qid, sphere);
            }
            Starter::Skill(skill) => {
                events::add_quest_from_skill(server, &p, qid, skill);
            }
            Starter::Buff(buff) => {
                events::add_quest_from_buff(server, &p, qid, buff);
            }
            Starter::Level(level) => {
                {
                    let mut c = p.data();
                    c.unit.level = c.unit.level.max(level);
                }
                events::on_level_up(server, &p);
            }
            Starter::Auto | Starter::Component => {
                events::add_quest(server, &p, qid, false);
            }
        }
        self.sim.drain_queue();
        // A quest without objectives completes inside the accepting call itself.
        events::has_quest(&p, qid) || events::has_quest_completed_in(&p.data(), qid)
    }

    // ----- state -----

    fn failing_reqs(ctx: &Ctx<'_>, cid: u32) -> Vec<String> {
        let reqs = ctx.data.unit_reqs.requirements("QuestComponent", cid);
        let owner = OwnerView {
            c: ctx.c,
            st: ctx.st,
            server: ctx.server,
        };
        let env = EnvView {
            server: ctx.server,
            env: &ctx.env,
        };
        let or = ctx.data.component(cid).is_some_and(|c| c.or_unit_reqs);
        let mut v: Vec<String> = reqs
            .iter()
            .filter(|r| !validate(r, &owner, &owner, &env))
            .map(|r| format!("kind {:#x} v1={} v2={}", r.kind, r.value1, r.value2))
            .collect();
        if or && !v.is_empty() {
            v.insert(0, "OR".into());
        }
        v
    }

    fn snapshot(&self) -> Snap {
        let qid = self.qid;
        events::with_quests(self.server(), &self.player, None, |ctx| {
            let completed = ctx.has_quest_completed(qid);
            let Some(h) = ctx.st.active_handle(qid) else {
                return Snap {
                    active: false,
                    completed,
                    step: 0,
                    status: 0,
                    component_id: 0,
                    objectives: [0; 5],
                    acceptor: (0, 0),
                    pending: Vec::new(),
                    inactive: Vec::new(),
                };
            };
            let q = ctx.st.quest(h).expect("quest");
            let (step, status, component_id, objectives) =
                (q.step, q.status, q.component_id, q.objectives);
            let acceptor = (q.acceptor_type, q.acceptor_id);
            let mut pending = Vec::new();
            let mut inactive = Vec::new();
            if let Some(si) = q.step_index(step) {
                let comps: Vec<(usize, u32, bool, usize)> = q.steps[si]
                    .components
                    .iter()
                    .enumerate()
                    .map(|(ci, c)| (ci, c.template_id, c.is_active, c.acts.len()))
                    .collect();
                for (ci, cid, active, n) in comps {
                    if !active {
                        inactive.push((cid, Self::failing_reqs(ctx, cid)));
                        continue;
                    }
                    if step != kind::PROGRESS {
                        continue;
                    }
                    for ai in 0..n {
                        let path = ActPath {
                            step: si,
                            component: ci,
                            act: ai,
                        };
                        let act = ctx.st.quest(h).expect("quest").steps[si].components[ci].acts[ai]
                            .template;
                        if ctx.run_act(h, path) {
                            continue;
                        }
                        let have = ctx
                            .st
                            .quest(h)
                            .and_then(|q| {
                                q.objectives
                                    .get(usize::from(ctx.data.acts[act].objective_index))
                            })
                            .copied()
                            .unwrap_or(0);
                        pending.push(Pend { act, have });
                    }
                }
            }
            Snap {
                active: true,
                completed,
                step,
                status,
                component_id,
                objectives,
                acceptor,
                pending,
                inactive,
            }
        })
        .expect("snapshot")
    }

    fn force_evaluation(&self) {
        let qid = self.qid;
        events::with_quests(self.server(), &self.player, None, |ctx| {
            if let Some(h) = ctx.st.active_handle(qid) {
                ctx.starting_evaluation(h);
                ctx.run_current_step(h);
            }
        });
        self.sim.drain_queue();
    }

    // ----- delivering -----

    fn need(&self, at: &QuestActTemplate, have: i32) -> usize {
        let max = self
            .data()
            .template(at.quest_id)
            .and_then(|t| at.max_objective(t))
            .unwrap_or(1);
        ((max - have).max(1) as usize).min(MAX_EVENTS_PER_ACT)
    }

    fn group_npc(&self, group: u32) -> Option<u32> {
        self.data().group_npcs.members(group).first().copied()
    }

    fn group_item(&self, group: u32) -> Option<u32> {
        self.data().group_items.members(group).first().copied()
    }

    fn zone_key_of_group(&self, group: u32) -> Option<u32> {
        if group == 0 {
            return Some(TEST_ZONE);
        }
        self.sim.zone_key_by_group.get(&group).copied()
    }

    fn deliver(&mut self, p: &Pend, acceptor: (u8, u32)) -> Delivered {
        let d = self.data();
        let at = &d.acts[p.act];
        let (server, pl, qid) = (self.server(), self.player.clone(), self.qid);
        let need = self.need(at, p.have);
        match &at.kind {
            ActKind::ObjMonsterHunt { npc_id, .. } => {
                let npc = self.plain_npc(*npc_id);
                self.target(npc.obj_id);
                for _ in 0..need {
                    events::on_npc_killed(server, &pl, &npc);
                }
                Delivered::Events(need)
            }
            ActKind::ObjMonsterGroupHunt {
                quest_monster_group_id,
                ..
            } => {
                let Some(npc_id) = self.group_npc(*quest_monster_group_id) else {
                    return Delivered::DataGap(format!(
                        "monster group {quest_monster_group_id} has no member NPCs"
                    ));
                };
                let npc = self.plain_npc(npc_id);
                self.target(npc.obj_id);
                for _ in 0..need {
                    events::on_npc_killed(server, &pl, &npc);
                }
                Delivered::Events(need)
            }
            ActKind::ObjZoneKill {
                zone_id,
                count_npc,
                count_player_kill,
                lvl_min,
                lvl_max,
                lvl_min_npc,
                lvl_max_npc,
                npc_faction_id,
                npc_faction_exclusive,
                pc_faction_id,
                pc_faction_exclusive,
                ..
            } => {
                let Some(zone_key) = self.zone_key_of_group(*zone_id) else {
                    return Delivered::DataGap(format!("no zone with group id {zone_id}"));
                };
                if *count_npc > 0 {
                    if *lvl_max_npc > 0 && lvl_min_npc > lvl_max_npc {
                        return Delivered::DataGap(format!(
                            "ObjZoneKill npc level range {lvl_min_npc}..{lvl_max_npc} is empty"
                        ));
                    }
                    let faction = match (*npc_faction_id, *npc_faction_exclusive) {
                        (0, _) => 0,
                        (f, false) => f,
                        (f, true) => f + 1,
                    };
                    let npc = self.npc(
                        900_001,
                        (*lvl_min_npc).clamp(1, 255) as u8,
                        faction,
                        zone_key,
                    );
                    self.target(npc.obj_id);
                    for _ in 0..need {
                        events::on_npc_killed(server, &pl, &npc);
                    }
                    return Delivered::Events(need);
                }
                if *count_player_kill > 0 {
                    // Player kills have no production event source (only `on_npc_killed`
                    // raises `ZoneKill`); the act logic is exercised directly.
                    if *lvl_max > 0 && lvl_min > lvl_max {
                        return Delivered::DataGap(format!(
                            "ObjZoneKill pc level range {lvl_min}..{lvl_max} is empty"
                        ));
                    }
                    let faction = match (*pc_faction_id, *pc_faction_exclusive) {
                        (0, _) => 0,
                        (f, false) => f,
                        (f, true) => f + 1,
                    };
                    let group = if *zone_id == 0 { 0 } else { *zone_id };
                    let (killer_obj, killer_id) = (pl.obj_id, pl.character_id);
                    let level = (*lvl_min).clamp(1, 255) as u8;
                    for _ in 0..need {
                        events::with_quests(server, &pl, None, |ctx| {
                            let _ = ctx.dispatch(&QuestEvent::ZoneKill {
                                zone_group_id: group,
                                killer: events::KillUnit {
                                    obj_id: killer_obj,
                                    character_id: Some(killer_id),
                                    level: 10,
                                    ..Default::default()
                                },
                                victim: events::KillUnit {
                                    obj_id: 0x3fff_0000,
                                    character_id: Some(0x3fff_0000),
                                    faction_id: faction,
                                    level,
                                    ..Default::default()
                                },
                            });
                        });
                    }
                    self.out.notes.push(
                        "pvp ZoneKill: delivered by direct dispatch, no production event source"
                            .into(),
                    );
                    return Delivered::Events(need);
                }
                Delivered::DataGap("ObjZoneKill with no npc and no player kill count".into())
            }
            ActKind::ObjZoneMonsterHunt { zone_id, .. } => {
                let Some(zone_key) = self.zone_key_of_group(*zone_id) else {
                    return Delivered::DataGap(format!("no zone with group id {zone_id}"));
                };
                let npc = self.npc(900_002, 10, 0, zone_key);
                self.target(npc.obj_id);
                for _ in 0..need {
                    events::on_npc_killed(server, &pl, &npc);
                }
                Delivered::Events(need)
            }
            ActKind::ObjAggro { .. } => {
                let (acc_type, acc_id) = acceptor;
                if acc_type != acceptor::NPC {
                    return Delivered::DataGap(format!(
                        "ObjAggro needs an NPC acceptor (accepted via type {acc_type})"
                    ));
                }
                let npc = self.plain_npc(acc_id);
                self.target(npc.obj_id);
                events::on_npc_killed(server, &pl, &npc);
                Delivered::Events(1)
            }
            ActKind::ObjItemGather { item_id, .. } => {
                let max = (have_max(self, at)).max(1);
                let give = (max - p.have).max(1);
                give_item(&mut pl.data(), *item_id, give);
                events::on_items_acquired(server, &pl, *item_id, give);
                Delivered::Events(1)
            }
            ActKind::ObjItemGroupGather { item_group_id, .. } => {
                let Some(item) = self.group_item(*item_group_id) else {
                    return Delivered::DataGap(format!(
                        "item group {item_group_id} has no members"
                    ));
                };
                let max = (have_max(self, at)).max(1);
                let give = (max - p.have).max(1);
                give_item(&mut pl.data(), item, give);
                events::on_items_acquired(server, &pl, item, give);
                Delivered::Events(1)
            }
            ActKind::ObjItemUse { item_id, .. } => {
                for _ in 0..need {
                    events::on_item_use(server, &pl, *item_id);
                }
                Delivered::Events(need)
            }
            ActKind::ObjItemGroupUse { item_group_id, .. } => {
                let Some(item) = self.group_item(*item_group_id) else {
                    return Delivered::DataGap(format!(
                        "item group {item_group_id} has no members"
                    ));
                };
                for _ in 0..need {
                    events::on_item_use(server, &pl, item);
                }
                Delivered::Events(need)
            }
            ActKind::ObjInteraction { doodad_id, .. } => {
                for _ in 0..need {
                    events::on_doodad_interaction(server, &pl, &pl, *doodad_id);
                }
                Delivered::Events(need)
            }
            ActKind::ObjTalk { npc_id, .. } => {
                let npc = self.plain_npc(*npc_id);
                self.target(npc.obj_id);
                events::on_talk_made(
                    server,
                    &pl,
                    &pl,
                    npc.obj_id,
                    qid,
                    at.component_id,
                    at.act_id,
                );
                Delivered::Events(1)
            }
            ActKind::ObjTalkNpcGroup { npc_group_id, .. } => {
                let Some(npc_id) = self.group_npc(*npc_group_id) else {
                    return Delivered::DataGap(format!("npc group {npc_group_id} has no members"));
                };
                let npc = self.plain_npc(npc_id);
                self.target(npc.obj_id);
                events::on_talk_made(
                    server,
                    &pl,
                    &pl,
                    npc.obj_id,
                    qid,
                    at.component_id,
                    at.act_id,
                );
                Delivered::Events(1)
            }
            ActKind::ObjExpressFire {
                express_key_id,
                npc_group_id,
                ..
            } => {
                let Some(npc_id) = self.group_npc(*npc_group_id) else {
                    return Delivered::DataGap(format!("npc group {npc_group_id} has no members"));
                };
                let npc = self.plain_npc(npc_id);
                self.target(npc.obj_id);
                for _ in 0..need {
                    events::on_express_fire(server, &pl, *express_key_id, pl.obj_id, npc.obj_id);
                }
                Delivered::Events(need)
            }
            ActKind::ObjLevel { level, .. } => {
                {
                    let mut c = pl.data();
                    c.unit.level = c.unit.level.max(*level);
                }
                events::on_level_up(server, &pl);
                Delivered::Events(1)
            }
            ActKind::ObjAbilityLevel {
                ability_id, level, ..
            } => {
                let exp = server.game_data.experience.exp_for_level(*level, false);
                {
                    let mut c = pl.data();
                    for a in c.abilities.abilities.iter_mut() {
                        if *ability_id == 0 || a.id == *ability_id {
                            a.exp = a.exp.max(exp);
                        }
                    }
                }
                events::on_level_up(server, &pl);
                Delivered::Events(1)
            }
            ActKind::ObjMateLevel { item_id, .. } => {
                give_item(&mut pl.data(), *item_id, 1);
                events::on_mate_level_up(server, &pl);
                Delivered::Events(1)
            }
            ActKind::ObjSphere { .. } | ActKind::CheckSphere { .. } => {
                let sphere = SphereQuest {
                    zone_id: 0,
                    world_id: "main_world".into(),
                    quest_id: qid,
                    component_id: at.component_id,
                    radius: 1.0,
                    xyz: (0.0, 0.0, 0.0),
                };
                events::on_enter_sphere(server, &pl, sphere);
                Delivered::Events(1)
            }
            ActKind::ObjCinema { .. } => {
                events::on_cinema_started(server, &pl);
                events::on_cinema_ended(server, &pl);
                Delivered::Events(2)
            }
            ActKind::ObjCompleteQuest { quest_id, .. } => {
                let other = *quest_id;
                events::with_quests(server, &pl, None, |ctx| {
                    ctx.set_completed_quest_flag(other, true);
                    let _ = ctx.dispatch(&QuestEvent::QuestComplete { quest_id: other });
                });
                Delivered::Events(1)
            }
            ActKind::ObjCraft { craft_id, .. } => {
                for _ in 0..need {
                    events::on_craft(server, &pl, *craft_id);
                }
                Delivered::Events(need)
            }
            ActKind::ObjDistance {
                within,
                npc_id,
                distance,
                ..
            }
            | ActKind::CheckDistance {
                within,
                npc_id,
                distance,
            } => {
                if *within && *distance > 0 {
                    self.plain_npc(*npc_id);
                } else if *within {
                    return Delivered::DataGap("distance act with a non-positive range".into());
                }
                Delivered::Events(0)
            }
            ActKind::CheckCompleteComponent { .. } => Delivered::Indirect,
            ActKind::ConFail { .. }
            | ActKind::ObjCondition { .. }
            | ActKind::ObjDoodadPhaseCheck { .. }
            | ActKind::ObjEffectFire { .. }
            | ActKind::ObjSendMail { .. }
            | ActKind::ObjZoneNpcTalk { .. }
            | ActKind::ObjZoneQuestComplete { .. }
            | ActKind::SupplyAaPoint { .. }
            | ActKind::SupplyInteraction { .. }
            | ActKind::SupplySkill { .. } => Delivered::DataGap(format!(
                "{} is not implemented in acts.rs (act_run returns false)",
                at.kind.class_name()
            )),
            other => {
                Delivered::Unsupported(format!("no event synthesizer for {}", other.class_name()))
            }
        }
    }

    /// Reports the quest (Ready step, or a Progress step whose objectives are all met).
    /// Returns whether a report was sent.
    fn report(&mut self, inactive: &[(u32, Vec<String>)]) -> bool {
        let d = self.data();
        let (server, pl, qid) = (self.server(), self.player.clone(), self.qid);
        let Some(t) = d.template(qid) else {
            return false;
        };
        let selected = self.selected_reward();
        for c in t.components_of(&d.components, kind::READY) {
            if inactive.iter().any(|(cid, _)| *cid == c.id) {
                continue;
            }
            for &a in &c.acts {
                match d.acts[a].kind {
                    ActKind::ConReportNpc { npc_id, .. } => {
                        let npc = self.plain_npc(npc_id);
                        self.target(npc.obj_id);
                        events::on_report(server, &pl, qid, npc.obj_id, 0, selected);
                        return true;
                    }
                    ActKind::ConReportDoodad { doodad_id, .. } => {
                        let dd = self.doodad(doodad_id);
                        events::on_report(server, &pl, qid, 0, dd.obj_id, selected);
                        return true;
                    }
                    ActKind::ConReportJournal => {
                        events::on_report(server, &pl, qid, 0, 0, selected);
                        return true;
                    }
                    _ => {}
                }
            }
        }
        if t.let_it_done {
            // `CSTryQuestCompleteAsLetItDone` / an object-free `CSCompleteQuestContextPacket`.
            events::on_report(server, &pl, qid, 0, 0, selected);
            return true;
        }
        false
    }

    /// The first selective reward index of the quest (0 when it has none).
    fn selected_reward(&self) -> i32 {
        let d = self.data();
        let Some(t) = d.template(self.qid) else {
            return 0;
        };
        t.components_of(&d.components, kind::REWARD)
            .iter()
            .flat_map(|c| c.acts.iter())
            .filter(|&&a| matches!(d.acts[a].kind, ActKind::SupplySelectiveItem { .. }))
            .map(|&a| d.acts[a].selective_index)
            .min()
            .unwrap_or(0)
    }

    // ----- reload -----

    fn rows(&self) -> Vec<crate::models::char_data::ActiveQuest> {
        self.player
            .data()
            .quests
            .active
            .iter()
            .filter(|q| q.template_id == self.qid)
            .cloned()
            .collect()
    }

    /// Save -> reload: `CharacterQuests.Load` from the rows `sync_rows` produced.
    fn reload(&mut self) -> Value {
        self.sim.drain_queue();
        let before = self.rows();
        let server = self.server();
        server.systems.quests.remove_quest_timer(self.char_id, 0);
        {
            let mut c = self.player.data();
            let rows = c.quests.active.clone();
            c.quest_state = QuestState::default();
            c.quests.active = rows;
        }
        events::with_quests(server, &self.player, None, |_| ());
        self.sim.drain_queue();
        let after = self.rows();
        let mut diffs = Vec::new();
        match (before.first(), after.first()) {
            (Some(b), Some(a)) => {
                if b.step != a.step {
                    diffs.push(format!("step {} -> {}", b.step, a.step));
                }
                if b.status != a.status {
                    diffs.push(format!("status {} -> {}", b.status, a.status));
                }
                if b.objectives != a.objectives {
                    diffs.push(format!(
                        "objectives {:?} -> {:?}",
                        b.objectives, a.objectives
                    ));
                }
                if (b.acceptor_type, b.acceptor_id) != (a.acceptor_type, a.acceptor_id) {
                    diffs.push("acceptor changed".into());
                }
                if b.component_id != a.component_id {
                    diffs.push(format!(
                        "component {} -> {}",
                        b.component_id, a.component_id
                    ));
                }
            }
            (Some(_), None) => diffs.push("quest missing after reload".into()),
            (None, _) => diffs.push("quest not active before reload".into()),
        }
        json!({"ok": diffs.is_empty(), "diffs": diffs,
               "before": before.first().map(|b| json!({"step": b.step, "status": b.status, "objectives": b.objectives})),
               "after": after.first().map(|b| json!({"step": b.step, "status": b.status, "objectives": b.objectives}))})
    }

    // ----- the run -----

    fn describe_snap(&self, snap: &Snap) -> Value {
        let d = self.data();
        let pending: Vec<Value> = snap
            .pending
            .iter()
            .map(|p| {
                let at = &d.acts[p.act];
                let max = d
                    .template(at.quest_id)
                    .and_then(|t| at.max_objective(t))
                    .unwrap_or(-1);
                json!({"act_id": at.act_id, "detail_id": at.detail_id,
                       "class": at.kind.class_name(), "component_id": at.component_id,
                       "objective_index": at.objective_index, "have": p.have,
                       "required": at.count, "max_objective": max})
            })
            .collect();
        let inactive: Vec<Value> = snap
            .inactive
            .iter()
            .map(|(c, r)| json!({"component_id": c, "failing_reqs": r}))
            .collect();
        json!({"active": snap.active, "completed": snap.completed, "step": snap.step,
               "step_name": kind::name(snap.step), "status": snap.status,
               "component_id": snap.component_id,
               "component_kind": kind::name(snap.step),
               "objectives": snap.objectives, "acceptor": [snap.acceptor.0, snap.acceptor.1],
               "pending": pending, "inactive_components": inactive})
    }

    fn play(&mut self, with_reload: bool) {
        // accept
        let starters = self.starters();
        let mut tried = Vec::new();
        let mut accepted = false;
        for (name, s) in &starters {
            if self.attempt(s) {
                self.out.accept_via = name.clone();
                accepted = true;
                break;
            }
            tried.push(name.clone());
        }
        if !accepted {
            self.out.result = "rejected_accept";
            self.out.accept_via = tried.join(", ");
            let qid = self.qid;
            let reasons = events::with_quests(self.server(), &self.player, None, |ctx| {
                let Some(t) = ctx.data.template(qid) else {
                    return Vec::new();
                };
                t.components_of(&ctx.data.components, kind::START)
                    .iter()
                    .filter(|c| !ctx.can_component_run(c.id))
                    .map(|c| json!({"component_id": c.id, "failing_reqs": Self::failing_reqs(ctx, c.id)}))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
            self.out.reject =
                Some(json!({"tried": tried, "start_components_not_running": reasons}));
            return;
        }

        let mut last_sig = None;
        let mut stagnant = 0usize;
        let mut reload_pending = with_reload;
        let mut reload_mid = false;
        for it in 0..MAX_ITERATIONS {
            self.out.iterations = it + 1;
            self.sim.drain_queue();
            let snap = self.snapshot();
            if !snap.active {
                if snap.completed {
                    self.out.result = "completed";
                } else {
                    self.out.result = "stuck";
                    self.out
                        .notes
                        .push("quest left the active list without being completed".into());
                }
                self.out.final_snap = Some(self.describe_snap(&snap));
                return;
            }
            if reload_pending && matches!(snap.step, kind::PROGRESS | kind::READY) {
                let r = self.reload();
                self.out.reload.push(r);
                reload_pending = false;
                reload_mid = true;
                continue;
            }
            let sig = (
                snap.step,
                snap.status,
                snap.objectives,
                snap.pending.len(),
                snap.component_id,
            );
            if last_sig == Some(sig) {
                stagnant += 1;
            } else {
                stagnant = 0;
            }
            last_sig = Some(sig);
            if matches!(snap.step, kind::FAIL | kind::DROP) {
                self.out.result = "stuck";
                self.out.final_snap = Some(self.describe_snap(&snap));
                return;
            }
            if stagnant >= 3 {
                self.finish_stuck(&snap);
                return;
            }
            if stagnant == 1 {
                // Objectives saturated (or nothing moved): a player would now report. This is
                // how early-completable (`let_it_done`) score quests finish.
                let inactive = snap.inactive.clone();
                if self.report(&inactive) {
                    self.sim.drain_queue();
                    continue;
                }
            }
            if stagnant == 2 {
                // Natural evaluations did not advance the quest: tick it once (the engine has
                // no periodic re-evaluation) and record that it was necessary.
                self.out.forced_evaluation = true;
                self.force_evaluation();
                continue;
            }
            match snap.step {
                kind::PROGRESS if !snap.pending.is_empty() => {
                    let mut sent = false;
                    let mut unsupported = 0;
                    let pending: Vec<usize> = snap.pending.iter().map(|p| p.act).collect();
                    let first_step = snap.step;
                    for (i, p) in snap.pending.iter().enumerate() {
                        match self.deliver(p, snap.acceptor) {
                            Delivered::Events(n) => {
                                self.out.events_sent += n;
                                sent = true;
                            }
                            Delivered::Indirect => {}
                            Delivered::Unsupported(r) => {
                                unsupported += 1;
                                self.out.unsupported.insert(format!(
                                    "{}: {r}",
                                    self.data().acts[pending[i]].kind.class_name()
                                ));
                            }
                            Delivered::DataGap(r) => {
                                self.out.data_gaps.insert(format!(
                                    "{}: {r}",
                                    self.data().acts[pending[i]].kind.class_name()
                                ));
                            }
                        }
                        self.sim.drain_queue();
                        if reload_mid {
                            let s = self.snapshot();
                            if s.active && s.step == kind::PROGRESS {
                                let r = self.reload();
                                self.out.reload.push(r);
                                reload_mid = false;
                            }
                        }
                        let s = self.snapshot();
                        if !s.active || s.step != first_step {
                            break;
                        }
                    }
                    if !sent && unsupported == snap.pending.len() {
                        self.out.result = "harness_unsupported";
                        self.out.final_snap = Some(self.describe_snap(&snap));
                        return;
                    }
                }
                kind::PROGRESS | kind::READY => {
                    let inactive = snap.inactive.clone();
                    self.report(&inactive);
                    self.sim.drain_queue();
                }
                _ => {
                    // Start / None / Supply / Reward: only the evaluation queue moves these.
                }
            }
        }
        let snap = self.snapshot();
        self.out
            .notes
            .push(format!("iteration cap {MAX_ITERATIONS} reached"));
        self.finish_stuck(&snap);
    }

    fn finish_stuck(&mut self, snap: &Snap) {
        let only_unsupported = !snap.pending.is_empty()
            && !self.out.unsupported.is_empty()
            && self.out.data_gaps.is_empty()
            && snap.pending.iter().all(|p| {
                let name = self.data().acts[p.act].kind.class_name();
                self.out
                    .unsupported
                    .iter()
                    .any(|u| u.starts_with(&format!("{name}:")))
            });
        self.out.result = if only_unsupported {
            "harness_unsupported"
        } else {
            "stuck"
        };
        self.out.final_snap = Some(self.describe_snap(snap));
    }

    /// Checks the rewards of a completed quest against the hooks' log.
    fn check_rewards(&mut self, log: &RewardLog) {
        let d = self.data();
        let Some(t) = d.template(self.qid) else {
            return;
        };
        let selected = self.selected_reward();
        let mut exp = false;
        let mut copper = false;
        let mut items: Vec<(u32, i32)> = Vec::new();
        let has_aggro = t
            .components
            .iter()
            .filter_map(|c| d.component(*c))
            .flat_map(|c| c.acts.iter())
            .any(|&a| matches!(d.acts[a].kind, ActKind::ObjAggro { .. }));
        for c in t.components_of(&d.components, kind::REWARD) {
            for &a in &c.acts {
                let at = &d.acts[a];
                match at.kind {
                    ActKind::SupplyExp { exp: e } if e > 0 => exp = true,
                    ActKind::SupplyCopper { amount } if amount > 0 => copper = true,
                    ActKind::SupplyItem { item_id, .. } => items.push((item_id, at.count)),
                    ActKind::SupplySelectiveItem { item_id, .. }
                        if at.selective_index == selected =>
                    {
                        items.push((item_id, at.count))
                    }
                    _ => {}
                }
            }
        }
        // The level based exp / copper is added while the Reward step's components run: a quest
        // without a Reward step is not paid it (as in the C#).
        if t.level > 0 && t.first_component(&d.components, kind::REWARD).is_some() {
            if let Some(s) = d.supplies(t.level) {
                exp |= s.exp > 0;
                copper |= s.copper > 0;
            }
        }
        let mailed_copper: i64 = log.mails.iter().map(|m| i64::from(m.1)).sum();
        if exp && log.exp <= 0 {
            self.out.reward_warnings.push("no exp delivered".into());
        }
        if copper && log.copper + mailed_copper <= 0 {
            self.out.reward_warnings.push("no copper delivered".into());
        }
        if !has_aggro {
            for (item, count) in items {
                let got: i32 = log
                    .items
                    .iter()
                    .chain(log.mailed_items.iter())
                    .filter(|(i, _)| *i == item)
                    .map(|(_, n)| *n)
                    .sum();
                if got < count {
                    self.out
                        .reward_warnings
                        .push(format!("reward item {item} x{count}: delivered {got}"));
                }
            }
        }
    }
}

fn have_max(run: &Run<'_>, at: &QuestActTemplate) -> i32 {
    run.data()
        .template(at.quest_id)
        .and_then(|t| at.max_objective(t))
        .unwrap_or(1)
}

#[derive(Clone, Copy)]
enum Starter {
    Npc(u32),
    Doodad(u32),
    Item(u32, i32),
    ItemGain(u32, i32),
    ItemEquip(u32),
    Sphere(u32),
    Skill(u32),
    Buff(u32),
    Level(u8),
    Auto,
    Component,
}

impl Starter {
    fn arg(&self) -> String {
        match *self {
            Starter::Npc(v)
            | Starter::Doodad(v)
            | Starter::ItemEquip(v)
            | Starter::Sphere(v)
            | Starter::Skill(v)
            | Starter::Buff(v) => v.to_string(),
            Starter::Item(v, n) | Starter::ItemGain(v, n) => format!("{v}x{n}"),
            Starter::Level(v) => v.to_string(),
            Starter::Auto | Starter::Component => String::new(),
        }
    }
}

/// Runs one quest on a fresh character. Returns the outcome.
fn run_quest(sim: &Sim, qid: u32, with_reload: bool) -> Outcome {
    sim.hooks.reset();
    take_log();
    let char_id = sim.char_id();
    let mut c = test_character(char_id, "Sim", 100.0, 100.0);
    c.unit.level = 10;
    let fit_notes = sim.fit_character(qid, &mut c);
    let obj_id = 0x1000_0000 + char_id;
    let player = {
        let client = sim.client.borrow();
        spawn_player(&sim.server, &client, c, obj_id)
    };
    let mut run = Run {
        sim,
        qid,
        player,
        char_id,
        spawned: Vec::new(),
        npcs: HashMap::new(),
        doodads: HashMap::new(),
        out: Outcome::default(),
    };
    run.out.notes.extend(fit_notes.clone());
    let r = catch_unwind(AssertUnwindSafe(|| run.play(with_reload)));
    let mut out = std::mem::take(&mut run.out);
    let skip_notes: Vec<&String> = fit_notes
        .iter()
        .filter(|n| !n.starts_with("[self]"))
        .collect();
    if out.result == "rejected_accept" && !skip_notes.is_empty() {
        out.result = "skipped";
        out.skip_reason = format!(
            "requirement: {}",
            skip_notes
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    let log = sim.hooks.snapshot();
    if let Err(payload) = r {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let loc = LAST_PANIC.with(|p| p.borrow().clone());
        out.result = "panic";
        out.notes.push(format!("panic: {msg} @ {loc}"));
        // Cleanup may hit the poisoned locks again; the caller rebuilds the server.
        return out;
    }
    run.out = out;
    if run.out.result == "completed" {
        let player = run.player.clone();
        let completed = events::has_quest_completed_in(&player.data(), qid);
        let active = events::has_quest(&player, qid)
            || player
                .data()
                .quests
                .active
                .iter()
                .any(|q| q.template_id == qid);
        if !completed || active {
            run.out.result = "stuck";
            run.out.notes.push(format!(
                "inconsistent completion: completed flag {completed}, still active {active}"
            ));
        } else {
            run.check_rewards(&log);
        }
    }
    let mut out = std::mem::take(&mut run.out);
    out.notes.extend(
        log_tail(&take_log())
            .into_iter()
            .map(|l| format!("log: {l}")),
    );
    // cleanup
    let cleanup = catch_unwind(AssertUnwindSafe(|| {
        let world = sim.server.world.world(0).expect("instance 0");
        for o in run.spawned.drain(..) {
            world.delete(o);
        }
        sim.server.systems.quests.remove_quest_timer(char_id, 0);
        sim.server
            .systems
            .quests
            .spheres
            .remove_triggers(char_id, 0);
        world.delete(WorldObject::from(run.player.clone()));
        sim.server.world.try_remove_character(obj_id);
    }));
    if cleanup.is_err() {
        out.notes.push("cleanup panicked".into());
    }
    let errors = sim
        .client
        .borrow_mut()
        .drain()
        .iter()
        .filter(|(op, _)| *op == sc::SCErrorMsgPacket)
        .count();
    if errors > 0 {
        out.notes.push(format!("{errors} SCErrorMsgPacket sent"));
    }
    out
}

fn outcome_json(qid: u32, template_level: u8, out: &Outcome) -> Value {
    let snap = out.final_snap.clone().unwrap_or(Value::Null);
    json!({
        "id": qid,
        "level": template_level,
        "result": out.result,
        "accept_via": out.accept_via,
        "step": snap.get("step_name").cloned().unwrap_or(Value::Null),
        "component_id": snap.get("component_id").cloned().unwrap_or(Value::Null),
        "component_kind": snap.get("component_kind").cloned().unwrap_or(Value::Null),
        "pending": snap.get("pending").cloned().unwrap_or(Value::Null),
        "inactive_components": snap.get("inactive_components").cloned().unwrap_or(Value::Null),
        "objectives": snap.get("objectives").cloned().unwrap_or(Value::Null),
        "iterations": out.iterations,
        "events_sent": out.events_sent,
        "forced_evaluation": out.forced_evaluation,
        "harness_unsupported": out.unsupported.iter().collect::<Vec<_>>(),
        "data_gaps": out.data_gaps.iter().collect::<Vec<_>>(),
        "reward_warnings": out.reward_warnings,
        "reload": out.reload,
        "reject": out.reject,
        "reason": out.skip_reason,
        "notes": out.notes,
    })
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_db_quest_lifecycles() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open compact db");
    let only: Option<BTreeSet<u32>> = std::env::var("AAEMU_QUEST_SIM_ONLY")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect());
    let limit: usize = std::env::var("AAEMU_QUEST_SIM_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);

    let subscriber = tracing_subscriber::registry().with(LogCapture.with_filter(LevelFilter::WARN));
    let _guard = tracing::subscriber::set_default(subscriber);
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        LAST_PANIC.with(|p| *p.borrow_mut() = loc);
    }));

    let mut sim = Sim::new(&conn);
    let mut ids: Vec<u32> = sim.data().templates.keys().copied().collect();
    ids.sort_unstable();
    if let Some(only) = &only {
        ids.retain(|i| only.contains(i));
    }
    ids.truncate(limit);

    let started = std::time::Instant::now();
    let mut entries: Vec<Value> = Vec::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut stuck_by_class: BTreeMap<String, usize> = BTreeMap::new();
    let mut stuck_by_step: BTreeMap<String, usize> = BTreeMap::new();
    let mut unsupported_by_class: BTreeMap<String, usize> = BTreeMap::new();
    let mut gap_by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut reject_by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut panics: Vec<String> = Vec::new();
    let mut reload_total = 0usize;
    let mut reload_na = 0usize;
    let mut reload_bad: Vec<Value> = Vec::new();
    let mut completed_count = 0usize;
    let mut rebuilds = 0usize;

    for (n, qid) in ids.iter().copied().enumerate() {
        let (has_start, level) = {
            let d = sim.data();
            let t = d.template(qid).expect("template");
            (
                !t.components_of(&d.components, kind::START).is_empty(),
                t.level,
            )
        };
        if !has_start {
            *counts.entry("skipped".into()).or_default() += 1;
            entries.push(json!({"id": qid, "level": level, "result": "skipped",
                "reason": "auxiliary context: no Start component"}));
            continue;
        }
        let mut out = run_quest(&sim, qid, false);
        if out.result == "completed" {
            completed_count += 1;
            if completed_count.is_multiple_of(10) {
                // Re-run through save -> reload mid-quest.
                let again = run_quest(&sim, qid, true);
                let applicable = !again.reload.is_empty();
                if applicable {
                    reload_total += 1;
                } else {
                    reload_na += 1;
                }
                let ok = again.result == "completed"
                    && (!applicable
                        || again
                            .reload
                            .iter()
                            .all(|r| r.get("ok").and_then(Value::as_bool).unwrap_or(false)));
                if !ok {
                    reload_bad.push(json!({"id": qid, "rerun_result": again.result,
                        "reload": again.reload, "notes": again.notes}));
                }
                out.reload = vec![json!({"rerun_result": again.result, "reloads": again.reload})];
                if again.result == "panic" {
                    sim = Sim::new(&conn);
                    rebuilds += 1;
                }
            }
        }
        *counts.entry(out.result.to_string()).or_default() += 1;
        match out.result {
            "stuck" => {
                let snap = out.final_snap.clone().unwrap_or(Value::Null);
                let step = snap
                    .get("step_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let classes: BTreeSet<String> = snap
                    .get("pending")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| p.get("class").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                *stuck_by_step.entry(step.clone()).or_default() += 1;
                if classes.is_empty() {
                    let inactive = snap
                        .get("inactive_components")
                        .and_then(Value::as_array)
                        .is_some_and(|a| !a.is_empty());
                    let key = if inactive {
                        format!("{step}: inactive component (requirements)")
                    } else {
                        format!("{step}: no pending act")
                    };
                    *stuck_by_class.entry(key).or_default() += 1;
                }
                for c in classes {
                    *stuck_by_class.entry(c).or_default() += 1;
                }
                for g in &out.data_gaps {
                    let key: String = g.chars().take(60).collect();
                    *gap_by_kind.entry(key).or_default() += 1;
                }
            }
            "harness_unsupported" => {
                for u in &out.unsupported {
                    *unsupported_by_class
                        .entry(u.split(':').next().unwrap_or("?").to_string())
                        .or_default() += 1;
                }
            }
            "rejected_accept" => {
                let kinds: BTreeSet<String> = out
                    .reject
                    .as_ref()
                    .and_then(|r| r.get("start_components_not_running"))
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .flat_map(|c| {
                                c.get("failing_reqs")
                                    .and_then(Value::as_array)
                                    .cloned()
                                    .unwrap_or_default()
                            })
                            .filter_map(|v| {
                                v.as_str()
                                    .map(|s| s.split(' ').take(2).collect::<Vec<_>>().join(" "))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if kinds.is_empty() {
                    *reject_by_kind
                        .entry("no failing requirement (acceptor/starter mismatch)".into())
                        .or_default() += 1;
                }
                for k in kinds {
                    *reject_by_kind.entry(k).or_default() += 1;
                }
            }
            "panic" => {
                panics.push(format!(
                    "{qid}: {}",
                    out.notes.last().cloned().unwrap_or_default()
                ));
                sim = Sim::new(&conn);
                rebuilds += 1;
            }
            _ => {}
        }
        entries.push(outcome_json(qid, level, &out));
        if (n + 1) % 500 == 0 {
            eprintln!(
                "quest sim: {}/{} quests, {} completed, {:?} elapsed",
                n + 1,
                ids.len(),
                completed_count,
                started.elapsed()
            );
        }
    }

    std::panic::set_hook(prev_hook);

    let summary = json!({
        "quests": ids.len(),
        "counts": counts,
        "stuck_by_pending_act_class": stuck_by_class,
        "stuck_by_step": stuck_by_step,
        "harness_unsupported_by_class": unsupported_by_class,
        "stuck_data_gaps": gap_by_kind,
        "rejected_accept_by_requirement": reject_by_kind,
        "panics": panics,
        "server_rebuilds": rebuilds,
        "reload_samples": reload_total,
        "reload_not_applicable_completed_without_progress": reload_na,
        "reload_failures": reload_bad,
        "seconds": started.elapsed().as_secs(),
    });
    eprintln!(
        "quest lifecycle summary: {}",
        serde_json::to_string_pretty(&summary).unwrap()
    );
    if let Ok(out_path) = std::env::var("AAEMU_QUEST_SIM_OUT") {
        let p = std::path::Path::new(&out_path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create output dir");
        }
        let ledger = json!({"summary": summary, "quests": entries});
        std::fs::write(p, serde_json::to_vec_pretty(&ledger).unwrap()).expect("write ledger");
        eprintln!("quest lifecycle ledger written to {out_path}");
    }
    assert!(
        counts.get("completed").copied().unwrap_or(0) > 0 || only.is_some() || limit < 10,
        "the simulator completed no quest at all: harness broken"
    );
}
