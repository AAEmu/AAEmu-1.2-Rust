//! Authority-level tests of the WP-SD2a declaration state.

use super::state::*;

const ZONE: u32 = 33;
const DEFENDER: u32 = 10;
const ATTACKER: u32 = 20;
const OTHER: u32 = 30;
const CYCLE: i64 = 1_000;

fn tower() -> TowerIdentity {
    TowerIdentity {
        house_id: 61,
        instance_id: 0,
        guard_tower_setting_id: 6,
    }
}

fn owned() -> DominionManager {
    let manager = DominionManager::default();
    manager
        .declare_unowned_with(ZONE, tower(), DEFENDER, 500, || true)
        .unwrap();
    manager
}

fn declare(manager: &DominionManager, expedition: u32) -> Result<DominionState, DominionError> {
    manager.declare(ZONE, expedition, CYCLE, 1_500, 0x1234, (10.0, 20.0, 30.0))
}

#[test]
fn second_declaration_is_refused_for_self_and_other_expeditions() {
    let manager = owned();
    let declared = declare(&manager, ATTACKER).unwrap();
    assert_eq!(
        declared.declaration.map(|d| d.attacker_expedition_id),
        Some(ATTACKER)
    );
    assert_eq!(
        declare(&manager, ATTACKER),
        Err(DominionError::AlreadyDeclared)
    );
    assert_eq!(
        declare(&manager, OTHER),
        Err(DominionError::DeclaredByOther)
    );
    assert_eq!(
        declare(&manager, DEFENDER),
        Err(DominionError::InvalidIdentity)
    );
}

#[test]
fn cancel_clears_only_the_declaring_expedition() {
    let manager = owned();
    declare(&manager, ATTACKER).unwrap();
    assert_eq!(
        manager.cancel_declaration(ZONE, OTHER),
        Err(DominionError::NotDeclared)
    );
    let cancelled = manager.cancel_declaration(ZONE, ATTACKER).unwrap();
    assert_eq!(cancelled.declaration, None);
    assert_eq!(
        manager.cancel_declaration(ZONE, ATTACKER),
        Err(DominionError::NotDeclared)
    );
}

#[test]
fn materialisation_creates_exactly_one_siege_with_the_cycle_times() {
    let manager = owned();
    declare(&manager, ATTACKER).unwrap();
    // A different cycle cannot consume the declaration.
    assert_eq!(
        manager
            .materialise_declaration(ZONE, CYCLE + 7, 2_000, 3_000, 1_500, SiegeLimits::NATIVE)
            .unwrap(),
        None
    );
    assert!(manager.state(ZONE).unwrap().declaration.is_some());

    let materialised = manager
        .materialise_declaration(
            ZONE,
            CYCLE,
            2_000,
            3_000,
            1_500,
            SiegeLimits { ni: 50, nr: 70 },
        )
        .unwrap()
        .expect("declaration materialised");
    let siege = materialised.siege.as_ref().unwrap();
    assert_eq!((siege.starts_at, siege.ends_at), (2_000, 3_000));
    assert_eq!(
        (siege.attacker_expedition_id, siege.defender_expedition_id),
        (ATTACKER, DEFENDER)
    );
    assert_eq!((siege.defender_ni, siege.defender_nr), (50, 70));
    assert!(siege.materialised_from_declaration);
    assert_eq!(siege.calendar_cycle, CYCLE);
    assert_eq!(materialised.declaration, None);
    assert_eq!(materialised.declared_cycle(), Some(CYCLE));

    // A repeated tick must not create a second record or resurrect the declaration.
    assert_eq!(
        manager
            .materialise_declaration(ZONE, CYCLE, 2_000, 3_000, 1_500, SiegeLimits::NATIVE)
            .unwrap(),
        None
    );
    assert_eq!(
        manager.state(ZONE).unwrap().siege.unwrap().siege_id,
        siege.siege_id
    );
    // A finished declared siege no longer describes a cycle.
    manager
        .schedule_siege(ZONE, ATTACKER, 4_000, 5_000, 3_500)
        .unwrap_err();
    manager.cancel_siege(ZONE, 1_500).unwrap();
    assert_eq!(manager.state(ZONE).unwrap().declared_cycle(), None);
}

#[test]
fn a_declaration_requires_an_owned_territory_and_never_coexists_with_a_live_siege() {
    let mut unowned = DominionState::unowned(ZONE, tower());
    unowned.declaration = Some(Declaration {
        attacker_expedition_id: ATTACKER,
        declared_at: 1_500,
        cycle_auction_start: CYCLE,
        hq_obj_id: 1,
        hq_x: 0.0,
        hq_y: 0.0,
        hq_z: 0.0,
    });
    assert_eq!(unowned.validate(), Err(DominionError::InvalidState));

    let manager = owned();
    manager
        .schedule_siege(ZONE, ATTACKER, 2_000, 3_000, 1_500)
        .unwrap();
    assert_eq!(
        declare(&manager, OTHER),
        Err(DominionError::SiegeInProgress)
    );
    let mut state = manager.state(ZONE).unwrap();
    {
        let siege = state.siege.as_mut().unwrap();
        siege.status = SiegeStatus::Finished;
        siege.outcome = SiegeOutcome::Cancelled;
        siege.ended_at = siege.scheduled_at;
    }
    assert_eq!(state.validate(), Ok(()));
}

#[test]
fn release_expedition_drops_a_declaration_it_attacked_or_accepted() {
    let manager = owned();
    declare(&manager, ATTACKER).unwrap();
    let released = manager.release_expedition(ATTACKER, 2_000);
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].1.declaration, None);
    assert_eq!(manager.state(ZONE).unwrap().owner_expedition_id, DEFENDER);

    let manager = owned();
    declare(&manager, ATTACKER).unwrap();
    let released = manager.release_expedition(DEFENDER, 2_000);
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].1.declaration, None);
}
