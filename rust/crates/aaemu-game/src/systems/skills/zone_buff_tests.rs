//! Zone-group buffs across teleports and movement: the open-sea "turbulence" buff (real data:
//! buff 7743, zone groups 29/30/36/39/40/59/60) must be removed when Recall / a teleport / a
//! temple resurrection leaves the zone group, kept when the destination stays in it, and removed
//! by walking out as before.

use std::sync::Arc;

use aaemu_commons::PacketStream;

use crate::data::world_data::{Portal, Zone, ZoneData, ZoneGroup};
use crate::data::world_templates::{WorldTemplate, XmlWorldZone};
use crate::data::GameData;
use crate::models::npc::Npc;
use crate::models::transform::Transform;
use crate::net::packet::{encode, ServerPacket};
use crate::net::registry::ClientPacket;
use crate::opcodes::{cs, sc};
use crate::systems::npcs::portal::data::PortalTables;
use crate::systems::services::commands::scripts::util::teleport_player;
use crate::systems::skills::data::effects::SpecialEffect;
use crate::systems::skills::data::{BuffTemplate, EffectKind, EffectTemplate, SkillTemplate};
use crate::systems::skills::packets::SCBuffRemovedPacket;
use crate::systems::skills::resurrect::CSResurrectCharacterPacket;
use crate::systems::skills::types::{SkillCastTarget, SkillCaster};
use crate::test_support::{
    spawn_player, test_character, test_server_with_world, TestClient, TEST_ZONE,
};
use crate::world::session;
use crate::world::{Entity, PlayerRef, WorldObject};
use crate::GameServer;

/// `s_silent_sea_7` (real data: zone key 180, group 36, group buff 7743).
const SEA_ZONE: u32 = 180;
const SEA_GROUP: u32 = 36;
const LAND_ZONE: u32 = TEST_ZONE;
const TURBULENCE: u32 = 7743;
const RECALL_SKILL: u32 = 24_979;
const RECALL_SEA_SKILL: u32 = 24_980;
const LAND_GATE: u32 = 7001;
const SEA_GATE: u32 = 7002;
const RESPAWN_LAND: u32 = 9001;
const SEA_START: (f32, f32) = (100.0, 100.0);
const SEA_SPOT: (f32, f32) = (300.0, 300.0);
const LAND_SPOT: (f32, f32) = (700.0, 700.0);

fn body(p: &dyn ServerPacket) -> Vec<u8> {
    encode(p)[6..].to_vec()
}

fn zone(id: u32, key: u32, group: u32) -> Zone {
    Zone {
        id,
        name: format!("zone_{key}"),
        zone_key: key,
        group_id: group,
        ..Zone::default()
    }
}

fn zones(list: &[Zone]) -> ZoneData {
    let mut z = ZoneData::default();
    for x in list {
        z.zone_id_to_key.insert(x.id, x.zone_key);
        z.zones.insert(x.zone_key, x.clone());
        z.groups.entry(x.group_id).or_insert(ZoneGroup {
            id: x.group_id,
            name: format!("group_{}", x.group_id),
            ..ZoneGroup::default()
        });
    }
    z
}

fn gate(id: u32, zone_id: u32, x: f32, y: f32) -> Portal {
    Portal {
        id,
        name: format!("gate_{id}"),
        zone_id,
        x,
        y,
        z: 25.0,
        yaw: 90.0,
        sub_zone_id: id,
        ..Portal::default()
    }
}

/// A `main_world` whose sectors with `x >= 8` (world x >= 512 m) are land, the rest open sea.
fn world() -> WorldTemplate {
    let mut t = WorldTemplate::synthetic(0, "main_world", 2, 2, SEA_ZONE);
    t.xml_world.zones.insert(
        LAND_ZONE,
        XmlWorldZone {
            zone_key: LAND_ZONE,
            name: "land".into(),
            origin_x: 0,
            origin_y: 0,
            cells: std::collections::BTreeMap::new(),
            spawn_position: Default::default(),
        },
    );
    for x in 8..16 {
        for y in 0..16 {
            assert!(t.set_zone_key_at(x, y, LAND_ZONE));
        }
    }
    t
}

fn add_recall(data: &mut GameData, skill_id: u32, effect_id: u32, gate_id: u32) {
    data.systems.skills.add_effect(
        EffectKind::SpecialEffect,
        EffectTemplate::Special(SpecialEffect {
            id: effect_id,
            special_effect_type_id: 25, // Return
            value1: gate_id as i32,
            value2: 0,
            value3: 0,
            value4: 0,
        }),
        effect_id,
    );
    data.systems.skills.add_skill(SkillTemplate {
        target_type: 0, // Self
        effects: vec![crate::systems::skills::tests::skill_effect(
            effect_id,
            EffectKind::SpecialEffect,
            effect_id,
        )],
        ..crate::systems::skills::tests::skill_template(skill_id)
    });
}

fn server() -> Arc<GameServer> {
    let mut data = GameData::empty();
    data.zones = zones(&[zone(1, SEA_ZONE, SEA_GROUP), zone(2, LAND_ZONE, 1)]);
    data.systems
        .skills
        .zone_group_buffs
        .insert(SEA_GROUP, TURBULENCE);
    data.systems.skills.add_buff(BuffTemplate {
        id: TURBULENCE,
        ..Default::default()
    });
    data.systems.npcs.portal = PortalTables::from_parts(
        vec![gate(RESPAWN_LAND, LAND_ZONE, LAND_SPOT.0, LAND_SPOT.1)],
        vec![
            gate(LAND_GATE, LAND_ZONE, LAND_SPOT.0, LAND_SPOT.1),
            gate(SEA_GATE, SEA_ZONE, SEA_SPOT.0, SEA_SPOT.1),
        ],
        vec![],
        vec![],
    )
    .expect("portals");
    add_recall(&mut data, RECALL_SKILL, 1, LAND_GATE);
    add_recall(&mut data, RECALL_SEA_SKILL, 2, SEA_GATE);
    crate::test_support::open_client_skills(&mut data);
    let server = Arc::new(test_server_with_world(data, world()));
    server.set_self();
    crate::systems::skills::install_hooks(&server);
    // `world_gate` / `closest_return_portal` are the npcs subsystem's skill hooks.
    server
        .systems
        .skills
        .ext
        .npcs
        .install(Arc::new(crate::systems::npcs::bridges::NpcSkills));
    server
}

struct Pc {
    client: TestClient,
    p: PlayerRef,
}

impl Pc {
    fn new(server: &GameServer, id: u32, name: &str, at: (f32, f32)) -> Self {
        let mut client = TestClient::new(server, id, 1000 + id);
        let mut c = test_character(id, name, at.0, at.1);
        c.is_online = true;
        c.unit.hp = 100;
        c.unit.max_hp = 100;
        c.unit.transform.zone_id = SEA_ZONE;
        let p = spawn_player(
            server,
            &client,
            c,
            server.ids.object.get_next_id().expect("obj id"),
        );
        client.conn.active_char = Some(p.clone());
        client.drain();
        // World entry: `Character.OnZoneChange(0, zone)` applies the zone-group buff.
        session::on_zone_change(server, &p, 0, SEA_ZONE);
        client.drain();
        Self { client, p }
    }

    async fn frame(&mut self, server: &GameServer, opcode: u16, body: &[u8]) {
        let mut v = vec![0xDD, 0x01, 0, 0];
        v.extend_from_slice(&opcode.to_le_bytes());
        v.extend_from_slice(body);
        server
            .handle_frame(&mut self.client.conn, &v)
            .await
            .unwrap();
    }

    async fn cast_recall(&mut self, server: &GameServer, skill_id: u32) {
        let mut s = PacketStream::new();
        s.write_u32(skill_id);
        SkillCaster::Unit(self.p.obj_id).write(&mut s);
        SkillCastTarget::Unit(self.p.obj_id).write(&mut s);
        s.write_u8(0);
        self.frame(server, cs::CSStartSkillPacket, s.as_bytes())
            .await;
    }

    async fn move_to(&mut self, server: &GameServer, at: (f32, f32)) {
        let mut s = PacketStream::new();
        s.write_bc(self.p.obj_id)
            .write_u8(1)
            .write_u32(1234)
            .write_u8(0x02)
            .write_position(at.0, at.1, 100.0)
            .write_i16(0)
            .write_i16(0)
            .write_i16(0)
            .write_i8(0)
            .write_i8(0)
            .write_i8(0)
            .write_i8(0)
            .write_i8(0)
            .write_i8(0)
            .write_u8(1)
            .write_u8(0)
            .write_u8(0);
        self.frame(server, cs::CSMoveUnitPacket, s.as_bytes()).await;
    }

    fn has_turbulence(&self) -> bool {
        self.p.data().unit.buffs.check_buff(TURBULENCE)
    }

    fn buff_index(&self) -> u32 {
        self.p
            .data()
            .unit
            .buffs
            .effect_from_buff_id(TURBULENCE)
            .expect("turbulence buff")
            .index
    }

    async fn instance_loaded(&mut self, server: &GameServer) {
        self.frame(server, cs::CSInstanceLoadedPacket, &[]).await;
    }
}

fn removed_body(pc: &Pc, index: u32) -> (u16, Vec<u8>) {
    (
        sc::SCBuffRemovedPacket,
        body(&SCBuffRemovedPacket {
            obj_id: pc.p.obj_id,
            index,
        }),
    )
}

#[tokio::test]
async fn recall_out_of_the_open_sea_removes_the_turbulence_buff() {
    let server = server();
    let mut pc = Pc::new(&server, 1, "Sailor", SEA_START);
    assert!(pc.has_turbulence());
    let index = pc.buff_index();

    pc.cast_recall(&server, RECALL_SKILL).await;
    let frames = pc.client.drain();
    let ops: Vec<u16> = frames.iter().map(|f| f.0).collect();
    assert!(ops.contains(&sc::SCLoadInstancePacket));
    assert!(ops.contains(&sc::SCTeleportUnitPacket));
    assert!(frames.contains(&removed_body(&pc, index)));
    assert!(!pc.has_turbulence());
    let t = pc.p.data().unit.transform;
    assert_eq!((t.instance_id, t.zone_id), (0, LAND_ZONE));
    assert_eq!((t.x, t.y), LAND_SPOT);

    // The client answers the world load; the arrival region is filed and the buff stays gone.
    pc.instance_loaded(&server).await;
    assert!(!pc.has_turbulence());
    assert_eq!(pc.p.region(), Some((0, 10, 10)));
}

#[tokio::test]
async fn recall_within_the_open_sea_keeps_the_turbulence_buff() {
    let server = server();
    let mut pc = Pc::new(&server, 1, "Sailor", SEA_START);
    assert!(pc.has_turbulence());
    let index = pc.buff_index();

    pc.cast_recall(&server, RECALL_SEA_SKILL).await;
    let frames = pc.client.drain();
    assert!(!frames.contains(&removed_body(&pc, index)));
    assert!(pc.has_turbulence());
    let t = pc.p.data().unit.transform;
    assert_eq!(t.zone_id, SEA_ZONE);
    assert_eq!((t.x, t.y), SEA_SPOT);

    pc.instance_loaded(&server).await;
    assert!(pc.has_turbulence());
    assert_eq!(pc.p.region(), Some((0, 4, 4)));
}

#[tokio::test]
async fn walking_out_of_the_open_sea_removes_the_turbulence_buff() {
    let server = server();
    // Walks across the map in one packet: the movement guard has its own tests.
    server.config.update(|c| c.world.move_guard_enabled = false);
    let mut pc = Pc::new(&server, 1, "Sailor", SEA_START);
    assert!(pc.has_turbulence());
    let index = pc.buff_index();

    pc.move_to(&server, LAND_SPOT).await;
    let frames = pc.client.drain();
    assert!(frames.contains(&removed_body(&pc, index)));
    assert!(!pc.has_turbulence());
    let t = pc.p.data().unit.transform;
    assert_eq!(t.zone_id, LAND_ZONE);
    assert_eq!(pc.p.region(), Some((0, 10, 10)));
}

#[tokio::test]
async fn teleport_ended_out_of_the_open_sea_removes_the_turbulence_buff() {
    // GM `/move`, the same-instance portals and navi teleports only send `SCTeleportUnit`; the
    // landing report re-evaluates the region and must remove the zone-group buff.
    let server = server();
    let mut pc = Pc::new(&server, 1, "Sailor", SEA_START);
    assert!(pc.has_turbulence());
    let index = pc.buff_index();

    // GM `/move` / portal / navi: the teleport is armed with `SCTeleportUnit`.
    teleport_player(&server, &pc.p, 0, LAND_SPOT.0, LAND_SPOT.1, 100.0, 0.0);
    assert!(pc.p.data().disabled_set_position);
    pc.client.drain();

    let mut s = PacketStream::new();
    s.write_i64(crate::models::transform::convert_long(LAND_SPOT.0))
        .write_i64(crate::models::transform::convert_long(LAND_SPOT.1))
        .write_f32(100.0);
    for _ in 0..16 {
        s.write_u8(0);
    }
    pc.frame(&server, cs::CSTeleportEndedPacket, s.as_bytes())
        .await;
    let frames = pc.client.drain();
    assert!(frames.contains(&removed_body(&pc, index)));
    assert!(!pc.has_turbulence());
    assert_eq!(pc.p.data().unit.transform.zone_id, LAND_ZONE);
    assert_eq!(pc.p.region(), Some((0, 10, 10)));
}

#[tokio::test]
async fn unprompted_teleport_ended_does_not_move_the_character() {
    // Only a teleport the server started may move the server copy; an unsolicited completion
    // must not rewrite the position, the region or the zone-group buffs.
    let server = server();
    let mut pc = Pc::new(&server, 1, "Sailor", SEA_START);
    assert!(pc.has_turbulence());
    let index = pc.buff_index();
    assert!(!pc.p.data().disabled_set_position);
    let before = pc.p.data().unit.transform;

    let mut s = PacketStream::new();
    s.write_i64(crate::models::transform::convert_long(LAND_SPOT.0))
        .write_i64(crate::models::transform::convert_long(LAND_SPOT.1))
        .write_f32(100.0);
    for _ in 0..16 {
        s.write_u8(0);
    }
    pc.frame(&server, cs::CSTeleportEndedPacket, s.as_bytes())
        .await;
    let frames = pc.client.drain();
    assert!(!frames.contains(&removed_body(&pc, index)));
    assert_eq!(pc.p.data().unit.transform, before);
    assert!(pc.has_turbulence());
    assert_eq!(pc.p.region(), Some((0, 1, 1)));
}

#[tokio::test]
async fn teleport_ended_detaches_a_deck_stander_before_following_the_position() {
    // A GM `/move` of a character standing on a ship deck reports a world position; the old
    // parent must not be composed with it (its region / zone would otherwise win).
    let server = server();
    let mut pc = Pc::new(&server, 1, "Sailor", SEA_START);
    assert!(pc.has_turbulence());
    let index = pc.buff_index();

    let mut hull = Npc::new(1);
    hull.unit
        .transform
        .set_position(SEA_START.0, SEA_START.1, 100.0, 0.0, 0.0, 0.0);
    hull.unit.transform.zone_id = SEA_ZONE;
    let hull = Entity::new(server.ids.object.get_next_id().expect("obj id"), hull);
    let world = server.world.world(0).expect("main_world");
    world.spawn(&hull, &server.view_ctx());
    world.attach_child(&WorldObject::from(&hull), &WorldObject::from(&pc.p));
    pc.p.data().unit.transform.set_xyz(0.0, 0.0, 0.0);
    pc.client.drain();
    assert_ne!(pc.p.data().unit.transform.parent_obj_id, 0);

    teleport_player(&server, &pc.p, 0, LAND_SPOT.0, LAND_SPOT.1, 100.0, 0.0);
    pc.client.drain();
    let mut s = PacketStream::new();
    s.write_i64(crate::models::transform::convert_long(LAND_SPOT.0))
        .write_i64(crate::models::transform::convert_long(LAND_SPOT.1))
        .write_f32(100.0);
    for _ in 0..16 {
        s.write_u8(0);
    }
    pc.frame(&server, cs::CSTeleportEndedPacket, s.as_bytes())
        .await;
    let frames = pc.client.drain();

    assert_eq!(pc.p.data().unit.transform.parent_obj_id, 0);
    let t = pc.p.data().unit.transform;
    assert_eq!((t.x, t.y, t.z), (LAND_SPOT.0, LAND_SPOT.1, 100.0));
    assert_eq!(t.zone_id, LAND_ZONE);
    assert_eq!(pc.p.region(), Some((0, 10, 10)));
    assert!(frames.contains(&removed_body(&pc, index)));
    assert!(!pc.has_turbulence());
}

#[tokio::test]
async fn temple_resurrection_out_of_the_open_sea_removes_the_turbulence_buff() {
    // The 1.2 client does not answer `SCCharacterResurrected` with `CSTeleportEnded`.
    let server = server();
    let mut pc = Pc::new(&server, 1, "Sailor", SEA_START);
    assert!(pc.has_turbulence());
    let index = pc.buff_index();
    pc.p.data().unit.hp = 0;

    CSResurrectCharacterPacket { in_place: false }
        .handle(&server, &mut pc.client.conn)
        .await
        .unwrap();
    let frames = pc.client.drain();
    assert!(frames.contains(&removed_body(&pc, index)));
    assert!(!pc.has_turbulence());
    let t = pc.p.data().unit.transform;
    assert_eq!(t.zone_id, LAND_ZONE);
    assert_eq!((t.x, t.y), LAND_SPOT);
    assert_eq!(pc.p.region(), Some((0, 10, 10)));
}

/// The real 1.2 data behind these tests: the open-sea zone groups carry buff 7743.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only 1.2 compact.sqlite3)"]
fn real_compact_open_sea_zone_groups_carry_the_turbulence_buff() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let groups: Vec<(u32, u32)> = db
        .prepare("SELECT id, buff_id FROM zone_groups WHERE buff_id <> 0 ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        groups,
        vec![
            (29, TURBULENCE),
            (30, TURBULENCE),
            (36, TURBULENCE),
            (39, TURBULENCE),
            (40, TURBULENCE),
            (59, TURBULENCE),
            (60, TURBULENCE),
        ]
    );
    let name: String = db
        .query_row("SELECT name FROM buffs WHERE id = ?1", [TURBULENCE], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(name, "불규칙한 바닷바람");
    let sea_zones: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM zones WHERE group_id IN (29, 30, 36, 39, 40, 59, 60)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sea_zones > 0);
}

/// The helpers keep working for a same-instance teleport without a zone change (no buff churn).
#[tokio::test]
async fn same_zone_teleport_keeps_the_buff_and_the_world_entry() {
    let server = server();
    let mut pc = Pc::new(&server, 1, "Sailor", SEA_START);
    let index = pc.buff_index();
    let destination = Transform {
        instance_id: 0,
        zone_id: SEA_ZONE,
        x: SEA_SPOT.0,
        y: SEA_SPOT.1,
        z: 100.0,
        ..Transform::default()
    };
    assert!(session::teleport_character(&server, &pc.p, destination));
    session::file_teleport_arrival(&server, &pc.p);
    let frames = pc.client.drain();
    assert!(!frames.contains(&removed_body(&pc, index)));
    assert!(pc.has_turbulence());
    assert_eq!(pc.p.region(), Some((0, 4, 4)));
    assert!(server
        .world
        .object(pc.p.obj_id)
        .is_some_and(|o| matches!(o, WorldObject::Player(_))));
}
