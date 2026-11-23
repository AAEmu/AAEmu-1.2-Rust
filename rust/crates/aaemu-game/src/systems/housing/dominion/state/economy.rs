use super::{
    next_revision, replace, DominionError, DominionManager, DominionState, Treasury,
    MAX_NATIONAL_TAX_RATE, NATIONAL_TAX_RATE_CHANGE_COOLDOWN_SECONDS,
    TAX_RATE_CHANGE_COOLDOWN_SECONDS,
};

impl DominionManager {
    pub fn set_tax_rate(
        &self,
        zone_group_id: u32,
        owner_expedition_id: u32,
        tax_rate: i32,
        now_seconds: i64,
    ) -> Result<DominionState, DominionError> {
        if !(0..=50).contains(&tax_rate) || now_seconds < 0 {
            return Err(DominionError::InvalidTreasury);
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
        if state.tax_rate == tax_rate {
            return Ok(state);
        }
        if state.last_tax_rate_changed_at > 0
            && now_seconds.saturating_sub(state.last_tax_rate_changed_at)
                < TAX_RATE_CHANGE_COOLDOWN_SECONDS
        {
            return Err(DominionError::TaxRateChangeLater);
        }
        let revision = next_revision(&inner)?;
        state.tax_rate = tax_rate;
        state.last_tax_rate_changed_at = now_seconds;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Sets the national tax rate (permille/10, 0-1000) of a territory owned by
    /// `owner_expedition_id`. Mirrors the 1.2 client's 86400 s cooldown (`FUN_392f4820`) so a
    /// forged packet cannot bypass it. The caller has already checked that the player is the
    /// king of the owner expedition's top-level faction.
    pub fn set_national_tax_rate(
        &self,
        zone_group_id: u32,
        owner_expedition_id: u32,
        tax_rate: u16,
        now_seconds: i64,
    ) -> Result<DominionState, DominionError> {
        if tax_rate > MAX_NATIONAL_TAX_RATE {
            return Err(DominionError::InvalidNationalTaxRate);
        }
        if now_seconds < 0 {
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
        if state.national_tax_rate == tax_rate {
            return Ok(state);
        }
        if state.last_national_tax_rate_changed_at > 0
            && now_seconds.saturating_sub(state.last_national_tax_rate_changed_at)
                < NATIONAL_TAX_RATE_CHANGE_COOLDOWN_SECONDS
        {
            return Err(DominionError::NationalTaxRateChangeLater);
        }
        let revision = next_revision(&inner)?;
        state.national_tax_rate = tax_rate;
        state.last_national_tax_rate_changed_at = now_seconds;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Only real collected revenue should reach this entry point; no fabricated allowances.
    pub fn credit_treasury(
        &self,
        zone_group_id: u32,
        credit: Treasury,
    ) -> Result<DominionState, DominionError> {
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        let treasury = state
            .treasury
            .checked_add(credit)
            .ok_or(DominionError::InvalidTreasury)?;
        if treasury == state.treasury {
            return Ok(state);
        }
        let revision = next_revision(&inner)?;
        state.treasury = treasury;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    /// Preflight the exact current ownership/rate and ledger capacity before charging. The
    /// callback follows authority -> economy lock order and must not call this manager.
    pub fn credit_property_tax_with(
        &self,
        expected: &DominionState,
        amount: i32,
        charge: impl FnOnce() -> bool,
    ) -> Result<DominionState, DominionError> {
        if amount < 0 {
            return Err(DominionError::InvalidTreasury);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&expected.zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if state.owner_expedition_id == 0
            || state.tower != expected.tower
            || state.owner_expedition_id != expected.owner_expedition_id
            || state.ownership_generation != expected.ownership_generation
            || state.tax_rate != expected.tax_rate
        {
            return Err(DominionError::StaleTaxQuote);
        }
        let treasury = state
            .treasury
            .checked_add(Treasury {
                cur_house_tax_money: amount,
                ..Treasury::default()
            })
            .ok_or(DominionError::InvalidTreasury)?;
        let revision = (amount != 0).then(|| next_revision(&inner)).transpose()?;
        if !charge() {
            return Err(DominionError::Resources);
        }
        if let Some(revision) = revision {
            state.treasury = treasury;
            replace(&mut inner, state.clone(), revision);
        }
        Ok(state)
    }

    /// Collect only actual property revenue. The independently accumulated Peace Fund is
    /// reserved for the siege victor and is never cleared by an ordinary collection.
    pub fn collect_property_tax_with(
        &self,
        expected: &DominionState,
        now_seconds: i64,
        pay: impl FnOnce(i32, i64) -> bool,
    ) -> Result<DominionState, DominionError> {
        if now_seconds < 0 {
            return Err(DominionError::InvalidSchedule);
        }
        let mut inner = self.lock();
        let mut state = inner
            .states
            .get(&expected.zone_group_id)
            .ok_or(DominionError::Missing)?
            .state
            .clone();
        if state.owner_expedition_id == 0
            || state.tower != expected.tower
            || state.owner_expedition_id != expected.owner_expedition_id
            || state.ownership_generation != expected.ownership_generation
        {
            return Err(DominionError::WrongOwner);
        }
        let amount = state.treasury.cur_house_tax_money;
        if amount == 0 {
            return Ok(state);
        }
        if now_seconds < state.last_paid_at {
            return Err(DominionError::InvalidSchedule);
        }
        let revision = next_revision(&inner)?;
        if !pay(amount, state.last_paid_at.max(state.reign_start_at)) {
            return Err(DominionError::Resources);
        }
        state.treasury.cur_house_tax_money = 0;
        state.last_paid_at = now_seconds;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }

    pub fn collect_treasury_with(
        &self,
        zone_group_id: u32,
        owner_expedition_id: u32,
        now_seconds: i64,
        pay: impl FnOnce(Treasury) -> bool,
    ) -> Result<DominionState, DominionError> {
        if now_seconds < 0 {
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
        let revision = next_revision(&inner)?;
        if !pay(state.treasury) {
            return Err(DominionError::Resources);
        }
        state.treasury = Treasury::default();
        state.last_paid_at = now_seconds;
        replace(&mut inner, state.clone(), revision);
        Ok(state)
    }
}
