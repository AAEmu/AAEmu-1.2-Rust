//! Aura and crowd buffs.
//!
//! The 1.2 client only loads the `buffs` columns (`aura_child_only`, `aura_radius`,
//! `aura_relation_id`, `aura_slave_buff_id`, `crowd_*`); no 1.2 zone-server binary exists, so
//! the runtime below is reconstructed from the localized descriptions of the data
//! (research `D:/aaemu-audits/gap-audit-2026-11-02/research/aura-crowd.md`). Every rule is
//! capture-pending against a retail 1.2 zone server:
//!
//! * AURA-1: an aura buff with `aura_radius > 0` and an existing `aura_slave_buff_id` grants
//!   that slave buff to every unit within the radius that passes `aura_relation_id`
//!   (`FilterWithRelation`, relative to the aura owner); the slave buff's caster is the owner.
//! * AURA-2: the owner itself is a recipient for Any / Friendly / Party / Raid (the
//!   descriptions say "caster and allies"); whether 1.2 does this is inferred.
//! * AURA-3: a recipient leaving the radius or failing the relation loses only the copy applied
//!   by this aura (matched by template id and caster obj id, so a copy from another caster
//!   survives). Every tick also renews a current member that holds no live copy from ANY caster
//!   (expired, dispelled, dropped on death, or its caster left); it does not add while another
//!   caster's copy exists, so two owners do not refresh each other forever. A copy whose caster
//!   was overwritten by a REFRESH stack rule is re-added by the remaining owner within one tick
//!   after the overwriting owner leaves.
//! * AURA-4: the aura is evaluated on a 1000 ms enter/leave diff (no movement hook); the 1.2
//!   update period is UNKNOWN.
//! * AURA-5: `aura_child_only` reaches only the owner's attached characters (slave
//!   `attached_characters`, mate `passengers`) plus the owner's mount/vehicle (the transform
//!   parent) within the radius. Which attach scopes 1.2 uses is UNKNOWN.
//! * AURA-6: the owner losing the aura buff strips every recipient at once; a logout / despawn
//!   that leaves no live object is detected by the next driver tick and strips the stored
//!   recipients without a radius query. A dead owner whose aura buff lacks `remove_on_death`
//!   keeps its aura (UNKNOWN in 1.2).
//! * AURA-7: a dangling slave id (6668 -> 6669), a self-referencing slave (1224 -> 1224) and an
//!   aura without a slave buff (15 rows) are skipped and reported once at load.
//! * AURA-8: an entry is dropped unless the same owner object still holds a live source effect
//!   (object identity + effect check), so a recycled object id or a removal that raced the
//!   registration cannot leave a ghost aura. Recipients that are dead do not get the slave
//!   buff re-added unless it is `dead_applicable`.
//! * CROWD-1: a unit carrying a buff with `crowd_buff_id > 0` gets that buff (self-cast) while
//!   at least `crowd_number` OTHER units carry the same source buff within `crowd_radius` and
//!   match `crowd_friendly` / `crowd_hostile`; the crowd buff is removed when the count drops.
//!   Whether 1.2 includes the owner in the count is UNKNOWN (the data text suggests it does;
//!   the count here follows the task decision and excludes the owner).
//! * CROWD-2: `crowd_number <= 0` never satisfies the threshold.
//! * CROWD-3: a unit counts as friendly when `GetRelationStateTo` is Friendly and it cannot be
//!   attacked, as hostile when `CanAttack` is true; with both flags set either relation counts,
//!   with both clear nothing counts. Which relation 1.2 counts is UNKNOWN.
//! * CROWD-4: the crowd buff is removed by (template id, caster = owner), like the aura slave.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::models::buff::Buff;
use crate::server::GameServer;
use crate::world::WorldObject;

use super::data::BuffTemplate;
use super::geometry::Target;
use super::types::{RelationState, SkillTargetRelation};
use super::{buffs, geometry, relation};

/// How often aura/crowd owners are re-evaluated (AURA-4, CROWD-1).
const TICK_MS: u64 = 1000;

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
struct Entry {
    owner_obj_id: u32,
    /// Identity of the owner object (`ObjectCore` address), so a recycled obj id cannot keep a
    /// ghost aura alive (AURA-8).
    owner_core: usize,
    /// The aura / crowd source buff on the owner.
    buff_id: u32,
    /// Distinguishes a re-registered entry from an older tick's snapshot.
    generation: u64,
    /// The owner's transform instance at the last evaluation.
    instance_id: u32,
    /// Units that currently hold the slave buff applied by this aura.
    recipients: HashSet<u32>,
    /// The slave buff (`0` = the source buff is only a crowd source).
    slave_buff_id: u32,
    aura_radius: f32,
    aura_relation: SkillTargetRelation,
    aura_child_only: bool,
    crowd_buff_id: u32,
    crowd_radius: f32,
    crowd_number: i32,
    crowd_friendly: bool,
    crowd_hostile: bool,
    /// The crowd buff was applied by this entry and has not been removed.
    crowd_active: bool,
}

/// Aura/crowd driver state on `server.systems.skills`.
#[derive(Debug, Default)]
pub struct AuraState {
    entries: Mutex<HashMap<(u32, u32), Entry>>,
}

impl AuraState {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(u32, u32), Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn snapshot(&self) -> Vec<((u32, u32), Entry)> {
        self.lock().iter().map(|(k, e)| (*k, e.clone())).collect()
    }

    fn set_recipients(&self, key: (u32, u32), generation: u64, recipients: HashSet<u32>) -> bool {
        let mut map = self.lock();
        match map.get_mut(&key) {
            Some(e) if e.generation == generation => {
                e.recipients = recipients;
                true
            }
            _ => false,
        }
    }

    fn set_instance(&self, key: (u32, u32), generation: u64, instance_id: u32) -> bool {
        let mut map = self.lock();
        match map.get_mut(&key) {
            Some(e) if e.generation == generation => {
                e.instance_id = instance_id;
                e.recipients.clear();
                true
            }
            _ => false,
        }
    }

    fn set_crowd_active(&self, key: (u32, u32), generation: u64, active: bool) -> bool {
        let mut map = self.lock();
        match map.get_mut(&key) {
            Some(e) if e.generation == generation => {
                e.crowd_active = active;
                true
            }
            _ => false,
        }
    }

    fn remove(&self, key: (u32, u32), generation: u64) -> Option<Entry> {
        let mut map = self.lock();
        match map.get(&key) {
            Some(e) if e.generation == generation => map.remove(&key),
            _ => None,
        }
    }

    /// Registered aura/crowd owners (tests / diagnostics).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no aura/crowd owner is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Entry {
    fn new(
        owner_obj_id: u32,
        owner_core: usize,
        buff_id: u32,
        instance_id: u32,
        t: &BuffTemplate,
        has_slave: bool,
    ) -> Self {
        Self {
            owner_obj_id,
            owner_core,
            buff_id,
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            instance_id,
            recipients: HashSet::new(),
            slave_buff_id: if has_slave { t.aura_slave_buff_id } else { 0 },
            aura_radius: t.aura_radius as f32,
            aura_relation: SkillTargetRelation::from_raw(i64::from(t.aura_relation_id))
                .unwrap_or(SkillTargetRelation::Any),
            aura_child_only: t.aura_child_only,
            crowd_buff_id: t.crowd_buff_id,
            crowd_radius: t.crowd_radius,
            crowd_number: t.crowd_number,
            crowd_friendly: t.crowd_friendly,
            crowd_hostile: t.crowd_hostile,
            crowd_active: false,
        }
    }

    fn is_aura(&self) -> bool {
        self.slave_buff_id != 0
    }

    fn is_crowd(&self) -> bool {
        self.crowd_buff_id != 0
    }
}

/// Registers the owner when a buff that drives an aura / crowd effect starts (AURA-1, CROWD-1).
pub fn on_buff_started(server: &GameServer, owner: &WorldObject, b: &Buff) {
    if !owner.is_unit() {
        return;
    }
    let id = b.template.id;
    let Some(t) = server.game_data.systems.skills.buff(id) else {
        return;
    };
    let has_slave = usable_slave(t, |slave| {
        server.game_data.systems.skills.buff(slave).is_some()
    }) != 0;
    let is_crowd = usable_crowd(t, |crowd| {
        server.game_data.systems.skills.buff(crowd).is_some()
    });
    if !has_slave && !is_crowd {
        return;
    }
    // A Started event queued before a concurrent removal can arrive after the effect is gone;
    // do not register a ghost entry for it (AURA-8).
    let holds = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .any(|e| !e.is_ended() && e.template.id == id)
    });
    if !holds {
        return;
    }
    let key = (owner.obj_id(), id);
    // Read the transform before taking the registry lock, so the registry is never held while
    // an object's data is locked.
    let instance_id = owner.transform().instance_id;
    let owner_core = owner.core() as *const _ as usize;
    let mut map = server.systems.skills.aura.lock();
    map.entry(key)
        .or_insert_with(|| Entry::new(owner.obj_id(), owner_core, id, instance_id, t, has_slave));
}

/// Unregisters the owner and strips what this aura / crowd effect applied (AURA-3, AURA-6,
/// CROWD-4). Runs after the effect has left the owner's buff list.
pub fn on_buff_removed(server: &GameServer, owner: &WorldObject, b: &Buff) {
    let key = (owner.obj_id(), b.template.id);
    // The generation is read in its own statement: a guard created in a `let` initializer lives
    // until the end of the statement and would deadlock the `remove` below.
    let generation = server
        .systems
        .skills
        .aura
        .lock()
        .get(&key)
        .map_or(0, |e| e.generation);
    let Some(entry) = server.systems.skills.aura.remove(key, generation) else {
        return;
    };
    strip_recipients(server, &entry);
    if entry.is_crowd() {
        remove_owned(server, owner, entry.crowd_buff_id, entry.owner_obj_id);
    }
}

/// The slave buff an aura may apply, or `0` for a row the catalog skips (AURA-7): no radius,
/// no slave, a self-reference, or a slave buff absent from the catalog.
pub(crate) fn usable_slave(t: &BuffTemplate, exists: impl Fn(u32) -> bool) -> u32 {
    if t.aura_radius > 0
        && t.aura_slave_buff_id > 0
        && t.aura_slave_buff_id != t.id
        && exists(t.aura_slave_buff_id)
    {
        t.aura_slave_buff_id
    } else {
        0
    }
}

/// Whether the crowd buff of a source row exists in the catalog.
pub(crate) fn usable_crowd(t: &BuffTemplate, exists: impl Fn(u32) -> bool) -> bool {
    t.crowd_buff_id > 0 && exists(t.crowd_buff_id)
}

/// The periodic driver (AURA-4, CROWD-1).
pub fn start_ticker(server: &Arc<GameServer>) {
    let weak = Arc::downgrade(server);
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(TICK_MS));
        loop {
            iv.tick().await;
            let Some(s) = weak.upgrade() else { break };
            crate::systems::run_blocking(&s, tick).await;
        }
    });
}

/// One evaluation of every registered aura / crowd owner.
pub fn tick(server: &GameServer) {
    for (key, entry) in server.systems.skills.aura.snapshot() {
        step(server, key, entry);
    }
}

fn step(server: &GameServer, key: (u32, u32), mut entry: Entry) {
    let Some(owner) = buffs::find_obj(server, entry.owner_obj_id) else {
        drop_entry(server, key, &entry);
        return;
    };
    if !geometry::is_live_object(server, &owner) {
        drop_entry(server, key, &entry);
        return;
    }
    // A recycled obj id or a removal that raced the registration must not keep a ghost aura.
    if owner.core() as *const _ as usize != entry.owner_core {
        drop_entry(server, key, &entry);
        return;
    }
    let holds_source = owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .any(|e| !e.is_ended() && e.template.id == entry.buff_id)
    });
    if !holds_source {
        drop_entry(server, key, &entry);
        return;
    }
    // Leaving the world / a teleport to another instance strips the stored recipients at once
    // instead of waiting for a radius query in the old world (AURA-6).
    let instance_id = owner.transform().instance_id;
    if entry.instance_id != instance_id {
        strip_recipients(server, &entry);
        entry.recipients.clear();
        entry.instance_id = instance_id;
        if !server
            .systems
            .skills
            .aura
            .set_instance(key, entry.generation, instance_id)
        {
            return;
        }
    }
    if entry.is_aura() {
        let current = aura_targets(server, &owner, &entry);
        sync_recipients(server, key, &entry, &owner, current);
    }
    if entry.is_crowd() {
        step_crowd(server, key, &entry, &owner);
    }
}

/// The units the aura should reach this tick (AURA-1, AURA-2, AURA-5).
fn aura_targets(server: &GameServer, owner: &WorldObject, entry: &Entry) -> HashSet<u32> {
    let mut units: Vec<WorldObject> = geometry::around_radius(
        server,
        &Target::Obj(owner.clone()),
        entry.aura_radius,
        false,
    )
    .into_iter()
    .filter(WorldObject::is_unit)
    .collect();
    if entry.aura_child_only {
        let attached = attached_unit_ids(owner);
        units.retain(|u| attached.contains(&u.obj_id()));
    }
    let mut units = relation::filter_with_relation(
        server,
        entry.aura_relation,
        &Target::Obj(owner.clone()),
        units,
    );
    if admits_self(entry.aura_relation) && !units.iter().any(|u| u.same(owner)) {
        units.push(owner.clone());
    }
    units.into_iter().map(|u| u.obj_id()).collect()
}

/// `aura_child_only`: the owner's attached characters and the owner's mount/vehicle (AURA-5).
fn attached_unit_ids(owner: &WorldObject) -> Vec<u32> {
    let mut out = Vec::new();
    match owner {
        WorldObject::Slave(s) => {
            out.extend(s.data().attached_characters.iter().map(|(_, id)| *id));
        }
        WorldObject::Mate(m) => {
            out.extend(m.data().passengers.iter().map(|(_, p)| p.obj_id));
        }
        _ => {}
    }
    let parent = owner.transform().parent_obj_id;
    if parent != 0 {
        out.push(parent);
    }
    out
}

/// The relations whose `FilterWithRelation` result contains the caster itself (AURA-2).
fn admits_self(relation: SkillTargetRelation) -> bool {
    matches!(
        relation,
        SkillTargetRelation::Any
            | SkillTargetRelation::Friendly
            | SkillTargetRelation::Party
            | SkillTargetRelation::Raid
    )
}

fn sync_recipients(
    server: &GameServer,
    key: (u32, u32),
    entry: &Entry,
    owner: &WorldObject,
    current: HashSet<u32>,
) {
    // Leave: only this aura's copy goes (AURA-3).
    let removed: Vec<u32> = entry.recipients.difference(&current).copied().collect();
    for id in removed {
        if let Some(obj) = buffs::find_obj(server, id) {
            remove_owned(server, &obj, entry.slave_buff_id, entry.owner_obj_id);
        }
    }
    // Every current member needs a live copy, not merely a member that just entered: expiry,
    // dispels, death cleanup and a leaving caster all drop it while the member stays in range.
    // A copy from any caster counts, so two owners covering the same unit do not overwrite each
    // other every tick (REFRESH stack rule would move the caster back and forth).
    let dead_applicable = server
        .game_data
        .systems
        .skills
        .buff(entry.slave_buff_id)
        .is_some_and(|t| t.dead_applicable);
    let mut applied = Vec::new();
    for id in &current {
        let Some(obj) = buffs::find_obj(server, *id) else {
            continue;
        };
        if has_live_buff(&obj, entry.slave_buff_id) {
            continue;
        }
        if !dead_applicable && obj.with_unit(|u| u.unit().hp <= 0).unwrap_or(false) {
            continue;
        }
        buffs::add_buff_by_id(server, &obj, entry.slave_buff_id, owner, 0);
        applied.push(*id);
    }
    // The entry can be unregistered (or re-registered) while the buffs above start. Commit only
    // onto the same generation; otherwise the additions above belong to an ended aura.
    if !server
        .systems
        .skills
        .aura
        .set_recipients(key, entry.generation, current)
    {
        for id in applied {
            if let Some(obj) = buffs::find_obj(server, id) {
                remove_owned(server, &obj, entry.slave_buff_id, entry.owner_obj_id);
            }
        }
    }
}

fn step_crowd(server: &GameServer, key: (u32, u32), entry: &Entry, owner: &WorldObject) {
    let should = entry.crowd_number > 0 && crowd_count(server, entry, owner) >= entry.crowd_number;
    let has = owned_effect_index(owner, entry.crowd_buff_id, entry.owner_obj_id).is_some();
    if should && !has {
        buffs::add_buff_by_id(server, owner, entry.crowd_buff_id, owner, 0);
        if !server
            .systems
            .skills
            .aura
            .set_crowd_active(key, entry.generation, true)
        {
            remove_owned(server, owner, entry.crowd_buff_id, entry.owner_obj_id);
        }
    } else if !should && has {
        remove_owned(server, owner, entry.crowd_buff_id, entry.owner_obj_id);
        server
            .systems
            .skills
            .aura
            .set_crowd_active(key, entry.generation, false);
    } else if entry.crowd_active != should {
        server
            .systems
            .skills
            .aura
            .set_crowd_active(key, entry.generation, should);
    }
}

/// Other units within `crowd_radius` carrying the same source buff and matching the crowd
/// relation flags (CROWD-1, CROWD-3).
fn crowd_count(server: &GameServer, entry: &Entry, owner: &WorldObject) -> i32 {
    if !entry.crowd_friendly && !entry.crowd_hostile {
        return 0;
    }
    let me = Target::Obj(owner.clone());
    let mut count = 0;
    for unit in geometry::around_radius(
        server,
        &Target::Obj(owner.clone()),
        entry.crowd_radius,
        false,
    )
    .into_iter()
    .filter(WorldObject::is_unit)
    {
        if unit.same(owner) {
            continue;
        }
        let carries = unit.with_base_unit(|b| b.buffs().check_buff(entry.buff_id));
        if !carries {
            continue;
        }
        let target = Target::Obj(unit);
        let relation = relation::relation_to(server, &me, &target);
        let friendly = entry.crowd_friendly
            && relation == RelationState::Friendly
            && !relation::can_attack(server, &me, &target);
        let hostile = entry.crowd_hostile && relation::can_attack(server, &me, &target);
        if friendly || hostile {
            count += 1;
        }
    }
    count
}

/// Whether the unit holds a live copy of `buff_id` from any caster (AURA-3).
fn has_live_buff(owner: &WorldObject, buff_id: u32) -> bool {
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .any(|e| !e.is_ended() && e.template.id == buff_id)
    })
}

/// The index of the effect with `buff_id` whose caster is `caster_obj_id`.
fn owned_effect_index(owner: &WorldObject, buff_id: u32, caster_obj_id: u32) -> Option<u32> {
    owner.with_base_unit(|b| {
        b.buffs()
            .effects
            .iter()
            .find(|e| !e.is_ended() && e.template.id == buff_id && e.caster_obj_id == caster_obj_id)
            .map(|e| e.index)
    })
}

/// Removes only the copy this owner applied (AURA-3, CROWD-4).
fn remove_owned(server: &GameServer, owner: &WorldObject, buff_id: u32, caster_obj_id: u32) {
    if buff_id == 0 {
        return;
    }
    if let Some(index) = owned_effect_index(owner, buff_id, caster_obj_id) {
        buffs::exit(server, owner, index, false);
    }
}

fn strip_recipients(server: &GameServer, entry: &Entry) {
    if !entry.is_aura() {
        return;
    }
    for id in &entry.recipients {
        if let Some(obj) = buffs::find_obj(server, *id) {
            remove_owned(server, &obj, entry.slave_buff_id, entry.owner_obj_id);
        }
    }
}

fn drop_entry(server: &GameServer, key: (u32, u32), entry: &Entry) {
    strip_recipients(server, entry);
    server.systems.skills.aura.remove(key, entry.generation);
}

/// Reports the data quirks once at load (AURA-7); a bad row is skipped, never fatal.
pub(crate) fn warn_catalog(buffs: &HashMap<u32, BuffTemplate>) {
    let mut no_slave = Vec::new();
    let mut self_ref = Vec::new();
    let mut dangling = Vec::new();
    for t in buffs.values() {
        if t.aura_radius <= 0 {
            continue;
        }
        if t.aura_slave_buff_id == 0 {
            no_slave.push(t.id);
        } else if t.aura_slave_buff_id == t.id {
            self_ref.push(t.id);
        } else if !buffs.contains_key(&t.aura_slave_buff_id) {
            dangling.push((t.id, t.aura_slave_buff_id));
        }
    }
    let mut crowd_dangling: Vec<(u32, u32)> = buffs
        .values()
        .filter(|t| t.crowd_buff_id > 0 && !buffs.contains_key(&t.crowd_buff_id))
        .map(|t| (t.id, t.crowd_buff_id))
        .collect();
    if no_slave.is_empty()
        && self_ref.is_empty()
        && dangling.is_empty()
        && crowd_dangling.is_empty()
    {
        return;
    }
    no_slave.sort_unstable();
    self_ref.sort_unstable();
    dangling.sort_unstable();
    crowd_dangling.sort_unstable();
    tracing::warn!(
        no_slave = no_slave.len(),
        self_ref = ?&self_ref[..self_ref.len().min(5)],
        dangling = ?&dangling[..dangling.len().min(5)],
        crowd_dangling = ?&crowd_dangling[..crowd_dangling.len().min(5)],
        "aura/crowd buffs with unusable slave targets are skipped (see PORTING.md AURA-7)"
    );
}

#[cfg(test)]
#[path = "aura_tests.rs"]
mod aura_tests;
