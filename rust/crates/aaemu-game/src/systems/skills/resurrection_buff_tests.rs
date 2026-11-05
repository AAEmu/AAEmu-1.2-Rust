//! Resurrection buffs (`buffs.resurrection_health` / `resurrection_mana`): the 1.2 catalog
//! has one row, 239 "Nui's Favor" (10 / 10, percent, 30 min, `remove_on_death`). On a player
//! death it is consumed once, restores hp / mp, refunds this death's experience through the
//! `RecoverExpEffect` path and sends `SCCharacterResurrectedPacket` at the corpse position.

use std::sync::Arc;

use rusqlite::Connection;

use crate::data::GameData;
use crate::models::unit::{Bonus, BonusTemplate, UnitAttribute, UnitModifierType};
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::skills::data::BuffTemplate;
use crate::systems::skills::types::KillReason;
use crate::test_support::*;
use crate::world::{NpcRef, PlayerRef, WorldObject};

use super::buffs;

/// `buffs` row 239 "Nui's Favor" in the 1.2 `compact.sqlite3`.
const NUI_FAVOR: u32 = 239;
/// A synthetic buff with the INFERRED absolute-value reading (`resurrection_percent = 'f'`).
const ABSOLUTE_BUFF: u32 = 50_239;
const PLAYER_OBJ: u32 = 0x100;
const KILLER_OBJ: u32 = 0x200;
const KILLER_TEMPLATE: u32 = 200;
const LEVEL: u8 = 20;
/// Level 20 in `test_experience(55)`; each level spans 1 000 exp (5 % penalty = 50).
const START_EXP: i32 = 20_500;
const MAX_HP: i32 = 1_000;
const MAX_MP: i32 = 500;

fn nui_favor() -> BuffTemplate {
    BuffTemplate {
        id: NUI_FAVOR,
        kind: 1,
        duration: 1_800_000,
        remove_on_death: true,
        resurrection_health: 10,
        resurrection_mana: 10,
        resurrection_percent: true,
        ..Default::default()
    }
}

fn game_data() -> GameData {
    let mut gd = crate::systems::npcs::npc::manager::tests::npc_game_data();
    gd.experience = test_experience(55);
    gd.systems.skills.add_buff(nui_favor());
    gd.systems.skills.add_buff(BuffTemplate {
        id: ABSOLUTE_BUFF,
        kind: 1,
        duration: 60_000,
        resurrection_health: 500,
        resurrection_mana: 250,
        resurrection_percent: false,
        ..Default::default()
    });
    gd
}

/// A server with the hooks `systems::start_all` installs (the death tail and the buff
/// behaviour need them), as in `exp_recovery_tests`.
fn server_with(data: GameData) -> Arc<GameServer> {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::social::install_hooks(&server);
    crate::systems::quests::install_hooks(&server);
    crate::systems::economy::wiring::install(&server);
    crate::systems::economy::bridges::install(&server);
    server
}

struct Fx {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
    killer: NpcRef,
}

fn fixture(buff: Option<u32>) -> Fx {
    let server = server_with(game_data());
    let mut client = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "Nui", 100.0, 100.0);
    character.unit.level = LEVEL;
    character.unit.max_hp = MAX_HP;
    character.unit.hp = MAX_HP;
    character.unit.max_mp = MAX_MP;
    character.unit.mp = MAX_MP;
    character.experience = START_EXP;
    // A formula-less test character: MaxHp / MaxMp come from these bonuses, so a buff
    // add / removal (`refresh_cached_stats`) keeps the 1 000 / 500 pools.
    for (index, attribute, value) in [
        (1, UnitAttribute::MaxHealth, MAX_HP),
        (2, UnitAttribute::MaxMana, MAX_MP),
    ] {
        character.unit.bonuses.add_bonus(
            index,
            Bonus {
                template: BonusTemplate {
                    attribute,
                    modifier_type: UnitModifierType::Value,
                    value,
                    linear_level_bonus: 0,
                },
                value,
            },
        );
    }
    character.is_online = true;
    let player = spawn_player(&server, &client, character, PLAYER_OBJ);
    client.conn.active_char = Some(player.clone());
    let killer = crate::systems::npcs::npc::manager::create(&server, KILLER_OBJ, KILLER_TEMPLATE)
        .expect("killer template");
    {
        let mut n = killer.data();
        n.unit.hp = 10_000;
        n.unit.max_hp = 10_000;
        n.unit.transform.set_xyz(102.0, 100.0, 100.0);
    }
    let world = server.world.world(0).expect("instance 0");
    world.spawn(&killer, &server.view_ctx());
    if let Some(id) = buff {
        let me = WorldObject::from(&player);
        buffs::add_buff_by_id(&server, &me, id, &me, 0);
    }
    client.drain();
    Fx {
        server,
        client,
        player,
        killer,
    }
}

impl Fx {
    /// A lethal hit through the real damage path (`ReduceCurrentHp` -> `DoDie`).
    fn hit(&mut self, damage: i32) {
        let victim = WorldObject::from(&self.player);
        let killer = WorldObject::from(&self.killer);
        crate::systems::skills::combat::reduce_current_hp(
            &self.server,
            &victim,
            Some(&killer),
            damage,
            KillReason::Damage,
        );
    }

    fn has_buff(&self, id: u32) -> bool {
        self.player.data().unit.buffs.check_buff(id)
    }

    fn state(&self) -> (i32, i32, i32, i32) {
        let c = self.player.data();
        (c.unit.hp, c.unit.mp, c.experience, c.recoverable_exp)
    }
}

#[tokio::test]
async fn dying_with_nui_favor_resurrects_in_place_and_refunds_the_exp() {
    let mut fx = fixture(Some(NUI_FAVOR));
    assert!(fx.has_buff(NUI_FAVOR));
    assert_eq!(fx.state(), (MAX_HP, MAX_MP, START_EXP, 0));

    fx.hit(MAX_HP);

    // 10 % of max hp / mp, the buff is spent, and this death's 50 exp loss is refunded.
    assert_eq!(fx.state(), (100, 50, START_EXP, 0));
    assert!(!fx.has_buff(NUI_FAVOR), "the buff is consumed once");
    assert!(!fx.player.data().unit.is_dead());
    let ops = fx.client.drain_ops();
    assert!(ops.contains(&sc::SCUnitDeathPacket), "{ops:x?}");
    assert!(ops.contains(&sc::SCCharacterResurrectedPacket), "{ops:x?}");
    assert!(ops.contains(&sc::SCUnitPointsPacket), "{ops:x?}");
    assert!(ops.contains(&sc::SCExpChangedPacket), "{ops:x?}");
    let death = ops.iter().position(|op| *op == sc::SCUnitDeathPacket);
    let resurrected = ops
        .iter()
        .position(|op| *op == sc::SCCharacterResurrectedPacket);
    assert!(
        death < resurrected,
        "the self-resurrection follows the death packet: {ops:x?}"
    );
}

#[tokio::test]
async fn the_second_death_without_the_buff_kills_normally() {
    let mut fx = fixture(Some(NUI_FAVOR));
    fx.hit(MAX_HP);
    assert!(!fx.player.data().unit.is_dead());
    fx.client.drain();

    fx.hit(1_000);

    // Same death penalty as any other death, and no second resurrection (hp is 0; mp keeps
    // the value the resurrection left, the damage path does not zero it).
    assert_eq!(fx.state(), (0, 50, START_EXP - 50, 40));
    assert!(fx.player.data().unit.is_dead());
    let ops = fx.client.drain_ops();
    assert!(ops.contains(&sc::SCUnitDeathPacket), "{ops:x?}");
    assert!(!ops.contains(&sc::SCCharacterResurrectedPacket), "{ops:x?}");
}

#[tokio::test]
async fn death_without_a_resurrection_buff_is_unchanged() {
    let mut fx = fixture(None);
    fx.hit(MAX_HP);

    assert_eq!(fx.state(), (0, MAX_MP, START_EXP - 50, 40));
    let ops = fx.client.drain_ops();
    assert!(ops.contains(&sc::SCUnitDeathPacket), "{ops:x?}");
    assert!(!ops.contains(&sc::SCCharacterResurrectedPacket), "{ops:x?}");
}

/// `resurrection_percent = 'f'` (no 1.2 row uses it): the columns are absolute hp / mp.
#[tokio::test]
async fn absolute_resurrection_values_are_inferred_and_clamped() {
    let mut fx = fixture(Some(ABSOLUTE_BUFF));
    fx.hit(MAX_HP);

    assert_eq!(fx.state().0, 500);
    assert_eq!(fx.state().1, 250);
    assert!(!fx.has_buff(ABSOLUTE_BUFF));
}

/// `AAEMU_COMPACT_DB=... cargo test -p aaemu-game real_compact_db_resurrection_buff -- --ignored`
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_db_resurrection_buff_columns() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut files = crate::data::client_files::ClientFileManager::new();
    if let Ok(pak) = std::env::var("AAEMU_GAME_PAK") {
        files.add_source(&pak, std::path::Path::new("."));
    }
    let d = crate::systems::skills::data::Data::load_from(&conn, &files).unwrap();
    let t = d
        .buff(NUI_FAVOR)
        .unwrap_or_else(|| panic!("buff {NUI_FAVOR} not loaded"));
    assert_eq!(t.resurrection_health, 10);
    assert_eq!(t.resurrection_mana, 10);
    assert!(t.resurrection_percent);
    assert_eq!(t.duration, 1_800_000);
    assert!(t.remove_on_death);
    // The rest of the catalog leaves the columns NULL and must still load as zero.
    let other = d.buff(900).expect("buff 900");
    assert_eq!((other.resurrection_health, other.resurrection_mana), (0, 0));
    assert!(!other.resurrection_percent);
}
