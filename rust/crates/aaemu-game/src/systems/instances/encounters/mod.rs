//! Dungeon encounters: the template, skill, command-set, cinema, clout and buff ids that the
//! shared engine treats specially for one dungeon, and the dungeon behaviour that goes with them.
//!
//! Each dungeon has its own file. It starts with the dungeon's keys (`// ---- keys`: its zone and
//! world template), then holds the id tables (`// ---- tables`), with the original id lists and
//! their comments, and ends with its behaviour (`// ---- hooks`): the free functions moved from
//! the engine and the dungeon's [`hooks::Encounter`] implementation, which only delegates to
//! them. Engine code does not call the dungeon files for a lookup: it asks [`registry`], the
//! facade, which delegates to the dungeons a lookup concerns. It runs the behaviour through
//! [`hooks`], which picks the dungeons by zone, by world or (for id-keyed hooks) all of them.
//!
//! Most tables are keyed by an id (template, skill, command set, cinema, clout) and apply in every
//! world, as the code they came from did; only the lookups that tested a zone or a world test one
//! here.

pub mod armory;
pub mod cradle;
pub mod hadir;
pub mod hooks;
pub mod howling_abyss;
pub mod palace_cellar;
pub mod registry;
pub mod sharpwind;

#[cfg(test)]
mod tests;
