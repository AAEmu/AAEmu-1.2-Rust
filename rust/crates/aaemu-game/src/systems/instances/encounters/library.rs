//! Library startup spawns.

// ---- keys

pub(super) const WORLD: &str = "instance_library";

// ---- tables

/// `instance_library` is excluded from the inactive-spawner filter: only 3 of its 49 'f' spawner ids have
/// a schedule binding and no spawn effect or quest component names the other 46.
pub(super) fn keeps_inactive_spawners(world_name: &str) -> bool {
    world_name == WORLD
}

// ---- hooks

pub(super) struct Library;

impl super::hooks::Encounter for Library {}
