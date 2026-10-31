//! Lost-experience recovery at a Nui priest (`RecoverExpEffect`, the "Offer Prayer" cast).
//!
//! The 1.2 client casts skill 17063 as a plain `CSStartSkill` from its own priest interaction
//! window (`scriptsbin/x2ui/messageboximpl/message_box_from_npc_interaction.alb` ->
//! `X2Priest:RecoverExp`); no static data table names it, so the client-cast hardening refused
//! it with `NO_PERM` until `SkillData` learned the effect-based source in
//! [`crate::data::skills::CLIENT_SKILL_EFFECT_SOURCES`]. The C# `CSStartSkillPacket` ran any
//! template skill in its fallback branch, so the prayer worked there.
//!
//! These tests load the source set from a compact-shaped SQLite database (the same loader as the
//! real `compact.sqlite3`) instead of marking the id by hand, so the client-source decision is
//! covered end to end: death loses exp into `recoverable_exp`, the prayer cast at a priest
//! restores it and spends labor, and the effect's own priest / labor checks still apply.

use std::sync::Arc;

use rusqlite::Connection;

use crate::data::GameData;
use crate::opcodes::sc;
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::test_support::*;
use crate::world::{NpcRef, PlayerRef, WorldObject};

use super::data::effects::{EffectTemplate, RecoverExpEffect};
use super::data::EffectKind;
use super::handlers::{start_skill, CSStartSkillPacket};
use super::types::{KillReason, SkillCastTarget, SkillCaster, SkillObject};

/// Skill 17063 "Offer Prayer" (`recover_exp_effects` 1, the priest needs to be within 10 m).
const PRAYER_SKILL: u32 = 17_063;
/// `effects.id` of the prayer's `RecoverExpEffect` row.
const PRAYER_EFFECT: u32 = 18_268;
/// Skill 26611 "Full XP Restoration" (`recover_exp_effects` 5: no priest, no labor), the item
/// skill of the XP Restoration Scrolls 33965 / 33966 / 34024-34029 (`items.use_skill_id`).
const SCROLL_SKILL: u32 = 26_611;
/// One of the real XP Restoration Scrolls (33965, `use_skill_id` 26611, used as a reagent).
const SCROLL_ITEM: u32 = 33_965;
/// `effects.id` of the scroll's `RecoverExpEffect` row.
const SCROLL_EFFECT: u32 = 39_213;
/// `NpcTemplate` id of the Nui temple priest.
const PRIEST: u32 = 502;
const PLAYER_OBJ: u32 = 0x100;
const PRIEST_OBJ: u32 = 0x200;
/// `test_experience` totals: level 10 starts at 10 000, level 11 at 11 000.
const LEVEL_10_START: i32 = 10_000;
const LEVEL_10_SPAN: i32 = 1_000;

/// `SkillData` as `compact.sqlite3` shapes it: 17063 is named only by the `effects` table
/// (`actual_type = 'RecoverExpEffect'`, `need_priest = 't'`), while 26611 is named by
/// `items.use_skill_id`; both are the client-sourced rules under test.
fn client_source_skills() -> crate::data::SkillData {
    client_source_skills_with_items(true)
}

/// `with_items` false removes the scroll from the source set: 26611 must then stay unsourced,
/// because effect 5 (`need_priest = 'f'`) is not a cast of the client's priest window.
fn client_source_skills_with_items(with_items: bool) -> crate::data::SkillData {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE skills (id INT, show TEXT, ability_id INT, ability_level INT, \
             level_step INT, skill_points INT, auto_learn TEXT, need_learn TEXT);
         INSERT INTO skills VALUES (17063, 't', 0, 1, 1, 1, 't', 't');
         INSERT INTO skills VALUES (26611, 't', 0, 1, 1, 1, 't', 't');
         CREATE TABLE default_skills (skill_id INT, slot_index INT, add_to_slot TEXT);
         CREATE TABLE passive_buffs (id INT, ability_id INT, level INT, buff_id INT, \
             req_points INT, active TEXT);
         CREATE TABLE buffs (id INT, kind_id INT, duration INT, level_duration INT, \
             tick REAL, stack_rule_id INT, max_stack INT, init_min_charge INT);
         CREATE TABLE effects (id INT, actual_id INT, actual_type TEXT);
         INSERT INTO effects VALUES (18268, 1, 'RecoverExpEffect');
         INSERT INTO effects VALUES (39213, 5, 'RecoverExpEffect');
         CREATE TABLE skill_effects (skill_id INT, effect_id INT);
         INSERT INTO skill_effects VALUES (17063, 18268);
         INSERT INTO skill_effects VALUES (26611, 39213);
         CREATE TABLE recover_exp_effects (id INT, need_priest TEXT);
         INSERT INTO recover_exp_effects VALUES (1, 't');
         INSERT INTO recover_exp_effects VALUES (5, 'f');
         CREATE TABLE items (id INT, use_skill_id INT);",
    )
    .unwrap();
    if with_items {
        conn.execute("INSERT INTO items VALUES (33965, 26611)", [])
            .unwrap();
    }
    crate::data::SkillData::load(&conn).unwrap()
}

/// NPC data + the experience table + the prayer and scroll skills with their `RecoverExpEffect`
/// rows, and the Nui priest template.
fn game_data() -> GameData {
    use crate::systems::skills::tests::{skill_effect, skill_template};

    let mut gd = crate::systems::npcs::npc::manager::tests::npc_game_data();
    gd.experience = test_experience(55);
    gd.skills = client_source_skills();
    let s = &mut gd.systems.skills;
    s.add_effect(
        EffectKind::RecoverExpEffect,
        EffectTemplate::RecoverExp(RecoverExpEffect {
            id: 1,
            need_money: false,
            need_labor_power: true,
            need_priest: true,
        }),
        PRAYER_EFFECT,
    );
    let mut prayer = skill_template(PRAYER_SKILL);
    prayer.target_type = 0; // self, as in `skills.target_type_id`
    prayer.damage_type_id = 0;
    prayer.max_range = 4;
    prayer.default_gcd = false;
    prayer.casting_time = 0;
    prayer.level_rule_no_consideration = true;
    let mut effect = skill_effect(PRAYER_EFFECT, EffectKind::RecoverExpEffect, 1);
    effect.friendly = true;
    effect.non_friendly = true;
    prayer.effects.push(effect);
    s.add_skill(prayer);
    s.add_effect(
        EffectKind::RecoverExpEffect,
        EffectTemplate::RecoverExp(RecoverExpEffect {
            id: 5,
            need_money: false,
            need_labor_power: false,
            need_priest: false,
        }),
        SCROLL_EFFECT,
    );
    let mut scroll = skill_template(SCROLL_SKILL);
    scroll.target_type = 0;
    scroll.damage_type_id = 0;
    scroll.max_range = 4;
    scroll.default_gcd = false;
    scroll.casting_time = 0;
    scroll.level_rule_no_consideration = true;
    let mut effect = skill_effect(SCROLL_EFFECT, EffectKind::RecoverExpEffect, 5);
    effect.friendly = true;
    effect.non_friendly = true;
    // The real `skill_effects` row consumes one source item (the used scroll).
    effect.consume_item_count = 1;
    scroll.effects.push(effect);
    s.add_skill(scroll);
    let mut scroll_item = crate::data::items::ItemTemplate::plain(SCROLL_ITEM, 100);
    scroll_item.use_skill_id = SCROLL_SKILL;
    scroll_item.use_skill_as_reagent = true;
    gd.items.templates.insert(SCROLL_ITEM, scroll_item);
    gd.systems.npcs.npc.templates.insert(
        PRIEST,
        NpcTemplate {
            id: PRIEST,
            name: "Nui Temple Priest".into(),
            level: 50,
            faction_id: 101,
            priest: true,
            ..Default::default()
        },
    );
    gd
}

/// A server with the hooks `systems::start_all` installs (the delay effect and the death tail
/// need them).
fn server() -> Arc<crate::GameServer> {
    let server = Arc::new(test_server_with(game_data()));
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
    server: Arc<crate::GameServer>,
    client: TestClient,
    player: PlayerRef,
    priest: NpcRef,
}

/// `priest_distance` is the priest's x offset from the player at (100, 100).
fn fixture(priest_distance: f32) -> Fx {
    let server = server();
    let mut client = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "Prayer", 100.0, 100.0);
    character.unit.level = 10;
    character.experience = LEVEL_10_START + 500;
    character.unit.hp = 5_000;
    character.unit.max_hp = 5_000;
    character.unit.mp = 1_000;
    character.unit.max_mp = 1_000;
    character.labor_power = 100;
    character.is_online = true;
    let player = spawn_player(&server, &client, character, PLAYER_OBJ);
    client.conn.active_char = Some(player.clone());
    let priest = crate::systems::npcs::npc::manager::create(&server, PRIEST_OBJ, PRIEST)
        .expect("priest template");
    {
        let mut n = priest.data();
        n.unit.hp = 10_000;
        n.unit.max_hp = 10_000;
        n.unit.mp = 1_000;
        n.unit.max_mp = 1_000;
        n.unit
            .transform
            .set_xyz(100.0 + priest_distance, 100.0, 100.0);
    }
    let world = server.world.world(0).expect("instance 0");
    world.spawn(&priest, &server.view_ctx());
    client.drain();
    Fx {
        server,
        client,
        player,
        priest,
    }
}

impl Fx {
    /// `Character.DoDie` with the priest as the killer (any unit is enough; the death tail only
    /// needs a killer).
    fn die(&mut self) {
        let victim = WorldObject::from(&self.player);
        let killer = WorldObject::from(&self.priest);
        crate::systems::skills::combat::do_die(
            &self.server,
            &victim,
            Some(&killer),
            KillReason::Damage,
        );
    }

    /// Restores the HP pool, which starts a new life for the death claim.
    fn revive_for_next_death(&mut self) {
        let victim = WorldObject::from(&self.player);
        self.player.data().unit.hp = 5_000;
        crate::systems::skills::combat::post_update_current_hp(
            &self.server,
            &victim,
            Some(&victim),
            0,
            5_000,
            KillReason::Unknown,
        );
        self.player.data().unit.skill_last_used_ms = 0;
        self.client.drain();
    }

    fn cast(&mut self, skill_id: u32) {
        self.player.data().unit.skill_last_used_ms = 0;
        let packet = CSStartSkillPacket {
            skill_id,
            caster: SkillCaster::Unit(self.player.obj_id),
            target: SkillCastTarget::Unit(self.player.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        };
        start_skill(&self.server, &mut self.client.conn, packet);
    }

    /// Puts one real XP Restoration Scroll into the bag and returns its item id.
    fn give_scroll(&self) -> u64 {
        use crate::models::container::slot_type;
        use crate::models::item::Item;
        use crate::systems::economy::inventory::Inv;
        let template = self
            .server
            .game_data
            .items
            .template(SCROLL_ITEM)
            .expect("scroll template");
        let mut item = Item::create(0x7001, template, 1, 0, 1, &self.server.game_data.items, 0);
        item.owner_id = u64::from(self.player.character_id);
        item.slot_type = slot_type::INVENTORY;
        item.slot = 0;
        let bag = Inv::of(&self.player).bag();
        bag.lock().unwrap().items.push(item);
        self.server.items.add_container(bag);
        0x7001
    }

    /// The 1.2 client's `SkillItem` cast of the scroll: the item is the caster and its use skill
    /// is the scroll's own skill.
    fn cast_scroll(&mut self, item_id: u64) {
        self.player.data().unit.skill_last_used_ms = 0;
        let packet = CSStartSkillPacket {
            skill_id: SCROLL_SKILL,
            caster: SkillCaster::Item {
                obj_id: self.player.obj_id,
                item_id,
                item_template_id: SCROLL_ITEM,
                type1: 0,
                type2: 0,
            },
            target: SkillCastTarget::Unit(self.player.obj_id),
            flag: 0,
            skill_object: SkillObject::None,
        };
        start_skill(&self.server, &mut self.client.conn, packet);
    }

    fn pray(&mut self) {
        self.cast(PRAYER_SKILL);
    }
}

/// The death penalty of one level-10 death: 5 % of the level span, 80 % of it recoverable.
fn first_loss() -> i32 {
    (f64::from(LEVEL_10_SPAN) * 0.05).floor() as i32
}
fn first_recoverable() -> i32 {
    (first_loss() as f32 * 0.8).round() as i32
}

#[test]
fn recover_exp_skills_are_client_sourced_from_the_effect_table() {
    let data = client_source_skills();
    assert!(
        data.is_client_sourced_skill(PRAYER_SKILL),
        "the priest prayer is cast by the client but named by no source table"
    );
    assert!(
        !data.is_item_only_skill(PRAYER_SKILL),
        "the effect source keeps the prayer a plain cast, not an item skill"
    );
    assert!(
        !data.is_client_sourced_skill(PRAYER_SKILL + 1),
        "an unrelated skill must stay unsourced"
    );
}

#[test]
fn scroll_skill_is_sourced_by_items_not_by_the_priest_window() {
    let data = client_source_skills();
    assert!(data.is_client_sourced_skill(PRAYER_SKILL));
    assert!(
        data.is_client_sourced_skill(SCROLL_SKILL),
        "the scrolls name 26611 through items.use_skill_id"
    );
    assert!(
        data.is_item_only_skill(SCROLL_SKILL),
        "26611 is reached only through the scroll items"
    );

    let without_items = client_source_skills_with_items(false);
    assert!(without_items.is_client_sourced_skill(PRAYER_SKILL));
    assert!(
        !without_items.is_client_sourced_skill(SCROLL_SKILL),
        "effect 5 has need_priest = 'f': the priest window does not cast it"
    );
}

/// The real 1.2 `compact.sqlite3` keys `recover_exp_effects` by `effects.actual_id`: the prayer's
/// effect 18268 has `actual_id` 1 (`need_priest = 't'`) while the scroll's 39213 has `actual_id`
/// 5 (`need_priest = 'f'`). Joining the source table on `effects.id` returns no rows at all.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_db_sources_the_prayer_through_effects_actual_id() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let data = crate::data::SkillData::load(&conn).unwrap();
    assert!(
        data.is_client_sourced_skill(PRAYER_SKILL),
        "the priest prayer must be client-sourced from the effect tables"
    );
    let ids = crate::data::skills::effect_sourced_skills(
        &conn,
        "RecoverExpEffect",
        "recover_exp_effects",
        "need_priest",
    )
    .unwrap();
    assert!(
        ids.contains(&PRAYER_SKILL),
        "the prayer must be reached through effects.actual_id: {ids:?}"
    );
    assert!(
        !ids.contains(&SCROLL_SKILL),
        "effect 5 has need_priest = 'f': the priest window does not cast 26611: {ids:?}"
    );
}

#[tokio::test]
async fn prayer_at_a_priest_restores_the_lost_experience() {
    let mut fx = fixture(2.0);
    fx.die();
    {
        let c = fx.player.data();
        assert_eq!(c.experience, LEVEL_10_START + 500 - first_loss());
        assert_eq!(c.recoverable_exp, first_recoverable());
    }
    fx.client.drain();

    fx.pray();
    {
        let c = fx.player.data();
        assert_eq!(
            c.experience,
            LEVEL_10_START + 500 - first_loss() + first_recoverable(),
            "the prayer must restore the recoverable experience"
        );
        assert_eq!(c.recoverable_exp, 0);
        assert_eq!(c.labor_power, 100 - 10, "the prayer costs level labor");
    }
    let frames = fx.client.drain();
    let ops: Vec<u16> = frames.iter().map(|(op, _)| *op).collect();
    assert!(ops.contains(&sc::SCRecoverableExpPacket), "{ops:x?}");
    assert!(
        ops.contains(&sc::SCCharacterLaborPowerChangedPacket),
        "{ops:x?}"
    );
    assert!(
        !frames.iter().any(|(op, body)| {
            *op == sc::SCSkillStartedPacket && body.last() == Some(&0x2D) // NO_PERM
        }),
        "the client cast must not be refused: {frames:x?}"
    );
    let labor_amount = frames
        .iter()
        .find(|(op, _)| *op == sc::SCCharacterLaborPowerChangedPacket)
        .map(|(_, body)| i32::from_le_bytes(body[0..4].try_into().unwrap()));
    assert_eq!(
        labor_amount,
        Some(-10),
        "the prayer spends exactly the level-10 labor"
    );
}

#[tokio::test]
async fn scroll_restores_the_lost_experience_without_spending_labor() {
    use crate::systems::economy::inventory::Inv;

    let mut fx = fixture(2.0);
    fx.player.data().labor_power = 0;
    fx.die();
    let (experience, recoverable) = {
        let c = fx.player.data();
        (c.experience, c.recoverable_exp)
    };
    assert!(recoverable > 0);
    fx.client.drain();

    let scroll = fx.give_scroll();
    fx.cast_scroll(scroll);
    {
        let c = fx.player.data();
        assert_eq!(
            c.experience,
            experience + recoverable,
            "the scroll must restore the recoverable experience"
        );
        assert_eq!(c.recoverable_exp, 0);
        assert_eq!(
            c.labor_power, 0,
            "a labor-free effect must not subtract the level cost (or wrap labor below 0)"
        );
    }
    let frames = fx.client.drain();
    assert!(
        !frames
            .iter()
            .any(|(op, _)| *op == sc::SCCharacterLaborPowerChangedPacket),
        "no labor change for a labor-free effect: {frames:x?}"
    );
    let ops: Vec<u16> = frames.iter().map(|(op, _)| *op).collect();
    assert!(ops.contains(&sc::SCRecoverableExpPacket), "{ops:x?}");
    assert_eq!(
        Inv::of(&fx.player).items_count(SCROLL_ITEM, -1),
        0,
        "the scroll is consumed by the Item caster path"
    );
}

/// 26611 is item-only: a plain Unit cast without the scroll must be refused before the effect.
#[tokio::test]
async fn scroll_skill_plain_unit_cast_is_refused_without_the_scroll() {
    let mut fx = fixture(2.0);
    fx.player.data().labor_power = 0;
    fx.die();
    let (experience, recoverable) = {
        let c = fx.player.data();
        (c.experience, c.recoverable_exp)
    };
    assert!(recoverable > 0);
    fx.client.drain();

    fx.cast(SCROLL_SKILL);
    let c = fx.player.data();
    assert_eq!(c.experience, experience, "the exploit must not restore exp");
    assert_eq!(c.recoverable_exp, recoverable);
    assert_eq!(c.labor_power, 0);
    let frames = fx.client.drain();
    let refused = frames.iter().any(|(op, body)| {
        *op == sc::SCSkillStartedPacket
            && body.len() >= 4
            && u32::from_le_bytes(body[0..4].try_into().unwrap()) == SCROLL_SKILL
            && body.last() == Some(&0x2D) // NO_PERM
    });
    assert!(refused, "the plain cast must get NO_PERM: {frames:x?}");
    assert!(
        !frames
            .iter()
            .any(|(op, _)| *op == sc::SCRecoverableExpPacket),
        "the effect must not run: {frames:x?}"
    );
}

#[tokio::test]
async fn prayer_without_a_nearby_priest_is_refused_by_the_effect() {
    let mut fx = fixture(100.0);
    fx.die();
    let (experience, recoverable) = {
        let c = fx.player.data();
        (c.experience, c.recoverable_exp)
    };
    assert!(recoverable > 0);
    fx.client.drain();

    fx.pray();
    let c = fx.player.data();
    assert_eq!(c.experience, experience, "no exp may be restored");
    assert_eq!(c.recoverable_exp, recoverable);
    assert_eq!(c.labor_power, 100, "no labor may be spent");
    let frames = fx.client.drain();
    let too_far_away = frames.iter().any(|(op, body)| {
        *op == sc::SCErrorMsgPacket
            && body.len() >= 2
            && i16::from_le_bytes([body[0], body[1]]) == 154 // TOO_FAR_AWAY
    });
    assert!(
        too_far_away,
        "the effect must answer TOO_FAR_AWAY, not the cast filter: {frames:x?}"
    );
}

#[tokio::test]
async fn second_death_replaces_the_recoverable_exp_and_one_prayer_restores_only_it() {
    let mut fx = fixture(2.0);
    fx.die();
    assert_eq!(fx.player.data().recoverable_exp, first_recoverable());
    fx.revive_for_next_death();

    // Start the next life near the level start so the second loss is smaller and distinguishable
    // (as `Character.DoDie` does, the new death overwrites `RecoverableExp`).
    {
        let mut c = fx.player.data();
        c.experience = LEVEL_10_START + 500;
        c.unit.level = 11;
        c.experience = 11_000 + 10; // level 11, 10 exp into the level
    }
    fx.die();
    {
        let c = fx.player.data();
        assert_eq!(c.experience, 11_000, "loss capped to this level's exp");
        assert_eq!(c.recoverable_exp, 8, "80 % of the 10 exp loss");
    }
    fx.client.drain();

    fx.pray();
    let c = fx.player.data();
    assert_eq!(c.experience, 11_008);
    assert_eq!(c.recoverable_exp, 0);
}

#[tokio::test]
async fn recoverable_exp_carried_into_a_new_session_is_restored_by_the_prayer() {
    // A relog loads `characters.recoverable_exp` into the fresh character (db_character.rs);
    // the prayer then sees it exactly like after the death.
    let mut fx = fixture(2.0);
    {
        let mut c = fx.player.data();
        c.experience = LEVEL_10_START + 500 - first_loss();
        c.recoverable_exp = first_recoverable();
    }
    fx.client.drain();

    fx.pray();
    let c = fx.player.data();
    assert_eq!(
        c.experience,
        LEVEL_10_START + 500 - first_loss() + first_recoverable()
    );
    assert_eq!(c.recoverable_exp, 0);
}
