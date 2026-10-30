//! A fall never heals. The C# `Unit.DoFallDamage` computes `MaxHp * (fallVel - 8600) / 15000`,
//! which is negative for every client `fallVel` below 8600, and `ReduceCurrentHp` subtracted
//! that negative damage with no upper bound: a small jump landing healed the unit, past
//! `MaxHp`. The client value reaches `DoFallDamage` unchanged in water, without a physics
//! world and with `FallDamageZoneCap` off.
//!
//! The tests drive `do_fall_damage` and the real `CSMoveUnit` path (`session::move_unit`) on a
//! player of a test server (no collision service, synthetic world with ocean level 100).

use crate::models::movement::{MoveKind, MoveType, UnitMove};
use crate::models::unit::{Bonus, BonusTemplate, UnitAttribute, UnitModifierType};
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::ai::collision::fall::{FALL_VEL_UNITS_PER_MPS, SPEED_SAFE};
use crate::systems::skills::combat::{do_fall_damage, reduce_current_hp};
use crate::systems::skills::types::KillReason;
use crate::test_support::{spawn_player, test_character, test_server, TestClient};
use crate::world::{PlayerRef, WorldObject};

const MAX_HP: i32 = 1000;
const PLAYER_OBJ: u32 = 0x100;
/// Above the synthetic world's ocean level (100): dry land.
const LAND_Z: f32 = 150.0;
/// Below the ocean level: in the water.
const WATER_Z: f32 = 95.0;

/// The last client `fallVel` that does not exceed [`SPEED_SAFE`] (10118 with the inferred scale:
/// 20 m/s is 10118.6 client units, so no integer value lands exactly on it).
fn at_safe_speed() -> u16 {
    (SPEED_SAFE * FALL_VEL_UNITS_PER_MPS) as u16
}

/// The C# damage curve before `FallDamageMul`, for fall velocities above the safe speed.
fn curve(fall_vel: u16) -> i32 {
    MAX_HP.min((MAX_HP as f32 * ((i32::from(fall_vel) - 8600) as f32 / 15000.0)) as i32)
}

struct Faller {
    server: GameServer,
    client: TestClient,
    player: PlayerRef,
}

impl Faller {
    fn new(hp: i32) -> Self {
        let server = test_server();
        let client = TestClient::new(&server, 1, 1001);
        let mut c = test_character(1, "Faller", 500.0, 500.0);
        c.unit.hp = hp;
        c.unit.max_hp = MAX_HP;
        c.unit.transform.set_xyz(500.0, 500.0, LAND_Z);
        let player = spawn_player(&server, &client, c, PLAYER_OBJ);
        let mut faller = Self {
            server,
            client,
            player,
        };
        faller.client.conn.active_char = Some(faller.player.clone());
        faller.client.drain();
        faller
    }

    fn obj(&self) -> WorldObject {
        WorldObject::from(&self.player)
    }

    fn hp(&self) -> i32 {
        self.player.data().unit.hp
    }

    fn set_hp(&self, hp: i32) {
        self.player.data().unit.hp = hp;
    }

    /// `Unit.DoFallDamage` directly; the damage it reports.
    fn fall(&self, fall_vel: u16) -> i32 {
        do_fall_damage(&self.server, &self.obj(), fall_vel)
    }

    /// One `CSMoveUnit` of the player at `z` that carries `fall_vel`; the opcodes sent back.
    async fn land(&mut self, z: f32, fall_vel: u16) -> Vec<u16> {
        let landing = MoveType::new(MoveKind::Unit(UnitMove {
            x: 500.0,
            y: 500.0,
            z,
            fall_vel,
            ..UnitMove::default()
        }));
        crate::world::session::move_unit(&self.server, &mut self.client.conn, PLAYER_OBJ, landing)
            .await;
        self.client.drain_ops()
    }
}

#[tokio::test]
async fn a_fall_up_to_the_safe_speed_deals_no_damage_and_never_heals() {
    let mut f = Faller::new(MAX_HP);
    // 1..=8599: the C# curve is negative (it healed); 8600..: it hurt, below the zone's gate.
    for hp in [MAX_HP, MAX_HP / 2] {
        for fall_vel in [1, 500, 8599, 8600, 9000, 10000, at_safe_speed()] {
            f.set_hp(hp);
            f.client.drain();
            assert_eq!(f.fall(fall_vel), 0, "fallVel {fall_vel} at {hp} hp");
            assert_eq!(f.hp(), hp, "fallVel {fall_vel} at {hp} hp");
            let ops = f.client.drain_ops();
            assert!(
                !ops.contains(&sc::SCEnvDamagePacket) && !ops.contains(&sc::SCUnitPointsPacket),
                "fallVel {fall_vel} at {hp} hp sent {ops:x?}"
            );
        }
    }
}

#[tokio::test]
async fn fall_vel_below_at_and_above_the_safe_speed() {
    let at = at_safe_speed();
    assert!(f32::from(at) / FALL_VEL_UNITS_PER_MPS <= SPEED_SAFE);
    assert!(f32::from(at + 1) / FALL_VEL_UNITS_PER_MPS > SPEED_SAFE);
    let f = Faller::new(MAX_HP);
    let hit = |fall_vel: u16| {
        f.set_hp(MAX_HP);
        f.fall(fall_vel)
    };
    assert_eq!(hit(at - 500), 0, "below");
    assert_eq!(f.hp(), MAX_HP);
    assert_eq!(hit(at), 0, "at");
    assert_eq!(f.hp(), MAX_HP);
    let above = hit(at + 1);
    assert!(above > 0, "above: {above}");
    assert_eq!(f.hp(), MAX_HP - above);
    // Above the gate the C# curve is unchanged.
    for fall_vel in [at + 1, 12000, 20000] {
        assert_eq!(hit(fall_vel), curve(fall_vel), "fallVel {fall_vel}");
        assert_eq!(f.hp(), MAX_HP - curve(fall_vel), "fallVel {fall_vel}");
    }
}

#[tokio::test]
async fn deep_and_fatal_falls_keep_their_c_sharp_behaviour() {
    let mut f = Faller::new(MAX_HP);
    f.client.drain();
    // Deeper than the 5 % floor: the unit is left with MaxHp / 20 (the stun buff template is
    // not in the test data).
    assert!(f.fall(30000) >= MAX_HP);
    assert_eq!(f.hp(), MAX_HP / 20);
    assert!(f.client.drain_ops().contains(&sc::SCEnvDamagePacket));
    // `fallVel >= 32000` is instant death.
    f.set_hp(MAX_HP);
    f.fall(32000);
    assert_eq!(f.hp(), 0);
    assert!(f.player.data().unit.is_dead());
}

#[tokio::test]
async fn a_fall_damage_multiplier_below_minus_100_percent_cannot_heal() {
    // The real FallDamageMul modifiers (unit_modifiers, attribute 92: buffs at -10..-25 %, items
    // at -20 / -10 / +50 %) stack: five -22 % buffs are -110 %, which made the damage negative.
    let f = Faller::new(MAX_HP / 2);
    f.player.data().unit.bonuses.add_bonus(
        1,
        Bonus {
            template: BonusTemplate {
                attribute: UnitAttribute::FallDamageMul,
                modifier_type: UnitModifierType::Value,
                value: -150,
                linear_level_bonus: 0,
            },
            value: -150,
        },
    );
    assert_eq!(f.fall(20000), 0);
    assert_eq!(f.hp(), MAX_HP / 2);
}

#[tokio::test]
async fn reduce_current_hp_never_raises_hp_and_stays_within_max_hp() {
    let f = Faller::new(MAX_HP);
    let obj = f.obj();
    let hit =
        |value: i32| reduce_current_hp(&f.server, &obj, Some(&obj), value, KillReason::Damage);
    // A negative "damage" (a formula that went below zero) is no damage, never a heal.
    for value in [-1, -237, -1000, i32::MIN] {
        for hp in [MAX_HP / 2, MAX_HP] {
            f.set_hp(hp);
            hit(value);
            assert_eq!(f.hp(), hp, "damage {value} at {hp} hp");
        }
    }
    // A unit above its maximum (a max-HP buff ended) is back within it after a hit.
    f.set_hp(MAX_HP + 200);
    hit(50);
    assert!(f.hp() <= MAX_HP, "{} above {MAX_HP}", f.hp());
    // A unit whose maximum is not known (0) is not capped to 0.
    f.player.data().unit.max_hp = 0;
    f.set_hp(50);
    hit(10);
    assert_eq!(f.hp(), 40);
    f.player.data().unit.max_hp = MAX_HP;
    // Ordinary damage still lowers HP, down to 0 (dead).
    f.set_hp(MAX_HP);
    hit(100);
    assert_eq!(f.hp(), MAX_HP - 100);
    hit(5 * MAX_HP);
    assert_eq!(f.hp(), 0);
    assert!(f.player.data().unit.is_dead());
}

#[tokio::test]
async fn a_landing_in_water_never_heals() {
    for hp in [MAX_HP, MAX_HP / 2] {
        let mut f = Faller::new(hp);
        for fall_vel in [500, 8599, 9000] {
            f.set_hp(hp);
            f.client.drain();
            let ops = f.land(WATER_Z, fall_vel).await;
            assert!(
                f.player.data().unit.is_under_water,
                "the landing is in the water"
            );
            assert_eq!(f.hp(), hp, "fallVel {fall_vel} at {hp} hp");
            assert!(
                !ops.contains(&sc::SCEnvDamagePacket),
                "fallVel {fall_vel}: {ops:x?}"
            );
        }
    }
}

#[tokio::test]
async fn a_hard_landing_in_water_is_judged_like_one_on_land() {
    // Neither the C# nor the data has a water rule (the zone's own tracker skips swimming
    // landings, but its movement path forwards a client fall velocity above 20 m/s): water gets
    // the same gate as land. Pending a capture of what the real client reports there.
    let mut f = Faller::new(MAX_HP);
    f.land(WATER_Z, 20000).await;
    assert!(f.player.data().unit.is_under_water);
    assert_eq!(f.hp(), MAX_HP - curve(20000));
}

#[tokio::test]
async fn drop_then_jump_landing_then_drop_never_heals() {
    for start in [MAX_HP, MAX_HP / 2] {
        let mut f = Faller::new(start);
        f.client.drain();
        let mut hp = f.hp();
        // (client fallVel, does the landing hurt): a drop, three jump landings, another drop
        let steps = [
            (12000, true),
            (600, false),
            (8599, false),
            (14000, true),
            (700, false),
        ];
        for (step, (fall_vel, hurts)) in steps.into_iter().enumerate() {
            let ops = f.land(LAND_Z, fall_vel).await;
            let now = f.hp();
            assert!(
                (0..=MAX_HP).contains(&now) && now <= hp,
                "step {step} (fallVel {fall_vel}) from {start}: {hp} -> {now}"
            );
            if hurts {
                assert!(now < hp, "step {step}: the drop must hurt ({hp} -> {now})");
                assert!(ops.contains(&sc::SCEnvDamagePacket), "step {step}");
            } else {
                assert_eq!(now, hp, "step {step}: a jump landing must not heal or hurt");
                assert!(
                    !ops.contains(&sc::SCEnvDamagePacket),
                    "step {step}: {ops:x?}"
                );
            }
            hp = now;
        }
    }
}
