//! Persisted territory authority. The 1.2 calendar tables are static reference data;
//! [`calendar`] derives the weekly periods from them, while scheduling a siege explicitly
//! (GM override) remains a separate administrative operation.

pub mod auction_post;
pub mod brief;
pub mod calendar;
pub mod credit;
pub mod db;
pub mod declare;
pub mod fort;
pub mod headquarters;
pub mod hostility;
pub mod larceny;
pub mod participants;
pub mod peace;
pub mod period;
pub mod reinforce;
pub mod rights;
pub mod runtime;
pub mod seals;
pub mod siege_area;
pub mod siege_items;
pub mod state;
pub mod tax;

pub use state::{
    DominionError, DominionManager, DominionState, EngravingHandle, ParticipantCap, SiegeLimits,
    SiegeOutcome, SiegeParticipant, SiegeSide, SiegeState, SiegeStatus, TowerIdentity, Treasury,
};

#[cfg(test)]
mod declare_tests;
#[cfg(test)]
mod fort_tests;
#[cfg(test)]
mod headquarters_phase_tests;
#[cfg(test)]
mod headquarters_tests;
#[cfg(test)]
mod hostility_tests;
#[cfg(test)]
mod larceny_tests;
#[cfg(test)]
mod participants_tests;
#[cfg(test)]
mod rights_tests;
#[cfg(test)]
mod save_tests;
#[cfg(test)]
mod seals_tests;
#[cfg(test)]
mod siege_area_tests;
#[cfg(test)]
mod tax_tests;
#[cfg(test)]
mod tests;
