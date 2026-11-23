use super::{
    engraving_allowed, next_revision, replace, start_siege, Declaration, DominionError,
    DominionManager, DominionState, EngravingHandle, ParticipantCap, SiegeLimits, SiegeOutcome,
    SiegeParticipant, SiegeSide, SiegeStatus, TowerIdentity, ENGRAVING_MIN_DURATION_MS,
};

impl DominionManager {
    /// Schedules with the native 100-character roster limits. Callers that know the authored
    /// `siege_settings` row use [`Self::schedule_siege_with_limits`].
    pub fn schedule_siege(
        &self,
        zone_group_id: u32,
        attacker_expedition_id: u32,
        starts_at: i64,
        ends_at: i64,
        now_seconds: i64,
    ) -> Result<DominionState, DominionError> {
        self.schedule_siege_with_limits(
            zone_group_id,
            attacker_expedition_id,
            starts_at,
            ends_at,
            now_seconds,
            SiegeLimits::NATIVE,
        )
    }

    /// Schedules with roster limits frozen into the siege record.
    ///
    /// `defender_ni` applies before the defender reinforcement wave, `defender_nr` after it;
    /// the attacker uses `defender_nr` because its own cap is UNKNOWN. Freezing keeps a
    /// mid-siege change in the defender's castle count (or a lost ephemeral GM dominion)
    /// from changing the caps under a running siege. Existing participants above a frozen cap
    /// are not evicted: there is no evidence for it, and the authority only checks on join.
    pub fn schedule_siege_with_limits(
        &self,
        zone_group_id: u32,
        attacker_expedition_id: u32,
        starts_at: i64,
        ends_at: i64,
        now_seconds: i64,
        limits: SiegeLimits,
    ) -> Result<DominionState, DominionError> {
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        let revision = next_revision(&inner)?;
        start_siege(
            &mut state,
            attacker_expedition_id,
            starts_at,
            ends_at,
            now_seconds,
            limits,
            None,
        )?;
        inner.channels.retain(|(zone, _), _| *zone != zone_group_id);
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Records an accepted siege declaration for one calendar cycle (WP-SD2a). The period,
    /// range, policy and item checks happen in the caller under the same
    /// `housing.operation_lock`; this method only enforces the per-zone/cycle uniqueness.
    /// A second declaration by the same expedition is [`DominionError::AlreadyDeclared`]
    /// (client 442), by another expedition [`DominionError::DeclaredByOther`] (client 443).
    pub fn declare(
        &self,
        zone_group_id: u32,
        attacker_expedition_id: u32,
        cycle_auction_start: i64,
        declared_at: i64,
        hq_obj_id: u32,
        hq: (f32, f32, f32),
    ) -> Result<DominionState, DominionError> {
        if attacker_expedition_id == 0
            || cycle_auction_start <= 0
            || declared_at < 0
            || !hq.0.is_finite()
            || !hq.1.is_finite()
            || !hq.2.is_finite()
        {
            return Err(DominionError::InvalidIdentity);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if state.owner_expedition_id == 0 || state.owner_expedition_id == attacker_expedition_id {
            return Err(DominionError::InvalidIdentity);
        }
        if let Some(existing) = state.declaration {
            return Err(
                if existing.attacker_expedition_id == attacker_expedition_id {
                    DominionError::AlreadyDeclared
                } else {
                    DominionError::DeclaredByOther
                },
            );
        }
        if state
            .siege
            .as_ref()
            .is_some_and(|siege| siege.status != SiegeStatus::Finished)
        {
            return Err(DominionError::SiegeInProgress);
        }
        let revision = next_revision(&inner)?;
        state.declaration = Some(Declaration {
            attacker_expedition_id,
            declared_at,
            cycle_auction_start,
            hq_obj_id,
            hq_x: hq.0,
            hq_y: hq.1,
            hq_z: hq.2,
        });
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Reserves this territory's automatic camp-lot post for `cycle_auction_start`
    /// (WP-SD1c). The check and the marker write share the authority lock, so a repeated tick
    /// or two racing callers post once; `Ok(false)` means another caller already reserved it.
    /// The caller posts the lot and calls [`Self::release_auction_post`] when the post fails,
    /// so a bad data row or a failed item creation is retried on a later tick.
    pub fn claim_auction_post(
        &self,
        zone_group_id: u32,
        cycle_auction_start: i64,
    ) -> Result<bool, DominionError> {
        if cycle_auction_start <= 0 {
            return Err(DominionError::InvalidSchedule);
        }
        let mut inner = self.lock();
        let Some(entry) = inner.states.get(&zone_group_id) else {
            return Err(DominionError::Missing);
        };
        let mut state = entry.state.clone();
        if state.auction_posted_cycle == cycle_auction_start {
            return Ok(false);
        }
        let revision = next_revision(&inner)?;
        state.auction_posted_cycle = cycle_auction_start;
        replace(&mut inner, state, revision);
        Ok(true)
    }

    /// Undoes [`Self::claim_auction_post`] after the lot could not be created. Only the same
    /// cycle is cleared, so a later claim of another cycle is never dropped.
    pub fn release_auction_post(&self, zone_group_id: u32, cycle_auction_start: i64) {
        let mut inner = self.lock();
        let Some(entry) = inner.states.get(&zone_group_id) else {
            return;
        };
        if entry.state.auction_posted_cycle != cycle_auction_start {
            return;
        }
        let Ok(revision) = next_revision(&inner) else {
            return;
        };
        let mut state = entry.state.clone();
        state.auction_posted_cycle = 0;
        replace(&mut inner, state, revision);
    }

    /// Cancels the declaration of `expedition_id`. A declaration of another expedition (or
    /// none at all) is [`DominionError::NotDeclared`]; the caller has already checked the
    /// caller's rights.
    pub fn cancel_declaration(
        &self,
        zone_group_id: u32,
        attacker_expedition_id: u32,
    ) -> Result<DominionState, DominionError> {
        if attacker_expedition_id == 0 {
            return Err(DominionError::InvalidIdentity);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        let declaration = state.declaration.ok_or(DominionError::NotDeclared)?;
        if declaration.attacker_expedition_id != attacker_expedition_id {
            return Err(DominionError::NotDeclared);
        }
        let revision = next_revision(&inner)?;
        state.declaration = None;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Drops a declaration that can no longer be honoured (disband, owner change, missed
    /// warmup). Returns the cleared state only when one existed.
    pub fn drop_declaration(
        &self,
        zone_group_id: u32,
    ) -> Result<Option<DominionState>, DominionError> {
        let mut inner = self.lock();
        let Some(entry) = inner.states.get(&zone_group_id) else {
            return Err(DominionError::Missing);
        };
        if entry.state.declaration.is_none() {
            return Ok(None);
        }
        let mut state = entry.state.clone();
        let revision = next_revision(&inner)?;
        state.declaration = None;
        replace(&mut inner, state.clone(), revision);
        Ok(Some(state))
    }

    /// Records the real spawned camp doodad on an accepted declaration. Returns the updated
    /// state, or `None` when no declaration exists anymore (a racing cancel).
    pub fn bind_declaration_hq(
        &self,
        zone_group_id: u32,
        hq_obj_id: u32,
        hq: (f32, f32, f32),
    ) -> Result<Option<DominionState>, DominionError> {
        if hq_obj_id == 0 || !hq.0.is_finite() || !hq.1.is_finite() || !hq.2.is_finite() {
            return Err(DominionError::InvalidIdentity);
        }
        let mut inner = self.lock();
        let Some(entry) = inner.states.get(&zone_group_id) else {
            return Err(DominionError::Missing);
        };
        if entry.state.declaration.is_none() {
            return Ok(None);
        }
        let mut state = entry.state.clone();
        let revision = next_revision(&inner)?;
        let declaration = state.declaration.as_mut().expect("checked");
        declaration.hq_obj_id = hq_obj_id;
        declaration.hq_x = hq.0;
        declaration.hq_y = hq.1;
        declaration.hq_z = hq.2;
        replace(&mut inner, state.clone(), revision);
        Ok(Some(state))
    }

    /// Materialises the declaration into an explicit siege record at the cycle's warmup
    /// start (WP-SD2a). The check that this declaration belongs to `cycle_auction_start` and
    /// the schedule/clear happen under one lock. A record that already exists (a restart
    /// between the schedule and the save) keeps it and only clears the declaration.
    pub fn materialise_declaration(
        &self,
        zone_group_id: u32,
        cycle_auction_start: i64,
        starts_at: i64,
        ends_at: i64,
        now_seconds: i64,
        limits: SiegeLimits,
    ) -> Result<Option<DominionState>, DominionError> {
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        let Some(declaration) = state.declaration else {
            return Ok(None);
        };
        if declaration.cycle_auction_start != cycle_auction_start {
            return Ok(None);
        }
        let revision = next_revision(&inner)?;
        let attacker = declaration.attacker_expedition_id;
        state.declaration = None;
        match start_siege(
            &mut state,
            attacker,
            starts_at,
            ends_at,
            now_seconds,
            limits,
            Some(declaration.cycle_auction_start),
        ) {
            Ok(()) => {}
            Err(DominionError::SiegeInProgress) => {
                if let Some(siege) = state.siege.as_mut() {
                    siege.materialised_from_declaration = true;
                    siege.calendar_cycle = declaration.cycle_auction_start;
                }
            }
            Err(error) => return Err(error),
        }
        inner.channels.retain(|(zone, _), _| *zone != zone_group_id);
        replace(&mut inner, state.clone(), revision);
        Ok(Some(state))
    }

    /// Registers against the native 100-character array capacity. Callers that know the
    /// authored `siege_settings` caps use [`Self::register_participant_with_cap`].
    pub fn register_participant(
        &self,
        zone_group_id: u32,
        character_id: u32,
        expedition_id: u32,
        side: SiegeSide,
        now_seconds: i64,
    ) -> Result<DominionState, DominionError> {
        self.register_participant_with_cap(
            zone_group_id,
            character_id,
            expedition_id,
            side,
            now_seconds,
            ParticipantCap::NATIVE,
        )
    }

    /// Inserts a roster entry iff the side is below `cap`. Check and insert happen under the
    /// same lock; a full defender roster before the reinforcement wave reports
    /// [`DominionError::ReinforceBlocked`] so the caller can send the client's 457.
    pub fn register_participant_with_cap(
        &self,
        zone_group_id: u32,
        character_id: u32,
        expedition_id: u32,
        side: SiegeSide,
        now_seconds: i64,
        cap: ParticipantCap,
    ) -> Result<DominionState, DominionError> {
        if character_id == 0 || expedition_id == 0 || now_seconds < 0 {
            return Err(DominionError::InvalidIdentity);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        let siege = state.siege.as_mut().ok_or(DominionError::SiegeNotActive)?;
        if siege.status == SiegeStatus::Finished || now_seconds >= siege.ends_at {
            return Err(DominionError::SiegeNotActive);
        }
        let expected = match side {
            SiegeSide::Attacker => siege.attacker_expedition_id,
            SiegeSide::Defender => siege.defender_expedition_id,
        };
        if expedition_id != expected {
            return Err(DominionError::WrongSide);
        }
        let participant = SiegeParticipant {
            character_id,
            expedition_id,
            side,
        };
        if siege.participants.get(&character_id) == Some(&participant) {
            return Ok(state);
        }
        let count = siege
            .participants
            .values()
            .filter(|p| p.side == side && p.character_id != character_id)
            .count();
        if count >= cap.limit {
            return Err(if cap.awaiting_reinforcements {
                DominionError::ReinforceBlocked
            } else {
                DominionError::ParticipantLimit
            });
        }
        let revision = next_revision(&inner)?;
        siege.participants.insert(character_id, participant);
        inner.channels.remove(&(zone_group_id, character_id));
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    pub fn unregister_participant(
        &self,
        zone_group_id: u32,
        character_id: u32,
    ) -> Result<bool, DominionError> {
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        let Some(siege) = state.siege.as_mut() else {
            return Ok(false);
        };
        if !siege.participants.contains_key(&character_id) {
            return Ok(false);
        }
        let revision = next_revision(&inner)?;
        siege.participants.remove(&character_id);
        inner.channels.remove(&(zone_group_id, character_id));
        replace(&mut inner, state, revision);
        Ok(true)
    }

    /// Marks the defender reinforcement wave as arrived at `deadline` (`start(siege) + bdm`).
    ///
    /// The caller passes the authored deadline so the check and the flag change share one
    /// lock; the method is idempotent and returns `Some` only for the transition. It does not
    /// send anything: the caller announces `SCSiegeReinforce` exactly once.
    pub fn mark_reinforced(
        &self,
        zone_group_id: u32,
        siege_id: u64,
        deadline: i64,
        now_seconds: i64,
    ) -> Result<Option<DominionState>, DominionError> {
        if deadline < 0 || now_seconds < 0 {
            return Err(DominionError::InvalidSchedule);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        let Some(siege) = state.siege.as_mut() else {
            return Ok(None);
        };
        if siege.siege_id != siege_id
            || siege.status == SiegeStatus::Finished
            || siege.reinforced
            || now_seconds < deadline
        {
            return Ok(None);
        }
        let revision = next_revision(&inner)?;
        siege.reinforced = true;
        replace(&mut inner, state.clone(), revision);
        Ok(Some(state))
    }

    /// Check-and-sets the kinds 57-60 outcome-credit marker for a finished siege. The caller
    /// credits the roster only on `Some`; `Ok(None)` means the marker belongs to another siege,
    /// the outcome is still pending / cancelled, or the credit was already applied.
    pub fn mark_outcome_credited(
        &self,
        zone_group_id: u32,
        siege_id: u64,
    ) -> Result<Option<DominionState>, DominionError> {
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        let Some(siege) = state.siege.as_mut() else {
            return Ok(None);
        };
        if siege.siege_id != siege_id
            || siege.status != SiegeStatus::Finished
            || siege.outcome == SiegeOutcome::Pending
            || siege.outcome_credited
        {
            return Ok(None);
        }
        let revision = next_revision(&inner)?;
        siege.outcome_credited = true;
        replace(&mut inner, state.clone(), revision);
        Ok(Some(state))
    }

    pub fn allow_engraving(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
        character_id: u32,
        expedition_id: u32,
        now_ms: i64,
    ) -> bool {
        if now_ms < 0 {
            return false;
        }
        self.lock().states.get(&zone_group_id).is_some_and(|entry| {
            engraving_allowed(
                &entry.state,
                tower,
                character_id,
                expedition_id,
                now_ms / 1000,
            )
            .is_ok()
                && entry.state.siege.as_ref().is_some_and(|siege| {
                    now_ms
                        .checked_add(ENGRAVING_MIN_DURATION_MS)
                        .is_some_and(|ready| ready < siege.ends_at * 1000)
                })
        })
    }

    /// Damage authorization before destruction does not grant capture authority.
    pub fn registered_attacker(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
        character_id: u32,
        expedition_id: u32,
        now_seconds: i64,
    ) -> bool {
        if now_seconds < 0 || expedition_id == 0 {
            return false;
        }
        self.lock().states.get(&zone_group_id).is_some_and(|entry| {
            let state = &entry.state;
            state.tower == tower
                && state.siege.as_ref().is_some_and(|siege| {
                    siege.active_at(now_seconds)
                        && siege.defender_expedition_id == state.owner_expedition_id
                        && siege.ownership_generation == state.ownership_generation
                        && siege.attacker_expedition_id == expedition_id
                        && siege.participants.get(&character_id).is_some_and(|p| {
                            p.side == SiegeSide::Attacker && p.expedition_id == expedition_id
                        })
                })
        })
    }

    pub fn mark_tower_destroyed(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
        now_seconds: i64,
    ) -> Result<DominionState, DominionError> {
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if state.tower != tower {
            return Err(DominionError::WrongTower);
        }
        if now_seconds < 0
            || !state.siege.as_ref().is_some_and(|s| {
                s.active_at(now_seconds)
                    && s.defender_expedition_id == state.owner_expedition_id
                    && s.ownership_generation == state.ownership_generation
            })
        {
            return Err(DominionError::SiegeNotActive);
        }
        if state.tower_destroyed {
            return Ok(state);
        }
        let revision = next_revision(&inner)?;
        state.tower_destroyed = true;
        state.tower_hp = Some(0);
        if let Some(siege) = state.siege.as_mut() {
            siege.status = SiegeStatus::Active;
        }
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    pub fn update_tower_hp(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
        hp: i32,
    ) -> Result<DominionState, DominionError> {
        self.update_tower_hp_at(zone_group_id, tower, hp, chrono::Utc::now().timestamp())
    }

    pub fn update_tower_hp_at(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
        hp: i32,
        now_seconds: i64,
    ) -> Result<DominionState, DominionError> {
        if hp < 0 {
            return Err(DominionError::InvalidState);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if state.tower != tower {
            return Err(DominionError::WrongTower);
        }
        if !state.siege.as_ref().is_some_and(|s| {
            s.active_at(now_seconds)
                && s.ownership_generation == state.ownership_generation
                && s.defender_expedition_id == state.owner_expedition_id
        }) {
            return Err(DominionError::SiegeNotActive);
        }
        if state.tower_destroyed && hp != 0 {
            return Err(DominionError::InvalidState);
        }
        if state.tower_hp == Some(hp) && (hp != 0 || state.tower_destroyed) {
            return Ok(state);
        }
        let revision = next_revision(&inner)?;
        state.tower_hp = Some(hp);
        // HP notification precedes the generic death callback. A save in between must
        // already preserve the destroyed, engravable state of the retained lodestone.
        state.tower_destroyed = hp == 0;
        if let Some(siege) = state.siege.as_mut() {
            siege.status = SiegeStatus::Active;
        }
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    pub fn reset_tower_destroyed(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
    ) -> Result<DominionState, DominionError> {
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if state.tower != tower {
            return Err(DominionError::WrongTower);
        }
        if !state.tower_destroyed {
            return Ok(state);
        }
        let revision = next_revision(&inner)?;
        state.tower_destroyed = false;
        state.tower_hp = None;
        inner.channels.retain(|(zone, _), _| *zone != zone_group_id);
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    pub fn begin_engraving(
        &self,
        zone_group_id: u32,
        tower: TowerIdentity,
        character_id: u32,
        expedition_id: u32,
        now_ms: i64,
    ) -> Result<EngravingHandle, DominionError> {
        if now_ms < 0 {
            return Err(DominionError::InvalidSchedule);
        }
        let mut inner = self.lock();
        let state = &inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state;
        engraving_allowed(state, tower, character_id, expedition_id, now_ms / 1000)?;
        let siege = state.siege.as_ref().ok_or(DominionError::SiegeNotActive)?;
        let ready_at_ms = now_ms
            .checked_add(ENGRAVING_MIN_DURATION_MS)
            .ok_or(DominionError::Overflow)?;
        if ready_at_ms >= siege.ends_at * 1000 {
            return Err(DominionError::TooLate);
        }
        if inner.channels.contains_key(&(zone_group_id, character_id)) {
            return Err(DominionError::AlreadyEngraving);
        }
        let channel_nonce = inner
            .channel_nonce
            .checked_add(1)
            .ok_or(DominionError::Overflow)?;
        let handle = EngravingHandle {
            zone_group_id,
            siege_id: siege.siege_id,
            character_id,
            tower,
            started_at_ms: now_ms,
            ready_at_ms,
            ownership_generation: state.ownership_generation,
            session_nonce: self.session_nonce,
            channel_nonce,
        };
        inner.channel_nonce = channel_nonce;
        inner
            .channels
            .insert((zone_group_id, character_id), handle.clone());
        Ok(handle)
    }

    pub fn abort_engraving(&self, handle: &EngravingHandle) -> bool {
        let mut inner = self.lock();
        let key = (handle.zone_group_id, handle.character_id);
        if inner.channels.get(&key) != Some(handle) {
            return false;
        }
        inner.channels.remove(&key);
        true
    }

    pub fn abort_character(&self, character_id: u32) {
        self.lock()
            .channels
            .retain(|(_, id), _| *id != character_id);
    }

    /// The caller must also establish that the same live SkillRef completed uninterrupted.
    pub fn complete_engraving(
        &self,
        handle: &EngravingHandle,
        current_expedition_id: u32,
        now_ms: i64,
    ) -> Result<DominionState, DominionError> {
        let mut inner = self.lock();
        if handle.session_nonce != self.session_nonce
            || inner
                .channels
                .get(&(handle.zone_group_id, handle.character_id))
                != Some(handle)
        {
            return Err(DominionError::StaleHandle);
        }
        let mut state = inner
            .states
            .get(&handle.zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        engraving_allowed(
            &state,
            handle.tower,
            handle.character_id,
            current_expedition_id,
            now_ms / 1000,
        )?;
        let siege = state.siege.as_mut().ok_or(DominionError::SiegeNotActive)?;
        if siege.siege_id != handle.siege_id
            || state.ownership_generation != handle.ownership_generation
        {
            return Err(DominionError::StaleHandle);
        }
        if now_ms < handle.ready_at_ms {
            return Err(DominionError::NotReady);
        }
        let generation = state
            .ownership_generation
            .checked_add(1)
            .ok_or(DominionError::Overflow)?;
        let revision = next_revision(&inner)?;
        state.owner_expedition_id = siege.attacker_expedition_id;
        state.ownership_generation = generation;
        state.reign_start_at = now_ms / 1000;
        // A capture is not a claim: it must not start the fresh-claim siege delay.
        state.claimed_at = 0;
        state.last_siege_end_at = now_ms / 1000;
        state.tower_destroyed = false;
        state.tower_hp = None;
        siege.status = SiegeStatus::Finished;
        siege.outcome = SiegeOutcome::AttackerCaptured;
        siege.ended_at = now_ms / 1000;
        state.declaration = None;
        state.clear_national_monument_fields();
        inner
            .channels
            .retain(|(zone, _), _| *zone != handle.zone_group_id);
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Explicit schedule transitions only. An expired channel always leaves the defender
    /// in control; neither restart nor a elapsed deadline creates an attacker victory.
    pub fn tick(&self, now_seconds: i64) -> Result<Vec<DominionState>, DominionError> {
        self.tick_dual(now_seconds, now_seconds)
    }

    /// [`Self::tick`] with a separate clock for calendar-materialised sieges (WP-SD2a).
    ///
    /// A GM schedule uses the real clock. A siege materialised from a declaration follows the
    /// calendar, so the GM test clock that drives its periods also drives its start/end; with
    /// no test clock both values are the real time.
    pub fn tick_dual(
        &self,
        real_now: i64,
        calendar_now: i64,
    ) -> Result<Vec<DominionState>, DominionError> {
        if real_now < 0 || calendar_now < 0 {
            return Err(DominionError::InvalidSchedule);
        }
        let mut inner = self.lock();
        let mut changes = Vec::new();
        for entry in inner.states.values() {
            let mut state = entry.state.clone();
            let Some(siege) = state.siege.as_mut() else {
                continue;
            };
            if siege.status == SiegeStatus::Finished {
                continue;
            }
            let now_seconds = if siege.materialised_from_declaration {
                calendar_now
            } else {
                real_now
            };
            if now_seconds >= siege.ends_at {
                siege.status = SiegeStatus::Finished;
                siege.outcome = SiegeOutcome::DefenderHeld;
                siege.ended_at = siege.ends_at;
                state.last_siege_end_at = siege.ends_at;
                state.tower_destroyed = false;
                state.tower_hp = None;
                changes.push(state);
            } else if now_seconds >= siege.starts_at && siege.status == SiegeStatus::Scheduled {
                siege.status = SiegeStatus::Active;
                changes.push(state);
            }
        }
        inner
            .revision
            .checked_add(changes.len() as u64)
            .ok_or(DominionError::Overflow)?;
        for state in &changes {
            if state
                .siege
                .as_ref()
                .is_some_and(|s| s.status == SiegeStatus::Finished)
            {
                inner
                    .channels
                    .retain(|(zone, _), _| *zone != state.zone_group_id);
            }
            let revision = inner.revision + 1;
            replace(&mut inner, state.clone(), revision);
        }
        Ok(changes)
    }

    pub fn cancel_siege(
        &self,
        zone_group_id: u32,
        now_seconds: i64,
    ) -> Result<DominionState, DominionError> {
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        let siege = state.siege.as_mut().ok_or(DominionError::SiegeNotActive)?;
        if siege.status == SiegeStatus::Finished {
            return Err(DominionError::SiegeNotActive);
        }
        if now_seconds < siege.scheduled_at {
            return Err(DominionError::InvalidSchedule);
        }
        let revision = next_revision(&inner)?;
        siege.status = SiegeStatus::Finished;
        siege.outcome = SiegeOutcome::Cancelled;
        siege.ended_at = now_seconds;
        state.last_siege_end_at = now_seconds;
        state.tower_destroyed = false;
        state.tower_hp = None;
        inner.channels.retain(|(zone, _), _| *zone != zone_group_id);
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Releases every territory owned by `expedition_id` and cancels an unfinished siege it
    /// defends or attacks (the tower identity and history stay). Used when a forced guild
    /// dismissal leaves no member to hold the dominion; the 1.2 rules only forbid a disbanded
    /// guild from keeping one, and there is no retail capture of this path (PORTING.md,
    /// PIRATE-1). The released state has no owner (`owner_expedition_id = 0`), so it is valid
    /// for the next load and can be declared again. Each pair is `(previous, released)` so the
    /// caller can emit the same participant / phase broadcasts as the GM cancel.
    pub fn release_expedition(
        &self,
        expedition_id: u32,
        now_seconds: i64,
    ) -> Vec<(DominionState, DominionState)> {
        if expedition_id == 0 || now_seconds < 0 {
            return Vec::new();
        }
        let mut inner = self.lock();
        let zones: Vec<u32> = inner
            .states
            .iter()
            .filter(|(_, entry)| {
                let state = &entry.state;
                state.owner_expedition_id == expedition_id
                    || state
                        .declaration
                        .is_some_and(|d| d.attacker_expedition_id == expedition_id)
                    || state.siege.as_ref().is_some_and(|siege| {
                        siege.status != SiegeStatus::Finished
                            && (siege.attacker_expedition_id == expedition_id
                                || siege.defender_expedition_id == expedition_id)
                    })
            })
            .map(|(zone, _)| *zone)
            .collect();
        let mut released = Vec::new();
        for zone in zones {
            let Ok(revision) = next_revision(&inner) else {
                break;
            };
            let Some(entry) = inner.states.get(&zone) else {
                continue;
            };
            let previous = entry.state.clone();
            let mut state = previous.clone();
            if state.owner_expedition_id == expedition_id {
                state.owner_expedition_id = 0;
                state.reign_start_at = 0;
                state.claimed_at = 0;
                // The peace window belongs to the owner that started it; a released record
                // is dropped on the client (0x01e), so no 0x025 is sent and a later owner
                // must not inherit the stale window.
                state.non_pvp_start_at = 0;
                state.non_pvp_duration_min = 0;
                state.clear_national_monument_fields();
            }
            // A disband/forced release invalidates any declaration that involved the guild,
            // as an attacker or as the owner that accepted it.
            if state.declaration.is_some_and(|d| {
                d.attacker_expedition_id == expedition_id
                    || previous.owner_expedition_id == expedition_id
            }) {
                state.declaration = None;
            }
            let cancels_siege = state.siege.as_ref().is_some_and(|siege| {
                siege.status != SiegeStatus::Finished
                    && (siege.attacker_expedition_id == expedition_id
                        || siege.defender_expedition_id == expedition_id)
            });
            if cancels_siege {
                let siege = state.siege.as_mut().expect("checked");
                siege.status = SiegeStatus::Finished;
                siege.outcome = SiegeOutcome::Cancelled;
                siege.ended_at = now_seconds.max(siege.scheduled_at);
                state.last_siege_end_at = siege.ended_at;
                state.tower_destroyed = false;
                state.tower_hp = None;
                inner
                    .channels
                    .retain(|(channel_zone, _), _| *channel_zone != zone);
            }
            replace(&mut inner, state.clone(), revision);
            released.push((previous, state));
        }
        released
    }
}
