use super::{
    acceptor, bit_get, bit_set, condition, detail, error_msg, has_quest_completed, kind,
    quest_error, set_completed_quest_flag, status, ActKind, ActPath, CompletedQuest, Ctx,
    ObjectiveStatus, Quest, QuestHandle, SCQuestContextFailedPacket, SCQuestContextResetPacket,
    SCQuestContextUpdatedPacket, SCQuestUnitReqFailedPacket, StartRefusal, WIRE_QUEST_ID_LIMIT,
};

const DAILY_DETAILS: [u32; 4] = [
    detail::DAILY,
    detail::DAILY_GROUP,
    detail::DAILY_HUNT,
    detail::DAILY_LIVELIHOOD,
];

impl<'a> Ctx<'a> {
    /// r208022 0x7e5920 counts each daily context once when tracked or completed.
    fn daily_quota_reached(&self) -> bool {
        let limit = self.server.config.get().quest.daily_limit;
        if limit == 0 {
            return false;
        }

        let is_daily = |id| {
            self.data
                .template(id)
                .is_some_and(|t| DAILY_DETAILS.contains(&t.detail_id))
        };
        let active = self
            .st
            .active
            .values()
            .filter_map(|&h| self.quest(h))
            .filter(|q| is_daily(q.template_id));
        let completed = self
            .c
            .quests
            .completed
            .iter()
            .flat_map(|block| {
                (0..64).filter_map(move |bit| {
                    bit_get(&block.body, bit).then_some(block.id * 64 + bit as u32)
                })
            })
            .filter(|&id| is_daily(id) && !self.has_quest(id));
        active
            .map(|q| q.template_id)
            .chain(completed)
            .nth(limit - 1)
            .is_some()
    }

    // ----- CharacterQuests -----

    /// `HasQuest`.
    pub fn has_quest(&self, quest_id: u32) -> bool {
        self.st.has_quest(quest_id)
    }

    /// `HasQuestCompleted` / `IsQuestComplete`.
    pub fn has_quest_completed(&self, quest_id: u32) -> bool {
        has_quest_completed(&self.c.quests.completed, quest_id)
    }

    /// `SetCompletedQuestFlag`: returns the changed block.
    pub fn set_completed_quest_flag(&mut self, quest_id: u32, completed: bool) -> CompletedQuest {
        if quest_id >= WIRE_QUEST_ID_LIMIT {
            static LOGGED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    "Quest {quest_id} is above {WIRE_QUEST_ID_LIMIT}: its completed flag is kept (block {}), but the client's 16 bit completed block ids cannot show it",
                    quest_id / 64
                );
            }
        }
        set_completed_quest_flag(&mut self.c.quests.completed, quest_id, completed)
    }

    /// The `SupplyItem` act [`StartRefusal::SupplyItem`] refuses on, if any: an auto-equip
    /// trade pack whose backpack slot cannot be freed. `None` = the supply items are fine.
    fn supply_item_refusal(&self, template_id: u32) -> Option<u32> {
        let data = self.data;
        let t = data.template(template_id)?;
        for c in t.components_of(&data.components, kind::SUPPLY) {
            for &a in &c.acts {
                let ActKind::SupplyItem { item_id, .. } = data.acts[a].kind else {
                    continue;
                };
                if self.hooks.is_auto_equip_trade_pack(self.server, item_id)
                    && !self
                        .hooks
                        .can_replace_glider_in_backpack_slot(self.server, self.c)
                {
                    return Some(item_id);
                }
            }
        }
        None
    }

    /// The checks [`Ctx::add_quest`] runs before it installs a quest (`None` = it may start),
    /// shared with [`Ctx::can_offer_quest`]. The result says which refusal an
    /// [`Ctx::add_quest`] caller should log / answer; this function itself has no side effects
    /// (no logging, packets or state changes).
    ///
    /// `forcibly` is the GM path: like `add_quest`, it skips the active / schedule / supply /
    /// start-component / completed refusals (an active or completed quest is dropped / cleared
    /// by the caller) but still needs a `quest_contexts` row.
    pub fn start_refusal(&self, quest_id: u32, forcibly: bool) -> Option<StartRefusal> {
        let data = self.data;
        let Some(template) = data.template(quest_id) else {
            return Some(StartRefusal::UnknownQuest);
        };
        if !forcibly && self.has_quest(quest_id) {
            return Some(StartRefusal::Active);
        }
        // `game_schedule_quests`: a scheduled quest is acceptable only inside its window.
        if !forcibly
            && !super::quest_schedule_allows(
                &self.server.game_data.systems.npcs.schedule,
                quest_id,
                crate::systems::npcs::spawn_manager::unix_now(),
            )
        {
            return Some(StartRefusal::ScheduleWindow);
        }
        if !forcibly {
            if let Some(item_id) = self.supply_item_refusal(quest_id) {
                return Some(StartRefusal::SupplyItem(item_id));
            }
            for c in template.components_of(&data.components, kind::START) {
                if !self.can_component_run(c.id) {
                    return Some(StartRefusal::ComponentUnitReq(c.id));
                }
            }
        }
        if !forcibly && self.has_quest_completed(quest_id) && !template.repeatable {
            return Some(StartRefusal::Completed);
        }
        if !forcibly && DAILY_DETAILS.contains(&template.detail_id) && self.daily_quota_reached() {
            return Some(StartRefusal::DailyQuota);
        }
        None
    }

    /// The checks [`Ctx::add_quest`] runs before it installs a quest, without the refusal
    /// logging / packets: whether the character could start `quest_id` now. Used for the
    /// `SCQuestAcceptConditional` sphere offer, so the dialog is only sent for a quest the
    /// client's accept reply could actually start: the shared refusal rules plus the start
    /// component's acceptor check for the sphere the client would send back.
    pub fn can_offer_quest(&self, quest_id: u32, sphere_id: u32) -> bool {
        self.start_refusal(quest_id, false).is_none()
            && self.starter_accepts(quest_id, acceptor::SPHERE, sphere_id)
    }

    /// `AddQuest(questId, forcibly, questAcceptorType, acceptorId)`.
    pub fn add_quest(
        &mut self,
        quest_id: u32,
        forcibly: bool,
        acceptor_type: u8,
        acceptor_id: u32,
    ) -> bool {
        // A forced start of an active quest drops the old run first; the non-forced refusal
        // comes from `start_refusal` below.
        if forcibly && self.has_quest(quest_id) {
            tracing::info!("[GM] quest {quest_id}, added!");
            self.drop_quest(quest_id, true, false);
        }
        if let Some(refusal) = self.start_refusal(quest_id, forcibly) {
            match refusal {
                StartRefusal::Active => {
                    self.log_refusal("AddQuest", quest_id, "the quest is already active");
                    if self.answer_start_refusals {
                        self.send(SCQuestContextFailedPacket {
                            quest_id,
                            err: quest_error::ALREADY_HAVE,
                        });
                    }
                }
                StartRefusal::UnknownQuest => {
                    tracing::error!("Failed to start new Quest {quest_id}, invalid Id");
                }
                StartRefusal::ScheduleWindow => {
                    self.log_refusal(
                        "AddQuest",
                        quest_id,
                        "its game schedule window is not in progress",
                    );
                }
                StartRefusal::SupplyItem(item_id) => {
                    self.log_refusal(
                        "AddQuest",
                        quest_id,
                        &format!("backpack supply item {item_id} cannot be equipped"),
                    );
                    self.send_error(error_msg::BACKPACK_OCCUPIED);
                }
                StartRefusal::ComponentUnitReq(component_id) => {
                    self.log_refusal(
                        "AddQuest",
                        quest_id,
                        &format!("unit requirements of the start component {component_id} not met"),
                    );
                    if self.answer_start_refusals {
                        if let Some(req) = self.first_failed_component_req(component_id) {
                            // INFERRED (WP-QU1): kind = unit_reqs.kind_id, value = value1, and
                            // the message id stays 0 (`unit_reqs` has no message column). The
                            // client reader FUN_397b5d00 / bag FUN_397b1ef0 resolves the text
                            // from kind and value; case NO_BUFF prints the buff name of value,
                            // case EXCEPT_COMPLETE_QUEST_CONTEXT the quest name (ui-quests.md
                            // 1.1).
                            self.send(SCQuestUnitReqFailedPacket {
                                quest_id,
                                unit_obj_id: self.c.obj_id,
                                kind: req.kind as u8,
                                message_id: 0,
                                value: req.value1,
                            });
                        }
                    }
                }
                StartRefusal::DailyQuota => {
                    self.log_refusal("AddQuest", quest_id, "daily quest quota reached");
                    if self.answer_start_refusals {
                        self.send(SCQuestContextFailedPacket {
                            quest_id,
                            err: quest_error::DAILY_QUOTA,
                        });
                    }
                }
                StartRefusal::Completed => {
                    self.log_refusal("AddQuest", quest_id, "already completed and not repeatable");
                    if self.answer_start_refusals {
                        self.send(SCQuestContextFailedPacket {
                            quest_id,
                            err: quest_error::INVALID_QUEST_STATUS,
                        });
                    } else {
                        self.send_error(error_msg::QUEST_DAILY_LIMIT);
                    }
                }
            }
            return false;
        }
        // A forced start of a completed quest clears the flag.
        if forcibly && self.has_quest_completed(quest_id) {
            tracing::info!("[GM] quest {quest_id}, added!");
            self.set_completed_quest_flag(quest_id, false);
        }

        if self.install_quest(quest_id, forcibly, acceptor_type, acceptor_id, "AddQuest") {
            let msg = format!("[Quest] {}, quest {quest_id} added.", self.c.unit.name);
            self.send_debug_message(msg);
            true
        } else {
            false
        }
    }

    /// The object part of `AddQuest` (the object initializer runs after the constructor): a new
    /// quest object, its id, the first `SCQuestContextStartedPacket` and the first step. The
    /// caller has already checked whether the quest may start; the checks of `StartQuest`
    /// itself only remain for a non-forced quest. A failed restart of a `restart_on_fail`
    /// quest comes through here too, so it cannot be refused by a requirement that no longer
    /// holds (the quest already ran once).
    fn install_quest(
        &mut self,
        quest_id: u32,
        forcibly: bool,
        acceptor_type: u8,
        acceptor_id: u32,
        what: &str,
    ) -> bool {
        // Create new Quest Object (the object initializer runs after the constructor)
        let h = self.new_quest(quest_id, 0);
        let id = match self.server.ids.quest.get_next_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("QuestIdManager: {e}");
                self.st.quests.remove(&h);
                return false;
            }
        };
        if let Some(q) = self.quest_mut(h) {
            q.id = id;
            q.status = status::INVALID;
            q.condition = condition::PROGRESS;
            q.acceptor_type = acceptor_type;
            q.acceptor_id = acceptor_id;
            q.forced_start = forcibly;
        }

        // Cancel stale work before starting another instance of this quest.
        self.server
            .systems
            .quests
            .remove_quest_timer(self.c.id, quest_id);

        // Actually start the quest by setting step to Start and send the quest start packets
        if !self.start_quest(h) {
            self.log_refusal(
                what,
                quest_id,
                &format!(
                    "the start step does not accept acceptor type {acceptor_type} id {acceptor_id}"
                ),
            );
            self.finalize_quest_acts(h);
            self.st.quests.remove(&h);
            self.server.ids.quest.release_id(id);
            return false;
        }

        // Add it to the Active Quests
        self.st.active.add(quest_id, h);

        // Execute the first Step
        self.run_current_step(h);
        self.quest_initialized(h);
        true
    }

    /// Starts every quest whose Start component holds a `QuestActConAcceptItemGain` for
    /// `item_id` once the bag holds the required count. Deviation: the C# server never did this
    /// (`Inventory.cs` has the `OnItemGather` call commented out), which left the quests that
    /// hang on it (the 4881-4888 letters, 5450, 6160, ...) unstartable.
    pub fn start_quests_from_item_gain(&mut self, item_id: u32) {
        let data = self.data;
        let Some(quests) = data.item_gain_starters.get(&item_id) else {
            return;
        };
        for &quest_id in quests {
            if self.has_quest(quest_id) {
                continue;
            }
            if self.has_quest_completed(quest_id)
                && !data.template(quest_id).is_some_and(|t| t.repeatable)
            {
                continue;
            }
            // Probe the starter first: most gains do not reach the required count yet, which
            // must not be logged as refusals.
            if !self.starter_accepts(quest_id, super::model::acceptor::ITEM, item_id) {
                continue;
            }
            self.add_quest(quest_id, false, super::model::acceptor::ITEM, item_id);
        }
    }

    /// `DropQuest(questId, update, forcibly)`.
    pub fn drop_quest(&mut self, quest_id: u32, update: bool, forcibly: bool) {
        let Some(h) = self.st.active_handle(quest_id) else {
            return;
        };
        // make sure no further "update packets" are send to the player
        if let Some(q) = self.quest_mut(h) {
            q.skip_update = true;
        }
        // Cleanup runs for a quest that is being completed, however it got to the Reward step
        // (the early completion of a LetItDone quest does not set the status on every path).
        if self.is_completing(h) {
            self.quest_cleanup(h);
        }
        self.quest_drop(h, update);
        self.finalize_quest_acts(h);
        self.st.active.remove(quest_id);
        self.c.quests.removed.push(quest_id);
        if forcibly {
            self.set_completed_quest_flag(quest_id, false);
        }
        let msg = format!(
            "[Quest] for player: {}, quest: {quest_id} removed.",
            self.c.unit.name
        );
        self.send_debug_message(msg.clone());
        tracing::warn!("{msg}");
        self.server
            .systems
            .quests
            .remove_quest_timer(self.c.id, quest_id);
        if let Some(id) = self.quest(h).map(|q| q.id) {
            self.server.ids.quest.release_id(id);
        }
    }

    /// `FailQuest(owner, questId)`: a server-side failure (`CheckTimer` expiry, `CheckGuard`
    /// death). The client learns it from the status in `SCQuestContextUpdatedPacket` (status
    /// `Failed`, `OnQuestContextUpdated` `FUN_3937bdf0` raises the quest condition event and
    /// shows the failure message); `SCQuestContextFailedPacket` (0x146) stays reserved for a
    /// refused client report. A failed quest stays failed and droppable; restarting it is the
    /// client's `CSRestartMainQuestPacket` request ([`Ctx::try_restart_quest`]).
    pub fn fail_quest(&mut self, quest_id: u32) {
        let Some(h) = self.st.active_handle(quest_id) else {
            tracing::warn!(
                "FailQuest triggered for a quest that isn't active, Quest:{quest_id}, Player:{} ({})",
                self.c.unit.name,
                self.c.id
            );
            return;
        };
        self.set_status(h, status::FAILED);
        self.set_step(h, kind::FAIL);
        // During `CharacterQuests.Load` the `SCQuestsPacket` that follows already carries the
        // Failed row; an update sent before it would be out of order.
        if self.st.loaded {
            if let Some(row) = self.quest(h).map(Quest::row) {
                let now = self.now_ms;
                self.send(SCQuestContextUpdatedPacket::new(row, 0, now));
            }
        }
        tracing::debug!("[Quest] {}, quest {quest_id} failed.", self.c.unit.name);
    }

    /// `CSRestartMainQuestPacket` (0x0fd): the player asked to restart a main quest. The 1.2
    /// client only sends it for an active quest whose status is not `Completed` and whose
    /// template `detail_id` is 2 (`FUN_3937b200` / `FUN_3937a8d0`), after its own 3 s
    /// throttle. `restart_on_fail` is never read by the client; it is the inferred server gate
    /// (QUEST-1 in `PORTING.md`). The reply is a normal `SCQuestContextStartedPacket`, which
    /// the client turns into the `QUEST_CHAT_RESTART` bubble.
    pub fn try_restart_quest(&mut self, quest_id: u32) -> bool {
        const RESTART_THROTTLE_MS: i64 = 3_000;
        if self.now_ms.saturating_sub(self.st.restart_request_at) < RESTART_THROTTLE_MS {
            tracing::debug!(
                "RestartQuest throttled, Quest:{quest_id}, Player:{} ({})",
                self.c.unit.name,
                self.c.id
            );
            return false;
        }
        self.st.restart_request_at = self.now_ms;
        let Some(h) = self.st.active_handle(quest_id) else {
            tracing::debug!(
                "RestartQuest refused, Quest:{quest_id}, Player:{} ({}): the quest is not active",
                self.c.unit.name,
                self.c.id
            );
            return false;
        };
        let Some(template) = self.data.template(quest_id) else {
            return false;
        };
        let (restart_on_fail, detail_id) = (template.restart_on_fail, template.detail_id);
        if !restart_on_fail || detail_id != detail::MAIN {
            tracing::debug!(
                "RestartQuest refused, Quest:{quest_id}, Player:{} ({}): restart_on_fail {restart_on_fail}, detail_id {detail_id}",
                self.c.unit.name,
                self.c.id
            );
            return false;
        }
        if self
            .quest(h)
            .is_some_and(|q| matches!(q.status, status::COMPLETED | status::DAILY_COMPLETED))
        {
            tracing::debug!(
                "RestartQuest refused, Quest:{quest_id}, Player:{} ({}): the quest is completed",
                self.c.unit.name,
                self.c.id
            );
            return false;
        }
        let (acceptor_type, acceptor_id) = self
            .quest(h)
            .map_or((0, 0), |q| (q.acceptor_type, q.acceptor_id));
        self.restart_quest(h, quest_id, acceptor_type, acceptor_id)
    }

    /// Discards the current run and installs a fresh one from its first step. Unlike a
    /// give-up drop, it keeps the Start-component `ConAcceptItem` acceptor items (otherwise
    /// the new run stops at Start with the item gone) and skips `Quest.Drop`'s Ready/Reward
    /// skill and buff effects. The old object is removed so a dispatch that is still
    /// iterating its subscribers cannot act on it.
    fn restart_quest(
        &mut self,
        h: QuestHandle,
        quest_id: u32,
        acceptor_type: u8,
        acceptor_id: u32,
    ) -> bool {
        // Sphere triggers are keyed by (character, template): finalize the live step before
        // the act cleanup so a restart cannot leave a second set behind.
        if let Some(si) = self.quest(h).and_then(|q| q.step_index(q.step)) {
            self.finalize_step(h, si);
        }
        let data = self.data;
        if let Some(t) = data.template(quest_id) {
            for cid in &t.components {
                if let Some(c) = data.component(*cid) {
                    for &a in &c.acts {
                        self.act_quest_dropped(h, a, true);
                    }
                }
            }
        }
        self.finalize_quest_acts(h);
        self.st.active.remove(quest_id);
        self.c.quests.removed.push(quest_id);
        self.server
            .systems
            .quests
            .remove_quest_timer(self.c.id, quest_id);
        // Allocate the new run before releasing the old runtime id, so the timer event of
        // the discarded run cannot accidentally match the fresh one (ids are reused).
        let old_id = self.quest(h).map(|q| q.id);
        self.st.quests.remove(&h);
        let msg = format!("[Quest] {}, quest {quest_id} restarted.", self.c.unit.name);
        self.send_debug_message(msg);
        let started =
            self.install_quest(quest_id, true, acceptor_type, acceptor_id, "RestartQuest");
        if let Some(id) = old_id {
            self.server.ids.quest.release_id(id);
        }
        if !started {
            tracing::warn!(
                "RestartQuest could not restart quest {quest_id}, player {} ({})",
                self.c.unit.name,
                self.c.id
            );
            return false;
        }
        true
    }

    /// `SetStep(questContextId, step, selectedReward)` (GM helper).
    pub fn gm_set_step(&mut self, quest_id: u32, step: u32, selected_reward: i32) -> bool {
        if step > 8 {
            return false;
        }
        let Some(h) = self.st.active_handle(quest_id) else {
            return false;
        };
        if selected_reward >= 0 {
            if let Some(q) = self.quest_mut(h) {
                q.selected_reward_index = selected_reward;
            }
        }
        self.set_step(h, step as u8);
        true
    }

    /// `TryCompleteQuestAsLetItDone(questId, selectedReward)`.
    ///
    /// Deviation: the C# sets `Step = Reward` and leaves the rest to the evaluation of the Reward
    /// components, so a quest without a Reward step never completed, and the quest status was
    /// never `Completed` (the completion cleanup was skipped). Here the status becomes
    /// `Completed` with the step, and a quest without a Reward step is finished at once.
    pub fn try_complete_quest_as_let_it_done(&mut self, quest_id: u32, selected: i32) {
        let who = format!("{} ({})", self.c.unit.name, self.c.id);
        let Some(h) = self.st.active_handle(quest_id) else {
            self.log_refusal(
                "TryCompleteQuestAsLetItDone",
                quest_id,
                "the quest is not active",
            );
            self.send_report_refused(quest_id, quest_error::INVALID_QUEST);
            return;
        };
        let let_it_done = self.data.template(quest_id).is_some_and(|t| t.let_it_done);
        if !let_it_done {
            self.log_refusal(
                "TryCompleteQuestAsLetItDone",
                quest_id,
                "the quest cannot be completed early (not LetItDone)",
            );
            self.send_report_refused(quest_id, quest_error::INVALID_QUEST_STATUS);
            return;
        }
        let objective = self.objective_status(h);
        if objective < ObjectiveStatus::CanEarlyComplete {
            let objectives = self.quest(h).map(|q| q.objectives);
            self.log_refusal(
                "TryCompleteQuestAsLetItDone",
                quest_id,
                &format!("objectives not far enough ({objective:?}, objectives {objectives:?})"),
            );
            self.send_report_refused(quest_id, quest_error::INVALID_QUEST_STATUS);
            return;
        }
        if let Some(q) = self.quest_mut(h) {
            q.selected_reward_index = selected;
        }
        let (has_reward, has_ready) = self.quest(h).map_or((false, false), |q| {
            (
                q.step_index(kind::REWARD).is_some(),
                q.step_index(kind::READY).is_some(),
            )
        });
        if has_reward {
            // Go to the Reward step: its components complete the quest on the next evaluation.
            self.set_step(h, kind::REWARD);
            self.set_status(h, status::COMPLETED);
        } else if has_ready {
            // No Reward step: the Ready step holds whatever the quest hands out. Turn it in
            // there; with the report acts satisfied the step runs on to the quest completion.
            self.set_step(h, kind::READY);
            self.set_status(h, status::READY);
            self.override_report_acts(h, kind::READY);
            self.run_current_step(h);
            if self.quest(h).is_some_and(|q| q.step == kind::READY) {
                tracing::warn!(
                    "TryCompleteQuestAsLetItDone, {who}, Quest {quest_id}: has no Reward step and its Ready step did not complete, left at Ready"
                );
                self.send_report_refused(quest_id, quest_error::INVALID_QUEST_STATUS);
            }
        } else {
            // Neither a Ready nor a Reward step: complete right away.
            self.set_step(h, kind::REWARD);
            self.set_status(h, status::COMPLETED);
            self.go_to_next_step(h);
        }
    }

    /// Marks the report acts (`ConReportNpc` / `ConReportDoodad` / `ConReportJournal`) of a step
    /// as satisfied (the player is turning the quest in).
    fn override_report_acts(&mut self, h: QuestHandle, step_kind: u8) {
        let data = self.data;
        let Some(si) = self.quest(h).and_then(|q| q.step_index(step_kind)) else {
            return;
        };
        for p in self.step_paths(h, si) {
            let Some(a) = self.act_ref(h, p) else {
                continue;
            };
            if matches!(
                data.acts[a].kind,
                ActKind::ConReportNpc { .. }
                    | ActKind::ConReportDoodad { .. }
                    | ActKind::ConReportJournal
            ) {
                self.set_act_override(h, p, true);
            }
        }
    }

    /// `ResetQuests(questDetail, sendIfChanged)`.
    pub fn reset_quests(&mut self, details: &[u32], send_if_changed: bool) {
        let data = self.data;
        for bi in 0..self.c.quests.completed.len() {
            let block_id = self.c.quests.completed[bi].id;
            for idx in 0..64u32 {
                let quest_id = block_id * 64 + idx;
                let Some(t) = data.template(quest_id) else {
                    continue;
                };
                if self.has_quest(quest_id) {
                    continue;
                }
                for &qd in details {
                    let block = &mut self.c.quests.completed[bi];
                    if t.detail_id == qd && bit_get(&block.body, idx as usize) {
                        bit_set(&mut block.body, idx as usize, false);
                        let body = block.body8();
                        tracing::info!("QuestReset by {}, reset {quest_id}", self.c.unit.name);
                        if send_if_changed {
                            self.send(SCQuestContextResetPacket { quest_id, body });
                        }
                    }
                }
            }
        }
    }

    /// `ResetDailyQuests(sendPacketsIfChanged)`.
    pub fn reset_daily_quests(&mut self, send: bool) {
        self.reset_quests(&DAILY_DETAILS, send);
    }

    /// `CheckDailyResetAtLogin` (UTC dates of now and `LeaveTime`).
    pub fn check_daily_reset_at_login(&mut self) {
        let today = self.now_ms.div_euclid(1000).div_euclid(86_400);
        let leave_day = self.c.leave_time.div_euclid(86_400);
        if today - leave_day >= 1 {
            self.reset_daily_quests(false);
        }
    }

    /// `CharacterQuests.Load` from the `quests` rows already read into `c.quests.active`.
    pub fn load_from_rows(&mut self) {
        let rows = std::mem::take(&mut self.c.quests.active);
        for row in rows {
            if self.data.template(row.template_id).is_none() {
                tracing::error!(
                    "Quest {} by {} does not exist",
                    row.template_id,
                    self.c.unit.name
                );
                continue;
            }
            if self.st.has_quest(row.template_id) {
                tracing::error!(
                    "Quest {} of {} is in the quests table twice",
                    row.template_id,
                    self.c.unit.name
                );
                continue;
            }
            let h = self.new_quest(row.template_id, row.id);
            if let Some(q) = self.quest_mut(h) {
                q.status = row.status;
                q.acceptor_type = row.acceptor_type;
                q.acceptor_id = row.acceptor_id;
                q.time_ms = row.time_ms;
            }
            self.st.active.add(row.template_id, h);
            // Initializers need the saved deadline, acceptor and active quest identity.
            self.set_step(h, row.step);
            if let Some(q) = self.quest_mut(h) {
                q.objectives = row.objectives;
                q.acceptor_type = row.acceptor_type;
                q.component_id = row.component_id;
                q.acceptor_id = row.acceptor_id;
                q.status = row.status;
            }
            self.heal_loaded_status(h);
            // Timers in earlier components must also resume at a saved Progress step.
            if matches!(row.step, kind::START | kind::SUPPLY | kind::PROGRESS) {
                for path in self.all_paths(h) {
                    let Some(a) = self.act_ref(h, path) else {
                        continue;
                    };
                    if matches!(self.data.acts[a].kind, ActKind::CheckTimer { .. })
                        && self.data.acts[a].component_kind <= row.step
                        && !self
                            .server
                            .systems
                            .quests
                            .has_quest_timer(self.c.id, row.template_id)
                    {
                        self.act_initialize_action(h, path);
                    }
                }
                if self
                    .server
                    .systems
                    .quests
                    .has_quest_timer(self.c.id, row.template_id)
                    && self.quest(h).is_some_and(|q| q.time_ms <= self.now_ms)
                {
                    let _ = self.dispatch(&super::events::QuestEvent::TimerExpired {
                        quest_id: row.template_id,
                        run_id: row.id,
                    });
                }
            }
            self.quest_initialized(h);
            self.request_evaluation(h);
        }
        self.st.loaded = true;
    }

    /// A saved quest whose status says `Ready` while it still sits on its Progress step is only
    /// legal for `LetItDone` and score quests (their Progress step is left by a report, the
    /// status shows "ready to turn in"). On any other quest the status is stale, so it is reset
    /// to `Progress`; the evaluation requested by the load then moves the quest on to its Ready
    /// step when the objectives are complete.
    fn heal_loaded_status(&mut self, h: QuestHandle) {
        let Some(q) = self.quest(h) else {
            return;
        };
        if q.step != kind::PROGRESS || q.status != status::READY {
            return;
        }
        let template_id = q.template_id;
        let early = self
            .data
            .template(template_id)
            .is_some_and(|t| t.let_it_done || t.score > 0);
        if early {
            return;
        }
        tracing::warn!(
            "Quest {template_id} of {} ({}) was saved with status Ready on its Progress step, re-evaluating",
            self.c.unit.name,
            self.c.id
        );
        self.set_status(h, status::PROGRESS);
    }

    /// `OnQuestItemManuallyDestroyed(item)` (item template id + its `LootQuestId`).
    pub fn on_quest_item_manually_destroyed(&mut self, item_template_id: u32, loot_quest_id: u32) {
        if loot_quest_id == 0 {
            return;
        }
        let data = self.data;
        let mut do_drop = false;
        let handles: Vec<QuestHandle> = self.st.active.values().copied().collect();
        'quests: for h in handles {
            let Some(q) = self.quest(h) else {
                continue;
            };
            let Some(t) = data.template(q.template_id) else {
                continue;
            };
            let mut step = q.step;
            while step >= kind::START {
                for c in t.components_of(&data.components, step) {
                    for &a in &c.acts {
                        let at = &data.acts[a];
                        if let Some((item_id, drop)) = at.kind.generic_item() {
                            if item_id == item_template_id {
                                if drop {
                                    do_drop = true;
                                    break;
                                }
                                return;
                            }
                        }
                        match &at.kind {
                            ActKind::ObjItemGroupGather {
                                item_group_id,
                                drop_when_destroy,
                                ..
                            }
                            | ActKind::ObjItemGroupUse {
                                item_group_id,
                                drop_when_destroy,
                                ..
                            } if data.group_items.contains(*item_group_id, item_template_id) => {
                                if *drop_when_destroy {
                                    do_drop = true;
                                    break;
                                }
                                return;
                            }
                            ActKind::ObjItemUse {
                                item_id,
                                drop_when_destroy,
                                ..
                            } if *item_id == item_template_id => {
                                if *drop_when_destroy {
                                    do_drop = true;
                                    break;
                                }
                                return;
                            }
                            _ => {}
                        }
                    }
                    if do_drop {
                        break;
                    }
                }
                // C#: the step loop itself does not stop on doDropQuest
                step -= 1;
            }
            if do_drop {
                break 'quests;
            }
        }
        if do_drop {
            self.drop_quest(loot_quest_id, true, false);
        }
    }

    /// `GetActiveActsWithUseItem(itemId)`: component ids of the matching acts (quests whose
    /// current step has no components are skipped; the C# throws there).
    pub fn active_acts_with_use_item(&self, item_id: u32) -> Vec<(QuestHandle, ActPath)> {
        let data = self.data;
        let mut out = Vec::new();
        for h in self.st.active.values().copied() {
            let Some(q) = self.quest(h) else {
                continue;
            };
            let Some(si) = q.step_index(q.step) else {
                continue;
            };
            for (ci, c) in q.steps[si].components.iter().enumerate() {
                for (ai, a) in c.acts.iter().enumerate() {
                    if matches!(data.acts[a.template].kind, ActKind::ObjItemUse { item_id: i, .. } if i == item_id)
                    {
                        out.push((
                            h,
                            ActPath {
                                step: si,
                                component: ci,
                                act: ai,
                            },
                        ));
                    }
                }
            }
        }
        out
    }

    /// `AddQuestTimer(owner, quest, limitTime)`.
    pub fn add_quest_timer(&mut self, h: QuestHandle, limit_time: i32) -> bool {
        let template_id = self.template_id_of(h);
        let saved = self
            .quest(h)
            .map_or(super::state::DATETIME_MIN_MS, |q| q.time_ms);
        let deadline =
            if saved <= super::state::DATETIME_MIN_MS || saved >= super::state::DATETIME_MAX_MS {
                self.now_ms.saturating_add(i64::from(limit_time.max(0)))
            } else {
                saved
            };
        let remaining = deadline
            .saturating_sub(self.now_ms)
            .clamp(0, i64::from(limit_time.max(0))) as i32;
        // The task sleeps the clamped remaining time, so the deadline must match that, not a
        // saved value beyond the limit (a clock step can leave one there).
        let deadline = self.now_ms.saturating_add(i64::from(remaining));
        let id = self.quest(h).map_or(0, |q| q.id);
        let ok = match &self.player {
            Some(player) => self.server.systems.quests.add_player_quest_timer(
                player,
                template_id,
                id,
                remaining,
            ),
            None => {
                self.server
                    .systems
                    .quests
                    .add_quest_timer(self.c.id, template_id, id, remaining)
            }
        };
        if !ok {
            return false;
        }
        if let Some(q) = self.quest_mut(h) {
            q.time_ms = deadline;
        }
        self.send_debug_message(format!(
            "[Quest] Quest ({id}) will end in {} minutes.",
            limit_time / 60000
        ));
        true
    }

    /// Re-derives `c.quests.active` from the live quests (after every operation).
    pub fn sync_rows(&mut self) {
        self.c.quests.active = self.st.rows();
    }
}
