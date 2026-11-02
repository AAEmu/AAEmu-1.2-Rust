//! Aura / crowd buff rules (see `aura.rs` for the evidence and capture-pending case ids).
//! Every test drives the real wiring: `buffs::add_buff_by_id` -> buff events -> the installed
//! `UnitHooks` (registration) -> [`crate::systems::skills::aura::tick`] -> buff application.

use std::collections::HashMap;
use std::sync::Arc;

use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::models::slave::Slave;
use crate::systems::skills::data::templates;
use crate::systems::skills::data::BuffTemplate;
use crate::systems::skills::types::{stack_rule, RelationState};
use crate::systems::skills::{aura, bind, buffs};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient, TEST_ZONE};
use crate::world::{Entity, PlayerRef, WorldObject};
use crate::GameServer;

const AURA_ANY: u32 = 9001;
const AURA_FRIENDLY: u32 = 9002;
const AURA_PARTY: u32 = 9003;
const AURA_RAID: u32 = 9004;
const AURA_HOSTILE: u32 = 9005;
const AURA_OTHERS: u32 = 9006;
const SLAVE_ANY: u32 = 9100;
const SLAVE_FRIENDLY: u32 = 9101;
const SLAVE_PARTY: u32 = 9102;
const SLAVE_RAID: u32 = 9103;
const SLAVE_HOSTILE: u32 = 9104;
const SLAVE_OTHERS: u32 = 9105;
const AURA_CHILD: u32 = 9010;
const SLAVE_CHILD: u32 = 9110;
const AURA_SELF_REF: u32 = 9020;
const AURA_DANGLING: u32 = 9030;
const AURA_DEATH: u32 = 9040;
const SLAVE_DEATH: u32 = 9140;
const AURA_REFRESH: u32 = 9011;
const SLAVE_REFRESH: u32 = 9130;
const AURA_TIMED: u32 = 9012;
const SLAVE_TIMED: u32 = 9131;
const CROWD_SOURCE: u32 = 9200;
const CROWD_BUFF: u32 = 9201;

fn faction(id: u32) -> SystemFaction {
    SystemFaction {
        id,
        mother_id: 0,
        ..SystemFaction::default()
    }
}

fn buff(id: u32) -> BuffTemplate {
    BuffTemplate {
        id,
        ..BuffTemplate::default()
    }
}

fn aura_buff(id: u32, radius: i32, relation: u32, slave: u32) -> BuffTemplate {
    BuffTemplate {
        id,
        aura_radius: radius,
        aura_relation_id: relation,
        aura_slave_buff_id: slave,
        ..BuffTemplate::default()
    }
}

fn aura_server() -> Arc<GameServer> {
    let mut data = GameData::empty();
    for id in [101, 102, 103] {
        data.factions.add(faction(id));
    }
    data.factions.relations.push(FactionRelation {
        id: 101,
        id2: 102,
        state: RelationState::Hostile as u8,
        exp_time: 0,
    });
    for t in [
        aura_buff(AURA_ANY, 20, 0, SLAVE_ANY),
        aura_buff(AURA_FRIENDLY, 20, 1, SLAVE_FRIENDLY),
        aura_buff(AURA_PARTY, 20, 2, SLAVE_PARTY),
        aura_buff(AURA_RAID, 20, 3, SLAVE_RAID),
        aura_buff(AURA_HOSTILE, 20, 4, SLAVE_HOSTILE),
        aura_buff(AURA_OTHERS, 20, 5, SLAVE_OTHERS),
        BuffTemplate {
            aura_child_only: true,
            ..aura_buff(AURA_CHILD, 1, 0, SLAVE_CHILD)
        },
        aura_buff(AURA_SELF_REF, 15, 4, AURA_SELF_REF),
        aura_buff(AURA_DANGLING, 5, 0, AURA_DANGLING + 1),
        BuffTemplate {
            remove_on_death: true,
            ..aura_buff(AURA_DEATH, 20, 0, SLAVE_DEATH)
        },
        aura_buff(AURA_REFRESH, 20, 1, SLAVE_REFRESH),
        aura_buff(AURA_TIMED, 20, 1, SLAVE_TIMED),
        BuffTemplate {
            crowd_buff_id: CROWD_BUFF,
            crowd_radius: 10.0,
            crowd_number: 3,
            crowd_friendly: true,
            crowd_hostile: false,
            ..buff(CROWD_SOURCE)
        },
    ] {
        data.systems.skills.add_buff(t);
    }
    for id in [
        SLAVE_ANY,
        SLAVE_FRIENDLY,
        SLAVE_PARTY,
        SLAVE_RAID,
        SLAVE_HOSTILE,
        SLAVE_OTHERS,
        SLAVE_CHILD,
        CROWD_BUFF,
    ] {
        data.systems.skills.add_buff(buff(id));
    }
    data.systems.skills.add_buff(BuffTemplate {
        remove_on_death: true,
        ..buff(SLAVE_DEATH)
    });
    data.systems.skills.add_buff(BuffTemplate {
        stack_rule: stack_rule::REFRESH,
        ..buff(SLAVE_REFRESH)
    });
    data.systems.skills.add_buff(BuffTemplate {
        duration: 60,
        ..buff(SLAVE_TIMED)
    });
    let server = Arc::new(test_server_with(data));
    server.set_self();
    bind(&server);
    server
}

struct Pc {
    _client: TestClient,
    p: PlayerRef,
}

impl Pc {
    fn new(server: &GameServer, id: u32, x: f32, faction_id: u32) -> Self {
        let mut client = TestClient::new(server, id, 1000 + id);
        let mut c = test_character(id, &format!("P{id}"), x, 100.0);
        c.unit.faction_id = faction_id;
        c.unit.hp = 100;
        c.unit.max_hp = 100;
        c.is_online = true;
        let p = spawn_player(
            server,
            &client,
            c,
            server.ids.object.get_next_id().expect("obj id"),
        );
        client.conn.active_char = Some(p.clone());
        client.drain();
        Self { _client: client, p }
    }

    fn obj(&self) -> WorldObject {
        WorldObject::from(&self.p)
    }

    fn add(&self, server: &GameServer, id: u32) {
        buffs::add_buff_by_id(server, &self.obj(), id, &self.obj(), 0);
    }

    fn has(&self, id: u32) -> bool {
        self.p.data().unit.buffs.check_buff(id)
    }

    fn count(&self, id: u32) -> i32 {
        self.p.data().unit.buffs.buff_count_by_id(id)
    }

    fn casters(&self, id: u32) -> Vec<u32> {
        self.p
            .data()
            .unit
            .buffs
            .effects
            .iter()
            .filter(|e| e.template.id == id)
            .map(|e| e.caster_obj_id)
            .collect()
    }

    fn move_to(&self, x: f32) {
        self.p.data().unit.transform.set_xyz(x, 100.0, 100.0);
    }
}

/// Owner faction 101, ally faction 101, enemy 101<->102 hostile, neutral 103.
struct World {
    a: Pc,
    ally: Pc,
    enemy: Pc,
    neutral: Pc,
}

fn world(server: &GameServer) -> World {
    World {
        a: Pc::new(server, 1, 100.0, 101),
        ally: Pc::new(server, 2, 110.0, 101),
        enemy: Pc::new(server, 3, 105.0, 102),
        neutral: Pc::new(server, 4, 107.0, 103),
    }
}

#[tokio::test]
async fn relation_matrix_filters_aura_recipients() {
    let server = aura_server();
    let w = world(&server);
    for id in [
        AURA_ANY,
        AURA_FRIENDLY,
        AURA_PARTY,
        AURA_RAID,
        AURA_HOSTILE,
        AURA_OTHERS,
    ] {
        w.a.add(&server, id);
    }
    aura::tick(&server);

    // Any: everyone in range, plus the owner (AURA-2).
    for pc in [&w.a, &w.ally, &w.enemy, &w.neutral] {
        assert!(pc.has(SLAVE_ANY), "Any must reach {}", pc.p.obj_id);
        assert_eq!(pc.casters(SLAVE_ANY), vec![w.a.p.obj_id]);
    }
    // Friendly: the same-faction ally and the owner only.
    assert!(w.a.has(SLAVE_FRIENDLY));
    assert!(w.ally.has(SLAVE_FRIENDLY));
    assert!(!w.enemy.has(SLAVE_FRIENDLY));
    assert!(!w.neutral.has(SLAVE_FRIENDLY));
    // Party passes everyone (`FilterWithRelation` Party is a no-op) and admits the owner.
    for pc in [&w.a, &w.ally, &w.enemy, &w.neutral] {
        assert!(pc.has(SLAVE_PARTY), "Party must reach {}", pc.p.obj_id);
    }
    // Raid without a team: only the owner (the filter appends the caster).
    assert!(w.a.has(SLAVE_RAID));
    for pc in [&w.ally, &w.enemy, &w.neutral] {
        assert!(!pc.has(SLAVE_RAID), "Raid must not reach {}", pc.p.obj_id);
    }
    // Hostile: the enemy only, never the owner or its allies.
    assert!(w.enemy.has(SLAVE_HOSTILE));
    for pc in [&w.a, &w.ally, &w.neutral] {
        assert!(
            !pc.has(SLAVE_HOSTILE),
            "Hostile must not reach {}",
            pc.p.obj_id
        );
    }
    // Others: the neutral unit only.
    assert!(w.neutral.has(SLAVE_OTHERS));
    for pc in [&w.a, &w.ally, &w.enemy] {
        assert!(
            !pc.has(SLAVE_OTHERS),
            "Others must not reach {}",
            pc.p.obj_id
        );
    }
}

#[tokio::test]
async fn enter_and_leave_radius_adds_and_removes_the_slave_buff() {
    let server = aura_server();
    let w = world(&server);
    w.a.add(&server, AURA_FRIENDLY);
    aura::tick(&server);
    assert_eq!(w.ally.casters(SLAVE_FRIENDLY), vec![w.a.p.obj_id]);

    w.ally.move_to(150.0);
    aura::tick(&server);
    assert!(!w.ally.has(SLAVE_FRIENDLY), "leaving removes the aura copy");

    // Re-entering the radius adds it again (the member is not in the stored set any more).
    w.ally.move_to(110.0);
    aura::tick(&server);
    assert_eq!(w.ally.casters(SLAVE_FRIENDLY), vec![w.a.p.obj_id]);
}

#[tokio::test]
async fn refresh_slave_is_renewed_when_its_caster_leaves() {
    // Real slave buffs (all 66 with stack rule 1) use REFRESH, so a second owner's copy would
    // overwrite the caster instead of adding an effect. The remaining owner must re-add the buff
    // on the next tick after the overwriting owner leaves (AURA-3).
    let server = aura_server();
    let a = Pc::new(&server, 1, 100.0, 101);
    let b = Pc::new(&server, 2, 105.0, 101);
    let x = Pc::new(&server, 3, 110.0, 101);
    a.add(&server, AURA_REFRESH);
    b.add(&server, AURA_REFRESH);
    aura::tick(&server);
    assert_eq!(x.count(SLAVE_REFRESH), 1, "REFRESH keeps one copy");
    let caster = x.casters(SLAVE_REFRESH)[0];
    let (departing, remaining) = if caster == a.p.obj_id {
        (&a, &b)
    } else {
        (&b, &a)
    };

    buffs::remove_buff(&server, &departing.obj(), AURA_REFRESH);
    assert!(
        !x.has(SLAVE_REFRESH),
        "the only copy was the departing owner's"
    );
    aura::tick(&server);
    assert!(
        x.has(SLAVE_REFRESH),
        "the remaining owner renews it next tick"
    );
    assert_eq!(x.casters(SLAVE_REFRESH), vec![remaining.p.obj_id]);
}

#[tokio::test]
async fn dead_recipient_gets_the_slave_buff_back_after_resurrect() {
    let server = aura_server();
    let w = world(&server);
    w.a.add(&server, AURA_DEATH);
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_DEATH));

    w.ally.p.data().unit.hp = 0;
    buffs::remove_effects_on_death(&server, &w.ally.obj());
    aura::tick(&server);
    assert!(
        !w.ally.has(SLAVE_DEATH),
        "a dead unit does not get the slave buff re-added"
    );

    w.ally.p.data().unit.hp = 100;
    aura::tick(&server);
    assert!(
        w.ally.has(SLAVE_DEATH),
        "resurrected in range, the slave buff returns"
    );
}

#[tokio::test]
async fn expired_and_dispelled_slave_buffs_are_renewed_in_place() {
    let server = aura_server();
    let w = world(&server);
    w.a.add(&server, AURA_TIMED);
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_TIMED));
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!w.ally.has(SLAVE_TIMED), "the slave's own timer removed it");
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_TIMED), "a timed slave is renewed in range");

    w.a.add(&server, AURA_FRIENDLY);
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_FRIENDLY));
    buffs::remove_buff(&server, &w.ally.obj(), SLAVE_FRIENDLY);
    assert!(!w.ally.has(SLAVE_FRIENDLY));
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_FRIENDLY), "a dispelled slave is renewed");
}

#[tokio::test]
async fn third_party_copy_is_not_hijacked_or_removed() {
    // A separately cast REFRESH copy on a current member must not be overwritten by the aura
    // (which would make the aura's leave-removal take the third party's copy away).
    let server = aura_server();
    let a = Pc::new(&server, 1, 100.0, 101);
    let x = Pc::new(&server, 2, 105.0, 101);
    let third = Pc::new(&server, 3, 107.0, 101);
    buffs::add_buff_by_id(&server, &x.obj(), SLAVE_REFRESH, &third.obj(), 0);
    a.add(&server, AURA_REFRESH);
    aura::tick(&server);
    assert_eq!(
        x.casters(SLAVE_REFRESH),
        vec![third.p.obj_id],
        "the aura does not overwrite another caster's copy"
    );

    x.move_to(150.0);
    aura::tick(&server);
    assert_eq!(
        x.casters(SLAVE_REFRESH),
        vec![third.p.obj_id],
        "leaving removes only the aura's own copy"
    );
}

#[tokio::test]
async fn recycled_owner_id_does_not_keep_a_ghost_aura() {
    let server = aura_server();
    let w = world(&server);
    w.a.add(&server, AURA_FRIENDLY);
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_FRIENDLY));

    // Logout: the object leaves the world without Removed events, and object ids are released
    // for immediate reuse.
    let instance = server.world.world(0).expect("main_world");
    instance.remove_object(w.a.p.obj_id);
    server.world.try_remove_character(w.a.p.obj_id);

    // A different unit on the recycled id must not inherit the aura.
    let mut ghost = Npc::new(1);
    ghost.unit.hp = 100;
    ghost.unit.max_hp = 100;
    ghost.unit.transform.set_xyz(100.0, 100.0, 100.0);
    ghost.unit.transform.zone_id = TEST_ZONE;
    let ghost = Entity::new(w.a.p.obj_id, ghost);
    instance.spawn(&ghost, &server.view_ctx());
    aura::tick(&server);

    assert!(
        !WorldObject::from(&ghost).with_base_unit(|b| b.buffs().check_buff(SLAVE_FRIENDLY)),
        "the recycled unit is not an aura owner"
    );
    assert!(
        server.systems.skills.aura.is_empty(),
        "the ghost entry is dropped"
    );
}

#[tokio::test]
async fn owner_losing_the_aura_strips_recipients_at_once() {
    let server = aura_server();
    let w = world(&server);
    w.a.add(&server, AURA_FRIENDLY);
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_FRIENDLY));

    buffs::remove_buff(&server, &w.a.obj(), AURA_FRIENDLY);
    assert!(!w.ally.has(SLAVE_FRIENDLY), "dispel must strip recipients");
    assert!(server.systems.skills.aura.is_empty());
}

#[tokio::test]
async fn owner_death_removes_the_aura_and_recipients() {
    let server = aura_server();
    let w = world(&server);
    w.a.add(&server, AURA_DEATH);
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_DEATH));

    w.a.p.data().unit.hp = 0;
    buffs::remove_effects_on_death(&server, &w.a.obj());
    assert!(!w.a.has(AURA_DEATH));
    assert!(!w.ally.has(SLAVE_DEATH));
    assert!(server.systems.skills.aura.is_empty());
}

#[tokio::test]
async fn owner_despawn_logout_and_instance_change_strip_recipients() {
    // Despawn (`RemoveAllEffects`, the npc / instance path).
    let server = aura_server();
    let w = world(&server);
    w.a.add(&server, AURA_FRIENDLY);
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_FRIENDLY));
    buffs::remove_all_effects(&server, &w.a.obj());
    assert!(!w.ally.has(SLAVE_FRIENDLY));

    // Logout: the object and the character leave the world without removing its buffs.
    w.a.move_to(100.0);
    w.a.add(&server, AURA_FRIENDLY);
    aura::tick(&server);
    assert!(w.ally.has(SLAVE_FRIENDLY));
    let instance = server.world.world(0).expect("main_world");
    instance.remove_object(w.a.p.obj_id);
    server.world.try_remove_character(w.a.p.obj_id);
    aura::tick(&server);
    assert!(!w.ally.has(SLAVE_FRIENDLY));
    assert!(server.systems.skills.aura.is_empty());

    // A teleport to another instance strips at once; returning re-evaluates.
    let server2 = aura_server();
    let w2 = world(&server2);
    w2.a.add(&server2, AURA_FRIENDLY);
    aura::tick(&server2);
    assert!(w2.ally.has(SLAVE_FRIENDLY));
    w2.a.p.data().unit.transform.instance_id = 1;
    w2.a.move_to(700.0);
    aura::tick(&server2);
    assert!(!w2.ally.has(SLAVE_FRIENDLY));
    w2.a.p.data().unit.transform.instance_id = 0;
    w2.a.move_to(100.0);
    aura::tick(&server2);
    assert!(w2.ally.has(SLAVE_FRIENDLY));
}

#[tokio::test]
async fn child_only_reaches_only_attached_units() {
    let server = aura_server();
    let rider = Pc::new(&server, 1, 100.0, 101);
    let bystander = Pc::new(&server, 2, 100.5, 101);
    let mut slave = Slave::new(1, 1);
    slave.unit.hp = 100;
    slave.unit.max_hp = 100;
    slave.unit.transform.set_xyz(100.0, 100.0, 100.0);
    slave.unit.transform.zone_id = TEST_ZONE;
    slave.attached_characters.push((1, rider.p.obj_id));
    let slave = Entity::new(server.ids.object.get_next_id().expect("obj id"), slave);
    let world = server.world.world(0).expect("main_world");
    world.spawn(&slave, &server.view_ctx());
    rider.p.data().unit.transform.parent_obj_id = slave.obj_id;
    // A mounted / seated unit's local transform is relative to its parent.
    rider.p.data().unit.transform.set_xyz(0.0, 0.0, 0.0);

    buffs::add_buff_by_id(
        &server,
        &WorldObject::from(&slave),
        AURA_CHILD,
        &WorldObject::from(&slave),
        0,
    );
    aura::tick(&server);
    assert!(
        rider.has(SLAVE_CHILD),
        "the attached rider receives the slave buff"
    );
    assert!(!bystander.has(SLAVE_CHILD), "a nearby bystander must not");
    assert!(
        WorldObject::from(&slave).with_base_unit(|b| b.buffs().check_buff(SLAVE_CHILD)),
        "the owner itself is a recipient (AURA-2)"
    );

    slave.data().attached_characters.clear();
    aura::tick(&server);
    assert!(!rider.has(SLAVE_CHILD), "dismounting leaves the aura");
}

#[tokio::test]
async fn self_referencing_and_dangling_slave_ids_are_skipped() {
    let server = aura_server();
    let w = world(&server);
    w.a.add(&server, AURA_SELF_REF);
    w.a.add(&server, AURA_DANGLING);
    aura::tick(&server);
    // The self-referencing buff is never re-applied, and the dangling id is not registered.
    assert_eq!(w.a.count(AURA_SELF_REF), 1);
    assert!(server.systems.skills.aura.is_empty());
}

#[tokio::test]
async fn crowd_threshold_counts_only_matching_friendlies() {
    let server = aura_server();
    let a = Pc::new(&server, 1, 100.0, 101);
    let b = Pc::new(&server, 2, 105.0, 101);
    let c = Pc::new(&server, 3, 106.0, 101);
    let enemy = Pc::new(&server, 4, 103.0, 102);
    for pc in [&a, &b, &c, &enemy] {
        pc.add(&server, CROWD_SOURCE);
    }
    aura::tick(&server);
    // Two friendly carriers: below the threshold; the hostile carrier does not count.
    assert!(!a.has(CROWD_BUFF), "2 < crowd_number 3");

    let e = Pc::new(&server, 5, 107.0, 101);
    e.add(&server, CROWD_SOURCE);
    aura::tick(&server);
    assert!(
        a.has(CROWD_BUFF),
        "three friendly carriers reach the threshold"
    );
    assert_eq!(a.casters(CROWD_BUFF), vec![a.p.obj_id], "self-cast");

    e.move_to(140.0);
    aura::tick(&server);
    assert!(!a.has(CROWD_BUFF), "one carrier leaving drops the count");

    // The hostile carrier still does not count (friendly-only data row).
    enemy.move_to(100.5);
    aura::tick(&server);
    assert!(!a.has(CROWD_BUFF));
}

#[tokio::test]
async fn crowd_buff_is_removed_when_the_source_buff_ends() {
    let server = aura_server();
    let a = Pc::new(&server, 1, 100.0, 101);
    let b = Pc::new(&server, 2, 105.0, 101);
    let c = Pc::new(&server, 3, 106.0, 101);
    let d = Pc::new(&server, 4, 107.0, 101);
    for pc in [&a, &b, &c, &d] {
        pc.add(&server, CROWD_SOURCE);
    }
    aura::tick(&server);
    assert!(a.has(CROWD_BUFF));
    buffs::remove_buff(&server, &a.obj(), CROWD_SOURCE);
    assert!(!a.has(CROWD_BUFF), "the crowd buff goes with its source");
    // The other carriers keep their own entries.
    assert_eq!(server.systems.skills.aura.len(), 3);
}

/// The real 1.2 data behind the rules: 85 aura and 37 crowd buffs, the relation distribution
/// and the quirks `warn_catalog` skips (AURA-7).
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn real_compact_aura_and_crowd_catalogs() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let templates = templates::load_buffs(&conn).unwrap();
    let buffs: HashMap<u32, BuffTemplate> = templates.into_iter().map(|b| (b.id, b)).collect();

    let aura: Vec<&BuffTemplate> = buffs.values().filter(|b| b.aura_radius > 0).collect();
    assert_eq!(aura.len(), 85, "1.2 aura buffs");
    let mut distribution = [0usize; 6];
    for b in &aura {
        distribution[(b.aura_relation_id as usize).min(5)] += 1;
    }
    assert_eq!(distribution, [22, 18, 1, 13, 28, 3]);

    let no_slave: Vec<u32> = aura
        .iter()
        .filter(|b| b.aura_slave_buff_id == 0)
        .map(|b| b.id)
        .collect();
    assert_eq!(no_slave.len(), 15);
    let self_ref: Vec<u32> = aura
        .iter()
        .filter(|b| b.aura_slave_buff_id == b.id)
        .map(|b| b.id)
        .collect();
    assert_eq!(self_ref, vec![1224]);
    let dangling: Vec<(u32, u32)> = aura
        .iter()
        .filter(|b| b.aura_slave_buff_id > 0 && b.aura_slave_buff_id != b.id)
        .filter(|b| !buffs.contains_key(&b.aura_slave_buff_id))
        .map(|b| (b.id, b.aura_slave_buff_id))
        .collect();
    assert_eq!(dangling, vec![(6668, 6669)]);
    let mut child_only: Vec<u32> = aura
        .iter()
        .filter(|b| b.aura_child_only)
        .map(|b| b.id)
        .collect();
    child_only.sort_unstable();
    assert_eq!(child_only, vec![2405, 6641, 7116, 7738]);

    let crowd: Vec<&BuffTemplate> = buffs.values().filter(|b| b.crowd_buff_id > 0).collect();
    assert_eq!(crowd.len(), 37, "1.2 crowd buffs");
    assert!(crowd.iter().all(|b| buffs.contains_key(&b.crowd_buff_id)));
    let mut friendly_only: Vec<u32> = crowd
        .iter()
        .filter(|b| b.crowd_friendly && !b.crowd_hostile)
        .map(|b| b.id)
        .collect();
    friendly_only.sort_unstable();
    assert_eq!(friendly_only, vec![6789, 11006]);
    assert!(crowd
        .iter()
        .all(|b| b.crowd_radius >= 2.0 && b.crowd_radius <= 15.0));
    assert!(crowd.iter().all(|b| (2..=8).contains(&b.crowd_number)));

    // The registration guard skips exactly the quirky rows (AURA-7).
    let usable = |id: u32| {
        buffs.get(&id).map_or(0, |t| {
            super::usable_slave(t, |slave| buffs.contains_key(&slave))
        })
    };
    assert_eq!(usable(3730), 3731);
    assert_eq!(usable(6668), 0, "dangling slave");
    assert_eq!(usable(1224), 0, "self-reference");
    assert_eq!(usable(1031), 0, "no slave buff");
    assert!(crowd
        .iter()
        .all(|t| super::usable_crowd(t, |c| buffs.contains_key(&c))));

    // The loader warning runs on the real catalog without failing it.
    aura::warn_catalog(&buffs);
}
