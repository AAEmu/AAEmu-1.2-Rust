//! Content encounters and their hooks into the shared engine.
//!
//! Each encounter file starts with its keys (`// ---- keys`), followed by its lookups
//! (`// ---- tables`) and behaviour (`// ---- hooks`). Its [`hooks::Encounter`] implementation
//! delegates to the moved free functions. Engine code asks [`registry`] for lookups and
//! dispatches behaviour through [`hooks`] by zone, world or id.
//! Most tables are keyed by an id (template, skill, command set, cinema, clout) and apply in every
//! world, as the code they came from did; only the lookups that tested a zone or a world test one
//! here.

pub mod armory;
pub mod cradle;
pub mod hadir;
pub mod hooks;
pub mod howling_abyss;
mod library;
pub mod palace_cellar;
mod plateau;
pub mod registry;
pub mod sharpwind;

#[cfg(test)]
mod tests;
