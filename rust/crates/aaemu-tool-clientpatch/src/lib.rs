//! Backport bundle pipeline (see `rust/docs/client/BACKPORTING.md`).
//!
//! * [`pakops`]: list / extract / add / restore on ArcheAge `game_pak` files, on top of
//!   `aaemu_commons::aapak`.
//! * [`dbpatch`]: idempotent SQL patch applier for `compact.sqlite3` (records applied patches).
//! * [`bundle`]: a bundle is a directory with `bundle.json`, SQL patches and a list of donor
//!   (10.x) pak files; `build` stages the donor files, `apply` patches a client and/or a
//!   server database.

pub mod bundle;
pub mod dbpatch;
pub mod pakops;

#[cfg(test)]
pub(crate) mod test_util;
