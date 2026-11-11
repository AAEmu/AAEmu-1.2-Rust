//! Siege hostility wiring: the rule runs through the real `skills::relation::relation_to` /
//! `can_attack`, the real authority, a live lodestone and the real skill effect path
//! (`skill::use_skill`). The relation tests fail when the rule is removed from `relation_to`;
//! the skill flow tests fail when it only guards `can_attack`.

use std::sync::Arc;

use super::hostility::{classify, Party, SiegeRelation};
use super::runtime;
use super::siege_fixtures::{join_expedition, leave_expedition};
use super::state::SiegeSide;
use crate::data::world_data::{conflict_type, FactionRelation, SystemFaction, Zone, ZoneConflict};
use crate::data::GameData;
use crate::models::house::House;
use crate::server::GameServer;
use crate::systems::housing::data::{GuardTowerSetting, HousingTemplate};
use crate::systems::instances::data::IndunZone;
use crate::systems::skills::data::effects::EffectTemplate;
use crate::systems::skills::data::{BuffTemplate, EffectKind, SkillTemplate};
use crate::systems::skills::geometry::Target;
use crate::systems::skills::relation::{can_attack, relation_to};
use crate::systems::skills::skill::use_skill;
use crate::systems::skills::state::SkillRun;
use crate::systems::skills::tests::{damage_effect, skill_effect, skill_template};
use crate::systems::skills::types::{RelationState, SkillCastTarget, SkillCaster, SkillObject};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef, WorldObject};

const GROUP: u32 = 25;
const ATTACKER_EXPEDITION: u32 = 20;
const DEFENDER_EXPEDITION: u32 = 10;
const TOWER_X: f32 = 105.0;
const TOWER_Y: f32 = 100.0;
const INSIDE_X: f32 = 100.0;
const INSIDE_Y: f32 = 100.0;
const OUTSIDE_X: f32 = 600.0;
const OUTSIDE_Y: f32 = 100.0;
const RETRIBUTION: u32 = 2_167;
const DAMAGE_SKILL: u32 = 10_001;
const DAMAGE_EFFECT: u32 = 900_001;

struct Fixture {
    server: Arc<GameServer>,
    skill: Arc<SkillTemplate>,
    attacker: PlayerRef,
    defender: PlayerRef,
    same_party: PlayerRef,
    outsider: PlayerRef,
}

fn fixture(active: bool, no_pvp: bool) -> Fixture {
    let mut data = GameData::empty();
    // A damage skill like the real ones: a hostile-only effect that checks the crime rule.
    data.systems.skills.add_buff(BuffTemplate {
        id: RETRIBUTION,
        kind: 2,
        duration: 60_000,
        stack_rule: 1,
        ..Default::default()
    });
    let mut effect = damage_effect(1, 600);
    effect.check_crime = true;
    data.systems.skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(effect)),
        DAMAGE_EFFECT,
    );
    let mut skill = skill_template(DAMAGE_SKILL);
    let mut skill_damage = skill_effect(DAMAGE_EFFECT, EffectKind::DamageEffect, 1);
    skill_damage.non_friendly = true;
    skill.effects.push(skill_damage);
    let skill = Arc::new(skill);
    data.systems.skills.add_skill((*skill).clone());
    crate::test_support::open_client_skills(&mut data);
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            zone_key: TEST_ZONE,
            group_id: GROUP,
            ..Default::default()
        },
    );
    for id in [101, 109] {
        data.factions.add(SystemFaction {
            id,
            mother_id: 0,
            ..Default::default()
        });
    }
    // The same expedition can hold characters of two factions; the faction relation must not
    // decide a siege party pair.
    data.factions.relations.push(FactionRelation {
        id: 101,
        id2: 109,
        state: RelationState::Hostile as u8,
        exp_time: 0,
    });
    data.systems.housing.add_template(HousingTemplate {
        id: 139,
        guard_tower_setting_id: 6,
        ..Default::default()
    });
    data.systems.housing.castles.guard_tower_settings.insert(
        6,
        GuardTowerSetting {
            id: 6,
            radius_siege: 250,
            radius_offense_hq: 100,
            ..Default::default()
        },
    );
    if no_pvp {
        data.systems.instances.indun.zones.insert(
            GROUP,
            IndunZone {
                zone_group_id: GROUP,
                pvp: false,
                ..IndunZone::default()
            },
        );
    }
    let server = Arc::new(test_server_with(data));
    crate::systems::skills::bind(&server);
    let mut attacker_client = TestClient::new(&server, 1, 1001);
    let mut defender_client = TestClient::new(&server, 2, 1002);
    let mut same_party_client = TestClient::new(&server, 3, 1003);
    let mut outsider_client = TestClient::new(&server, 4, 1004);
    let make_player = |id, name, expedition, faction, x, y, client: &TestClient| {
        let mut c = test_character(id, name, x, y);
        c.unit.faction_id = faction;
        c.unit.hp = 5_000;
        c.unit.max_hp = 5_000;
        c.unit.mp = 1_000;
        c.unit.max_mp = 1_000;
        c.is_online = true;
        let player = spawn_player(&server, client, c, 0x100 + id);
        if expedition != 0 {
            join_expedition(&server, expedition, &player);
        }
        player
    };
    let attacker = make_player(
        1,
        "Attacker",
        ATTACKER_EXPEDITION,
        101,
        INSIDE_X,
        INSIDE_Y,
        &attacker_client,
    );
    let defender = make_player(
        2,
        "Defender",
        DEFENDER_EXPEDITION,
        101,
        INSIDE_X,
        INSIDE_Y,
        &defender_client,
    );
    let same_party = make_player(
        3,
        "SameParty",
        ATTACKER_EXPEDITION,
        109,
        INSIDE_X,
        INSIDE_Y,
        &same_party_client,
    );
    let outsider = make_player(4, "Outsider", 0, 101, INSIDE_X, INSIDE_Y, &outsider_client);
    let mut house = House::new(61, 139);
    house.current_step = -1;
    house.unit.transform.zone_id = TEST_ZONE;
    house.unit.transform.set_xyz(TOWER_X, TOWER_Y, 100.0);
    let tower = Entity::new(0x200, house);
    server.systems.housing.manager.insert(&tower);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&tower, &server.view_ctx());
    let (_, identity) = runtime::tower_identity(&server, &tower).unwrap();
    let now = chrono::Utc::now().timestamp();
    server
        .systems
        .housing
        .dominions
        .declare_unowned_with(GROUP, identity, DEFENDER_EXPEDITION, now - 1000, || true)
        .unwrap();
    let (starts_at, ends_at, scheduled_at) = if active {
        (now - 10, now + 3600, now - 20)
    } else {
        (now + 600, now + 3600, now - 20)
    };
    server
        .systems
        .housing
        .dominions
        .schedule_siege(GROUP, ATTACKER_EXPEDITION, starts_at, ends_at, scheduled_at)
        .unwrap();
    // The roster: both attacker-guild members and the defender. The outsider is on no roster.
    for (character_id, expedition, side) in [
        (1, ATTACKER_EXPEDITION, SiegeSide::Attacker),
        (2, DEFENDER_EXPEDITION, SiegeSide::Defender),
        (3, ATTACKER_EXPEDITION, SiegeSide::Attacker),
    ] {
        server
            .systems
            .housing
            .dominions
            .register_participant(GROUP, character_id, expedition, side, scheduled_at)
            .unwrap();
    }
    super::siege_area::refresh(&server);
    attacker_client.drain();
    defender_client.drain();
    same_party_client.drain();
    outsider_client.drain();
    drop(attacker_client);
    drop(defender_client);
    drop(same_party_client);
    drop(outsider_client);
    Fixture {
        server,
        skill,
        attacker,
        defender,
        same_party,
        outsider,
    }
}

/// A calendar-materialised siege: its window follows the calendar / GM test clock.
fn materialised_fixture(clock: i64) -> Fixture {
    let f = fixture(true, false);
    let now = clock - 100;
    f.server
        .systems
        .housing
        .dominions
        .cancel_siege(GROUP, chrono::Utc::now().timestamp())
        .unwrap();
    let cycle = 1_000i64;
    f.server
        .systems
        .housing
        .dominions
        .declare(GROUP, ATTACKER_EXPEDITION, cycle, now, 0, (0.0, 0.0, 0.0))
        .unwrap();
    f.server
        .systems
        .housing
        .dominions
        .materialise_declaration(
            GROUP,
            cycle,
            clock - 10,
            clock + 3_600,
            now,
            crate::systems::housing::dominion::SiegeLimits::NATIVE,
        )
        .unwrap();
    // The new record starts with an empty roster.
    for (character_id, expedition, side) in [
        (1, ATTACKER_EXPEDITION, SiegeSide::Attacker),
        (2, DEFENDER_EXPEDITION, SiegeSide::Defender),
    ] {
        f.server
            .systems
            .housing
            .dominions
            .register_participant(GROUP, character_id, expedition, side, now)
            .unwrap();
    }
    super::siege_area::refresh(&f.server);
    f
}

fn target(player: &PlayerRef) -> Target {
    Target::Obj(WorldObject::from(player))
}

fn move_to(fixture: &Fixture, player: &PlayerRef, x: f32, y: f32) {
    let object = WorldObject::from(player);
    player.data().unit.transform.set_xyz(x, y, 100.0);
    fixture
        .server
        .world
        .world(0)
        .unwrap()
        .add_visible_object(&object, &fixture.server.view_ctx());
}

#[tokio::test]
async fn attackers_and_defenders_are_hostile_inside_the_running_siege_only() {
    let f = fixture(true, false);
    assert!(
        can_attack(&f.server, &target(&f.attacker), &target(&f.defender)),
        "the attacking and defending roster parties are hostile while the siege runs"
    );
    assert!(can_attack(
        &f.server,
        &target(&f.defender),
        &target(&f.attacker)
    ));
    // Same faction would be friendly without the siege rule.
    assert_eq!(
        crate::systems::skills::relation::faction_relation(&f.server, 101, 101),
        crate::systems::skills::types::RelationState::Friendly
    );

    // A scheduled (warmup) siege does not make them hostile: only the running window does.
    let warmup = fixture(false, false);
    assert!(!can_attack(
        &warmup.server,
        &target(&warmup.attacker),
        &target(&warmup.defender)
    ));
}

#[tokio::test]
async fn the_same_party_is_friendly_inside_and_the_circle_gate_preserves_faction_rules() {
    let f = fixture(true, false);
    // Two attacker-expedition characters of hostile factions: the siege party rule wins.
    assert_eq!(
        crate::systems::skills::relation::faction_relation(&f.server, 101, 109),
        crate::systems::skills::types::RelationState::Hostile
    );
    assert!(!can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.same_party)
    ));
    // Outside the circle the siege rule does not apply; the normal hostile factions decide.
    move_to(&f, &f.same_party, OUTSIDE_X, OUTSIDE_Y);
    assert!(can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.same_party)
    ));
}

#[tokio::test]
async fn the_calendar_clock_arms_the_siege_relation() {
    let clock = chrono::Utc::now().timestamp() + 100_000;
    let f = materialised_fixture(clock);
    f.server
        .systems
        .housing
        .calendar
        .set_clock(Some(clock - 100));
    assert!(
        !can_attack(&f.server, &target(&f.attacker), &target(&f.defender)),
        "before the calendar start"
    );
    f.server
        .systems
        .housing
        .calendar
        .set_clock(Some(clock + 10));
    assert!(
        can_attack(&f.server, &target(&f.attacker), &target(&f.defender)),
        "inside the calendar window"
    );
    f.server.systems.housing.calendar.set_clock(None);
}

#[tokio::test]
async fn a_second_attacker_party_would_be_neutral() {
    assert_eq!(
        classify(
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 20
            },
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 30
            }
        ),
        SiegeRelation::Neutral
    );
    assert_eq!(
        classify(
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 20
            },
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 20
            }
        ),
        SiegeRelation::Friendly
    );
    assert_eq!(
        classify(
            Party {
                side: SiegeSide::Defender,
                expedition_id: 10
            },
            Party {
                side: SiegeSide::Defender,
                expedition_id: 10
            }
        ),
        SiegeRelation::Friendly
    );
    assert_eq!(
        classify(
            Party {
                side: SiegeSide::Defender,
                expedition_id: 10
            },
            Party {
                side: SiegeSide::Attacker,
                expedition_id: 20
            }
        ),
        SiegeRelation::Hostile
    );
}

#[tokio::test]
async fn peace_and_no_pvp_protections_still_refuse_the_siege_attack() {
    let f = fixture(true, false);
    f.server.systems.social.conflicts.init(
        &[ZoneConflict {
            zone_group_id: GROUP as u16,
            current_state: conflict_type::PEACE,
            ..ZoneConflict::default()
        }],
        crate::world::now_ms(),
    );
    assert!(!can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.defender)
    ));

    let no_pvp = fixture(true, true);
    assert!(!can_attack(
        &no_pvp.server,
        &target(&no_pvp.attacker),
        &target(&no_pvp.defender)
    ));
}

#[tokio::test]
async fn a_non_participant_is_not_made_hostile_by_the_siege() {
    let f = fixture(true, false);
    // The outsider shares the defender's faction and is in no siege party.
    assert!(!can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.outsider)
    ));
    // A hostile-faction outsider keeps the normal faction relation (attackable), which proves
    // the siege rule did not neutralise it.
    let hostile_outsider = f.same_party.clone();
    leave_expedition(&f.server, ATTACKER_EXPEDITION, &hostile_outsider);
    assert!(can_attack(
        &f.server,
        &target(&f.defender),
        &target(&hostile_outsider)
    ));
}

#[tokio::test]
async fn an_unregistered_guild_member_is_not_made_hostile() {
    let f = fixture(true, false);
    // The outsider belongs to the defending guild but never joined the roster: the same rule
    // that evicts them (`participants::participant_side`) keeps them out of the siege parties,
    // so the faction rules decide (same faction: friendly).
    join_expedition(&f.server, DEFENDER_EXPEDITION, &f.outsider);
    assert!(!can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.outsider)
    ));
    assert!(!can_attack(
        &f.server,
        &target(&f.outsider),
        &target(&f.attacker)
    ));
}

#[tokio::test]
async fn an_ex_member_with_a_roster_entry_is_not_made_hostile() {
    let f = fixture(true, false);
    assert!(can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.defender)
    ));
    // The attacker leaves the guild; the roster entry stays. Live at once, and after the next
    // snapshot as well.
    leave_expedition(&f.server, ATTACKER_EXPEDITION, &f.attacker);
    assert!(!can_attack(
        &f.server,
        &target(&f.attacker),
        &target(&f.defender)
    ));
    super::siege_area::refresh(&f.server);
    assert!(!can_attack(
        &f.server,
        &target(&f.defender),
        &target(&f.attacker)
    ));
    // Back in the guild they fight again once the snapshot has seen it.
    join_expedition(&f.server, ATTACKER_EXPEDITION, &f.attacker);
    super::siege_area::refresh(&f.server);
    assert!(can_attack(
        &f.server,
        &target(&f.defender),
        &target(&f.attacker)
    ));
}

#[tokio::test]
async fn the_siege_parties_set_the_relation_state_for_every_reader() {
    let f = fixture(true, false);
    let (a, d, s, o) = (
        target(&f.attacker),
        target(&f.defender),
        target(&f.same_party),
        target(&f.outsider),
    );
    // Same faction, opposite sides: Hostile although the faction relation says Friendly.
    assert_eq!(relation_to(&f.server, &a, &d), RelationState::Hostile);
    assert_eq!(relation_to(&f.server, &d, &a), RelationState::Hostile);
    // One party of two hostile factions: Friendly.
    assert_eq!(relation_to(&f.server, &a, &s), RelationState::Friendly);
    // Not a participant: the faction relation (the same faction, friendly) is unchanged.
    assert_eq!(relation_to(&f.server, &a, &o), RelationState::Friendly);
    // Outside the circle the faction relation decides again.
    move_to(&f, &f.defender, OUTSIDE_X, OUTSIDE_Y);
    assert_eq!(relation_to(&f.server, &a, &d), RelationState::Friendly);
    // A warmup record changes nothing.
    let warmup = fixture(false, false);
    assert_eq!(
        relation_to(
            &warmup.server,
            &target(&warmup.attacker),
            &target(&warmup.defender)
        ),
        RelationState::Friendly
    );
}

fn cast(f: &Fixture, caster: &PlayerRef, victim: &PlayerRef) {
    use_skill(
        &f.server,
        &SkillRun::new(f.skill.clone(), 1),
        &WorldObject::from(caster),
        SkillCaster::Unit(caster.obj_id),
        &mut SkillCastTarget::Unit(victim.obj_id),
        Some(SkillObject::None),
        true,
    );
}

/// No Retribution buff, no assault record, no crime points: the attacker was not flagged.
fn unflagged(player: &PlayerRef) -> bool {
    let c = player.data();
    !c.unit.buffs.check_buff(RETRIBUTION)
        && c.skill_char.assaulted_by.is_empty()
        && c.crime_point == 0
}

#[tokio::test]
async fn siege_enemies_of_one_faction_damage_each_other_with_a_skill_and_commit_no_crime() {
    // The effect path reads the faction relation of the pair: two guilds of one nation are
    // Friendly there, so a hostile-only damage effect used to be skipped without Ctrl+F, and
    // with Ctrl+F the hit flagged the attacker as a criminal.
    let f = fixture(true, false);
    let hp = f.defender.data().unit.hp;
    cast(&f, &f.attacker, &f.defender);
    assert!(
        f.defender.data().unit.hp < hp,
        "the damage effect lands on a siege enemy without force attack"
    );
    assert!(
        unflagged(&f.attacker),
        "an enemy is no friendly-fire victim"
    );

    let f = fixture(true, false);
    f.attacker.data().unit.force_attack = true;
    let hp = f.defender.data().unit.hp;
    cast(&f, &f.attacker, &f.defender);
    assert!(f.defender.data().unit.hp < hp);
    assert!(
        unflagged(&f.attacker),
        "Ctrl+F makes no criminal of the attacker"
    );
}

#[tokio::test]
async fn a_siege_kill_counts_as_a_pvp_kill() {
    // `Character.DoDie` withholds the PvP bookkeeping from a kill of a Friendly relation.
    let f = fixture(true, false);
    f.defender.data().unit.hp = 100;
    cast(&f, &f.attacker, &f.defender);
    assert_eq!(f.defender.data().unit.hp, 0, "the defender died");
    assert_eq!(f.attacker.data().hostile_faction_kills, 1);
    assert!(f.defender.data().died_in_pvp);
}

#[tokio::test]
async fn a_party_member_is_not_hurt_by_a_party_member_even_of_a_hostile_faction() {
    let f = fixture(true, false);
    let hp = f.same_party.data().unit.hp;
    cast(&f, &f.attacker, &f.same_party);
    assert_eq!(f.same_party.data().unit.hp, hp);
    // Outside the circle the hostile factions hurt each other again.
    let f = fixture(true, false);
    for player in [&f.attacker, &f.same_party] {
        move_to(&f, player, OUTSIDE_X, OUTSIDE_Y);
    }
    let hp = f.same_party.data().unit.hp;
    cast(&f, &f.attacker, &f.same_party);
    assert!(f.same_party.data().unit.hp < hp);
}

#[tokio::test]
async fn skills_between_one_faction_stay_harmless_outside_the_circle_and_before_the_siege() {
    // Both stand outside the circle, in reach of each other: the faction relation decides.
    let f = fixture(true, false);
    for player in [&f.attacker, &f.defender] {
        move_to(&f, player, OUTSIDE_X, OUTSIDE_Y);
    }
    let hp = f.defender.data().unit.hp;
    cast(&f, &f.attacker, &f.defender);
    assert_eq!(f.defender.data().unit.hp, hp, "outside the siege circle");

    let warmup = fixture(false, false);
    let hp = warmup.defender.data().unit.hp;
    cast(&warmup, &warmup.attacker, &warmup.defender);
    assert_eq!(
        warmup.defender.data().unit.hp,
        hp,
        "the siege is not running"
    );
}

#[tokio::test]
async fn peace_and_no_pvp_still_refuse_the_siege_skill() {
    let f = fixture(true, false);
    f.server.systems.social.conflicts.init(
        &[ZoneConflict {
            zone_group_id: GROUP as u16,
            current_state: conflict_type::PEACE,
            ..ZoneConflict::default()
        }],
        crate::world::now_ms(),
    );
    let hp = f.defender.data().unit.hp;
    cast(&f, &f.attacker, &f.defender);
    assert_eq!(f.defender.data().unit.hp, hp, "Peace is a lockdown");

    let f = fixture(true, true);
    let hp = f.defender.data().unit.hp;
    cast(&f, &f.attacker, &f.defender);
    assert_eq!(f.defender.data().unit.hp, hp, "no-PvP instance");
}
