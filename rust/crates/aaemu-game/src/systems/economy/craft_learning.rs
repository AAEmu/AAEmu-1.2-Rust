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
use crate::server::GameServer;
use crate::systems::DataLoadCtx;
use crate::world::PlayerRef;

/// The crafts that need learning and the sources that teach them
/// (`server.game_data.systems.economy.craft_learning`).
#[derive(Debug, Default)]
pub struct CraftRecipes {
    /// Crafts with at least one reachable source. Only these are gated: a craft whose
    /// `item_recipes` item is missing or has no `TrainCraftEffect` use skill, and whose training
    /// skills are not referenced anywhere, would be locked forever.
    pub gated: HashSet<u32>,
    /// `item_recipes`: recipe item template id -> the craft ids it teaches.
    pub item_crafts: HashMap<u32, Vec<u32>>,
    /// `train_craft_effects`: effect row id (`effects.actual_id`) -> craft id.
    pub effect_crafts: HashMap<u32, u32>,
}

/// Player-castable references to a skill (the skill book / a command / a quest grant / a
/// prerequisite). An item `use_skill_id` is not one: an item cast has a source item, so
/// `train_craft` takes the craft from `item_recipes`, never from the effect row.
const TRAINING_REFERENCE_QUERIES: &[&str] = &[
    "SELECT DISTINCT skill_id AS skill_id FROM default_skills WHERE skill_id <> 0",
    "SELECT DISTINCT skill_id AS skill_id FROM slash_commands WHERE skill_id <> 0",
    "SELECT DISTINCT skill_id AS skill_id FROM skill_req_skills WHERE skill_id <> 0",
    "SELECT DISTINCT skill_id AS skill_id FROM quest_act_supply_skills WHERE skill_id <> 0",
    "SELECT DISTINCT skill_id AS skill_id FROM quest_act_con_accept_skills WHERE skill_id <> 0",
];

impl CraftRecipes {
    /// Loads `item_recipes` + `train_craft_effects` and keeps only crafts with a reachable
    /// source. Rows whose craft is missing from `crafts`, or whose item is missing, are
    /// skipped; training skills counted as reachable are the ones another table grants,
    /// requires or commands (none in the shipped 1.2 data). The counts are logged once.
    pub fn load(ctx: &DataLoadCtx<'_>) -> std::result::Result<Self, GameDataError> {
        let mut crafts = HashSet::new();
        for_each_row(ctx.conn, "SELECT id FROM crafts", [], |r| {
            crafts.insert(r.u32("id")?);
            Ok(())
        })?;

        // `effect actual_id` carried by a skill: `skill_effects.effect_id` is the `effects.id`
        // row, whose `actual_id` is the `train_craft_effects.id`.
        let mut effect_skills: HashMap<u32, HashSet<u32>> = HashMap::new();
        let mut skills_by_effect: HashMap<u32, HashSet<u32>> = HashMap::new();
        for_each_row(
            ctx.conn,
            "SELECT DISTINCT se.skill_id AS skill_id, e.actual_id AS effect_id \
             FROM skill_effects se JOIN effects e ON e.id = se.effect_id \
             WHERE e.actual_type = 'TrainCraftEffect'",
            [],
            |r| {
                let skill_id = r.u32("skill_id")?;
                let effect_id = r.u32("effect_id")?;
                effect_skills.entry(skill_id).or_default().insert(effect_id);
                skills_by_effect
                    .entry(effect_id)
                    .or_default()
                    .insert(skill_id);
                Ok(())
            },
        )?;

        let mut castable_skills = HashSet::new();
        for query in TRAINING_REFERENCE_QUERIES {
            for_each_row(ctx.conn, query, [], |r| {
                castable_skills.insert(r.u32("skill_id")?);
                Ok(())
            })?;
        }

        let mut d = Self::default();
        let mut candidates = HashSet::new();
        let mut item_gated = HashSet::new();
        let mut training_gated = HashSet::new();
        let mut skipped = 0usize;

        for_each_row(
            ctx.conn,
            "SELECT r.item_id AS item_id, r.craft_id AS craft_id, \
                    (i.id IS NOT NULL) AS item_exists, COALESCE(i.use_skill_id, 0) AS use_skill_id \
             FROM item_recipes r LEFT JOIN items i ON i.id = r.item_id",
            [],
            |r| {
                let item_id = r.u32("item_id")?;
                let craft_id = r.u32("craft_id")?;
                if !r.bool("item_exists")? || !crafts.contains(&craft_id) {
                    skipped += 1;
                    return Ok(());
                }
                candidates.insert(craft_id);
                let list = d.item_crafts.entry(item_id).or_default();
                if !list.contains(&craft_id) {
                    list.push(craft_id);
                }
                let use_skill = r.u32("use_skill_id")?;
                if use_skill != 0 && effect_skills.contains_key(&use_skill) {
                    item_gated.insert(craft_id);
                }
                Ok(())
            },
        )?;

        for_each_row(
            ctx.conn,
            "SELECT id, craft_id FROM train_craft_effects",
            [],
            |r| {
                let effect_id = r.u32("id")?;
                let craft_id = r.u32("craft_id")?;
                if !crafts.contains(&craft_id) {
                    skipped += 1;
                    return Ok(());
                }
                candidates.insert(craft_id);
                d.effect_crafts.insert(effect_id, craft_id);
                let castable = skills_by_effect.get(&effect_id).is_some_and(|skills| {
                    skills.iter().any(|skill| castable_skills.contains(skill))
                });
                if castable {
                    training_gated.insert(craft_id);
                }
                Ok(())
            },
        )?;

        d.gated = item_gated.union(&training_gated).copied().collect();
        for list in d.item_crafts.values_mut() {
            list.sort_unstable();
        }
        tracing::info!(
            gated_crafts = d.gated.len(),
            item_sources = item_gated.len(),
            training_sources = training_gated.len(),
            skipped_rows = skipped,
            unreachable_crafts = candidates.len().saturating_sub(d.gated.len()),
            "craft learning: loaded item_recipes / train_craft_effects"
        );
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

    /// Whether the craft must be learned before it can be executed.
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

/// Teaches the craft(s) of a `TrainCraftEffect` to the player. An item cast (`item_template_id`
/// from `SkillCaster::Item`) uses its `item_recipes` row; only a non-item cast falls back to the
/// effect's own craft id (plain training skills). The check and the insert happen under one
/// character lock.
pub fn train_craft(
    server: &GameServer,
    player: &PlayerRef,
    effect_craft_id: u32,
    item_template_id: Option<u32>,
) -> TrainOutcome {
    let economy = &server.game_data.systems.economy;
    let crafts = match item_template_id {
        Some(template_id) => match economy.craft_learning.crafts_for_item(template_id) {
            Some(crafts) => crafts.to_vec(),
            None => return TrainOutcome::NoRecipe { item_template_id },
        },
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
        if character.learn_craft(craft_id) {
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

/// Persists the crafts learned since the last committed save (`Character.Save`), in one
/// multi-row statement. `learned_at` is UTC; a retried snapshot updates the row instead of
/// relying on `INSERT IGNORE`.
pub async fn save_character_crafts(
    tx: &mut Transaction<'_, MySql>,
    character_id: u32,
    crafts: &HashSet<u32>,
) -> Result<()> {
    if crafts.is_empty() {
        return Ok(());
    }
    let mut ids: Vec<u32> = crafts.iter().copied().collect();
    ids.sort_unstable();
    let values = vec!["(?, ?, UTC_TIMESTAMP())"; ids.len()].join(", ");
    let sql = format!(
        "INSERT INTO `character_crafts` (`character_id`, `craft_id`, `learned_at`) VALUES {values} \
         ON DUPLICATE KEY UPDATE `learned_at` = VALUES(`learned_at`)"
    );
    let mut query = sqlx::query(&sql);
    for craft_id in ids {
        query = query.bind(character_id).bind(craft_id);
    }
    query.execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
#[path = "craft_learning_tests.rs"]
mod craft_learning_tests;
