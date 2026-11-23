//! Territory authority independent of packet/world objects. Callers validate the original
//! skill, current character/expedition membership, live tower, range and uninterrupted cast.
//! The authority rechecks its own participant, deadline and ownership generation at completion.
//!
//! Lock order for `*_with` callbacks is authority -> economy ItemTxn. The callback must not
//! call back into this manager, and ItemTxn holders must not acquire the authority lock.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

mod economy;
mod siege;

/// Original 1.2 engraving skill 20866 channels for five minutes. Milliseconds are used for
/// channel completion; every persisted timestamp is Unix seconds.
pub const ENGRAVING_MIN_DURATION_MS: i64 = 5 * 60 * 1000;
/// Native 1.2 DominionUnkData has a fixed array of 100 character IDs per siege side.
pub const MAX_PARTICIPANTS_PER_SIDE: usize = 100;
/// Original 1.2 TAXRATE_CHANGE_LATER limits territory rate changes to once per hour.
pub const TAX_RATE_CHANGE_COOLDOWN_SECONDS: i64 = 60 * 60;
/// The 1.2 client's national tax confirm validator (`FUN_392f4820`) allows one change per
/// 86400 s (the wire rate is permille/10, 0-1000).
pub const NATIONAL_TAX_RATE_CHANGE_COOLDOWN_SECONDS: i64 = 86_400;
/// The largest national tax rate the 1.2 client sends (`FUN_392f4820`: `1000 < rate` is
/// `dominion_national_tax_invalid_rate`).
pub const MAX_NATIONAL_TAX_RATE: u16 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TowerIdentity {
    pub house_id: u32,
    pub instance_id: u32,
    pub guard_tower_setting_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Treasury {
    pub cur_house_tax_money: i32,
    pub cur_hunt_tax_money: i32,
    pub peace_tax_money: i32,
    pub cur_house_tax_aa_point: i32,
    pub peace_tax_aa_point: i32,
}

impl Treasury {
    pub fn is_valid(self) -> bool {
        self.cur_house_tax_money >= 0
            && self.cur_hunt_tax_money >= 0
            && self.peace_tax_money >= 0
            && self.cur_house_tax_aa_point >= 0
            && self.peace_tax_aa_point >= 0
    }

    fn checked_add(self, credit: Self) -> Option<Self> {
        if !self.is_valid() || !credit.is_valid() {
            return None;
        }
        Some(Self {
            cur_house_tax_money: self
                .cur_house_tax_money
                .checked_add(credit.cur_house_tax_money)?,
            cur_hunt_tax_money: self
                .cur_hunt_tax_money
                .checked_add(credit.cur_hunt_tax_money)?,
            peace_tax_money: self.peace_tax_money.checked_add(credit.peace_tax_money)?,
            cur_house_tax_aa_point: self
                .cur_house_tax_aa_point
                .checked_add(credit.cur_house_tax_aa_point)?,
            peace_tax_aa_point: self
                .peace_tax_aa_point
                .checked_add(credit.peace_tax_aa_point)?,
        })
    }
}

/// Database state, not the native client's phase enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SiegeStatus {
    Scheduled = 0,
    Active = 1,
    Finished = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SiegeOutcome {
    Pending = 0,
    DefenderHeld = 1,
    AttackerCaptured = 2,
    Cancelled = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SiegeSide {
    Attacker = 1,
    Defender = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiegeParticipant {
    pub character_id: u32,
    pub expedition_id: u32,
    pub side: SiegeSide,
}

/// Roster size checked by [`DominionManager::register_participant_with_cap`].
///
/// The data-driven caps come from `siege_settings`; the authority itself never reads game
/// data, so every caller supplies the cap under the same lock as the insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParticipantCap {
    /// Highest roster size the side may reach right now.
    pub limit: usize,
    /// The defender's authored reinforcement wave has not arrived yet, so a full roster is
    /// `SIEGE_BLOCK_DEFENSE_REINFORCE` rather than `SIEGE_PARTICIPANT_FULL`.
    pub awaiting_reinforcements: bool,
}

impl ParticipantCap {
    /// The native 100-character array, used when no `siege_settings` row applies.
    pub const NATIVE: Self = Self {
        limit: MAX_PARTICIPANTS_PER_SIDE,
        awaiting_reinforcements: false,
    };
}

impl Default for ParticipantCap {
    fn default() -> Self {
        Self::NATIVE
    }
}

/// Roster limits frozen into a siege record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiegeLimits {
    /// Defender cap before the reinforcement wave (`ni`).
    pub ni: u8,
    /// Defender cap after the wave and the attacker cap (`nr`).
    pub nr: u8,
}

impl SiegeLimits {
    /// The native 100-character array, used when no `siege_settings` row applies.
    pub const NATIVE: Self = Self {
        ni: MAX_PARTICIPANTS_PER_SIDE as u8,
        nr: MAX_PARTICIPANTS_PER_SIDE as u8,
    };

    pub fn pending(self) -> bool {
        self.nr > self.ni
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiegeState {
    pub siege_id: u64,
    pub attacker_expedition_id: u32,
    pub defender_expedition_id: u32,
    pub ownership_generation: u64,
    pub status: SiegeStatus,
    pub outcome: SiegeOutcome,
    pub scheduled_at: i64,
    pub starts_at: i64,
    pub ends_at: i64,
    pub ended_at: i64,
    /// The defender's reinforcement wave (at `starts_at + bdm`) has been announced. Persisted
    /// so a restart does not normally repeat `SCSiegeReinforce` (a crash between the broadcast
    /// and the periodic save can still repeat it; the client just sets `limit = nr` again).
    pub reinforced: bool,
    /// Roster limits frozen when the siege was scheduled. `ni` is the defender cap before the
    /// reinforcement wave, `nr` after it (the attacker uses `nr`); a change in the defender's
    /// held-castle count mid-siege does not move them.
    pub defender_ni: u8,
    pub defender_nr: u8,
    /// The `char_records` kinds 57-60 credit for this siege's outcome has been applied. Only a
    /// finished siege with a non-`Pending` outcome is ever marked; `Cancelled` credits nothing.
    /// Persisted so a repeated tick, a capture racing the tick or a restart cannot credit one
    /// siege id twice.
    pub outcome_credited: bool,
    /// The record was created from the calendar declaration at the cycle's warmup (WP-SD2a),
    /// not by a GM `/dominion schedule`. The calendar then keeps describing its periods (the
    /// GM test clock can walk 3 -> 4 -> 5), while a GM schedule keeps owning its own timers.
    /// Runtime-only: after a restart the persisted real deadlines describe the same cycle.
    pub materialised_from_declaration: bool,
    /// `Cycle::auction_start` of the declared cycle a calendar materialisation belongs to
    /// (0 = GM schedule). Runtime-only, like `materialised_from_declaration`.
    pub calendar_cycle: i64,
    pub participants: BTreeMap<u32, SiegeParticipant>,
}

impl SiegeState {
    pub fn active_at(&self, now_seconds: i64) -> bool {
        self.status != SiegeStatus::Finished
            && self.starts_at <= now_seconds
            && now_seconds < self.ends_at
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Declaration {
    pub attacker_expedition_id: u32,
    /// When the camp use was accepted (Unix seconds).
    pub declared_at: i64,
    /// `calendar::Cycle::auction_start` of the cycle this declaration belongs to; a
    /// declaration only describes that one cycle.
    pub cycle_auction_start: i64,
    /// The spawned camp doodad; runtime-only, re-bound after a restart (WP-SD2c).
    pub hq_obj_id: u32,
    pub hq_x: f32,
    pub hq_y: f32,
    pub hq_z: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DominionState {
    pub zone_group_id: u32,
    pub tower: TowerIdentity,
    pub owner_expedition_id: u32,
    pub ownership_generation: u64,
    pub reign_start_at: i64,
    /// When the current owner claimed the *unowned* territory (`declare_unowned_with_limit`).
    /// A siege capture sets `reign_start_at` but not this, so only a claim starts the
    /// `var_delay_siege_x_days_after_dominion_declare` protection. 0 = not a direct claim.
    pub claimed_at: i64,
    pub last_siege_end_at: i64,
    pub tax_rate: i32,
    pub national_tax_rate: u16,
    pub last_paid_at: i64,
    pub last_tax_rate_changed_at: i64,
    pub last_national_tax_rate_changed_at: i64,
    /// Persistent doodad row id of the nation monument (0 = none), and its position.
    pub national_monument_db_id: i64,
    pub national_monument_x: f32,
    pub national_monument_y: f32,
    pub national_monument_z: f32,
    /// Start of the current peace declaration (Unix seconds; 0 = none).
    pub non_pvp_start_at: i64,
    /// Authored peace duration in minutes (special effect 110 `value1`).
    pub non_pvp_duration_min: u16,
    pub treasury: Treasury,
    /// Only authoritative tower death during an active siege enables engraving.
    pub tower_destroyed: bool,
    /// None means the template's initial/full HP; an active siege preserves partial damage.
    pub tower_hp: Option<i32>,
    /// Last explicit siege generation; retained when no siege is currently present.
    pub last_siege_id: u64,
    /// Accepted attacker for one calendar cycle (WP-SD2a). At most one per zone and cycle;
    /// cleared when the cycle's warmup materialises the siege, on cancel, or on a disband or
    /// owner change before the warmup.
    pub declaration: Option<Declaration>,
    pub siege: Option<SiegeState>,
    /// `Cycle::auction_start` of the last cycle whose declaration-auction camp lot this
    /// territory posted automatically (WP-SD1c); 0 = none. Persisted so a restart inside the
    /// auction period cannot post the same lot twice.
    pub auction_posted_cycle: i64,
    /// A test-only authority row that must never reach `dominions` (the GM `nation dominion`
    /// hook uses a placeholder tower that would fail startup validation). Never persisted.
    pub ephemeral: bool,
}

impl DominionState {
    pub fn unowned(zone_group_id: u32, tower: TowerIdentity) -> Self {
        Self {
            zone_group_id,
            tower,
            owner_expedition_id: 0,
            ownership_generation: 0,
            reign_start_at: 0,
            claimed_at: 0,
            last_siege_end_at: 0,
            tax_rate: 0,
            national_tax_rate: 0,
            last_paid_at: 0,
            last_tax_rate_changed_at: 0,
            last_national_tax_rate_changed_at: 0,
            national_monument_db_id: 0,
            national_monument_x: 0.0,
            national_monument_y: 0.0,
            national_monument_z: 0.0,
            non_pvp_start_at: 0,
            non_pvp_duration_min: 0,
            treasury: Treasury::default(),
            tower_destroyed: false,
            tower_hp: None,
            last_siege_id: 0,
            declaration: None,
            siege: None,
            auction_posted_cycle: 0,
            ephemeral: false,
        }
    }

    /// The cycle a declaration (or its calendar-materialised siege) belongs to. A GM
    /// schedule has none, so the calendar ignores it.
    pub fn declared_cycle(&self) -> Option<i64> {
        self.declaration
            .map(|declaration| declaration.cycle_auction_start)
            .or_else(|| {
                self.siege
                    .as_ref()
                    .filter(|siege| {
                        siege.materialised_from_declaration
                            && siege.status != SiegeStatus::Finished
                            && siege.calendar_cycle > 0
                    })
                    .map(|siege| siege.calendar_cycle)
            })
    }

    /// Drops the national monument reference. The caller deletes the doodad row and announces
    /// the clear (the authority cannot reach the world / doodad manager).
    fn clear_national_monument_fields(&mut self) {
        self.national_monument_db_id = 0;
        self.national_monument_x = 0.0;
        self.national_monument_y = 0.0;
        self.national_monument_z = 0.0;
    }

    pub fn validate(&self) -> Result<(), DominionError> {
        validate_identity(self.zone_group_id, self.tower)?;
        if !(0..=50).contains(&self.tax_rate) || !self.treasury.is_valid() {
            return Err(DominionError::InvalidTreasury);
        }
        if self.tower_hp.is_some_and(|hp| hp < 0)
            || (self.tower_destroyed && self.tower_hp != Some(0))
            || self.national_tax_rate > MAX_NATIONAL_TAX_RATE
            || self.national_monument_db_id < 0
            || ![
                self.national_monument_x,
                self.national_monument_y,
                self.national_monument_z,
            ]
            .iter()
            .all(|v| v.is_finite())
        {
            return Err(DominionError::InvalidState);
        }
        if self.national_monument_db_id == 0
            && (self.national_monument_x != 0.0
                || self.national_monument_y != 0.0
                || self.national_monument_z != 0.0)
        {
            return Err(DominionError::InvalidState);
        }
        if [
            self.reign_start_at,
            self.claimed_at,
            self.last_siege_end_at,
            self.last_paid_at,
            self.last_tax_rate_changed_at,
            self.last_national_tax_rate_changed_at,
            self.non_pvp_start_at,
            self.auction_posted_cycle,
        ]
        .iter()
        .any(|t| *t < 0)
            || (self.owner_expedition_id == 0 && (self.reign_start_at != 0 || self.claimed_at != 0))
            || (self.owner_expedition_id != 0 && self.ownership_generation == 0)
            || self.claimed_at > self.reign_start_at
        {
            return Err(DominionError::InvalidState);
        }
        if let Some(siege) = &self.siege {
            if siege.siege_id == 0
                || siege.siege_id != self.last_siege_id
                || siege.attacker_expedition_id == 0
                || siege.defender_expedition_id == 0
                || siege.attacker_expedition_id == siege.defender_expedition_id
                || siege.ownership_generation == 0
                || siege.scheduled_at < 0
                || siege.starts_at < siege.scheduled_at
                || siege.ends_at <= siege.starts_at
                || siege.ends_at.checked_mul(1000).is_none()
                || siege.defender_ni > siege.defender_nr
                || usize::from(siege.defender_nr) > MAX_PARTICIPANTS_PER_SIDE
                || (siege.status == SiegeStatus::Finished)
                    != (siege.outcome != SiegeOutcome::Pending)
                || (siege.status != SiegeStatus::Finished
                    && (siege.ended_at != 0
                        || siege.defender_expedition_id != self.owner_expedition_id
                        || siege.ownership_generation != self.ownership_generation))
                || (siege.status == SiegeStatus::Finished && siege.ended_at < siege.scheduled_at)
                || (self.tower_hp.is_some() && siege.status != SiegeStatus::Active)
            {
                return Err(DominionError::InvalidSiege);
            }
            for side in [SiegeSide::Attacker, SiegeSide::Defender] {
                if siege
                    .participants
                    .values()
                    .filter(|p| p.side == side)
                    .count()
                    > MAX_PARTICIPANTS_PER_SIDE
                {
                    return Err(DominionError::ParticipantLimit);
                }
            }
            for (id, participant) in &siege.participants {
                let expedition = match participant.side {
                    SiegeSide::Attacker => siege.attacker_expedition_id,
                    SiegeSide::Defender => siege.defender_expedition_id,
                };
                if *id == 0
                    || *id != participant.character_id
                    || participant.expedition_id != expedition
                {
                    return Err(DominionError::InvalidSiege);
                }
            }
        } else if self.tower_destroyed || self.tower_hp.is_some() {
            return Err(DominionError::InvalidSiege);
        }
        if let Some(declaration) = self.declaration {
            if self.owner_expedition_id == 0
                || declaration.attacker_expedition_id == 0
                || declaration.attacker_expedition_id == self.owner_expedition_id
                || declaration.declared_at < 0
                || declaration.cycle_auction_start <= 0
                || self
                    .siege
                    .as_ref()
                    .is_some_and(|siege| siege.status != SiegeStatus::Finished)
                || ![declaration.hq_x, declaration.hq_y, declaration.hq_z]
                    .iter()
                    .all(|v| v.is_finite())
            {
                return Err(DominionError::InvalidState);
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum DominionError {
    #[error("unknown dominion")]
    Missing,
    #[error("invalid territory/tower/expedition/character identity")]
    InvalidIdentity,
    #[error("tower does not match this territory")]
    WrongTower,
    #[error("territory is already owned")]
    AlreadyOwned,
    #[error("expedition has reached the configured owned-dominion limit")]
    TooManyOwned,
    #[error("invalid persisted territory authority")]
    InvalidState,
    #[error("invalid siege state")]
    InvalidSiege,
    #[error("siege deadlines are invalid")]
    InvalidSchedule,
    #[error("a siege is already scheduled or active")]
    SiegeInProgress,
    #[error("siege is not active")]
    SiegeNotActive,
    #[error("tower has not been destroyed during this siege")]
    TowerNotDestroyed,
    #[error("character is not a registered attacker in the designated expedition")]
    NotRegistered,
    #[error("participant side or expedition does not match this siege")]
    WrongSide,
    #[error("siege side has reached its participant capacity")]
    ParticipantLimit,
    #[error("defence reinforcements have not arrived yet")]
    ReinforceBlocked,
    #[error("engraving is already in progress for this character")]
    AlreadyEngraving,
    #[error("engraving cannot finish before the siege deadline")]
    TooLate,
    #[error("engraving has not completed its minimum channel duration")]
    NotReady,
    #[error("engraving handle is stale, cancelled or belongs to another server session")]
    StaleHandle,
    #[error("territory is not owned by this expedition")]
    WrongOwner,
    #[error("resource operation failed")]
    Resources,
    #[error("invalid or overflowing treasury amount")]
    InvalidTreasury,
    #[error("territory tax rate cannot change before its one-hour cooldown expires")]
    TaxRateChangeLater,
    #[error("the national tax rate is outside 0..=1000")]
    InvalidNationalTaxRate,
    #[error("the national tax rate cannot change before its 24-hour cooldown expires")]
    NationalTaxRateChangeLater,
    #[error("this dominion already has a nation monument")]
    NoMoreNationalMonument,
    #[error("a nation monument cannot be placed during a siege period")]
    NationalMonumentDuringSiege,
    #[error("the territory is already in its non-PvP peace duration")]
    NonPvpActive,
    #[error("property tax quote no longer matches the territory owner or rate")]
    StaleTaxQuote,
    #[error("this territory is already declared by the same expedition")]
    AlreadyDeclared,
    #[error("this territory is already declared by another expedition")]
    DeclaredByOther,
    #[error("this territory has no siege declaration")]
    NotDeclared,
    #[error("timestamp or generation overflow")]
    Overflow,
}

/// Only the authority can construct a handle. It is deliberately absent from DB rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngravingHandle {
    pub zone_group_id: u32,
    pub siege_id: u64,
    pub character_id: u32,
    pub tower: TowerIdentity,
    pub started_at_ms: i64,
    pub ready_at_ms: i64,
    ownership_generation: u64,
    session_nonce: u128,
    channel_nonce: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct SavedRow {
    pub revision: u64,
    pub state: DominionState,
}

/// A transaction's exact snapshots. A failed/replaced save never consumes another batch.
#[derive(Debug, Clone, Default)]
pub struct SaveBatch {
    session_nonce: u128,
    pub(crate) rows: Vec<SavedRow>,
    pub(crate) deleted: Vec<(u32, u64)>,
}

impl SaveBatch {
    pub fn changed_count(&self) -> usize {
        self.rows.len() + self.deleted.len()
    }
}

#[derive(Debug)]
struct Entry {
    state: DominionState,
    revision: u64,
    dirty: bool,
}

#[derive(Debug, Default)]
struct Inner {
    states: BTreeMap<u32, Entry>,
    removed: BTreeMap<u32, u64>,
    revision: u64,
    channel_nonce: u64,
    channels: BTreeMap<(u32, u32), EngravingHandle>,
    pending_save: Option<SaveBatch>,
}

#[derive(Debug)]
pub struct DominionManager {
    inner: Mutex<Inner>,
    session_nonce: u128,
}

impl Default for DominionManager {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            session_nonce: rand::random(),
        }
    }
}

fn validate_identity(zone_group_id: u32, tower: TowerIdentity) -> Result<(), DominionError> {
    if zone_group_id == 0
        || zone_group_id > u16::MAX as u32
        || tower.house_id == 0
        || tower.guard_tower_setting_id == 0
    {
        return Err(DominionError::InvalidIdentity);
    }
    Ok(())
}

fn next_revision(inner: &Inner) -> Result<u64, DominionError> {
    inner.revision.checked_add(1).ok_or(DominionError::Overflow)
}

/// Creates a siege record on `state`; the caller holds the authority lock and validates the
/// declaration-specific preconditions. Shared by the GM schedule and the calendar
/// materialisation (WP-SD2a) so both write the same record.
fn start_siege(
    state: &mut DominionState,
    attacker_expedition_id: u32,
    starts_at: i64,
    ends_at: i64,
    now_seconds: i64,
    limits: SiegeLimits,
    declaration_cycle: Option<i64>,
) -> Result<(), DominionError> {
    if now_seconds < 0
        || starts_at < now_seconds
        || ends_at <= starts_at
        || ends_at.checked_mul(1000).is_none()
        || limits.ni > limits.nr
        || usize::from(limits.nr) > MAX_PARTICIPANTS_PER_SIDE
    {
        return Err(DominionError::InvalidSchedule);
    }
    if attacker_expedition_id == 0
        || state.owner_expedition_id == 0
        || attacker_expedition_id == state.owner_expedition_id
    {
        return Err(DominionError::InvalidIdentity);
    }
    if state
        .siege
        .as_ref()
        .is_some_and(|s| s.status != SiegeStatus::Finished)
    {
        return Err(DominionError::SiegeInProgress);
    }
    let siege_id = state
        .last_siege_id
        .checked_add(1)
        .ok_or(DominionError::Overflow)?;
    state.last_siege_id = siege_id;
    state.tower_destroyed = false;
    state.tower_hp = None;
    state.siege = Some(SiegeState {
        siege_id,
        attacker_expedition_id,
        defender_expedition_id: state.owner_expedition_id,
        ownership_generation: state.ownership_generation,
        status: if starts_at == now_seconds {
            SiegeStatus::Active
        } else {
            SiegeStatus::Scheduled
        },
        outcome: SiegeOutcome::Pending,
        scheduled_at: now_seconds,
        starts_at,
        ends_at,
        ended_at: 0,
        reinforced: false,
        defender_ni: limits.ni,
        defender_nr: limits.nr,
        outcome_credited: false,
        materialised_from_declaration: declaration_cycle.is_some(),
        calendar_cycle: declaration_cycle.unwrap_or(0),
        participants: BTreeMap::new(),
    });
    Ok(())
}

fn replace(inner: &mut Inner, state: DominionState, revision: u64) {
    inner.revision = revision;
    inner.removed.remove(&state.zone_group_id);
    inner.states.insert(
        state.zone_group_id,
        Entry {
            state,
            revision,
            dirty: true,
        },
    );
}

fn engraving_allowed(
    state: &DominionState,
    tower: TowerIdentity,
    character_id: u32,
    expedition_id: u32,
    now_seconds: i64,
) -> Result<(), DominionError> {
    if state.tower != tower {
        return Err(DominionError::WrongTower);
    }
    let siege = state.siege.as_ref().ok_or(DominionError::SiegeNotActive)?;
    if !siege.active_at(now_seconds)
        || siege.defender_expedition_id != state.owner_expedition_id
        || siege.ownership_generation != state.ownership_generation
    {
        return Err(DominionError::SiegeNotActive);
    }
    let participant = siege
        .participants
        .get(&character_id)
        .ok_or(DominionError::NotRegistered)?;
    if expedition_id == 0
        || participant.side != SiegeSide::Attacker
        || participant.expedition_id != expedition_id
        || siege.attacker_expedition_id != expedition_id
    {
        return Err(DominionError::NotRegistered);
    }
    if !state.tower_destroyed {
        return Err(DominionError::TowerNotDestroyed);
    }
    Ok(())
}

impl DominionManager {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, zone_group_id: u32) -> Option<DominionState> {
        self.lock()
            .states
            .get(&zone_group_id)
            .map(|entry| entry.state.clone())
    }

    /// The 1.2 siege larceny window (`ui_texts` 3334 `in_siege_warning`): the territory's siege
    /// is active and its attackers hold the Archeum Lodestone (`tower_destroyed`). The window
    /// closes when the siege ends (the `active_at` deadline) or the lodestone is retaken
    /// (`reset_tower_destroyed`); a capture / release clears the flag as well. Server logic:
    /// the 1.2 data has no table for the rule (INFERRED, see `larceny`).
    pub fn siege_larceny_window_open(&self, zone_group_id: u32, now_seconds: i64) -> bool {
        if now_seconds < 0 {
            return false;
        }
        self.lock().states.get(&zone_group_id).is_some_and(|entry| {
            let state = &entry.state;
            state.tower_destroyed
                && state
                    .siege
                    .as_ref()
                    .is_some_and(|siege| siege.active_at(now_seconds))
        })
    }

    pub fn all(&self) -> Vec<DominionState> {
        self.lock()
            .states
            .values()
            .map(|entry| entry.state.clone())
            .collect()
    }

    pub fn state(&self, zone_group_id: u32) -> Option<DominionState> {
        self.get(zone_group_id)
    }

    pub fn snapshots(&self) -> Vec<DominionState> {
        self.all()
    }

    pub fn by_tower(&self, tower: TowerIdentity) -> Option<DominionState> {
        self.lock()
            .states
            .values()
            .find(|entry| entry.state.tower == tower)
            .map(|entry| entry.state.clone())
    }

    /// Validate the complete DB snapshot before replacing authority; no channel resumes.
    pub fn restore(&self, states: Vec<DominionState>) -> Result<(), DominionError> {
        let mut live = self.lock();
        let mut restored = Inner {
            revision: live.revision,
            channel_nonce: live.channel_nonce,
            ..Inner::default()
        };
        let mut towers = BTreeMap::new();
        for state in states {
            state.validate()?;
            if restored.states.contains_key(&state.zone_group_id)
                || towers
                    .insert(state.tower.house_id, state.zone_group_id)
                    .is_some()
            {
                return Err(DominionError::InvalidState);
            }
            let revision = next_revision(&restored)?;
            restored.revision = revision;
            restored.states.insert(
                state.zone_group_id,
                Entry {
                    state,
                    revision,
                    dirty: false,
                },
            );
        }
        // Reload never reuses a revision or cancelled channel nonce in this manager.
        *live = restored;
        Ok(())
    }

    pub fn register_tower(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
    ) -> Result<DominionState, DominionError> {
        validate_identity(zone_group_id, tower)?;
        let mut inner = self.lock();
        if let Some(existing) = inner.states.get(&zone_group_id) {
            return if existing.state.tower == tower {
                Ok(existing.state.clone())
            } else {
                Err(DominionError::WrongTower)
            };
        }
        if inner
            .states
            .values()
            .any(|e| e.state.tower.house_id == tower.house_id)
        {
            return Err(DominionError::WrongTower);
        }
        let state = DominionState::unowned(zone_group_id, tower);
        let revision = next_revision(&inner)?;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Pack validation/consumption must succeed while the same authority lock protects the
    /// claim. All fallible state work precedes the callback; no ownership is minted on failure.
    pub fn declare_unowned_with(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
        expedition_id: u32,
        now_seconds: i64,
        consume: impl FnOnce() -> bool,
    ) -> Result<DominionState, DominionError> {
        self.declare_unowned_with_limit(
            zone_group_id,
            tower,
            expedition_id,
            now_seconds,
            u32::MAX,
            consume,
        )
    }

    /// [`declare_unowned_with`] with the expedition's owned-territory limit applied under the
    /// same authority lock as the claim, so two claims cannot both pass the check. The count
    /// is taken over current owners (an unowned record does not count).
    pub fn declare_unowned_with_limit(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
        expedition_id: u32,
        now_seconds: i64,
        limit: u32,
        consume: impl FnOnce() -> bool,
    ) -> Result<DominionState, DominionError> {
        validate_identity(zone_group_id, tower)?;
        if expedition_id == 0 || now_seconds < 0 {
            return Err(DominionError::InvalidIdentity);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .map(|e| e.state.clone())
            .unwrap_or_else(|| DominionState::unowned(zone_group_id, tower));
        if state.tower != tower
            || inner.states.values().any(|e| {
                e.state.zone_group_id != zone_group_id && e.state.tower.house_id == tower.house_id
            })
        {
            return Err(DominionError::WrongTower);
        }
        if state.owner_expedition_id != 0 {
            return Err(DominionError::AlreadyOwned);
        }
        // GM-seeded test dominions are ephemeral and do not represent a real holding.
        let owned = inner
            .states
            .values()
            .filter(|e| !e.state.ephemeral && e.state.owner_expedition_id == expedition_id)
            .count();
        if u64::try_from(owned).unwrap_or(u64::MAX) >= u64::from(limit) {
            return Err(DominionError::TooManyOwned);
        }
        let revision = next_revision(&inner)?;
        state.ownership_generation = state
            .ownership_generation
            .checked_add(1)
            .ok_or(DominionError::Overflow)?;
        state.owner_expedition_id = expedition_id;
        state.reign_start_at = now_seconds;
        state.claimed_at = now_seconds;
        // A row restored while unowned must not hand a stale peace window to the new owner.
        state.non_pvp_start_at = 0;
        state.non_pvp_duration_min = 0;
        state.declaration = None;
        state.clear_national_monument_fields();
        if !consume() {
            return Err(DominionError::Resources);
        }
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Administrative owner change / bestowal (WP-SL6a). One locked check-and-set: the
    /// target expedition's owned-territory limit is counted under the same lock, so two
    /// transfers cannot both pass. A transfer is not a fresh claim (`claimed_at = 0`, so the
    /// `var_delay_siege_x_days_after_dominion_declare` protection does not start), and an
    /// unfinished siege is refused because its frozen defender would stop matching the owner.
    /// The caller broadcasts the owner change.
    pub fn transfer_owner(
        &self,
        zone_group_id: u32,
        expedition_id: u32,
        now_seconds: i64,
        limit: u32,
    ) -> Result<DominionState, DominionError> {
        if expedition_id == 0 || now_seconds < 0 {
            return Err(DominionError::InvalidIdentity);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if state.owner_expedition_id == expedition_id {
            return Ok(state);
        }
        if state
            .siege
            .as_ref()
            .is_some_and(|siege| siege.status != SiegeStatus::Finished)
        {
            return Err(DominionError::SiegeInProgress);
        }
        let owned = inner
            .states
            .values()
            .filter(|e| !e.state.ephemeral && e.state.owner_expedition_id == expedition_id)
            .count();
        if u64::try_from(owned).unwrap_or(u64::MAX) >= u64::from(limit) {
            return Err(DominionError::TooManyOwned);
        }
        let revision = next_revision(&inner)?;
        state.ownership_generation = state
            .ownership_generation
            .checked_add(1)
            .ok_or(DominionError::Overflow)?;
        state.owner_expedition_id = expedition_id;
        state.reign_start_at = now_seconds;
        state.claimed_at = 0;
        state.non_pvp_start_at = 0;
        state.non_pvp_duration_min = 0;
        state.declaration = None;
        state.clear_national_monument_fields();
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Marks a test-only state as never persisted (see [`DominionState::ephemeral`]).
    pub fn mark_ephemeral(&self, zone_group_id: u32) {
        let mut inner = self.lock();
        if let Some(entry) = inner.states.get_mut(&zone_group_id) {
            entry.state.ephemeral = true;
        }
    }

    pub fn remove_tower(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
    ) -> Result<DominionState, DominionError> {
        let mut inner = self.lock();
        let state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if state.tower != tower {
            return Err(DominionError::WrongTower);
        }
        let revision = next_revision(&inner)?;
        inner.states.remove(&zone_group_id);
        inner.channels.retain(|(zone, _), _| *zone != zone_group_id);
        inner.removed.insert(zone_group_id, revision);
        inner.revision = revision;
        Ok(state)
    }

    /// Places the national monument: one persistent doodad per territory, never during a
    /// siege period. The caller spawns the statue, passes its doodad row id and broadcasts
    /// `SCNationalMonumentChanged` (RENAME-9).
    pub fn place_national_monument(
        &self,
        zone_group_id: u32,
        owner_expedition_id: u32,
        db_id: u32,
        x: f32,
        y: f32,
        z: f32,
    ) -> Result<DominionState, DominionError> {
        if db_id == 0 || ![x, y, z].iter().all(|v| v.is_finite()) {
            return Err(DominionError::InvalidState);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if owner_expedition_id == 0 || state.owner_expedition_id != owner_expedition_id {
            return Err(DominionError::WrongOwner);
        }
        if state.national_monument_db_id != 0 {
            return Err(DominionError::NoMoreNationalMonument);
        }
        if state
            .siege
            .as_ref()
            .is_some_and(|siege| siege.status != SiegeStatus::Finished)
        {
            return Err(DominionError::NationalMonumentDuringSiege);
        }
        let revision = next_revision(&inner)?;
        state.national_monument_db_id = i64::from(db_id);
        state.national_monument_x = x;
        state.national_monument_y = y;
        state.national_monument_z = z;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Removes the national monument fields from a territory. The caller deletes the doodad
    /// row and broadcasts the empty `SCNationalMonumentChanged`. Returns the new state.
    pub fn clear_national_monument(
        &self,
        zone_group_id: u32,
        owner_expedition_id: u32,
    ) -> Result<DominionState, DominionError> {
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if owner_expedition_id == 0 || state.owner_expedition_id != owner_expedition_id {
            return Err(DominionError::WrongOwner);
        }
        if state.national_monument_db_id == 0 {
            return Ok(state);
        }
        let revision = next_revision(&inner)?;
        state.national_monument_db_id = 0;
        state.national_monument_x = 0.0;
        state.national_monument_y = 0.0;
        state.national_monument_z = 0.0;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Starts the territory's non-PvP peace window. `duration_min` is the authored special
    /// effect `value1` (skill 23992: 120 minutes). The skill system owns the 4-hour reuse
    /// cooldown and the reagent consumption.
    pub fn start_non_pvp(
        &self,
        zone_group_id: u32,
        owner_expedition_id: u32,
        duration_min: u16,
        now_seconds: i64,
    ) -> Result<DominionState, DominionError> {
        if duration_min == 0 || now_seconds < 0 {
            return Err(DominionError::InvalidSchedule);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if owner_expedition_id == 0 || state.owner_expedition_id != owner_expedition_id {
            return Err(DominionError::WrongOwner);
        }
        let end = state
            .non_pvp_start_at
            .saturating_add(i64::from(state.non_pvp_duration_min).saturating_mul(60));
        if state.non_pvp_start_at > 0 && now_seconds < end {
            return Err(DominionError::NonPvpActive);
        }
        let revision = next_revision(&inner)?;
        state.non_pvp_start_at = now_seconds;
        state.non_pvp_duration_min = duration_min;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Clears every non-PvP window whose authored end (`start + duration_min*60`) has passed.
    /// The client derives the same end from the start packet but keeps its cached end until
    /// `OnDominionNonPvPStopped` arrives (its peace-skill precheck refuses while it is set), so
    /// this is the natural-expiry edge. Early termination is UNKNOWN in 1.2. Returns the zone
    /// groups that ended; a second call at the same time returns none.
    pub fn expire_non_pvp(&self, now_seconds: i64) -> Result<Vec<u32>, DominionError> {
        if now_seconds < 0 {
            return Err(DominionError::InvalidSchedule);
        }
        let mut inner = self.lock();
        let expired: Vec<u32> = inner
            .states
            .iter()
            .filter(|(_, entry)| {
                let state = &entry.state;
                state.non_pvp_start_at > 0
                    && now_seconds
                        >= state.non_pvp_start_at.saturating_add(
                            i64::from(state.non_pvp_duration_min).saturating_mul(60),
                        )
            })
            .map(|(zone, _)| *zone)
            .collect();
        let mut ended = Vec::with_capacity(expired.len());
        for zone in expired {
            let revision = next_revision(&inner)?;
            let Some(entry) = inner.states.get(&zone) else {
                continue;
            };
            let mut state = entry.state.clone();
            state.non_pvp_start_at = 0;
            state.non_pvp_duration_min = 0;
            replace(&mut inner, state, revision);
            ended.push(zone);
        }
        Ok(ended)
    }

    pub fn prepare_save(&self) -> SaveBatch {
        let inner = self.lock();
        SaveBatch {
            session_nonce: self.session_nonce,
            rows: inner
                .states
                .values()
                .filter(|e| e.dirty && !e.state.ephemeral)
                .map(|e| SavedRow {
                    revision: e.revision,
                    state: e.state.clone(),
                })
                .collect(),
            deleted: inner
                .removed
                .iter()
                .map(|(zone, revision)| (*zone, *revision))
                .collect(),
        }
    }

    /// Call only after this exact batch's transaction committed. Revision comparisons leave
    /// changes made while SQL was in flight dirty, including deletion/recreation of a zone.
    pub fn acknowledge_committed(&self, batch: &SaveBatch) {
        if batch.session_nonce != self.session_nonce {
            return;
        }
        let mut inner = self.lock();
        for row in &batch.rows {
            if let Some(entry) = inner.states.get_mut(&row.state.zone_group_id) {
                if entry.revision == row.revision {
                    entry.dirty = false;
                }
            }
        }
        for (zone, revision) in &batch.deleted {
            if inner.removed.get(zone) == Some(revision) {
                inner.removed.remove(zone);
            }
        }
    }

    /// SaveManager serializes world saves. Failed writes discard this transaction's pending
    /// acknowledgment while dirty rows remain available to the next retry.
    pub async fn save(&self, tx: &mut sqlx::Transaction<'_, sqlx::MySql>) -> sqlx::Result<usize> {
        let batch = self.prepare_save();
        self.save_prepared(tx, batch).await
    }

    /// Housing and Dominion snapshots are captured together under housing.operation_lock,
    /// released before awaiting SQL. Keep this exact revision receipt until transaction commit.
    pub async fn save_prepared(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        batch: SaveBatch,
    ) -> sqlx::Result<usize> {
        self.lock().pending_save = None;
        let count = super::db::write_batch(tx, &batch).await?;
        self.lock().pending_save = Some(batch);
        Ok(count)
    }

    /// Conventional world-save callback; call immediately after tx.commit succeeds.
    pub fn commit_save(&self) {
        let batch = self.lock().pending_save.take();
        if let Some(batch) = batch {
            self.acknowledge_committed(&batch);
        }
    }
}
