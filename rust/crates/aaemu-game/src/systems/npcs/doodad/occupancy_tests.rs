//! The 1.2 single-user rule of `doodad_almighties.once_one_man` (`super::occupancy`): a second
//! unit's timed interaction with a doodad another unit is gathering from (or a held single seat
//! another unit occupies) is refused with `ALREADY_INTERACTING_SOMEONE_ELSE` (error 4), and
//! every exit releases the doodad. `once_one_interaction` alone is not enforced; multi-seat
//! attachments and the instant / loot / skill-hit kinds are never held nor refused.
//!
//! The flows drive the real `CSStartSkill` handler; the fixtures mirror the real data shape
//! (a tree: timed `DoodadFuncUse` gather chain, both columns set; a chair: `DoodadFuncAttachment`
//! space 1; a 2-seat carriage chair; an instant plain `Use` doodad; a loot chest and a SkillHit
//! doodad held by another unit).

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
use crate::systems::skills::state::SkillRun;
use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillTargetType};
use crate::test_support::*;
use crate::world::{DoodadRef, PlayerRef, WorldObject};

const TREE: u32 = 401;
const TREE_START: u32 = 4010;
const TREE_LOOT: u32 = 4011;
const SOLO: u32 = 404;
const SOLO_START: u32 = 4040;
const SOLO_LOOT: u32 = 4041;
const CHAIR: u32 = 402;
const CHAIR_GROUP: u32 = 4020;
const CHAIR2: u32 = 405;
const CHAIR2_GROUP: u32 = 4050;
const INSTANT: u32 = 403;
const INSTANT_GROUP: u32 = 4030;
const CHEST: u32 = 406;
const CHEST_START: u32 = 4060;
const CHEST_DONE: u32 = 4061;
const HIT: u32 = 407;
const HIT_START: u32 = 4070;
const HIT_NEXT: u32 = 4071;

const TREE_SKILL: u32 = 7401;
const TREE_DELAYED: u32 = 7404;
const CHAIR_SKILL: u32 = 7402;
const INSTANT_SKILL: u32 = 7403;
const HIT_SKILL: u32 = 7405;
const USE_EFFECT: u32 = 940_001;
const INTERACTION_MS: i32 = 5000;
const DELAYED_MS: i32 = 400;
const EFFECT_DELAY_MS: i32 = 1500;

const ORE: u32 = 8022;
const LABOR_BASE: i32 = 100;

/// Synthetic `DoodadData` builder (templates, funcs).
#[derive(Default)]
struct DoodadBuilder {
    data: DoodadData,
}

impl DoodadBuilder {
    fn template(
        mut self,
        id: u32,
        groups: &[(u32, u32)],
        once_one_interaction: bool,
        once_one_man: bool,
    ) -> Self {
        let mut t = DoodadTemplate {
            id,
            once_one_interaction,
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

fn use_func(id: u32) -> FuncTemplate {
    FuncTemplate::Use(DoodadFuncUse { id, skill_id: 0 })
}

fn loot(id: u32) -> FuncTemplate {
    FuncTemplate::LootItem(DoodadFuncLootItem {
        id,
        item_id: ORE,
        count_min: 1,
        count_max: 1,
        percent: 10_000,
        ..DoodadFuncLootItem::default()
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
    s.add_skill(tree.clone());
    // The same gather, but its interaction effect fires after the cast slot was cleared.
    let mut delayed = tree.clone();
    delayed.id = TREE_DELAYED;
    delayed.casting_time = DELAYED_MS;
    delayed.consume_labor_power = 0;
    delayed.effect_delay = EFFECT_DELAY_MS;
    s.add_skill(delayed);
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
    // Instant plain Use: never blocked, never held.
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
    // SkillHit by an AoE: never refused by the single-user rule.
    let mut hit = crate::systems::skills::tests::skill_template(HIT_SKILL);
    hit.target_type = SkillTargetType::Doodad as i32;
    hit.effects
        .push(crate::systems::skills::tests::skill_effect(
            USE_EFFECT,
            EffectKind::InteractionEffect,
            USE_EFFECT,
        ));
    s.add_skill(hit);

    d.systems.npcs.doodad = DoodadBuilder::default()
        // Apple / sequoia tree: a timed gather cast, phase -> loot -> back. 1.2 trees carry
        // both columns; only once_one_man is enforced.
        .template(TREE, &[(TREE_START, 1), (TREE_LOOT, 2)], true, true)
        .func(TREE_START, use_func(1), TREE_LOOT as i32)
        .skill(TREE_START, TREE_SKILL)
        .func(TREE_START, use_func(5), TREE_LOOT as i32)
        .skill(TREE_START, TREE_DELAYED)
        .func(TREE_LOOT, loot(2), TREE_START as i32)
        // The same shape with once_one_interaction only: NOT enforced.
        .template(SOLO, &[(SOLO_START, 1), (SOLO_LOOT, 2)], true, false)
        .func(SOLO_START, use_func(10), SOLO_LOOT as i32)
        .skill(SOLO_START, TREE_SKILL)
        .func(SOLO_LOOT, loot(11), SOLO_START as i32)
        // Soft chair / piano: an instant use whose `DoodadFuncAttachment` bonds the sitter.
        .template(CHAIR, &[(CHAIR_GROUP, 1)], true, true)
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
        // Carriage chair: two places; the seat capacity decides, no occupancy.
        .template(CHAIR2, &[(CHAIR2_GROUP, 1)], true, true)
        .func(
            CHAIR2_GROUP,
            FuncTemplate::Attachment(DoodadFuncAttachment {
                id: 4,
                attach_point_id: 2,
                space: 2,
                bond_kind_id: bond_kind::BOND_CHAIR_DOUBLE,
            }),
            0,
        )
        .skill(CHAIR2_GROUP, CHAIR_SKILL)
        // An instant plain Use doodad with once_one_man: never blocked, never held.
        .template(INSTANT, &[(INSTANT_GROUP, 1)], true, true)
        .func(INSTANT_GROUP, use_func(6), 0)
        .skill(INSTANT_GROUP, INSTANT_SKILL)
        // A loot chest (skill-less LootItem) whose start group also has a holdable timed use.
        .template(CHEST, &[(CHEST_START, 1), (CHEST_DONE, 2)], true, true)
        .func(CHEST_START, use_func(7), CHEST_DONE as i32)
        .skill(CHEST_START, TREE_SKILL)
        .func(CHEST_START, loot(8), CHEST_DONE as i32)
        // A SkillHit doodad (an AoE must still run its SkillHit func).
        .template(HIT, &[(HIT_START, 1), (HIT_NEXT, 2)], true, true)
        .func(HIT_START, use_func(9), HIT_NEXT as i32)
        .skill(HIT_START, TREE_SKILL)
        .func(
            HIT_START,
            FuncTemplate::SkillHit(DoodadFuncSkillHit {
                id: 10,
                skill_id: HIT_SKILL,
            }),
            HIT_NEXT as i32,
        )
        .skill(HIT_START, HIT_SKILL)
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

    fn ctx(&self, d: &DoodadRef) -> super::runtime::DoodadCtx<'_> {
        super::runtime::DoodadCtx::new(&self.server, self.server.world.world(0).unwrap(), d.clone())
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

    fn run_skill(&self, skill_id: u32) -> crate::systems::skills::state::SkillRef {
        let template = self
            .server
            .game_data
            .systems
            .skills
            .skill(skill_id)
            .expect("fixture skill")
            .clone();
        SkillRun::new(template, 1)
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

fn assert_not_refused(frames: &[(u16, Vec<u8>)]) {
    let expected =
        crate::packets::world::SCErrorMsgPacket::new(ALREADY_INTERACTING_SOMEONE_ELSE, 0, true);
    assert!(
        !frames.contains(&(sc::SCErrorMsgPacket, body(&expected))),
        "unexpected ALREADY_INTERACTING_SOMEONE_ELSE in {frames:?}"
    );
}

/// A second unit's gather cast on a tree the first unit is gathering from is refused before
/// any cost: no cast task, no labor, no loot, no phase change. The first unit's cast continues.
#[tokio::test]
async fn second_unit_gather_is_refused_while_the_first_is_casting() {
    let mut fx = fx();
    let d = fx.doodad(TREE);

    let frames = fx.cast(true, TREE_SKILL, &d);
    assert!(
        fx.p1.data().unit.skill_state.skill_task.is_some(),
        "{frames:?}"
    );

    let frames = fx.cast(false, TREE_SKILL, &d);
    assert_refused(&frames);
    assert!(fx.p2.data().unit.skill_state.skill_task.is_none());
    assert_eq!(fx.p2.data().labor_power, LABOR_BASE as i16);
    assert_eq!(fx.count(&fx.p2, ORE), 0);
    assert_eq!(d.data().func_group_id, TREE_START);
    assert!(fx.p1.data().unit.skill_state.skill_task.is_some());
}

/// The same unit repeats its own interaction, and once the cast ended (or was cancelled) the
/// tree is free for the other unit again.
#[tokio::test]
async fn tree_is_free_after_the_cast_finishes_or_is_cancelled() {
    let mut fx = fx();
    let d = fx.doodad(TREE);

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

    fx.p2.data().unit.global_cooldown_ms = 0;
    let frames = fx.cast(false, TREE_SKILL, &d);
    assert!(
        fx.p2.data().unit.skill_state.skill_task.is_some(),
        "refused after the first finished: {frames:?}"
    );
    fx.wait_no_cast(&fx.p2, TREE_SKILL).await;
    assert_eq!(fx.count(&fx.p2, ORE), 1);

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
/// use the same `once_one_man` doodad one after the other.
#[tokio::test]
async fn instant_interactions_do_not_occupy_and_are_never_blocked() {
    let mut fx = fx();
    let d = fx.doodad(INSTANT);

    let frames = fx.cast(true, INSTANT_SKILL, &d);
    assert!(
        fx.p1.data().unit.skill_state.skill_task.is_none(),
        "{frames:?}"
    );
    assert_not_refused(&frames);
    assert_eq!(fx.server.systems.npcs.doodads.occupancy.len(), 0);

    let frames = fx.cast(false, INSTANT_SKILL, &d);
    assert!(
        fx.p2.data().unit.skill_state.skill_task.is_none(),
        "{frames:?}"
    );
    assert_not_refused(&frames);
    assert_eq!(fx.server.systems.npcs.doodads.occupancy.len(), 0);
}

/// `once_one_man` holds a single seat; a `space > 1` attachment is never held (the passenger
/// capacity decides): two units sit on the same 2-place carriage chair.
#[tokio::test]
async fn multi_seat_attachment_is_never_held() {
    let mut fx = fx();
    let d = fx.doodad(CHAIR2);

    let frames = fx.cast(true, CHAIR_SKILL, &d);
    assert!(
        fx.p1
            .data()
            .bonding
            .is_some_and(|b| b.owner_obj_id == d.obj_id),
        "first sit failed: {frames:?}"
    );
    let frames = fx.cast(false, CHAIR_SKILL, &d);
    assert!(
        fx.p2
            .data()
            .bonding
            .is_some_and(|b| b.owner_obj_id == d.obj_id),
        "second sit refused on a 2-place seat: {frames:?}"
    );
    assert_not_refused(&frames);
    assert_eq!(d.data().seat.seats.get(&d.obj_id), Some(&vec![1, 2]));
    assert_eq!(fx.server.systems.npcs.doodads.occupancy.len(), 0);
}

/// `once_one_man` holds the single seat of a chair: a second unit's sit is refused; once the
/// first unbinds, the chair is free again.
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

/// The race of the accepted-but-not-yet-scheduled window: the first acquire has no cast task
/// yet, a second begin must not drop it, and the first keeps its hold.
#[tokio::test]
async fn a_fresh_hold_is_not_dropped_by_a_second_begin() {
    let fx = fx();
    let d = fx.doodad(TREE);
    let w1 = WorldObject::from(&fx.p1);
    let w2 = WorldObject::from(&fx.p2);
    let skill = fx.run_skill(TREE_SKILL);

    assert!(super::occupancy::begin_cast(&fx.server, &d, &w1, &skill));
    // The cast task is set only after the accept; simulate the window with no task at all.
    assert!(fx.p1.data().unit.skill_state.skill_task.is_none());
    assert!(!super::occupancy::begin_cast(&fx.server, &d, &w2, &skill));
    assert!(!super::occupancy::permits(&fx.server, &d, &w2));
    assert!(super::occupancy::permits(&fx.server, &d, &w1));

    super::occupancy::end_cast(&fx.server, d.obj_id, &w1);
    assert!(super::occupancy::begin_cast(&fx.server, &d, &w2, &skill));
}

/// `once_one_interaction` alone is loaded but not enforced: two units may gather the same
/// flagged tree while the first cast runs.
#[tokio::test]
async fn once_one_interaction_alone_is_not_enforced() {
    let mut fx = fx();
    let d = fx.doodad(SOLO);

    let frames = fx.cast(true, TREE_SKILL, &d);
    assert!(
        fx.p1.data().unit.skill_state.skill_task.is_some(),
        "{frames:?}"
    );
    let frames = fx.cast(false, TREE_SKILL, &d);
    assert!(
        fx.p2.data().unit.skill_state.skill_task.is_some(),
        "once_one_interaction alone refused the second unit: {frames:?}"
    );
    assert_not_refused(&frames);
    for p in [&fx.p1, &fx.p2] {
        crate::systems::skills::skill::interrupt_skills(
            &fx.server,
            &crate::world::WorldObject::from(p),
        );
    }
}

/// A delayed interaction effect keeps the hold after the cast slot was cleared, refuses a new
/// timed start by another unit, and the occupant's own effect still runs.
#[tokio::test]
async fn delayed_effect_keeps_the_hold_and_the_occupant_effect_runs() {
    let mut fx = fx();
    let d = fx.doodad(TREE);

    fx.cast(true, TREE_DELAYED, &d);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while fx.p1.data().unit.skill_state.skill_task.is_some()
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        fx.p1.data().unit.skill_state.skill_task.is_none(),
        "delayed cast did not clear its slot"
    );

    // The effect is still pending: the tree is not free.
    let frames = fx.cast(false, TREE_SKILL, &d);
    assert_refused(&frames);
    assert!(fx.p2.data().unit.skill_state.skill_task.is_none());

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while fx.count(&fx.p1, ORE) == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        fx.count(&fx.p1, ORE),
        1,
        "the occupant's delayed effect was refused"
    );
}

/// A skill-less loot interaction (the direct `Doodad.Use` / `CSLootOpenBag` shape) runs while
/// another unit holds the doodad.
#[tokio::test]
async fn loot_runs_during_another_units_hold() {
    let mut fx = fx();
    let d = fx.doodad(CHEST);
    let w1 = WorldObject::from(&fx.p1);
    let w2 = WorldObject::from(&fx.p2);
    let skill = fx.run_skill(TREE_SKILL);

    assert!(super::occupancy::begin_cast(&fx.server, &d, &w1, &skill));
    fx.c2.drain();
    let ctx = fx.ctx(&d);
    super::phase::use_doodad(&ctx, Some(&w2), 0, 0);
    drop(ctx);
    let frames = fx.c2.drain();
    assert_not_refused(&frames);
    assert_eq!(fx.count(&fx.p2, ORE), 1);
    assert_eq!(d.data().func_group_id, CHEST_DONE);
}

/// A skill hit (an AoE on a `DoodadFuncSkillHit` doodad) still runs while another unit holds it.
#[tokio::test]
async fn skill_hit_runs_during_another_units_hold() {
    let mut fx = fx();
    let d = fx.doodad(HIT);
    let w1 = WorldObject::from(&fx.p1);
    let w2 = WorldObject::from(&fx.p2);
    let skill = fx.run_skill(TREE_SKILL);

    assert!(super::occupancy::begin_cast(&fx.server, &d, &w1, &skill));
    fx.c2.drain();
    let ctx = fx.ctx(&d);
    super::phase::on_skill_hit(&ctx, Some(&w2), HIT_SKILL);
    drop(ctx);
    let frames = fx.c2.drain();
    assert_not_refused(&frames);
    assert_eq!(d.data().func_group_id, HIT_NEXT);
}
