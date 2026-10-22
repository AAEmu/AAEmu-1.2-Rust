//! Mining / shearing gathering flows: `DoodadFuncOreMine` / `RockMine` / `Shear` advance to the
//! authored loot group, and the gathering skill pays the labor and grants the loot through the
//! real phase machine and economy.
//!
//! The compact tables only carry the func id (`doodad_func_ore_mines` / `..._rock_mines`) or
//! the shear type / term (`doodad_func_shears`); the items, depletion and regrowth are in the
//! next func groups (ore 255 -> 425 LootItem -> 426 Final / RatioRespawn, sheared 56 -> 57
//! LootItem + Timer). The 1.2 server never validated `SkillTemplate.MainhandToolId`, so only
//! the labor requirement is enforced server-side.

use aaemu_commons::{Marshal, PacketStream};

use crate::data::items::{GradeTemplate, ItemTemplate};
use crate::data::GameData;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::inventory::Inv;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::manager;
use crate::systems::npcs::doodad::templates::*;
use crate::systems::skills::data::effects::EffectTemplate;
use crate::systems::skills::data::EffectKind;
use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
use crate::systems::skills::types::{
    skill_result, SkillCastTarget, SkillCaster, SkillObject, SkillTargetType,
};
use crate::test_support::*;
use crate::world::{DoodadRef, PlayerRef};

const ORE: u32 = 8022;
const WOOL: u32 = 8053;
const LABOR_BASE: i32 = 100;
const MINING_LABOR: i32 = 5;
const SHEAR_LABOR: i32 = 1;
const REGROW_MS: i32 = 60_000;

const VEIN: u32 = 101;
const VEIN_START: u32 = 10;
const VEIN_LOOT: u32 = 11;
const VEIN_DEPLETED: u32 = 12;

const ROCK: u32 = 103;
const ROCK_START: u32 = 30;
const ROCK_LOOT: u32 = 31;
const ROCK_DEPLETED: u32 = 32;

const SHEEP: u32 = 102;
const SHEEP_START: u32 = 20;
const SHEEP_LOOT: u32 = 21;
const SHEEP_SHEARED: u32 = 22;

const ORE_MINE_SKILL: u32 = 9100;
const SHEAR_SKILL: u32 = 9101;
/// `mainhand_tool_id` of the 1.2 mining skills (data only: no server-side check).
const PICKAXE: u32 = 2654;

const ORE_MINE_EFFECT: u32 = 900_001;
const SHEAR_EFFECT: u32 = 900_002;

/// Synthetic `DoodadData` builder (templates, funcs and phase funcs).
#[derive(Default)]
struct DoodadBuilder {
    data: DoodadData,
}

impl DoodadBuilder {
    fn template(mut self, id: u32, groups: &[(u32, u32)]) -> Self {
        let mut t = DoodadTemplate {
            id,
            ..DoodadTemplate::default()
        };
        for &(gid, kind) in groups {
            let g = DoodadFuncGroups {
                id: gid,
                almighty: id,
                group_kind_id: kind,
                ..DoodadFuncGroups::default()
            };
            t.func_groups.push(g.clone());
            self.data
                .all_func_groups_index
                .insert(gid, self.data.all_func_groups.len());
            self.data.all_func_groups.push(g);
        }
        self.data.template_order.push(id);
        self.data.templates.insert(id, t);
        self
    }

    fn func(mut self, group: u32, t: FuncTemplate, next_phase: i32) -> Self {
        let (class, id) = (t.func_type(), t.id());
        self.data
            .func_templates
            .entry(class)
            .or_default()
            .insert(id, t);
        let key = self.data.funcs_by_id.len() as u32 + 1;
        let f = DoodadFunc {
            group_id: group,
            func_id: id,
            func_key: key,
            func_type: class.into(),
            next_phase,
            ..DoodadFunc::default()
        };
        self.data.funcs_by_id.insert(key, f.clone());
        self.data.funcs_by_groups.entry(group).or_default().push(f);
        self
    }

    fn phase_func(mut self, group: u32, t: FuncTemplate) -> Self {
        let (class, id) = (t.func_type(), t.id());
        self.data
            .phase_func_templates
            .entry(class)
            .or_default()
            .insert(id, t);
        self.data
            .phase_funcs
            .entry(group)
            .or_default()
            .push(DoodadPhaseFunc {
                group_id: group,
                func_id: id,
                func_type: class.into(),
            });
        self
    }
}

fn loot(id: u32, item_id: u32) -> FuncTemplate {
    FuncTemplate::LootItem(DoodadFuncLootItem {
        id,
        item_id,
        count_min: 1,
        count_max: 1,
        percent: 10_000,
        ..DoodadFuncLootItem::default()
    })
}

fn regrow_timer(id: u32, start_phase: i32) -> FuncTemplate {
    FuncTemplate::Timer(DoodadFuncTimer {
        id,
        delay: REGROW_MS,
        next_phase: start_phase,
        ..DoodadFuncTimer::default()
    })
}

fn game_data() -> GameData {
    let mut d = GameData::empty();
    d.items.grades.insert(
        0,
        GradeTemplate {
            grade: 0,
            grade_order: 0,
            durability: 1.0,
        },
    );
    d.items.config.durability_const = 1.0;
    d.items.templates.insert(ORE, ItemTemplate::plain(ORE, 100));
    d.items
        .templates
        .insert(WOOL, ItemTemplate::plain(WOOL, 100));

    let s = &mut d.systems.skills;
    s.add_effect(
        EffectKind::InteractionEffect,
        EffectTemplate::Interaction {
            id: ORE_MINE_EFFECT,
            world_interaction: 73, // WorldInteractionType.OreMine
            doodad_id: 0,
        },
        ORE_MINE_EFFECT,
    );
    s.add_effect(
        EffectKind::InteractionEffect,
        EffectTemplate::Interaction {
            id: SHEAR_EFFECT,
            world_interaction: 19, // WorldInteractionType.Use
            doodad_id: 0,
        },
        SHEAR_EFFECT,
    );
    let mut mining = crate::systems::skills::tests::skill_template(ORE_MINE_SKILL);
    mining.consume_labor_power = MINING_LABOR;
    mining.mainhand_tool_id = PICKAXE;
    mining.target_type = SkillTargetType::Doodad as i32;
    mining
        .effects
        .push(crate::systems::skills::tests::skill_effect(
            ORE_MINE_EFFECT,
            EffectKind::InteractionEffect,
            ORE_MINE_EFFECT,
        ));
    s.add_skill(mining);
    let mut shear = crate::systems::skills::tests::skill_template(SHEAR_SKILL);
    shear.consume_labor_power = SHEAR_LABOR;
    shear.target_type = SkillTargetType::Doodad as i32;
    shear
        .effects
        .push(crate::systems::skills::tests::skill_effect(
            SHEAR_EFFECT,
            EffectKind::InteractionEffect,
            SHEAR_EFFECT,
        ));
    s.add_skill(shear);

    d.systems.npcs.doodad = DoodadBuilder::default()
        .template(VEIN, &[(VEIN_START, 1), (VEIN_LOOT, 2), (VEIN_DEPLETED, 2)])
        .func(
            VEIN_START,
            FuncTemplate::OreMine(DoodadFuncOreMine { id: 1 }),
            VEIN_LOOT as i32,
        )
        .func(VEIN_LOOT, loot(2, ORE), VEIN_DEPLETED as i32)
        .phase_func(VEIN_DEPLETED, regrow_timer(3, VEIN_START as i32))
        .template(ROCK, &[(ROCK_START, 1), (ROCK_LOOT, 2), (ROCK_DEPLETED, 2)])
        .func(
            ROCK_START,
            FuncTemplate::RockMine(DoodadFuncRockMine { id: 7 }),
            ROCK_LOOT as i32,
        )
        .func(ROCK_LOOT, loot(8, ORE), ROCK_DEPLETED as i32)
        .phase_func(ROCK_DEPLETED, regrow_timer(9, ROCK_START as i32))
        .template(
            SHEEP,
            &[(SHEEP_START, 1), (SHEEP_LOOT, 2), (SHEEP_SHEARED, 2)],
        )
        .func(
            SHEEP_START,
            FuncTemplate::Shear(DoodadFuncShear {
                id: 4,
                shear_type_id: 1,
                shear_term: REGROW_MS,
            }),
            SHEEP_LOOT as i32,
        )
        .func(SHEEP_LOOT, loot(5, WOOL), SHEEP_SHEARED as i32)
        .phase_func(SHEEP_SHEARED, regrow_timer(6, SHEEP_START as i32))
        .data;
    d
}

struct Fx {
    server: std::sync::Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
}

fn fx() -> Fx {
    let mut data = game_data();
    open_client_skills(&mut data);
    let server = std::sync::Arc::new(test_server_with(data));
    server.set_self();
    crate::systems::skills::bind(&server);
    crate::systems::npcs::install_hooks(&server);
    crate::systems::social::install_hooks(&server);
    crate::systems::quests::install_hooks(&server);
    crate::systems::economy::wiring::install(&server);
    crate::systems::economy::bridges::install(&server);
    server.systems.npcs.doodads.tasks.set_manual(true);

    let mut client = TestClient::new(&server, 1, 1001);
    let mut c = test_character(1, "Miner", 100.0, 100.0);
    c.labor_power = LABOR_BASE as i16;
    let player = spawn_player(&server, &client, c, 0x100);
    client.conn.active_char = Some(player.clone());
    client.drain();
    Fx {
        server,
        client,
        player,
    }
}

impl Fx {
    fn doodad(&self, template: u32) -> DoodadRef {
        let world = self.server.world.world(0).unwrap();
        let d = manager::create(&self.server, &world, 0x500, template, None, true).unwrap();
        d.data().transform.set_xyz(102.0, 100.0, 100.0);
        manager::spawn(&self.server, &world, &d);
        manager::init_doodad(&self.server, &world, &d);
        d
    }

    fn cast(&mut self, skill: u32, d: &DoodadRef) -> Vec<(u16, Vec<u8>)> {
        self.player.data().unit.skill_last_used_ms = 0;
        let packet = CSStartSkillPacket {
            skill_id: skill,
            caster: SkillCaster::Unit(self.player.obj_id),
            target: SkillCastTarget::Doodad(d.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        };
        start_skill(&self.server, &mut self.client.conn, packet);
        self.client.drain()
    }

    fn count(&self, item: u32) -> i32 {
        Inv::of(&self.player).items_count(item, -1)
    }

    fn advance(&self, ms: u64) -> usize {
        self.server
            .systems
            .npcs
            .doodads
            .tasks
            .advance(&self.server, ms)
    }
}

fn body(p: &dyn Marshal) -> Vec<u8> {
    let mut s = PacketStream::new();
    p.write(&mut s);
    s.into_bytes()
}

#[tokio::test]
async fn ore_mine_pays_labor_grants_ore_and_regrows() {
    let mut fx = fx();
    let d = fx.doodad(VEIN);
    assert_eq!(d.data().func_group_id, VEIN_START);

    let frames = fx.cast(ORE_MINE_SKILL, &d);

    assert_eq!(
        fx.player.data().labor_power,
        (LABOR_BASE - MINING_LABOR) as i16
    );
    assert_eq!(fx.count(ORE), 1);
    assert_eq!(d.data().func_group_id, VEIN_DEPLETED);
    assert!(
        frames.iter().any(|f| f.0 == sc::SCDoodadPhaseChangedPacket),
        "no phase packet in {frames:?}"
    );

    // The depleted group's timer brings the vein back (the authored Final / RatioRespawn
    // chain does the same in the real data).
    assert_eq!(fx.advance(REGROW_MS as u64), 1);
    assert_eq!(d.data().func_group_id, VEIN_START);
}

#[tokio::test]
async fn rock_mine_pays_labor_grants_the_authored_loot_and_depletes() {
    let mut fx = fx();
    let d = fx.doodad(ROCK);
    assert_eq!(d.data().func_group_id, ROCK_START);

    fx.cast(ORE_MINE_SKILL, &d);

    assert_eq!(
        fx.player.data().labor_power,
        (LABOR_BASE - MINING_LABOR) as i16
    );
    assert_eq!(fx.count(ORE), 1);
    assert_eq!(d.data().func_group_id, ROCK_DEPLETED);
}

#[tokio::test]
async fn shear_pays_labor_grants_wool_and_regrows() {
    let mut fx = fx();
    let d = fx.doodad(SHEEP);
    assert_eq!(d.data().func_group_id, SHEEP_START);

    fx.cast(SHEAR_SKILL, &d);

    assert_eq!(
        fx.player.data().labor_power,
        (LABOR_BASE - SHEAR_LABOR) as i16
    );
    assert_eq!(fx.count(WOOL), 1);
    assert_eq!(d.data().func_group_id, SHEEP_SHEARED);

    // The sheared group's regrow timer returns the animal to its un-sheared phase.
    assert_eq!(fx.advance(REGROW_MS as u64), 1);
    assert_eq!(d.data().func_group_id, SHEEP_START);
}

#[tokio::test]
async fn mining_without_labor_is_refused_and_changes_nothing() {
    let mut fx = fx();
    fx.player.data().labor_power = (MINING_LABOR - 1) as i16;
    let d = fx.doodad(VEIN);

    let frames = fx.cast(ORE_MINE_SKILL, &d);

    assert_eq!(fx.player.data().labor_power, (MINING_LABOR - 1) as i16);
    assert_eq!(fx.count(ORE), 0);
    assert_eq!(d.data().func_group_id, VEIN_START);
    let mut expected = crate::systems::skills::packets::SCSkillStartedPacket::new(
        ORE_MINE_SKILL,
        0,
        SkillCaster::Unit(fx.player.obj_id),
        SkillCastTarget::Doodad(d.obj_id),
        SkillObject::None,
    );
    expected.set_skill_result(skill_result::NEED_LABOR_POWER);
    assert!(
        frames.contains(&(sc::SCSkillStartedPacket, body(&expected))),
        "no NEED_LABOR_POWER in {frames:?}"
    );
}
