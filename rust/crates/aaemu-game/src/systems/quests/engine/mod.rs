//! The quest state machine: `Quest` (+ `NewQuestCode`), `QuestStep`, `QuestComponent`,
//! `QuestAct`, `CharacterQuests` and the `QuestManager` helpers they call, run against one
//! locked character ([`Ctx`]). The per-act behaviour is in `acts.rs`.

use std::sync::Arc;

use super::data::QuestData;
use super::hooks::QuestHooks;
use super::model::{
    acceptor, condition, detail, error_msg, item_task, kind, npc_ai, status,
    ItemCreationDefinition, ObjectiveStatus, FACTION_MONSTROSITY,
};
use super::packets::{
    quest_error, SCQuestContextCompletedPacket, SCQuestContextFailedPacket,
    SCQuestContextResetPacket, SCQuestContextStartedPacket, SCQuestContextUpdatedPacket,
    SCQuestRewardedByMailPacket, SCQuestUnitReqFailedPacket,
};
use super::reward_scale::{gap_exp, scaled_supply_copper, scaled_supply_exp};
use super::state::{
    ActPath, CinemaMark, Quest, QuestActState, QuestComponentState, QuestHandle, QuestState,
    QuestStepState,
};
use super::templates::ActKind;
use super::unit_reqs::{ReqEnv, ReqSubject, UnitReq};
use crate::models::buff::{Buff, SkillCaster};
use crate::models::char_data::CompletedQuest;
use crate::models::character::Character;
use crate::models::container::slot_type;
use crate::net::packet::ServerPacket;
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::PlayerRef;

use super::{events, model, quest_schedule_allows, state};
mod character_quests;
mod components;
mod requirements;
mod rewards;
pub use requirements::{EnvView, OwnerView, TargetView};

/// A C# exception thrown inside quest code (index out of range, division by zero, ...): it
/// aborts the current event invocation list / evaluation like the C# `throw` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Throw(pub String);

pub type QResult<T = ()> = Result<T, Throw>;

/// Why [`Ctx::start_refusal`] refuses to start a quest (`None` = it may start). The reason
/// carries the component / item id the caller logs or answers with; the checks themselves are
/// side-effect free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRefusal {
    /// The quest is already active (a non-forced start; a forced one drops and reinstalls it).
    Active,
    /// No `quest_contexts` row for the id.
    UnknownQuest,
    /// `game_schedule_quests`: the quest's schedule window is not in progress.
    ScheduleWindow,
    /// `CanAcceptSupplyItems`: this auto-equip trade pack cannot go to the backpack slot.
    SupplyItem(u32),
    /// The Start component's `unit_reqs` are not met.
    ComponentUnitReq(u32),
    /// Already completed and not repeatable.
    Completed,
}

/// A snapshot of a unit taken before the character was locked (the current target).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnitSnapshot {
    pub obj_id: u32,
    /// `BaseUnitType` (0 = Character, 1 = Npc, ...).
    pub base_unit_type: u8,
    pub is_unit: bool,
    pub npc_template_id: Option<u32>,
    pub doodad_template_id: Option<u32>,
    pub level: u8,
    pub hpp: i32,
    pub in_battle: bool,
    pub under_water: bool,
    pub faction_id: u32,
    pub mother_faction_id: u32,
    pub buff_ids: Vec<u32>,
    pub zone_key: u32,
    pub position: (f32, f32, f32),
}

/// A unit taking part in a kill (`OnKillArgs` / `OnZoneKillArgs` members), captured when the
/// event is raised.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KillUnitInfo {
    pub obj_id: u32,
    /// Set for player characters.
    pub character_id: Option<u32>,
    /// Set for NPCs.
    pub npc_template_id: Option<u32>,
    /// `Faction.Id`.
    pub faction_id: u32,
    pub level: u8,
    /// `Npc.GetAggroRatingInPercent(owner.ObjId)` for the event's owner (100 when not an NPC).
    pub aggro_percent: f32,
    /// World position at kill time. The 1.2 compact's ObjAggro range (r208022
    /// loader 0x8a0450) provisionally limits credit by owner-to-victim distance.
    pub position: (f32, f32, f32),
}

/// World facts an operation needs, captured before the character is locked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuestEnv {
    /// `Owner.CurrentTarget` (None = no target / not found).
    pub target: Option<UnitSnapshot>,
    /// NPCs in the owner's neighbourhood: (template id, world x, y, z).
    pub nearby_npcs: Vec<(u32, f32, f32, f32)>,
    /// Owner world position.
    pub position: (f32, f32, f32),
    /// `TimeManager.GetTime`.
    pub time_of_day: f32,
}

impl QuestEnv {
    /// `Owner.CurrentTarget is Npc npc ? npc.TemplateId`.
    pub fn target_npc_template(&self) -> Option<u32> {
        self.target.as_ref().and_then(|t| t.npc_template_id)
    }
}

/// Work the operation leaves for after the character lock is released.
#[derive(Debug, Clone, PartialEq)]
pub enum Deferred {
    /// `Owner.UseSkill(skillId, Owner)`.
    UseSkillSelf {
        skill_id: u32,
    },
    /// `ParentWorld.GetNpcByTemplateId(npcId)?.UseSkill(skillId, npc)`.
    NpcUseSkill {
        npc_template_id: u32,
        skill_id: u32,
    },
    /// `ParentWorld.GetNpcByTemplateId(npcId)?.SetFaction(faction)`.
    NpcSetFaction {
        npc_template_id: u32,
        faction_id: u32,
    },
    NpcComponent {
        component_id: u32,
    },
    /// `teamMember.Character.Events.OnX(sender, args)` for team-shared acts.
    TeamEvent {
        event: super::events::QuestEvent,
    },
    /// `SendDebugMessage` / system chat line.
    SystemMessage {
        text: String,
    },
}

/// One quest operation on one locked character (`Quest.Owner` + `CharacterQuests`).
pub struct Ctx<'a> {
    pub server: &'a GameServer,
    pub data: &'a QuestData,
    pub hooks: Arc<dyn QuestHooks>,
    /// The player (packets, evaluation queue); `None` only in data-level tests.
    pub player: Option<PlayerRef>,
    pub c: &'a mut Character,
    pub st: &'a mut QuestState,
    pub env: QuestEnv,
    pub now_ms: i64,
    pub deferred: Vec<Deferred>,
    /// Buffs were added (dispatch the buff events once unlocked).
    pub buffs_changed: bool,
    /// Packets sent through [`Ctx::send`] when there is no player (tests).
    pub sent: Vec<(u16, Vec<u8>)>,
    /// What the report acts saw while a `ReportNpc` / `ReportDoodad` event was dispatched.
    pub report_reply: Option<ReportReply>,
    /// Quest ids already answered with [`SCQuestContextFailedPacket`] in this operation.
    pub report_refused: Vec<u32>,
    /// Whether a refused report is answered with [`SCQuestContextFailedPacket`]: only a
    /// report the client asked for (`CSCompleteQuestContext`) puts it in quest directing
    /// mode. A server-side report (`DoodadFuncQuest`, a doodad click on an owned quest) is
    /// only logged, so a click before the objectives are met shows no quest error.
    pub answer_reports: bool,
    /// Whether a refused start (`AddQuest` blocked by a start-component unit requirement) is
    /// answered with [`SCQuestUnitReqFailedPacket`]: only the requests the client made
    /// (`CSStartQuestContext`, all four context kinds). Automatic starters (level up, item
    /// gain, sphere entry, ...) stay silent, like [`Ctx::log_refusal`]'s repeat suppression.
    pub answer_start_refusals: bool,
}

/// The result of one report event, collected across the quest's report acts: a component may
/// hold several report NPCs / doodads (OR acts), so a mismatch in one act does not refuse the
/// report when another act accepts it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportReply {
    /// A report act matched the NPC / doodad of the event.
    pub target_matched: bool,
    /// The matching act advanced the quest.
    pub accepted: bool,
    /// Refusal of the matching act (it matched but could not run).
    pub matched_refusal: Option<u8>,
    /// Refusal of a non-matching act (wrong NPC / doodad).
    pub mismatch_refusal: Option<u8>,
}

/// Quest ids from here on have a completed block id above `u16::MAX` (`SCCompletedQuests`).
const WIRE_QUEST_ID_LIMIT: u32 = (u16::MAX as u32 + 1) * 64;

/// Minimum time between two WARN lines for the same refusal of the same quest.
const REFUSAL_REPEAT_MS: i64 = 60_000;

/// r208022 0x7e71b0 and 0x7e5d70 round up at 150% of the target.
fn overdone_threshold(target: i32) -> i32 {
    (target as f32 * 1.5).ceil() as i32
}

impl<'a> Ctx<'a> {
    // ----- plumbing -----

    pub fn send<P: ServerPacket + 'static>(&mut self, p: P) {
        match &self.player {
            Some(pl) => pl.send(p),
            None => {
                let mut s = aaemu_commons::PacketStream::new();
                p.write_body(&mut s);
                self.sent.push((p.opcode(), s.into_bytes()));
            }
        }
    }

    /// `Owner.SendErrorMessage(type)` (`type = 0, isNotify = true`).
    pub fn send_error(&mut self, error: i16) {
        self.send(SCErrorMsgPacket::new(error, 0, true));
    }

    /// `Owner.SendDebugMessage(message)`.
    pub fn send_debug_message(&mut self, text: String) {
        let cfg = &self.server.config;
        if cfg.debug_info && i64::from(self.c.access_level) >= i64::from(cfg.debug_info_level) {
            self.deferred.push(Deferred::SystemMessage { text });
        }
    }

    pub fn owner_id(&self) -> u32 {
        self.c.id
    }

    /// A quest request the server refused, logged at WARN with the character, the quest and
    /// the reason (bug reports depend on it). A refusal repeated within [`REFUSAL_REPEAT_MS`]
    /// (an automatic trigger such as a level up or a sphere entry) goes to DEBUG.
    pub fn log_refusal(&mut self, what: &str, quest_id: u32, reason: &str) {
        let key = (quest_id, format!("{what}: {reason}"));
        if self.st.refusal_log.len() > 1024 {
            self.st.refusal_log.clear();
        }
        let repeated = self
            .st
            .refusal_log
            .get(&key)
            .is_some_and(|&t| self.now_ms.saturating_sub(t) < REFUSAL_REPEAT_MS);
        if repeated {
            tracing::debug!(
                "{what} refused, {} ({}), Quest {quest_id}: {reason}",
                self.c.unit.name,
                self.c.id
            );
        } else {
            tracing::warn!(
                "{what} refused, {} ({}), Quest {quest_id}: {reason}",
                self.c.unit.name,
                self.c.id
            );
            self.st.refusal_log.insert(key, self.now_ms);
        }
    }

    /// Answers a refused quest report with `SCQuestContextFailedPacket` (0x146), at most
    /// once per quest for this operation. Independent of [`Ctx::log_refusal`]'s repeat
    /// suppression: even a refusal that is only logged at DEBUG is answered.
    pub fn send_report_refused(&mut self, quest_id: u32, err: u8) {
        if !self.answer_reports || self.report_refused.contains(&quest_id) {
            return;
        }
        self.report_refused.push(quest_id);
        self.send(SCQuestContextFailedPacket { quest_id, err });
    }

    /// The reply of the report event being dispatched, created on first use.
    pub fn report_reply(&mut self) -> &mut ReportReply {
        self.report_reply.get_or_insert_with(ReportReply::default)
    }

    /// Sends the answer of a finished report event when no report act accepted the turn-in.
    /// `wrong_target` is the error for an NPC/doodad that is not one of the quest's report
    /// targets.
    pub fn finish_report(&mut self, quest_id: u32, wrong_target: u8) {
        let Some(reply) = self.report_reply.take() else {
            // Active quest, but no report act is listening at this step.
            self.send_report_refused(quest_id, quest_error::INVALID_QUEST_STATUS);
            return;
        };
        if reply.accepted {
            return;
        }
        let err = if reply.target_matched {
            reply
                .matched_refusal
                .unwrap_or(quest_error::INVALID_QUEST_STATUS)
        } else {
            reply.mismatch_refusal.unwrap_or(wrong_target)
        };
        self.send_report_refused(quest_id, err);
    }

    fn quest(&self, h: QuestHandle) -> Option<&Quest> {
        self.st.quests.get(&h)
    }

    fn quest_mut(&mut self, h: QuestHandle) -> Option<&mut Quest> {
        self.st.quests.get_mut(&h)
    }

    pub fn template_id_of(&self, h: QuestHandle) -> u32 {
        self.quest(h).map_or(0, |q| q.template_id)
    }

    fn act_ref(&self, h: QuestHandle, p: ActPath) -> Option<usize> {
        self.quest(h)
            .and_then(|q| q.steps.get(p.step))
            .and_then(|s| s.components.get(p.component))
            .and_then(|c| c.acts.get(p.act))
            .map(|a| a.template)
    }

    // ----- Quest (constructor, NewQuestCode) -----

    /// `new Quest(template, owner)`: `CreateQuestSteps` + `InitializeQuestActs`.
    pub fn new_quest(&mut self, template_id: u32, id: u32) -> QuestHandle {
        let data = self.data;
        let mut q = Quest::new_blank(id, template_id);
        if let Some(t) = data.template(template_id) {
            // CreateQuestSteps
            for cid in &t.components {
                let Some(ct) = data.component(*cid) else {
                    continue;
                };
                let si = match q.steps.iter().position(|s| s.kind == ct.kind_id) {
                    Some(i) => i,
                    None => {
                        q.steps.push(QuestStepState {
                            kind: ct.kind_id,
                            components: Vec::new(),
                        });
                        q.steps.len() - 1
                    }
                };
                q.steps[si].components.push(QuestComponentState {
                    template_id: *cid,
                    override_completed: false,
                    is_active: true,
                    acts: ct
                        .acts
                        .iter()
                        .map(|&a| QuestActState {
                            template: a,
                            override_completed: false,
                            sphere_inside: false,
                            supplied: false,
                        })
                        .collect(),
                });
            }
            if q.steps.is_empty() {
                tracing::warn!("Quest {template_id} does not seem to have any components!");
            }
        }
        let h = self.st.alloc(q);
        // InitializeQuestActs
        for p in self.all_paths(h) {
            self.act_initialize_quest(h, p);
        }
        h
    }

    /// Every act of a quest in `QuestSteps` / `Components` / `Acts` order.
    pub fn all_paths(&self, h: QuestHandle) -> Vec<ActPath> {
        let mut v = Vec::new();
        if let Some(q) = self.quest(h) {
            for (si, s) in q.steps.iter().enumerate() {
                for (ci, c) in s.components.iter().enumerate() {
                    for ai in 0..c.acts.len() {
                        v.push(ActPath {
                            step: si,
                            component: ci,
                            act: ai,
                        });
                    }
                }
            }
        }
        v
    }

    fn step_paths(&self, h: QuestHandle, si: usize) -> Vec<ActPath> {
        let mut v = Vec::new();
        if let Some(s) = self.quest(h).and_then(|q| q.steps.get(si)) {
            for (ci, c) in s.components.iter().enumerate() {
                for ai in 0..c.acts.len() {
                    v.push(ActPath {
                        step: si,
                        component: ci,
                        act: ai,
                    });
                }
            }
        }
        v
    }

    /// `FinalizeQuestActs`.
    pub fn finalize_quest_acts(&mut self, h: QuestHandle) {
        for p in self.all_paths(h) {
            self.act_finalize_quest(h, p);
        }
    }

    /// `StartQuest`.
    pub fn start_quest(&mut self, h: QuestHandle) -> bool {
        let Some(q) = self.quest(h) else {
            return false;
        };
        let template_id = q.template_id;
        let Some(si) = q.step_index(kind::START) else {
            tracing::warn!(
                "Tried to start a quest without a starter component Quest: {template_id}"
            );
            return false;
        };
        if !q.forced_start && !self.starter_accepts(q.template_id, q.acceptor_type, q.acceptor_id) {
            return false;
        }
        self.set_step(h, kind::START);
        // Send the first components, or the one that's used to start this ?
        let first = self
            .quest(h)
            .and_then(|q| q.steps[si].components.first())
            .map_or(0, |c| c.template_id);
        if let Some(q) = self.quest_mut(h) {
            q.component_id = first;
        }
        let row = self.quest(h).map(Quest::row);
        if let Some(row) = row {
            let now = self.now_ms;
            self.send(SCQuestContextStartedPacket {
                quest: row,
                component_id: first,
                now_ms: now,
            });
        }
        tracing::debug!(
            "StartQuest, Quest:{template_id}, Player {} ({})",
            self.c.unit.name,
            self.c.id
        );
        true
    }

    fn starter_accepts(&self, quest_id: u32, source: u8, id: u32) -> bool {
        let Some(template) = self.data.template(quest_id) else {
            return false;
        };
        template
            .components_of(&self.data.components, kind::START)
            .into_iter()
            .filter(|component| self.can_component_run(component.id))
            .any(|component| {
                let conditions: Vec<_> = component
                    .acts
                    .iter()
                    .filter_map(|&a| {
                        let at = &self.data.acts[a];
                        use ActKind::*;
                        Some(match at.kind {
                            ConAcceptNpc { npc_id }
                            | ConAcceptNpcKill { npc_id }
                            | ConAcceptNpcEmotion { npc_id, .. } => {
                                source == super::model::acceptor::NPC && id == npc_id
                            }
                            ConAcceptDoodad { doodad_id } => {
                                source == super::model::acceptor::DOODAD && id == doodad_id
                            }
                            ConAcceptItem { item_id, .. } => {
                                source == super::model::acceptor::ITEM
                                    && id == item_id
                                    && check_items(self.c, item_id, 1)
                            }
                            ConAcceptItemGain { item_id } => {
                                source == super::model::acceptor::ITEM
                                    && id == item_id
                                    && check_items(self.c, item_id, at.count)
                            }
                            ConAcceptItemEquip { item_id } => {
                                source == super::model::acceptor::ITEM
                                    && id == item_id
                                    && items_by_template(self.c, &[slot_type::EQUIPMENT], item_id).0
                            }
                            ConAcceptSkill { skill_id } => {
                                source == super::model::acceptor::SKILL && id == skill_id
                            }
                            ConAcceptBuff { buff_id } => {
                                source == super::model::acceptor::BUFF && id == buff_id
                            }
                            ConAcceptSphere { sphere_id } => {
                                source == super::model::acceptor::SPHERE && id == sphere_id
                            }
                            ConAcceptLevelUp { level } => self.c.unit.level >= level,
                            ConAutoComplete | ConAcceptComponent { .. } => true,
                            _ => return None,
                        })
                    })
                    .collect();
                conditions.is_empty() || conditions.iter().any(|valid| *valid)
            })
    }

    /// `RunCurrentStep`.
    pub fn run_current_step(&mut self, h: QuestHandle) -> bool {
        let Some(q) = self.quest(h) else {
            return false;
        };
        let Some(si) = q.step_index(q.step) else {
            return false;
        };
        if q.step == kind::REWARD
            && !self.valid_reward_choice(q.template_id, q.selected_reward_index)
        {
            return false;
        }
        let mut res = self.run_components(h, si);

        // A LetItBeDone quest normally waits for a report. When it has no Ready step, an
        // authored auto-complete reward (e.g. Free the Shimmering Skyfin) also accepts full
        // objective progress. Leave partial progress available for a manual early turn-in.
        let data = self.data;
        if let Some(q) = self.quest(h) {
            if let Some(t) = data.template(q.template_id) {
                if q.step == kind::PROGRESS && t.let_it_done && q.step_index(kind::READY).is_none()
                {
                    let auto_complete = t
                        .components_of(&data.components, kind::REWARD)
                        .iter()
                        .flat_map(|c| &c.acts)
                        .any(|&a| matches!(data.acts[a].kind, ActKind::ConAutoComplete));
                    res |= (t.score > 0 && self.objective_percent(h) >= 1.0)
                        || (auto_complete
                            && self.objective_status(h) >= ObjectiveStatus::QuestComplete);
                }
            }
        }

        // Deviation: `SetStep` clears `ComponentId` (the final C# then sent 0 in the update of
        // a step change), but the client plays the cinema of a non-dialog component only when
        // an update names that component, and the pre-rewrite C# sent the current one. The
        // component that completed the Progress step is named once, in this update. (Only
        // Progress: `SCQuestContextStartedPacket` already named the Start component, and the
        // client runs a named component's effects again for every packet that names it.)
        let completed_component = if res {
            self.quest(h)
                .filter(|q| q.step == kind::PROGRESS)
                .map_or(0, |q| q.component_id)
        } else {
            0
        };
        if res {
            self.go_to_next_step(h);
        }

        // Send update to player
        if let Some(q) = self.quest(h) {
            if !q.skip_update {
                let named = completed_component != 0;
                let cid = if named {
                    completed_component
                } else {
                    q.component_id
                };
                let row = q.row();
                let now = self.now_ms;
                self.send(SCQuestContextUpdatedPacket::new(row, cid, now));
                if named {
                    self.expect_component_cinema(cid);
                }
            }
        }
        res
    }

    /// An update named `component_id`: when the component has a cinema the client now plays it
    /// (unless the component is dialog-driven), so the next `CSStartedCinemaPacket` is that
    /// cinema (Rust addition; `events::request_cinema` for a running operation). A component
    /// without a cinema leaves a pending request alone.
    fn expect_component_cinema(&mut self, component_id: u32) {
        if let Some(cinema_id) = self.data.component_cinema_id(component_id) {
            self.st.requested_cinema = Some(CinemaMark {
                cinema_id,
                at_ms: self.now_ms,
            });
        }
    }

    /// `GoToNextStep`.
    pub fn go_to_next_step(&mut self, h: QuestHandle) {
        let Some(last_step) = self.quest(h).map(|q| q.step) else {
            return;
        };
        let template_id = self.template_id_of(h);
        let mut loop_lock_check = 0;
        loop {
            if loop_lock_check > 20 {
                tracing::error!(
                    "GoToNextStep got stuck in a infinite loop for Quest:{template_id}, Step:{} -> {}, Player {} ({}",
                    kind::name(last_step),
                    kind::name(self.quest(h).map_or(0, |q| q.step)),
                    self.c.unit.name,
                    self.c.id
                );
                break;
            }
            loop_lock_check += 1;
            let step = self.quest(h).map_or(kind::INVALID, |q| q.step);
            match step {
                kind::NONE => self.set_step(h, kind::SUPPLY),
                kind::START => self.set_step(h, kind::NONE),
                kind::SUPPLY => {
                    self.set_step(h, kind::PROGRESS);
                    self.set_status(h, status::PROGRESS);
                }
                kind::PROGRESS => {
                    self.set_step(h, kind::READY);
                    self.set_status(h, status::READY);
                }
                kind::FAIL => {
                    self.set_status(h, status::FAILED);
                    return;
                }
                kind::READY => {
                    self.set_step(h, kind::REWARD);
                    self.set_status(h, status::COMPLETED);
                }
                kind::DROP => {
                    self.drop_quest(template_id, true, false);
                    return;
                }
                kind::REWARD => {
                    // Mark quest as completed
                    let block = self.set_completed_quest_flag(template_id, true);
                    self.drop_quest(template_id, false, false);
                    let _ = self.dispatch(&super::events::QuestEvent::QuestComplete {
                        quest_id: template_id,
                    });
                    self.report_quest_completed(template_id);
                    self.send(SCQuestContextCompletedPacket {
                        quest_id: template_id,
                        body: block.body8(),
                        component_id: 0,
                    });
                    return;
                }
                other => {
                    tracing::warn!(
                        "Quest GoToNextStep failed for Step:{}, Quest:{template_id}, Player:{} ({}",
                        kind::name(other),
                        self.c.unit.name,
                        self.c.id
                    );
                    return;
                }
            }
            // If the new current step is valid for this quest template, exit the loop
            if let Some(q) = self.quest(h) {
                if q.step_index(q.step).is_some() {
                    break;
                }
            }
        }
    }

    /// COMPLETE_QUEST_TYPE / COMPLETE_QUEST_CATEGORY achievement credit for one completed run of
    /// `quest_id`. The report is deferred by the achievement hooks until this quest operation has
    /// released the character and economy locks.
    fn report_quest_completed(&self, quest_id: u32) {
        let Some(player) = &self.player else {
            return;
        };
        let category_id = self.data.template(quest_id).map_or(0, |t| t.category_id);
        let mut inputs = vec![AchievementInput::Increment {
            kind_id: char_record_kind::COMPLETE_QUEST_TYPE,
            amount: 1,
            matcher: RecordMatch::Value1(quest_id),
        }];
        if category_id != 0 {
            inputs.push(AchievementInput::Increment {
                kind_id: char_record_kind::COMPLETE_QUEST_CATEGORY,
                amount: 1,
                matcher: RecordMatch::Value1(category_id),
            });
        }
        crate::systems::report_achievements(self.server, player, &inputs);
    }

    pub(super) fn set_status(&mut self, h: QuestHandle, s: u8) {
        if let Some(q) = self.quest_mut(h) {
            q.status = s;
        }
    }

    /// The number of `QuestActSupplySelectiveItem` acts of the quest (`ThisSelectiveIndex` of
    /// the last one: the indices are 1-based, which is also what the 1.2 client sends).
    pub fn selective_reward_count(&self, template_id: u32) -> i32 {
        let data = self.data;
        data.template(template_id).map_or(0, |t| {
            t.components
                .iter()
                .filter_map(|c| data.component(*c))
                .flat_map(|c| c.acts.iter())
                .filter(|&&a| matches!(data.acts[a].kind, ActKind::SupplySelectiveItem { .. }))
                .count() as i32
        })
    }

    /// r208022 0x4b1f30 indexes the authored Reward choices from one.
    pub(crate) fn valid_reward_choice(&self, template_id: u32, selected: i32) -> bool {
        let count = self.selective_reward_count(template_id);
        count == 0 || (1..=count).contains(&selected)
    }

    /// `Step` setter (`SetStep`).
    pub fn set_step(&mut self, h: QuestHandle, value: u8) {
        let Some(q) = self.quest(h) else {
            return;
        };
        if value == q.step {
            return;
        }
        // Finalize old Step (if any)
        if let Some(old) = q.step_index(q.step) {
            self.finalize_step(h, old);
        }
        let template_id = {
            let Some(q) = self.quest_mut(h) else {
                return;
            };
            q.step = value;
            // Reset active component (used by packet only)
            q.component_id = 0;
            q.template_id
        };
        // Initialize Acts for this Step (if any)
        if let Some(new) = self.quest(h).and_then(|q| q.step_index(value)) {
            self.initialize_step(h, new);
        }
        // Trigger OnQuestStepChanged event, even if this step is not available
        let _ = self.dispatch(&super::events::QuestEvent::QuestStepChanged {
            quest_id: template_id,
            step: value,
        });
        self.request_evaluation(h);
    }

    /// `RequestEvaluation` (`RequestEvaluationFlag = true`).
    pub fn request_evaluation(&mut self, h: QuestHandle) {
        let Some(q) = self.quest_mut(h) else {
            return;
        };
        if q.request_evaluation {
            return;
        }
        q.request_evaluation = true;
        if q.init_finished {
            self.enqueue_evaluation(h);
        }
    }

    /// `QuestInitialized`.
    pub fn quest_initialized(&mut self, h: QuestHandle) {
        let Some(q) = self.quest_mut(h) else {
            return;
        };
        q.init_finished = true;
        if q.request_evaluation {
            self.enqueue_evaluation(h);
        }
    }

    /// `StartingEvaluation`.
    pub fn starting_evaluation(&mut self, h: QuestHandle) {
        if let Some(q) = self.quest_mut(h) {
            q.request_evaluation = false;
        }
    }

    /// `QuestManager.EnqueueEvaluation(quest)`.
    fn enqueue_evaluation(&mut self, h: QuestHandle) {
        let template_id = self.template_id_of(h);
        tracing::info!(
            "EnqueueEvaluation, {} ({}), Quest {template_id}",
            self.c.unit.name,
            self.c.id
        );
        match &self.player {
            Some(p) => self.server.systems.quests.enqueue_evaluation(p, h),
            None => self.st.local_queue_push(h),
        }
    }

    /// `GetQuestObjectiveStatus`.
    pub fn objective_status(&self, h: QuestHandle) -> ObjectiveStatus {
        let data = self.data;
        let Some(q) = self.quest(h) else {
            return ObjectiveStatus::NotReady;
        };
        let Some(t) = data.template(q.template_id) else {
            return ObjectiveStatus::QuestComplete;
        };
        let Some(si) = q.step_index(kind::PROGRESS) else {
            return ObjectiveStatus::QuestComplete;
        };
        if !self.contains_objectives(h, si) {
            return ObjectiveStatus::QuestComplete;
        }
        let step = &q.steps[si];
        let obj = |idx: u8| q.objectives.get(usize::from(idx)).copied().unwrap_or(0);
        if t.score > 0 {
            let mut score = 0i32;
            for c in step.components.iter().filter(|c| c.is_active) {
                for a in &c.acts {
                    let at = &data.acts[a.template];
                    if at.objective_index == 0xFF {
                        continue;
                    }
                    score = score.wrapping_add(at.count.wrapping_mul(obj(at.objective_index)));
                }
            }
            let ceil = |f: f32| (t.score as f32 * f).ceil() as i32;
            if t.let_it_done && score >= overdone_threshold(t.score) {
                return ObjectiveStatus::Overachieved;
            }
            if t.let_it_done && score > t.score {
                return ObjectiveStatus::ExtraProgress;
            }
            if score >= t.score {
                return ObjectiveStatus::QuestComplete;
            }
            if t.let_it_done && score >= ceil(1.0 / 2.0) {
                return ObjectiveStatus::CanEarlyComplete;
            }
            return ObjectiveStatus::NotReady;
        }
        let mut min = ObjectiveStatus::Overachieved;
        let mut max = ObjectiveStatus::NotReady;
        for c in step.components.iter().filter(|c| c.is_active) {
            for a in &c.acts {
                let at = &data.acts[a.template];
                if at.objective_index == 0xFF {
                    continue;
                }
                let o = obj(at.objective_index);
                let s = if t.let_it_done && o >= overdone_threshold(at.count) {
                    ObjectiveStatus::Overachieved
                } else if t.let_it_done && o > at.count {
                    ObjectiveStatus::ExtraProgress
                } else if o >= at.count {
                    ObjectiveStatus::QuestComplete
                } else if t.let_it_done && o >= at.count / 2 {
                    ObjectiveStatus::CanEarlyComplete
                } else {
                    ObjectiveStatus::NotReady
                };
                min = min.min(s);
                max = max.max(s);
            }
        }
        if t.selective {
            min.max(max)
        } else {
            min.min(max)
        }
    }

    /// Whether `quest_id` can be handed in at the doodad of template `doodad_id`: the quest
    /// is active, one of its `QuestActConReportDoodad` acts names that doodad, and the
    /// objectives meet the report act's threshold (the same check `ReportDoodad` runs).
    /// A ready quest is reported to the client with `SCDoodadCompleteQuestPacket` instead of
    /// being completed server-side.
    pub fn report_doodad_ready(&self, quest_id: u32, doodad_id: u32) -> bool {
        let Some(h) = self.st.active_handle(quest_id) else {
            return false;
        };
        let Some(q) = self.st.quest(h) else {
            return false;
        };
        let Some(si) = q.step_index(kind::READY) else {
            return false;
        };
        let data = self.data;
        let targets = self.step_paths(h, si).into_iter().any(|p| {
            self.act_ref(h, p).is_some_and(|a| {
                matches!(
                    data.acts[a].kind,
                    ActKind::ConReportDoodad { doodad_id: d, .. } if d == doodad_id
                )
            })
        });
        targets && self.report_ready(h, quest_id)
    }

    /// `GetQuestObjectivePercent`.
    pub fn objective_percent(&self, h: QuestHandle) -> f32 {
        let data = self.data;
        let Some(q) = self.quest(h) else {
            return 1.0;
        };
        let Some(t) = data.template(q.template_id) else {
            return 1.0;
        };
        let Some(si) = q.step_index(kind::PROGRESS) else {
            return 1.0;
        };
        if !self.contains_objectives(h, si) {
            return 1.0;
        }
        let step = &q.steps[si];
        let obj = |idx: u8| q.objectives.get(usize::from(idx)).copied().unwrap_or(0);
        if t.score > 0 {
            let mut score = 0i32;
            for c in step.components.iter().filter(|c| c.is_active) {
                for a in &c.acts {
                    let at = &data.acts[a.template];
                    if at.objective_index == 0xFF {
                        continue;
                    }
                    score = score.wrapping_add(at.count.wrapping_mul(obj(at.objective_index)));
                }
            }
            if t.let_it_done {
                score = score.min(overdone_threshold(t.score));
            }
            return score as f32 / t.score as f32;
        }
        let mut highest = 0f32;
        for c in step.components.iter().filter(|c| c.is_active) {
            for a in &c.acts {
                let at = &data.acts[a.template];
                if at.objective_index == 0xFF {
                    continue;
                }
                if at.count <= 0 {
                    continue;
                }
                let mut count = obj(at.objective_index);
                if t.let_it_done {
                    count = count.min(overdone_threshold(at.count));
                }
                highest = highest.max(count as f32 / at.count as f32);
            }
        }
        highest
    }

    // ----- Quest (rewards, drop, cleanup) -----

    /// `UseSkillAndBuff(component)`.
    pub fn use_skill_and_buff(&mut self, component_id: u32) {
        self.use_skill(component_id);
        self.use_buff(component_id);
    }

    fn use_buff(&mut self, component_id: u32) {
        let Some(ct) = self.data.component(component_id) else {
            return;
        };
        if ct.buff_id == 0 {
            return;
        }
        let Some(t) = self.server.game_data.skills.buffs.get(&ct.buff_id).cloned() else {
            tracing::warn!(
                "Quest component {component_id}: buff template {} not loaded",
                ct.buff_id
            );
            return;
        };
        let caster = Some((self.c.id, self.c.unit.level));
        let buff = Buff::new(t, SkillCaster::default(), caster, self.now_ms);
        self.c.unit.buffs.add_buff(buff, 0, 0, self.now_ms);
        self.buffs_changed = true;
    }

    /// `UseSkill(component)`.
    fn use_skill(&mut self, component_id: u32) {
        let Some(ct) = self.data.component(component_id) else {
            return;
        };
        if ct.skill_id > 0 {
            if ct.skill_self {
                self.deferred.push(Deferred::UseSkillSelf {
                    skill_id: ct.skill_id,
                });
            } else if ct.npc_id > 0 {
                self.deferred.push(Deferred::NpcUseSkill {
                    npc_template_id: ct.npc_id,
                    skill_id: ct.skill_id,
                });
            }
        }
    }

    /// `SetNpcAggro(component)`.
    pub fn set_npc_aggro(&mut self, component_id: u32) {
        let Some(ct) = self.data.component(component_id) else {
            return;
        };
        if ct.npc_ai_id == npc_ai::ATTACK_UNIT && ct.npc_id > 0 {
            self.deferred.push(Deferred::NpcSetFaction {
                npc_template_id: ct.npc_id,
                faction_id: FACTION_MONSTROSITY,
            });
        }
        if ct.npc_spawner_id > 0
            || matches!(
                ct.npc_ai_id,
                npc_ai::FOLLOW_UNIT
                    | npc_ai::FOLLOW_PATH
                    | npc_ai::GO_AWAY
                    | npc_ai::RUN_COMMAND_SET
            )
        {
            self.deferred.push(Deferred::NpcComponent { component_id });
        }
    }

    /// The quest reached its Reward step or `Completed` status (as opposed to being dropped by
    /// the player / a GM / a failed state).
    fn is_completing(&self, h: QuestHandle) -> bool {
        self.quest(h)
            .is_some_and(|q| q.status == status::COMPLETED || q.step == kind::REWARD)
    }

    /// `Quest.Drop(update)`.
    pub fn quest_drop(&mut self, h: QuestHandle, update: bool) {
        let completing = self.is_completing(h);
        self.set_status(h, status::DROPPED);
        self.set_step(h, kind::DROP);
        let data = self.data;
        let template_id = self.template_id_of(h);
        let Some(t) = data.template(template_id) else {
            return;
        };
        for step in kind::READY..kind::REWARD {
            if let Some(c) = t.first_component(&data.components, step) {
                self.use_skill(c.id);
                self.use_buff(c.id);
            }
        }
        if update {
            if let Some(row) = self.quest(h).map(Quest::row) {
                let now = self.now_ms;
                self.send(SCQuestContextUpdatedPacket::new(row, 0, now));
            }
        }
        if !completing {
            for cid in &t.components {
                if let Some(c) = data.component(*cid) {
                    for &a in &c.acts {
                        self.act_quest_dropped(h, a, false);
                    }
                }
            }
        }
        // ClearObjectives
        if let Some(q) = self.quest_mut(h) {
            q.objectives = [0; 5];
        }
        self.request_evaluation(h);
    }

    /// `Quest.Cleanup()`.
    pub fn quest_cleanup(&mut self, h: QuestHandle) {
        let data = self.data;
        let Some(t) = data.template(self.template_id_of(h)) else {
            return;
        };
        for cid in &t.components {
            if let Some(c) = data.component(*cid) {
                for &a in &c.acts {
                    self.act_quest_cleanup(h, a);
                }
            }
        }
    }
}

// ----- completed quest blocks (BitArray over the stored bytes) -----

pub(crate) fn bit_get(body: &[u8], idx: usize) -> bool {
    body.get(idx / 8).is_some_and(|b| b >> (idx % 8) & 1 == 1)
}

pub(crate) fn bit_set(body: &mut Vec<u8>, idx: usize, v: bool) {
    if body.len() <= idx / 8 {
        body.resize(idx / 8 + 1, 0);
    }
    if v {
        body[idx / 8] |= 1 << (idx % 8);
    } else {
        body[idx / 8] &= !(1 << (idx % 8));
    }
}

/// `HasQuestCompleted(questId)`.
pub fn has_quest_completed(blocks: &[CompletedQuest], quest_id: u32) -> bool {
    let block_id = quest_id / 64;
    blocks
        .iter()
        .find(|b| b.id == block_id)
        .is_some_and(|b| bit_get(&b.body, (quest_id % 64) as usize))
}

/// `SetCompletedQuestFlag(questId, isCompleted)`.
pub fn set_completed_quest_flag(
    blocks: &mut Vec<CompletedQuest>,
    quest_id: u32,
    completed: bool,
) -> CompletedQuest {
    let block_id = quest_id / 64;
    let idx = (quest_id % 64) as usize;
    let i = match blocks.iter().position(|b| b.id == block_id) {
        Some(i) => i,
        None => {
            blocks.push(CompletedQuest::new(block_id));
            blocks.len() - 1
        }
    };
    bit_set(&mut blocks[i].body, idx, completed);
    blocks[i].clone()
}

/// `Inventory.GetAllItemsByTemplate(containers, templateId, -1, ..)` -> (found, units);
/// empty `containers` = Inventory + Equipment + Bank.
pub fn items_by_template(c: &Character, containers: &[u8], template_id: u32) -> (bool, i32) {
    let default = [slot_type::INVENTORY, slot_type::EQUIPMENT, slot_type::BANK];
    let list: &[u8] = if containers.is_empty() {
        &default
    } else {
        containers
    };
    let mut found = false;
    let mut units = 0i32;
    for st in list {
        if let Some(cont) = c.inventory.get(*st) {
            let (ids, n) = cont
                .lock()
                .expect("container poisoned")
                .items_by_template_unlocked(template_id, -1);
            found |= !ids.is_empty();
            units = units.wrapping_add(n);
        }
    }
    (found, units)
}

/// `Inventory.GetItemsCount(templateId)`.
pub fn items_count(c: &Character, template_id: u32) -> i32 {
    let (found, units) = items_by_template(c, &[], template_id);
    if found {
        units
    } else {
        0
    }
}

/// `Inventory.CheckItems(SlotType.Inventory, templateId, count)`.
pub fn check_items(c: &Character, template_id: u32, count: i32) -> bool {
    let (_, units) = items_by_template(c, &[slot_type::INVENTORY], template_id);
    units >= count
}
