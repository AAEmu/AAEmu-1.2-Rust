//! `Core/Managers/TrialManager` + `Models/Game/Crime/{TrialData,TrialCourtRoom,TrialJuryBox,
//! TrialStep,TrialSentenceResult,CourtRoomRegion}` + `Models/Tasks/Crime/TrialUpdateTask` and
//! `ChatManager.GetTrialChat(character)`.
//!
//! Locking: one mutex holds the court rooms, the trials and the jury queues. While holding it the
//! code locks at most one player's / doodad's data at a time (copy, release, act), sends packets
//! (non-blocking), joins / leaves chat channels, broadcasts to the players around an object and
//! reads the crime manager (order: trials -> crimes -> object data). Nothing calls into this
//! manager while holding a player's data lock. Calls that may re-enter other subsystems (buff
//! event dispatch, `PostUpdateCurrentHp`, `ForceDismount`, `RemoveBuffs` by tag, `SetFaction`)
//! are collected as [`Effect`]s and run after the mutex is released. The `check_buff_tag` /
//! `judge_obj_id` hooks are called while the mutex is held (they must not call back into the
//! trial manager).
//!
//! Time: every function that reads `DateTime.UtcNow` takes `now_ms` (unix ms); the 5 s
//! `TrialUpdateTask` calls [`TrialManager::update_tick`] with the real time.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::data::skills::BuffKind;
use crate::data::world_data::factions;
use crate::db_character::CrimeFactionChange;
use crate::managers::chat::ChannelRef;
use crate::net::packet::ServerPacket;
use crate::packets::world::{SCChatMessagePacket, SCUnitPointsPacket};
use crate::server::GameServer;
use crate::systems::skills::packets_special::SCLoadInstancePacket;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::systems::social::chat::{chat_type, court_room_region};
use crate::systems::social::config::SpawnPosition;
use crate::systems::social::crime::{
    self, add_own_buff, buff, crime_state, fmt_vec3, set_crime_point, set_infamy_point,
    set_jury_point, sus_log, world_pos_rot, CrimeEvent,
};
use crate::systems::social::packets::trial::*;
use crate::systems::social::util::{self, error, send_error};
use crate::world::{broadcast, DoodadRef, PlayerRef, WorldObject};

mod court;
mod effects;
mod verdict;

pub use self::court::generate_crime_records_packet_list;
use self::court::{
    cleanup_jury_member, cleanup_trial, create_trial_case, enter_court_jail, enter_court_room,
    generate_jury_list, summon_jury_member, teleport,
};
pub use self::effects::Effect;
use self::effects::{run_effects, Effects};
use self::verdict::{calculate_jail_time, finalize_verdict, result_is_guilty};

/// `TrialManager.TrialInitSeconds`.
pub const TRIAL_INIT_SECONDS: i64 = 10;
/// `TrialManager.AwaitingJurySummonsMinutes`.
pub const AWAITING_JURY_SUMMONS_MINUTES: i64 = 2;
const CONFIRM_CRIMINAL_RECORD_MINUTES: i64 = 5;
const CLOSING_STATEMENT_MINUTES: i64 = 1;
const JURY_VERDICT_MINUTES: i64 = 1;
const END_TRIAL_SECONDS: i64 = 30;
/// `TrialUpdateTask` interval.
pub const TRIAL_TICK_INTERVAL_MS: u64 = 5000;

/// `DateTime.MaxValue` as a step end time.
const TIME_MAX: i64 = i64::MAX;

const MINUTE_MS: i64 = 60_000;
const SECOND_MS: i64 = 1000;
// Durations on the 1.2 wire and on buffs are signed milliseconds.
const MAX_JAIL_MINUTES: i32 = i32::MAX / 60_000;

/// `TrialStep`.
pub mod trial_step {
    /// 0 - Actual trial has not started yet, waiting in court jail.
    pub const DEFENDANT_AWAITING_TRIAL: u8 = 0;
    /// 1 - Initialize the trial, send crime records and stuff to defendant.
    pub const VERIFY_CRIMINAL_RECORD: u8 = 1;
    /// 2 - Send out jury invites and wait for them.
    pub const AWAITING_JURY_SUMMONS: u8 = 2;
    /// 3 - Shows crime records to participants.
    pub const CONFIRM_CRIMINAL_RECORD: u8 = 3;
    /// 4 - Defendant closing statement (asks to skip).
    pub const CLOSING_STATEMENT: u8 = 4;
    /// 5 - Jury decides verdict (shows selection box to jury).
    pub const JURY_VERDICT: u8 = 5;
    /// 6 - Trial canceled => default sentence.
    pub const TRIAL_CANCELLED: u8 = 6;
    /// 7 - Defendant plead guilty.
    pub const PLEAD_GUILTY: u8 = 7;
    /// 8 - Trial has ended.
    pub const END_TRIAL: u8 = 8;
    /// Internal cleanup.
    pub const CLEANUP: u8 = 9;
    /// No longer active.
    pub const INVALID: u8 = 10;
}

/// `TrialSentenceResult`.
pub mod sentence_result {
    pub const UNDEFINED: u8 = 0;
    pub const NOT_GUILTY: u8 = 1;
    pub const GUILTY1: u8 = 2;
    pub const GUILTY2: u8 = 3;
    pub const GUILTY3: u8 = 4;
    pub const GUILTY4: u8 = 5;
    pub const GUILTY5: u8 = 6;
}

use trial_step as step;

/// `TrialCourtRoom`.
#[derive(Debug, Clone)]
pub struct CourtRoom {
    pub id: u32,
    pub name: String,
    /// [`court_room_region`].
    pub region: u8,
    /// `FactionsEnum`.
    pub faction: u32,
    /// `JudgeSpawner` (an opaque spawner id registered by the npcs subsystem; the judge's obj id
    /// is resolved through `SocialHooks::judge_obj_id`).
    pub judge_spawner: Option<u32>,
    pub jury_seats: BTreeMap<i32, DoodadRef>,
    pub audience_seats: Vec<DoodadRef>,
    pub audience_members: Vec<PlayerRef>,
    pub defendant: SpawnPosition,
    /// `Jail` (the configuration's `HoldingCell`).
    pub jail: SpawnPosition,
    /// `TrialChatChannel` (`None` = the C# null channel of an invalid region).
    pub chat: Option<ChannelRef>,
    /// `CurrentTrial` (trial id).
    pub current_trial: Option<u32>,
}

impl CourtRoom {
    /// `ClearRoom()`.
    pub fn clear_room(&mut self) {
        self.current_trial = None;
        self.jury_seats.clear();
    }

    fn join_chat(&self, p: &PlayerRef) {
        match &self.chat {
            Some(c) => {
                c.join(p);
            }
            None => tracing::error!("Court room {} has no trial chat channel", self.id),
        }
    }

    fn leave_chat(&self, p: &PlayerRef) {
        match &self.chat {
            Some(c) => {
                c.leave(p);
            }
            None => tracing::error!("Court room {} has no trial chat channel", self.id),
        }
    }
}

/// `TrialJuryBox`.
#[derive(Debug, Clone)]
pub struct JuryBox {
    pub seat: DoodadRef,
    pub seat_id: i32,
    pub jury_member: Option<PlayerRef>,
    pub confirm_testimony: bool,
    pub selected_sentence: i32,
}

/// `TrialData`.
#[derive(Debug, Clone)]
pub struct TrialData {
    pub id: u32,
    /// [`court_room_region`] of the court handling this case.
    pub court_region: u8,
    /// `CourtRoom` (court room id).
    pub court_room: Option<u32>,
    /// [`trial_step`].
    pub step: u8,
    pub defendant: PlayerRef,
    pub defendant_id: u32,
    pub defendant_name: String,
    /// `CurrentStepEndTime` (unix ms; `i64::MAX` = `DateTime.MaxValue`).
    pub current_step_end_ms: i64,
    pub jury: BTreeMap<i32, JuryBox>,
    /// Characters actually invited to this case.
    pub invited_jury: Vec<u32>,
    /// Minutes.
    pub jail_time: i32,
    pub evidence: Vec<CrimeEvent>,
}

impl TrialData {
    /// `RemainingCurrentStepTime` (`(uint)TotalMilliseconds`, saturating like .NET 10).
    pub fn remaining_ms(&self, now_ms: i64) -> u32 {
        self.current_step_end_ms
            .saturating_sub(now_ms)
            .clamp(0, i64::from(u32::MAX)) as u32
    }

    /// `SendPackets(packet)`: the defendant and every jury member.
    pub fn send_packets(&self, packet: &dyn ServerPacket) {
        let mut out = crate::net::connection::Fanout::new(packet);
        out.send(&self.defendant.conn);
        for j in self.jury.values() {
            if let Some(m) = &j.jury_member {
                out.send(&m.conn);
            }
        }
    }

    /// `GetActiveJuryCount()`.
    pub fn active_jury_count(&self) -> i32 {
        if self.court_room.is_none() {
            return 0;
        }
        self.jury
            .values()
            .filter(|j| j.jury_member.as_ref().is_some_and(|m| m.data().is_online))
            .count() as i32
    }

    /// `AllJuryConfirmedCriminalRecords()`.
    pub fn all_jury_confirmed_criminal_records(&self) -> bool {
        self.jury.values().all(|j| {
            !j.jury_member.as_ref().is_some_and(|m| m.data().is_online) || j.confirm_testimony
        })
    }

    /// `AllJurySelectedSentence()`.
    pub fn all_jury_selected_sentence(&self) -> bool {
        self.jury.values().all(|j| {
            !j.jury_member.as_ref().is_some_and(|m| m.data().is_online) || j.selected_sentence >= 0
        })
    }

    fn state_packet(&self, cur_jury: i32, now_ms: i64) -> SCChangeTrialStatePacket {
        SCChangeTrialStatePacket {
            trial_id: self.id,
            state: self.step,
            cur_jury,
            remain_time: self.remaining_ms(now_ms),
        }
    }

    fn send_state(&self, now_ms: i64) {
        self.send_packets(&self.state_packet(self.active_jury_count(), now_ms));
    }

    /// `SCChangeJuryVerdictCountPacket` to every seated juror who has not voted yet.
    /// Returns `(votes cast, seated jurors)`. The audience is INFERRED: `verdict.alb`
    /// registers `VERDICT_COUNT` and prints the remaining count, and the C# sent no such packet.
    fn send_verdict_count(&self) -> (i32, i32) {
        let total = self
            .jury
            .values()
            .filter(|j| j.jury_member.is_some())
            .count() as i32;
        let count = self
            .jury
            .values()
            .filter(|j| j.jury_member.is_some() && j.selected_sentence >= 0)
            .count() as i32;
        for j in self.jury.values() {
            if j.selected_sentence < 0 {
                if let Some(m) = &j.jury_member {
                    m.send(SCChangeJuryVerdictCountPacket { count, total });
                }
            }
        }
        (count, total)
    }
}

#[derive(Debug, Default)]
struct State {
    loading: bool,
    loaded: bool,
    /// `CourtRooms` (configuration order).
    rooms: Vec<CourtRoom>,
    /// `Trials` (a `ConcurrentDictionary`: iterated here in id order).
    trials: BTreeMap<u32, TrialData>,
    /// `JuryQueues` (mother faction -> player ids).
    queues: Vec<(u32, Vec<u32>)>,
}

/// `TrialManager`.
#[derive(Debug, Default)]
pub struct TrialManager {
    state: Mutex<State>,
}

/// `GetCourtRoomRegionByFaction(faction)`.
pub fn court_room_region_by_faction(faction: u32) -> u8 {
    match faction {
        factions::NUIA_ALLIANCE => court_room_region::NUIAN,
        factions::HARANYA_ALLIANCE => court_room_region::HARANYAN,
        _ => court_room_region::INVALID,
    }
}

/// `float.DegToRad()`.
fn deg_to_rad(v: f32) -> f32 {
    std::f32::consts::PI / 180.0 * v
}

fn room_by_id(rooms: &[CourtRoom], id: Option<u32>) -> Option<&CourtRoom> {
    let id = id?;
    rooms.iter().find(|r| r.id == id)
}

fn room_by_id_mut(rooms: &mut [CourtRoom], id: Option<u32>) -> Option<&mut CourtRoom> {
    let id = id?;
    rooms.iter_mut().find(|r| r.id == id)
}

fn mother_of(server: &GameServer, p: &PlayerRef) -> u32 {
    let f = p.data().unit.faction_id;
    util::mother_id(server, f)
}

/// `RemovePlayerFromQueue(player)`.
fn remove_from_queues(queues: &mut [(u32, Vec<u32>)], id: u32) {
    for (_, q) in queues.iter_mut() {
        if let Some(i) = q.iter().position(|x| *x == id) {
            q.remove(i);
        }
    }
}

/// `GetParticipatingTrial(player)`.
fn participating_trial(
    rooms: &[CourtRoom],
    trials: &BTreeMap<u32, TrialData>,
    p: &PlayerRef,
) -> Option<u32> {
    for room in rooms {
        let Some(tid) = room.current_trial else {
            continue;
        };
        let Some(t) = trials.get(&tid) else {
            continue;
        };
        if Arc::ptr_eq(&t.defendant, p) {
            return Some(tid);
        }
        if t.jury
            .values()
            .any(|j| j.jury_member.as_ref().is_some_and(|m| Arc::ptr_eq(m, p)))
        {
            return Some(tid);
        }
        if room.audience_members.iter().any(|a| Arc::ptr_eq(a, p)) {
            return Some(tid);
        }
    }
    None
}

/// `CanAcceptTrialInvites(character)`.
fn can_accept_trial_invites(
    server: &GameServer,
    rooms: &[CourtRoom],
    trials: &BTreeMap<u32, TrialData>,
    p: Option<&PlayerRef>,
) -> bool {
    let Some(p) = p else {
        return false;
    };
    let (online, jury_point, crime_point, detained) = {
        let c = p.data();
        (
            c.is_online,
            c.jury_point,
            c.crime_point,
            c.unit.buffs.check_buffs(&[
                buff::FORCIBLY_AWAITING_TRIAL,
                buff::TRIAL_DEFENDANT,
                buff::PRISONER_NUIAN,
                buff::PRISONER_HARANYAN,
            ]),
        )
    };
    if !online || detained || jury_point <= 0 || crime_point >= crime::WANTED_CRIME_POINT_THRESHOLD
    {
        return false;
    }
    let hooks = server.systems.social.hooks.get();
    if hooks.check_buff_tag(server, p, buff::TAG_OFFENDER)
        || hooks.check_buff_tag(server, p, buff::TAG_PRISONER)
    {
        return false;
    }
    participating_trial(rooms, trials, p).is_none()
}

/// Why [`TrialManager::force_summon_jury`] refused to seat a player. The GM command maps
/// these to chat messages; no packet is sent for a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceJuryError {
    NoTrial,
    NotSummoning,
    NotInMainWorld,
    AlreadyInTrial,
    NoSeat,
}

impl TrialManager {
    fn st(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("trial manager poisoned")
    }

    /// `Load()`: the jury queues and the configured court rooms (seats and judges are added
    /// by [`TrialManager::initialize_doodads`] / [`TrialManager::initialize_npc`]). The 5 s tick
    /// is started by [`TrialManager::start_tick`].
    pub fn load(&self, server: &GameServer) {
        let mut st = self.st();
        if st.loading || st.loaded {
            return;
        }
        st.loading = true;
        st.queues.push((factions::NUIA_ALLIANCE, Vec::new()));
        st.queues.push((factions::HARANYA_ALLIANCE, Vec::new()));
        for cfg in &server.config.justice.court_rooms {
            if st.rooms.iter().any(|r| r.id == cfg.id) {
                // `Dictionary.Add` throws in C#.
                tracing::error!("Duplicate court room id {} in the configuration", cfg.id);
                continue;
            }
            let region = court_room_region_by_faction(cfg.faction);
            st.rooms.push(CourtRoom {
                id: cfg.id,
                name: cfg.name.clone(),
                region,
                faction: cfg.faction,
                judge_spawner: None,
                jury_seats: BTreeMap::new(),
                audience_seats: Vec::new(),
                audience_members: Vec::new(),
                defendant: cfg.defendant,
                jail: cfg.holding_cell,
                chat: server.chat.court_chat(region),
                current_trial: None,
            });
        }
        tracing::info!("Loaded trials data");
        st.loaded = true;
        st.loading = false;
    }

    /// Schedules the `TrialUpdateTask` (`Schedule(task, null, 5 s)`: first run immediately, then
    /// every 5 s).
    pub fn start_tick(server: &Arc<GameServer>) {
        let weak = Arc::downgrade(server);
        tokio::spawn(async move {
            let mut iv =
                tokio::time::interval(std::time::Duration::from_millis(TRIAL_TICK_INTERVAL_MS));
            loop {
                iv.tick().await;
                let Some(server) = weak.upgrade() else {
                    break;
                };
                server
                    .systems
                    .social
                    .trials
                    .update_tick(&server, util::now_ms())
                    .await;
            }
        });
    }

    /// Updates the cached defendant name of an in-progress trial (character rename). Returns the
    /// number of changed trials. Trials are runtime only; the name is not persisted.
    pub fn rename_character(&self, character_id: u32, new_name: &str) -> usize {
        let mut st = self.st();
        let mut changed = 0;
        for trial in st.trials.values_mut() {
            if trial.defendant_id == character_id && trial.defendant_name != new_name {
                trial.defendant_name = new_name.to_string();
                changed += 1;
            }
        }
        changed
    }

    /// The configured court rooms (snapshot).
    pub fn court_rooms(&self) -> Vec<CourtRoom> {
        self.st().rooms.clone()
    }

    /// `GetTrial(trialId)` (snapshot).
    pub fn get_trial(&self, trial_id: u32) -> Option<TrialData> {
        self.st().trials.get(&trial_id).cloned()
    }

    /// The ids of the running trials.
    pub fn trial_ids(&self) -> Vec<u32> {
        self.st().trials.keys().copied().collect()
    }

    /// The jury queue of a mother faction (snapshot).
    pub fn jury_queue(&self, faction: u32) -> Vec<u32> {
        self.st()
            .queues
            .iter()
            .find(|q| q.0 == faction)
            .map(|q| q.1.clone())
            .unwrap_or_default()
    }

    /// `InitializeDoodads(world)`: `links` are the (`SpecialLink`, last spawned doodad) pairs of
    /// the world's `justice.` doodad spawners (`SpawnManager.GetSpecialDoodadSpawners`).
    pub fn initialize_doodads(&self, links: &[(String, DoodadRef)]) {
        let mut st = self.st();
        for room in st.rooms.iter_mut() {
            for (link, doodad) in links {
                for seat_id in 1..=5 {
                    if *link == format!("justice.{}.seat.{seat_id}", room.id) {
                        match room.jury_seats.entry(seat_id) {
                            // `Dictionary.Add` throws in C#.
                            std::collections::btree_map::Entry::Occupied(_) => {
                                tracing::error!("Duplicate jury seat {link}")
                            }
                            std::collections::btree_map::Entry::Vacant(v) => {
                                v.insert(doodad.clone());
                            }
                        }
                    }
                }
                if *link == format!("justice.{}.spectator", room.id) {
                    room.audience_seats.push(doodad.clone());
                }
            }
        }
    }

    /// `InitializeNpc(world)`: `links` are the (`SpecialLink`, spawner id) pairs of the world's
    /// `justice.` npc spawners; the first `justice.<court>.judge` becomes the court's judge.
    pub fn initialize_npc(&self, links: &[(String, u32)]) {
        let mut st = self.st();
        for room in st.rooms.iter_mut() {
            for (link, spawner) in links {
                if *link == format!("justice.{}.judge", room.id) && room.judge_spawner.is_none() {
                    room.judge_spawner = Some(*spawner);
                    break;
                }
            }
        }
    }

    /// `UpdateJuryQueue()`.
    fn update_jury_queue(&self, server: &GameServer, st: &mut State) {
        let State {
            rooms,
            trials,
            queues,
            ..
        } = st;
        let mut to_remove = Vec::new();
        if let Some(w) = server.world.world(0) {
            for ch in w.all_characters() {
                if !can_accept_trial_invites(server, rooms, trials, Some(&ch)) {
                    continue;
                }
                let mother = mother_of(server, &ch);
                let Some((_, q)) = queues.iter_mut().find(|q| q.0 == mother) else {
                    continue;
                };
                if !q.contains(&ch.character_id) {
                    q.push(ch.character_id);
                }
            }
        }
        for (faction, q) in queues.iter() {
            for &pid in q {
                let player = server.world.character_by_id(pid);
                if !can_accept_trial_invites(server, rooms, trials, player.as_ref())
                    || player.is_some_and(|p| *faction != mother_of(server, &p))
                {
                    to_remove.push(pid);
                }
            }
        }
        for pid in to_remove {
            for (_, q) in queues.iter_mut() {
                if let Some(i) = q.iter().position(|x| *x == pid) {
                    q.remove(i);
                }
            }
        }
    }

    /// `UpdateTrialStates()`.
    fn update_trial_states(&self, server: &GameServer, st: &mut State, now: i64, fx: &mut Effects) {
        let State {
            rooms,
            trials,
            queues,
            ..
        } = st;
        let ids: Vec<u32> = trials.keys().copied().collect();
        for id in ids {
            let mut remove = false;
            {
                let Some(t) = trials.get_mut(&id) else {
                    continue;
                };
                // Waiting for an empty court room when the defendant decided to go on trial.
                if t.step == step::DEFENDANT_AWAITING_TRIAL && t.current_step_end_ms <= now {
                    let Some(ri) = rooms
                        .iter()
                        .position(|r| r.region == t.court_region && r.current_trial.is_none())
                    else {
                        continue;
                    };
                    enter_court_room(server, rooms, queues, t, ri, now, fx);
                }
                if t.step > step::AWAITING_JURY_SUMMONS
                    && t.step < step::TRIAL_CANCELLED
                    && t.active_jury_count() <= 0
                {
                    // No jury left during a trial: auto-cancel it.
                    t.step = step::TRIAL_CANCELLED;
                }
                // Nobody left in the queue: skip waiting for the others.
                if t.step == step::AWAITING_JURY_SUMMONS {
                    let faction = room_by_id(rooms, t.court_room).map(|r| r.faction);
                    let available = queues
                        .iter()
                        .find(|q| Some(q.0) == faction)
                        .is_some_and(|q| !q.1.is_empty());
                    if !available {
                        tracing::debug!(
                            "TrialStep.AwaitingJurySummons skipping jury wait, nobody in queue - {}",
                            t.id
                        );
                        t.current_step_end_ms = now;
                    }
                }
                // Wait for the jury.
                if t.step == step::AWAITING_JURY_SUMMONS && t.current_step_end_ms <= now {
                    tracing::debug!("TrialStep.ConfirmCriminalRecord - {}", t.id);
                    t.step = step::CONFIRM_CRIMINAL_RECORD;
                    t.current_step_end_ms = now + CONFIRM_CRIMINAL_RECORD_MINUTES * MINUTE_MS;
                    t.send_packets(&SCChatMessagePacket {
                        chat_type: chat_type::JUDGE,
                        sender: None,
                        message: format!("Begin the trial of {}", t.defendant_name),
                        ability: 0,
                        language_type: 0,
                    });
                    t.send_state(now);
                }
                // Wait for "give verdict" by the jury.
                if t.step == step::CONFIRM_CRIMINAL_RECORD && t.current_step_end_ms <= now {
                    tracing::debug!("TrialStep.ClosingStatement - {}", t.id);
                    t.step = step::CLOSING_STATEMENT;
                    t.current_step_end_ms = now + CLOSING_STATEMENT_MINUTES * MINUTE_MS;
                    t.send_state(now);
                }
                // Wait for the closing statement of the defendant.
                if t.step == step::CLOSING_STATEMENT && t.current_step_end_ms <= now {
                    tracing::debug!("TrialStep.JuryVerdict - {}", t.id);
                    t.step = step::JURY_VERDICT;
                    t.current_step_end_ms = now + JURY_VERDICT_MINUTES * MINUTE_MS;
                    t.send_state(now);
                    // Update the jury verdict panel to default.
                    t.defendant.send(SCRulingStatusPacket {
                        count: 0,
                        total: t.active_jury_count(),
                        sentence_type: 0,
                        sentence_time: 0,
                    });
                    // Every seated juror is still expected to vote: count 0 of `total`.
                    t.send_verdict_count();
                }
                if t.step == step::JURY_VERDICT && t.current_step_end_ms <= now {
                    tracing::debug!("TrialStep.CalculateSentence - {}", t.id);
                    finalize_verdict(server, rooms, t, now, fx);
                }
                if t.step == step::END_TRIAL && t.current_step_end_ms <= now {
                    t.send_packets(&SCRulingClosedPacket {});
                    tracing::debug!("TrialStep.Cleanup - {}", t.id);
                    t.step = step::CLEANUP;
                    t.current_step_end_ms = TIME_MAX;
                }
                if t.step == step::PLEAD_GUILTY {
                    t.send_packets(&SCRulingClosedPacket {});
                    tracing::debug!("TrialStep.Cleanup from PleadGuilty - {}", t.id);
                    t.step = step::CLEANUP;
                    t.current_step_end_ms = TIME_MAX;
                }
                if t.step == step::TRIAL_CANCELLED {
                    // The defendant and the seated jury hear the cancellation before the
                    // ruling closes. `TRIAL_CANCELED` is registered by `usertrial/trial_status.alb`;
                    // the C# sent no such packet and the audience is INFERRED from `SendPackets`.
                    t.send_packets(&SCTrialCanceledPacket { trial_id: t.id });
                    let d = t.defendant.clone();
                    result_is_guilty(server, rooms, &d, t, false, now, fx);
                    t.send_packets(&SCRulingClosedPacket {});
                    tracing::debug!("TrialStep.TrialCancelled - {}", t.id);
                    t.step = step::CLEANUP;
                    t.current_step_end_ms = TIME_MAX;
                }
                // Delete the trial when done.
                if t.step == step::CLEANUP {
                    t.step = step::INVALID;
                    cleanup_trial(server, rooms, t, true, fx);
                    tracing::debug!("TrialStep.Cleanup - {}", t.id);
                    remove = true;
                }
            }
            if remove && trials.remove(&id).is_none() {
                tracing::warn!("Failed to remove Trial {id}");
            }
        }
    }

    /// `UpdateTick()` (the `TrialUpdateTask`).
    pub async fn update_tick(&self, server: &GameServer, now_ms: i64) {
        let mut fx = Vec::new();
        {
            let mut st = self.st();
            self.update_jury_queue(server, &mut st);
            self.update_trial_states(server, &mut st, now_ms, &mut fx);
        }
        run_effects(server, fx);
    }

    /// `UpdateJuryQueue()` on its own.
    pub fn update_jury_queues(&self, server: &GameServer) {
        let mut st = self.st();
        self.update_jury_queue(server, &mut st);
    }

    /// `GetJuryQueueForPlayer(player)`: sends `SCJuryWaitingNumberPacket` when queued.
    pub fn get_jury_queue_for_player(&self, server: &GameServer, player: &PlayerRef) {
        let mother = mother_of(server, player);
        let st = self.st();
        let Some((_, q)) = st.queues.iter().find(|q| q.0 == mother) else {
            return;
        };
        if let Some(pos) = q.iter().position(|x| *x == player.character_id) {
            player.send(SCJuryWaitingNumberPacket {
                waiting_number: pos as i32 + 1,
            });
        }
    }

    /// `RemovePlayerFromQueue(player)`.
    pub fn remove_player_from_queue(&self, player_id: u32) {
        remove_from_queues(&mut self.st().queues, player_id);
    }

    /// `GenerateJuryList(trial, maxCount)`.
    pub fn generate_jury_list(
        &self,
        server: &GameServer,
        trial_id: u32,
        max_count: usize,
    ) -> Vec<PlayerRef> {
        let st = self.st();
        match st.trials.get(&trial_id) {
            Some(t) => generate_jury_list(server, &st.rooms, &st.queues, t, max_count),
            None => Vec::new(),
        }
    }

    /// `ProcessTrialInviteReply(player, accept, trialId)`.
    /// GM `/set_jury` (retail command, WP-SL6b): seats `player` in the first free jury seat
    /// of `trial_id` without an invitation or queue entry. The structural checks of
    /// `ProcessTrialInviteReply` still apply (step, main world, no other trial); the
    /// invitation / queue / eligibility checks are deliberately skipped for the GM command.
    pub fn force_summon_jury(
        &self,
        server: &GameServer,
        trial_id: u32,
        player: &PlayerRef,
        now_ms: i64,
    ) -> Result<(), ForceJuryError> {
        let mut fx = Vec::new();
        let res = {
            let mut st = self.st();
            let State {
                rooms,
                trials,
                queues,
                ..
            } = &mut *st;
            let Some(t) = trials.get(&trial_id) else {
                return Err(ForceJuryError::NoTrial);
            };
            if t.step != trial_step::AWAITING_JURY_SUMMONS {
                return Err(ForceJuryError::NotSummoning);
            }
            if player.data().unit.transform.instance_id != 0 {
                return Err(ForceJuryError::NotInMainWorld);
            }
            if trials.values().any(|t| {
                t.jury.values().any(|j| {
                    j.jury_member
                        .as_ref()
                        .is_some_and(|m| Arc::ptr_eq(m, player))
                })
            }) {
                return Err(ForceJuryError::AlreadyInTrial);
            }
            let Some(t) = trials.get_mut(&trial_id) else {
                return Err(ForceJuryError::NoTrial);
            };
            if summon_jury_member(server, rooms, queues, t, player, now_ms, &mut fx) {
                Ok(())
            } else {
                Err(ForceJuryError::NoSeat)
            }
        };
        run_effects(server, fx);
        res
    }

    pub fn process_trial_invite_reply(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        accept: bool,
        trial_id: u32,
        now_ms: i64,
    ) -> bool {
        let mother = mother_of(server, player);
        let mut fx = Vec::new();
        let res = {
            let mut st = self.st();
            let State {
                rooms,
                trials,
                queues,
                ..
            } = &mut *st;
            self.invite_reply_inner(
                server, rooms, trials, queues, player, mother, accept, trial_id, now_ms, &mut fx,
            )
        };
        run_effects(server, fx);
        res
    }

    #[allow(clippy::too_many_arguments)]
    fn invite_reply_inner(
        &self,
        server: &GameServer,
        rooms: &mut [CourtRoom],
        trials: &mut BTreeMap<u32, TrialData>,
        queues: &mut [(u32, Vec<u32>)],
        player: &PlayerRef,
        mother: u32,
        accept: bool,
        trial_id: u32,
        now_ms: i64,
        fx: &mut Effects,
    ) -> bool {
        let name = player.data().unit.name.clone();
        if !trials.contains_key(&trial_id) {
            // Replied too late, don't remove from the queue.
            tracing::debug!(
                "{name} sent a jury invite reply for a non-existing trial ({trial_id}). Possibly already concluded."
            );
            send_error(player, error::TRIALS_ALREADY_CLOSED);
            return false;
        }
        let Some(qi) = queues.iter().position(|q| q.0 == mother) else {
            send_error(player, error::TRIALS_JURY_FULL);
            sus_log(
                server,
                crate::systems::services::sus::CATEGORY_CHEATING,
                player.character_id,
                "Player is trying to join as jury while not in an eligible faction",
            );
            return false;
        };
        if !queues[qi].1.contains(&player.character_id) {
            send_error(player, error::TRIALS_JURY_FULL);
            sus_log(
                server,
                crate::systems::services::sus::CATEGORY_CHEATING,
                player.character_id,
                "Player is trying to join as jury while not in an the jury queue",
            );
            return false;
        }
        if !accept {
            // Remove the current queue position if manually declined.
            let q = &mut queues[qi].1;
            if let Some(i) = q.iter().position(|x| *x == player.character_id) {
                q.remove(i);
            }
            return false;
        }
        let Some(t) = trials.get(&trial_id) else {
            return false;
        };
        if t.step != step::AWAITING_JURY_SUMMONS {
            send_error(player, error::TRIALS_CANNOT_JOIN_AFTER_START);
            return false;
        }
        if !t.invited_jury.contains(&player.character_id)
            || room_by_id(rooms, t.court_room).is_none_or(|r| r.faction != mother)
            || !can_accept_trial_invites(server, rooms, trials, Some(player))
        {
            send_error(player, error::TRIALS_JURY_FULL);
            return false;
        }
        // Verify we are not already part of another trial.
        if trials.values().any(|t| {
            t.jury.values().any(|j| {
                j.jury_member
                    .as_ref()
                    .is_some_and(|m| Arc::ptr_eq(m, player))
            })
        }) {
            send_error(player, error::TRIALS_JURY_FULL);
            return false;
        }
        if player.data().unit.transform.instance_id != 0 {
            // Only allow joining from within the main world.
            send_error(player, error::CANNOT_JOIN_TRIAL_FROM_INSTANT_ZONE);
            return false;
        }
        let Some(t) = trials.get_mut(&trial_id) else {
            return false;
        };
        if t.step > step::AWAITING_JURY_SUMMONS {
            send_error(player, error::TRIALS_CANNOT_JOIN_AFTER_START);
            return false;
        }
        let res = summon_jury_member(server, rooms, queues, t, player, now_ms, fx);
        if !res {
            send_error(player, error::TRIALS_JURY_FULL);
        }
        res
    }

    /// Restore justice state before considering jury eligibility.
    pub fn handle_player_login(&self, server: &GameServer, player: &PlayerRef) {
        let mother = mother_of(server, player);
        let now = util::now_ms();
        let mut fx = Vec::new();
        {
            let mut st = self.st();
            let State {
                rooms,
                trials,
                queues,
                ..
            } = &mut *st;
            if trials
                .values()
                .any(|t| t.defendant_id == player.character_id)
            {
                return;
            }
            let (offline_guilty_time, region, prisoner_region) = {
                let c = player.data();
                let prisoner = if c.unit.buffs.check_buff(buff::PRISONER_NUIAN) {
                    Some(court_room_region::NUIAN)
                } else if c.unit.buffs.check_buff(buff::PRISONER_HARANYAN) {
                    Some(court_room_region::HARANYAN)
                } else {
                    None
                };
                (
                    c.offline_guilty_time,
                    c.offline_guilty_region as u8,
                    prisoner,
                )
            };
            if let Some(prisoner_region) = prisoner_region {
                if let Some(room) = rooms.iter().find(|r| r.region == prisoner_region) {
                    if let Some(jail) = server
                        .config
                        .justice
                        .jails
                        .iter()
                        .find(|j| j.faction == room.faction)
                    {
                        fx.push(Effect::ForceDismount(player.clone()));
                        teleport(server, player, jail.pos, teleport_reason::JAIL, &mut fx);
                    }
                }
            } else if offline_guilty_time > 0 {
                if let Some(room) = rooms.iter().find(|r| {
                    r.region == region
                        && server
                            .config
                            .justice
                            .jails
                            .iter()
                            .any(|j| j.faction == r.faction)
                }) {
                    if let Some(tid) = create_trial_case(server, trials, player, region) {
                        if let Some(t) = trials.get_mut(&tid) {
                            t.court_room = Some(room.id);
                            t.jail_time = offline_guilty_time.clamp(0, MAX_JAIL_MINUTES);
                            result_is_guilty(server, rooms, player, t, false, now, &mut fx);
                        }
                    }
                } else {
                    tracing::error!(
                        "No court/jail for pending sentence of {} in region {region}",
                        player.character_id
                    );
                }
            } else if can_accept_trial_invites(server, rooms, trials, Some(player)) {
                if let Some((_, q)) = queues.iter_mut().find(|q| q.0 == mother) {
                    if !q.contains(&player.character_id) {
                        q.push(player.character_id);
                    }
                }
            }
        }
        run_effects(server, fx);
    }

    /// Close an abandoned defendant's case, or vacate a disconnected juror's seat before save.
    pub fn handle_player_logout(&self, server: &GameServer, player: &PlayerRef) {
        let mut fx = Vec::new();
        {
            let mut st = self.st();
            let State {
                rooms,
                trials,
                queues,
                ..
            } = &mut *st;
            remove_from_queues(queues, player.character_id);
            let defendant_trial = trials
                .values()
                .find(|t| Arc::ptr_eq(&t.defendant, player))
                .map(|t| t.id);
            if let Some(id) = defendant_trial {
                if let Some(t) = trials.remove(&id) {
                    t.send_packets(&SCRulingClosedPacket {});
                    cleanup_trial(server, rooms, &t, false, &mut fx);
                }
            }
            for t in trials.values_mut() {
                let seat_id = t
                    .jury
                    .iter()
                    .find(|(_, j)| {
                        j.jury_member
                            .as_ref()
                            .is_some_and(|m| Arc::ptr_eq(m, player))
                    })
                    .map(|(&id, _)| id);
                if let Some(seat_id) = seat_id {
                    cleanup_jury_member(server, rooms, t, player, false, &mut fx);
                    if let Some(j) = t.jury.get_mut(&seat_id) {
                        j.jury_member = None;
                        j.confirm_testimony = false;
                        j.selected_sentence = -1;
                    }
                    t.send_state(util::now_ms());
                }
            }
            for room in rooms {
                room.audience_members.retain(|m| !Arc::ptr_eq(m, player));
                room.leave_chat(player);
            }
        }
        run_effects(server, fx);
    }

    /// `ArrestCriminal(criminal, arrestor)`: returns the new trial id.
    pub fn arrest_criminal(
        &self,
        server: &GameServer,
        criminal: &PlayerRef,
        arrestor: Option<&PlayerRef>,
    ) -> Option<u32> {
        let mut fx = Vec::new();
        let res = {
            let mut st = self.st();
            let State { rooms, trials, .. } = &mut *st;
            self.arrest_inner(server, rooms, trials, criminal, arrestor, &mut fx)
        };
        run_effects(server, fx);
        res
    }

    fn arrest_inner(
        &self,
        server: &GameServer,
        rooms: &[CourtRoom],
        trials: &mut BTreeMap<u32, TrialData>,
        criminal: &PlayerRef,
        arrestor: Option<&PlayerRef>,
        fx: &mut Effects,
    ) -> Option<u32> {
        if let Some(t) = trials
            .values()
            .find(|t| t.defendant_id == criminal.character_id)
        {
            return Some(t.id);
        }
        let mut criminal_region = court_room_region_by_faction(mother_of(server, criminal));
        let arrestor_region = match arrestor {
            Some(a) => court_room_region_by_faction(mother_of(server, a)),
            None => criminal.data().offline_guilty_region as u8,
        };
        let criminal_name = criminal.data().unit.name.clone();
        if let Some(a) = arrestor {
            if criminal_region == court_room_region::INVALID
                && arrestor_region == court_room_region::INVALID
            {
                let an = a.data().unit.name.clone();
                tracing::debug!(
                    "ArrestCriminal: {an} cannot arrest {criminal_name}, both fall outside of justice system"
                );
                return None;
            }
        }
        if criminal_region == court_room_region::INVALID {
            criminal_region = arrestor_region;
        }
        let Some(room) = rooms.iter().find(|r| r.region == criminal_region) else {
            tracing::warn!("Failed to find a court room for {criminal_name}");
            return None;
        };
        if !server
            .config
            .justice
            .jails
            .iter()
            .any(|j| j.faction == room.faction)
        {
            tracing::warn!("No jail configured for court {}", room.id);
            return None;
        }
        if arrestor.is_some() {
            criminal.data().arrest_count += 1;
        }
        let Some(tid) = create_trial_case(server, trials, criminal, room.region) else {
            tracing::warn!("Failed to create a court case for {criminal_name}");
            return None;
        };
        if let Some(a) = arrestor {
            let an = a.data().unit.name.clone();
            let inst = criminal.data().unit.transform.instance_id;
            if let Some(w) = server.world.world(inst) {
                broadcast(
                    &w,
                    criminal,
                    &SCCriminalArrestedPacket {
                        criminal_obj_id: criminal.obj_id,
                        criminal: criminal_name,
                        arrestor: an,
                    },
                    true,
                );
            }
        }
        let t = trials.get_mut(&tid)?;
        enter_court_jail(server, t, room, fx);
        Some(tid)
    }

    /// `AddNewEvidence(newEvent)`.
    pub fn add_new_evidence(&self, server: &GameServer, e: &CrimeEvent) {
        let mut st = self.st();
        let Some(t) = st
            .trials
            .values_mut()
            .find(|t| t.defendant_id == e.criminal && t.step < step::TRIAL_CANCELLED)
        else {
            return;
        };
        t.evidence.push(e.clone());
        calculate_jail_time(t);
        let entry = server.systems.social.crimes.record_entry(server, e);
        t.send_packets(&SCCrimeRecordsPacket {
            trial_id: t.id,
            unknown1: 0,
            total_count: t.evidence.len() as i32,
            this_count: 1,
            entries: vec![entry],
        });
    }

    /// `ReplyImprisonOrTrial(defendant, requestTrial)`.
    pub fn reply_imprison_or_trial(
        &self,
        server: &GameServer,
        defendant: &PlayerRef,
        request_trial: bool,
        now_ms: i64,
    ) {
        let mut fx = Vec::new();
        {
            let mut st = self.st();
            let State { rooms, trials, .. } = &mut *st;
            let Some(t) = trials
                .values_mut()
                .find(|t| t.defendant_id == defendant.character_id)
            else {
                let name = defendant.data().unit.name.clone();
                tracing::warn!(
                    "{name} ({}) sent a CSReplyImprisonOrTrialPacket without being on trial. requestTrial={request_trial}",
                    defendant.character_id
                );
                return;
            };
            if !Arc::ptr_eq(&t.defendant, defendant) {
                return;
            }
            if t.step != step::DEFENDANT_AWAITING_TRIAL {
                return;
            }
            if !request_trial {
                // Directly to jail with trial.JailTime.
                result_is_guilty(server, rooms, defendant, t, true, now_ms, &mut fx);
            } else if t.current_step_end_ms == TIME_MAX {
                // End the waiting time now.
                defendant.data().accept_trial_count += 1;
                t.current_step_end_ms = now_ms;
            }
        }
        run_effects(server, fx);
    }

    /// `CSCancelTrialPacket`: `ResultIsGuilty(activeChar, trial, true)` for the defendant (a
    /// missing trial is the C# `NullReferenceException`: logged).
    pub fn cancel_trial(&self, server: &GameServer, p: &PlayerRef, trial_id: u32, now_ms: i64) {
        let mut fx = Vec::new();
        {
            let mut st = self.st();
            let State { rooms, trials, .. } = &mut *st;
            let Some(t) = trials.get_mut(&trial_id) else {
                tracing::error!(
                    "CSCancelTrialPacket: NullReferenceException (trial {trial_id} not found)"
                );
                return;
            };
            if Arc::ptr_eq(&t.defendant, p) {
                if t.step < step::TRIAL_CANCELLED {
                    // INFERRED audience (see the auto-cancel path): the defendant and the
                    // seated jury, before the later `SCRulingClosedPacket`.
                    t.send_packets(&SCTrialCanceledPacket { trial_id: t.id });
                    result_is_guilty(server, rooms, p, t, true, now_ms, &mut fx);
                }
            } else {
                let name = p.data().unit.name.clone();
                sus_log(
                    server,
                    crate::systems::services::sus::CATEGORY_CHEATING,
                    p.character_id,
                    &format!("Player {name} tried to cancel a trial they do not belong to"),
                );
            }
        }
        run_effects(server, fx);
    }

    /// `ResultIsGuilty(defendant, trial, pleadGuilty)` for a trial id.
    pub fn result_is_guilty(
        &self,
        server: &GameServer,
        defendant: &PlayerRef,
        trial_id: u32,
        plead_guilty: bool,
        now_ms: i64,
    ) {
        let mut fx = Vec::new();
        {
            let mut st = self.st();
            let State { rooms, trials, .. } = &mut *st;
            match trials.get_mut(&trial_id) {
                Some(t) if Arc::ptr_eq(&t.defendant, defendant) => {
                    result_is_guilty(server, rooms, defendant, t, plead_guilty, now_ms, &mut fx)
                }
                _ => tracing::warn!("No matching trial for {}", defendant.character_id),
            }
        }
        run_effects(server, fx);
    }

    /// Confirm records only for the caller's occupied jury seat.
    pub fn jury_end_testimony(
        &self,
        jury_member: &PlayerRef,
        trial_id: u32,
        jury_id: i32,
        now_ms: i64,
    ) {
        let mut st = self.st();
        let Some(t) = st.trials.get_mut(&trial_id) else {
            return;
        };
        let name = jury_member.data().unit.name.clone();
        if t.step != step::CONFIRM_CRIMINAL_RECORD {
            tracing::warn!(
                "JuryEndTestimony by {name} for trial {} with seatId {jury_id} was confirmed outside of the correct trial step",
                t.id
            );
            return;
        }
        match t.jury.get_mut(&jury_id) {
            Some(j)
                if j.seat_id == jury_id
                    && j.jury_member
                        .as_ref()
                        .is_some_and(|m| Arc::ptr_eq(m, jury_member))
                    && !j.confirm_testimony =>
            {
                j.confirm_testimony = true
            }
            _ => {
                tracing::warn!(
                    "JuryEndTestimony by {name} seems to be invalid for trial {} with seatId {jury_id}",
                    t.id
                );
                return;
            }
        }
        let total = t.jury.values().filter(|j| j.jury_member.is_some()).count() as i32;
        let count = t
            .jury
            .values()
            .filter(|j| j.jury_member.is_some() && j.confirm_testimony)
            .count() as i32;
        t.send_packets(&SCChangeJuryOkCountPacket { count, total });
        if t.all_jury_confirmed_criminal_records() {
            t.current_step_end_ms = now_ms;
        }
    }

    /// `JuryVerdict(juryMember, trial, juryId, sentence)`.
    pub fn jury_verdict(
        &self,
        jury_member: &PlayerRef,
        trial_id: u32,
        jury_id: i32,
        sentence: u8,
        now_ms: i64,
    ) {
        let mut st = self.st();
        let Some(t) = st.trials.get_mut(&trial_id) else {
            return;
        };
        let name = jury_member.data().unit.name.clone();
        if !(sentence_result::NOT_GUILTY..=sentence_result::GUILTY5).contains(&sentence) {
            tracing::warn!("JuryVerdict by {name}: invalid sentence {sentence}");
            return;
        }
        if t.step != step::JURY_VERDICT {
            tracing::warn!(
                "JuryVerdict by {name} for trial {} with seatId {jury_id} was confirmed outside of the correct trial step",
                t.id
            );
            return;
        }
        let Some(j) = t.jury.get_mut(&jury_id).filter(|j| j.seat_id == jury_id) else {
            tracing::warn!(
                "JuryVerdict by {name} seems to be invalid for trial {} with seatId {jury_id}",
                t.id
            );
            return;
        };
        match &j.jury_member {
            None => {
                tracing::error!("JuryVerdict: NullReferenceException (seat {jury_id} is empty)");
                return;
            }
            Some(m) if !Arc::ptr_eq(m, jury_member) => {
                tracing::warn!("Jury Verdict Seat does not match the expected player");
                return;
            }
            Some(_) => {}
        }
        if j.selected_sentence >= 0 {
            return;
        }
        j.selected_sentence = i32::from(sentence);
        let total = t.jury.values().filter(|j| j.jury_member.is_some()).count() as i32;
        let count = t
            .jury
            .values()
            .filter(|j| j.jury_member.is_some() && j.selected_sentence >= 0)
            .count() as i32;
        let pkt = SCRulingStatusPacket {
            count,
            total,
            sentence_type: 0,
            sentence_time: 0,
        };
        t.defendant.send(pkt.clone());
        for j in t.jury.values() {
            if j.selected_sentence >= 0 {
                if let Some(m) = &j.jury_member {
                    m.send(pkt.clone());
                }
            }
        }
        // Tell the jurors who still owe a vote the new `votes cast / seated` count.
        t.send_verdict_count();
        if t.all_jury_selected_sentence() {
            t.current_step_end_ms = now_ms;
        }
    }

    /// `CSJurySummonedPacket` acknowledgement: true only for the player holding jury seat
    /// `jury_id` of the trial. The packet changes no state.
    pub fn jury_summoned(&self, player: &PlayerRef, trial_id: u32, jury_id: i32) -> bool {
        let st = self.st();
        let Some(t) = st.trials.get(&trial_id) else {
            return false;
        };
        t.jury.get(&jury_id).is_some_and(|j| {
            j.seat_id == jury_id
                && j.jury_member
                    .as_ref()
                    .is_some_and(|m| Arc::ptr_eq(m, player))
        })
    }

    /// `SkipFinalStatementReply(defendant, trial)`.
    pub fn skip_final_statement_reply(&self, defendant: &PlayerRef, trial_id: u32, now_ms: i64) {
        let mut st = self.st();
        let Some(t) = st.trials.get_mut(&trial_id) else {
            return;
        };
        if !Arc::ptr_eq(&t.defendant, defendant) {
            return;
        }
        if t.step != step::CLOSING_STATEMENT {
            return;
        }
        t.current_step_end_ms = now_ms;
    }

    /// `IsPlayerInCourt(playerId)`.
    pub fn is_player_in_court(&self, player_id: u32) -> bool {
        self.st().trials.values().any(|t| {
            t.defendant_id == player_id
                || t.jury.values().any(|j| {
                    j.jury_member
                        .as_ref()
                        .is_some_and(|m| m.character_id == player_id)
                })
        })
    }

    /// `GetParticipatingTrial(player)` (trial id).
    pub fn get_participating_trial(&self, player: &PlayerRef) -> Option<u32> {
        let st = self.st();
        participating_trial(&st.rooms, &st.trials, player)
    }

    /// `JoinTrialAudience(player, doodadTemplateId)`.
    pub fn join_trial_audience(
        &self,
        server: &GameServer,
        player: &PlayerRef,
        doodad_template_id: u32,
    ) {
        let mut st = self.st();
        for room in st.rooms.iter_mut() {
            if room.audience_members.iter().any(|a| Arc::ptr_eq(a, player)) {
                return; // Already in this audience
            }
            let mut found = false;
            for seat in &room.audience_seats {
                if seat.data().template_id != doodad_template_id {
                    continue;
                }
                // player.GetDistanceTo(seat) (2D, minus the player's model size)
                use crate::systems::skills::geometry::{distance_to, Target};
                let dist = distance_to(
                    server,
                    &Target::Obj(WorldObject::from(player)),
                    &Target::Obj(WorldObject::from(seat)),
                    false,
                );
                // `!(dist <= 5f)` (NaN never matches).
                if dist.is_nan() || dist > 5.0 {
                    continue;
                }
                found = true;
                break;
            }
            if found {
                room.audience_members.push(player.clone());
                let (name, inst) = {
                    let c = player.data();
                    (c.unit.name.clone(), c.unit.transform.instance_id)
                };
                if let Some(w) = server.world.world(inst) {
                    broadcast(
                        &w,
                        player,
                        &SCTrialAudienceJoinedPacket {
                            trial_id: room.current_trial.unwrap_or(0),
                            bc: player.obj_id,
                            audience_name: name,
                        },
                        true,
                    );
                }
                room.join_chat(player);
                return;
            }
        }
        let name = player.data().unit.name.clone();
        let msg = format!("{name} tried to join courtroom audience while not being near a seat");
        tracing::warn!("{msg}");
        sus_log(
            server,
            crate::systems::services::sus::CATEGORY_CHEATING,
            player.character_id,
            &msg,
        );
    }

    /// `LeaveTrialAudience(player)`.
    pub fn leave_trial_audience(&self, server: &GameServer, player: &PlayerRef) {
        let mut st = self.st();
        for room in st.rooms.iter_mut() {
            if let Some(i) = room
                .audience_members
                .iter()
                .position(|a| Arc::ptr_eq(a, player))
            {
                room.audience_members.remove(i);
                let (name, inst) = {
                    let c = player.data();
                    (c.unit.name.clone(), c.unit.transform.instance_id)
                };
                if let Some(w) = server.world.world(inst) {
                    broadcast(
                        &w,
                        player,
                        &SCTrialAudienceLeftPacket {
                            bc: player.obj_id,
                            audience_name: name,
                        },
                        true,
                    );
                }
                room.leave_chat(player);
                return;
            }
        }
        let name = player.data().unit.name.clone();
        tracing::debug!("{name} tried to leave the courtroom audience but was not part of it");
    }
}

/// `TrialManager.HandlePlayerLogin(player)`.
pub fn handle_player_login(server: &GameServer, p: &PlayerRef) {
    server.systems.social.trials.handle_player_login(server, p);
}

/// `ChatManager.GetTrialChat(character)`. A player without a home court that takes part in no
/// trial is the C# `NullReferenceException` (logged, `None`).
pub fn trial_chat(server: &GameServer, p: &PlayerRef) -> Option<ChannelRef> {
    let region = court_room_region_by_faction(mother_of(server, p));
    if region == court_room_region::INVALID {
        let trials = &server.systems.social.trials;
        let st = trials.st();
        return match participating_trial(&st.rooms, &st.trials, p) {
            Some(tid) => {
                let r = st.trials.get(&tid).map(|t| t.court_region)?;
                server.chat.court_chat(r)
            }
            None => {
                tracing::error!(
                    "GetTrialChat: NullReferenceException (character {} has no court and no trial)",
                    p.character_id
                );
                None
            }
        };
    }
    server.chat.court_chat(region)
}
