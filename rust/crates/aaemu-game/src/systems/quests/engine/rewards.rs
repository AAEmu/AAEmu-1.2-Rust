use super::{
    error_msg, gap_exp, item_task, kind, scaled_supply_copper, scaled_supply_exp, ActKind, Ctx,
    ItemCreationDefinition, ObjectiveStatus, QuestHandle, SCQuestRewardedByMailPacket,
};

/// `C# Math.Round(double)` (banker's rounding).
fn round_even(v: f64) -> f64 {
    v.round_ties_even()
}

impl<'a> Ctx<'a> {
    /// Some r208022 reward rows (e.g. 991 → 16390) have a class-specific item
    /// row but no base `items` row, so the runtime template has no stack capacity.
    pub(super) fn usable_reward_item(&self, template_id: u32) -> bool {
        self.server
            .game_data
            .items
            .template(template_id)
            .is_some_and(|item| item.max_count > 0)
    }

    /// Count newly occupied bag slots across the entire payout, including repeated
    /// definitions sharing a grade and quantities that span multiple stacks.
    fn reward_items_fit_bag(&self, pool: &[ItemCreationDefinition]) -> bool {
        let mut batches = std::collections::BTreeMap::new();
        let items = &self.server.game_data.items;
        for item in pool {
            let Some(template) = items.template(item.template_id) else {
                continue;
            };
            let grade = if template.fixed_grade >= 0 {
                template.fixed_grade
            } else {
                i32::from(item.grade_id)
            };
            *batches.entry((item.template_id, grade)).or_insert(0_i64) += i64::from(item.count);
        }
        let bag = self.c.inventory.bag().lock().expect("bag poisoned");
        let mut slots = i64::from(bag.free_slot_count());
        for ((template_id, grade), amount) in batches {
            let Some(template) = items.template(template_id) else {
                continue;
            };
            let needed = bag.acquisition_slots(template, grade, amount);
            if needed > slots {
                return false;
            }
            slots -= needed;
        }
        true
    }

    /// `DistributeRewards(addBaseQuestReward)`.
    pub fn distribute_rewards(&mut self, h: QuestHandle, add_base: bool) -> bool {
        let hooks = self.hooks.clone();
        let player = self.player.clone();
        let Some(q) = self.quest(h) else {
            return true;
        };
        let template_id = q.template_id;
        let data = self.data;
        let (mut exp, mut copper) = (q.reward_exp, q.reward_coins);
        let mut exp_ratio = q.reward_ratio;
        let mut copper_ratio = q.reward_ratio;
        if add_base && q.step == kind::REWARD {
            if let Some(t) = data.template(template_id) {
                // Keep the server's completion ratios. The separate r208022 0x4ab090
                // display adjustment needs verified values for game parameters 0x10 and 0x11.
                if t.let_it_done {
                    exp_ratio = match self.objective_status(h) {
                        ObjectiveStatus::NotReady => 0.0,
                        ObjectiveStatus::CanEarlyComplete => f64::from(0.3f32),
                        ObjectiveStatus::QuestComplete | ObjectiveStatus::ExtraProgress => 1.0,
                        ObjectiveStatus::Overachieved => f64::from(1.2f32),
                    };
                }
                if let Some(supply) = data.supplies(t.level).filter(|_| t.level > 0) {
                    if !t.let_it_done {
                        exp_ratio = 1.0;
                    }
                    copper_ratio = exp_ratio;
                    // r208022 0x4b1370 and 0x4b0fd0 suppress fallback on act presence,
                    // including explicit zeroes. Compute fallback locally so retries don't add it.
                    let mut explicit_exp = false;
                    let mut explicit_copper = false;
                    for component in t.components_of(&data.components, kind::REWARD) {
                        for &act in &component.acts {
                            match data.acts[act].kind {
                                ActKind::SupplyExp { .. } => explicit_exp = true,
                                ActKind::SupplyCopper { .. } => explicit_copper = true,
                                _ => {}
                            }
                        }
                    }
                    let scaling = self.server.config.get().quest.reward_scaling;
                    if !explicit_exp {
                        let fallback = if scaling {
                            gap_exp(
                                scaled_supply_exp(supply.exp, t.degree),
                                t.level,
                                self.c.unit.level,
                            )
                        } else {
                            supply.exp
                        };
                        exp = exp.wrapping_add(fallback);
                    }
                    if !explicit_copper {
                        let fallback = if scaling {
                            scaled_supply_copper(supply.copper, t.degree)
                        } else {
                            supply.copper
                        };
                        copper = copper.wrapping_add(fallback);
                    }
                }
            }
        }
        let xp = round_even(f64::from(exp) * exp_ratio) as i32;
        let mut copper = round_even(f64::from(copper) * copper_ratio) as i32;

        if !q.reward_items.is_empty() && q.allow_item_rewards {
            let pool: Vec<ItemCreationDefinition> = q
                .reward_items
                .iter()
                .filter(|item| item.count > 0 && self.usable_reward_item(item.template_id))
                .cloned()
                .collect();
            if let Some(q) = self.quest_mut(h) {
                q.reward_items = pool.clone();
            }
            if !self.reward_items_fit_bag(&pool) {
                let failed = hooks.send_quest_reward_mails(
                    self.server,
                    player.as_ref(),
                    self.c,
                    template_id,
                    &pool,
                    copper,
                );
                if failed > 0 {
                    self.send_error(error_msg::MAIL_UNKNOWN_FAILURE);
                    return false;
                }
                copper = 0;
                if let Some(q) = self.quest_mut(h) {
                    q.reward_items.clear();
                }
                self.send(SCQuestRewardedByMailPacket {
                    success: true,
                    quest_id: template_id,
                });
            } else {
                for item in &pool {
                    let acquire = if hooks.is_auto_equip_trade_pack(self.server, item.template_id) {
                        hooks.try_equip_new_backpack(
                            self.server,
                            player.as_ref(),
                            self.c,
                            item_task::QUEST_SUPPLY_ITEMS,
                            item.template_id,
                            item.count,
                            item.grade_id,
                        )
                    } else {
                        hooks.acquire_default_item(
                            self.server,
                            player.as_ref(),
                            self.c,
                            item_task::QUEST_SUPPLY_ITEMS,
                            item.template_id,
                            item.count,
                            item.grade_id,
                        )
                    };
                    if !acquire {
                        return false;
                    }
                    if let Some(q) = self.quest_mut(h) {
                        q.reward_items.remove(0);
                    }
                }
            }
        }

        if xp > 0 {
            hooks.add_exp(self.server, player.as_ref(), self.c, xp, true);
        }
        if copper > 0 {
            hooks.change_money(self.server, player.as_ref(), self.c, copper);
        }
        if let Some(q) = self.quest_mut(h) {
            q.reward_exp = 0;
            q.reward_coins = 0;
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
        true
    }
}
