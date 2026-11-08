//! The rolls of `Models/Game/Items/Loots/LootPack` (`GeneratePack` -> `GeneratePackNewV2`,
//! `GiveLootPack`, `GetGradeFromDistribution`) and `ItemManager.GetLootConvertFish`.
//!
//! Randomness goes through [`LootRng`] (the `System.Random` calls of the C#, one method per
//! overload) so tests can script the rolls. The unused C# variants `GeneratePack(float, float,
//! ICharacter)` (private) and `GeneratePackNew` (never called) are not ported.

use crate::config::WorldConfig;
use crate::data::items::{ItemData, ITEM_COINS};
use crate::models::container::slot_type;
use crate::models::item::{Item, ItemClass, ItemDetails};
use crate::models::unit::UnitAttribute;
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::PlayerRef;

use super::inventory::{self, Inv, ItemTxn};
use super::loot_data::{Loot, LootPack};

/// The `System.Random` overloads the loot code calls.
pub trait LootRng {
    /// `Random.Next(minValue, maxValue)` (`maxValue` exclusive; `minValue` when equal).
    fn next_i32(&mut self, min: i32, max: i32) -> i32;
    /// `Random.NextSingle()` (`[0, 1)`).
    fn next_f32(&mut self) -> f32;
    /// `Random.Next(maxValue)`.
    fn next_below(&mut self, max: i32) -> i32 {
        self.next_i32(0, max)
    }
    /// `RandomExtensions.Next(this Random, float, float)` (what `Random.Shared.Next(0, uintValue)`
    /// binds to in C#).
    fn next_f32_range(&mut self, min: f32, max: f32) -> f32 {
        self.next_f32() * (max - min) + min
    }
}

/// `Random.Shared`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SharedRandom;

impl LootRng for SharedRandom {
    fn next_i32(&mut self, min: i32, max: i32) -> i32 {
        use rand::Rng;
        if min >= max {
            if min > max {
                // C#: ArgumentOutOfRangeException
                tracing::error!("Random.Next({min}, {max}): minValue > maxValue");
            }
            return min;
        }
        rand::thread_rng().gen_range(min..max)
    }

    fn next_f32(&mut self) -> f32 {
        use rand::Rng;
        rand::thread_rng().gen::<f32>()
    }
}

/// A scripted [`LootRng`] (tests / deterministic replays): integer rolls and float rolls are
/// taken from their queues; an exhausted queue yields the lowest possible value.
#[derive(Debug, Clone, Default)]
pub struct ScriptedRng {
    pub ints: std::collections::VecDeque<i32>,
    pub floats: std::collections::VecDeque<f32>,
    /// Every `(min, max)` passed to `next_i32`, in order.
    pub int_calls: Vec<(i32, i32)>,
}

impl ScriptedRng {
    pub fn new(ints: &[i32], floats: &[f32]) -> Self {
        Self {
            ints: ints.iter().copied().collect(),
            floats: floats.iter().copied().collect(),
            int_calls: Vec::new(),
        }
    }
}

impl LootRng for ScriptedRng {
    fn next_i32(&mut self, min: i32, max: i32) -> i32 {
        self.int_calls.push((min, max));
        let v = self.ints.pop_front().unwrap_or(min);
        if min >= max {
            min
        } else {
            v.clamp(min, max - 1)
        }
    }

    fn next_f32(&mut self) -> f32 {
        self.floats.pop_front().unwrap_or(0.0)
    }
}

/// One generated loot entry (`(itemId, count, grade, lootGroupOrigin)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeneratedLoot {
    pub item_id: u32,
    pub count: i32,
    pub grade: u8,
    pub group: u32,
}

/// Where the C# loot generation throws.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LootError {
    /// `selectedItemsByGroup[groupNo]` when only quest items were selected
    /// (`KeyNotFoundException`).
    #[error("loot pack {pack}: group {group} has quest items but no rolled item")]
    QuestOnlyGroup { pack: u32, group: u32 },
    /// `GetGradeDistributions(id)` returned null (`NullReferenceException`).
    #[error("item grade distribution {0} not found")]
    MissingGradeDistribution(u8),
}

/// What the loot rolls need from the player (`Character` in `GeneratePackNewV2`).
pub trait LootPlayer {
    /// `player.Quests.HasQuest(questId)`.
    fn has_quest(&self, quest_id: u32) -> bool;
    /// `player.Actability.Actabilities[(byte)type].GetLootMultiplier()` (`None` = no entry).
    fn actability_loot_multiplier(&self, actability_type: u8) -> Option<f32>;
}

/// A player as [`LootPlayer`]: quests through the loot hooks, actabilities from its data.
pub struct PlayerLoot<'a> {
    pub server: &'a GameServer,
    pub player: &'a PlayerRef,
}

impl LootPlayer for PlayerLoot<'_> {
    fn has_quest(&self, quest_id: u32) -> bool {
        super::looting::has_quest(self.server, self.player, quest_id)
    }

    fn actability_loot_multiplier(&self, actability_type: u8) -> Option<f32> {
        let c = self.player.data();
        c.actability
            .actabilities
            .iter()
            .find(|a| a.id == u32::from(actability_type))
            .map(|a| inventory::actability_exp_multiplier(a.step))
    }
}

/// The `AppConfiguration.World` loot multipliers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LootConfig {
    pub loot_rate: f64,
    pub gold_loot_multiplier: f64,
}

impl LootConfig {
    pub fn from_world(w: &WorldConfig) -> Self {
        Self {
            loot_rate: w.loot_rate,
            gold_loot_multiplier: w.gold_loot_multiplier,
        }
    }
}

impl Default for LootConfig {
    fn default() -> Self {
        Self {
            loot_rate: 1.0,
            gold_loot_multiplier: 1.0,
        }
    }
}

/// `(100f + player.DropRateMul) / 100f` and `(100f + player.LootGoldMul) / 100f`.
pub fn player_loot_rates(player: &PlayerRef, now_ms: i64) -> (f32, f32) {
    let c = player.data();
    let drop = c
        .unit
        .calculate_with_bonuses(0.0, UnitAttribute::DropRateMul, now_ms) as f32;
    let gold = c
        .unit
        .calculate_with_bonuses(0.0, UnitAttribute::LootGoldMul, now_ms) as f32;
    ((100f32 + drop) / 100f32, (100f32 + gold) / 100f32)
}

/// `LootPack.GetGradeFromDistribution(id)`.
pub fn grade_from_distribution(
    items: &ItemData,
    id: u8,
    rng: &mut dyn LootRng,
) -> Result<u8, LootError> {
    let d = items
        .grade_distributions(id)
        .ok_or(LootError::MissingGradeDistribution(id))?;
    let mut grade = 0u8;
    let mut old = 0i32;
    let drop = rng.next_i32(0, 100);
    for (i, w) in d.weights.iter().enumerate() {
        if drop <= w.wrapping_add(old) {
            grade = i as u8;
            break;
        }
        old = old.wrapping_add(*w);
    }
    Ok(grade)
}

/// `LootPack.GeneratePackNewV2(lootDropRate, lootGoldRate, player, actabilityType,
/// inheritedGrade)`.
#[allow(clippy::too_many_arguments)]
pub fn generate_pack_v2(
    pack: &LootPack,
    items: &ItemData,
    cfg: LootConfig,
    loot_drop_rate: f32,
    loot_gold_rate: f32,
    player: Option<&dyn LootPlayer>,
    actability_type: u8,
    inherited_grade: Option<u8>,
    rng: &mut dyn LootRng,
) -> Result<Vec<GeneratedLoot>, LootError> {
    let mut out = Vec::new();
    let loot_rate = cfg.loot_rate;
    let quest_of = |l: &Loot| items.template(l.item_id).map_or(0, |t| t.loot_quest_id);
    for (group_no, group_loot) in &pack.loots_by_group_no {
        let group_no = *group_no;
        let group = pack.group(group_no);
        // If group is defined, use it's DropRate for group rolls
        let group_rate = match group {
            Some(g) if g.drop_rate > 0 => g.drop_rate as f32 / 10_000_000f32,
            _ => 1f32,
        };
        // selectedItemsByGroup (insertion ordered)
        let mut selected: Vec<(u32, Vec<Loot>)> = Vec::new();
        let push = |selected: &mut Vec<(u32, Vec<Loot>)>, l: Loot| match selected
            .iter_mut()
            .find(|(g, _)| *g == l.group)
        {
            Some((_, v)) => v.push(l),
            None => selected.push((l.group, vec![l])),
        };
        if group_no == 0 {
            for l in group_loot {
                // Check for Quest loot drops
                let q = quest_of(l);
                if q > 0 {
                    if let Some(p) = player {
                        if !p.has_quest(q) {
                            continue;
                        }
                    }
                }
                // Roll each item
                let requires =
                    (f64::from(l.drop_rate as f32 * loot_drop_rate) * loot_rate).floor() as i64;
                let dice = i64::from(rng.next_i32(0, 10_000_000));
                if dice < requires || l.always_drop {
                    push(&mut selected, *l);
                }
            }
        } else {
            // Roll group
            let requires =
                (f64::from(10_000_000f32 * group_rate * loot_drop_rate) * loot_rate).floor() as i64;
            let dice = i64::from(rng.next_i32(0, 10_000_000));
            if dice < requires {
                let mut tmp: Vec<Loot> = Vec::new();
                let mut tmp_quest: Vec<Loot> = Vec::new();
                let mut normalized_rate = 0u32;
                for l in group_loot {
                    let q = quest_of(l);
                    if q > 0 && player.is_some() {
                        // Add item if player has quest
                        if player.is_some_and(|p| p.has_quest(q)) {
                            tmp_quest.push(*l);
                        }
                        continue;
                    }
                    // Check if this loot uses ActAbilityGroup dice
                    if let Some(act) = pack.actability_group(l.group) {
                        let act_dice = i64::from(rng.next_i32(0, 10_000));
                        let act_dice = (act_dice as f64 / (f64::from(loot_drop_rate) * loot_rate))
                            .floor() as i64;
                        let mut mul = 1f32;
                        if let Some(m) =
                            player.and_then(|p| p.actability_loot_multiplier(actability_type))
                        {
                            mul *= m;
                        }
                        if act_dice as f32 * mul > act.max_dice as f32 {
                            continue;
                        }
                    }
                    normalized_rate = normalized_rate.wrapping_add(if l.always_drop {
                        10_000_000
                    } else {
                        l.drop_rate
                    });
                    tmp.push(*l);
                }
                // Roll item (from best chance item to lower chance item) with cumulative rate
                if !tmp.is_empty() {
                    let mut cumulative = 0f32;
                    let roll = rng.next_f32_range(0.0, normalized_rate as f32) as i64;
                    for l in &tmp {
                        let item_rate = if l.drop_rate > 1 {
                            l.drop_rate as f32 / normalized_rate as f32
                        } else {
                            1f32
                        };
                        cumulative +=
                            (f64::from(normalized_rate as f32 * item_rate * loot_drop_rate)
                                * loot_rate)
                                .floor() as i64 as f32;
                        if (roll as f32) < cumulative {
                            push(&mut selected, *l);
                            break;
                        }
                    }
                }
                // Merge quests items in selected items
                if !tmp_quest.is_empty() {
                    let Some((_, list)) = selected.iter_mut().find(|(g, _)| *g == group_no) else {
                        return Err(LootError::QuestOnlyGroup {
                            pack: pack.id,
                            group: group_no,
                        });
                    };
                    for l in tmp_quest {
                        // Skip quest item if it was randomly selected
                        if list.contains(&l) {
                            continue;
                        }
                        list.push(l);
                    }
                }
            }
        }
        // Finalize items loot list
        for (_, loots) in selected {
            for l in loots {
                // Roll amount
                let mut count = rng.next_i32(l.min_amount, l.max_amount.wrapping_add(1));
                if l.item_id == ITEM_COINS {
                    count = (f64::from(count as f32 * loot_gold_rate) * cfg.gold_loot_multiplier)
                        .round_ties_even() as i32;
                }
                let mut grade = inherited_grade.unwrap_or(l.grade_id);
                if inherited_grade.is_none() {
                    if let Some(g) = group.filter(|g| g.item_grade_distribution_id > 0) {
                        grade = grade_from_distribution(items, g.item_grade_distribution_id, rng)?;
                    }
                }
                out.push(GeneratedLoot {
                    item_id: l.item_id,
                    count,
                    grade,
                    group: l.group,
                });
            }
        }
    }
    Ok(out)
}

/// Drops generated loot rows whose item template does not exist. A `loots` row can point at a
/// missing item (185 of the 13 823 rows of the 1.2 compact data); one bad static row must not
/// abort the rest of the pack (C# 10.x PR #1488). The rows are static data, so this is a debug
/// event, not a warning.
pub fn retain_known_items(
    data: &crate::data::items::ItemData,
    pack_id: u32,
    list: Vec<GeneratedLoot>,
) -> Vec<GeneratedLoot> {
    list.into_iter()
        .filter(|g| {
            if g.item_id == ITEM_COINS || data.template(g.item_id).is_some() {
                return true;
            }
            tracing::debug!(
                "Loot pack {pack_id}: skipping unknown item template {}",
                g.item_id
            );
            false
        })
        .collect()
}

/// `LootPack.GeneratePack(player, actabilityType, inheritedGrade)`.
pub fn generate_pack_for_player(
    server: &GameServer,
    pack: &LootPack,
    player: &PlayerRef,
    actability_type: u8,
    inherited_grade: Option<u8>,
    rng: &mut dyn LootRng,
) -> Result<Vec<GeneratedLoot>, LootError> {
    let (drop, gold) = player_loot_rates(player, chrono::Utc::now().timestamp_millis());
    let lp = PlayerLoot { server, player };
    let list = generate_pack_v2(
        pack,
        &server.game_data.items,
        LootConfig::from_world(&server.config.world),
        drop,
        gold,
        Some(&lp),
        actability_type,
        inherited_grade,
        rng,
    )?;
    Ok(retain_known_items(&server.game_data.items, pack.id, list))
}

/// `LootPack.GiveLootPack(character, actabilityType, taskType, generatedList, inheritedGrade)`.
/// Runs inside `txn`. As in C#, a failure after some items were added keeps them.
#[allow(clippy::too_many_arguments)]
pub fn give_loot_pack(
    txn: &mut ItemTxn<'_>,
    player: &PlayerRef,
    pack: &LootPack,
    actability_type: u8,
    task: u8,
    generated: Option<Vec<GeneratedLoot>>,
    inherited_grade: Option<u8>,
    rng: &mut dyn LootRng,
) -> bool {
    let server = txn.server;
    let list = match generated {
        Some(l) => l,
        None => match generate_pack_for_player(
            server,
            pack,
            player,
            actability_type,
            inherited_grade,
            rng,
        ) {
            Ok(l) => l,
            Err(e) => {
                // C#: the exception escapes GiveLootPack.
                tracing::error!("GiveLootPack {}: {e}", pack.id);
                return false;
            }
        },
    };
    // A generated list from another caller may still name a missing item template: skip that
    // row instead of failing the whole pack (C# 10.x PR #1488).
    let list = retain_known_items(txn.data(), pack.id, list);
    let inv = Inv::of(player);
    let money = player.data().money;
    // First check for room
    {
        let bag = inv.bag();
        let bag = bag.lock().expect("container poisoned");
        for g in &list {
            if g.item_id == ITEM_COINS {
                continue;
            }
            if bag.space_left_for_template(g.item_id, txn.data(), money) < g.count {
                return false;
            }
        }
    }
    let mut coins = 0i32;
    for g in &list {
        if g.item_id == ITEM_COINS {
            coins = coins.wrapping_add(g.count);
            continue;
        }
        let Some(t) = txn.data().template(g.item_id) else {
            // The list was filtered above; a template that disappeared since then is skipped
            // like any other unknown row.
            tracing::debug!(
                "GiveLootPack {}: skipping unknown item template {}",
                pack.id,
                g.item_id
            );
            continue;
        };
        let grade = if t.fixed_grade > 0 {
            t.fixed_grade
        } else if g.grade > 1 {
            i32::from(g.grade)
        } else {
            -1
        };
        if !inventory::try_add_new_item(txn, &inv, task, g.item_id, g.count, grade, 0) {
            tracing::error!(
                "Unable to give loot to {} - ItemId: {} x {} at grade {grade} (loot grade {})",
                player.data().unit.name,
                g.item_id,
                g.count,
                g.grade
            );
            return false;
        }
        // GET_LOOTITEM: an item actually handed over by a loot roll (deferred until the item
        // transaction is released).
        crate::systems::report_achievement(
            server,
            player,
            AchievementInput::Increment {
                kind_id: char_record_kind::GET_LOOTITEM,
                amount: i64::from(g.count),
                matcher: RecordMatch::Value1(g.item_id),
            },
        );
    }
    if coins > 0 {
        inventory::add_money(&mut txn.out(), player, slot_type::INVENTORY, coins, task);
    }
    true
}

/// `ItemManager.GetLootConvertFish(templateId)`: the trophy items for a fish (only the first
/// convert pack is used, as in C#). New item ids come from the item manager. `None` where the C#
/// throws (unknown loot pack).
pub fn loot_convert_fish(
    server: &GameServer,
    template_id: u32,
    rng: &mut dyn LootRng,
) -> Option<Vec<Item>> {
    let Some(convert) = server
        .game_data
        .items
        .loot_packs_by_fish(template_id)
        .first()
    else {
        return Some(Vec::new());
    };
    loot_convert_fish_from_pack(server, template_id, convert.loot_pack_id, rng)
}

/// A fish stand's permitted conversion pack, selected by its actual conversion function.
pub(crate) fn loot_convert_fish_from_pack(
    server: &GameServer,
    template_id: u32,
    loot_pack_id: u32,
    rng: &mut dyn LootRng,
) -> Option<Vec<Item>> {
    let mut items = Vec::new();
    let pack = server.game_data.systems.economy.loot.pack(loot_pack_id)?;
    let mut drop_rate_max = 0u32;
    for l in &pack.loots {
        drop_rate_max = drop_rate_max.wrapping_add(l.drop_rate);
    }
    // Random.Shared.Next(0, uint) binds to the float extension.
    let roll = rng.next_f32_range(0.0, drop_rate_max as f32);
    let mut acc = 0u32;
    for l in &pack.loots {
        if (l.drop_rate.wrapping_add(acc)) as f32 >= roll {
            let id = server.items.new_id(&server.ids.item)?;
            items.push(Item {
                class: ItemClass::Item,
                id,
                template_id: l.item_id,
                slot: -1,
                grade: 0,
                flags: 0,
                count: rng.next_i32(l.min_amount, l.max_amount),
                lifespan_mins: 0,
                made_unit_id: template_id,
                crafter_id: 0,
                world_id: server.config.id,
                create_time: chrono::Utc::now().timestamp(),
                unsecure_time: 0,
                unpack_time: 0,
                charge_start_time: 0,
                ucc_id: 0,
                details: ItemDetails::default(),
                bind_on_unpack: false,
                owner_id: 0,
                slot_type: 0,
                expiration_time: 0,
                expiration_online_minutes_left: 0.0,
                charge_count: 0,
                dirty: true,
            });
            break;
        }
        acc = acc.wrapping_add(l.drop_rate);
    }
    Some(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::items::{GradeDistributions, ItemTemplate};
    use crate::systems::economy::loot_data::LootGroups;

    struct Quests(Vec<u32>);
    impl LootPlayer for Quests {
        fn has_quest(&self, q: u32) -> bool {
            self.0.contains(&q)
        }
        fn actability_loot_multiplier(&self, _: u8) -> Option<f32> {
            None
        }
    }

    fn loot(id: u32, group: u32, item: u32, rate: u32, min: i32, max: i32) -> Loot {
        Loot {
            id,
            group,
            item_id: item,
            drop_rate: rate,
            min_amount: min,
            max_amount: max,
            loot_pack_id: 1,
            grade_id: 0,
            always_drop: false,
        }
    }

    fn pack(loots: Vec<Loot>, groups: Vec<LootGroups>) -> LootPack {
        let mut p = LootPack {
            id: 1,
            ..Default::default()
        };
        for g in groups {
            p.groups.push((g.group_no, g));
        }
        for l in &loots {
            match p.loots_by_group_no.iter_mut().find(|(g, _)| *g == l.group) {
                Some((_, v)) => v.push(*l),
                None => p.loots_by_group_no.push((l.group, vec![*l])),
            }
            p.group_count = p.group_count.max(l.group);
        }
        p.loots = loots;
        p
    }

    fn items() -> ItemData {
        let mut d = ItemData::default();
        d.templates.insert(10, ItemTemplate::plain(10, 10));
        d.templates.insert(11, ItemTemplate::plain(11, 10));
        let mut q = ItemTemplate::plain(12, 1);
        q.loot_quest_id = 77;
        d.templates.insert(12, q);
        d.grade_distributions.insert(
            3,
            GradeDistributions {
                id: 3,
                name: "x".into(),
                weights: [10, 20, 30, 40, 0, 0, 0, 0, 0, 0, 0, 0],
            },
        );
        d
    }

    #[test]
    fn group_zero_rolls_each_item_and_coins_use_gold_rate() {
        let p = pack(
            vec![
                loot(1, 0, 10, 10_000_000, 1, 3),
                loot(2, 0, 11, 5_000_000, 1, 1),
                loot(3, 0, ITEM_COINS, 10_000_000, 10, 10),
            ],
            vec![],
        );
        // dice: item1 5_000_000 (< 10M), item2 6_000_000 (>= 5M: skipped), coins 0; amounts: 2, 10
        let mut rng = ScriptedRng::new(&[5_000_000, 6_000_000, 0, 2, 10], &[]);
        let cfg = LootConfig {
            loot_rate: 1.0,
            gold_loot_multiplier: 2.0,
        };
        let out = generate_pack_v2(&p, &items(), cfg, 1.0, 1.5, None, 0, None, &mut rng).unwrap();
        assert_eq!(
            out,
            vec![
                GeneratedLoot {
                    item_id: 10,
                    count: 2,
                    grade: 0,
                    group: 0
                },
                GeneratedLoot {
                    item_id: ITEM_COINS,
                    count: 30,
                    grade: 0,
                    group: 0
                },
            ]
        );
        // count rolls are Next(min, max + 1)
        assert_eq!(rng.int_calls[3], (1, 4));
    }

    #[test]
    fn group_roll_picks_by_cumulative_rate_and_grade_distribution() {
        let p = pack(
            vec![loot(1, 1, 10, 600, 1, 1), loot(2, 1, 11, 400, 1, 1)],
            vec![LootGroups {
                id: 1,
                pack_id: 1,
                group_no: 1,
                drop_rate: 5_000_000,
                item_grade_distribution_id: 3,
            }],
        );
        // group dice 4_999_999 < 5M -> roll; float roll 0.7 * 1000 = 700 -> second item
        // (600 <= 700 < 1000); amount 1; grade drop 35 -> weights 10, 20, 30: 35 <= 60 -> 2
        let mut rng = ScriptedRng::new(&[4_999_999, 1, 35], &[0.7]);
        let out = generate_pack_v2(
            &p,
            &items(),
            LootConfig::default(),
            1.0,
            1.0,
            None,
            0,
            None,
            &mut rng,
        )
        .unwrap();
        assert_eq!(
            out,
            vec![GeneratedLoot {
                item_id: 11,
                count: 1,
                grade: 2,
                group: 1
            }]
        );
        // group dice failing: nothing
        let mut rng = ScriptedRng::new(&[5_000_000], &[]);
        let out = generate_pack_v2(
            &p,
            &items(),
            LootConfig::default(),
            1.0,
            1.0,
            None,
            0,
            Some(4),
            &mut rng,
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn quest_items_need_the_quest_and_quest_only_group_errors() {
        let p = pack(
            vec![loot(1, 1, 10, 600, 1, 1), loot(2, 1, 12, 400, 1, 1)],
            vec![],
        );
        let with = Quests(vec![77]);
        // group rate 1 -> requires 10M; roll 0.1*600 = 60 < 600 -> item 10, quest item merged
        let mut rng = ScriptedRng::new(&[0, 1, 1], &[0.1]);
        let out = generate_pack_v2(
            &p,
            &items(),
            LootConfig::default(),
            1.0,
            1.0,
            Some(&with),
            0,
            None,
            &mut rng,
        )
        .unwrap();
        assert_eq!(
            out.iter().map(|g| g.item_id).collect::<Vec<_>>(),
            vec![10, 12]
        );
        // only the quest item in the group: C# KeyNotFoundException
        let p = pack(vec![loot(2, 1, 12, 400, 1, 1)], vec![]);
        let mut rng = ScriptedRng::new(&[0], &[]);
        let err = generate_pack_v2(
            &p,
            &items(),
            LootConfig::default(),
            1.0,
            1.0,
            Some(&with),
            0,
            None,
            &mut rng,
        )
        .unwrap_err();
        assert_eq!(err, LootError::QuestOnlyGroup { pack: 1, group: 1 });
        // without a player, quest items roll like any other item
        let mut rng = ScriptedRng::new(&[0, 1], &[0.0]);
        let out = generate_pack_v2(
            &p,
            &items(),
            LootConfig::default(),
            1.0,
            1.0,
            None,
            0,
            None,
            &mut rng,
        )
        .unwrap();
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn grade_distribution_falls_back_to_zero() {
        let d = items();
        let mut rng = ScriptedRng::new(&[99], &[]);
        // 10+20+30+40 = 100 >= 99 -> grade 3
        assert_eq!(grade_from_distribution(&d, 3, &mut rng).unwrap(), 3);
        let mut rng = ScriptedRng::new(&[0], &[]);
        assert_eq!(grade_from_distribution(&d, 3, &mut rng).unwrap(), 0);
        assert!(grade_from_distribution(&d, 9, &mut rng).is_err());
    }

    // ---- the crop harvest packs (potato 6452, cucumber 6453, barley 6472, ...) ----
    //
    // Group 1 is the crop itself, group 2 the rare "fine" variant and group 3 the seed. Groups 2
    // and 3 have a `loot_actability_groups` row (`max_dice` 345 / 355): the seed is NOT a
    // guaranteed drop, it comes back when the actability roll (0..9999) is at most 355, about
    // one harvest in 28.

    fn potato_pack() -> LootPack {
        use crate::systems::economy::loot_data::LootActabilityGroups;
        let mut p = pack(
            vec![
                loot(1, 1, 10, 10_000_000, 2, 4),
                Loot {
                    grade_id: 2,
                    ..loot(2, 2, 11, 10_000_000, 1, 1)
                },
                loot(3, 3, 13, 10_000_000, 1, 1),
            ],
            vec![],
        );
        for (group_id, max_dice) in [(2, 345), (3, 355)] {
            p.actability_groups.push((
                group_id,
                LootActabilityGroups {
                    id: 100 + group_id,
                    loot_pack_id: 1,
                    group_id,
                    max_dice,
                    min_dice: max_dice,
                },
            ));
        }
        p
    }

    fn harvest(act_dice_fine: i32, act_dice_seed: i32) -> Vec<(u32, i32, u8)> {
        let mut d = items();
        d.templates.insert(13, ItemTemplate::plain(13, 10));
        // Per group: the group roll, the actability roll (groups 2 and 3), and the amount when
        // something is picked: crop | fine crop | seed.
        let mut ints = vec![0, 3, 0, act_dice_fine];
        if act_dice_fine <= 345 {
            ints.push(1);
        }
        ints.extend([0, act_dice_seed, 1]);
        let mut rng = ScriptedRng::new(&ints, &[]);
        generate_pack_v2(
            &potato_pack(),
            &d,
            LootConfig::default(),
            1.0,
            1.0,
            None,
            0,
            None,
            &mut rng,
        )
        .unwrap()
        .into_iter()
        .map(|g| (g.item_id, g.count, g.grade))
        .collect()
    }

    #[test]
    fn harvest_gives_the_seed_back_only_when_its_actability_dice_hit() {
        // Both dice at their limit: crop, fine crop (grade 2) and seed.
        assert_eq!(harvest(345, 355), vec![(10, 3, 0), (11, 1, 2), (13, 1, 0)]);
        // One above the limit: that group is skipped.
        assert_eq!(harvest(346, 355), vec![(10, 3, 0), (13, 1, 0)]);
        assert_eq!(harvest(345, 356), vec![(10, 3, 0), (11, 1, 2)]);
        // The crop alone is the normal outcome.
        assert_eq!(harvest(9_999, 9_999), vec![(10, 3, 0)]);
    }

    /// A reproducible `System.Random` (splitmix64).
    struct Seeded(u64);

    impl Seeded {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    impl LootRng for Seeded {
        fn next_i32(&mut self, min: i32, max: i32) -> i32 {
            if min >= max {
                return min;
            }
            min + (self.next() % (max - min) as u64) as i32
        }

        fn next_f32(&mut self) -> f32 {
            (self.next() >> 40) as f32 / (1u64 << 24) as f32
        }
    }

    /// The real harvest packs of the three crops the player bots farm: the crop always drops (2-4),
    /// the fine variant (grade 2) and the seed each with the chance their dice allow (~3.5%).
    #[test]
    #[ignore]
    fn real_compact_db_crop_harvests_return_the_seed_about_once_in_28() {
        use crate::systems::economy::loot_data::LootData;
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let data = LootData::load(&conn).unwrap();
        let items = ItemData::default();
        const ROLLS: usize = 200_000;
        let mut rng = Seeded(0xAA11);
        // potato, cucumber, barley
        for pack_id in [6452, 6453, 6472] {
            let pack = data.pack(pack_id).unwrap();
            let by_group = |g: u32| {
                pack.loots_by_group_no
                    .iter()
                    .find(|(no, _)| *no == g)
                    .map(|(_, l)| l[0])
                    .unwrap()
            };
            let (crop, fine, seed) = (
                by_group(1).item_id,
                by_group(2).item_id,
                by_group(3).item_id,
            );
            let (mut crops, mut fines, mut seeds) = (0usize, 0usize, 0usize);
            for _ in 0..ROLLS {
                let got = generate_pack_v2(
                    pack,
                    &items,
                    LootConfig::default(),
                    1.0,
                    1.0,
                    None,
                    0,
                    None,
                    &mut rng,
                )
                .unwrap();
                for g in got {
                    if g.item_id == crop {
                        crops += 1;
                        assert!((2..=4).contains(&g.count), "{g:?}");
                    } else if g.item_id == fine {
                        fines += 1;
                        assert_eq!((g.count, g.grade), (1, 2));
                    } else if g.item_id == seed {
                        seeds += 1;
                        assert_eq!(g.count, 1);
                    }
                }
            }
            let rate = |n: usize| n as f64 / ROLLS as f64;
            assert_eq!(crops, ROLLS, "pack {pack_id}: the crop is always there");
            // The actability roll is 0..9999 and passes at `<= max_dice`.
            let expected = |group: u32| {
                (f64::from(pack.actability_group(group).unwrap().max_dice) + 1.0) / 10_000.0
            };
            for (what, n, group) in [("seed", seeds, 3), ("fine crop", fines, 2)] {
                assert!(
                    (rate(n) - expected(group)).abs() < 0.002,
                    "pack {pack_id}: {what} rate {} (expected {})",
                    rate(n),
                    expected(group)
                );
                assert!(rate(n) < 0.05, "never a guaranteed drop");
            }
        }
    }
}
