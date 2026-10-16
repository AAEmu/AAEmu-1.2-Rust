//! Housing rebuild ("upgrade") backported from the 10.x client: turn an owned, finished house
//! into another housing template **in place** (same `House` entity: id, object id, owner,
//! position, permissions, dates, furniture and chests stay), paying materials and labor.
//! Design: `rust/docs/client/backports/HOUSING.md`.
//!
//! | module | what |
//! |---|---|
//! | [`data`] | the `housing_rebuilding*` tables (optional: absent in vanilla 1.2 data) |
//! | [`ops`] | validation, options listing and the in-place template swap |
//! | [`preview`] | the per-client 3D preview (a phantom house only the requester sees) |
//! | [`channels`] | the `house.rebuild.*` client-extension channels (`systems::ext`) |
//!
//! The 1.2 client has no rebuild support at all, so the trigger is the extension channel
//! (feature `house.rebuild`) with the `/rebuild` chat command and a GM subcommand as fallbacks.

pub mod channels;
pub mod data;
pub mod ops;
pub mod preview;

pub use channels::register_ext;
pub use data::{RebuildData, RebuildMaterial, Rebuilding};
pub use ops::{force_rebuild, rebuild, HouseView, OptionView, RebuildError};

/// The extension feature name negotiated in `ext.hello` / `ext.welcome`.
pub const FEATURE: &str = "house.rebuild";

/// `explicit house id` / auto-resolve distances (metres).
pub const RESOLVE_DISTANCE: f32 = 30.0;
/// A preview ends when the player moves farther than this from the house.
pub const PREVIEW_MAX_DISTANCE: f32 = 60.0;
/// A preview ends by itself after this long.
pub const PREVIEW_TIMEOUT_MS: i64 = 120_000;
