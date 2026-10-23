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
use crate::net::registry::ClientPacket;
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
const GENERIC_USE_SKILL: u32 = 15212;
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

    fn skill(mut self, group: u32, skill_id: u32) -> Self {
        let f = self
            .data
            .funcs_by_groups
            .get_mut(&group)
            .unwrap()
            .last_mut()
            .unwrap();
        f.skill_id = skill_id;
        self.data.funcs_by_id.insert(f.func_key, f.clone());
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
    shear.mainhand_tool_id = 15607;
    shear.target_type = SkillTargetType::Doodad as i32;
    shear
        .effects
        .push(crate::systems::skills::tests::skill_effect(
            SHEAR_EFFECT,
            EffectKind::InteractionEffect,
            SHEAR_EFFECT,
        ));
    s.add_skill(shear);
    let mut generic = crate::systems::skills::tests::skill_template(GENERIC_USE_SKILL);
    generic.target_type = SkillTargetType::Doodad as i32;
    generic
        .effects
        .push(crate::systems::skills::tests::skill_effect(
            SHEAR_EFFECT,
            EffectKind::InteractionEffect,
            SHEAR_EFFECT,
        ));
    s.add_skill(generic);

    d.systems.npcs.doodad = DoodadBuilder::default()
        .template(VEIN, &[(VEIN_START, 1), (VEIN_LOOT, 2), (VEIN_DEPLETED, 2)])
        .func(
            VEIN_START,
            FuncTemplate::OreMine(DoodadFuncOreMine { id: 1 }),
            VEIN_LOOT as i32,
        )
        .skill(VEIN_START, ORE_MINE_SKILL)
        .func(VEIN_LOOT, loot(2, ORE), VEIN_DEPLETED as i32)
        .phase_func(VEIN_DEPLETED, regrow_timer(3, VEIN_START as i32))
        .template(ROCK, &[(ROCK_START, 1), (ROCK_LOOT, 2), (ROCK_DEPLETED, 2)])
        .func(
            ROCK_START,
            FuncTemplate::RockMine(DoodadFuncRockMine { id: 7 }),
            ROCK_LOOT as i32,
        )
        .skill(ROCK_START, ORE_MINE_SKILL)
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
        .skill(SHEEP_START, SHEAR_SKILL)
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
    fx_with(game_data())
}

fn fx_with(mut data: GameData) -> Fx {
    open_client_skills(&mut data);
    fx_loaded(data)
}

fn fx_loaded(data: GameData) -> Fx {
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
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    c.unit.mp = 1000;
    c.unit.max_mp = 1000;
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
        self.cast_as(skill, SkillCaster::Unit(self.player.obj_id), d)
    }

    fn cast_as(&mut self, skill: u32, caster: SkillCaster, d: &DoodadRef) -> Vec<(u16, Vec<u8>)> {
        self.client.drain(); // Only observe frames caused by this cast, not doodad creation.
        self.player.data().unit.skill_last_used_ms = 0;
        let mut wire = PacketStream::new();
        wire.write_u32(skill);
        caster.write(&mut wire);
        SkillCastTarget::Doodad(d.obj_id).write(&mut wire);
        wire.write_u8(0);
        let packet =
            CSStartSkillPacket::read(&mut PacketStream::from_bytes(wire.into_bytes())).unwrap();
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

    async fn cast_and_wait(&mut self, skill: u32, d: &DoodadRef) -> Vec<(u16, Vec<u8>)> {
        let mut frames = self.cast(skill, d);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        while self.player.data().unit.skill_state.skill_task.is_some()
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            self.player.data().unit.skill_state.skill_task.is_none(),
            "cast {skill} did not finish"
        );
        frames.extend(self.client.drain());
        frames
    }
}

fn body(p: &dyn Marshal) -> Vec<u8> {
    let mut s = PacketStream::new();
    p.write(&mut s);
    s.into_bytes()
}

fn assert_refused(fx: &Fx, d: &DoodadRef, skill: u32, frames: &[(u16, Vec<u8>)]) {
    let mut expected = crate::systems::skills::packets::SCSkillStartedPacket::new(
        skill,
        0,
        SkillCaster::Unit(fx.player.obj_id),
        SkillCastTarget::Doodad(d.obj_id),
        SkillObject::None,
    );
    expected.set_skill_result(skill_result::INVALID_TARGET);
    assert!(
        frames.contains(&(sc::SCSkillStartedPacket, body(&expected))),
        "{frames:?}"
    );
    assert!(!frames.iter().any(|f| f.0 == sc::SCDoodadPhaseChangedPacket));
    let c = fx.player.data();
    assert_eq!(c.unit.global_cooldown_ms, 0);
    assert_eq!(c.unit.skill_last_used_ms, 0);
    assert!(c.unit.skill_state.skill_task.is_none());
}

#[tokio::test]
async fn generic_use_15212_cannot_gather_at_zero_labor() {
    for (template, start, authored) in [
        (VEIN, VEIN_START, false),
        (ROCK, ROCK_START, false),
        (SHEEP, SHEEP_START, false),
        (VEIN, VEIN_START, true),
        (ROCK, ROCK_START, true),
        (SHEEP, SHEEP_START, true),
    ] {
        let mut data = game_data();
        if !authored {
            data.systems
                .npcs
                .doodad
                .funcs_by_groups
                .get_mut(&start)
                .unwrap()[0]
                .skill_id = 0;
        }
        let mut fx = fx_with(data);
        let d = fx.doodad(template);
        fx.player.data().labor_power = 0;
        let mana = fx.player.data().unit.mp;

        let frames = fx.cast(GENERIC_USE_SKILL, &d);

        assert_eq!(d.data().func_group_id, start, "target {template}");
        assert_eq!(fx.count(ORE), 0);
        assert_eq!(fx.count(WOOL), 0);
        assert_eq!(fx.player.data().labor_power, 0);
        assert_eq!(fx.player.data().unit.mp, mana);
        assert_refused(&fx, &d, GENERIC_USE_SKILL, &frames);
        assert_eq!(fx.advance(REGROW_MS as u64), 0);
    }
}

#[tokio::test]
async fn a_cheaper_mining_skill_cannot_replace_the_authored_skill() {
    let mut data = game_data();
    let mut cheap = (**data.systems.skills.skill(ORE_MINE_SKILL).unwrap()).clone();
    cheap.id = 9200;
    cheap.consume_labor_power = 0;
    cheap.mana_cost = 25;
    // Same tool and InteractionEffect as the real gathering skill: classification alone
    // is not enough; the current function's authored skill must match.
    data.systems.skills.add_skill(cheap);
    let mut fx = fx_with(data);
    fx.player.data().labor_power = 0;
    let d = fx.doodad(VEIN);
    let mana = fx.player.data().unit.mp;
    let frames = fx.cast(9200, &d);
    assert_eq!(fx.player.data().unit.mp, mana);
    assert_eq!(d.data().func_group_id, VEIN_START);
    assert_eq!(fx.count(ORE), 0);
    assert_refused(&fx, &d, 9200, &frames);
}

#[tokio::test]
async fn gathering_use_and_fake_use_dispatch_require_their_authored_skill() {
    for fake in [false, true] {
        let mut data = game_data();
        let doodads = &mut data.systems.npcs.doodad;
        let t = if fake {
            FuncTemplate::FakeUse(DoodadFuncFakeUse {
                id: 50,
                fake_skill_id: ORE_MINE_SKILL,
                ..Default::default()
            })
        } else {
            FuncTemplate::Use(DoodadFuncUse {
                id: 50,
                skill_id: 0,
            })
        };
        let f = &mut doodads.funcs_by_groups.get_mut(&VEIN_START).unwrap()[0];
        f.func_type = t.func_type().into();
        f.func_id = t.id();
        // Match compact's normal Use row binding and FakeUse template binding.
        f.skill_id = if fake { 0 } else { ORE_MINE_SKILL };
        doodads.funcs_by_id.insert(f.func_key, f.clone());
        doodads
            .func_templates
            .entry(t.func_type())
            .or_default()
            .insert(t.id(), t);
        let mut fx = fx_with(data);
        let d = fx.doodad(VEIN);
        fx.player.data().labor_power = 0;
        let frames = fx.cast(GENERIC_USE_SKILL, &d);
        assert_eq!(d.data().func_group_id, VEIN_START);
        assert_eq!(fx.count(ORE), 0);
        assert_refused(&fx, &d, GENERIC_USE_SKILL, &frames);

        fx.player.data().labor_power = LABOR_BASE as i16;
        fx.cast(ORE_MINE_SKILL, &d);
        assert_eq!(d.data().func_group_id, VEIN_DEPLETED, "fake={fake}");
        assert_eq!(fx.count(ORE), 1);
        assert_eq!(
            fx.player.data().labor_power,
            (LABOR_BASE - MINING_LABOR) as i16
        );
    }
}

#[tokio::test]
async fn authorized_gathering_runs_every_skill_less_automatic_loot_row() {
    let mut data = game_data();
    let mut builder = DoodadBuilder {
        data: std::mem::take(&mut data.systems.npcs.doodad),
    };
    builder = builder.func(VEIN_LOOT, loot(60, WOOL), VEIN_DEPLETED as i32);
    data.systems.npcs.doodad = builder.data;
    let mut fx = fx_with(data);
    let d = fx.doodad(VEIN);
    fx.cast(ORE_MINE_SKILL, &d);
    assert_eq!(fx.count(ORE), 1);
    assert_eq!(fx.count(WOOL), 1);
    assert_eq!(d.data().func_group_id, VEIN_DEPLETED);
    assert_eq!(
        fx.player.data().labor_power,
        (LABOR_BASE - MINING_LABOR) as i16
    );
}

#[tokio::test]
async fn owned_mate_cannot_advance_gathering_without_character_labor() {
    let mut fx = fx();
    fx.player.data().labor_power = 0;
    let mut mate_data = crate::models::mate::Mate::new(1, 1);
    mate_data.owner_obj_id = fx.player.obj_id;
    mate_data.unit.hp = 1000;
    mate_data.unit.mp = 1000;
    mate_data.unit.transform.set_xyz(100.0, 100.0, 100.0);
    let mate = crate::world::Entity::new(0x600, mate_data);
    fx.server
        .world
        .world(0)
        .unwrap()
        .spawn(&mate, &fx.server.view_ctx());
    let d = fx.doodad(VEIN);
    fx.cast_as(
        ORE_MINE_SKILL,
        SkillCaster::Mount {
            obj_id: mate.obj_id,
            mount_skill_template_id: 0,
        },
        &d,
    );
    assert_eq!(d.data().func_group_id, VEIN_START);
    assert_eq!(fx.count(ORE), 0);
    assert_eq!(fx.player.data().labor_power, 0);
    assert_eq!(mate.data().unit.mp, 1000);
    assert_eq!(mate.data().unit.skill_last_used_ms, 0);
    assert!(mate.data().unit.skill_state.skill_task.is_none());

    // The owner cannot follow a free mate cast with a generic loot retry.
    let frames = fx.cast(GENERIC_USE_SKILL, &d);
    assert_refused(&fx, &d, GENERIC_USE_SKILL, &frames);
    assert_eq!(fx.count(ORE), 0);
    assert_eq!(d.data().func_group_id, VEIN_START);

    // The same target/skill works for its character caster and pays the normal cost.
    fx.player.data().labor_power = LABOR_BASE as i16;
    fx.cast(ORE_MINE_SKILL, &d);
    assert_eq!(fx.count(ORE), 1);
    assert_eq!(
        fx.player.data().labor_power,
        (LABOR_BASE - MINING_LABOR) as i16
    );
}

#[tokio::test]
async fn direct_phase_execution_cannot_bypass_gathering_authorization() {
    let mut data = game_data();
    data.systems
        .npcs
        .doodad
        .funcs_by_groups
        .get_mut(&VEIN_START)
        .unwrap()[0]
        .skill_id = 0;
    let fx = fx_with(data);
    let d = fx.doodad(VEIN);
    let world = fx.server.world.world(0).unwrap();
    let ctx = super::runtime::DoodadCtx::new(&fx.server, world, d.clone());
    let caster = crate::world::WorldObject::from(&fx.player);
    super::phase::use_doodad(&ctx, Some(&caster), GENERIC_USE_SKILL, 0);
    assert_eq!(d.data().func_group_id, VEIN_START);
    assert_eq!(fx.count(ORE), 0);
}

#[tokio::test]
async fn unbound_gathering_funcs_require_their_implicit_skill() {
    let mut data = game_data();
    data.systems
        .npcs
        .doodad
        .funcs_by_groups
        .get_mut(&VEIN_START)
        .unwrap()[0]
        .skill_id = 0;
    let mut fx = fx_with(data);
    let d = fx.doodad(VEIN);
    let frames = fx.cast(ORE_MINE_SKILL, &d);
    assert_eq!(fx.player.data().labor_power, LABOR_BASE as i16);
    assert_eq!(fx.count(ORE), 0);
    assert_eq!(d.data().func_group_id, VEIN_START);
    assert_refused(&fx, &d, ORE_MINE_SKILL, &frames);
}

#[tokio::test]
async fn legacy_gathering_uses_implicit_skills_and_authored_labor() {
    for (template, group, skill, source, cost, item, after) in [
        (
            VEIN,
            VEIN_START,
            11106,
            ORE_MINE_SKILL,
            3,
            ORE,
            VEIN_DEPLETED,
        ),
        (
            ROCK,
            ROCK_START,
            11107,
            ORE_MINE_SKILL,
            0,
            ORE,
            ROCK_DEPLETED,
        ),
        (
            SHEEP,
            SHEEP_START,
            11642,
            SHEAR_SKILL,
            0,
            WOOL,
            SHEEP_SHEARED,
        ),
    ] {
        let mut data = game_data();
        let mut gather = (**data.systems.skills.skill(source).unwrap()).clone();
        gather.id = skill;
        gather.consume_labor_power = cost;
        data.systems.skills.add_skill(gather);
        data.systems
            .npcs
            .doodad
            .funcs_by_groups
            .get_mut(&group)
            .unwrap()[0]
            .skill_id = 0;
        if template == VEIN {
            // A phase can expose multiple legacy funcs. Select the matching implicit
            // skill before the generic first skill-less row, even if that row is RockMine.
            let builder = DoodadBuilder {
                data: std::mem::take(&mut data.systems.npcs.doodad),
            }
            .func(
                VEIN_START,
                FuncTemplate::RockMine(DoodadFuncRockMine { id: 70 }),
                VEIN_LOOT as i32,
            );
            data.systems.npcs.doodad = builder.data;
            data.systems
                .npcs
                .doodad
                .funcs_by_groups
                .get_mut(&group)
                .unwrap()
                .swap(0, 1);
        }
        let mut fx = fx_with(data);
        let d = fx.doodad(template);
        fx.cast(skill, &d);
        assert_eq!(fx.player.data().labor_power, (LABOR_BASE - cost) as i16);
        assert_eq!(fx.count(item), 1);
        assert_eq!(d.data().func_group_id, after);
    }
}

#[tokio::test]
async fn shearing_without_labor_is_refused_before_gathering() {
    let mut fx = fx();
    fx.player.data().labor_power = 0;
    let d = fx.doodad(SHEEP);
    let frames = fx.cast(SHEAR_SKILL, &d);
    assert_eq!(fx.count(WOOL), 0);
    assert_eq!(d.data().func_group_id, SHEEP_START);
    assert_eq!(fx.player.data().labor_power, 0);
    let mut expected = crate::systems::skills::packets::SCSkillStartedPacket::new(
        SHEAR_SKILL,
        0,
        SkillCaster::Unit(fx.player.obj_id),
        SkillCastTarget::Doodad(d.obj_id),
        SkillObject::None,
    );
    expected.set_skill_result(skill_result::NEED_LABOR_POWER);
    assert!(frames.contains(&(sc::SCSkillStartedPacket, body(&expected))));
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_gathering_with_test_only_removed_item_templates() {
    use crate::data::client_files::ClientFileManager;

    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut data = game_data();
    data.skills = crate::data::skills::SkillData::load(&conn).unwrap();
    data.systems.skills =
        crate::systems::skills::Data::load_from(&conn, &ClientFileManager::new()).unwrap();
    data.systems.npcs.doodad = DoodadData::load_from(&conn).unwrap();
    data.items = crate::data::items::ItemData::load(&conn).unwrap();
    let has_base_item = |id| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM items WHERE id = ?1)",
            [id],
            |row| row.get::<_, bool>(0),
        )
        .unwrap()
    };
    // Ore 1534 uses its complete, real compact item definition; never synthesize 8077.
    assert!(has_base_item(8077));
    // TEST ONLY: compact retains rock/sheep loot references to removed items 4734 and 3.
    // Minimal in-memory stand-ins isolate authored dispatch, quantities and authorization.
    // No skill, requirement, loot row, compact file or production item data is modified;
    // successful stand-in cases do not establish complete usable live/client content.
    for test_only_missing_item_template in [4734, 3] {
        if !has_base_item(test_only_missing_item_template) {
            // The loader can retain orphan subtype shells: item 3 has an item_weapons
            // row but no base item, leaving max_count at zero. Replace that shell too.
            data.items.templates.insert(
                test_only_missing_item_template,
                ItemTemplate::plain(test_only_missing_item_template, 100),
            );
        }
    }
    assert!(data.skills.is_client_sourced_skill(GENERIC_USE_SKILL));
    let generic = data.systems.skills.skill(GENERIC_USE_SKILL).unwrap();
    assert_eq!(generic.consume_labor_power, 0);
    assert_eq!(generic.casting_time, 0);
    assert!(!data
        .systems
        .skills
        .unit_reqs
        .get("Skill")
        .unwrap()
        .iter()
        .any(|r| r.owner_id == GENERIC_USE_SKILL));

    let mut fx = fx_loaded(data);
    fx.player.data().labor_power = 0;
    for (template, group, row) in [(1534, 2387, 2213), (310, 61, 1293), (307, 56, 53)] {
        let f = fx.server.game_data.systems.npcs.doodad.func(row).unwrap();
        assert_eq!(f.skill_id, 0);
        assert_eq!(f.group_id, group);
        let d = fx.doodad(template);
        d.data().func_group_id = group;
        fx.client.drain();
        let frames = fx.cast(GENERIC_USE_SKILL, &d);
        assert_eq!(d.data().func_group_id, group, "template {template}");
        for item in [8077, 4734, 3] {
            assert_eq!(fx.count(item), 0, "template {template}, item {item}");
        }
        assert_eq!(fx.player.data().labor_power, 0);
        assert_refused(&fx, &d, GENERIC_USE_SKILL, &frames);
        manager::delete(&fx.server, &fx.server.world.world(0).unwrap(), &d);
    }
    // Real compact encodes authored shearing on Use.func_skill_id, and the legacy rock
    // skill on FakeUse.fake_skill_id. Neither is inferred from a generic Use effect.
    for (template, group, skill) in [(518, 5649, 13802), (1354, 2244, 11107)] {
        let d = fx.doodad(template);
        d.data().func_group_id = group;
        let skills = &fx.server.game_data.systems.skills;
        assert!(super::gathering::allows_skill(
            &fx.server,
            &d,
            skills.skill(skill).unwrap(),
            &crate::world::WorldObject::from(&fx.player),
        ));
        assert!(!super::gathering::allows_skill(
            &fx.server,
            &d,
            skills.skill(GENERIC_USE_SKILL).unwrap(),
            &crate::world::WorldObject::from(&fx.player),
        ));
        manager::delete(&fx.server, &fx.server.world.world(0).unwrap(), &d);
    }
    // Rock's free skill has the same mining InteractionEffect kind. It cannot substitute
    // for ore's implicit three-labor skill, even though both skills are client-common.
    fx.player.data().unit.global_cooldown_ms = 0;
    let ore = fx.doodad(1534);
    ore.data().func_group_id = 2387;
    let frames = fx.cast(11107, &ore);
    assert_refused(&fx, &ore, 11107, &frames);
    assert_eq!(fx.count(8077), 0);
    assert_eq!(ore.data().func_group_id, 2387);
    fx.player.data().labor_power = 2;
    let frames = fx.cast(11106, &ore);
    let mut expected = crate::systems::skills::packets::SCSkillStartedPacket::new(
        11106,
        0,
        SkillCaster::Unit(fx.player.obj_id),
        SkillCastTarget::Doodad(ore.obj_id),
        SkillObject::None,
    );
    expected.set_skill_result(skill_result::NEED_LABOR_POWER);
    assert!(
        frames.contains(&(sc::SCSkillStartedPacket, body(&expected))),
        "{frames:?}"
    );
    assert_eq!(fx.player.data().labor_power, 2);
    assert_eq!(fx.count(8077), 0);
    assert_eq!(ore.data().func_group_id, 2387);
    manager::delete(&fx.server, &fx.server.world.world(0).unwrap(), &ore);

    for (template, group, skill, cost, item, minimum, after) in [
        (1534, 2387, 11106, 3, 8077, 1, 2389),
        (310, 61, 11107, 0, 4734, 1, 1280),
        (307, 56, 11642, 0, 3, 8, 57),
    ] {
        assert!(fx.server.game_data.skills.is_common_skill(skill));
        let authored = fx.server.game_data.systems.skills.skill(skill).unwrap();
        assert_eq!(authored.consume_labor_power, cost);
        assert_eq!(authored.casting_time, 5000);
        fx.player.data().labor_power = LABOR_BASE as i16;
        fx.player.data().unit.global_cooldown_ms = 0;
        let d = fx.doodad(template);
        d.data().func_group_id = group;
        let before = fx.count(item);
        if skill == 11642 {
            // Keep the real precomplete-quest requirement (kind 37) operative. Establish
            // its state immediately before this cast: preceding loot events sync quests.
            let frames = fx.cast(skill, &d);
            let mut expected = crate::systems::skills::packets::SCSkillStartedPacket::new(
                skill,
                0,
                SkillCaster::Unit(fx.player.obj_id),
                SkillCastTarget::Doodad(d.obj_id),
                SkillObject::None,
            );
            expected
                .set_skill_result(crate::systems::skills::unit_reqs::skill_result_for_key(
                    "skill_urk_precomplete_quest_context",
                ))
                .set_result_uint(266);
            assert!(
                frames.contains(&(sc::SCSkillStartedPacket, body(&expected))),
                "{frames:?}"
            );
            assert_eq!(d.data().func_group_id, group);
            assert_eq!(fx.count(item), before);
            fx.player
                .data()
                .quests
                .active
                .push(crate::models::char_data::ActiveQuest {
                    id: 266,
                    template_id: 266,
                    status: crate::systems::quests::model::status::PROGRESS,
                    objectives: [0; 5],
                    step: crate::systems::quests::model::kind::PROGRESS,
                    acceptor_type: 0,
                    component_id: 0,
                    acceptor_id: 0,
                    time_ms: crate::models::char_data::ActiveQuest::DATETIME_MIN_MS,
                });
        }
        let frames = fx.cast_and_wait(skill, &d).await;
        assert_eq!(
            fx.player.data().labor_power,
            (LABOR_BASE - cost) as i16,
            "skill {skill}, {frames:?}"
        );
        assert!(
            fx.count(item) >= before + minimum,
            "skill {skill}, {frames:?}"
        );
        assert_eq!(d.data().func_group_id, after, "skill {skill}");
        assert!(frames.iter().any(|f| f.0 == sc::SCDoodadPhaseChangedPacket));
        manager::delete(&fx.server, &fx.server.world.world(0).unwrap(), &d);
    }
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
