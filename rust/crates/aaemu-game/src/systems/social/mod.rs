//! Phase-3 subsystem: team / expedition / family / friend / duel / crime / trial, chat
//! channels and faction relations.
//!
//! C# scope: `TeamManager` (+ `Models/Game/Team`), `ExpeditionManager` (+ `Models/Game/
//! Expeditions`), `FamilyManager` (+ `Models/Game/Family`), `FriendMananger` (+ the character
//! friend / blocked lists), `DuelManager` (+ `Models/Game/Duels`, duel tasks), `CrimeManager` /
//! `TrialManager` (+ `Models/Game/Crime`, trial tasks), `ChatManager` (+ `Models/Game/Chat`),
//! the relation part of `FactionManager` and `Unit/Character.SetFaction`, and their packets.
//!
//! Module layout (see `CONTRIBUTING-PORT.md`): managers in their own files, packets under
//! `packets/` (one file per area), configuration in `config.rs`, cross-subsystem services in
//! `hooks.rs`, the session call sites in `session.rs`. `managers::{chat, expedition, friend}`
//! re-export the moved phase-2 managers (`server.chat`, `server.expeditions`, `server.friends`).

pub mod achievement;
pub mod achievement_hooks;
pub mod achievement_manager;
pub mod achievement_persistence;
pub mod achievement_rewards;
pub mod achievement_runtime;
pub mod bounty;
pub mod bounty_runtime;
pub mod bridges;
pub mod chat;
pub mod config;
pub mod crime;
pub mod dominion;
pub mod dominion_notifications;
pub mod duel;
pub mod expedition;
pub mod faction;
pub mod family;
pub mod friend;
pub mod hooks;
pub mod nation;
pub mod packets;
pub mod ranking_data;
pub mod ranking_manager;
pub mod ranking_packets;
pub mod ranking_persistence;
pub mod ranking_runtime;
pub mod rename;
pub mod session;
pub mod team;
pub mod trial;
pub mod ui_packets;
pub mod util;
pub mod zone_conflict;

#[cfg(test)]
pub(crate) mod achievement_flow_tests;
#[cfg(test)]
mod achievement_game_data_tests;
#[cfg(test)]
mod bounty_flow_tests;
#[cfg(test)]
mod expedition_rename_tests;
#[cfg(test)]
mod flow_tests;
#[cfg(test)]
mod friend_flow_tests;
#[cfg(test)]
mod guild_flow_tests;
#[cfg(test)]
mod manager_tests;
#[cfg(test)]
mod nation_stage4_tests;
#[cfg(test)]
mod nation_tests;
#[cfg(test)]
pub(crate) mod ranking_flow_tests;
#[cfg(test)]
mod trial_flow_tests;

use std::sync::Arc;

use crate::net::registry::PacketRegistry;
use crate::server::GameServer;
use crate::systems::{DataLoadCtx, SaveStage};

/// Game-data tables of this subsystem (`server.game_data.systems.social`).
#[derive(Debug, Default)]
pub struct Data {
    /// `SystemFaction.Relations` (`FactionManager.Load`, relation part).
    pub relations: faction::FactionRelations,
    /// `AchievementGameData.Load`.
    pub achievements: achievement::AchievementGameData,
    /// `ranks` / `rank_scopes` / `rank_rewards` and their links (`RankingGameData.Load`).
    pub ranking: ranking_data::RankingGameData,
    /// `content_configs` 46 / 50 / 51 / 54 (nation diplomacy periods and the treaty cost).
    pub nation: nation::NationSettings,
}

impl Data {
    /// Loads this subsystem's tables from `ctx`.
    pub fn load(ctx: &DataLoadCtx<'_>) -> Result<Self, crate::data::GameDataError> {
        Ok(Self {
            relations: faction::FactionRelations::load(ctx)?,
            achievements: achievement::AchievementGameData::load(ctx.conn)?,
            ranking: ranking_data::RankingGameData::load(ctx)?,
            nation: nation::NationSettings::load(ctx.conn)?,
        })
    }
}

/// Managers of this subsystem (`server.systems.social`).
#[derive(Debug, Default)]
pub struct System {
    /// Serialises every faction-name change across the expedition and nation stores
    /// (`rename::lock_names`, RENAME-6).
    pub(crate) name_lock: std::sync::Mutex<()>,
    pub teams: team::TeamManager,
    pub families: family::FamilyManager,
    /// Runtime player nations (`nations` / `nation_relations`).
    pub nations: nation::NationManager,
    pub duels: duel::DuelManager,
    pub crimes: crime::CrimeManager,
    pub trials: trial::TrialManager,
    /// The wanted-player bounty ledger, its placement permits and its persistence.
    pub bounties: bounty::BountyManager,
    /// `ZoneManager.GetConflicts()` runtime state (`ZoneConflict`).
    pub conflicts: zone_conflict::ZoneConflictManager,
    /// The ranking boards' score store (`ranking_manager`).
    pub ranking: ranking_manager::RankingManager,
    /// Services other subsystems provide (`hooks::SocialHooks`).
    pub hooks: hooks::SocialHookSlot,
}

impl System {
    /// Startup after the phase-1/2 managers: DB loads, timers, hook installation.
    pub async fn start(server: &Arc<GameServer>) -> anyhow::Result<()> {
        use anyhow::Context;
        install_hooks(server);
        // ZoneManager.Load: SetState(Conflict) of the open zones schedules their timers.
        zone_conflict::ZoneConflictManager::start(server);
        // Player nations and their relation overlay.
        server
            .systems
            .social
            .nations
            .load(&server.db)
            .await
            .context("NationManager")?;
        nation::rebuild_treaty_mail_tracking(server)
            .await
            .context("Nation treaty mail tracking")?;
        nation::start_tick(server);
        server
            .systems
            .social
            .families
            .load(&server.db)
            .await
            .context("FamilyManager")?;
        team::TeamManager::start_tick(server);
        expedition::start_tick(server);
        // CrimeManager.Load, TrialManager.Load (court rooms + the 5 s TrialUpdateTask).
        server.systems.social.crimes.load(server).await?;
        server
            .systems
            .social
            .bounties
            .load(&server.db)
            .await
            .context("BountyManager")?;
        server.systems.social.trials.load(server);
        trial::TrialManager::start_tick(server);
        // Ranking scores (`character_rank_scores`) and the closed-period payout ledger.
        server
            .systems
            .social
            .ranking
            .load(&server.db)
            .await
            .context("RankingManager")?;
        ranking_manager::RankingManager::start_tick(server);
        Ok(())
    }

    /// This subsystem's part of `SaveManager.DoSave` (inside its transaction). Returns the
    /// number of rows written.
    pub async fn save(
        &self,
        server: &GameServer,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        stage: SaveStage,
    ) -> crate::Result<usize> {
        if matches!(stage, SaveStage::AfterCharacters) {
            let mut written = self.ranking.save(tx).await?;
            written += self.nations.save(server, tx).await?;
            written += self.bounties.save(tx).await?;
            return Ok(written);
        }
        if matches!(stage, SaveStage::AfterItems) {
            // SaveManager.DoSave: `CrimeManager.Instance.Save(connection, transaction)`.
            let (updated, deleted) = self.crimes.save(tx).await?;
            return Ok(updated + deleted);
        }
        Ok(0)
    }
}

/// Registers this subsystem's C->S packets.
pub fn register(reg: &mut PacketRegistry) {
    packets::register(reg);
    ranking_manager::register(reg);
}

/// Installs the social subsystem's hooks (its implementations of the other subsystems' hooks
/// and its own `SocialHooks`). No I/O; also used by the flow tests.
pub fn install_hooks(server: &GameServer) {
    bridges::install(server);
}
