//! Bounty escrow ledger: the pure, in-memory state behind the 1.2 wanted-player bounty.
//!
//! This module owns the ledger and its transitions only. It knows nothing about packets, the
//! wallet, MySQL, timers or the clock: the caller captures Unix UTC seconds, checks the wanted
//! eligibility with the same predicate as the arrest (`systems::skills::combat`) and moves the
//! copper.
//!
//! * [`BountyLedger::prepare_placement`] validates one placement and reports the amount the
//!   caller has to debit. It changes nothing.
//! * [`BountyLedger::commit_placement`] records the pledge once the wallet debit succeeded. It
//!   answers [`BountyError::StalePlan`] when the ledger changed between the two calls, so a
//!   caller that debits between them never overwrites a concurrent placement.
//! * [`BountyLedger::settle_arrest`] pays every unpaid pledge of one target to the arresting
//!   player in one step: it marks them paid and returns the total. A repeated call pays nothing,
//!   so a replayed death cannot pay twice.
//!
//! Rules that the 1.2 evidence does not settle are not invented here: there is no fee, no
//! expiry, no refund, no account-level restriction and no minimum / maximum. A pledge stays
//! until it is paid, even while its target is not wanted. The payer is debited the exact pledge
//! amount in copper; the whole accumulated total goes to the arrester. See `PORTING.md`
//! (BOUNTY-1..BOUNTY-5) for the capture-pending policy decisions.

use std::collections::BTreeMap;
use std::fmt;

/// The canonical identity of a payer or a target: an allocated character id (`0` is never one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BountyActor {
    pub character_id: u32,
}

/// One individual pledge. `paid` is set by [`BountyLedger::settle_arrest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BountyPledge {
    pub id: u64,
    pub payer: BountyActor,
    pub target: BountyActor,
    /// Escrowed copper, strictly positive.
    pub amount: i64,
    /// Unix UTC seconds the pledge was created at.
    pub created: i64,
    /// The pledge left the active total because its target was arrested.
    pub paid: bool,
}

/// A validated, not-yet-committed placement.
///
/// Only [`BountyLedger::prepare_placement`] can build one: every field is private and read
/// through its accessor, so a validated plan cannot be tampered with before the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPlacement {
    generation: u64,
    payer: BountyActor,
    target: BountyActor,
    amount: i64,
    now: i64,
}

impl PreparedPlacement {
    /// The payer whose wallet the caller debits.
    pub fn payer(&self) -> BountyActor {
        self.payer
    }

    /// The wanted target the pledge is placed on.
    pub fn target(&self) -> BountyActor {
        self.target
    }

    /// The escrow amount, also the exact wallet debit.
    pub fn amount(&self) -> i64 {
        self.amount
    }

    /// The caller's captured instant, also the committed pledge's `created`.
    pub fn now(&self) -> i64 {
        self.now
    }
}

/// A whole-ledger snapshot for persistence, in ascending id order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BountySnapshot {
    pub pledges: Vec<BountyPledge>,
}

// ---- errors ----

/// A refused bounty transition or an unusable saved row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BountyError {
    /// The placement amount is not strictly positive.
    InvalidAmount(i64),
    /// An actor identity has a zero character id.
    InvalidActor(&'static str),
    /// Payer and target are the same character.
    SameCharacter,
    /// The caller's canonical wanted eligibility says the target is not wanted.
    TargetNotWanted,
    /// A checked amount, id or generation would leave its range.
    ArithmeticOverflow(&'static str),
    /// The ledger changed between prepare and commit; the caller must roll the debit back.
    StalePlan,
    /// A saved row is invalid or inconsistent with the rest of the snapshot.
    CorruptRow(String),
}

impl fmt::Display for BountyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAmount(amount) => {
                write!(f, "bounty amount must be positive, got {amount}")
            }
            Self::InvalidActor(role) => {
                write!(f, "bounty {role} identity has a zero id")
            }
            Self::SameCharacter => {
                write!(f, "bounty payer and target are the same character")
            }
            Self::TargetNotWanted => write!(f, "bounty target is not wanted"),
            Self::ArithmeticOverflow(field) => write!(f, "bounty {field} overflowed"),
            Self::StalePlan => write!(f, "bounty placement plan is stale"),
            Self::CorruptRow(reason) => write!(f, "corrupt bounty row: {reason}"),
        }
    }
}

impl std::error::Error for BountyError {}

fn valid_actor(actor: BountyActor) -> bool {
    actor.character_id != 0
}

/// The bounty ledger: every pledge and every transition, no I/O.
///
/// Every mutating transition bumps [`BountyLedger::generation`], so an outstanding
/// [`PreparedPlacement`] can be detected as stale. Ids are allocated monotonically from 1, and a
/// restored snapshot continues above its highest id, so a saved id is never reused.
#[derive(Debug, Clone)]
pub struct BountyLedger {
    generation: u64,
    next_pledge_id: u64,
    pledges: BTreeMap<u64, BountyPledge>,
}

impl Default for BountyLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl BountyLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self {
            generation: 0,
            next_pledge_id: 1,
            pledges: BTreeMap::new(),
        }
    }

    /// The change counter: bumped by every mutation, captured by a prepare and compared by the
    /// matching commit.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// One pledge by id.
    pub fn pledge(&self, id: u64) -> Option<&BountyPledge> {
        self.pledges.get(&id)
    }

    /// Drops a pledge whose paid state is already saved. Returns the removed row.
    pub fn remove_pledge(&mut self, id: u64) -> Option<BountyPledge> {
        self.pledges.remove(&id)
    }

    /// Every pledge, ascending id.
    pub fn pledges(&self) -> impl Iterator<Item = &BountyPledge> {
        self.pledges.values()
    }

    /// The displayed active total of one target, saturating instead of wrapping.
    ///
    /// Only unpaid pledges count. The total is the same value `SCSetBountyPermitted` /
    /// `SCSetBountyDone` carry and the payout pays.
    pub fn active_total(&self, target_character_id: u32) -> i64 {
        if target_character_id == 0 {
            return 0;
        }
        self.pledges
            .values()
            .filter(|pledge| !pledge.paid && pledge.target.character_id == target_character_id)
            .fold(0i64, |total, pledge| total.saturating_add(pledge.amount))
    }

    /// Whether the target has at least one unpaid pledge.
    pub fn has_active(&self, target_character_id: u32) -> bool {
        self.active_total(target_character_id) > 0
    }

    // ---- placement ----

    /// Validates one placement and reports the amount the caller has to debit.
    ///
    /// `target_wanted` is the caller's canonical wanted eligibility (the same buff-tag test the
    /// arrest uses) and `now` the caller's captured instant. The caller then debits
    /// [`PreparedPlacement::amount`] and calls [`Self::commit_placement`] without awaiting and
    /// with the same captured `now`. There is no fee: the debit is exactly the pledge.
    pub fn prepare_placement(
        &self,
        payer: BountyActor,
        target: BountyActor,
        amount: i64,
        target_wanted: bool,
        now: i64,
    ) -> Result<PreparedPlacement, BountyError> {
        if !valid_actor(payer) {
            return Err(BountyError::InvalidActor("payer"));
        }
        if !valid_actor(target) {
            return Err(BountyError::InvalidActor("target"));
        }
        if amount <= 0 {
            return Err(BountyError::InvalidAmount(amount));
        }
        if payer.character_id == target.character_id {
            return Err(BountyError::SameCharacter);
        }
        if !target_wanted {
            return Err(BountyError::TargetNotWanted);
        }
        Ok(PreparedPlacement {
            generation: self.generation,
            payer,
            target,
            amount,
            now,
        })
    }

    /// Commits a prepared placement after the caller's wallet debit succeeded.
    ///
    /// Answers [`BountyError::StalePlan`] when the ledger changed since the prepare (another
    /// placement or a settlement) rather than overwriting that change; the caller rolls the
    /// debit back and retries.
    pub fn commit_placement(
        &mut self,
        plan: &PreparedPlacement,
    ) -> Result<BountyPledge, BountyError> {
        if plan.generation != self.generation {
            return Err(BountyError::StalePlan);
        }
        let next_pledge_id = self
            .next_pledge_id
            .checked_add(1)
            .ok_or(BountyError::ArithmeticOverflow("pledge id"))?;
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(BountyError::ArithmeticOverflow("generation"))?;
        let pledge = BountyPledge {
            id: self.next_pledge_id,
            payer: plan.payer,
            target: plan.target,
            amount: plan.amount,
            created: plan.now,
            paid: false,
        };
        self.pledges.insert(pledge.id, pledge.clone());
        self.next_pledge_id = next_pledge_id;
        self.generation = generation;
        Ok(pledge)
    }

    // ---- settlement ----

    /// Pays the arrest of one wanted target: marks every unpaid pledge of that target paid and
    /// returns their saturating total. Nothing is paid when there is no unpaid pledge, so a
    /// repeated call (a replayed death) pays nothing.
    ///
    /// The caller credits the returned total to the arresting player. A pledge is paid in full;
    /// it is not split between several contributors at settlement time (their own pledge rows
    /// stay as the record of who paid).
    pub fn settle_arrest(&mut self, target_character_id: u32) -> Result<i64, BountyError> {
        if target_character_id == 0 {
            return Err(BountyError::InvalidActor("target"));
        }
        let mut total = 0i64;
        let mut paid_any = false;
        for pledge in self.pledges.values_mut() {
            if pledge.paid || pledge.target.character_id != target_character_id {
                continue;
            }
            pledge.paid = true;
            total = total.saturating_add(pledge.amount);
            paid_any = true;
        }
        if paid_any {
            self.generation = self
                .generation
                .checked_add(1)
                .ok_or(BountyError::ArithmeticOverflow("generation"))?;
        }
        Ok(total)
    }

    // ---- persistence ----

    /// The whole ledger as save rows in ascending id order.
    pub fn snapshot(&self) -> BountySnapshot {
        BountySnapshot {
            pledges: self.pledges.values().cloned().collect(),
        }
    }

    /// Replaces the ledger with a restored snapshot, or refuses it whole.
    ///
    /// Every row is validated before anything is applied, so a corrupt snapshot leaves the
    /// ledger untouched. Ids continue above the restored ones and any outstanding
    /// [`PreparedPlacement`] becomes stale.
    pub fn restore(&mut self, snapshot: BountySnapshot) -> Result<(), BountyError> {
        let mut pledges = BTreeMap::new();
        for pledge in snapshot.pledges {
            validate_pledge_row(&pledge)?;
            let id = pledge.id;
            if pledges.insert(id, pledge).is_some() {
                return Err(BountyError::CorruptRow(format!("duplicate pledge id {id}")));
            }
        }
        let next_pledge_id = pledges
            .keys()
            .next_back()
            .map_or(1, |id| id.saturating_add(1));
        if next_pledge_id == 0 {
            return Err(BountyError::CorruptRow("pledge ids are exhausted".into()));
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(BountyError::ArithmeticOverflow("generation"))?;
        self.pledges = pledges;
        self.next_pledge_id = next_pledge_id;
        self.generation = generation;
        Ok(())
    }
}

/// Validates one restored row. `paid` rows keep their history; an unpaid row must be payable.
fn validate_pledge_row(pledge: &BountyPledge) -> Result<(), BountyError> {
    if pledge.id == 0 {
        return Err(BountyError::CorruptRow("pledge id 0 is not valid".into()));
    }
    if !valid_actor(pledge.payer) || !valid_actor(pledge.target) {
        return Err(BountyError::CorruptRow(format!(
            "pledge {} has a zero actor identity",
            pledge.id
        )));
    }
    if pledge.payer.character_id == pledge.target.character_id {
        return Err(BountyError::CorruptRow(format!(
            "pledge {} is a self bounty",
            pledge.id
        )));
    }
    if pledge.amount <= 0 {
        return Err(BountyError::CorruptRow(format!(
            "pledge {} has amount {}",
            pledge.id, pledge.amount
        )));
    }
    if pledge.created < 0 {
        return Err(BountyError::CorruptRow(format!(
            "pledge {} has a negative creation time",
            pledge.id
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(id: u32) -> BountyActor {
        BountyActor { character_id: id }
    }

    fn place(ledger: &mut BountyLedger, payer: u32, target: u32, amount: i64) {
        let plan = ledger
            .prepare_placement(actor(payer), actor(target), amount, true, 100)
            .unwrap();
        ledger.commit_placement(&plan).unwrap();
    }

    #[test]
    fn a_default_ledger_starts_at_pledge_id_one() {
        let mut ledger = BountyLedger::default();
        place(&mut ledger, 1, 2, 100);
        assert_eq!(ledger.pledge(1).unwrap().id, 1);
        assert_eq!(ledger.next_pledge_id, 2);
    }

    #[test]
    fn prepare_refuses_invalid_placements() {
        let ledger = BountyLedger::new();
        assert_eq!(
            ledger.prepare_placement(actor(1), actor(2), 0, true, 0),
            Err(BountyError::InvalidAmount(0))
        );
        assert_eq!(
            ledger.prepare_placement(actor(1), actor(2), -5, true, 0),
            Err(BountyError::InvalidAmount(-5))
        );
        assert_eq!(
            ledger.prepare_placement(actor(0), actor(2), 100, true, 0),
            Err(BountyError::InvalidActor("payer"))
        );
        assert_eq!(
            ledger.prepare_placement(actor(1), actor(0), 100, true, 0),
            Err(BountyError::InvalidActor("target"))
        );
        assert_eq!(
            ledger.prepare_placement(actor(1), actor(1), 100, true, 0),
            Err(BountyError::SameCharacter)
        );
        assert_eq!(
            ledger.prepare_placement(actor(1), actor(2), 100, false, 0),
            Err(BountyError::TargetNotWanted)
        );
    }

    #[test]
    fn pledges_stack_per_target_and_are_not_fee_adjusted() {
        let mut ledger = BountyLedger::new();
        place(&mut ledger, 1, 2, 100);
        place(&mut ledger, 3, 2, 250);
        assert_eq!(ledger.active_total(2), 350);
        assert_eq!(ledger.active_total(9), 0);
        let first = ledger.pledge(1).unwrap();
        assert_eq!(
            (first.payer, first.target, first.amount),
            (actor(1), actor(2), 100)
        );
        assert!(!first.paid);
    }

    #[test]
    fn settle_pays_once_and_resets_the_total() {
        let mut ledger = BountyLedger::new();
        place(&mut ledger, 1, 2, 100);
        place(&mut ledger, 3, 2, 250);
        assert_eq!(ledger.settle_arrest(2), Ok(350));
        assert_eq!(ledger.active_total(2), 0);
        assert!(ledger.pledges().all(|p| p.paid));
        // A second settlement of the same arrest pays nothing.
        assert_eq!(ledger.settle_arrest(2), Ok(0));
    }

    #[test]
    fn settle_of_one_target_keeps_another() {
        let mut ledger = BountyLedger::new();
        place(&mut ledger, 1, 2, 100);
        place(&mut ledger, 1, 3, 400);
        assert_eq!(ledger.settle_arrest(2), Ok(100));
        assert_eq!(ledger.active_total(2), 0);
        assert_eq!(ledger.active_total(3), 400);
    }

    #[test]
    fn a_stale_plan_never_overwrites_a_concurrent_placement() {
        let mut ledger = BountyLedger::new();
        let plan = ledger
            .prepare_placement(actor(1), actor(2), 100, true, 10)
            .unwrap();
        // Another placement lands between the caller's prepare and commit.
        place(&mut ledger, 3, 2, 50);
        assert_eq!(ledger.commit_placement(&plan), Err(BountyError::StalePlan));
        assert_eq!(ledger.active_total(2), 50);
    }

    #[test]
    fn snapshot_round_trip_keeps_paid_rows_and_continues_ids() {
        let mut ledger = BountyLedger::new();
        place(&mut ledger, 1, 2, 100);
        place(&mut ledger, 3, 4, 200);
        ledger.settle_arrest(2).unwrap();
        let snapshot = ledger.snapshot();

        let mut restored = BountyLedger::new();
        restored.restore(snapshot.clone()).unwrap();
        assert_eq!(restored.snapshot(), snapshot);
        assert_eq!(restored.active_total(2), 0);
        assert_eq!(restored.active_total(4), 200);
        // A new pledge after the restore never reuses a saved id.
        place(&mut restored, 5, 4, 25);
        assert_eq!(restored.pledge(3).unwrap().amount, 25);
    }

    #[test]
    fn restore_refuses_a_corrupt_snapshot_whole() {
        let mut ledger = BountyLedger::new();
        place(&mut ledger, 1, 2, 100);
        let before = ledger.snapshot();

        let mut duplicate = before.clone();
        duplicate.pledges.push(before.pledges[0].clone());
        assert!(matches!(
            ledger.restore(duplicate),
            Err(BountyError::CorruptRow(_))
        ));

        let mut self_bounty = before.clone();
        self_bounty.pledges[0].target = self_bounty.pledges[0].payer;
        assert!(matches!(
            ledger.restore(self_bounty),
            Err(BountyError::CorruptRow(_))
        ));

        let mut zero = before.clone();
        zero.pledges[0].amount = 0;
        assert!(matches!(
            ledger.restore(zero),
            Err(BountyError::CorruptRow(_))
        ));
        assert_eq!(ledger.snapshot(), before);
    }

    #[test]
    fn a_target_keeps_the_bounty_while_not_wanted() {
        let mut ledger = BountyLedger::new();
        place(&mut ledger, 1, 2, 100);
        // The wanted buff lapses; the pledge stays payable until the target is arrested. A new
        // placement is refused while the target is not wanted, but the stored total survives.
        assert!(ledger
            .prepare_placement(actor(3), actor(2), 100, false, 200)
            .is_err());
        assert_eq!(ledger.active_total(2), 100);
        assert_eq!(ledger.settle_arrest(2), Ok(100));
    }
}
