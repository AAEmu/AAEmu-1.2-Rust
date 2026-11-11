//! "Quest Details Map Display" range: the client's given-quest map markers are produced by
//! `X2::GameClient::ClientUnit::UpdateQuestMark` for every unit the server streams
//! (`D:/aa-re/client12/out-x2game/x2game.dll.c:693168` `FUN_39359c60`, event `0x34` at
//! `FUN_39359e90`). 1.2 has no client-side distance option, so the range is widened by
//! streaming safely quest-relevant NPCs further than the ordinary 2-region neighbourhood.
//!
//! The 3.5 option semantics (`option_map_given_quest_distance`, slot * 100 m, 4.0 =
//! unlimited; 10.x `sub_396433C0`) are reproduced as a per-character range. Quest objectives
//! of accepted quests are already map-wide (`X2Map:AddNotifyQuestInfo` reads the static quest
//! data, `FUN_392b6ac0`); this module only adds quest givers / reporters.
//!
//! The range is not persisted server-side: the client stores the option value and the injected
//! driver sends `/questmap <m|0>` after entering the world and when the option is saved.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::server::GameServer;
use crate::world::visibility;
use crate::world::{NpcRef, PlayerRef, WorldInstance, WorldObject};

/// How often a tracked player is refreshed while moving.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Movement that forces an immediate refresh even inside [`REFRESH_INTERVAL`].
const MOVE_EPSILON_SQ: f32 = 64.0; // 8 m
/// Hard cap of extra marker NPCs streamed per character. Keeps the Unlimited option bounded
/// (the 1.2 world has about two thousand quest giver placements).
pub const MAX_EXTRA_NPCS: usize = 256;
/// Placements prepared per refresh; the rest follow on later ticks.
const MAX_SPAWNERS_PER_REFRESH: usize = 192;
/// NPC creations per refresh across all tracked players (one quest-map tick).
pub const SPAWN_BUDGET_PER_TICK: i64 = 32;

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
    /// NPCs streamed only because of this option (never naturally visible).
    extra: HashMap<u32, NpcRef>,
    last_refresh: Instant,
    last_pos: (f32, f32),
}

/// Per-character quest-map range state (`server.systems.quests.quest_map`).
#[derive(Debug, Default)]
pub struct State {
    players: Mutex<HashMap<u32, PlayerState>>,
}

impl State {
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
}

fn new_player_state(range: Range) -> PlayerState {
    PlayerState {
        range,
        extra: HashMap::new(),
        last_refresh: Instant::now() - REFRESH_INTERVAL * 2,
        last_pos: (f32::NAN, f32::NAN),
    }
}

/// `/questmap <100|200|300|0>`: changes the range and applies it immediately.
pub fn set_range(server: &GameServer, player: &PlayerRef, range: Range) -> Range {
    let state = &server.systems.quests.quest_map;
    let previous = {
        let mut players = state.players.lock().expect("quest map state poisoned");
        let entry = players
            .entry(player.character_id)
            .or_insert_with(|| new_player_state(range));
        let previous = entry.range;
        entry.range = range;
        entry.last_refresh = Instant::now() - REFRESH_INTERVAL * 2;
        previous
    };
    refresh_player(server, player, SPAWN_BUDGET_PER_TICK);
    if range.is_default() {
        // Nothing can be extra at 100 m; the removal pass above has run.
        state
            .players
            .lock()
            .expect("quest map state poisoned")
            .remove(&player.character_id);
    }
    previous
}

/// One quest-map tick: refresh every player that selected a wider range, drop stale state.
pub fn tick(server: &GameServer) {
    let present: HashSet<u32> = server
        .world
        .all_characters()
        .iter()
        .map(|p| p.character_id)
        .collect();
    let state = &server.systems.quests.quest_map;
    let mut budget = SPAWN_BUDGET_PER_TICK;
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
        let spent = refresh_player(server, &player, budget);
        budget -= spent;
        if budget <= 0 {
            break;
        }
    }
}

/// Applies the player's range and returns the NPC creation budget spent.
fn refresh_player(server: &GameServer, player: &PlayerRef, budget: i64) -> i64 {
    let state = &server.systems.quests.quest_map;
    let mut players = state.players.lock().expect("quest map state poisoned");
    let Some(ps) = players.get_mut(&player.character_id) else {
        return 0;
    };
    let now = Instant::now();
    let Some((inst, prx, pry)) = player.region() else {
        return 0;
    };
    let Some(world) = server.world.world(inst) else {
        return 0;
    };
    let world_object = WorldObject::from(player);
    let pos = world.world_pos_rot(&world_object);
    let range = ps.range;
    let forced = ps.last_refresh.elapsed() >= REFRESH_INTERVAL
        || (pos.x - ps.last_pos.0).powi(2) + (pos.y - ps.last_pos.1).powi(2) > MOVE_EPSILON_SQ;
    if !forced && !range.is_default() {
        return 0;
    }
    ps.last_refresh = now;
    ps.last_pos = (pos.x, pos.y);

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
    };
    let mut set = CandidateSet {
        natural,
        candidates: HashMap::new(),
        spent: 0,
    };
    if !range.is_default() {
        collect_live_quest_givers(server, &world, &query, &mut set);
        if let Some(manager) =
            crate::systems::npcs::spawn_manager::manager_of(server, &world_object, Some(inst))
        {
            collect_from_spawners(server, &world, &manager, &query, budget, &mut set);
        }
    }

    // Removals first: an NPC that left the range, despawned or became naturally visible.
    let mut dead: Vec<u32> = Vec::new();
    let mut removals: Vec<NpcRef> = Vec::new();
    for (id, stored) in ps.extra.iter() {
        let current = world.object(*id).and_then(|o| o.as_npc().cloned());
        let same = current
            .as_ref()
            .is_some_and(|npc| std::sync::Arc::ptr_eq(npc, stored));
        if !same {
            dead.push(*id);
            continue;
        }
        let npc = current.expect("checked above");
        let obj = WorldObject::Npc(npc.clone());
        let is_natural = world
            .region_of(&obj)
            .is_some_and(|r| set.natural.contains(&r));
        let np = world.world_pos_rot(&obj);
        let d2 = (np.x - pos.x).powi(2) + (np.y - pos.y).powi(2);
        if is_natural {
            // The regular region add/remove path owns it from now on.
            dead.push(*id);
        } else if d2 > radius2 {
            removals.push(npc);
            dead.push(*id);
        }
    }
    for npc in removals {
        visibility::remove_visible_object(&world, &WorldObject::Npc(npc), player);
    }
    for id in dead {
        ps.extra.remove(&id);
    }

    // Adds: nearest first, bounded by the option cap.
    if !range.is_default() {
        let mut ordered: Vec<(f32, NpcRef)> = set.candidates.into_values().collect();
        ordered.sort_by(|a, b| a.0.total_cmp(&b.0));
        let ctx = server.view_ctx();
        for (_, npc) in ordered {
            if ps.extra.len() >= MAX_EXTRA_NPCS {
                break;
            }
            let id = npc.obj_id;
            if ps.extra.contains_key(&id) {
                continue;
            }
            let obj = WorldObject::Npc(npc.clone());
            if world
                .region_of(&obj)
                .is_some_and(|r| set.natural.contains(&r))
            {
                continue;
            }
            visibility::add_visible_object(&world, &obj, player, &ctx);
            ps.extra.insert(id, npc);
        }
    }
    set.spent
}

/// The player-centred query shared by the two candidate collectors.
struct MarkerQuery {
    center: (f32, f32),
    radius: f32,
    radius2: f32,
}

/// Candidate NPCs and the state shared while collecting them.
struct CandidateSet {
    natural: HashSet<(i32, i32)>,
    candidates: HashMap<u32, (f32, NpcRef)>,
    spent: i64,
}

/// Live NPCs already in the instance whose template is a safe quest giver.
fn collect_live_quest_givers(
    server: &GameServer,
    world: &WorldInstance,
    query: &MarkerQuery,
    set: &mut CandidateSet,
) {
    let residents = server.systems.npcs.spawns.resident_npcs(server);
    let npcs = &server.game_data.systems.npcs.npc;
    for npc in world.all_npcs() {
        let template_id = npc.data().unit.template_id;
        let Some(template) = npcs.template(template_id) else {
            continue;
        };
        if !residents.is_quest_map_candidate(template) {
            continue;
        }
        let obj = WorldObject::Npc(npc.clone());
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

/// Prepares ordinary quest-giver placements around the player and collects their live NPCs.
fn collect_from_spawners(
    server: &GameServer,
    world: &WorldInstance,
    manager: &crate::systems::npcs::spawn_manager::SpawnManager,
    query: &MarkerQuery,
    budget: i64,
    set: &mut CandidateSet,
) {
    let residents = server.systems.npcs.spawns.resident_npcs(server);
    let npcs = &server.game_data.systems.npcs.npc;
    for spawner in manager.quest_giver_spawners_near(
        server,
        query.center.0,
        query.center.1,
        query.radius,
        MAX_SPAWNERS_PER_REFRESH,
    ) {
        if set.spent < budget {
            let want = (budget - set.spent).clamp(0, 4);
            if want > 0 {
                let created = spawner.update_warm(server, true, want);
                set.spent += created as i64;
            }
        }
        for npc in spawner.spawned_npcs().into_values().flatten() {
            let template_id = npc.data().unit.template_id;
            let Some(template) = npcs.template(template_id) else {
                continue;
            };
            if !residents.is_quest_map_candidate(template) {
                continue;
            }
            if npc.data().unit.is_dead() {
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
    use aaemu_commons::PacketStream;

    use super::*;
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::opcodes::sc;
    use crate::systems::npcs::npc::data::NpcTemplate;
    use crate::systems::quests::data::QuestData;
    use crate::systems::quests::model::kind;
    use crate::systems::quests::templates::{ActKind, QuestComponentTemplate, QuestTemplate};
    use crate::test_support::*;
    use crate::world::Entity;

    const QUEST_NPC: u32 = 500;
    const OTHER_NPC: u32 = 600;

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
        n.unit.name = format!("npc{template_id}");
        n.unit.level = 5;
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.transform.set_xyz(x, y, 100.0);
        n
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
        let server = test_server_with(data());
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            0x100,
        );
        client.drain();

        // 250 m away: outside the ordinary 2-region neighbourhood.
        let far = Entity::new(0x500, npc(QUEST_NPC, 1000.0, 1250.0));
        let other = Entity::new(0x600, npc(OTHER_NPC, 1000.0, 1270.0));
        let world = server.world.world(0).expect("main_world");
        let ctx = server.view_ctx();
        world.spawn(&far, &ctx);
        world.spawn(&other, &ctx);
        assert!(client.drain().is_empty(), "both are naturally out of range");

        // 300 m: only the safe quest giver is streamed.
        set_range(&server, &player, Range::Meters300);
        let frames = client.drain();
        assert_eq!(
            frames.iter().map(|f| f.0).collect::<Vec<_>>(),
            vec![sc::SCUnitStatePacket, sc::SCUnitPointsPacket],
            "one quest giver"
        );
        let mut expected = npc(QUEST_NPC, 1000.0, 1250.0);
        let body =
            crate::world::unit_state::npc_unit_state(0x500, &mut expected, "", ctx.now_ms).body;
        assert_eq!(
            frames[0].1, body,
            "SCUnitState body is the NPC's own writer"
        );

        // Back to the default: the extra marker is removed.
        set_range(&server, &player, Range::Meters100);
        assert_eq!(client.drain(), vec![ids_packet(&[0x500])]);

        // Unlimited reaches an NPC far across the instance, still only quest givers.
        let far2 = Entity::new(0x501, npc(QUEST_NPC, 0.0, 2000.0));
        world.spawn(&far2, &ctx);
        client.drain();
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
        let server = test_server_with(data());
        let mut client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(
            &server,
            &client,
            test_character(1, "Quester", 1000.0, 1000.0),
            0x100,
        );
        let world = server.world.world(0).expect("main_world");
        let ctx = server.view_ctx();
        let far = Entity::new(0x500, npc(QUEST_NPC, 1000.0, 1250.0));
        world.spawn(&far, &ctx);
        client.drain();

        set_range(&server, &player, Range::Meters100);
        assert!(client.drain().is_empty());
        assert_eq!(
            server
                .systems
                .quests
                .quest_map
                .range_of(player.character_id),
            Range::Meters100
        );
    }
}
