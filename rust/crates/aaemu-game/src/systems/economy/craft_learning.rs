//! Recipe learning (`TrainCraftEffect` / recipe items) and the learned-craft gate.
//!
//! The 1.2 client loads `SELECT item_id, craft_id FROM item_recipes` itself (X2G:1478514) and
//! `SELECT id, craft_id FROM train_craft_effects` (X2G:1574676); the server's copy is
//! [`CraftRecipes`]. A recipe item (`items.impl_id = 12`) carries the generic use skill 11144 /
//! 10003 whose effect is `TrainCraftEffect`, so the craft to learn comes from
//! `item_recipes[source item template]`; a skill with no source item (the plain training
//! skills) falls back to `train_craft_effects[effect.actual_id]`.
//!
//! Learned crafts are stored on [`crate::models::character::Character::learned_crafts`], loaded
//! and saved with the character (`character_crafts`). The client carrier for the learned set
//! was not found (WP-I1 UNKNOWN-2): the only writes to the client's `item_recipes` arrays are its
//! own loader, and `CRAFT_TRAINED`/`CRAFT_RECIPE_ADDED` appear only in the event-name
//! registration (`FUN_39214380`, indices 117/118). The server therefore keeps the state correct
//! and sends no announcement; a capture of the original client is pending.

use std::collections::{HashMap, HashSet};

use sqlx::mysql::MySqlPool;
use sqlx::{MySql, Transaction};

use crate::data::sqlite::for_each_row;
use crate::data::GameDataError;
use crate::error::Result;
use crate::models::item::Item;
use crate::server::GameServer;
use crate::systems::DataLoadCtx;
use crate::world::PlayerRef;

/// The crafts that need learning and the sources that teach them
/// (`server.game_data.systems.economy.craft_learning`).
#[derive(Debug, Default)]
pub struct CraftRecipes {
    /// Crafts that appear in `item_recipes` or `train_craft_effects` (`crafts.id`).
    pub gated: HashSet<u32>,
    /// `item_recipes`: recipe item template id -> the craft ids it teaches.
    pub item_crafts: HashMap<u32, Vec<u32>>,
    /// `train_craft_effects`: effect row id (`effects.actual_id`) -> craft id.
    pub effect_crafts: HashMap<u32, u32>,
}

impl CraftRecipes {
    /// Loads `item_recipes` + `train_craft_effects`. Rows whose craft is missing from `crafts`
    /// are skipped with one warning (49 recipe rows / 1 effect row in the shipped compact DB).
    pub fn load(ctx: &DataLoadCtx<'_>) -> std::result::Result<Self, GameDataError> {
        let mut crafts = HashSet::new();
        for_each_row(ctx.conn, "SELECT id FROM crafts", [], |r| {
            crafts.insert(r.u32("id")?);
            Ok(())
        })?;

        let mut d = Self::default();
        let mut skipped = 0usize;
        for_each_row(
            ctx.conn,
            "SELECT item_id, craft_id FROM item_recipes",
            [],
            |r| {
                let item_id = r.u32("item_id")?;
                let craft_id = r.u32("craft_id")?;
                if !crafts.contains(&craft_id) {
                    skipped += 1;
                    return Ok(());
                }
                d.gated.insert(craft_id);
                let list = d.item_crafts.entry(item_id).or_default();
                if !list.contains(&craft_id) {
                    list.push(craft_id);
                }
                Ok(())
            },
        )?;
        for_each_row(
            ctx.conn,
            "SELECT id, craft_id FROM train_craft_effects",
            [],
            |r| {
                let id = r.u32("id")?;
                let craft_id = r.u32("craft_id")?;
                if !crafts.contains(&craft_id) {
                    skipped += 1;
                    return Ok(());
                }
                d.gated.insert(craft_id);
                d.effect_crafts.insert(id, craft_id);
                Ok(())
            },
        )?;
        if skipped > 0 {
            tracing::warn!(
                skipped,
                "craft learning: skipped rows whose craft is not in `crafts`"
            );
        }
        for list in d.item_crafts.values_mut() {
            list.sort_unstable();
        }
        Ok(d)
    }

    /// The crafts an item recipe teaches (`None` = the item is not a recipe item).
    pub fn crafts_for_item(&self, item_template_id: u32) -> Option<&[u32]> {
        self.item_crafts.get(&item_template_id).map(Vec::as_slice)
    }

    /// The craft of a `TrainCraftEffect` row (`None` = no such effect row / skipped row).
    pub fn craft_for_effect(&self, effect_id: u32) -> Option<u32> {
        self.effect_crafts.get(&effect_id).copied()
    }

    /// Whether the craft is in the learn-gated set (`item_recipes` or `train_craft_effects`).
    pub fn requires_learning(&self, craft_id: u32) -> bool {
        self.gated.contains(&craft_id)
    }
}

/// Result of applying a `TrainCraftEffect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrainOutcome {
    /// The crafts were inserted into the character's learned set.
    Learned(Vec<u32>),
    /// Every craft of the source was already learned; the item must not be consumed.
    AlreadyLearned,
    /// The item recipe / effect craft is missing from the data; the item must not be consumed.
    NoRecipe { item_template_id: Option<u32> },
}

/// Teaches the craft(s) of a `TrainCraftEffect` to the player. `source_item` is the item cast's
/// source (a recipe item); without one the effect's own craft id applies (plain training
/// skills). The check and the insert happen under one character lock.
pub fn train_craft(
    server: &GameServer,
    player: &PlayerRef,
    effect_craft_id: u32,
    source_item: Option<&Item>,
) -> TrainOutcome {
    let economy = &server.game_data.systems.economy;
    let mut item_template_id = None;
    let crafts = match source_item {
        Some(item) => {
            item_template_id = Some(item.template_id);
            match economy.craft_learning.crafts_for_item(item.template_id) {
                Some(crafts) => crafts.to_vec(),
                None => return TrainOutcome::NoRecipe { item_template_id },
            }
        }
        None => vec![effect_craft_id],
    };
    let valid: Vec<u32> = crafts
        .into_iter()
        .filter(|craft_id| economy.craft.craft(*craft_id).is_some())
        .collect();
    if valid.is_empty() {
        return TrainOutcome::NoRecipe { item_template_id };
    }

    let mut character = player.data();
    let mut learned = Vec::new();
    for craft_id in valid {
        if character.learned_crafts.insert(craft_id) {
            learned.push(craft_id);
        }
    }
    if learned.is_empty() {
        TrainOutcome::AlreadyLearned
    } else {
        TrainOutcome::Learned(learned)
    }
}

/// Whether `craft_id` may be executed at this moment. Ungated crafts and the default
/// configuration (`Crafting.RequireLearnedRecipes = false`) always pass.
pub fn craft_allowed(server: &GameServer, player: &PlayerRef, craft_id: u32) -> bool {
    if !server.config.crafting.require_learned_recipes
        || !server
            .game_data
            .systems
            .economy
            .craft_learning
            .requires_learning(craft_id)
    {
        return true;
    }
    player.data().learned_crafts.contains(&craft_id)
}

/// The character's learned crafts (`Character.Load`).
pub async fn load_character_crafts(pool: &MySqlPool, character_id: u32) -> Result<HashSet<u32>> {
    let mut crafts = HashSet::new();
    let rows = sqlx::query(
        "SELECT CAST(`craft_id` AS SIGNED) AS `craft_id` FROM `character_crafts` \
         WHERE `character_id` = ?",
    )
    .bind(character_id)
    .fetch_all(pool)
    .await?;
    for row in &rows {
        crafts.insert(crate::db::int(row, "craft_id")? as u32);
    }
    Ok(crafts)
}

/// Persists the learned set (`Character.Save`). `INSERT IGNORE` keeps the first `learned_at`
/// and is a no-op for a craft that is already stored.
pub async fn save_character_crafts(
    tx: &mut Transaction<'_, MySql>,
    character_id: u32,
    crafts: &HashSet<u32>,
) -> Result<()> {
    let mut ids: Vec<u32> = crafts.iter().copied().collect();
    ids.sort_unstable();
    for craft_id in ids {
        sqlx::query(
            "INSERT IGNORE INTO `character_crafts` (`character_id`, `craft_id`, `learned_at`) \
             VALUES (?, ?, NOW())",
        )
        .bind(character_id)
        .bind(craft_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "craft_learning_tests.rs"]
mod craft_learning_tests;
