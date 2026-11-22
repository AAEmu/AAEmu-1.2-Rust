//! Dungeon encounter tables: the template, skill, command-set, cinema, clout and buff ids that
//! the shared engine treats specially for one dungeon.
//!
//! Each dungeon has its own file. It starts with the dungeon's keys (`// ---- keys`: its zone and
//! world template) and then holds the id tables (`// ---- tables`), with the original id lists
//! and their comments. Engine code does not call the dungeon files: it asks [`registry`], the
//! facade, which delegates to the dungeons a lookup concerns.
//!
//! Most tables are keyed by an id (template, skill, command set, cinema, clout) and apply in every
//! world, as the code they came from did; only the lookups that tested a zone or a world test one
//! here. The behaviour that goes with some of these ids (geometry, motion, cast routing) is still
//! in the engine files.

pub mod armory;
pub mod cradle;
pub mod hadir;
pub mod howling_abyss;
pub mod palace_cellar;
pub mod registry;
pub mod sharpwind;

#[cfg(test)]
mod tests;
