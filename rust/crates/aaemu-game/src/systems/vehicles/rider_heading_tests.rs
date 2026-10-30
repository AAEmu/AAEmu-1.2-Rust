//! A mounted rider faces where its mount faces. The heading the character had when it mounted
//! must not survive as a local rotation: it skewed the composed world heading that the skill
//! angle checks (`target_angle`) read, so Charge / Overwhelm from a mount were refused with
//! `skill_outof_angle` whenever the leftover angle pushed the heading 90 degrees off.

use super::*;
use crate::models::npc::Npc;
use crate::systems::skills::data::SkillTemplate;
use crate::systems::skills::skill::use_skill;
use crate::systems::skills::state::SkillRun;
use crate::systems::skills::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};
use crate::world::{Entity, MateRef};

/// The mount's heading, and the (different) one the rider had when it mounted.
const MATE_YAW: f32 = 0.7;
const RIDER_YAW: f32 = 2.5;

/// Smallest signed difference of two angles in radians.
fn angle_diff(a: f32, b: f32) -> f32 {
    (a - b).sin().atan2((a - b).cos())
}

/// An owner at (100, 100) who has summoned a mount at (100, 103), turned to `mate_yaw`, and
/// then turned to `rider_yaw` (the heading kept from before the mount) and mounts the driver
/// seat. Returns the server, the owner's client (kept connected), the owner, the mount and its
/// tl id.
fn mounted(
    mate_yaw: f32,
    rider_yaw: f32,
) -> (Arc<GameServer>, TestClient, PlayerRef, MateRef, u32) {
    let server = server_with(data());
    crate::systems::skills::bind(&server);
    let client = TestClient::new(&server, 1, 1);
    let (owner, item_id) = player_with_item(&server, &client, 1, PA, (100.0, 100.0), MATE_ITEM);
    mates::spawn_mount(&server, &owner, item_id);
    let mate = server
        .systems
        .vehicles
        .mates
        .get(0)
        .get_active_mates(1)
        .pop()
        .expect("active mate");
    let tl = u32::from(mate.data().unit.tl_id);
    mate.data()
        .unit
        .transform
        .set_position(100.0, 103.0, 100.0, 0.0, 0.0, mate_yaw);
    {
        let mut c = owner.data();
        c.unit.transform.rot_z = rider_yaw;
        c.unit.hp = 1000;
        c.unit.mp = 100;
        c.unit.max_mp = 1000;
    }
    mates::mount_mate(&server, &owner, tl, attach_point::DRIVER, 1);
    assert!(owner.data().is_riding);
    (server, client, owner, mate, tl)
}

/// The unit vector a unit with yaw `yaw` faces (yaw 0 faces +y, a positive yaw turns left).
fn forward(yaw: f32) -> (f32, f32) {
    (-yaw.sin(), yaw.cos())
}

/// A hostile NPC `distance` metres from the mount along `direction`.
fn target_at(mate: &MateRef, direction: (f32, f32), distance: f32) -> Npc {
    let (mx, my) = {
        let t = mate.data().unit.transform;
        (t.x, t.y)
    };
    let mut n = Npc::new(1);
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.transform.set_xyz(
        mx + direction.0 * distance,
        my + direction.1 * distance,
        100.0,
    );
    n
}

/// Casts a 180 degree skill (Charge / Overwhelm: `target_angle` 180) on `target`.
fn cast_180(server: &GameServer, owner: &PlayerRef, target: Npc, obj_id: u32) -> u8 {
    let npc = Entity::new(obj_id, target);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&npc, &server.view_ctx());
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            target_angle: 180,
            ..crate::systems::skills::tests::skill_template(1)
        }),
        1,
    );
    use_skill(
        server,
        &skill,
        &WorldObject::from(owner),
        SkillCaster::Unit(owner.obj_id),
        &mut SkillCastTarget::Unit(npc.obj_id),
        Some(SkillObject::None),
        false,
    )
    .0
}

#[tokio::test]
async fn a_mounted_cast_is_aimed_along_the_mount_not_the_old_heading() {
    // The rider's old heading is anywhere from "same as the mount" to "facing away": the
    // target 6 m ahead of the mount is accepted in every case, the one behind it is not.
    for (mate_yaw, rider_yaw) in [
        (0.0, std::f32::consts::PI),
        (0.0, 2.5),
        (0.7, 2.5),
        (std::f32::consts::FRAC_PI_2, -1.4),
        (-2.0, 1.2),
        (3.0, 0.1),
        (1.0, 1.0),
    ] {
        let (server, _client, owner, mate, _) = mounted(mate_yaw, rider_yaw);
        let ahead = target_at(&mate, forward(mate_yaw), 6.0);
        assert_eq!(
            cast_180(&server, &owner, ahead, 0x200),
            skill_result::SUCCESS,
            "target ahead of the mount (mount yaw {mate_yaw}, rider yaw {rider_yaw})"
        );
        let (server, _client, owner, mate, _) = mounted(mate_yaw, rider_yaw);
        let (fx, fy) = forward(mate_yaw);
        let behind = target_at(&mate, (-fx, -fy), 6.0);
        assert_eq!(
            cast_180(&server, &owner, behind, 0x200),
            skill_result::OUTOF_ANGLE,
            "target behind the mount (mount yaw {mate_yaw}, rider yaw {rider_yaw})"
        );
    }
}

#[tokio::test]
async fn a_mounted_rider_has_the_mounts_position_and_heading() {
    let (server, _client, owner, mate, _) = mounted(MATE_YAW, RIDER_YAW);
    let t = owner.data().unit.transform;
    assert_eq!(t.parent_obj_id, mate.obj_id);
    assert_eq!(
        (t.x, t.y, t.z, t.rot_x, t.rot_y, t.rot_z),
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        "local transform"
    );
    let world = server.world.world(0).unwrap();
    let rider = world.world_pos_rot(&WorldObject::from(&owner));
    let seat = mate.data().unit.transform;
    assert!((rider.x - seat.x).abs() < 1e-3, "x {}", rider.x);
    assert!((rider.y - seat.y).abs() < 1e-3, "y {}", rider.y);
    assert!((rider.z - seat.z).abs() < 1e-3, "z {}", rider.z);
    assert!(
        angle_diff(rider.rot_z, MATE_YAW).abs() < 1e-3,
        "rider heading {} vs mount heading {MATE_YAW}",
        rider.rot_z
    );
    // The heading follows the mount when it turns afterwards.
    mate.data().unit.transform.rot_z = -1.2;
    let rider = world.world_pos_rot(&WorldObject::from(&owner));
    assert!(
        angle_diff(rider.rot_z, -1.2).abs() < 1e-3,
        "rider heading {}",
        rider.rot_z
    );
}

#[tokio::test]
async fn a_dismounted_rider_stands_at_the_mounts_position_and_heading() {
    let (server, _client, owner, mate, tl) = mounted(MATE_YAW, RIDER_YAW);
    mate.data()
        .unit
        .transform
        .set_position(120.0, 130.0, 101.0, 0.0, 0.0, -1.2);
    mates::unmount_mate(&server, &owner, tl, attach_point::DRIVER, 4);
    assert!(!owner.data().is_riding);
    let t = owner.data().unit.transform;
    assert_eq!(t.parent_obj_id, 0);
    assert_eq!((t.x, t.y, t.z), (120.0, 130.0, 101.0));
    assert!(
        angle_diff(t.rot_z, -1.2).abs() < 1e-3,
        "dismounted heading {}",
        t.rot_z
    );
}

#[tokio::test]
async fn a_riding_character_reattached_to_its_mount_is_seated_again() {
    let (server, _client, owner, mate, _) = mounted(MATE_YAW, RIDER_YAW);
    let world = server.world.world(0).unwrap();
    let rider = WorldObject::from(&owner);
    // The parent was lost: the local transform is now the world one (a stale place and heading).
    world.detach_child(&rider);
    assert_eq!(owner.data().unit.transform.parent_obj_id, 0);
    assert!(owner.data().unit.transform.x != 0.0);
    // The rider's next movement packet is ignored and re-attaches it to the mount.
    assert!(crate::systems::vehicles::movement::player_riding(
        &server, &owner, &owner
    ));
    let t = owner.data().unit.transform;
    assert_eq!(t.parent_obj_id, mate.obj_id);
    assert_eq!(
        (t.x, t.y, t.z, t.rot_x, t.rot_y, t.rot_z),
        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0)
    );
    let seat = mate.data().unit.transform;
    let at = world.world_pos_rot(&rider);
    assert!((at.x - seat.x).abs() < 1e-3 && (at.y - seat.y).abs() < 1e-3);
    assert!(angle_diff(at.rot_z, MATE_YAW).abs() < 1e-3);
}
