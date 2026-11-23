use super::{
    kind, status, ActPath, Ctx, EnvView, ObjectiveStatus, OwnerView, QResult, QuestHandle,
    TargetView, Throw, UnitReq,
};

impl<'a> Ctx<'a> {
    // ----- QuestStep / QuestComponent / QuestAct -----

    /// `QuestStep.InitializeStep`.
    pub(super) fn initialize_step(&mut self, h: QuestHandle, si: usize) {
        // Distribute any rewards that may still be open so the pool is empty
        self.distribute_rewards(h, false);
        for p in self.step_paths(h, si) {
            self.act_initialize_action(h, p);
        }
    }

    /// `QuestStep.FinalizeStep`.
    pub(super) fn finalize_step(&mut self, h: QuestHandle, si: usize) {
        for p in self.step_paths(h, si) {
            self.act_finalize_action(h, p);
        }
    }

    /// `QuestStep.ContainsObjectives`.
    pub fn contains_objectives(&self, h: QuestHandle, si: usize) -> bool {
        let data = self.data;
        self.quest(h)
            .and_then(|q| q.steps.get(si))
            .is_some_and(|s| {
                s.components
                    .iter()
                    .flat_map(|c| c.acts.iter())
                    .any(|a| data.acts[a.template].objective_index != 0xFF)
            })
    }

    /// `UnitRequirementsGameData.CanComponentRun(component, Owner)`.
    pub fn can_component_run(&self, component_id: u32) -> bool {
        let Some(ct) = self.data.component(component_id) else {
            return true;
        };
        let owner = OwnerView {
            c: self.c,
            st: self.st,
            server: self.server,
        };
        let env = EnvView {
            server: self.server,
            env: &self.env,
        };
        match &self.env.target {
            Some(t) => self.data.unit_reqs.can_component_run(
                component_id,
                ct.or_unit_reqs,
                &owner,
                &TargetView(t, self.server),
                &env,
            ),
            None => self.data.unit_reqs.can_component_run(
                component_id,
                ct.or_unit_reqs,
                &owner,
                &owner,
                &env,
            ),
        }
    }

    /// The requirement [`Ctx::can_component_run`] refuses on (same AND / OR order; an OR list
    /// that fails reports its first row), for `SCQuestUnitReqFailedPacket`.
    pub fn first_failed_component_req(&self, component_id: u32) -> Option<UnitReq> {
        let ct = self.data.component(component_id)?;
        let owner = OwnerView {
            c: self.c,
            st: self.st,
            server: self.server,
        };
        let env = EnvView {
            server: self.server,
            env: &self.env,
        };
        let req = match &self.env.target {
            Some(t) => self.data.unit_reqs.first_failure(
                "QuestComponent",
                component_id,
                ct.or_unit_reqs,
                &owner,
                &TargetView(t, self.server),
                &env,
            ),
            None => self.data.unit_reqs.first_failure(
                "QuestComponent",
                component_id,
                ct.or_unit_reqs,
                &owner,
                &owner,
                &env,
            ),
        };
        req.cloned()
    }

    /// `QuestStep.RunComponents`.
    pub(super) fn run_components(&mut self, h: QuestHandle, si: usize) -> bool {
        let data = self.data;
        let Some(q) = self.quest(h) else {
            return false;
        };
        let this_step = q.steps[si].kind;
        let Some(t) = data.template(q.template_id) else {
            return false;
        };
        let n = q.steps[si].components.len();
        // Cache which components are active
        for ci in 0..n {
            let cid = self
                .quest(h)
                .map_or(0, |q| q.steps[si].components[ci].template_id);
            let active = self.can_component_run(cid);
            if let Some(q) = self.quest_mut(h) {
                q.steps[si].components[ci].is_active = active;
            }
        }
        let mut res = true;
        if t.selective && this_step == kind::PROGRESS {
            // Require only one of the components to be true in the progress step
            res = false;
            for ci in 0..n {
                let Some((active, cid)) = self.component_info(h, si, ci) else {
                    continue;
                };
                if !active {
                    continue;
                }
                let r = self.run_component(h, si, ci);
                if r {
                    if let Some(q) = self.quest_mut(h) {
                        q.component_id = cid;
                    }
                }
                res |= r;
            }
        } else {
            // Requires all components to be true
            for ci in 0..n {
                let Some((active, cid)) = self.component_info(h, si, ci) else {
                    continue;
                };
                if !active {
                    res = false;
                    continue;
                }
                let r = self.run_component(h, si, ci);
                res &= r;
                if res {
                    if let Some(q) = self.quest_mut(h) {
                        q.component_id = cid;
                    }
                }
            }
        }

        // Override result for score quests
        if this_step == kind::PROGRESS && t.score > 0 {
            if !self.contains_objectives(h, si) {
                // Deviation: the C# compared the (always 0) score with `Template.Score` and
                // kept such a quest in Progress forever (quest 5641 has a score of 100 and no
                // acts). Without any score-bearing act the requirement counts as met.
                tracing::debug!(
                    "Quest {}: score {} but the Progress step has no objective acts, treating the score as reached",
                    t.id,
                    t.score
                );
            } else {
                let mut score = 0i32;
                if let Some(q) = self.quest(h) {
                    for c in q.steps[si].components.iter().filter(|c| c.is_active) {
                        for a in &c.acts {
                            let at = &data.acts[a.template];
                            // `questAct.GetObjective(Parent)` (index 0xFF would throw)
                            let o = q
                                .objectives
                                .get(usize::from(at.objective_index))
                                .copied()
                                .unwrap_or(0);
                            score = score.wrapping_add(o.wrapping_mul(at.count));
                        }
                    }
                }
                res = score >= t.score;
                let os = self.objective_status(h);
                self.set_status(
                    h,
                    if os >= ObjectiveStatus::QuestComplete {
                        status::READY
                    } else {
                        status::PROGRESS
                    },
                );
            }
        } else if this_step == kind::PROGRESS && t.let_it_done {
            let os = self.objective_status(h);
            res = os >= ObjectiveStatus::Overachieved;
            self.set_status(
                h,
                if os >= ObjectiveStatus::QuestComplete {
                    status::READY
                } else {
                    status::PROGRESS
                },
            );
        }

        // Handle Supply/Reward Distribution
        if this_step != kind::REWARD || res {
            res &= self.distribute_rewards(h, true);
        }

        // LetItBeDone type of quests, are always forced forward using the Report Acts
        if this_step == kind::PROGRESS && t.let_it_done {
            res = false;
        }
        res
    }

    fn component_info(&self, h: QuestHandle, si: usize, ci: usize) -> Option<(bool, u32)> {
        self.quest(h)
            .and_then(|q| q.steps.get(si))
            .and_then(|s| s.components.get(ci))
            .map(|c| (c.is_active, c.template_id))
    }

    /// `QuestComponent.RunComponent`.
    pub fn run_component(&mut self, h: QuestHandle, si: usize, ci: usize) -> bool {
        let Some(q) = self.quest(h) else {
            return false;
        };
        let step_kind = q.steps[si].kind;
        let comp = &q.steps[si].components[ci];
        let n = comp.acts.len();
        let cid = comp.template_id;
        let or_check = (step_kind == kind::START || step_kind == kind::READY) && n > 0;
        let mut res = true;
        if or_check {
            // Or checks, needed to handle quests with multiple starters or multiple report NPCs
            res = false;
            for ai in 0..n {
                let r = self.run_act(
                    h,
                    ActPath {
                        step: si,
                        component: ci,
                        act: ai,
                    },
                );
                res |= r;
            }
        } else {
            for ai in 0..n {
                let r = self.run_act(
                    h,
                    ActPath {
                        step: si,
                        component: ci,
                        act: ai,
                    },
                );
                res &= r;
            }
        }
        res |= self
            .quest(h)
            .and_then(|q| q.steps.get(si))
            .and_then(|s| s.components.get(ci))
            .is_some_and(|c| c.override_completed);
        // If acts completed, handle skill and buff effects
        if res {
            self.use_skill_and_buff(cid);
            self.set_npc_aggro(cid);
        }
        res
    }

    /// `QuestAct.RunAct`.
    pub fn run_act(&mut self, h: QuestHandle, p: ActPath) -> bool {
        let Some(a) = self.act_ref(h, p) else {
            return false;
        };
        let at = &self.data.acts[a];
        let count = match self.quest(h) {
            Some(q)
                if at.component_kind == kind::PROGRESS
                    && usize::from(at.objective_index) < q.objectives.len() =>
            {
                q.objectives[usize::from(at.objective_index)]
            }
            _ => 0,
        };
        let supply = at.kind.class_name().starts_with("QuestActSupply");
        if supply
            && self
                .quest(h)
                .is_some_and(|q| q.steps[p.step].components[p.component].acts[p.act].supplied)
        {
            return true;
        }
        let r = self.act_run(h, p, count);
        if r && supply {
            if let Some(q) = self.quest_mut(h) {
                q.steps[p.step].components[p.component].acts[p.act].supplied = true;
            }
        }
        r || self.act_override(h, p)
    }

    /// `QuestAct.OverrideObjectiveCompleted` getter.
    pub fn act_override(&self, h: QuestHandle, p: ActPath) -> bool {
        self.quest(h)
            .and_then(|q| q.steps.get(p.step))
            .and_then(|s| s.components.get(p.component))
            .and_then(|c| c.acts.get(p.act))
            .is_some_and(|a| a.override_completed)
    }

    /// `QuestAct.OverrideObjectiveCompleted` setter.
    pub fn set_act_override(&mut self, h: QuestHandle, p: ActPath, value: bool) {
        let changed = {
            let Some(c) = self
                .quest_mut(h)
                .and_then(|q| q.steps.get_mut(p.step))
                .and_then(|s| s.components.get_mut(p.component))
            else {
                return;
            };
            let Some(a) = c.acts.get_mut(p.act) else {
                return;
            };
            if a.override_completed == value {
                false
            } else {
                a.override_completed = value;
                if value {
                    c.override_completed = true;
                }
                true
            }
        };
        if changed {
            self.request_evaluation(h);
        }
    }

    // ----- objective helpers (QuestActTemplate) -----

    /// `SetObjective(quest, value)`.
    pub fn set_objective(&mut self, h: QuestHandle, act: usize, value: i32) -> QResult {
        let data = self.data;
        let at = &data.acts[act];
        let Some(q) = self.quest(h) else {
            return Ok(());
        };
        let Some(t) = data.template(q.template_id) else {
            return Ok(());
        };
        let max = at
            .max_objective(t)
            .ok_or_else(|| Throw("DivideByZeroException in MaxObjective".into()))?;
        let value = value.min(max);
        let idx = usize::from(at.objective_index);
        let Some(q) = self.quest_mut(h) else {
            return Ok(());
        };
        let Some(slot) = q.objectives.get_mut(idx) else {
            return Err(Throw(format!(
                "IndexOutOfRangeException: Objectives[{idx}] (act {} {})",
                at.kind.class_name(),
                at.detail_id
            )));
        };
        if *slot != value {
            *slot = value;
            self.request_evaluation(h);
        }
        Ok(())
    }

    /// `AddObjective(quest, amount)`; returns the new value.
    pub fn add_objective(&mut self, h: QuestHandle, act: usize, amount: i32) -> QResult<i32> {
        let data = self.data;
        let at = &data.acts[act];
        let idx = usize::from(at.objective_index);
        let Some(q) = self.quest(h) else {
            return Ok(0);
        };
        if amount == 0 {
            return q
                .objectives
                .get(idx)
                .copied()
                .ok_or_else(|| Throw(format!("IndexOutOfRangeException: Objectives[{idx}]")));
        }
        let Some(t) = data.template(q.template_id) else {
            return Ok(0);
        };
        let max = at
            .max_objective(t)
            .ok_or_else(|| Throw("DivideByZeroException in MaxObjective".into()))?;
        let Some(q) = self.quest_mut(h) else {
            return Ok(0);
        };
        let Some(slot) = q.objectives.get_mut(idx) else {
            return Err(Throw(format!(
                "IndexOutOfRangeException: Objectives[{idx}]"
            )));
        };
        if max > 0 && slot.wrapping_add(amount) >= max {
            *slot = max;
        } else {
            *slot = slot.wrapping_add(amount);
        }
        let v = *slot;
        self.request_evaluation(h);
        Ok(v)
    }

    /// `GetObjective(quest)` (safe read: an out-of-range index reads 0).
    pub fn get_objective(&self, h: QuestHandle, act: usize) -> i32 {
        let idx = usize::from(self.data.acts[act].objective_index);
        self.quest(h)
            .and_then(|q| q.objectives.get(idx).copied())
            .unwrap_or(0)
    }
}
