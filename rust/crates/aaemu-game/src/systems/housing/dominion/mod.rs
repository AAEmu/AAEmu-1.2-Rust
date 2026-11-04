//! Persisted territory authority. The 1.2 calendar tables are static reference data;
//! [`calendar`] derives the weekly periods from them, while scheduling a siege explicitly
//! (GM override) remains a separate administrative operation.

pub mod brief;
pub mod calendar;
pub mod db;
pub mod fort;
pub mod headquarters;
pub mod participants;
pub mod peace;
pub mod period;
pub mod reinforce;
pub mod rights;
pub mod runtime;
pub mod state;
pub mod tax;

pub use state::{
    DominionError, DominionManager, DominionState, EngravingHandle, ParticipantCap, SiegeOutcome,
    SiegeParticipant, SiegeSide, SiegeState, SiegeStatus, TowerIdentity, Treasury,
};

#[cfg(test)]
mod fort_tests;
#[cfg(test)]
mod headquarters_phase_tests;
#[cfg(test)]
mod headquarters_tests;
#[cfg(test)]
mod participants_tests;
#[cfg(test)]
mod rights_tests;
#[cfg(test)]
mod save_tests;
#[cfg(test)]
mod tax_tests;
#[cfg(test)]
mod tests;
