//! "Quest Details Map Display" range. **Inferred (no 1.2 server evidence):** the 1.2 server never
//! streams units beyond the ordinary 2-region neighbourhood; doing it for a per-character range
//! is a Rust-only deviation (`PORTING.md`). The 1.2 client's given-quest map markers are
//! produced per unit by `X2::GameClient::ClientUnit::UpdateQuestMark` (x2game.dll, r208022
//! dump, VA `0x39359C60`; the event `0x34` is raised by the function at VA `0x39359E90`), so a
//! unit the client was never sent has no mark. 1.2 has no client-side distance option, so the
//! range is widened by streaming safely quest-relevant NPCs further than the ordinary 2-region
//! neighbourhood.
//!
//! The 3.5 option semantics (`option_map_given_quest_distance`, slot * 100 m, 4.0 =
//! unlimited) come from a 10.x decompile that cannot be checked here (only the option name is
//! verifiable, in the 10.x client's strings); they are reproduced as a per-character range.
//! Quest objectives of accepted quests are already map-wide (`X2Map:AddNotifyQuestInfo` reads
//! the static quest data); this module only adds quest givers / reporters.
//!
//! The range is not persisted server-side: the client stores the option value and the injected
//! driver sends `/questmap <m|0>` after entering the world and when the option is saved.
//!
//! What is streamed: the [`MAX_EXTRA_NPCS`] nearest candidates (a nearer one replaces the
//! farthest streamed one, `select_extras`), chosen among the nearest `MAX_EXTRA_NPCS` +
//! `SPARE_PLACEMENTS` placements outside the player's own neighbourhood. The tracked set
//! (`PlayerState::extra`) is what the server believes the client has, and the client loses
//! units behind its back, so it is corrected from three sides: the region visibility path logs
//! every NPC unit it removes from this client (`Player::note_region_removed_npc`) and each refresh
//! applies that log first and runs at once when it is not empty; [`State::client_world_reset`]
//! forgets the whole set when the client emptied its world (cut-scene end, `SCLoadInstance`,
//! teleport ended); and an NPC that retires between a refresh's packets and its commit, when its
//! retirement hook had nothing to remove yet, is looked at again once it is tracked
//! (`is_live_marker`).
//!
//! Locking: `State::players` is the leaf (`CONTRIBUTING-PORT.md` rule 5): it is taken only to
//! snapshot or commit and never held across a world, spawner or packet call.
//! `State::refreshing` is not a state mutex but an outer serialisation token (the pattern of
//! the spawn manager's reentrant spawn lock): a refresh, a range change and a client world
//! reset hold it across their world work so they cannot interleave. It is taken before
//! `players`, never while a manager lock or the spawn `operation_scope` is held, and never from
//! a world or spawner path (`forget_npc` deliberately does not take it).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::server::GameServer;
use crate::systems::npcs::spawn_manager;
use crate::world::visibility;
use crate::world::{NpcRef, PlayerRef, WorldInstance, WorldObject};

/// How often a tracked player is refreshed while moving.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Movement that forces an immediate refresh even inside [`REFRESH_INTERVAL`].
const MOVE_EPSILON_SQ: f32 = 64.0; // 8 m
/// Hard cap of extra marker NPCs streamed per character: the nearest ones, a nearer NPC replaces
/// a farther streamed one. Keeps the Unlimited option bounded (the 1.2 world has about two
/// thousand quest giver placements).
pub const MAX_EXTRA_NPCS: usize = 256;
/// Placements looked at beyond the NPC cap. The cap counts NPCs and a placement can have none
/// right now (creation budget, respawn delay, schedule), so a few spare placements keep the
/// streamed set full. The player's own neighbourhood is not part of the list.
const SPARE_PLACEMENTS: usize = 8;
/// NPC creations per refresh across all tracked players (one quest-map tick).
pub const SPAWN_BUDGET_PER_TICK: i64 = 32;
/// Minimum time between two `/questmap` changes of one character that refresh at once.
const MIN_RANGE_CHANGE_INTERVAL: Duration = Duration::from_secs(1);

/// The four option steps. Values match the client option item (0..3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Range {
    #[default]
    Meters100,
    Meters200,
    Meters300,
    Unlimited,
}

impl Range {
    /// `option_map_given_quest_distance` slot: 0 = 100 m, 1 = 200 m, 2 = 300 m, 3 = unlimited.
    pub fn from_slot(slot: u32) -> Option<Self> {
        match slot {
            0 => Some(Self::Meters100),
            1 => Some(Self::Meters200),
            2 => Some(Self::Meters300),
            3 => Some(Self::Unlimited),
            _ => None,
        }
    }

    /// The `/questmap` argument: metres, or 0 for unlimited.
    pub fn from_meters(meters: u32) -> Option<Self> {
        match meters {
            100 => Some(Self::Meters100),
            200 => Some(Self::Meters200),
            300 => Some(Self::Meters300),
            0 => Some(Self::Unlimited),
            _ => None,
        }
    }

    pub fn slot(self) -> u32 {
        match self {
            Self::Meters100 => 0,
            Self::Meters200 => 1,
            Self::Meters300 => 2,
            Self::Unlimited => 3,
        }
    }

    pub fn meters(self) -> u32 {
        match self {
            Self::Meters100 => 100,
            Self::Meters200 => 200,
            Self::Meters300 => 300,
            Self::Unlimited => 0,
        }
    }

    /// The extra (beyond the ordinary region neighbourhood) radius; infinite for Unlimited.
    fn radius(self) -> f32 {
        match self {
            Self::Meters100 => 100.0,
            Self::Meters200 => 200.0,
            Self::Meters300 => 300.0,
            Self::Unlimited => f32::INFINITY,
        }
    }

    fn is_default(self) -> bool {
        self == Self::Meters100
    }
}

#[derive(Debug)]
struct PlayerState {
    range: Range,
    /// NPCs streamed only because of this option (never naturally visible) and believed to be
    /// on the client; the module doc says how it is kept in step with the client.
    extra: HashMap<u32, NpcRef>,
    last_refresh: Instant,
    last_pos: (f32, f32),
}

/// Test-only knobs of [`State`].
#[cfg(test)]
#[derive(Default)]
struct TestSeams {
    /// Replaces [`MAX_EXTRA_NPCS`] while it is not 0.
    cap: std::sync::atomic::AtomicUsize,
    /// Runs once right before a refresh commits: what happens between its adds and its commit.
    before_commit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[cfg(test)]
impl std::fmt::Debug for TestSeams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestSeams").finish_non_exhaustive()
    }
}

/// Per-character quest-map range state (`server.systems.quests.quest_map`).
#[derive(Debug, Default)]
pub struct State {
    players: Mutex<HashMap<u32, PlayerState>>,
    /// Serializes a snapshot + world work + commit with range changes. A refresh holds this,
    /// takes `players` only to snapshot / commit, releases it before any world call and never
    /// takes this lock from inside a world / spawner path.
    refreshing: Mutex<()>,
    /// When each character last had a range change refreshed at once (see [`set_range`]). A leaf
    /// like `players`, never held together with it; it outlives the `players` entry, which is
    /// dropped at the default range, and only holds changes younger than the interval.
    range_changes: Mutex<HashMap<u32, Instant>>,
    #[cfg(test)]
    seams: TestSeams,
}

impl State {
    /// Whether a range change of `character_id` may refresh at once: at most one per
    /// [`MIN_RANGE_CHANGE_INTERVAL`]. Records the change when it is allowed.
    fn admit_range_change(&self, character_id: u32, now: Instant) -> bool {
        let mut changes = self.range_changes.lock().expect("quest map state poisoned");
        match changes.get(&character_id) {
            Some(&at) if now.saturating_duration_since(at) < MIN_RANGE_CHANGE_INTERVAL => false,
            _ => {
                changes.insert(character_id, now);
                true
            }
        }
    }

    /// Forgets the range changes that are old enough not to limit anything any more.
    fn prune_range_changes(&self, now: Instant) {
        self.range_changes
            .lock()
            .expect("quest map state poisoned")
            .retain(|_, at| now.saturating_duration_since(*at) < MIN_RANGE_CHANGE_INTERVAL);
    }

    /// The most marker NPCs one character streams ([`MAX_EXTRA_NPCS`]; tests lower it).
    fn cap(&self) -> usize {
        #[cfg(test)]
        {
            let lowered = self.seams.cap.load(std::sync::atomic::Ordering::Relaxed);
            if lowered != 0 {
                return lowered;
            }
        }
        MAX_EXTRA_NPCS
    }

    /// Lowers [`State::cap`] (tests).
    #[cfg(test)]
    fn set_cap(&self, cap: usize) {
        self.seams
            .cap
            .store(cap, std::sync::atomic::Ordering::Relaxed);
    }

    /// Runs `hook` once, right before the next refresh commits (tests).
    #[cfg(test)]
    fn before_next_commit(&self, hook: impl FnOnce() + Send + 'static) {
        *self.seams.before_commit.lock().expect("test seam poisoned") = Some(Box::new(hook));
    }

    pub fn range_of(&self, character_id: u32) -> Range {
        self.players
            .lock()
            .expect("quest map state poisoned")
            .get(&character_id)
            .map_or(Range::Meters100, |p| p.range)
    }

    pub fn tracked_count(&self) -> usize {
        self.players.lock().expect("quest map state poisoned").len()
    }

    /// Drops a character without sending packets (it is leaving the world).
    pub fn forget(&self, character_id: u32) {
        self.players
            .lock()
            .expect("quest map state poisoned")
            .remove(&character_id);
    }

    /// The client emptied its world (cut-scene end, `SCLoadInstance`, teleport ended) or refused
    /// our unit states while a cut scene played: nothing streamed through this option is on its
    /// screen any more, although the tracked set still claims it is. Forgets the character's
    /// streamed set without sending anything and makes the next tick stream it again.
    ///
    /// Call it only where no manager lock / spawn `operation_scope` is held (the packet handlers
    /// of those events): it waits for the refresh in flight, whose commit would otherwise write
    /// the old set back, so it takes [`State::refreshing`] before [`State::players`].
    pub fn client_world_reset(&self, character_id: u32) {
        let tracked = self
            .players
            .lock()
            .expect("quest map state poisoned")
            .contains_key(&character_id);
        if !tracked {
            // The default range streams nothing: no wait on the refresh token for everyone else.
            return;
        }
        let _serial = self.refreshing.lock().expect("quest map state poisoned");
        if let Some(ps) = self
            .players
            .lock()
            .expect("quest map state poisoned")
            .get_mut(&character_id)
        {
            ps.extra.clear();
            ps.last_refresh = overdue(spawn_manager::now());
        }
    }

    /// Obj ids of the extra marker NPCs currently streamed by any tracked player. The spawn
    /// activation sweep keeps their placements warm instead of reclaiming and rebuilding them
    /// every retention period.
    pub fn streamed_npc_ids(&self) -> HashSet<u32> {
        self.players
            .lock()
            .expect("quest map state poisoned")
            .values()
            .flat_map(|p| p.extra.keys().copied())
            .collect()
    }

    /// Test guard against the 2026-11-11 lock cycle: a world call must never run while the
    /// state mutex is held. `try_lock` re-entered from the owning thread fails, so a caller
    /// that kept the guard across world work trips this.
    #[cfg(test)]
    fn assert_unlocked(&self) {
        assert!(
            self.players.try_lock().is_ok(),
            "quest map state lock held across a world call"
        );
    }

    /// Forgets `npc` for every player that streamed it only through this option and sends
    /// those players the unit removal. The ordinary removal broadcast reaches only the NPC's
    /// region neighbourhood; a far quest-map viewer would otherwise keep a ghost unit / marker.
    pub fn forget_npc(&self, server: &GameServer, npc: &NpcRef) {
        let mut targets: Vec<u32> = Vec::new();
        {
            let mut players = self.players.lock().expect("quest map state poisoned");
            for (&character_id, ps) in players.iter_mut() {
                if ps
                    .extra
                    .get(&npc.obj_id)
                    .is_some_and(|tracked| Arc::ptr_eq(tracked, npc))
                {
                    ps.extra.remove(&npc.obj_id);
                    targets.push(character_id);
                }
            }
        }
        if targets.is_empty() {
            return;
        }
        let obj = WorldObject::Npc(npc.clone());
        let Some(world) = server.world.world_of(&obj) else {
            return;
        };
        for character_id in targets {
            if let Some(player) = server.world.character_by_id(character_id) {
                visibility::remove_visible_object(&world, &obj, &player);
            }
        }
    }
}

/// A refresh time that is already overdue, so the next refresh runs at once. `checked_sub`:
/// a monotonic clock younger than the interval cannot be moved back that far (it would panic).
fn overdue(now: Instant) -> Instant {
    now.checked_sub(REFRESH_INTERVAL * 2).unwrap_or(now)
}

fn new_player_state(range: Range, now: Instant) -> PlayerState {
    PlayerState {
        range,
        extra: HashMap::new(),
        last_refresh: overdue(now),
        last_pos: (f32::NAN, f32::NAN),
    }
}

/// `/questmap <100|200|300|0>`: changes the range and applies it immediately. Returns the range
/// that was in force.
///
/// Any player can send the command (level 0) and the driver sends it on every world entry and
/// options-window init, so it is cheap to repeat: the range already in force does nothing. A
/// change to a wider range refreshes at once only if the character's previous such change is at
/// least [`MIN_RANGE_CHANGE_INTERVAL`] old; a faster one is recorded and applied by the next
/// tick, so the last value still wins while a client that spams the command cannot make the
/// server rebuild its marker set per packet. Going back to the default only removes what was
/// streamed and is always applied at once.
pub fn set_range(server: &GameServer, player: &PlayerRef, range: Range) -> Range {
    let state = &server.systems.quests.quest_map;
    let _serial = state.refreshing.lock().expect("quest map state poisoned");
    let previous = state.range_of(player.character_id);
    if previous == range {
        return previous;
    }
    let now = spawn_manager::now();
    let at_once = range.is_default() || state.admit_range_change(player.character_id, now);
    {
        let mut players = state.players.lock().expect("quest map state poisoned");
        let entry = players
            .entry(player.character_id)
            .or_insert_with(|| new_player_state(range, now));
        entry.range = range;
        entry.last_refresh = overdue(now);
    }
    if at_once {
        refresh_player_locked(server, player, SPAWN_BUDGET_PER_TICK);
        if range.is_default() {
            // Nothing can be extra at 100 m; the removal pass above has run.
            state
                .players
                .lock()
                .expect("quest map state poisoned")
                .remove(&player.character_id);
        }
    }
    previous
}

/// One quest-map tick: refresh every player that selected a wider range, drop stale state.
pub fn tick(server: &GameServer) {
    tick_with_budget(server, SPAWN_BUDGET_PER_TICK);
}

/// [`tick`] with the NPC creation budget of the tick given (tests spend it up front). The budget
/// only limits how many NPCs are created: every tracked player is refreshed either way, because
/// removals and re-adds of NPCs that already exist need none, and a refresh with a spent budget
/// creates nothing.
fn tick_with_budget(server: &GameServer, creation_budget: i64) {
    let state = &server.systems.quests.quest_map;
    state.prune_range_changes(spawn_manager::now());
    if state.tracked_count() == 0 {
        // Nobody selected a wider range (the usual case): do not walk every character.
        return;
    }
    let present: HashSet<u32> = server
        .world
        .all_characters()
        .iter()
        .map(|p| p.character_id)
        .collect();
    let mut budget = creation_budget;
    let tracked: Vec<PlayerRef> = {
        let mut players = state.players.lock().expect("quest map state poisoned");
        players.retain(|id, _| present.contains(id));
        let ids: Vec<u32> = players.keys().copied().collect();
        drop(players);
        ids.iter()
            .filter_map(|id| server.world.character_by_id(*id))
            .collect()
    };
    for player in tracked {
        let spent = refresh_player(server, &player, budget.max(0));
        budget -= spent;
    }
}

/// Refreshes one tracked player (tick / [`set_range`] entry point).
fn refresh_player(server: &GameServer, player: &PlayerRef, budget: i64) -> i64 {
    let state = &server.systems.quests.quest_map;
    let _serial = state.refreshing.lock().expect("quest map state poisoned");
    refresh_player_locked(server, player, budget)
}

/// Applies the player's range and returns the NPC creation budget spent. The caller holds
/// [`State::refreshing`] and must not hold the `players` mutex: it is taken only to snapshot
/// and to commit, never across a world call (2026-11-11 lock cycle).
fn refresh_player_locked(server: &GameServer, player: &PlayerRef, budget: i64) -> i64 {
    let state = &server.systems.quests.quest_map;
    // The region visibility path takes units off this client on its own: a streamed NPC whose
    // ordinary neighbourhood the player entered and left again between two refreshes was removed
    // by that path, although `extra` still lists it and the diff below would never send it
    // again. The path logs those removals per player; apply them to the tracked set before the
    // snapshot, and refresh now, so a marker that is still wanted is streamed again. Drained with
    // only `refreshing` held (the log is a leaf: the region path takes it with the grid held and
    // nothing is taken while it is held) and before `players` is.
    let region_removed = player.take_region_removed_npcs();
    let (range, last_refresh, last_pos, extra) = {
        let mut players = state.players.lock().expect("quest map state poisoned");
        let Some(ps) = players.get_mut(&player.character_id) else {
            return 0;
        };
        for id in &region_removed {
            ps.extra.remove(id);
        }
        (ps.range, ps.last_refresh, ps.last_pos, ps.extra.clone())
    };
    #[cfg(test)]
    state.assert_unlocked();

    let now = spawn_manager::now();
    let Some((inst, prx, pry)) = player.region() else {
        return 0;
    };
    let Some(world) = server.world.world(inst) else {
        return 0;
    };
    let world_object = WorldObject::from(player);
    let pos = world.world_pos_rot(&world_object);
    let forced = now.saturating_duration_since(last_refresh) >= REFRESH_INTERVAL
        || (pos.x - last_pos.0).powi(2) + (pos.y - last_pos.1).powi(2) > MOVE_EPSILON_SQ
        || !region_removed.is_empty();
    if !forced && !range.is_default() {
        return 0;
    }

    let cap = state.cap();
    let radius = range.radius();
    let radius2 = if radius.is_finite() {
        radius * radius
    } else {
        f32::INFINITY
    };
    let natural: HashSet<(i32, i32)> = world.neighbors((prx, pry)).into_iter().collect();
    let query = MarkerQuery {
        center: (pos.x, pos.y),
        radius,
        radius2,
        placements: cap + SPARE_PLACEMENTS,
    };
    let mut set = CandidateSet {
        natural,
        candidates: HashMap::new(),
        spent: 0,
    };
    if !range.is_default() {
        #[cfg(test)]
        state.assert_unlocked();
        if let Some(manager) =
            crate::systems::npcs::spawn_manager::manager_of(server, &world_object, Some(inst))
        {
            collect_from_spawners(server, &world, &manager, &query, budget, &mut set);
        }
    }

    // Removals first: an NPC that left the range, despawned or became naturally visible.
    #[cfg(test)]
    state.assert_unlocked();
    let mut dead: Vec<u32> = Vec::new();
    let mut removals: Vec<NpcRef> = Vec::new();
    // The extras that stay valid, with their squared distance (see the selection below).
    let mut kept: Vec<(u32, f32)> = Vec::new();
    for (id, stored) in extra.iter() {
        let current = world.object(*id).and_then(|o| o.as_npc().cloned());
        let same = current.as_ref().is_some_and(|npc| Arc::ptr_eq(npc, stored));
        if !same {
            // The unit the client still holds is gone (deleted, or its id reused). Do not hide
            // a replacement that already became naturally visible under the reused id.
            let replacement_visible = current.as_ref().is_some_and(|npc| {
                world
                    .region_of(&WorldObject::Npc(npc.clone()))
                    .is_some_and(|r| set.natural.contains(&r))
            });
            if !replacement_visible {
                removals.push(stored.clone());
            }
            dead.push(*id);
            continue;
        }
        let npc = current.expect("checked above");
        let obj = WorldObject::Npc(npc.clone());
        let is_natural = world
            .region_of(&obj)
            .is_some_and(|r| set.natural.contains(&r));
        if is_natural {
            // The regular region add/remove path owns it from now on.
            dead.push(*id);
            continue;
        }
        let np = world.world_pos_rot(&obj);
        let d2 = (np.x - pos.x).powi(2) + (np.y - pos.y).powi(2);
        // A corpse is no marker (its retirement hook normally removes it; this is the backstop).
        let is_dead = npc.data().unit.is_dead();
        if d2 > radius2 || is_dead || !npc.is_visible() {
            removals.push(npc);
            dead.push(*id);
        } else {
            kept.push((*id, d2));
        }
    }

    // The streamed set is the `cap` nearest NPCs of the kept extras and the new candidates: a
    // nearer candidate displaces the farthest extra, so a moving player, and the unlimited
    // range that never leaves a radius, converge on the markers around the current position.
    let cands: Vec<(u32, f32)> = set
        .candidates
        .iter()
        .filter(|(id, _)| !extra.contains_key(*id))
        .map(|(&id, &(d2, _))| (id, d2))
        .collect();
    let (displaced, to_add) = select_extras(cap, &kept, &cands);
    for id in displaced {
        if let Some(stored) = extra.get(&id) {
            removals.push(stored.clone());
            dead.push(id);
        }
    }
    for npc in removals {
        visibility::remove_visible_object(&world, &WorldObject::Npc(npc), player);
    }

    // Adds: nearest first.
    let mut added: Vec<(u32, NpcRef)> = Vec::new();
    if !to_add.is_empty() {
        #[cfg(test)]
        state.assert_unlocked();
        let ctx = server.view_ctx();
        for id in to_add {
            let Some((_, npc)) = set.candidates.remove(&id) else {
                continue;
            };
            let obj = WorldObject::Npc(npc.clone());
            if world
                .region_of(&obj)
                .is_some_and(|r| set.natural.contains(&r))
            {
                continue;
            }
            visibility::add_visible_object(&world, &obj, player, &ctx);
            added.push((id, npc));
        }
    }

    #[cfg(test)]
    {
        state.assert_unlocked();
        let hook = state
            .seams
            .before_commit
            .lock()
            .expect("test seam poisoned")
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    // Commit briefly. `refreshing` keeps range changes out, so only a `forget` (leave-world)
    // can have removed the entry; a changed set is merged by Arc identity.
    let committed = {
        let mut players = state.players.lock().expect("quest map state poisoned");
        match players.get_mut(&player.character_id) {
            Some(ps) if ps.range == range => {
                for id in &dead {
                    let tracked = extra.get(id);
                    if ps
                        .extra
                        .get(id)
                        .is_some_and(|n| tracked.is_some_and(|t| Arc::ptr_eq(n, t)))
                    {
                        ps.extra.remove(id);
                    }
                }
                // `select_extras` keeps the kept extras plus the additions within the cap.
                for (id, npc) in &added {
                    ps.extra.entry(*id).or_insert_with(|| npc.clone());
                }
                ps.last_refresh = now;
                ps.last_pos = (pos.x, pos.y);
                true
            }
            _ => false,
        }
    };
    if committed {
        // An NPC that died or was deleted between its add above and this commit was not tracked
        // yet when its retirement hook ran, so the hook sent nothing: look again now that it is
        // tracked (`forget_npc` then finds the entry and removes the unit from the client).
        for (id, npc) in &added {
            if !is_live_marker(&world, *id, npc) {
                state.forget_npc(server, npc);
            }
        }
    } else {
        // The player left the world or changed range while this refresh ran: nothing may stay
        // streamed without being tracked, it would never be removed.
        for (_, npc) in added {
            visibility::remove_visible_object(&world, &WorldObject::Npc(npc), player);
        }
    }
    set.spent
}

/// Whether `npc` is still the live, visible NPC of its id: a marker the client may keep.
fn is_live_marker(world: &WorldInstance, id: u32, npc: &NpcRef) -> bool {
    let same = world
        .object(id)
        .and_then(|o| o.as_npc().cloned())
        .is_some_and(|current| Arc::ptr_eq(&current, npc));
    if !same {
        return false;
    }
    let is_dead = npc.data().unit.is_dead();
    !is_dead && npc.is_visible()
}

/// Which marker NPCs a player streams: the `cap` nearest of the extras that stay valid (`kept`)
/// and the new candidates (`cands`), both as `(obj id, squared distance)`. Returns the kept
/// extras a nearer candidate displaces (they are removed from the client) and the candidates to
/// add, nearest first. Ties break by id, so the choice does not depend on map iteration order.
fn select_extras(cap: usize, kept: &[(u32, f32)], cands: &[(u32, f32)]) -> (Vec<u32>, Vec<u32>) {
    let mut ranked: Vec<(f32, u32, bool)> = kept
        .iter()
        .map(|&(id, d2)| (d2, id, true))
        .chain(cands.iter().map(|&(id, d2)| (d2, id, false)))
        .collect();
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut displaced = Vec::new();
    let mut add = Vec::new();
    for (rank, (_, id, is_kept)) in ranked.into_iter().enumerate() {
        match (rank < cap, is_kept) {
            (true, false) => add.push(id),
            (false, true) => displaced.push(id),
            _ => {}
        }
    }
    (displaced, add)
}

/// The player-centred query of the placement collector.
struct MarkerQuery {
    center: (f32, f32),
    radius: f32,
    radius2: f32,
    /// Placements to look at: the NPC cap plus [`SPARE_PLACEMENTS`].
    placements: usize,
}

/// Candidate NPCs and the state shared while collecting them.
struct CandidateSet {
    natural: HashSet<(i32, i32)>,
    candidates: HashMap<u32, (f32, NpcRef)>,
    spent: i64,
}

/// Prepares ordinary quest-giver placements around the player and collects their live NPCs.
/// This is the only marker source: every production quest giver / reporter is created by an
/// ordinary spawner placement, so the placement index already yields the same live NPCs a
/// world-wide `all_npcs()` scan would, without scanning tens of thousands of NPCs per player.
fn collect_from_spawners(
    server: &GameServer,
    world: &WorldInstance,
    manager: &crate::systems::npcs::spawn_manager::SpawnManager,
    query: &MarkerQuery,
    budget: i64,
    set: &mut CandidateSet,
) {
    #[cfg(test)]
    server.systems.quests.quest_map.assert_unlocked();
    let residents = server.systems.npcs.spawns.resident_npcs(server);
    let npcs = &server.game_data.systems.npcs.npc;
    // The player's own neighbourhood is left out of the placement list (the region path streams
    // it), so its placements neither get prepared here nor count against the placement limit.
    for spawner in manager.quest_giver_spawners_near(
        server,
        query.center.0,
        query.center.1,
        query.radius,
        &set.natural,
        query.placements,
    ) {
        let mut created = 0;
        if set.spent < budget {
            let want = (budget - set.spent).clamp(0, 4);
            if want > 0 {
                created = spawner.update_warm(server, true, want);
                set.spent += created as i64;
            }
        }
        let live = spawner.spawned_npcs();
        if created > 0 || !live.is_empty() {
            // Track the placement like one the activation pass prepared, so the retention sweep
            // reclaims it once nobody streams it any more.
            manager.note_prepared(server, &spawner);
        }
        for npc in live.into_values().flatten() {
            let template_id = npc.data().unit.template_id;
            let Some(template) = npcs.template(template_id) else {
                continue;
            };
            if !residents.is_quest_map_candidate(template) {
                continue;
            }
            if npc.data().unit.is_dead() || !npc.is_visible() {
                continue;
            }
            let obj = WorldObject::Npc(npc.clone());
            if obj.region().map(|(instance, _, _)| instance) != Some(world.id) {
                continue;
            }
            if world
                .region_of(&obj)
                .is_some_and(|r| set.natural.contains(&r))
            {
                continue;
            }
            let p = world.world_pos_rot(&obj);
            let d2 = (p.x - query.center.0).powi(2) + (p.y - query.center.1).powi(2);
            if d2 <= query.radius2 {
                set.candidates.entry(npc.obj_id).or_insert((d2, npc));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use aaemu_commons::PacketStream;

    use super::*;
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::opcodes::sc;
    use crate::systems::npcs::doodad::spawner::{NpcSpawnerDef, SpawnerBase};
    use crate::systems::npcs::npc::data::NpcTemplate;
    use crate::systems::npcs::npc::spawner_data::{
        NpcSpawnerData, NpcSpawnerNpc, NpcSpawnerTemplate,
    };
    use crate::systems::npcs::spawn_manager::{backend::TestBackend, SpawnManager};
    use crate::systems::quests::data::QuestData;
    use crate::systems::quests::model::kind;
    use crate::systems::quests::templates::{ActKind, QuestComponentTemplate, QuestTemplate};
    use crate::test_support::*;
    use crate::world::{DoodadRef, Entity, NpcRef, WorldInstance};

    const QUEST_NPC: u32 = 500;
    const OTHER_NPC: u32 = 600;
    const GIVER_SPAWNER: u32 = 10;
    const OTHER_SPAWNER: u32 = 20;

    /// Creates NPCs for the ordinary spawner path without the DB-backed manager.
    struct Backend;

    impl TestBackend for Backend {
        fn npc_create(&self, server: &GameServer, obj_id: u32, template_id: u32) -> Option<NpcRef> {
            let id = if obj_id == 0 {
                server.ids.object.get_next_id().ok()?
            } else {
                obj_id
            };
            let mut n = Npc::new(template_id);
            n.unit.level = 5;
            n.unit.hp = 100;
            n.unit.max_hp = 100;
            Some(Entity::new(id, n))
        }

        fn doodad_create(
            &self,
            _server: &GameServer,
            _world: &Arc<WorldInstance>,
            _obj_id: u32,
            _template_id: u32,
        ) -> Option<DoodadRef> {
            None
        }
    }

    fn add_quest_act(q: &mut QuestData, id: u32, component_kind: u8, act: ActKind) {
        q.add_template(QuestTemplate {
            id,
            ..QuestTemplate::default()
        });
        q.add_component(QuestComponentTemplate {
            id,
            quest_id: id,
            kind_id: component_kind,
            ..QuestComponentTemplate::default()
        });
        q.add_act(id, id, id, 0, act);
    }

    fn data() -> GameData {
        let mut gd = GameData::empty();
        for id in [QUEST_NPC, OTHER_NPC] {
            gd.systems.npcs.npc.templates.insert(
                id,
                NpcTemplate {
                    id,
                    ..NpcTemplate::default()
                },
            );
        }
        add_quest_act(
            &mut gd.systems.quests,
            100,
            kind::START,
            ActKind::ConAcceptNpc { npc_id: QUEST_NPC },
        );
        gd.systems.quests.finish();
        gd
    }

    fn npc(template_id: u32, x: f32, y: f32) -> Npc {
        let mut n = Npc::new(template_id);
        n.unit.level = 5;
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.transform.set_xyz(x, y, 100.0);
        n
    }

    fn spawner_template(id: u32) -> NpcSpawnerTemplate {
        NpcSpawnerTemplate {
            id,
            name: format!("t{id}"),
            max_population: 1,
            spawn_delay_min: 10.0,
            spawn_delay_max: 10.0,
            test_radius_npc: 3.0,
            ..NpcSpawnerTemplate::default()
        }
    }

    fn spawner_member(id: u32, spawner_id: u32, npc_id: u32) -> NpcSpawnerNpc {
        NpcSpawnerNpc {
            id,
            npc_spawner_template_id: spawner_id,
            member_id: npc_id,
            member_type: "Npc".into(),
            weight: 1.0,
            unit_id: 0,
        }
    }

    fn npc_def(unit_id: u32, x: f32, y: f32) -> NpcSpawnerDef {
        npc_def_count(unit_id, x, y, 1)
    }

    fn npc_def_count(unit_id: u32, x: f32, y: f32, count: u32) -> NpcSpawnerDef {
        let base = SpawnerBase {
            unit_id,
            position: crate::data::world_templates::WorldSpawnPosition {
                x,
                y,
                z: 100.0,
                ..Default::default()
            },
            ..SpawnerBase::default()
        };
        NpcSpawnerDef {
            base,
            count,
            npc_spawner_ids: Vec::new(),
        }
    }

    fn server() -> Arc<GameServer> {
        server_with_population(1)
    }

    /// A server whose placements keep `population` NPCs each (the cap counts NPCs, the placement
    /// limit counts placements).
    fn server_with_population(population: u32) -> Arc<GameServer> {
        let mut gd = data();
        gd.systems.npcs.npc_spawners = NpcSpawnerData::from_parts(
            vec![
                NpcSpawnerTemplate {
                    max_population: population,
                    ..spawner_template(GIVER_SPAWNER)
                },
                NpcSpawnerTemplate {
                    max_population: population,
                    ..spawner_template(OTHER_SPAWNER)
                },
            ],
            vec![
                spawner_member(100, GIVER_SPAWNER, QUEST_NPC),
                spawner_member(200, OTHER_SPAWNER, OTHER_NPC),
            ],
        )
        .expect("spawner data");
        let server = Arc::new(test_server_with(gd));
        server.set_self();
        *server
            .systems
            .npcs
            .spawns
            .test_backend
            .lock()
            .expect("test backend") = Some(Arc::new(Backend));
        server
    }

    fn manager(server: &Arc<GameServer>) -> Arc<SpawnManager> {
        server
            .systems
            .npcs
            .spawns
            .get_or_create(&server.world.world(0).expect("main_world"))
    }

    fn ids_packet(ids: &[u32]) -> (u16, Vec<u8>) {
        let mut s = PacketStream::new();
        s.write_u16(ids.len() as u16);
        for &id in ids {
            s.write_bc(id);
        }
        (sc::SCUnitsRemovedPacket, s.into_bytes())
    }

    #[test]
    fn option_slots_match_the_3_5_slider() {
        assert_eq!(Range::from_slot(0), Some(Range::Meters100));
        assert_eq!(Range::from_slot(1), Some(Range::Meters200));
        assert_eq!(Range::from_slot(2), Some(Range::Meters300));
        assert_eq!(Range::from_slot(3), Some(Range::Unlimited));
        assert_eq!(Range::from_slot(4), None);
        assert_eq!(Range::from_meters(0), Some(Range::Unlimited));
        assert_eq!(Range::from_meters(100), Some(Range::Meters100));
        assert_eq!(Range::from_meters(150), None);
        assert_eq!(Range::Meters300.slot(), 2);
        assert_eq!(Range::Unlimited.meters(), 0);
    }

    #[tokio::test]
    async fn range_streams_far_quest_givers_and_removes_them_again() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        m.add_npc_spawner(&server, npc_def(OTHER_NPC, 1000.0, 1270.0))
            .expect("other placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        let world = server.world.world(0).expect("main_world");

        // 250 m away: outside the ordinary 2-region neighbourhood.
        assert!(client.drain().is_empty(), "nothing is streamed yet");

        // 300 m: only the safe quest giver placement is prepared and streamed.
        set_range(&server, &player, Range::Meters300);
        let frames = client.drain();
        assert_eq!(
            frames.iter().map(|f| f.0).collect::<Vec<_>>(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket],
            "one quest giver"
        );
        let giver = world
            .all_npcs()
            .into_iter()
            .find(|n| n.data().unit.template_id == QUEST_NPC)
            .expect("prepared giver");
        let mut expected = npc(QUEST_NPC, 1000.0, 1250.0);
        let body = crate::world::unit_state::npc_unit_state(
            giver.obj_id,
            &mut expected,
            "",
            server.view_ctx().now_ms,
        )
        .body;
        assert_eq!(
            frames[0].1, body,
            "SCUnitState body is the NPC's own writer"
        );
        assert!(
            !world
                .all_npcs()
                .iter()
                .any(|n| n.data().unit.template_id == OTHER_NPC),
            "a non-quest placement is never prepared for a marker"
        );

        // Back to the default: the extra marker is removed.
        let giver_id = giver.obj_id;
        set_range(&server, &player, Range::Meters100);
        assert_eq!(client.drain(), vec![ids_packet(&[giver_id])]);

        // Unlimited reaches a second quest giver far across the instance. (A change within a
        // second of the 300 m one would only be applied by the next tick.)
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 0.0, 2000.0))
            .expect("far placement");
        client.drain();
        spawn_manager::advance_test_clock(MIN_RANGE_CHANGE_INTERVAL);
        set_range(&server, &player, Range::Unlimited);
        let ops = client.drain_ops();
        assert_eq!(
            ops,
            vec![
                sc::SCUnitStatePacket,
                sc::SCUnitPointsPacket,
                sc::SCUnitStatePacket,
                sc::SCUnitPointsPacket
            ],
            "both quest givers, nearest first"
        );

        // Leaving the world drops the state without sending anything.
        crate::systems::quests::on_leave_world(&server, player.character_id);
        assert_eq!(server.systems.quests.quest_map.tracked_count(), 0);
    }

    #[tokio::test]
    async fn default_range_does_not_stream_anything() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();

        set_range(&server, &player, Range::Meters100);
        assert!(client.drain().is_empty());
        assert!(
            server
                .world
                .world(0)
                .expect("main_world")
                .all_npcs()
                .is_empty(),
            "the default range prepares and streams nothing"
        );
        assert_eq!(
            server
                .systems
                .quests
                .quest_map
                .range_of(player.character_id),
            Range::Meters100
        );
    }

    /// The 2026-11-11 lock cycle: a refresh used to hold `players` while it called into the
    /// world (spawner `update_warm`, NPC data locks, visibility packets). The world paths of a
    /// normal refresh assert the state mutex is free, so holding it across them fails here.
    #[tokio::test]
    async fn refresh_runs_world_work_without_the_state_lock() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();

        set_range(&server, &player, Range::Meters300);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );
        set_range(&server, &player, Range::Meters100);
        assert_eq!(server.systems.quests.quest_map.tracked_count(), 0);
    }

    #[tokio::test]
    async fn despawned_extra_npc_sends_the_unit_removal_to_the_far_player() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        set_range(&server, &player, Range::Meters300);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );

        let world = server.world.world(0).expect("main_world");
        let npc = world
            .all_npcs()
            .into_iter()
            .find(|n| n.data().unit.template_id == QUEST_NPC)
            .expect("streamed giver");
        let obj_id = npc.obj_id;
        let spawner = server
            .systems
            .npcs
            .spawns
            .npc_spawner_of(obj_id)
            .expect("spawner link");

        // The player is several regions away: the ordinary deletion broadcast never reaches it.
        spawner.despawn(&server, &npc);
        assert_eq!(
            client.drain(),
            vec![ids_packet(&[obj_id])],
            "the far viewer must not keep a ghost marker"
        );
        assert!(server
            .systems
            .quests
            .quest_map
            .streamed_npc_ids()
            .is_empty());
    }

    #[tokio::test]
    async fn dying_extra_npc_sends_the_unit_removal_to_the_far_player() {
        let server = server();
        // The NPC death path reaches `after_death` through the skills -> npcs bridge.
        crate::systems::npcs::install_hooks(&server);
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        set_range(&server, &player, Range::Meters300);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );

        let world = server.world.world(0).expect("main_world");
        let npc = world
            .all_npcs()
            .into_iter()
            .find(|n| n.data().unit.template_id == QUEST_NPC)
            .expect("streamed giver");
        let obj_id = npc.obj_id;
        crate::systems::npcs::npc::death::do_die(&server, &npc, None, 0);
        let frames = client.drain();
        assert!(
            frames.contains(&ids_packet(&[obj_id])),
            "a dying marker is removed for the far viewer: {frames:?}"
        );
        assert!(server
            .systems
            .quests
            .quest_map
            .streamed_npc_ids()
            .is_empty());
    }

    /// Markers come from the ordinary quest-giver placement index, not from a world-wide NPC
    /// scan: an ad-hoc NPC with a quest-giver template is not a marker source.
    #[tokio::test]
    async fn markers_come_from_placements_not_a_world_npc_scan() {
        let server = server();
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        let world = server.world.world(0).expect("main_world");
        let ctx = server.view_ctx();
        world.spawn(Entity::new(0x500, npc(QUEST_NPC, 1000.0, 1250.0)), &ctx);
        client.drain();

        set_range(&server, &player, Range::Meters300);
        assert!(
            client.drain().is_empty(),
            "a directly spawned quest giver is not a placement marker"
        );
        assert!(server
            .systems
            .quests
            .quest_map
            .streamed_npc_ids()
            .is_empty());
    }

    /// `tick` refreshes a standing player only once [`REFRESH_INTERVAL`] has passed (or the
    /// player moved). The spawn clock is the seam, so the test does not sleep; the tokio clock
    /// is paused, so real elapsed time cannot cross the interval on a slow machine either.
    #[tokio::test(start_paused = true)]
    async fn tick_refreshes_a_standing_player_only_after_the_interval() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        set_range(&server, &player, Range::Meters300);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );

        // A second giver placement appears 250 m east: the next refresh is not due yet ...
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1250.0, 1000.0))
            .expect("second placement");
        tick(&server);
        assert!(client.drain().is_empty(), "inside the refresh interval");

        // ... and runs once the interval has passed.
        spawn_manager::advance_test_clock(REFRESH_INTERVAL);
        tick(&server);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );
    }

    /// A tick whose creation budget is spent still refreshes every tracked player: removals of
    /// markers that left the range need no new NPC, and the spent budget creates none.
    #[tokio::test(start_paused = true)]
    async fn a_spent_creation_budget_still_refreshes_every_tracked_player() {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut clients = Vec::new();
        let mut players = Vec::new();
        for (i, x) in [(1u32, 1000.0f32), (2, 1010.0)] {
            let mut client = TestClient::new(&server, i, 1000 + i);
            let player = spawn_player(
                &server,
                &client,
                test_character(i, &format!("Quester{i}"), x, 1000.0),
                5000 + i,
            );
            client.drain();
            set_range(&server, &player, Range::Meters300);
            assert_eq!(
                client.drain_ops(),
                vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
            );
            clients.push(client);
            players.push(player);
        }
        // (The first client also saw the second character spawn next to it.)
        for client in &mut clients {
            client.drain();
        }
        // Both walk 400 m west: the giver is out of their range now.
        for player in &players {
            let t = player.data().unit.transform;
            player.data().unit.transform.set_xyz(t.x - 400.0, t.y, t.z);
        }
        spawn_manager::advance_test_clock(REFRESH_INTERVAL);

        // The creation budget of the tick is spent before the first player: the old tick refreshed
        // one player and stopped, so the other kept the far marker.
        tick_with_budget(&server, 0);
        for client in &mut clients {
            assert_eq!(
                client.drain_ops(),
                vec![sc::SCUnitsRemovedPacket],
                "every player loses the giver that left its range"
            );
        }
        assert!(server
            .systems
            .quests
            .quest_map
            .streamed_npc_ids()
            .is_empty());
    }

    /// A player with one far giver streamed at 300 m, the client in sync with the server.
    async fn streamed_giver_fixture() -> (Arc<GameServer>, TestClient, PlayerRef) {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        set_range(&server, &player, Range::Meters300);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );
        (server, client, player)
    }

    /// A cut-scene end (and a teleport end, an in-place zone load) empties the client's world,
    /// while the server repaints only the ordinary neighbourhood. The tracked set must not keep
    /// claiming the far marker is on the client: every later refresh skipped it before.
    #[tokio::test]
    async fn a_cut_scene_end_streams_the_far_markers_again() {
        let (server, mut client, player) = streamed_giver_fixture().await;

        crate::systems::npcs::portal::resend_visible_objects_to_character(&server, &player);
        let repaint = client.drain();
        assert!(
            !repaint.iter().any(|f| f.0 == sc::SCUnitStatePacket),
            "the giver 250 m away is not part of the ordinary repaint: {repaint:?}"
        );

        // The next tick streams the marker again ...
        tick(&server);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );
        // ... and tracks it again: a later refresh has nothing to do.
        spawn_manager::advance_test_clock(REFRESH_INTERVAL);
        tick(&server);
        assert!(client.drain().is_empty());
        assert_eq!(server.systems.quests.quest_map.streamed_npc_ids().len(), 1);
    }

    /// `CSInstanceLoaded` (a recall, a Return gate, a resurrection point, another instance):
    /// the load emptied the client's world, so the far markers are streamed again as well.
    #[tokio::test]
    async fn a_zone_load_streams_the_far_markers_again() {
        let (server, mut client, player) = streamed_giver_fixture().await;

        crate::systems::instances::worlds::instance_loaded(&server, &player);
        let load = client.drain();
        assert_eq!(
            load.iter().filter(|f| f.0 == sc::SCUnitStatePacket).count(),
            1,
            "only the character's own state, not the giver 250 m away: {load:?}"
        );

        tick(&server);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );
    }

    /// The region path takes a unit off the client when the player leaves its neighbourhood, and
    /// the tracked set never hears of it: a player who walked up to a streamed giver and back
    /// between two refreshes has lost the marker although it is still tracked. The path logs the
    /// removal; the next tick applies the log and streams the marker again. The player is back
    /// where the last refresh saw it and the clock is paused, so only the log makes it refresh.
    #[tokio::test(start_paused = true)]
    async fn a_region_removal_of_a_streamed_marker_streams_it_again() {
        let (server, mut client, player) = streamed_giver_fixture().await;
        let world = server.world.world(0).expect("main_world");
        let giver = npc_at(&world, 1000.0, 1250.0);

        // Walk into the giver's neighbourhood (the region path streams it) and out again.
        player.data().unit.transform.set_xyz(1000.0, 1250.0, 100.0);
        world.add_visible_object(&player, &server.view_ctx());
        client.drain();
        player.data().unit.transform.set_xyz(1000.0, 1000.0, 100.0);
        world.add_visible_object(&player, &server.view_ctx());
        let frames = client.drain();
        assert!(
            frames.contains(&ids_packet(&[giver.obj_id])),
            "the region path removed the marker: {frames:?}"
        );

        tick(&server);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket],
            "the logged removal is applied and the marker is streamed again"
        );
        assert_eq!(
            server.systems.quests.quest_map.streamed_npc_ids(),
            HashSet::from([giver.obj_id])
        );
    }

    /// The other way the region path removes a streamed marker behind the tracked set's back: the
    /// NPC itself wanders through the player's neighbourhood (the path streams it) and out again,
    /// a single-unit removal instead of the batch of the test above.
    #[tokio::test(start_paused = true)]
    async fn a_marker_that_wanders_through_the_neighbourhood_is_streamed_again() {
        let (server, mut client, _player) = streamed_giver_fixture().await;
        let world = server.world.world(0).expect("main_world");
        let giver = npc_at(&world, 1000.0, 1250.0);
        let ctx = server.view_ctx();

        // 100 m from the player (inside the ordinary window), then back 250 m away.
        giver.data().unit.transform.set_xyz(1000.0, 1100.0, 100.0);
        world.add_visible_object(WorldObject::Npc(giver.clone()), &ctx);
        client.drain();
        giver.data().unit.transform.set_xyz(1000.0, 1250.0, 100.0);
        world.add_visible_object(WorldObject::Npc(giver.clone()), &ctx);
        let frames = client.drain();
        assert!(
            frames.contains(&ids_packet(&[giver.obj_id])),
            "the region path removed the marker: {frames:?}"
        );

        tick(&server);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket],
            "the logged removal is applied and the marker is streamed again"
        );
    }

    #[test]
    fn select_extras_keeps_the_nearest_and_displaces_the_farthest() {
        // The cap is full of far extras: a nearer candidate displaces the farthest one.
        assert_eq!(
            select_extras(2, &[(1, 900.0 * 900.0), (2, 800.0 * 800.0)], &[(3, 100.0)]),
            (vec![1], vec![3])
        );
        // Room left: nothing is displaced and the candidates come nearest first.
        assert_eq!(
            select_extras(4, &[(1, 50.0)], &[(5, 30.0), (4, 10.0)]),
            (vec![], vec![4, 5])
        );
        // A full cap without a nearer candidate changes nothing.
        assert_eq!(
            select_extras(2, &[(1, 1.0), (2, 2.0)], &[(3, 3.0)]),
            (vec![], vec![])
        );
        // Ties break by id.
        assert_eq!(
            select_extras(1, &[(7, 10.0)], &[(6, 10.0), (8, 20.0)]),
            (vec![7], vec![6])
        );
        // A zero cap streams nothing.
        assert_eq!(
            select_extras(0, &[(1, 1.0)], &[(2, 0.5)]),
            (vec![1], vec![])
        );
    }

    /// The NPC the world holds at `(x, y)` (the placements of these tests are far apart).
    fn npc_at(world: &WorldInstance, x: f32, y: f32) -> NpcRef {
        world
            .all_npcs()
            .into_iter()
            .find(|n| {
                let p = world.world_pos_rot(&WorldObject::Npc(n.clone()));
                (p.x - x).abs() < 1.0 && (p.y - y).abs() < 1.0
            })
            .unwrap_or_else(|| panic!("no NPC at ({x}, {y})"))
    }

    /// Unlimited has no radius to drop far extras by, so with the cap full the nearest NPCs must
    /// win: after the player walked, a nearer giver displaces the farthest streamed one.
    #[tokio::test]
    async fn unlimited_swaps_in_a_nearer_giver_when_the_cap_is_full() {
        let server = server();
        let quest_map = &server.systems.quests.quest_map;
        quest_map.set_cap(2);
        let m = manager(&server);
        // 800 m and 900 m north of the player, and one far to the south-west.
        for (x, y) in [(1000.0, 1900.0), (1000.0, 1800.0), (100.0, 100.0)] {
            m.add_npc_spawner(&server, npc_def(QUEST_NPC, x, y))
                .expect("giver placement");
        }
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        let world = server.world.world(0).expect("main_world");

        // The cap of 2 streams the two nearest of the three.
        set_range(&server, &player, Range::Unlimited);
        assert_eq!(
            client.drain_ops().len(),
            4,
            "two givers, state + points each"
        );
        let far = npc_at(&world, 1000.0, 1900.0);
        let mid = npc_at(&world, 1000.0, 1800.0);
        let south_west = npc_at(&world, 100.0, 100.0);
        assert_eq!(
            quest_map.streamed_npc_ids(),
            HashSet::from([far.obj_id, mid.obj_id])
        );

        // The player walks to the south-west: that giver is 412 m away now, the 900 m one is the
        // farthest streamed. Nothing leaves a radius, only the selection can swap them.
        player.data().unit.transform.set_xyz(200.0, 500.0, 100.0);
        world.add_visible_object(&player, &server.view_ctx());
        client.drain();
        tick(&server);
        let frames = client.drain();
        assert_eq!(frames.len(), 3, "one removal, one add: {frames:?}");
        assert_eq!(
            frames[0],
            ids_packet(&[far.obj_id]),
            "the farthest extra is removed first"
        );
        assert_eq!(
            frames[1..].iter().map(|f| f.0).collect::<Vec<_>>(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );
        assert_eq!(
            quest_map.streamed_npc_ids(),
            HashSet::from([mid.obj_id, south_west.obj_id])
        );
    }

    /// 15 x 14 placements 70 m apart, all outside the player's neighbourhood: all 200 are
    /// streamed (the placement list used to stop at 192 placements, the player's own
    /// neighbourhood included).
    #[tokio::test]
    async fn unlimited_streams_two_hundred_placements() {
        let server = server();
        let m = manager(&server);
        for i in 0..200u32 {
            let (col, row) = ((i % 15) as f32, (i / 15) as f32);
            m.add_npc_spawner(
                &server,
                npc_def(QUEST_NPC, 400.0 + 70.0 * col, 400.0 + 70.0 * row),
            )
            .expect("giver placement");
        }
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 100.0, 100.0),
            5000,
        );
        client.drain();
        let world = server.world.world(0).expect("main_world");

        // A refresh prepares at most 32 NPCs: repeat until the set is stable.
        set_range(&server, &player, Range::Unlimited);
        for _ in 0..10 {
            spawn_manager::advance_test_clock(REFRESH_INTERVAL);
            tick(&server);
        }
        assert_eq!(world.all_npcs().len(), 200);
        assert_eq!(
            server.systems.quests.quest_map.streamed_npc_ids().len(),
            200
        );
    }

    /// The real cap and placements of two NPCs each (the cap counts NPCs, the placement limit
    /// placements): after travelling, the streamed set is exactly the nearest `MAX_EXTRA_NPCS`
    /// NPCs of the old and the new groups, the far markers are replaced by the nearer ones.
    #[tokio::test]
    async fn unlimited_keeps_the_nearest_markers_after_travel() {
        let server = server_with_population(2);
        let m = manager(&server);
        // 128 old placements of two givers each around y = 1250 (250 m from the start).
        for col in 0..16 {
            for row in 0..8 {
                m.add_npc_spawner(
                    &server,
                    npc_def_count(
                        QUEST_NPC,
                        900.0 + col as f32 * 7.0,
                        1230.0 + row as f32 * 5.0,
                        2,
                    ),
                )
                .expect("old placement");
            }
        }
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        // Fill the set with the old group: 32 creations per refresh, 8 refreshes.
        set_range(&server, &player, Range::Unlimited);
        for _ in 0..7 {
            spawn_manager::advance_test_clock(REFRESH_INTERVAL);
            tick(&server);
        }
        client.drain();
        assert_eq!(
            server.systems.quests.quest_map.streamed_npc_ids().len(),
            MAX_EXTRA_NPCS
        );

        // Travel 500 m north; 40 closer placements of two givers each appear.
        player.data().unit.transform.set_xyz(1000.0, 1500.0, 100.0);
        let world = server.world.world(0).expect("main_world");
        world.add_visible_object(&player, &server.view_ctx());
        client.drain();
        for col in 0..8 {
            for row in 0..5 {
                m.add_npc_spawner(
                    &server,
                    npc_def_count(
                        QUEST_NPC,
                        960.0 + col as f32 * 10.0,
                        1695.0 + row as f32 * 5.0,
                        2,
                    ),
                )
                .expect("near placement");
            }
        }
        for _ in 0..3 {
            spawn_manager::advance_test_clock(REFRESH_INTERVAL);
            tick(&server);
        }
        client.drain();

        // The client's set is exactly the nearest 256 markers of both groups.
        let (_, prx, pry) = player.region().expect("player region");
        let natural: HashSet<(i32, i32)> = world.neighbors((prx, pry)).into_iter().collect();
        let pos = world.world_pos_rot(&WorldObject::from(&player));
        let mut candidates: Vec<(f32, u32)> = world
            .all_npcs()
            .into_iter()
            .filter(|n| {
                n.data().unit.template_id == QUEST_NPC
                    && !n.data().unit.is_dead()
                    && n.is_visible()
                    && !world
                        .region_of(&WorldObject::Npc(n.clone()))
                        .is_some_and(|r| natural.contains(&r))
            })
            .map(|n| {
                let p = world.world_pos_rot(&WorldObject::Npc(n.clone()));
                ((p.x - pos.x).powi(2) + (p.y - pos.y).powi(2), n.obj_id)
            })
            .collect();
        candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let expected: HashSet<u32> = candidates
            .into_iter()
            .take(MAX_EXTRA_NPCS)
            .map(|(_, id)| id)
            .collect();
        assert_eq!(
            server.systems.quests.quest_map.streamed_npc_ids(),
            expected,
            "the nearest markers replace the far ones"
        );
    }

    /// The player's own neighbourhood is no marker source: its placements are neither prepared
    /// for the option nor do they use up the placement limit (cap + spare) the far ones need.
    #[tokio::test]
    async fn natural_placements_do_not_use_up_the_placement_limit() {
        let server = server();
        let quest_map = &server.systems.quests.quest_map;
        quest_map.set_cap(5); // 5 + SPARE_PLACEMENTS = 13 placements are looked at
        let m = manager(&server);
        // 20 placements inside the player's 5x5 regions (x, y < 256), 20 in a row far east.
        for i in 0..20u32 {
            let (col, row) = ((i % 5) as f32, (i / 5) as f32);
            m.add_npc_spawner(
                &server,
                npc_def(QUEST_NPC, 40.0 + 40.0 * col, 40.0 + 40.0 * row),
            )
            .expect("natural placement");
            m.add_npc_spawner(&server, npc_def(QUEST_NPC, 500.0 + 40.0 * i as f32, 100.0))
                .expect("far placement");
        }
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 100.0, 100.0),
            5000,
        );
        client.drain();
        let world = server.world.world(0).expect("main_world");

        set_range(&server, &player, Range::Unlimited);
        let x_of = |id: u32| {
            let npc = world.npc(id).expect("a live NPC");
            world.world_pos_rot(&WorldObject::Npc(npc)).x as i32
        };
        let mut streamed: Vec<i32> = quest_map.streamed_npc_ids().into_iter().map(x_of).collect();
        streamed.sort_unstable();
        assert_eq!(
            streamed,
            vec![500, 540, 580, 620, 660],
            "the cap of 5, nearest"
        );
        let prepared: Vec<i32> = world.all_npcs().iter().map(|n| x_of(n.obj_id)).collect();
        assert_eq!(prepared.len(), 13, "the limit of 13 placements, all far");
        assert!(
            prepared.iter().all(|&x| x >= 500),
            "no placement of the player's own neighbourhood is prepared: {prepared:?}"
        );
    }

    /// Streams the giver at (1000, 1250) to a fresh far player while `retire` runs between the
    /// refresh's add of it and its commit: nothing is tracked yet, so the retirement hook finds
    /// nothing to remove. The refresh itself must notice the retired NPC once it is tracked, or
    /// the far player keeps a ghost marker.
    async fn assert_retired_before_the_commit_is_removed(
        retire: impl FnOnce(&GameServer, &NpcRef) + Send + 'static,
    ) {
        let server = server();
        let m = manager(&server);
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("giver placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();

        let retired = Arc::new(AtomicU32::new(0));
        let (hook_server, hook_retired) = (server.clone(), retired.clone());
        server.systems.quests.quest_map.before_next_commit(move || {
            let world = hook_server.world.world(0).expect("main_world");
            let npc = npc_at(&world, 1000.0, 1250.0);
            hook_retired.store(npc.obj_id, Ordering::SeqCst);
            retire(&hook_server, &npc);
        });
        set_range(&server, &player, Range::Meters300);

        let obj_id = retired.load(Ordering::SeqCst);
        assert_ne!(obj_id, 0, "the hook ran between the add and the commit");
        let frames = client.drain();
        assert_eq!(
            frames.iter().map(|f| f.0).collect::<Vec<_>>(),
            vec![
                sc::SCUnitStatePacket,
                sc::SCUnitPointsPacket,
                sc::SCUnitsRemovedPacket
            ]
        );
        assert_eq!(frames[2], ids_packet(&[obj_id]));
        assert!(server
            .systems
            .quests
            .quest_map
            .streamed_npc_ids()
            .is_empty());
    }

    /// The NPC is deleted in that window.
    #[tokio::test]
    async fn an_npc_retired_before_the_commit_is_removed_from_the_far_player() {
        assert_retired_before_the_commit_is_removed(|server, npc| {
            let spawner = server
                .systems
                .npcs
                .spawns
                .npc_spawner_of(npc.obj_id)
                .expect("spawner link");
            spawner.despawn(server, npc);
        })
        .await;
    }

    /// The NPC dies in that window: the lethal hit sets its hit points, then the death hook runs
    /// (and finds nothing tracked). The corpse stays in the world, so only the dead unit tells.
    #[tokio::test]
    async fn an_npc_that_dies_before_the_commit_is_removed_from_the_far_player() {
        assert_retired_before_the_commit_is_removed(|server, npc| {
            npc.data().unit.hp = 0;
            crate::systems::npcs::spawn_manager::on_npc_died(server, npc);
        })
        .await;
    }

    /// The driver re-sends the stored range on every world entry and options-window init, and any
    /// player can type the command: repeating the range in force must cost nothing.
    #[tokio::test]
    async fn a_repeated_range_is_not_refreshed_again() {
        let (server, mut client, player) = streamed_giver_fixture().await;
        // A refresh would stream this second giver; the repeated command must not refresh, also
        // when the interval of range changes is long over.
        manager(&server)
            .add_npc_spawner(&server, npc_def(QUEST_NPC, 1250.0, 1000.0))
            .expect("second placement");
        spawn_manager::advance_test_clock(MIN_RANGE_CHANGE_INTERVAL);

        assert_eq!(
            set_range(&server, &player, Range::Meters300),
            Range::Meters300
        );
        assert!(
            client.drain().is_empty(),
            "the range in force is not applied again"
        );
        assert_eq!(server.systems.quests.quest_map.streamed_npc_ids().len(), 1);
    }

    /// A change to a wider range refreshes at once at most once per second per character; a
    /// faster one is recorded and applied by the next tick, so the last value still wins. (The
    /// tokio clock is paused: only the spawn test clock moves time.)
    #[tokio::test(start_paused = true)]
    async fn a_range_change_within_the_interval_is_applied_by_the_next_tick() {
        let server = server();
        let m = manager(&server);
        // A giver 250 m north (inside 300 m) and one 600 m north (only unlimited reaches it).
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1250.0))
            .expect("near placement");
        m.add_npc_spawner(&server, npc_def(QUEST_NPC, 1000.0, 1600.0))
            .expect("far placement");
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            5000,
        );
        client.drain();
        let world = server.world.world(0).expect("main_world");
        let quest_map = &server.systems.quests.quest_map;

        set_range(&server, &player, Range::Meters300);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );

        // The option is dragged on to Unlimited within the same second: recorded, not applied.
        assert_eq!(
            set_range(&server, &player, Range::Unlimited),
            Range::Meters300
        );
        assert!(
            client.drain().is_empty(),
            "applied by the next tick, not at once"
        );
        assert_eq!(quest_map.range_of(player.character_id), Range::Unlimited);
        tick(&server);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket],
            "the tick streams the giver only unlimited reaches"
        );

        // A second later a change is applied at once again.
        spawn_manager::advance_test_clock(MIN_RANGE_CHANGE_INTERVAL);
        set_range(&server, &player, Range::Meters300);
        let far = npc_at(&world, 1000.0, 1600.0);
        assert_eq!(client.drain(), vec![ids_packet(&[far.obj_id])]);
    }

    /// Going back to the default is always applied at once (it only removes what was streamed),
    /// but it does not reset the limit: alternating cannot force a refresh per command. (The
    /// tokio clock is paused: only the spawn test clock moves time.)
    #[tokio::test(start_paused = true)]
    async fn alternating_with_the_default_does_not_reset_the_limit() {
        let (server, mut client, player) = streamed_giver_fixture().await;

        set_range(&server, &player, Range::Meters100);
        assert_eq!(client.drain().len(), 1, "the giver is removed at once");
        set_range(&server, &player, Range::Meters300);
        assert!(
            client.drain().is_empty(),
            "within the interval: left to the tick"
        );
        tick(&server);
        assert_eq!(
            client.drain_ops(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket]
        );
    }

    /// The death hook normally removes a dying extra; if it is missed the NPC stays as a corpse
    /// (it is not deleted), and the next refresh must still drop it.
    #[tokio::test]
    async fn a_dead_extra_npc_is_dropped_by_the_next_refresh() {
        let (server, mut client, _player) = streamed_giver_fixture().await;
        let world = server.world.world(0).expect("main_world");
        let giver = npc_at(&world, 1000.0, 1250.0);
        // A death whose hook never ran: only the hit points say so.
        giver.data().unit.hp = 0;

        spawn_manager::advance_test_clock(REFRESH_INTERVAL);
        tick(&server);
        // The corpse is removed first (the placement may refill itself in the same refresh,
        // which streams its new NPC after that).
        let frames = client.drain();
        assert_eq!(
            frames.first(),
            Some(&ids_packet(&[giver.obj_id])),
            "{frames:?}"
        );
        assert!(!server
            .systems
            .quests
            .quest_map
            .streamed_npc_ids()
            .contains(&giver.obj_id));
    }
}
