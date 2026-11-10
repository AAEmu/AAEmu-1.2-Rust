//! The 1.2 single-user rule of `doodad_almighties.once_one_interaction` / `once_one_man`
//! (`super::occupancy`): a second unit's interaction with a doodad another unit is gathering
//! from or sitting on is refused with `ALREADY_INTERACTING_SOMEONE_ELSE` (error 4), and every
//! exit releases the doodad.
//!
//! The flows drive the real `CSStartSkill` handler; the fixtures mirror the real data shape
//! (a tree: timed `DoodadFuncUse` gather chain `once_one_interaction` + `once_one_man`; a chair:
//! `DoodadFuncAttachment` with both flags; an instant `Use` doodad with `once_one_interaction`).

use aaemu_commons::{Marshal, PacketStream};

use crate::data::items::{GradeTemplate, ItemTemplate};
use crate::data::GameData;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::economy::error_message::ALREADY_INTERACTING_SOMEONE_ELSE;
use crate::systems::economy::inventory::Inv;
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::manager;
use crate::systems::npcs::doodad::templates::*;
use crate::systems::skills::data::effects::EffectTemplate;
use crate::systems::skills::data::EffectKind;
use crate::systems::skills::handlers::{start_skill, CSStartSkillPacket};
use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillTargetType};
use crate::test_support::*;
use crate::world::{DoodadRef, PlayerRef};

const TREE: u32 = 401;
const TREE_START: u32 = 4010;
const TREE_LOOT: u32 = 4011;
const CHAIR: u32 = 402;
const CHAIR_GROUP: u32 = 4020;
const INSTANT: u32 = 403;
const INSTANT_GROUP: u32 = 4030;

const TREE_SKILL: u32 = 7401;
const CHAIR_SKILL: u32 = 7402;
const INSTANT_SKILL: u32 = 7403;
const USE_EFFECT: u32 = 940_001;
const INTERACTION_MS: i32 = 5000;

const ORE: u32 = 8022;
const LABOR_BASE: i32 = 100;

/// Synthetic `DoodadData` builder (templates, funcs).
#[derive(Default)]
struct DoodadBuilder {
    data: DoodadData,
}

impl DoodadBuilder {
    fn template(mut self, id: u32, groups: &[(u32, u32)], once_one_man: bool) -> Self {
        let mut t = DoodadTemplate {
            id,
            once_one_interaction: true,
            once_one_man,
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

    let s = &mut d.systems.skills;
    s.add_effect(
        EffectKind::InteractionEffect,
        EffectTemplate::Interaction {
            id: USE_EFFECT,
            world_interaction: 19, // WorldInteractionType.Use
            doodad_id: 0,
        },
        USE_EFFECT,
    );
    let mut tree = crate::systems::skills::tests::skill_template(TREE_SKILL);
    tree.casting_time = INTERACTION_MS;
    tree.consume_labor_power = 5;
    tree.target_type = SkillTargetType::Doodad as i32;
    tree.effects
        .push(crate::systems::skills::tests::skill_effect(
            USE_EFFECT,
            EffectKind::InteractionEffect,
            USE_EFFECT,
        ));
    s.add_skill(tree);
    // Instant seat / instrument use (casting_time 0, like the real 15857 / 20847 sit skills).
    let mut chair = crate::systems::skills::tests::skill_template(CHAIR_SKILL);
    chair.target_type = SkillTargetType::Doodad as i32;
    chair
        .effects
        .push(crate::systems::skills::tests::skill_effect(
            USE_EFFECT,
            EffectKind::InteractionEffect,
            USE_EFFECT,
        ));
    s.add_skill(chair);
    // Instant interaction with no seat: must neither be blocked nor occupy.
    let mut instant = crate::systems::skills::tests::skill_template(INSTANT_SKILL);
    instant.target_type = SkillTargetType::Doodad as i32;
    instant
        .effects
        .push(crate::systems::skills::tests::skill_effect(
            USE_EFFECT,
            EffectKind::InteractionEffect,
            USE_EFFECT,
        ));
    s.add_skill(instant);

    d.systems.npcs.doodad = DoodadBuilder::default()
        // Apple / sequoia tree: a timed gather cast, phase -> loot -> back. 1.2 trees carry
        // both flags.
        .template(
            TREE,
            &[(TREE_START, 1), (TREE_LOOT, 2)],
            true, // once_one_man
        )
        .func(
            TREE_START,
            FuncTemplate::Use(DoodadFuncUse { id: 1, skill_id: 0 }),
            TREE_LOOT as i32,
        )
        .skill(TREE_START, TREE_SKILL)
        .func(
            TREE_LOOT,
            FuncTemplate::LootItem(DoodadFuncLootItem {
                id: 2,
                item_id: ORE,
                count_min: 1,
                count_max: 1,
                percent: 10_000,
                ..DoodadFuncLootItem::default()
            }),
            TREE_START as i32,
        )
        // Soft chair / piano: an instant use whose `DoodadFuncAttachment` bonds the sitter.
        .template(CHAIR, &[(CHAIR_GROUP, 1)], true)
        .func(
            CHAIR_GROUP,
            FuncTemplate::Attachment(DoodadFuncAttachment {
                id: 3,
                attach_point_id: 2,
                space: 1,
                bond_kind_id: bond_kind::BOND_CHAIR_SINGLE,
            }),
            0,
        )
        .skill(CHAIR_GROUP, CHAIR_SKILL)
        // An instant interaction (mailbox-like): never blocked by its own kind, no occupancy.
        .template(INSTANT, &[(INSTANT_GROUP, 1)], false)
        .func(
            INSTANT_GROUP,
            FuncTemplate::Use(DoodadFuncUse { id: 4, skill_id: 0 }),
            0,
        )
        .skill(INSTANT_GROUP, INSTANT_SKILL)
        .data;
    d
}

struct Fx {
    server: std::sync::Arc<GameServer>,
    c1: TestClient,
    p1: PlayerRef,
    c2: TestClient,
    p2: PlayerRef,
}

fn fx() -> Fx {
    fx_with(game_data())
}

fn fx_with(mut data: GameData) -> Fx {
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

    let mut c1 = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "First", 100.0, 100.0);
    character.labor_power = LABOR_BASE as i16;
    character.unit.hp = 1000;
    character.unit.max_hp = 1000;
    let p1 = spawn_player(&server, &c1, character, 0x100);
    c1.conn.active_char = Some(p1.clone());
    c1.drain();
    let mut c2 = TestClient::new(&server, 2, 1002);
    let mut character = test_character(2, "Second", 101.0, 100.0);
    character.labor_power = LABOR_BASE as i16;
    character.unit.hp = 1000;
    character.unit.max_hp = 1000;
    let p2 = spawn_player(&server, &c2, character, 0x101);
    c2.conn.active_char = Some(p2.clone());
    c2.drain();
    Fx {
        server,
        c1,
        p1,
        c2,
        p2,
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

    fn cast(&mut self, first: bool, skill: u32, d: &DoodadRef) -> Vec<(u16, Vec<u8>)> {
        let (client, player) = if first {
            (&mut self.c1, &self.p1)
        } else {
            (&mut self.c2, &self.p2)
        };
        client.drain();
        player.data().unit.skill_last_used_ms = 0;
        let mut wire = PacketStream::new();
        wire.write_u32(skill);
        SkillCaster::Unit(player.obj_id).write(&mut wire);
        SkillCastTarget::Doodad(d.obj_id).write(&mut wire);
        wire.write_u8(0);
        let packet =
            CSStartSkillPacket::read(&mut PacketStream::from_bytes(wire.into_bytes())).unwrap();
        start_skill(&self.server, &mut client.conn, packet);
        client.drain()
    }

    fn count(&self, player: &PlayerRef, item: u32) -> i32 {
        Inv::of(player).items_count(item, -1)
    }

    async fn wait_no_cast(&self, player: &PlayerRef, skill: u32) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        while player.data().unit.skill_state.skill_task.is_some()
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            player.data().unit.skill_state.skill_task.is_none(),
            "cast {skill} did not finish"
        );
    }
}

fn body(p: &dyn Marshal) -> Vec<u8> {
    let mut s = PacketStream::new();
    p.write(&mut s);
    s.into_bytes()
}

fn assert_refused(frames: &[(u16, Vec<u8>)]) {
    let expected =
        crate::packets::world::SCErrorMsgPacket::new(ALREADY_INTERACTING_SOMEONE_ELSE, 0, true);
    assert!(
        frames.contains(&(sc::SCErrorMsgPacket, body(&expected))),
        "no ALREADY_INTERACTING_SOMEONE_ELSE in {frames:?}"
    );
}

/// A second unit's gather cast on a tree the first unit is gathering from is refused before
/// any cost: no cast task, no labor, no loot, no phase change. The first unit's cast continues.
#[tokio::test]
async fn second_unit_gather_is_refused_while_the_first_is_casting() {
    let mut fx = fx();
    let d = fx.doodad(TREE);

    // The first player starts the 5 s gather cast; the interaction holds the tree.
    let frames = fx.cast(true, TREE_SKILL, &d);
    assert!(
        fx.p1.data().unit.skill_state.skill_task.is_some(),
        "{frames:?}"
    );

    // The second player's cast is refused with the 1.2 error and changes nothing.
    let frames = fx.cast(false, TREE_SKILL, &d);
    assert_refused(&frames);
    assert!(fx.p2.data().unit.skill_state.skill_task.is_none());
    assert_eq!(fx.p2.data().labor_power, LABOR_BASE as i16);
    assert_eq!(fx.count(&fx.p2, ORE), 0);
    assert_eq!(d.data().func_group_id, TREE_START);
    // The first player's cast was not disturbed.
    assert!(fx.p1.data().unit.skill_state.skill_task.is_some());
}

/// The same unit repeats its own interaction, and once the cast ended (or was cancelled) the
/// tree is free for the other unit again.
#[tokio::test]
async fn tree_is_free_after_the_cast_finishes_or_is_cancelled() {
    let mut fx = fx();
    let d = fx.doodad(TREE);

    // Finish: the first player gathers, then repeats its own interaction.
    fx.cast(true, TREE_SKILL, &d);
    fx.wait_no_cast(&fx.p1, TREE_SKILL).await;
    assert_eq!(fx.count(&fx.p1, ORE), 1);
    assert_eq!(d.data().func_group_id, TREE_START);
    fx.p1.data().unit.global_cooldown_ms = 0;
    let frames = fx.cast(true, TREE_SKILL, &d);
    assert!(
        fx.p1.data().unit.skill_state.skill_task.is_some(),
        "own repeat refused: {frames:?}"
    );
    fx.wait_no_cast(&fx.p1, TREE_SKILL).await;

    // The other unit may now gather too.
    fx.p2.data().unit.global_cooldown_ms = 0;
    let frames = fx.cast(false, TREE_SKILL, &d);
    assert!(
        fx.p2.data().unit.skill_state.skill_task.is_some(),
        "refused after the first finished: {frames:?}"
    );
    fx.wait_no_cast(&fx.p2, TREE_SKILL).await;
    assert_eq!(fx.count(&fx.p2, ORE), 1);

    // Cancelled: a cancelled cast releases the tree as well.
    fx.p1.data().unit.global_cooldown_ms = 0;
    fx.cast(true, TREE_SKILL, &d);
    assert!(fx.p1.data().unit.skill_state.skill_task.is_some());
    crate::systems::skills::skill::interrupt_skills(
        &fx.server,
        &crate::world::WorldObject::from(&fx.p1),
    );
    assert!(fx.p1.data().unit.skill_state.skill_task.is_none());
    fx.p2.data().unit.global_cooldown_ms = 0;
    let frames = fx.cast(false, TREE_SKILL, &d);
    assert!(
        fx.p2.data().unit.skill_state.skill_task.is_some(),
        "refused after the first cancelled: {frames:?}"
    );
    crate::systems::skills::skill::interrupt_skills(
        &fx.server,
        &crate::world::WorldObject::from(&fx.p2),
    );
}

/// A logout (the character leaves the world) must not leave the tree locked: the stale entry
/// is dropped by the safety check even when no release path ran.
#[tokio::test]
async fn tree_is_free_after_the_occupant_logs_out() {
    let mut fx = fx();
    let d = fx.doodad(TREE);

    fx.cast(true, TREE_SKILL, &d);
    assert!(fx.p1.data().unit.skill_state.skill_task.is_some());

    // What the logout does to the world after its departure save.
    let world = fx.server.world.world(0).unwrap();
    world.delete(crate::world::WorldObject::from(&fx.p1));
    fx.server.world.try_remove_character(fx.p1.obj_id);

    fx.p2.data().unit.global_cooldown_ms = 0;
    let frames = fx.cast(false, TREE_SKILL, &d);
    assert!(
        fx.p2.data().unit.skill_state.skill_task.is_some(),
        "stale occupant still blocked the tree: {frames:?}"
    );
    crate::systems::skills::skill::interrupt_skills(
        &fx.server,
        &crate::world::WorldObject::from(&fx.p2),
    );
}

/// Instant interactions are never blocked by their own kind and take no occupancy: two units
/// use the same `once_one_interaction` doodad one after the other.
#[tokio::test]
async fn instant_interactions_do_not_occupy_and_are_never_blocked() {
    let mut fx = fx();
    let d = fx.doodad(INSTANT);
    let expected =
        crate::packets::world::SCErrorMsgPacket::new(ALREADY_INTERACTING_SOMEONE_ELSE, 0, true);

    let frames = fx.cast(true, INSTANT_SKILL, &d);
    assert!(
        fx.p1.data().unit.skill_state.skill_task.is_none(),
        "{frames:?}"
    );
    assert!(!frames.contains(&(sc::SCErrorMsgPacket, body(&expected))));
    assert_eq!(fx.server.systems.npcs.doodads.occupancy.len(), 0);

    let frames = fx.cast(false, INSTANT_SKILL, &d);
    assert!(
        fx.p2.data().unit.skill_state.skill_task.is_none(),
        "{frames:?}"
    );
    assert!(!frames.contains(&(sc::SCErrorMsgPacket, body(&expected))));
    assert_eq!(fx.server.systems.npcs.doodads.occupancy.len(), 0);
}

/// `once_one_man`: while the first unit sits on the chair, a second unit's sit is refused; once
/// the first unbinds, the chair is free again.
#[tokio::test]
async fn chair_is_refused_while_occupied_and_free_after_unbond() {
    let mut fx = fx();
    let d = fx.doodad(CHAIR);

    let frames = fx.cast(true, CHAIR_SKILL, &d);
    assert!(
        fx.p1
            .data()
            .bonding
            .is_some_and(|b| b.owner_obj_id == d.obj_id),
        "first sit failed: {frames:?}"
    );

    let frames = fx.cast(false, CHAIR_SKILL, &d);
    assert_refused(&frames);
    assert!(fx.p2.data().bonding.is_none());
    assert_eq!(d.data().seat.seats.get(&d.obj_id), Some(&vec![1]));

    manager::unbond_doodad(&fx.server, &fx.p1, fx.p1.obj_id, d.obj_id);
    assert!(fx.p1.data().bonding.is_none());

    let frames = fx.cast(false, CHAIR_SKILL, &d);
    assert!(
        fx.p2
            .data()
            .bonding
            .is_some_and(|b| b.owner_obj_id == d.obj_id),
        "chair not free after unbond: {frames:?}"
    );
}
