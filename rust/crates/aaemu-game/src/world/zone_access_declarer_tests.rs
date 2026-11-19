//! `Housing.OpenCastleZonesForDeclarers`: the declare-period exemption for the camp-casting
//! captain. The fixture gives the attacker guild (leader with the siege-master policy) a camp
//! item for the castle's zone group while the weekly calendar runs the declare period.

use std::sync::Arc;

use chrono::{FixedOffset, NaiveDate, TimeZone as _};

use crate::data::items::ItemTemplate;
use crate::data::world_data::{SystemFaction, Zone};
use crate::data::world_templates::{WorldTemplate, WorldTemplates};
use crate::data::GameData;
use crate::models::container::slot_type;
use crate::models::house::House;
use crate::models::item::{Item, ITEM_FLAG_SECURE};
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::housing::data::{HousingTemplate, SiegePlan, SiegeZone};
use crate::systems::housing::dominion::calendar::{CalendarTimeZone, SiegeCalendar};
use crate::systems::housing::dominion::{declarer_access, runtime, SiegeSide};
use crate::systems::social::expedition::{member_from_character, Expedition, ExpeditionRolePolicy};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef};

use super::{advance, walk, warning_count, zone_access};

const CASTLE_ZONE: u32 = 300;
const OTHER_CLOSED_ZONE: u32 = 301;
const GROUP: u32 = 55;
const OTHER_GROUP: u32 = 56;
const ATTACKER_EXPEDITION: u32 = 20;
const RIVAL_EXPEDITION: u32 = 30;
const DEFENDER_EXPEDITION: u32 = 10;
const CASTLE_X: f32 = 1400.0;
const OTHER_X: f32 = 1800.0;
const CAMP_ITEM: u32 = 21_130;
const OTHER_CAMP_ITEM: u32 = 21_134;
/// A declare item row whose template does not carry the declare skill.
const NOT_A_CAMP_ITEM: u32 = 21_140;
const DECLARE_SKILL: u32 = 16_918;

fn kst(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    FixedOffset::east_opt(9 * 3600)
        .unwrap()
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .unwrap()
        .timestamp()
}

/// Friday 22:31 of the shipped weekly rotation (the declare period opens 22:30).
fn declare_time() -> i64 {
    kst(2014, 1, 10, 22, 31)
}

/// Thursday 22:31 (the auction period).
fn auction_time() -> i64 {
    kst(2014, 1, 9, 22, 31)
}

/// Sunday 20:45: warmup, which without a declaration the calendar shows as peace.
fn warmup_time() -> i64 {
    kst(2014, 1, 12, 20, 45)
}

fn schedule(group: u32, item: u32) -> SiegeZone {
    SiegeZone {
        id: group,
        zone_group_id: group,
        start_siege_weekday: 0,
        start_siege_hour: 21,
        start_siege_min: 0,
        siege_days: 0,
        siege_hours: 1,
        siege_mins: 30,
        start_auction_weekday: 3,
        start_auction_hour: 22,
        start_auction_min: 30,
        start_declare_weekday: 5,
        start_declare_hour: 22,
        start_declare_min: 30,
        start_warmup_weekday: 0,
        start_warmup_hour: 20,
        start_warmup_min: 30,
        pay_weekday: 0,
        pay_hour: 23,
        pay_min: 55,
        declare_item_id: item,
        reinforce_defense_delay_mins: 20,
        open_weekday: 0,
        open_hour: 0,
        open_duration_hours: 168,
        ..Default::default()
    }
}

struct Fixture {
    server: Arc<GameServer>,
    captain: PlayerRef,
    owner: PlayerRef,
    stranger: PlayerRef,
    captain_client: TestClient,
    owner_client: TestClient,
    stranger_client: TestClient,
}

fn fixture(switch: bool) -> Fixture {
    fixture_with(switch, CAMP_ITEM)
}

/// `switch`: `Housing.OpenCastleZonesForDeclarers`; `declare_item`: the castle group's
/// `siege_zones.declare_item_id`.
fn fixture_with(switch: bool, declare_item: u32) -> Fixture {
    let mut data = GameData::empty();
    for (key, closed, group) in [
        (0, false, 0),
        (TEST_ZONE, false, GROUP),
        (CASTLE_ZONE, true, GROUP),
        (OTHER_CLOSED_ZONE, true, OTHER_GROUP),
    ] {
        data.zones.zones.insert(
            key,
            Zone {
                zone_key: key,
                closed,
                group_id: group,
                ..Default::default()
            },
        );
    }
    data.systems.housing.add_template(HousingTemplate {
        id: 139,
        guard_tower_setting_id: 6,
        ..Default::default()
    });
    for (group, item) in [(GROUP, declare_item), (OTHER_GROUP, OTHER_CAMP_ITEM)] {
        data.systems
            .housing
            .castles
            .siege_zones
            .insert(group, schedule(group, item));
        // A zone in all three rotation slots runs a cycle every week.
        let rows = data
            .systems
            .housing
            .castles
            .siege_plans
            .entry(group)
            .or_default();
        for (id, day) in [5, 12, 19].into_iter().enumerate() {
            rows.push(SiegePlan {
                id: id as u32,
                zone_group_id: group,
                week_start: NaiveDate::from_ymd_opt(2014, 1, day)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            });
        }
    }
    for (id, skill) in [
        (CAMP_ITEM, DECLARE_SKILL),
        (OTHER_CAMP_ITEM, DECLARE_SKILL),
        (NOT_A_CAMP_ITEM, 0),
    ] {
        data.items.templates.insert(
            id,
            ItemTemplate {
                use_skill_id: skill,
                ..ItemTemplate::plain(id, 1)
            },
        );
    }
    let mut server = test_server_with(data);
    server.config.update(|c| {
        c.world.move_guard_enabled = false;
        c.housing.siege_calendar = Some(true);
        c.housing.siege_calendar_timezone = "+09:00".into();
        c.housing.open_castle_zones_for_declarers = switch;
    });
    let mut template = WorldTemplate::synthetic(0, "castle_world", 2, 2, TEST_ZONE);
    for x in 16..32 {
        for y in 0..32 {
            template.set_zone_key_at(
                x,
                y,
                if x < 24 {
                    CASTLE_ZONE
                } else {
                    OTHER_CLOSED_ZONE
                },
            );
        }
    }
    Arc::get_mut(&mut server.game_data).unwrap().worlds =
        WorldTemplates::from_templates(vec![template], vec![]).unwrap();
    server.world = Arc::new(crate::world::WorldManager::default());
    server.world.create_world_instance(
        Arc::new(
            server
                .game_data
                .worlds
                .get_world_template_by_name("castle_world")
                .unwrap()
                .clone(),
        ),
        0,
    );
    let server = Arc::new(server);
    server.set_self();
    crate::systems::npcs::install_hooks(&server);
    server
        .systems
        .housing
        .calendar
        .set_clock(Some(declare_time()));

    let mut captain_client = TestClient::new(&server, 1, 1001);
    let mut owner_client = TestClient::new(&server, 2, 1002);
    let mut stranger_client = TestClient::new(&server, 3, 1003);
    let make = |id: u32, name: &str, expedition: u32, client: &TestClient| {
        let mut c = test_character(id, name, 100.0, 100.0);
        c.is_online = true;
        c.unit.hp = 100;
        c.unit.max_hp = 100;
        c.unit.expedition_id = expedition;
        spawn_player(&server, client, c, 0x100 + id)
    };
    let captain = make(1, "Captain", ATTACKER_EXPEDITION, &captain_client);
    let owner = make(2, "Owner", DEFENDER_EXPEDITION, &owner_client);
    let stranger = make(3, "Stranger", 0, &stranger_client);
    captain_client.conn.active_char = Some(captain.clone());
    owner_client.conn.active_char = Some(owner.clone());
    stranger_client.conn.active_char = Some(stranger.clone());
    for (expedition, p) in [
        (ATTACKER_EXPEDITION, &captain),
        (DEFENDER_EXPEDITION, &owner),
    ] {
        server.expeditions.add(Expedition {
            faction: SystemFaction {
                id: expedition,
                owner_id: p.character_id,
                ..Default::default()
            },
            members: vec![member_from_character(&server, expedition, p, true)],
            policies: Vec::new(),
            removed_members: Vec::new(),
            is_disbanded: false,
        });
        set_siege_master(&server, expedition, true);
    }

    let mut house = House::new(61, 139);
    house.current_step = -1;
    house.unit.transform.zone_id = TEST_ZONE;
    house.unit.transform.set_xyz(105.0, 100.0, 100.0);
    let tower = Entity::new(0x200, house);
    server.systems.housing.manager.insert(&tower);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&tower, &server.view_ctx());
    let (_, identity) = runtime::tower_identity(&server, &tower).unwrap();
    server
        .systems
        .housing
        .dominions
        .declare_unowned_with(
            GROUP,
            identity,
            DEFENDER_EXPEDITION,
            chrono::Utc::now().timestamp() - 1000,
            || true,
        )
        .unwrap();
    captain_client.drain();
    owner_client.drain();
    stranger_client.drain();
    Fixture {
        server,
        captain,
        owner,
        stranger,
        captain_client,
        owner_client,
        stranger_client,
    }
}

/// Replaces the guild's single role-255 policy with one that has `siege_master` as given.
fn set_siege_master(server: &GameServer, expedition: u32, siege_master: bool) {
    server.expeditions.with_mut(expedition, |guild| {
        guild.policies.clear();
        guild.policies.push(ExpeditionRolePolicy {
            expedition_id: expedition,
            role: 255,
            name: "master".into(),
            dominion_declare: true,
            invite: true,
            expel: true,
            promote: true,
            dismiss: true,
            chat: true,
            manager_chat: true,
            siege_master,
            join_siege: true,
        });
    });
}

fn set_clock(f: &Fixture, now: i64) {
    f.server.systems.housing.calendar.set_clock(Some(now));
}

fn give(f: &Fixture, player: &PlayerRef, template: u32, st: u8, expire_at: i64) -> u64 {
    let id = u64::from(f.server.ids.item.get_next_id().unwrap());
    let mut item = Item::create(
        id,
        f.server.game_data.items.template(template).unwrap(),
        1,
        0,
        1,
        &f.server.game_data.items,
        0,
    );
    item.owner_id = u64::from(player.character_id);
    item.slot_type = st;
    item.expiration_time = expire_at;
    let container = Inv::of(player).get(st).unwrap();
    assert!(container
        .lock()
        .unwrap()
        .add_new_item(item, 0, &f.server.game_data.items)
        .unwrap());
    f.server.items.add_container(container);
    id
}

fn give_camp(f: &Fixture, player: &PlayerRef) -> u64 {
    give(f, player, CAMP_ITEM, slot_type::INVENTORY, 0)
}

fn take(player: &PlayerRef, item_id: u64) {
    Inv::of(player)
        .get(slot_type::INVENTORY)
        .unwrap()
        .lock()
        .unwrap()
        .remove_item(item_id)
        .unwrap();
}

fn cycle(f: &Fixture, now: i64) -> i64 {
    SiegeCalendar::from_castles(&f.server.game_data.systems.housing.castles)
        .view_for_declaration(
            GROUP,
            now,
            &CalendarTimeZone::Fixed(FixedOffset::east_opt(9 * 3600).unwrap()),
            None,
            0,
        )
        .unwrap()
        .cycle
        .unwrap()
        .auction_start
}

fn declare(f: &Fixture, expedition: u32) {
    f.server
        .systems
        .housing
        .dominions
        .declare(
            GROUP,
            expedition,
            cycle(f, declare_time()),
            declare_time(),
            0,
            (CASTLE_X, 100.0, 100.0),
        )
        .unwrap();
}

/// Walks the captain into the castle zone and waits out the cast-length eviction timer (10 s
/// of warnings plus the 200 ms grace); true when it never warned and is still standing there.
async fn stays(f: &mut Fixture) -> bool {
    walk(&f.server, &mut f.captain_client, &f.captain, CASTLE_X).await;
    let warned = warning_count(&mut f.captain_client);
    advance(10_000).await;
    advance(200).await;
    warned == 0 && f.captain.data().unit.transform.x == CASTLE_X
}

/// The ordinary closed-zone result: one warning, then returned out of the zone.
async fn is_evicted(f: &mut Fixture) -> bool {
    walk(&f.server, &mut f.captain_client, &f.captain, CASTLE_X).await;
    let warned = warning_count(&mut f.captain_client);
    advance(10_000).await;
    advance(200).await;
    warned == 1 && f.captain.data().unit.transform.x != CASTLE_X
}

#[tokio::test(start_paused = true)]
async fn camp_holder_outlasts_the_ten_second_cast_during_the_declare_period() {
    let mut f = fixture(true);
    give_camp(&f, &f.captain);
    assert!(stays(&mut f).await);
    assert_eq!(f.captain.data().unit.transform.zone_id, CASTLE_ZONE);
}

#[tokio::test(start_paused = true)]
async fn default_off_still_evicts_the_camp_holder() {
    let mut f = fixture(false);
    give_camp(&f, &f.captain);
    assert!(is_evicted(&mut f).await);
}

#[tokio::test(start_paused = true)]
async fn other_phases_do_not_open_the_zone() {
    for now in [auction_time(), warmup_time()] {
        let mut f = fixture(true);
        set_clock(&f, now);
        give_camp(&f, &f.captain);
        assert!(is_evicted(&mut f).await, "phase at {now}");
    }
}

#[tokio::test(start_paused = true)]
async fn absent_wrong_expired_locked_or_banked_camp_items_open_nothing() {
    // Absent.
    let mut f = fixture(true);
    assert!(is_evicted(&mut f).await, "absent");
    // The camp item of another zone group.
    let mut f = fixture(true);
    give(&f, &f.captain, OTHER_CAMP_ITEM, slot_type::INVENTORY, 0);
    assert!(is_evicted(&mut f).await, "other zone's camp");
    // This group's declare item row, but the template carries no declare skill.
    let mut f = fixture_with(true, NOT_A_CAMP_ITEM);
    give(&f, &f.captain, NOT_A_CAMP_ITEM, slot_type::INVENTORY, 0);
    assert!(is_evicted(&mut f).await, "no declare skill");
    // Expired by its predefined end time.
    let mut f = fixture(true);
    give(
        &f,
        &f.captain,
        CAMP_ITEM,
        slot_type::INVENTORY,
        chrono::Utc::now().timestamp() - 10,
    );
    assert!(is_evicted(&mut f).await, "expired");
    // A locked (secured) stack cannot be used.
    let mut f = fixture(true);
    let id = give_camp(&f, &f.captain);
    Inv::of(&f.captain)
        .get(slot_type::INVENTORY)
        .unwrap()
        .lock()
        .unwrap()
        .items
        .iter_mut()
        .find(|i| i.id == id)
        .unwrap()
        .flags |= ITEM_FLAG_SECURE;
    assert!(is_evicted(&mut f).await, "locked");
    // In the bank: the cast cannot take it from there.
    let mut f = fixture(true);
    give(&f, &f.captain, CAMP_ITEM, slot_type::BANK, 0);
    assert!(is_evicted(&mut f).await, "bank");
    // A timed item that has not run out yet is fine.
    let mut f = fixture(true);
    give(
        &f,
        &f.captain,
        CAMP_ITEM,
        slot_type::INVENTORY,
        chrono::Utc::now().timestamp() + 3600,
    );
    assert!(stays(&mut f).await, "unexpired");
}

#[tokio::test(start_paused = true)]
async fn wrong_role_guild_owner_pirate_or_no_guild_open_nothing() {
    // The rank lacks the siege-master policy.
    let mut f = fixture(true);
    give_camp(&f, &f.captain);
    set_siege_master(&f.server, ATTACKER_EXPEDITION, false);
    assert!(is_evicted(&mut f).await, "rank");
    // The territory owner's own captain.
    let mut f = fixture(true);
    give_camp(&f, &f.owner);
    walk(&f.server, &mut f.owner_client, &f.owner, CASTLE_X).await;
    assert_eq!(warning_count(&mut f.owner_client), 1, "owner");
    settle().await;
    assert_ne!(f.owner.data().unit.transform.x, CASTLE_X, "owner");
    // A pirate guild.
    let mut f = fixture(true);
    give_camp(&f, &f.captain);
    f.server.expeditions.with_mut(ATTACKER_EXPEDITION, |guild| {
        guild.faction.mother_id = crate::data::world_data::factions::PIRATE;
    });
    assert!(is_evicted(&mut f).await, "pirate");
    // No guild at all.
    let mut f = fixture(true);
    give_camp(&f, &f.stranger);
    walk(&f.server, &mut f.stranger_client, &f.stranger, CASTLE_X).await;
    assert_eq!(warning_count(&mut f.stranger_client), 1, "no guild");
    settle().await;
    assert_ne!(f.stranger.data().unit.transform.x, CASTLE_X, "no guild");
    // The unit says guild, the guild manager disagrees (left or was kicked).
    let mut f = fixture(true);
    give_camp(&f, &f.captain);
    f.server.expeditions.with_mut(ATTACKER_EXPEDITION, |guild| {
        guild.members.clear();
    });
    assert!(is_evicted(&mut f).await, "left the guild");
}

#[tokio::test(start_paused = true)]
async fn a_rival_claim_or_an_unfinished_siege_closes_the_window() {
    // Another guild's accepted declaration.
    let mut f = fixture(true);
    give_camp(&f, &f.captain);
    declare(&f, RIVAL_EXPEDITION);
    assert!(is_evicted(&mut f).await, "rival declaration");
    // An unfinished siege on the territory.
    let mut f = fixture(true);
    give_camp(&f, &f.captain);
    let now = chrono::Utc::now().timestamp();
    f.server
        .systems
        .housing
        .dominions
        .schedule_siege(GROUP, RIVAL_EXPEDITION, now + 600, now + 3600, now - 20)
        .unwrap();
    assert!(is_evicted(&mut f).await, "unfinished siege here");
    // The guild already fights an unfinished siege of another territory.
    let mut f = fixture(true);
    give_camp(&f, &f.captain);
    let mut house = House::new(62, 139);
    house.current_step = -1;
    house.unit.transform.zone_id = TEST_ZONE;
    house.unit.transform.set_xyz(110.0, 100.0, 100.0);
    let tower = Entity::new(0x201, house);
    f.server.systems.housing.manager.insert(&tower);
    f.server
        .world
        .world(0)
        .unwrap()
        .spawn(&tower, &f.server.view_ctx());
    let identity = runtime::tower_identity(&f.server, &tower).unwrap().1;
    let now = chrono::Utc::now().timestamp();
    let authority = &f.server.systems.housing.dominions;
    authority
        .declare_unowned_with(OTHER_GROUP, identity, RIVAL_EXPEDITION, now - 1000, || true)
        .unwrap();
    authority
        .schedule_siege(
            OTHER_GROUP,
            ATTACKER_EXPEDITION,
            now + 600,
            now + 3600,
            now - 20,
        )
        .unwrap();
    authority
        .register_participant(
            OTHER_GROUP,
            1,
            ATTACKER_EXPEDITION,
            SiegeSide::Attacker,
            now - 10,
        )
        .unwrap();
    assert!(is_evicted(&mut f).await, "fighting elsewhere");
}

#[tokio::test(start_paused = true)]
async fn the_accepted_declaration_lets_the_admitted_captain_stay_after_the_item_is_taken() {
    let mut f = fixture(true);
    let item = give_camp(&f, &f.captain);
    assert!(stays(&mut f).await);
    // The cast completes: the item is taken, the declaration is recorded.
    take(&f.captain, item);
    declare(&f, ATTACKER_EXPEDITION);
    f.captain_client.drain();
    crate::world::session::recheck_siege_participants(&f.server);
    assert_eq!(warning_count(&mut f.captain_client), 0);
    settle().await;
    assert_eq!(f.captain.data().unit.transform.x, CASTLE_X);
}

#[tokio::test(start_paused = true)]
async fn losing_the_item_without_a_declaration_ends_the_permit_through_the_sweep() {
    let mut f = fixture(true);
    let item = give_camp(&f, &f.captain);
    assert!(stays(&mut f).await);
    take(&f.captain, item);
    f.captain_client.drain();
    crate::world::session::recheck_siege_participants(&f.server);
    assert_eq!(warning_count(&mut f.captain_client), 1);
    settle().await;
    assert_ne!(f.captain.data().unit.transform.x, CASTLE_X);
}

#[tokio::test(start_paused = true)]
async fn declaration_end_cancel_rank_or_roster_loss_end_the_stay_through_the_sweep() {
    // Each case: admitted on an item, the item taken, the guild's own declaration accepted,
    // then the permit's basis disappears.
    for case in ["period ends", "cancelled", "rank removed", "left the guild"] {
        let mut f = fixture(true);
        let item = give_camp(&f, &f.captain);
        assert!(stays(&mut f).await, "{case}");
        take(&f.captain, item);
        declare(&f, ATTACKER_EXPEDITION);
        f.captain_client.drain();
        crate::world::session::recheck_siege_participants(&f.server);
        assert_eq!(warning_count(&mut f.captain_client), 0, "{case}");
        match case {
            "period ends" => set_clock(&f, warmup_time()),
            "cancelled" => {
                f.server
                    .systems
                    .housing
                    .dominions
                    .cancel_declaration(GROUP, ATTACKER_EXPEDITION)
                    .unwrap();
            }
            "rank removed" => set_siege_master(&f.server, ATTACKER_EXPEDITION, false),
            _ => {
                f.server.expeditions.with_mut(ATTACKER_EXPEDITION, |guild| {
                    guild.members.clear();
                });
            }
        }
        crate::world::session::recheck_siege_participants(&f.server);
        assert_eq!(warning_count(&mut f.captain_client), 1, "{case}");
        settle().await;
        assert_ne!(f.captain.data().unit.transform.x, CASTLE_X, "{case}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_declaration_alone_never_admits_and_a_rival_declaration_never_keeps() {
    // The guild's own declaration, but this character was never admitted on a camp item.
    let mut f = fixture(true);
    declare(&f, ATTACKER_EXPEDITION);
    assert!(is_evicted(&mut f).await, "declaration alone");
    // Admitted on an item, but the accepted declaration belongs to another guild.
    let mut f = fixture(true);
    let item = give_camp(&f, &f.captain);
    assert!(stays(&mut f).await);
    take(&f.captain, item);
    declare(&f, RIVAL_EXPEDITION);
    f.captain_client.drain();
    crate::world::session::recheck_siege_participants(&f.server);
    assert_eq!(warning_count(&mut f.captain_client), 1, "rival declaration");
}

#[tokio::test(start_paused = true)]
async fn unrelated_zones_instances_and_players_are_unaffected() {
    let mut f = fixture(true);
    give_camp(&f, &f.captain);
    // A closed zone of another zone group stays closed to the same holder.
    walk(&f.server, &mut f.captain_client, &f.captain, OTHER_X).await;
    assert_eq!(warning_count(&mut f.captain_client), 1);
    settle().await;
    assert_ne!(f.captain.data().unit.transform.x, OTHER_X);
    // A player without a camp item is evicted from the same castle zone.
    walk(&f.server, &mut f.stranger_client, &f.stranger, CASTLE_X).await;
    assert_eq!(warning_count(&mut f.stranger_client), 1);
    settle().await;
    assert_ne!(f.stranger.data().unit.transform.x, CASTLE_X);
    // The tower's own world instance and the exact castle zone are the only ones that count.
    assert!(declarer_access::permit(&f.server, &f.captain, 0, CASTLE_ZONE, None).is_some());
    assert!(declarer_access::permit(&f.server, &f.captain, 7, CASTLE_ZONE, None).is_none());
    assert!(declarer_access::permit(&f.server, &f.captain, 0, OTHER_CLOSED_ZONE, None).is_none());
}

#[tokio::test]
async fn destination_checks_admit_only_a_camp_holder_and_never_on_a_declaration() {
    let f = fixture(true);
    let allowed = |f: &Fixture| {
        zone_access::zone_access_allowed(&f.server, &f.captain, 0, CASTLE_ZONE, CASTLE_X, 100.0)
    };
    assert!(!allowed(&f));
    let item = give_camp(&f, &f.captain);
    assert!(allowed(&f));
    // Entering never records an admission, so a declaration cannot substitute for the item.
    take(&f.captain, item);
    declare(&f, ATTACKER_EXPEDITION);
    assert!(!allowed(&f));
    // Switch off: closed again.
    f.server
        .config
        .update(|c| c.housing.open_castle_zones_for_declarers = false);
    give_camp(&f, &f.captain);
    assert!(!allowed(&f));
}

/// The whole eviction timer: five 2 s warnings, then the 200 ms dismount grace.
async fn settle() {
    advance(10_000).await;
    advance(200).await;
}
