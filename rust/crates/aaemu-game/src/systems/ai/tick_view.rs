//! Per-tick position cache of the AI perception scans.
//!
//! `Behavior.CheckAggression` / `CheckAlert` / `CheckForTalk` call `WorldManager.GetAround` two
//! or three times per NPC tick. Every call re-reads the region grid and world position of every
//! object in the region under its own object lock, so with N NPCs in one region the scans cost
//! O(N²) position reads per tick (the measured dominant cost of the 1500-candidate cloud run).
//!
//! During one AI pass ([`begin`] around `process_plan`) the first query of a region records that
//! region's objects and their world positions; later queries of the same region filter the
//! recorded entries. Object membership per region is still read from the grid, once per region.
//! The cache lives on the thread running the pass (chunk jobs of a parallel batch each have
//! their own) and is dropped when the pass ends, so code outside the tick (skills, packets,
//! physics, GM commands) always reads live positions.
//!
//! Deviation from the C# (PORTING.md): within one 100 ms tick every NPC perceives the others at
//! the positions of the first scan of that region in the same pass, instead of a slightly
//! different instant per NPC. The scan rules themselves are unchanged.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::models::transform::PositionAndRotation;
use crate::server::GameServer;
use crate::systems::skills::geometry::{self, Target};
use crate::world::{WorldInstance, WorldObject};

/// One object of a cached region: the handle, its world position at cache time and the lazily
/// resolved model size (`ModelSize` is only read by the `useModelSize` queries).
struct Entry {
    obj: WorldObject,
    pos: PositionAndRotation,
    model_size: Cell<Option<f32>>,
}

impl Entry {
    fn model_size(&self, server: &GameServer) -> f32 {
        if let Some(ms) = self.model_size.get() {
            return ms;
        }
        let ms = geometry::model_size(server, &Target::Obj(self.obj.clone()));
        self.model_size.set(Some(ms));
        ms
    }
}

#[derive(Default)]
struct View {
    /// `(instance, region x, region y) -> objects` in `objects_in_region` order.
    regions: HashMap<(u32, i32, i32), Rc<Vec<Entry>>>,
}

thread_local! {
    static VIEW: RefCell<Option<View>> = const { RefCell::new(None) };
}

/// Whether an AI pass is running on this thread (a tick view is installed).
fn active() -> bool {
    VIEW.with(|v| v.borrow().is_some())
}

/// Installs an empty tick view for the duration of an AI pass (`process_plan`).
pub struct Guard;

/// Starts a tick view on the current thread; dropping the guard clears it (also on panic).
pub fn begin() -> Guard {
    VIEW.with(|v| *v.borrow_mut() = Some(View::default()));
    Guard
}

impl Drop for Guard {
    fn drop(&mut self) {
        VIEW.with(|v| *v.borrow_mut() = None);
    }
}

/// The objects of `region` with their cached world positions, building the region on its first
/// query in this pass. `None` when no tick view is installed.
fn region_entries(world: &WorldInstance, region: (i32, i32)) -> Option<Rc<Vec<Entry>>> {
    VIEW.with(|v| {
        let mut view = v.borrow_mut();
        let view = view.as_mut()?;
        let key = (world.id, region.0, region.1);
        if let Some(entries) = view.regions.get(&key) {
            return Some(entries.clone());
        }
        let entries: Vec<Entry> = world
            .objects_in_region(region)
            .into_iter()
            .map(|obj| {
                let pos = world.world_pos_rot(&obj);
                Entry {
                    obj,
                    pos,
                    model_size: Cell::new(None),
                }
            })
            .collect();
        let entries = Rc::new(entries);
        view.regions.insert(key, entries.clone());
        Some(entries)
    })
}

/// `WorldManager.GetAround` over the tick view: the same region selection, radius and
/// `useModelSize` rules as [`geometry::around_radius`], evaluated on the cached positions.
/// `None` when no tick view is installed (the caller reads live positions).
pub fn around_entries(
    server: &GameServer,
    origin: &WorldObject,
    radius: f32,
    use_model_size: bool,
) -> Option<Vec<(WorldObject, PositionAndRotation)>> {
    if !active() {
        return None;
    }
    let mut out = Vec::new();
    if radius <= 0.0 {
        return Some(out);
    }
    let Some(world) = geometry::target_world(server, &Target::Obj(origin.clone())) else {
        return Some(out);
    };
    let pos = geometry::world_pos(server, &Target::Obj(origin.clone()));
    let Some(region) = world
        .region_of(origin)
        .filter(|_| origin.region().is_some())
    else {
        return Some(out);
    };
    let mut radius = radius;
    if use_model_size {
        radius += geometry::model_size(server, &Target::Obj(origin.clone()));
    }
    let regions = if geometry::radius_fits_region(pos.x, pos.y, radius) {
        vec![region]
    } else {
        world.neighbors(region)
    };
    let exclude = origin.obj_id();
    let sq = radius * radius;
    for r in regions {
        let entries = region_entries(&world, r)?;
        for entry in entries.iter() {
            if entry.obj.obj_id() == exclude {
                continue;
            }
            let mut final_rad = sq;
            if use_model_size {
                let ms = entry.model_size(server);
                final_rad += ms * ms;
            }
            let dx = (entry.pos.x - pos.x) * (entry.pos.x - pos.x);
            if dx > final_rad {
                continue;
            }
            let dy = (entry.pos.y - pos.y) * (entry.pos.y - pos.y);
            if dx + dy < final_rad {
                out.push((entry.obj.clone(), entry.pos));
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::npc::Npc;
    use crate::test_support::*;
    use crate::world::Entity;

    /// The cached scan returns the same objects, in the same order, as the live one.
    #[tokio::test]
    async fn tick_view_matches_the_live_around() {
        let server = test_server();
        let world = server.world.world(0).expect("world");
        let ctx = server.view_ctx();
        let mut objects = Vec::new();
        for i in 0..40u32 {
            let mut n = Npc::new(1);
            n.unit.transform.set_xyz(
                100.0 + (i % 8) as f32 * 3.0,
                100.0 + (i / 8) as f32 * 3.0,
                100.0,
            );
            n.unit.transform.zone_id = TEST_ZONE;
            let npc = Entity::new(0x500 + i, n);
            world.spawn(&npc, &ctx);
            objects.push(npc);
        }
        let origin = WorldObject::from(&objects[20]);
        let live = super::super::npc::around(&server, &origin, 12.0, false);
        let guard = begin();
        let cached = super::super::npc::around(&server, &origin, 12.0, false);
        drop(guard);
        assert_eq!(
            live.iter().map(WorldObject::obj_id).collect::<Vec<_>>(),
            cached.iter().map(WorldObject::obj_id).collect::<Vec<_>>()
        );
    }

    /// Outside a pass the view is empty again: no stale positions survive the tick.
    #[tokio::test]
    async fn tick_view_is_dropped_with_the_guard() {
        let server = test_server();
        let world = server.world.world(0).expect("world");
        let mut n = Npc::new(1);
        n.unit.transform.set_xyz(100.0, 100.0, 100.0);
        n.unit.transform.zone_id = TEST_ZONE;
        let npc = Entity::new(0x600, n);
        world.spawn(&npc, &server.view_ctx());
        let origin = WorldObject::from(&npc);
        {
            let _guard = begin();
            assert!(around_entries(&server, &origin, 5.0, false).is_some());
        }
        assert!(around_entries(&server, &origin, 5.0, false).is_none());
    }
}
