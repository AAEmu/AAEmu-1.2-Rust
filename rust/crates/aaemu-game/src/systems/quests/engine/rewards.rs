use super::{
    error_msg, gap_exp, item_task, kind, scaled_supply_copper, scaled_supply_exp, ActKind, Ctx,
    ItemCreationDefinition, ObjectiveStatus, QuestHandle, SCQuestRewardedByMailPacket,
};

/// `C# Math.Round(double)` (banker's rounding).
fn round_even(v: f64) -> f64 {
    v.round_ties_even()
}

impl<'a> Ctx<'a> {
    /// `DistributeRewards(addBaseQuestReward)`.
    pub fn distribute_rewards(&mut self, h: QuestHandle, add_base: bool) -> bool {
        let res = true;
        let hooks = self.hooks.clone();
        let player = self.player.clone();
        let Some(q) = self.quest(h) else {
            return true;
        };
        let template_id = q.template_id;
        // Distribute Items if needed. Filter the pool once, before the free-slot check and the
        // mail fallback: a reward item without a template (282 rows of the 1.2
        // `quest_act_supply_items`) yields nothing and must not push the rest into the mail.
        if !q.reward_items.is_empty() && q.allow_item_rewards {
            let server = self.server;
            let pool: Vec<ItemCreationDefinition> = q
                .reward_items
                .iter()
                .filter(|item| {
                    let known = server.game_data.items.template(item.template_id).is_some();
                    if !known {
                        tracing::debug!(
                            "Quest {template_id}: skipping unknown reward item template {}",
                            item.template_id
                        );
                    }
                    known
                })
                .cloned()
                .collect();
            let coins = q.reward_coins;
            let free = self
                .c
                .inventory
                .bag()
                .lock()
                .expect("bag poisoned")
                .free_slot_count();
            if free < pool.len() as i32 {
                let failed = hooks.send_quest_reward_mails(
                    self.server,
                    player.as_ref(),
                    self.c,
                    template_id,
                    &pool,
                    coins,
                );
                if failed > 0 {
                    self.send_error(error_msg::MAIL_UNKNOWN_FAILURE);
                    return false;
                }
                // Coins will be distributed in mail if any mail needed to be sent
                if let Some(q) = self.quest_mut(h) {
                    q.reward_coins = 0;
                }
                self.send(SCQuestRewardedByMailPacket {
                    success: true,
                    quest_id: template_id,
                });
            } else {
                for item in &pool {
                    if hooks.is_auto_equip_trade_pack(self.server, item.template_id) {
                        if !hooks.try_equip_new_backpack(
                            self.server,
                            player.as_ref(),
                            self.c,
                            item_task::QUEST_SUPPLY_ITEMS,
                            item.template_id,
                            item.count,
                            item.grade_id,
                        ) {
                            return false;
                        }
                    } else {
                        if !hooks.acquire_default_item(
                            self.server,
                            player.as_ref(),
                            self.c,
                            item_task::QUEST_SUPPLY_ITEMS,
                            item.template_id,
                            item.count,
                            item.grade_id,
                        ) {
                            return false;
                        }
                    }
                }
            }
            if let Some(q) = self.quest_mut(h) {
                q.reward_items.clear();
            }
        }

        // Add quest level based rewards
        let data = self.data;
        let (level, let_it_done, degree) = data
            .template(template_id)
            .map_or((0, false, 0), |t| (t.level, t.let_it_done, t.degree));
        let step = self.quest(h).map_or(0, |q| q.step);
        // Retail 1.2 replaces the level based supply with the Reward component's own
        // `QuestActSupplyExp` / `QuestActSupplyCopper` act instead of adding both (the client's
        // `X2Quest.GetQuestContextRewardExp` scans the Reward component and falls back to
        // `quest_supplies` only when the act is absent; see PORTING.md). Data point: 376
        // category 79 (exploration, level 50) quests carry a Reward act with exp 0 / copper 0
        // and would otherwise be paid the full level supply.
        let (reward_exp_act, reward_copper_act) = data
            .template(template_id)
            .map(|t| {
                let mut exp = false;
                let mut copper = false;
                for c in t.components_of(&data.components, kind::REWARD) {
                    for &a in &c.acts {
                        match &data.acts[a].kind {
                            ActKind::SupplyExp { .. } => exp = true,
                            ActKind::SupplyCopper { .. } => copper = true,
                            _ => {}
                        }
                    }
                }
                (exp, copper)
            })
            .unwrap_or((false, false));
        if add_base && level > 0 && step == kind::REWARD {
            if let Some(lb) = data.supplies(level).copied() {
                let ratio = if let_it_done {
                    match self.objective_status(h) {
                        ObjectiveStatus::NotReady => 0f32,
                        ObjectiveStatus::CanEarlyComplete => 0.3f32,
                        ObjectiveStatus::QuestComplete => 1f32,
                        ObjectiveStatus::ExtraProgress => 1f32,
                        ObjectiveStatus::Overachieved => 1.2f32,
                    }
                } else {
                    1f32
                };
                // `Quest:RewardScaling`: the client scales the supplies fallback by the quest
                // degree and exp by the quest-vs-player level gap (see `reward_scale`). The
                // act values replace the fallback per field, so each keeps its own path.
                let scaling = self.server.config.get().quest.reward_scaling;
                let player_level = self.c.unit.level;
                if let Some(q) = self.quest_mut(h) {
                    q.reward_ratio = f64::from(ratio);
                    if !reward_exp_act {
                        let exp = if scaling {
                            gap_exp(scaled_supply_exp(lb.exp, degree), level, player_level)
                        } else {
                            lb.exp
                        };
                        q.reward_exp = q.reward_exp.wrapping_add(exp);
                    }
                    if !reward_copper_act {
                        let copper = if scaling {
                            scaled_supply_copper(lb.copper, degree)
                        } else {
                            lb.copper
                        };
                        q.reward_coins = q.reward_coins.wrapping_add(copper);
                    }
                }
            }
        }

        // Add XP
        let Some(q) = self.quest(h) else {
            return res;
        };
        let (exp_pool, coin_pool, ratio) = (q.reward_exp, q.reward_coins, q.reward_ratio);
        if exp_pool > 0 {
            let xp = round_even(f64::from(exp_pool) * ratio) as i32;
            if xp > 0 {
                hooks.add_exp(self.server, player.as_ref(), self.c, xp, true);
            }
            if let Some(q) = self.quest_mut(h) {
                q.reward_exp = 0;
            }
        }
        // Add copper coins
        if coin_pool > 0 {
            let copper = round_even(f64::from(coin_pool) * ratio) as i32;
            if copper > 0 {
                hooks.change_money(self.server, player.as_ref(), self.c, copper);
            }
            if let Some(q) = self.quest_mut(h) {
                q.reward_coins = 0;
            }
        }
        // Cleanup used Items from quest
        let cleanup = self
            .quest(h)
            .map(|q| q.cleanup_items.clone())
            .unwrap_or_default();
        if !cleanup.is_empty() {
            for item in &cleanup {
                hooks.consume_item(
                    self.server,
                    player.as_ref(),
                    self.c,
                    item_task::QUEST_COMPLETE,
                    item.template_id,
                    item.count,
                    None,
                );
            }
            if let Some(q) = self.quest_mut(h) {
                q.cleanup_items.clear();
            }
        }
        res
    }
}
