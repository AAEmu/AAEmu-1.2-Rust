//! Flow tests of the AI subsystem without MySQL: an aggressive NPC sees a player, chases and
//! hits it with its base skill, and returns home (and resets) when the player runs away; the
//! state machine, aggro help, idle-to-default transitions, path following, the return timeout
//! teleport and the hooks used by the NPC / skills subsystems.

use std::sync::Arc;
use std::time::Duration;

use crate::data::formulas::FormulaManager;
use crate::data::GameData;
use crate::models::movement::GameStanceType;
use crate::models::npc::Npc;
use crate::models::unit::{Bonus, BonusTemplate, UnitAttribute, UnitModifierType};
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::skills::controllers::Vec3;
use crate::systems::skills::data::effects::{BuffEffect, DamageEffect, EffectTemplate};
use crate::systems::skills::data::{
    BuffTemplate, BuffTriggerTemplate, EffectKind, SkillEffect, SkillTemplate,
};
use crate::systems::skills::state::AggroKind;
use crate::systems::skills::types::BuffEventTriggerKind;
use crate::test_support::*;
use crate::world::{Entity, NpcRef, PlayerRef, WorldObject};

use super::data::{ActorModel, GameStance};
use super::framework::{AiKind, BehaviorKind as B};
use super::manager::{post, tick_all};
use super::params::{AiCommandCategory, AiCommandSets, AiCommands};
use super::path_handler::{AiPathPoint, AiPathPointAction};

const PLAYER_OBJ: u32 = 0x100;
const NPC_OBJ: u32 = 0x200;
const HELPER_OBJ: u32 = 0x201;
const NPC_TEMPLATE: u32 = 7000;
const PASSIVE_TEMPLATE: u32 = 7001;
const HELPER_TEMPLATE: u32 = 7002;
const MODEL: u32 = 900;
const HIT_SKILL: u32 = 10_001;
const HIT_DAMAGE: i32 = 40;

fn damage_effect(id: u32, amount: i32) -> DamageEffect {
    DamageEffect {
        id,
        damage_type: 1,
        fixed_min: amount,
        fixed_max: amount,
        multiplier: 1.0,
        use_mainhand_weapon: false,
        use_offhand_weapon: false,
        use_ranged_weapon: false,
        critical_bonus: 0,
        target_buff_tag_id: 0,
        target_buff_bonus: 0,
        use_fixed_damage: true,
        use_level_damage: false,
        level_md: 0.0,
        level_va_start: 0,
        level_va_end: 0,
        target_buff_bonus_mul: 1.0,
        use_charged_buff: false,
        charged_buff_id: 0,
        charged_mul: 0.0,
        aggro_multiplier: 1.0,
        health_steal_ratio: 0,
        mana_steal_ratio: 0,
        dps_multiplier: 0.0,
        weapon_slot_id: -1,
        check_crime: false,
        hit_anim_timing_id: 0,
        use_target_charged_buff: false,
        target_charged_buff_id: 0,
        target_charged_mul: 0.0,
        dps_inc_multiplier: 0.0,
        engage_combat: true,
        synergy: false,
        actability_group_id: 0,
        actability_step: 0,
        actability_mul: 0.0,
        actability_add: 0.0,
        charged_level_mul: 0.0,
        adjust_damage_by_height: false,
        use_percent_damage: false,
        percent_min: 0,
        percent_max: 0,
        use_current_health: false,
        target_health_min: 0,
        target_health_max: 0,
        target_health_mul: 0.0,
        target_health_add: 0,
        fire_proc: false,
        bonuses: Vec::new(),
    }
}

fn skill_effect(effect_id: u32, kind: EffectKind, actual: u32) -> SkillEffect {
    SkillEffect {
        effect_id,
        template: Some((kind, actual)),
        weight: 0,
        start_level: 0,
        end_level: 255,
        friendly: false,
        non_friendly: false,
        target_buff_tag_id: 0,
        target_no_buff_tag_id: 0,
        source_buff_tag_id: 0,
        source_no_buff_tag_id: 0,
        chance: 100,
        front: false,
        back: false,
        target_npc_tag_id: 0,
        application_method: 1,
        consume_source_item: false,
        consume_item_id: 0,
        consume_item_count: 0,
        always_hit: false,
        item_set_id: 0,
        interaction_success_hit: false,
    }
}

fn stance(
    stance_id: GameStanceType,
    walk: f32,
    run: f32,
    sprint: f32,
) -> (GameStanceType, GameStance) {
    (
        stance_id,
        GameStance {
            id: u32::from(stance_id.0) + 1,
            stance_id,
            actor_model_id: 1,
            ai_move_speed_run: run,
            ai_move_speed_slow: walk,
            ai_move_speed_sprint: sprint,
            ai_move_speed_walk: walk,
            height_collider: 0.0,
            height_pivot: 0.0,
            normal_speed: run,
            max_speed: 20.0,
            size: [0.0; 3],
            model_offset: [0.0; 3],
            view_offset: [0.0; 3],
            use_capsule: true,
        },
    )
}

/// An aggressive hold-position NPC template whose base skill hits for 40 within 4 m, a passive
/// helper template, the actor model with run / walk / sprint speeds, and the NpcReturn buff.
fn stealth_formulas() -> FormulaManager {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE unit_formulas (id INTEGER, formula TEXT, kind_id INTEGER, owner_type_id INTEGER);
         CREATE TABLE unit_formula_variables (id INTEGER, unit_formula_id INTEGER, variable_kind_id INTEGER, key INTEGER, value REAL);
         CREATE TABLE wearable_formulas (id INTEGER, kind_id INTEGER, formula TEXT);
         CREATE TABLE formulas (id INTEGER, formula TEXT);
         INSERT INTO formulas VALUES
           (4, 'min(40, 12 * (1 + (source_level - target_level) / 50) / (1 + stealth_level / 50))'),
           (10, '0.5');",
    )
    .unwrap();
    FormulaManager::load(&conn).unwrap()
}

/// [`stealth_formulas`] with formula 4 referring to a variable the predicate never sets: a
/// prepared formula whose evaluation fails.
fn broken_stealth_formulas() -> FormulaManager {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE unit_formulas (id INTEGER, formula TEXT, kind_id INTEGER, owner_type_id INTEGER);
         CREATE TABLE unit_formula_variables (id INTEGER, unit_formula_id INTEGER, variable_kind_id INTEGER, key INTEGER, value REAL);
         CREATE TABLE wearable_formulas (id INTEGER, kind_id INTEGER, formula TEXT);
         CREATE TABLE formulas (id INTEGER, formula TEXT);
         INSERT INTO formulas VALUES
           (4, 'unknown_variable + 1'),
           (10, '0.5');",
    )
    .unwrap();
    FormulaManager::load(&conn).unwrap()
}

fn data() -> GameData {
    let mut data = GameData::empty();
    data.formulas = stealth_formulas();
    let s = &mut data.systems.skills;
    s.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(damage_effect(1, HIT_DAMAGE))),
        900_001,
    );
    let mut hit = SkillTemplate {
        id: HIT_SKILL,
        target_type: 4, // Hostile
        max_range: 4,
        level_rule_no_consideration: true,
        damage_type_id: 1,
        default_gcd: false,
        ..SkillTemplate::default()
    };
    hit.effects
        .push(skill_effect(900_001, EffectKind::DamageEffect, 1));
    s.add_skill(hit);
    s.add_buff(BuffTemplate {
        id: super::npc::BUFF_NPC_RETURN,
        kind: 1,
        duration: 0,
        stack_rule: 1,
        ..BuffTemplate::default()
    });
    for (id, stealth, detect_stealth, anti_stealth) in [
        (STEALTH_COLUMN_BUFF, true, false, false),
        (DETECT_STEALTH_BUFF, false, true, false),
        (ANTI_STEALTH_BUFF, false, false, true),
    ] {
        s.add_buff(BuffTemplate {
            id,
            kind: 1,
            duration: 60_000,
            stack_rule: 1,
            stealth,
            detect_stealth,
            anti_stealth,
            ..BuffTemplate::default()
        });
    }
    let base = NpcTemplate {
        id: NPC_TEMPLATE,
        model_id: MODEL,
        level: 10,
        scale: 1.0,
        aggression: true,
        attack_start_range_scale: 2.0,
        sight_range_scale: 1.0,
        sight_fov_scale: 2.0,
        base_skill_id: HIT_SKILL as i32,
        base_skill_delay: 0.2,
        ai_file_id: 15, // HoldPosition
        accept_aggro_link: true,
        aggro_link_help_dist: 30.0,
        ..NpcTemplate::default()
    };
    // not aggressive, blind (no alerts)
    let passive = NpcTemplate {
        id: PASSIVE_TEMPLATE,
        aggression: false,
        sight_range_scale: 0.0,
        ..base.clone()
    };
    // aggressive (helps without a sight check) but with a 1 m aggression radius
    let helper = NpcTemplate {
        id: HELPER_TEMPLATE,
        attack_start_range_scale: 0.1,
        sight_range_scale: 0.0,
        ..base.clone()
    };
    data.systems.npcs.npc.templates.insert(NPC_TEMPLATE, base);
    data.systems
        .npcs
        .npc
        .templates
        .insert(PASSIVE_TEMPLATE, passive);
    data.systems
        .npcs
        .npc
        .templates
        .insert(HELPER_TEMPLATE, helper);
    // ModelManager.GetActorModel(MODEL).Radius as seen by the skills geometry
    data.systems.skills.model_radii.insert(MODEL, 0.5);
    data.systems.ai.ai.actor_models.insert(
        MODEL,
        Arc::new(ActorModel {
            id: 1,
            radius: 0.5,
            height: 2.0,
            movement_id: 0,
            underwater_creature: false,
            stances: vec![
                stance(GameStanceType::COMBAT, 2.0, 5.0, 8.0),
                stance(GameStanceType::RELAXED, 2.0, 5.0, 8.0),
            ],
        }),
    );
    // The two `ai_command_sets` states of WP-5b (the real table: 302 'f', 23 't').
    data.systems.ai.ai.command_sets.insert(
        LOCKED_SET,
        AiCommandSets {
            id: LOCKED_SET,
            name: "locked".into(),
            can_interact: false,
        },
    );
    data.systems.ai.ai.command_sets.insert(
        OPEN_SET,
        AiCommandSets {
            id: OPEN_SET,
            name: "open".into(),
            can_interact: true,
        },
    );
    data
}

/// A command set whose `can_interact` is `'f'`.
const LOCKED_SET: u32 = 9001;
/// A command set whose `can_interact` is `'t'`.
const OPEN_SET: u32 = 9002;
/// A command set id absent from `ai_command_sets` (an NPC without a set).
const PLAIN_SET: u32 = 9003;

struct Fixture {
    server: Arc<GameServer>,
    client: TestClient,
    player: PlayerRef,
    npc: NpcRef,
}

fn server() -> Arc<GameServer> {
    server_with_data(data())
}

/// [`data`]'s server setup (shared by [`server`]).
fn server_with_data(data: GameData) -> Arc<GameServer> {
    let server = Arc::new(test_server_with(data));
    server.set_self();
    // The command-set interaction lock is opt-in; these flow tests exercise it.
    server
        .config
        .update(|cfg| cfg.interaction.command_set_lock = true);
    crate::systems::skills::bind(&server);
    // The AI parts of the npcs NpcSkillHooks (`npcs::bridges::NpcSkills` at startup; its npc
    // stat overrides need npc data the bare test NPCs do not have).
    server.systems.skills.ext.npcs.install(Arc::new(AiParts));
    super::hooks::install(&server);
    server
}

/// [`data`] plus skill 2 (the melee hack's id) with max range 25; `PASSIVE_TEMPLATE` uses it
/// so the melee guard has to see the 3D distance of a flying NPC.
fn data_with_melee_skill() -> GameData {
    let mut data = data();
    let mut melee = data
        .systems
        .skills
        .skill(HIT_SKILL)
        .expect("hit skill")
        .as_ref()
        .clone();
    melee.id = 2;
    melee.max_range = 25;
    data.systems.skills.add_skill(melee);
    data.systems
        .npcs
        .npc
        .templates
        .get_mut(&PASSIVE_TEMPLATE)
        .expect("passive template")
        .base_skill_id = 2;
    data
}

/// Creates an NPC with an AI and spawns it like `NpcSpawnerNpc.Spawn` (home position, Spawning).
fn spawn_npc(
    server: &Arc<GameServer>,
    obj_id: u32,
    template: u32,
    x: f32,
    y: f32,
    kind: AiKind,
) -> NpcRef {
    let mut n = Npc::new(template);
    n.unit.name = "Wolf".into();
    n.unit.level = 10;
    n.unit.hp = 1000;
    n.unit.max_hp = 1000;
    n.unit.mp = 100;
    n.unit.max_mp = 100;
    n.unit.model_id = MODEL;
    n.unit.scale = 1.0;
    n.unit.transform.set_xyz(x, y, 100.0);
    n.unit.transform.zone_id = TEST_ZONE;
    // `NpcManager.Create` adds the template's `unit_modifiers` (`owner_type='Npc'`).
    if let Some(t) = server.game_data.systems.npcs.npc.template(template) {
        for bt in &t.bonuses {
            n.unit.bonuses.add_bonus(
                0,
                Bonus {
                    template: bt.clone(),
                    value: bt.value,
                },
            );
        }
    }
    let npc = Entity::new(obj_id, n);
    server.systems.ai.ais.add_ai(&npc, kind);
    let world = server.world.world(0).expect("world");
    world.spawn(&npc, &server.view_ctx());
    super::hooks::spawn_positioned(server, &npc);
    npc
}

fn fixture(player_x: f32) -> Fixture {
    let server = server();
    let client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Runner", player_x, 100.0);
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    let player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        NPC_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    Fixture {
        server,
        client,
        player,
        npc,
    }
}

fn behavior(server: &GameServer, id: u32) -> Option<B> {
    server
        .systems
        .ai
        .ais
        .inspect(id, |a| a.current_behavior())
        .flatten()
}

fn npc_pos(npc: &NpcRef) -> Vec3 {
    let t = npc.data().unit.transform;
    Vec3::new(t.x, t.y, t.z)
}

/// Ticks the AI manager (`delta_ms` per tick, `sleep_ms` real time between ticks) until `done`.
async fn tick_until(
    server: &Arc<GameServer>,
    delta_ms: i64,
    sleep_ms: u64,
    max: usize,
    mut done: impl FnMut() -> bool,
) -> bool {
    for _ in 0..max {
        tick_all(server, delta_ms);
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
    }
    false
}

/// Opcodes of the drained frames, with the level-4 (compressed) batches opened.
fn frame_ops(client: &mut TestClient) -> Vec<u16> {
    let mut out = Vec::new();
    for (op, body) in client.drain() {
        if op & 0x8000 != 0 && (op & 0x7FFF) != 0 {
            if let Some(raw) = crate::systems::skills::packets::inflate_stored(&body) {
                // each batched packet starts with [u16 0][u16 opcode]; the first one is enough
                // for the tests (the skill code batches one packet per frame here)
                if raw.len() >= 4 {
                    out.push(u16::from_le_bytes([raw[2], raw[3]]));
                }
                continue;
            }
        }
        out.push(op);
    }
    out
}

fn move_player(server: &GameServer, p: &PlayerRef, x: f32, y: f32) {
    p.data().unit.transform.set_xyz(x, y, 100.0);
    let world = server.world.world(0).expect("world");
    world.add_visible_object(p, &server.view_ctx());
}

#[tokio::test]
async fn aggressive_npc_chases_hits_and_returns_home() {
    let mut f = fixture(115.0);
    let server = f.server.clone();
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Spawning));
    // Spawning -> RunCommandSet -> Idle -> HoldPosition / Attack: the player 15 m away is
    // inside the 20 m aggression radius.
    let attacked = tick_until(&server, 100, 20, 50, || {
        behavior(&server, NPC_OBJ) == Some(B::Attack)
    })
    .await;
    assert!(
        attacked,
        "npc never attacked: {:?}",
        behavior(&server, NPC_OBJ)
    );
    {
        let n = f.npc.data();
        assert!(n.unit.is_in_battle);
        assert!(n.unit.skill_state.aggro_table.contains_key(&PLAYER_OBJ));
        assert_eq!(n.current_game_stance, GameStanceType::COMBAT);
    }
    f.client.drain();

    // Chase (run speed 5 m/s, 0.5 s per tick) and hit with the base skill.
    let start = npc_pos(&f.npc);
    let player = f.player.clone();
    let hit = tick_until(&server, 500, 160, 80, || player.data().unit.hp < 1000).await;
    assert!(hit, "player never damaged; npc at {:?}", npc_pos(&f.npc));
    let now = npc_pos(&f.npc);
    assert!(
        now.x > start.x + 9.0,
        "npc did not chase: {start:?} -> {now:?}"
    );
    assert_eq!((1000 - f.player.data().unit.hp) % HIT_DAMAGE, 0);
    let ops = frame_ops(&mut f.client);
    assert!(ops.contains(&sc::SCOneUnitMovementPacket), "{ops:x?}");
    assert!(ops.contains(&sc::SCTargetChangedPacket), "{ops:x?}");
    assert!(ops.contains(&sc::SCUnitDamagedPacket), "{ops:x?}");
    assert!(ops.contains(&sc::SCAggroTargetChanged), "{ops:x?}");
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);
    assert_eq!(f.npc.data().current_aggro_target, PLAYER_OBJ);

    // Hurt the NPC a little: ReturnState restores it.
    f.npc.data().unit.hp = 700;

    // The player runs 70 m away (beyond ReturnDistance 50 from the NPC, still in its region
    // neighbourhood): the NPC gives up, returns home at sprint speed and resets.
    move_player(&server, &f.player, 185.0, 100.0);
    let npc = f.npc.clone();
    let returned = tick_until(&server, 500, 30, 80, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(
        returned,
        "npc did not return: {:?} at {:?}",
        behavior(&server, NPC_OBJ),
        npc_pos(&npc)
    );
    let home = npc_pos(&f.npc);
    assert!(
        (home.x - 100.0).abs() < 1.0 && (home.y - 100.0).abs() < 1.0,
        "{home:?}"
    );
    let n = f.npc.data();
    assert!(n.unit.skill_state.aggro_table.is_empty());
    assert!(!n.unit.is_in_battle);
    assert_eq!(n.unit.hp, 1000, "ReturnState restores the hp");
    assert_eq!(
        n.current_aggro_target, PLAYER_OBJ,
        "CurrentAggroTarget is never reset by ReturnState"
    );
    assert_eq!(
        n.unit.current_target, NPC_OBJ,
        "HoldPosition targets itself"
    );
    assert!(
        !n.unit.buffs.check_buff(super::npc::BUFF_NPC_RETURN),
        "NpcReturn buff removed on exit"
    );
    assert_eq!(n.current_game_stance, GameStanceType::RELAXED);
    drop(n);
    // the player keeps the damage, and is no longer in this NPC's aggro list
    assert!(!f
        .player
        .data()
        .skill_char
        .is_in_aggro_list_of
        .contains(&NPC_OBJ));
}

#[tokio::test]
async fn passive_npc_fights_back_and_calls_for_help() {
    let server = server();
    let client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Hunter", 103.0, 100.0);
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    let player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::Roaming,
    );
    let helper = spawn_npc(
        &server,
        HELPER_OBJ,
        HELPER_TEMPLATE,
        95.0,
        100.0,
        AiKind::Default,
    );
    // settle both into their default behaviors (Roaming / DoNothing): no aggression
    tick_until(&server, 100, 120, 10, || false).await;
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Roaming));
    assert_eq!(behavior(&server, HELPER_OBJ), Some(B::DoNothing));
    assert!(npc.data().unit.skill_state.aggro_table.is_empty());

    // Npc.OnDamageReceived: aggro + OnAggroTargetChanged (Roaming -> Attack)
    let attacker = WorldObject::from(&player);
    super::combat::add_unit_aggro(
        &server,
        &WorldObject::from(&npc),
        AggroKind::Damage,
        &attacker,
        50,
    );
    server
        .systems
        .skills
        .ext
        .npcs()
        .npc_damage_received(&server, &npc, &attacker, 50);
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Attack));
    // UpdateTarget + UpdateAggroHelp: the helper (accepts aggro links, within 30 m) joins
    tick_all(&server, 100);
    assert_eq!(npc.data().unit.current_target, PLAYER_OBJ);
    assert_eq!(behavior(&server, HELPER_OBJ), Some(B::Attack));
    assert!(helper
        .data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&PLAYER_OBJ));
}

#[tokio::test]
async fn ai_state_machine_events_and_go_tos() {
    let server = server();
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    // GoToSpawn happened through the hook
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Spawning));
    // No transition for an event the behavior does not handle
    post(&server, NPC_OBJ, |c| c.on_no_aggro_target());
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Spawning));
    // Spawning -> RunCommandSet -> Idle -> HoldPosition (default) without players around
    // (no player activity: nothing ticks, so drive the behaviors directly)
    post(&server, NPC_OBJ, |c| c.go_to_idle());
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Idle));
    assert!(npc.data().ai_hold_or_idle);
    post(&server, NPC_OBJ, |c| c.go_to_default_behavior());
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::HoldPosition));
    assert_eq!(npc.data().unit.current_target, NPC_OBJ);
    // OnAggroTargetChanged from HoldPosition -> Attack, OnNoAggroTarget from Attack -> Return
    post(&server, NPC_OBJ, |c| c.on_aggro_target_changed());
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Attack));
    assert!(npc.data().unit.is_in_battle);
    post(&server, NPC_OBJ, |c| c.on_no_aggro_target());
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::ReturnState));
    assert!(!npc.data().unit.is_in_battle);
    assert!(npc
        .data()
        .unit
        .buffs
        .check_buff(super::npc::BUFF_NPC_RETURN));
    // GoToDead from anywhere
    post(&server, NPC_OBJ, |c| c.go_to_dead());
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Dead));
    assert!(!npc
        .data()
        .unit
        .buffs
        .check_buff(super::npc::BUFF_NPC_RETURN));
    // a behavior the character lacks is ignored
    post(&server, NPC_OBJ, |c| c.go_to_dummy());
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Dead));
}

/// A tower defense controller casts its wave plot (`OnSpawn`: seconds of projectile steps) while
/// its AI settles from Spawning into Idle / HoldPosition. Those behaviors interrupt the skills
/// of every other NPC, but must not cancel the plot of the event's own controllers, or no wave
/// ever appears.
#[tokio::test]
async fn event_controller_keeps_its_wave_plot_while_the_ai_settles() {
    use crate::systems::instances::tower_def::{adopt_for_test, EventKey};
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::plots::PlotState;
    use crate::systems::skills::skill::CastCtx;
    use crate::systems::skills::state::{PlotStateRef, SkillRun};
    use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};

    let server = server();
    let plot_of = |npc: &NpcRef| {
        let skill = SkillRun::new(
            Arc::new(SkillTemplate {
                id: 15311,
                ..SkillTemplate::default()
            }),
            1,
        );
        let caster = WorldObject::from(npc);
        let plot = PlotState::new(
            skill,
            CastCtx {
                caster: caster.clone(),
                caster_caster: SkillCaster::Unit(npc.obj_id),
                target: Target::Obj(caster),
                target_caster: SkillCastTarget::Unit(npc.obj_id),
                skill_object: SkillObject::None,
            },
        );
        npc.data().unit.skill_state.active_plot = Some(PlotStateRef(plot.clone()));
        plot
    };
    let ordinary = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let controller = spawn_npc(
        &server,
        HELPER_OBJ,
        HELPER_TEMPLATE,
        120.0,
        100.0,
        AiKind::HoldPosition,
    );
    adopt_for_test(
        &server,
        EventKey {
            world_id: 0,
            id: 3,
            zone_group_id: 5,
        },
        &controller,
    );
    let ordinary_plot = plot_of(&ordinary);
    let controller_plot = plot_of(&controller);
    for obj in [NPC_OBJ, HELPER_OBJ] {
        post(&server, obj, |c| c.go_to_idle());
    }
    assert_eq!(behavior(&server, HELPER_OBJ), Some(B::Idle));
    assert!(
        ordinary_plot.cancellation_requested(),
        "Idle interrupts the skills of an ordinary NPC"
    );
    assert!(
        !controller_plot.cancellation_requested(),
        "Idle must not cancel the wave plot of an event controller"
    );
    for obj in [NPC_OBJ, HELPER_OBJ] {
        post(&server, obj, |c| c.go_to_default_behavior());
    }
    assert_eq!(behavior(&server, HELPER_OBJ), Some(B::HoldPosition));
    assert!(
        !controller_plot.cancellation_requested(),
        "HoldPosition must not cancel the wave plot of an event controller"
    );
}

#[tokio::test]
async fn return_state_teleports_home_after_the_timeout() {
    let server = server();
    let client = TestClient::new(&server, 1, 1);
    let c = test_character(1, "Watcher", 150.0, 100.0);
    let _player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    // displaced 30 m from home and rooted in place by a 0 move speed model stance
    npc.data().unit.transform.set_xyz(130.0, 100.0, 100.0);
    post(&server, NPC_OBJ, |c| {
        c.go_to_return();
        // expire the 20 s timeout
        c.st(B::ReturnState).timeout_time = 0;
        // MoveTowards does nothing while displaced
        c.npc.data().unit.displaced_until_ms = Some(i64::MAX);
    });
    tick_all(&server, 100);
    // OnCompletedReturn: farther than 4 m -> MoveTowards(idle, 1000000) is blocked by the
    // displacement too, then GoToIdle
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Idle));
    npc.data().unit.displaced_until_ms = None;
    post(&server, NPC_OBJ, |c| {
        c.go_to_return();
        c.st(B::ReturnState).timeout_time = 0;
    });
    tick_all(&server, 100);
    let p = npc_pos(&npc);
    assert!((p.x - 100.0).abs() < 1.5, "teleported home: {p:?}");
}

#[tokio::test]
async fn follow_path_moves_along_the_points() {
    let server = server();
    let client = TestClient::new(&server, 1, 1);
    let c = test_character(1, "Watcher", 300.0, 300.0);
    let _player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::Default,
    );
    server.systems.ai.paths.insert(
        "test_path",
        vec![
            AiPathPoint {
                position: Vec3::new(0.0, 0.0, 0.0),
                action: AiPathPointAction::Speed,
                param: "4".into(),
            },
            AiPathPoint {
                position: Vec3::new(110.0, 100.0, 100.0),
                action: AiPathPointAction::None,
                param: String::new(),
            },
            AiPathPoint {
                position: Vec3::new(110.0, 110.0, 100.0),
                action: AiPathPointAction::DisableLoop,
                param: String::new(),
            },
        ],
    );
    super::hooks::spawned(&server, &npc, "test_path");
    // the loaded path makes the AI persistent (ticks without players around)
    assert!(server
        .systems
        .ai
        .ais
        .inspect(NPC_OBJ, |a| a.has_persistent_ai())
        .unwrap());
    // Spawning -> RunCommandSet -> Idle -> FollowPath (CheckFollowPath)
    let following = tick_until(&server, 100, 110, 20, || {
        behavior(&server, NPC_OBJ) == Some(B::FollowPath)
    })
    .await;
    assert!(following, "{:?}", behavior(&server, NPC_OBJ));
    // 4 m/s * 0.5 s per tick
    let reached = tick_until(&server, 500, 5, 40, || {
        let p = npc_pos(&npc);
        (p.x - 110.0).abs() < 1.1 && (p.y - 110.0).abs() < 1.1
    })
    .await;
    assert!(reached, "{:?}", npc_pos(&npc));
    // path done (loop disabled): back to Idle / default
    tick_until(&server, 100, 110, 6, || false).await;
    assert_ne!(behavior(&server, NPC_OBJ), Some(B::FollowPath));
}

fn path_point(x: f32, y: f32, z: f32) -> AiPathPoint {
    AiPathPoint {
        position: Vec3::new(x, y, z),
        action: AiPathPointAction::None,
        param: String::new(),
    }
}

fn ai_command(cmd_id: AiCommandCategory, param1: u32, param2: &str) -> AiCommands {
    AiCommands {
        id: 0,
        cmd_set_id: 0,
        cmd_id,
        param1,
        param2: param2.to_string(),
    }
}

/// Retail command set 146 (`public_farm*`): the first FollowPath loads the route named by its
/// own `param2` (the C# loaded the *previous* name, so the first command did nothing and the
/// last route never loaded), every route point is visited in order even when its authored Z is
/// above the ground the walker follows, the sentinel returns to the command set (the C# then
/// overrode `GoToRunCommandSet` with `GoToIdle`), and the NPC settles back into HoldPosition
/// without an endless in-place micro-step at an unreachable elevated point.
#[tokio::test]
async fn follow_path_command_set_walks_every_route_point() {
    let server = server();
    let client = TestClient::new(&server, 1, 1);
    let c = test_character(1, "Watcher", 110.0, 100.0);
    let _player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let farm = vec![
        path_point(101.6, 100.2, 100.0),
        path_point(102.0, 100.5, 101.4),
        path_point(104.0, 101.0, 100.0),
    ];
    let farm2 = vec![
        path_point(104.5, 101.2, 100.0),
        path_point(105.5, 102.0, 100.0),
    ];
    let farm3 = vec![
        path_point(104.0, 101.0, 100.0),
        path_point(101.0, 100.0, 100.0),
    ];
    server.systems.ai.paths.insert("test_farm", farm.clone());
    server.systems.ai.paths.insert("test_farm2", farm2.clone());
    server.systems.ai.paths.insert("test_farm3", farm3.clone());
    // has_simulation, like NpcSpawnerNpc.Spawn (no spawner-attached route)
    super::hooks::spawned(&server, &npc, "");
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));

    // The real `ai_commands` rows of set 146 (UseSkill 0 = no template, so it is a no-op).
    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[
                ai_command(AiCommandCategory::FollowPath, 1, "test_farm"),
                ai_command(AiCommandCategory::UseSkill, 0, ""),
                ai_command(AiCommandCategory::Timeout, 2, ""),
                ai_command(AiCommandCategory::FollowPath, 1, "test_farm2"),
                ai_command(AiCommandCategory::UseSkill, 0, ""),
                ai_command(AiCommandCategory::Timeout, 2, ""),
                ai_command(AiCommandCategory::FollowPath, 1, "test_farm3"),
            ],
            false,
        );
    });

    let expected: Vec<Vec3> = farm
        .iter()
        .chain(farm2.iter())
        .chain(farm3.iter())
        .map(|p| p.position)
        .collect();
    let mut seen: Vec<Vec3> = Vec::new();
    let mut last_target = Vec3::default();
    let mut same_target_ticks = 0;
    let mut max_same_target_ticks = 0;
    let mut finished = false;
    for _ in 0..300 {
        tick_all(&server, 100);
        let (target, current, queued) = server
            .systems
            .ai
            .ais
            .inspect(NPC_OBJ, |a| {
                (
                    a.path_handler.target_position,
                    a.current_command.is_some(),
                    a.commands_queue.len(),
                )
            })
            .expect("npc ai");
        if target == last_target {
            if !is_zero_vec3(target) {
                same_target_ticks += 1;
                max_same_target_ticks = max_same_target_ticks.max(same_target_ticks);
            }
        } else {
            if !is_zero_vec3(target) {
                seen.push(target);
            }
            last_target = target;
            same_target_ticks = 0;
        }
        if !current && queued == 0 && behavior(&server, NPC_OBJ) == Some(B::HoldPosition) {
            finished = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        finished,
        "command set did not finish: behavior {:?}, seen {:?}",
        behavior(&server, NPC_OBJ),
        seen
    );
    assert_eq!(
        seen, expected,
        "every route point must be targeted in order"
    );
    assert!(
        max_same_target_ticks < 40,
        "a target was chased for {max_same_target_ticks} ticks (in-place micro-steps)"
    );
    let end = npc_pos(&npc);
    assert!(
        (end.x - 101.0).abs() < 1.1 && (end.y - 100.0).abs() < 1.1,
        "did not walk the last route: {end:?}"
    );
    // Settled: no more movement at all.
    let rest = npc_pos(&npc);
    for _ in 0..20 {
        tick_all(&server, 100);
    }
    let after = npc_pos(&npc);
    assert!(
        (after.x - rest.x).abs() < 1e-4 && (after.y - rest.y).abs() < 1e-4,
        "still moving in HoldPosition: {rest:?} -> {after:?}"
    );
}

fn is_zero_vec3(v: Vec3) -> bool {
    v.x == 0.0 && v.y == 0.0 && v.z == 0.0
}

#[tokio::test]
async fn hooks_create_ai_by_file_type_and_detach_on_delete() {
    let server = server();
    let mk = |id: u32| Entity::new(id, Npc::new(PASSIVE_TEMPLATE));
    let a = mk(0x300);
    assert!(super::hooks::create_ai(&server, &a, 22));
    assert!(a.data().has_ai);
    assert_eq!(
        server.systems.ai.ais.inspect(0x300, |ai| ai.kind),
        Some(AiKind::ArcherRoaming)
    );
    // SiegeWeaponPlace / Bomb / undefined types have no AI (GetAiByType returns null)
    for (id, t) in [(0x301, 11), (0x302, 12), (0x303, 14), (0x304, 0)] {
        let n = mk(id);
        n.data().has_ai = true;
        assert!(!super::hooks::create_ai(&server, &n, t));
        assert!(!n.data().has_ai);
    }
    // Npc.Delete: a spawned NPC that left the world is detached on the next tick
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    tick_all(&server, 100);
    assert!(super::manager::has_ai(&server, NPC_OBJ));
    server.world.world(0).unwrap().delete(&npc);
    tick_all(&server, 100);
    assert!(!super::manager::has_ai(&server, NPC_OBJ));
}

#[tokio::test]
async fn npc_death_goes_to_dead_and_clears_aggro() {
    let server = server();
    let client = TestClient::new(&server, 1, 1);
    let c = test_character(1, "Killer", 102.0, 100.0);
    let player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let p = WorldObject::from(&player);
    super::combat::add_unit_aggro(&server, &WorldObject::from(&npc), AggroKind::Damage, &p, 10);
    post(&server, NPC_OBJ, |c| c.on_aggro_target_changed());
    assert!(player
        .data()
        .skill_char
        .is_in_aggro_list_of
        .contains(&NPC_OBJ));
    // Unit.DoDie -> NpcSkillHooks.npc_after_death (the AI decorator)
    npc.data().unit.hp = 0;
    crate::systems::skills::combat::do_die(
        &server,
        &WorldObject::from(&npc),
        Some(&p),
        crate::systems::skills::types::KillReason::Damage,
    );
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::Dead));
    assert!(npc.data().unit.skill_state.aggro_table.is_empty());
    assert!(player.data().skill_char.is_in_aggro_list_of.is_empty());
    assert!(!player.data().unit.is_in_battle);
}

/// The AI calls of `systems::npcs::bridges::NpcSkills` (`Npc.OnDamageReceived`,
/// `OnNoAggroTarget`, `CheckIfEmptyAggroToReturn`, `Npc.DoDie` -> `GoToDead`, the
/// `NpcControlEffect`, the move state).
struct AiParts;

impl crate::systems::skills::ext::NpcSkillHooks for AiParts {
    fn npc_after_death(
        &self,
        server: &GameServer,
        npc: &NpcRef,
        _killer: Option<&crate::world::WorldObject>,
    ) {
        super::hooks::on_npc_died(server, npc);
    }

    fn npc_damage_received(
        &self,
        server: &GameServer,
        npc: &NpcRef,
        _attacker: &crate::world::WorldObject,
        _amount: i32,
    ) {
        super::hooks::on_damage_received(server, npc);
    }

    fn npc_no_aggro_target(&self, server: &GameServer, npc: &NpcRef) {
        super::hooks::on_no_aggro_target(server, npc);
    }

    fn npc_aggro_removed(&self, server: &GameServer, _npc: &NpcRef, abuser: u32) {
        super::hooks::check_abuser_empty_aggro(server, abuser);
    }

    fn npc_move_state(
        &self,
        _server: &GameServer,
        npc: &NpcRef,
    ) -> (
        crate::models::movement::GameStanceType,
        crate::models::movement::MoveTypeAlertness,
    ) {
        let n = npc.data();
        (n.current_game_stance, n.current_alertness)
    }

    fn apply_effect(
        &self,
        server: &GameServer,
        _kind: crate::systems::skills::data::EffectKind,
        template: &crate::systems::skills::data::EffectTemplate,
        args: &crate::systems::skills::effects::EffectArgs<'_>,
    ) -> bool {
        if let crate::systems::skills::data::EffectTemplate::NpcControl(e) = template {
            if let Some(n) = args
                .target
                .as_obj()
                .and_then(crate::world::WorldObject::as_npc)
            {
                super::hooks::npc_control_effect(server, e, n);
            }
            return true;
        }
        false
    }
}

/// `Npc.Delete()`: the AI is detached and forgotten with the NPC.
#[tokio::test]
async fn npc_delete_detaches_its_ai() {
    let f = fixture(300.0);
    assert!(f.server.systems.ai.ais.get(NPC_OBJ).is_some());
    let world = f.server.world.world(0).expect("world");
    crate::systems::npcs::npc::manager::delete(&f.server, &world, &f.npc);
    assert!(f.server.systems.ai.ais.get(NPC_OBJ).is_none());
    assert!(!f.npc.data().has_ai);
    assert!(world.npc(NPC_OBJ).is_none());
}

#[tokio::test]
async fn npc_deleting_itself_from_an_ai_call_does_not_stall_other_enemies() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    let f = fixture(115.0);
    let server = f.server.clone();
    let queued_ran = Arc::new(AtomicBool::new(false));
    let queued = queued_ran.clone();
    let (finished, completion) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        // NPC skills such as KillNpcWithoutCorpse can delete their caster during
        // an AI step. Callbacks already queued for that AI must be discarded.
        post(&server, NPC_OBJ, move |c| {
            post(c.server, NPC_OBJ, move |_| {
                queued.store(true, Ordering::SeqCst);
            });
            let world = c.server.world.world(0).expect("world");
            crate::systems::npcs::npc::manager::delete(c.server, &world, &c.npc);
        });
        finished.send(()).unwrap();
    });
    completion
        .recv_timeout(Duration::from_secs(2))
        .expect("NPC deletion deadlocked inside its own AI call");
    worker.join().unwrap();
    assert!(!queued_ran.load(Ordering::SeqCst));
    assert!(!super::manager::has_ai(&f.server, NPC_OBJ));
    assert!(f.server.world.world(0).unwrap().npc(NPC_OBJ).is_none());

    spawn_npc(
        &f.server,
        HELPER_OBJ,
        NPC_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let player = f.player.clone();
    assert!(
        tick_until(&f.server, 500, 160, 60, || player.data().unit.hp < 1000).await,
        "another enemy did not chase and attack after NPC deletion"
    );
}

#[tokio::test]
async fn busy_npc_ai_does_not_stop_other_enemies_chasing_and_attacking() {
    use std::sync::mpsc;

    let f = fixture(115.0);
    // The busy NPC stays registered in the AI manager (the global tick still has to reach
    // it), but is parked outside the healthy enemy's neighbourhood: without faction
    // templates `can_attack` is true for the collocated NPC, and the enemy would attack it
    // instead of chasing the player.
    {
        let npc = &f.npc;
        npc.data().unit.transform.set_xyz(1000.0, 1000.0, 100.0);
    }
    let world = f.server.world.world(0).expect("world");
    world.add_visible_object(&f.npc, &f.server.view_ctx());
    let other = spawn_npc(
        &f.server,
        HELPER_OBJ,
        NPC_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let (entered, ready) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let server = f.server.clone();
    let holder = std::thread::spawn(move || {
        post(&server, NPC_OBJ, move |_| {
            entered.send(()).unwrap();
            resume.recv().unwrap();
        });
    });
    ready.recv_timeout(Duration::from_secs(2)).unwrap();

    let server = f.server.clone();
    let runtime = tokio::runtime::Handle::current();
    let (finished, completion) = mpsc::channel();
    let ticker = std::thread::spawn(move || {
        let _runtime = runtime.enter();
        for _ in 0..10 {
            tick_all(&server, 500);
            std::thread::sleep(Duration::from_millis(160));
        }
        finished.send(()).unwrap();
    });
    let result = completion.recv_timeout(Duration::from_secs(5));
    // Always release the holder, including when checking the old blocking tick.
    release.send(()).unwrap();
    holder.join().unwrap();
    ticker.join().unwrap();
    result.expect("one busy NPC blocked the global AI tick");
    assert!(npc_pos(&other).x > 100.0, "another enemy stopped moving");
    assert!(
        f.player.data().unit.hp < 1000,
        "another enemy stopped attacking"
    );
}

/// A damaged flying NPC descends onto its attacker in 3D and, once the aggro is gone, climbs
/// back to its spawn altitude; the return must not end at the home XY while still below it.
#[tokio::test]
async fn flying_npc_descends_to_its_attacker_and_returns_to_spawn_altitude() {
    // The ranged base skill (max 4) and the melee-id skill (id 2, max 25, built on the same
    // effect, but with the 4 m guard) in both geodata modes.
    for (geo_data_mode, melee_skill) in [(false, false), (true, false), (false, true), (true, true)]
    {
        let data = if melee_skill {
            data_with_melee_skill()
        } else {
            data()
        };
        let server = server_with_data(data);
        server
            .config
            .update(|cfg| cfg.world.geo_data_mode = geo_data_mode);
        let client = TestClient::new(&server, 1, 1);
        let mut c = test_character(1, "Hunter", 100.0, 100.0);
        // Survive the base skill for the whole descent.
        c.unit.hp = 100_000;
        c.unit.max_hp = 100_000;
        let player = spawn_player(&server, &client, c, PLAYER_OBJ);
        // The attacker stands straight below the flying NPC's spawn position.
        player.data().unit.transform.set_xyz(100.0, 100.0, 80.0);
        let world = server.world.world(0).expect("world");
        world.add_visible_object(&player, &server.view_ctx());

        let npc = spawn_npc(
            &server,
            NPC_OBJ,
            PASSIVE_TEMPLATE,
            100.0,
            100.0,
            AiKind::HoldPosition,
        );
        npc.data().can_fly = true;
        // Settle to HoldPosition before the damage aggro (like the passive-help pattern).
        let settled = tick_until(&server, 100, 20, 10, || {
            behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
        })
        .await;
        assert!(
            settled,
            "never settled: {:?} (geo_data_mode={geo_data_mode})",
            behavior(&server, NPC_OBJ)
        );
        assert_eq!(npc_pos(&npc), Vec3::new(100.0, 100.0, 100.0));
        assert_eq!(npc.data().current_game_stance, GameStanceType::FLY);

        // Npc.OnDamageReceived, as passive_npc_fights_back_and_calls_for_help.
        let attacker = WorldObject::from(&player);
        super::combat::add_unit_aggro(
            &server,
            &WorldObject::from(&npc),
            AggroKind::Damage,
            &attacker,
            50,
        );
        server
            .systems
            .skills
            .ext
            .npcs()
            .npc_damage_received(&server, &npc, &attacker, 50);
        assert_eq!(behavior(&server, NPC_OBJ), Some(B::Attack));

        // Bounded descent: 0.5 m per 500 ms tick (no FLY stance in the model, so
        // BaseMoveSpeed falls back to 1) until the 2 m attack range; edge distance subtracts
        // the 0.5 m model radius, so the raw 3D distance stops at 2.5 m.
        let mut in_range = false;
        let mut last_z = 100.0;
        for tick in 0..80 {
            tick_all(&server, 500);
            // Let async skill casts and their strafe gates finish between simulated ticks.
            tokio::time::sleep(Duration::from_millis(2)).await;
            let (current_target, aggro_target, stance, pos) = {
                let n = npc.data();
                let t = n.unit.transform;
                (
                    n.unit.current_target,
                    n.current_aggro_target,
                    n.current_game_stance,
                    Vec3::new(t.x, t.y, t.z),
                )
            };
            assert_eq!(
                current_target, PLAYER_OBJ,
                "lost the attacker at tick {tick}: {pos:?} (geo_data_mode={geo_data_mode})"
            );
            assert_eq!(aggro_target, PLAYER_OBJ, "aggro lost at tick {tick}");
            assert_eq!(
                stance,
                GameStanceType::FLY,
                "stance left FLY at tick {tick}"
            );
            assert!(
                pos.z <= 100.0 && pos.z > 80.0,
                "flew past the attacker at tick {tick}: {pos:?}"
            );
            assert!(
                (last_z - pos.z) <= 0.5 + 0.001 && pos.z <= last_z + 0.001,
                "unbounded descent step at tick {tick}: {last_z} -> {pos:?}"
            );
            last_z = pos.z;
            // Outside the 4 m 3D edge distance (raw distance minus the 0.5 m model radius):
            // neither the height-aware skill range nor the melee skill-2 guard may let the
            // flyer cast. With the old 2D caller distance the melee fixture hits from here.
            if pos.z - 80.0 > 4.5 + 0.001 {
                assert_eq!(
                    player.data().unit.hp,
                    100_000,
                    "cast at a victim out of 3D range at tick {tick}: {pos:?} \
                     (geo_data_mode={geo_data_mode}, melee_skill={melee_skill})"
                );
            }
            if pos.z <= 82.5 {
                in_range = true;
                break;
            }
        }
        assert!(
            in_range,
            "never descended into attack range: {:?} (geo_data_mode={geo_data_mode})",
            npc_pos(&npc)
        );
        assert!(npc
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&PLAYER_OBJ));

        // Inside the 4 m 3D range the base skill reaches the attacker (real sleep lets the
        // real-time skill interval and any async cast complete).
        let damaged = tick_until(&server, 500, 20, 30, || player.data().unit.hp < 100_000).await;
        assert!(
            damaged,
            "never hit the attacker after closing range: {:?} (geo_data_mode={geo_data_mode})",
            npc_pos(&npc)
        );

        // Losing aggro (the player stays for AI activity): the flyer climbs straight up while
        // still in ReturnState and only leaves it within MoveTowards's 1 m arrival tolerance
        // of the home z.
        post(&server, NPC_OBJ, |c| c.on_no_aggro_target());
        assert_eq!(behavior(&server, NPC_OBJ), Some(B::ReturnState));
        assert_eq!(npc.data().current_game_stance, GameStanceType::FLY);
        let mut zs = Vec::new();
        let mut left_return = false;
        for _ in 0..80 {
            tick_all(&server, 500);
            tokio::time::sleep(Duration::from_millis(2)).await;
            if behavior(&server, NPC_OBJ) != Some(B::ReturnState) {
                left_return = true;
                break;
            }
            zs.push(npc_pos(&npc).z);
        }
        assert!(
            left_return,
            "stuck in ReturnState at {:?} (geo_data_mode={geo_data_mode})",
            npc_pos(&npc)
        );
        assert!(zs.len() > 2, "no gradual climb: {zs:?}");
        assert!(
            zs.windows(2).all(|w| w[1] >= w[0]),
            "descended during the return: {zs:?} (geo_data_mode={geo_data_mode})"
        );
        assert!(
            zs.first() < zs.last(),
            "did not climb back: {zs:?} (geo_data_mode={geo_data_mode})"
        );
        assert!(
            zs.iter().all(|z| *z <= 100.0 + 0.001),
            "climbed past home: {zs:?}"
        );
        assert!(
            (zs.last().copied().unwrap_or(0.0) - 100.0).abs() <= 1.0 + 0.001,
            "left ReturnState outside the arrival tolerance: {zs:?}"
        );
        let home = npc_pos(&npc);
        assert!(
            (home.x - 100.0).abs() < 0.01 && (home.y - 100.0).abs() < 0.01,
            "return drifted in XY: {home:?} (geo_data_mode={geo_data_mode})"
        );
        assert!(
            (home.z - 100.0).abs() <= 1.0 + 0.001,
            "return stopped outside the arrival tolerance: {home:?} (geo_data_mode={geo_data_mode})"
        );
    }
}

/// The ReturnState timeout teleport must count vertical displacement: the same XY as home but
/// 20 m below is not home for a flyer.
#[tokio::test]
async fn flying_npc_timeout_return_counts_vertical_displacement() {
    let server = server();
    // The player keeps the NPC's region active so `tick_all` advances its AI.
    let client = TestClient::new(&server, 1, 1);
    let c = test_character(1, "Watcher", 150.0, 100.0);
    let _player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    npc.data().can_fly = true;
    let settled = tick_until(&server, 100, 20, 10, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));

    npc.data().unit.transform.set_xyz(100.0, 100.0, 80.0);
    post(&server, NPC_OBJ, |c| {
        c.go_to_return();
        c.st(B::ReturnState).timeout_time = 0;
    });
    assert_eq!(behavior(&server, NPC_OBJ), Some(B::ReturnState));
    tick_all(&server, 500);
    let home = npc_pos(&npc);
    assert!(
        (home.z - 100.0).abs() < 0.01,
        "timeout did not restore the home altitude: {home:?}"
    );
    assert!(
        (home.x - 100.0).abs() < 0.01 && (home.y - 100.0).abs() < 0.01,
        "{home:?}"
    );
}

/// A guard NPC that kills a hostile mob must drop it as target, stop attacking and leave combat
/// (live report: Graymist guards kept attacking the dead mob, whose client health bar stayed
/// gray as if the death never registered).
const MOB_OBJ: u32 = 0x202;
const GUARD2_OBJ: u32 = 0x203;
const GUARD_FACTION: u32 = 101;
const MOB_FACTION: u32 = 109;

/// [`data`] with the guard faction (also the test characters' faction), hostile to the mob faction.
fn guard_data() -> GameData {
    use crate::data::world_data::{FactionRelation, SystemFaction};
    let mut data = data();
    for id in [GUARD_FACTION, MOB_FACTION] {
        data.factions.add(SystemFaction {
            id,
            ..SystemFaction::default()
        });
    }
    data.factions.relations.push(FactionRelation {
        id: GUARD_FACTION,
        id2: MOB_FACTION,
        state: crate::systems::skills::types::RelationState::Hostile as u8,
        exp_time: 0,
    });
    data
}

fn tracked_state(server: &GameServer, guard: &NpcRef, mob: &NpcRef) -> String {
    let (target, aggro_target, battle, aggro) = {
        let g = guard.data();
        let mut keys: Vec<u32> = g.unit.skill_state.aggro_table.keys().copied().collect();
        keys.sort_unstable();
        (
            g.unit.current_target,
            g.current_aggro_target,
            g.unit.is_in_battle,
            keys,
        )
    };
    let (hp, dead) = {
        let m = mob.data();
        (m.unit.hp, m.unit.is_dead())
    };
    format!(
        "guard[beh={:?} target={target:#x} aggro_target={aggro_target:#x} battle={battle} aggro={aggro:x?}] mob[hp={hp} dead={dead} beh={:?}]",
        behavior(server, NPC_OBJ),
        behavior(server, MOB_OBJ),
    )
}

/// `with_player`: a same-faction bystander 30 m away (region activity + a packet receiver);
/// without one the guard is seeded with aggro on the mob (an idle NPC without region activity is
/// dormant, so it would never notice the mob by itself).
async fn run_guard_kills_mob(guard_kind: AiKind, with_player: bool, second_guard: bool) {
    let server = server_with_data(guard_data());
    let mut client = TestClient::new(&server, 1, 1);
    let player = with_player.then(|| {
        let mut c = test_character(1, "Bystander", 130.0, 100.0);
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
        spawn_player(&server, &client, c, PLAYER_OBJ)
    });
    let guard = spawn_npc(&server, NPC_OBJ, NPC_TEMPLATE, 100.0, 100.0, guard_kind);
    guard.data().unit.faction_id = GUARD_FACTION;
    let mob = spawn_npc(
        &server,
        MOB_OBJ,
        PASSIVE_TEMPLATE,
        103.0,
        100.0,
        AiKind::Default,
    );
    mob.data().unit.faction_id = MOB_FACTION;
    mob.data().unit.hp = HIT_DAMAGE + 20;
    mob.data().unit.max_hp = HIT_DAMAGE + 20;
    let guard2 = second_guard.then(|| {
        let g = spawn_npc(
            &server,
            GUARD2_OBJ,
            NPC_TEMPLATE,
            100.0,
            103.0,
            AiKind::HoldPosition,
        );
        g.data().unit.faction_id = GUARD_FACTION;
        g
    });
    if !with_player {
        // settle (the spawn / command-set grace runs without region activity), then seed
        tick_until(&server, 100, 120, 5, || false).await;
        let mob_obj = WorldObject::from(&mob);
        super::combat::add_unit_aggro(
            &server,
            &WorldObject::from(&guard),
            AggroKind::Damage,
            &mob_obj,
            1,
        );
        post(&server, NPC_OBJ, |c| c.on_aggro_target_changed());
    }
    client.drain();

    let mut trace = Vec::new();
    let mut died_at = None;
    for i in 0..120 {
        tick_all(&server, 500);
        tokio::time::sleep(Duration::from_millis(160)).await;
        let ops = frame_ops(&mut client);
        let line = format!(
            "#{i:03} {} ops={:x?}",
            tracked_state(&server, &guard, &mob),
            ops
        );
        if died_at.is_none() && mob.data().unit.hp <= 0 {
            died_at = Some(i);
        }
        trace.push((i, line, ops));
        if died_at.is_some_and(|d| i >= d + 30) {
            break;
        }
    }
    for (_, line, _) in &trace {
        eprintln!("{line}");
    }
    let died = died_at.expect("the guard never killed the mob");
    assert!(mob.data().unit.is_dead(), "hp 0 but the mob is not dead");
    assert_eq!(behavior(&server, MOB_OBJ), Some(B::Dead));
    // the death packet reached the bystander, after the points packet
    if with_player {
        let ops = &trace[died].2;
        let death = ops.iter().position(|o| *o == sc::SCUnitDeathPacket);
        assert!(
            death.is_some(),
            "no SCUnitDeathPacket to the player: {ops:x?}"
        );
        let points = ops.iter().position(|o| *o == sc::SCUnitPointsPacket);
        assert!(points < death, "points must precede death: {ops:x?}");
    }
    // after the death: no further skill starts / damage against the dead mob
    for (i, line, ops) in &trace[(died + 3).min(trace.len() - 1)..] {
        assert!(
            !ops.contains(&sc::SCUnitDamagedPacket),
            "guard still attacking the dead mob at tick {i}: {line}"
        );
    }
    // the dead mob is no target / aggro entry anywhere and the guards left combat
    for g in [Some(&guard), guard2.as_ref()].into_iter().flatten() {
        let gd = g.data();
        assert!(
            !gd.unit.skill_state.aggro_table.contains_key(&MOB_OBJ),
            "dead mob still in aggro table: {}",
            tracked_state(&server, g, &mob)
        );
        assert_ne!(gd.unit.current_target, MOB_OBJ, "dead mob still targeted");
        assert!(!gd.unit.is_in_battle, "guard still in battle");
    }
    assert_ne!(behavior(&server, NPC_OBJ), Some(B::Attack));
    drop(player);
}

#[tokio::test]
async fn guard_stops_attacking_a_mob_it_killed() {
    run_guard_kills_mob(AiKind::HoldPosition, true, false).await;
}

#[tokio::test]
async fn two_guards_stop_attacking_a_mob_they_killed() {
    run_guard_kills_mob(AiKind::HoldPosition, true, true).await;
}

/// The kill lands while the mob's AI is busy on another thread (a player's skill task / the
/// blocking-pool AI batch racing the guard): `go_to_dead` is queued behind the damage effect's
/// tail, which re-adds the killer to the dead mob's aggro table
/// (`skills/effects/combat.rs` damage: `add_unit_aggro(trg, ..)` after `reduce_current_hp`).
/// `dead_enter` -> `npc_clear_all_aggro` then sees a non-empty table and, with the mob away from
/// its idle position, `check_if_empty_aggro_to_return` runs `go_to_return` on the corpse:
/// `return_state_enter` restores the hp, so the mob is alive again and hostile.
#[tokio::test]
async fn guard_kill_while_the_mob_ai_is_busy_keeps_the_mob_dead() {
    let server = server_with_data(guard_data());
    let mut client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Bystander", 130.0, 100.0);
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    let _player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let guard = spawn_npc(
        &server,
        NPC_OBJ,
        NPC_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    guard.data().unit.faction_id = GUARD_FACTION;
    let mob = spawn_npc(
        &server,
        MOB_OBJ,
        PASSIVE_TEMPLATE,
        103.0,
        100.0,
        AiKind::Roaming,
    );
    mob.data().unit.faction_id = MOB_FACTION;
    // three hits: 40 + 40 + 40 >= 3 * 40 - 20
    mob.data().unit.hp = 3 * HIT_DAMAGE - 20;
    mob.data().unit.max_hp = 3 * HIT_DAMAGE - 20;
    client.drain();

    // normal engagement until the first hit landed
    let first_hit = tick_until(&server, 500, 160, 60, || mob.data().unit.hp < 100).await;
    assert!(first_hit, "{}", tracked_state(&server, &guard, &mob));
    // the mob chased / drifted away from its idle position
    mob.data().unit.transform.set_xyz(110.0, 100.0, 100.0);

    // block the mob AI (as a concurrent tick would) until it is dead
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let (ready_tx, ready) = std::sync::mpsc::channel::<()>();
    let s = server.clone();
    let blocker = std::thread::spawn(move || {
        super::manager::run_now(&s, MOB_OBJ, move |_c| {
            ready_tx.send(()).expect("ready");
            let _ = wait.recv();
        });
    });
    ready.recv().expect("mob ai busy");
    let killed = tick_until(&server, 500, 160, 60, || mob.data().unit.hp <= 0).await;
    eprintln!(
        "after kill (mob ai still busy): mob hp {} guard aggro {:?}",
        mob.data().unit.hp,
        guard
            .data()
            .unit
            .skill_state
            .aggro_table
            .keys()
            .collect::<Vec<_>>()
    );
    eprintln!(
        "mob aggro table after kill: {:?}",
        mob.data()
            .unit
            .skill_state
            .aggro_table
            .keys()
            .collect::<Vec<_>>()
    );
    release.send(()).expect("release");
    blocker.join().expect("blocker");
    assert!(killed, "{}", tracked_state(&server, &guard, &mob));
    // the queued `go_to_dead` ran now: the corpse must stay a corpse
    let after_release = tracked_state(&server, &guard, &mob);
    eprintln!("after the queued AI calls ran: {after_release}");
    assert_eq!(
        mob.data().unit.hp,
        0,
        "the corpse was healed back to life by ReturnState: {after_release}"
    );
    // the death of the kill above
    let kill_ops = frame_ops(&mut client);
    assert_eq!(
        kill_ops
            .iter()
            .filter(|o| **o == sc::SCUnitDeathPacket)
            .count(),
        1,
        "{kill_ops:x?}"
    );
    let mut deaths = 0;
    for i in 0..40 {
        tick_all(&server, 500);
        tokio::time::sleep(Duration::from_millis(160)).await;
        let ops = frame_ops(&mut client);
        deaths += ops.iter().filter(|o| **o == sc::SCUnitDeathPacket).count();
        let line = tracked_state(&server, &guard, &mob);
        eprintln!("#{i:02} {line} ops={ops:x?}");
        assert_eq!(mob.data().unit.hp, 0, "the mob came back to life: {line}");
    }
    assert_eq!(deaths, 0, "the mob had to be killed a second time");
    assert_eq!(behavior(&server, MOB_OBJ), Some(B::Dead));
    assert!(!guard
        .data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&MOB_OBJ));
    assert_ne!(behavior(&server, NPC_OBJ), Some(B::Attack));
}

/// `NpcControlEffect` `RunCommandSet` on an NPC with an AI: the queued `FollowPath` command
/// walks the route, so the legacy `Simulation.GoToPath` mover must not run beside it (two
/// movers kept pulling the NPC to different points and stalled it part-way along its route).
#[tokio::test]
async fn run_command_set_effect_does_not_start_the_simulation_mover_for_an_ai_npc() {
    use super::params::{AiCommandCategory, AiCommands};
    let mut game_data = data();
    game_data.systems.ai.ai.commands.insert(
        900,
        vec![AiCommands {
            id: 1,
            cmd_set_id: 900,
            cmd_id: AiCommandCategory::FollowPath,
            param1: 1,
            param2: "public_farm".to_string(),
        }],
    );
    let server = server_with_data(game_data);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        NPC_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let sim = Arc::new(std::sync::Mutex::new(super::simulation::Simulation::new(
        None,
    )));
    server.systems.ai.simulations.insert(NPC_OBJ, sim.clone());
    let effect = crate::systems::skills::data::effects::NpcControlEffect {
        id: 1,
        category_id: 5,
        param_string: String::new(),
        param_int: 900,
    };
    super::hooks::npc_control_effect(&server, &effect, &npc);
    assert!(sim.lock().unwrap().move_file_name.is_empty());
    assert!(!sim.lock().unwrap().move_to_path_enabled);
    assert!(!npc.data().unit.is_in_patrol);
}

/// A `NpcControlEffect` `FollowPath` that arrives twice at once (the Timeout trigger of the
/// channeling target buff of a quest item reaches the effect from the buff's own timer and from
/// the end of the channel): the walk must start, and start once. A second caller that passed
/// the `IsInPatrol` check before the first set it ran `GoToPath` again, which toggles the walk
/// off (`MoveToPathEnabled = !MoveToPathEnabled`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follow_path_effect_arriving_twice_at_once_starts_the_walk_once() {
    let dir = std::env::temp_dir().join(format!("aaemu_follow_twice_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("twice.path"),
        "|100.0|103.0|100.0|\n|100.0|120.0|100.0|\n|100.0|140.0|100.0|\n",
    )
    .unwrap();
    let server = server();
    server.systems.ai.set_paths_folder(dir.clone());
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        NPC_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    // A dead NPC does not take the scheduled steps of the walks started below (they would
    // stop the walk of a later round); the effect itself does not care.
    npc.data().unit.hp = 0;
    let sim = Arc::new(std::sync::Mutex::new(super::simulation::Simulation::new(
        Some(dir.clone()),
    )));
    server.systems.ai.simulations.insert(NPC_OBJ, sim.clone());
    let effect = crate::systems::skills::data::effects::NpcControlEffect {
        id: 1,
        category_id: 2,
        param_string: "twice".to_string(),
        param_int: 2,
    };
    let handle = tokio::runtime::Handle::current();
    let mut not_walking = 0;
    for _ in 0..400 {
        // the previous walk is over
        super::simulation::stop_move(&server, &sim, &npc);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    let _runtime = handle.enter();
                    barrier.wait();
                    super::hooks::npc_control_effect(&server, &effect, &npc);
                });
            }
        });
        let walking = super::simulation::lock_sim(&sim).move_to_path_enabled;
        if !walking || !npc.data().unit.is_in_patrol {
            not_walking += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        not_walking, 0,
        "the walk was toggled off in {not_walking} of 400 double deliveries"
    );
}

// ------------------------------------------------------- taunt buffs (10x-ONLY)
//
// The 1.2 client and the C# have no taunt logic; the mechanic and this ordering come from the
// 10.x zone server (`NpcAggroTable::TauntedBy` / `CancelTauntedBy`), the 1.2 buff descriptions
// agree (buff 171, buff 502, skill 17300). See `PORTING.md`.

const TAUNT_BUFF: u32 = 5000;
const TAUNT_BUFF_2: u32 = 5001;
const TAUNT_TOP_BUFF: u32 = 5002;
const TAUNT_TRIGGER_BUFF: u32 = 5003;
const TAUNT_EXPIRY_BUFF: u32 = 5004;
const TAUNTER_OBJ: u32 = 0x101;
const TAUNTER2_OBJ: u32 = 0x102;

fn taunt_data() -> GameData {
    let mut data = data();
    let s = &mut data.systems.skills;
    for (id, copy_top, duration) in [
        (TAUNT_BUFF, false, 4000),
        (TAUNT_BUFF_2, false, 4000),
        (TAUNT_TOP_BUFF, true, 4000),
        // Short enough to let the test wait for the real expiry timer.
        (TAUNT_EXPIRY_BUFF, false, 200),
    ] {
        s.add_buff(BuffTemplate {
            id,
            kind: 2,
            duration,
            stack_rule: crate::data::skills::stack_rule::REFRESH,
            taunt: true,
            taunt_with_top_aggro: copy_top,
            ..BuffTemplate::default()
        });
    }
    // A buff whose `Started` trigger applies the taunt buff to its holder with the original
    // caster (not a skill's BuffEffect).
    s.add_buff(BuffTemplate {
        id: TAUNT_TRIGGER_BUFF,
        kind: 2,
        duration: 10_000,
        ..BuffTemplate::default()
    });
    s.add_effect(
        EffectKind::BuffEffect,
        EffectTemplate::Buff(BuffEffect {
            id: 1,
            buff_id: Some(TAUNT_BUFF),
            chance: 100,
            stack: 1,
            ab_level: 0,
        }),
        0,
    );
    s.buff_triggers.insert(
        TAUNT_TRIGGER_BUFF,
        vec![BuffTriggerTemplate {
            id: 1,
            kind: BuffEventTriggerKind::Started as u16,
            effect_on_source: false,
            effect: (EffectKind::BuffEffect, 1),
            use_damage_amount: false,
            use_original_source: true,
            target_buff_tag_id: 0,
            target_no_buff_tag_id: 0,
            synergy: false,
        }],
    );
    // Used only by the non-hostile test: driven units on the same faction are friendly.
    data.factions.add(crate::data::world_data::SystemFaction {
        id: 1,
        ..Default::default()
    });
    data
}

struct TauntFixture {
    server: Arc<GameServer>,
    _client_a: TestClient,
    _client_b: TestClient,
    _client_c: TestClient,
    player_a: PlayerRef,
    player_b: PlayerRef,
    player_c: PlayerRef,
    npc: NpcRef,
    npc_obj: WorldObject,
}

fn taunt_fixture() -> TauntFixture {
    let server = server_with_data(taunt_data());
    let client_a = TestClient::new(&server, 1, 1);
    let client_b = TestClient::new(&server, 2, 2);
    let client_c = TestClient::new(&server, 3, 3);
    let mut a = test_character(1, "Tank", 103.0, 100.0);
    a.unit.hp = 1000;
    a.unit.max_hp = 1000;
    let player_a = spawn_player(&server, &client_a, a, PLAYER_OBJ);
    // The taunters stay out of the NPC's 4 m melee range, so chasing does not add threat.
    let mut b = test_character(2, "Taunter", 110.0, 100.0);
    b.unit.hp = 1000;
    b.unit.max_hp = 1000;
    let player_b = spawn_player(&server, &client_b, b, TAUNTER_OBJ);
    let mut c = test_character(3, "Later", 112.0, 100.0);
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    let player_c = spawn_player(&server, &client_c, c, TAUNTER2_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let npc_obj = WorldObject::from(&npc);
    TauntFixture {
        server,
        _client_a: client_a,
        _client_b: client_b,
        _client_c: client_c,
        player_a,
        player_b,
        player_c,
        npc,
        npc_obj,
    }
}

/// Lets a freshly spawned NPC reach its default behavior (`HoldPosition`) before it is pulled.
async fn settle(f: &TauntFixture) {
    tick_until(&f.server, 100, 120, 10, || false).await;
}

fn add_threat(f: &TauntFixture, caster: &PlayerRef, threat: i32) {
    let attacker = WorldObject::from(caster);
    super::combat::add_unit_aggro(&f.server, &f.npc_obj, AggroKind::Damage, &attacker, threat);
    f.server
        .systems
        .skills
        .ext
        .npcs()
        .npc_damage_received(&f.server, &f.npc, &attacker, threat);
}

fn apply_taunt(f: &TauntFixture, buff_id: u32, caster: &PlayerRef) {
    crate::systems::skills::buffs::add_buff_by_id(
        &f.server,
        &f.npc_obj,
        buff_id,
        &WorldObject::from(caster),
        0,
    );
}

fn aggro_entry(f: &TauntFixture, abuser: u32) -> crate::systems::skills::state::Aggro {
    f.npc.data().unit.skill_state.aggro_table[&abuser]
}

#[tokio::test]
async fn taunt_switches_the_ai_target_over_higher_threat_and_ends_cleanly() {
    let f = taunt_fixture();
    settle(&f).await;
    add_threat(&f, &f.player_a, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);

    apply_taunt(&f, TAUNT_BUFF, &f.player_b);
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        TAUNTER_OBJ,
        "the taunt must outrank A's higher threat"
    );
    assert_eq!(aggro_entry(&f, PLAYER_OBJ).total(), 100);
    assert_eq!(aggro_entry(&f, TAUNTER_OBJ).total(), 0);
    assert_ne!(aggro_entry(&f, TAUNTER_OBJ).taunt_time, 0);
    // The client / quest rating follows the same order.
    assert_eq!(
        super::combat::aggro_rating_in_percent(&f.npc_obj, TAUNTER_OBJ),
        0.0
    );
    assert_eq!(
        crate::systems::quests::bridges::aggro_rating_percent(&f.npc, TAUNTER_OBJ),
        0.0
    );

    crate::systems::skills::buffs::remove_buff(&f.server, &f.npc_obj, TAUNT_BUFF);
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        PLAYER_OBJ,
        "the threat order must return when the last taunt from the caster ends"
    );
    assert_eq!(aggro_entry(&f, TAUNTER_OBJ).taunt_time, 0);
    assert_eq!(
        aggro_entry(&f, TAUNTER_OBJ).total(),
        0,
        "taunt end must not reduce threat"
    );
}

#[tokio::test]
async fn the_latest_taunt_stamp_wins_even_over_higher_threat() {
    let f = taunt_fixture();
    settle(&f).await;
    add_threat(&f, &f.player_b, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);

    apply_taunt(&f, TAUNT_BUFF, &f.player_b);
    tokio::time::sleep(Duration::from_millis(2)).await;
    apply_taunt(&f, TAUNT_BUFF_2, &f.player_c);
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        TAUNTER2_OBJ,
        "the later stamp must win over B's higher threat"
    );
    assert!(aggro_entry(&f, TAUNTER2_OBJ).taunt_time > aggro_entry(&f, TAUNTER_OBJ).taunt_time);

    // When the later taunt ends the earlier caster's live stamp takes over.
    crate::systems::skills::buffs::remove_buff(&f.server, &f.npc_obj, TAUNT_BUFF_2);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);
    assert_eq!(aggro_entry(&f, TAUNTER2_OBJ).taunt_time, 0);
    assert_ne!(aggro_entry(&f, TAUNTER_OBJ).taunt_time, 0);
}

#[tokio::test]
async fn a_second_taunt_buff_from_the_same_caster_keeps_the_lock() {
    let f = taunt_fixture();
    settle(&f).await;
    add_threat(&f, &f.player_a, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);

    apply_taunt(&f, TAUNT_BUFF, &f.player_b);
    apply_taunt(&f, TAUNT_BUFF_2, &f.player_b);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);

    crate::systems::skills::buffs::remove_buff(&f.server, &f.npc_obj, TAUNT_BUFF);
    tick_all(&f.server, 100);
    assert_ne!(
        aggro_entry(&f, TAUNTER_OBJ).taunt_time,
        0,
        "the other taunt buff of the same caster still pins the table"
    );
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);

    crate::systems::skills::buffs::remove_buff(&f.server, &f.npc_obj, TAUNT_BUFF_2);
    tick_all(&f.server, 100);
    assert_eq!(aggro_entry(&f, TAUNTER_OBJ).taunt_time, 0);
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);
}

#[tokio::test]
async fn taunt_with_top_aggro_copies_the_current_top_total_once() {
    let f = taunt_fixture();
    settle(&f).await;
    add_threat(&f, &f.player_a, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);

    apply_taunt(&f, TAUNT_TOP_BUFF, &f.player_b);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);
    assert_eq!(
        aggro_entry(&f, TAUNTER_OBJ).total(),
        100,
        "the copy raises B to A's total, not above it"
    );
    assert_eq!(aggro_entry(&f, TAUNTER_OBJ).etc_aggro, 100);
    assert_eq!(aggro_entry(&f, PLAYER_OBJ).total(), 100);

    crate::systems::skills::buffs::remove_buff(&f.server, &f.npc_obj, TAUNT_TOP_BUFF);
    tick_all(&f.server, 100);
    assert_eq!(aggro_entry(&f, TAUNTER_OBJ).taunt_time, 0);
    assert_eq!(
        aggro_entry(&f, TAUNTER_OBJ).total(),
        100,
        "the copied threat persists after the taunt (10x keeps it)"
    );
    // Equal totals: pull aggro keeps B (A's total does not exceed B's times the ratio). With
    // `Ai.PullAggroEnabled` off the stable table order would put the lower obj id (A) first.
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);
}

#[tokio::test]
async fn taunt_refuses_self_and_non_hostile_casters_and_players() {
    let f = taunt_fixture();
    // Self: no entry, no stamp.
    crate::systems::skills::buffs::add_buff_by_id(&f.server, &f.npc_obj, TAUNT_BUFF, &f.npc_obj, 0);
    assert!(f.npc.data().unit.skill_state.aggro_table.is_empty());

    // Non-hostile: the NPC and the caster share faction 1 (friendly).
    f.npc.data().unit.faction_id = 1;
    f.player_b.data().unit.faction_id = 1;
    apply_taunt(&f, TAUNT_BUFF, &f.player_b);
    assert!(
        f.npc.data().unit.skill_state.aggro_table.is_empty(),
        "a friendly caster must not be able to taunt the NPC"
    );

    // A taunt buff on a player holder does nothing (10x only stamps NPC holders).
    let b_obj = WorldObject::from(&f.player_b);
    let c_obj = WorldObject::from(&f.player_c);
    crate::systems::skills::buffs::add_buff_by_id(&f.server, &b_obj, TAUNT_BUFF, &c_obj, 0);
    assert!(!f
        .player_b
        .data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&TAUNTER2_OBJ));
}

#[tokio::test]
async fn taunt_from_a_trigger_applied_buff_reaches_the_aggro_table() {
    let f = taunt_fixture();
    settle(&f).await;
    add_threat(&f, &f.player_a, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);

    apply_taunt(&f, TAUNT_TRIGGER_BUFF, &f.player_b);
    assert!(
        f.npc.data().unit.buffs.check_buff(TAUNT_BUFF),
        "the Started trigger applies the taunt buff to its holder"
    );
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        TAUNTER_OBJ,
        "the generic buff-start hook, not the skill BuffEffect, must stamp the table"
    );
    assert_ne!(aggro_entry(&f, TAUNTER_OBJ).taunt_time, 0);
}

#[tokio::test]
async fn refreshing_the_taunt_buff_restamps_the_table() {
    let f = taunt_fixture();
    settle(&f).await;
    add_threat(&f, &f.player_a, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);

    apply_taunt(&f, TAUNT_BUFF, &f.player_b);
    let first = aggro_entry(&f, TAUNTER_OBJ).taunt_time;
    assert_ne!(first, 0);

    // Re-applying the same buff refreshes it; the stamp is renewed (TAUNT-1: capture-pending).
    tokio::time::sleep(Duration::from_millis(2)).await;
    apply_taunt(&f, TAUNT_BUFF, &f.player_b);
    let refreshed = aggro_entry(&f, TAUNTER_OBJ).taunt_time;
    assert!(
        refreshed > first,
        "a refresh must re-stamp the taunt ({first} -> {refreshed})"
    );
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);
}

#[tokio::test]
async fn taunt_refresh_by_a_second_caster_releases_the_first_stamp_on_expiry() {
    let f = taunt_fixture();
    settle(&f).await;
    add_threat(&f, &f.player_a, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);

    apply_taunt(&f, TAUNT_EXPIRY_BUFF, &f.player_b);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);
    assert_ne!(aggro_entry(&f, TAUNTER_OBJ).taunt_time, 0);

    // A second taunter uses the same buff: the REFRESH rule replaces the effect and its
    // caster, so the first taunter's stamp must go with their buff instead of pinning the
    // NPC above the table until an aggro reset.
    tokio::time::sleep(Duration::from_millis(20)).await;
    apply_taunt(&f, TAUNT_EXPIRY_BUFF, &f.player_c);
    tick_all(&f.server, 100);
    assert_eq!(
        aggro_entry(&f, TAUNTER_OBJ).taunt_time,
        0,
        "the previous caster's stamp must be cleared when their buff is replaced"
    );
    assert_ne!(aggro_entry(&f, TAUNTER2_OBJ).taunt_time, 0);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER2_OBJ);

    // Let the last taunt expire through its timer, not an explicit removal.
    tokio::time::sleep(Duration::from_millis(400)).await;
    tick_all(&f.server, 100);
    assert!(
        !f.npc.data().unit.buffs.check_buff(TAUNT_EXPIRY_BUFF),
        "the taunt buff must have expired by its timer"
    );
    assert_eq!(aggro_entry(&f, TAUNTER2_OBJ).taunt_time, 0);
    assert_eq!(aggro_entry(&f, TAUNTER_OBJ).taunt_time, 0);
    assert_eq!(
        f.npc.data().unit.current_target,
        PLAYER_OBJ,
        "with both stamps gone the threat order returns"
    );
}

/// The 1.2 data `buffs.taunt` / `taunt_with_top_aggro` rows and the skills that apply them.
#[test]
#[ignore = "needs AAEMU_COMPACT_DB"]
fn real_compact_taunt_buffs_and_the_skills_that_apply_them() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let taunts: Vec<(u32, String)> = conn
        .prepare("SELECT id, taunt_with_top_aggro FROM buffs WHERE taunt='t' ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(taunts.len(), 134);
    assert_eq!(taunts.iter().filter(|(_, top)| top == "t").count(), 97);
    assert!(taunts.iter().any(|(id, top)| *id == 171 && top == "f"));
    assert!(taunts.iter().any(|(id, top)| *id == 502 && top == "t"));
    assert!(taunts.iter().any(|(id, top)| *id == 2111 && top == "f"));

    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT se.skill_id, be.buff_id
             FROM skill_effects se
             JOIN effects e ON e.id = se.effect_id AND e.actual_type = 'BuffEffect'
             JOIN buff_effects be ON be.id = e.actual_id
             JOIN buffs b ON b.id = be.buff_id AND b.taunt = 't'
             ORDER BY se.skill_id, be.buff_id",
        )
        .unwrap();
    let applied: Vec<(u32, u32)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(applied.len(), 97);
    let applied_buffs: std::collections::BTreeSet<u32> =
        applied.iter().map(|(_, buff)| *buff).collect();
    assert_eq!(applied_buffs.len(), 85);
    assert!(applied.contains(&(12_048, 502)));
    assert!(applied.contains(&(10_436, 171)));
    assert!(applied.contains(&(17_300, 3170)));
    // The other 49 taunts are applied by triggers / npc_initial_buffs / quests / scripts, so
    // the hook has to live in the generic buff-start path, not in the skill BuffEffect.
    let taunt_ids: std::collections::BTreeSet<u32> = taunts.iter().map(|(id, _)| *id).collect();
    assert_eq!(taunt_ids.difference(&applied_buffs).count(), 49);
    let initial: i64 = conn
        .query_row(
            "SELECT count(*) FROM npc_initial_buffs WHERE buff_id IN
             (SELECT id FROM buffs WHERE taunt='t')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(initial, 2, "buff 6998 is an NPC initial buff");
}

const STEALTH_COLUMN_BUFF: u32 = 7_100;
const TAG_100_ONLY_BUFF: u32 = 7_101;

/// [`fixture`] with the caller's game data.
fn fixture_with_data(data: GameData, player_x: f32) -> Fixture {
    let server = server_with_data(data);
    let client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Runner", player_x, 100.0);
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    let player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        NPC_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    Fixture {
        server,
        client,
        player,
        npc,
    }
}

/// `buffs.stealth` = 't' without tag 100: the 1.2 client hides the unit, so the AI must not
/// aggro it either (the C# `CanSeeTarget` only checked tag 100).
#[tokio::test]
async fn stealth_column_buff_without_tag_100_hides_the_player_from_aggro() {
    let mut data = data();
    data.systems.skills.add_buff(BuffTemplate {
        id: STEALTH_COLUMN_BUFF,
        kind: 1,
        duration: 60_000,
        stack_rule: 1,
        stealth: true,
        ..BuffTemplate::default()
    });
    let f = fixture_with_data(data, 115.0);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    crate::systems::skills::buffs::add_buff_by_id(&server, &me, STEALTH_COLUMN_BUFF, &me, 0);
    assert!(crate::systems::skills::buffs::has_stealth(&server, &me));
    assert!(
        !super::npc::can_see_target(&server, &npc, &me),
        "the stealth column must mark the player stealthed for the AI"
    );

    let attacked = tick_until(&server, 100, 20, 40, || {
        behavior(&server, NPC_OBJ) == Some(B::Attack)
    })
    .await;
    assert!(!attacked, "the NPC aggroed a buff.stealth player");
    assert!(f.npc.data().unit.skill_state.aggro_table.is_empty());
}

/// Tag 100 without `buffs.stealth`: the 1.2 client shows the unit, so the AI must see it too.
#[tokio::test]
async fn tag_100_buff_without_the_stealth_column_does_not_hide_the_player_from_aggro() {
    let mut data = data();
    data.systems.skills.add_buff(BuffTemplate {
        id: TAG_100_ONLY_BUFF,
        kind: 1,
        duration: 60_000,
        stack_rule: 1,
        stealth: false,
        ..BuffTemplate::default()
    });
    // Tag 100 is the C# stealth marker only; the client does not use it.
    data.systems
        .skills
        .tagged_buffs
        .insert(100, vec![TAG_100_ONLY_BUFF]);
    let f = fixture_with_data(data, 115.0);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    crate::systems::skills::buffs::add_buff_by_id(&server, &me, TAG_100_ONLY_BUFF, &me, 0);
    assert!(
        super::npc::can_see_target(&server, &npc, &me),
        "tag 100 alone must not hide the player"
    );

    let attacked = tick_until(&server, 100, 20, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::Attack)
    })
    .await;
    assert!(
        attacked,
        "the NPC never aggroed a tag-100 non-stealth player"
    );
    assert!(f
        .npc
        .data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&PLAYER_OBJ));
}

const DETECT_STEALTH_BUFF: u32 = 7_102;
const ANTI_STEALTH_BUFF: u32 = 7_103;

/// [`fixture_with_data`] with a free player position and NPC facing (`rot_z`, the `MathUtil.
/// IsFront` yaw: -PI/2 faces +x). `distance_to` subtracts the model radii (the NPC's 0.5 m
/// here), and the fixture stealth buff has `ab_level` 1, so formula 4 is 12 / 1.02 = 11.76 m.
fn stealth_fixture(data: GameData, player_x: f32, player_y: f32, npc_rot_z: f32) -> Fixture {
    let server = server_with_data(data);
    let client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Runner", player_x, player_y);
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    let player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        NPC_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    npc.data().unit.transform.rot_z = npc_rot_z;
    Fixture {
        server,
        client,
        player,
        npc,
    }
}

fn apply(server: &GameServer, owner: &WorldObject, buff_id: u32) {
    crate::systems::skills::buffs::add_buff_by_id(server, owner, buff_id, owner, 0);
}

/// Walks straight at the player: true once the NPC's behavior is `Attack`.
async fn aggros(server: &Arc<GameServer>, ticks: usize) -> bool {
    tick_until(server, 100, 20, ticks, || {
        behavior(server, NPC_OBJ) == Some(B::Attack)
    })
    .await
}

const FACE_PLUS_X: f32 = -std::f32::consts::FRAC_PI_2;

/// Formula 4 (`DetectStealthRangeFront`, 12 m at equal levels) is evaluated: an 11 m stealthed
/// player in front is detected (the old predicate hid every stealthed unit outside 1 m).
#[tokio::test]
async fn stealth_detection_front_formula_detects_inside_range() {
    let f = stealth_fixture(data(), 111.0, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    assert!(
        super::npc::can_see_target(&server, &npc, &me),
        "11 m in front is inside formula 4"
    );
    assert!(aggros(&server, 60).await, "the NPC never aggroed at 11 m");
    assert!(f
        .npc
        .data()
        .unit
        .skill_state
        .aggro_table
        .contains_key(&PLAYER_OBJ));
}

/// Formula 4's range is real: 13 m in front is outside it.
#[tokio::test]
async fn stealth_detection_front_formula_misses_outside_range() {
    let f = stealth_fixture(data(), 113.0, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    assert!(
        !super::npc::can_see_target(&server, &npc, &me),
        "13 m in front is outside formula 4"
    );
    assert!(!aggros(&server, 40).await, "the NPC aggroed at 13 m");
}

/// Formula 10 (`DetectStealthRangeBack`, 0.5 m) applies when the target is behind the observer:
/// 0.9 m centre to centre becomes 0.4 m after `distance_to` subtracts the NPC's 0.5 m model
/// radius, so it is inside the 0.5 m back range.
#[tokio::test]
async fn stealth_detection_back_formula_detects_close_behind() {
    let f = stealth_fixture(data(), 99.1, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    assert!(
        super::npc::can_see_target(&server, &npc, &me),
        "0.4 m behind after the model radii is inside formula 10"
    );
    assert!(aggros(&server, 60).await, "the NPC never aggroed at 0.9 m");
}

/// The old C# `< 1.0` / `< 0.5` fallbacks no longer decide: 1.5 m behind is outside formula 10.
#[tokio::test]
async fn stealth_detection_back_formula_misses_far_behind() {
    let f = stealth_fixture(data(), 98.5, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    assert!(
        !super::npc::can_see_target(&server, &npc, &me),
        "1.5 m behind is outside formula 10"
    );
    assert!(
        !aggros(&server, 40).await,
        "the NPC aggroed at 1.5 m behind"
    );
}

/// `UnitAttribute.DetectStealthRangeMul` (94) is applied: -100 turns the range to 0, so even a
/// 0.3 m target is not detected (the C# `< 1.0` fallback used to make it visible).
#[tokio::test]
async fn stealth_detection_old_front_fallback_no_longer_decides() {
    let mut data = data();
    data.systems
        .npcs
        .npc
        .templates
        .get_mut(&NPC_TEMPLATE)
        .expect("template")
        .bonuses
        .push(BonusTemplate {
            attribute: UnitAttribute::DetectStealthRangeMul,
            modifier_type: UnitModifierType::Value,
            value: -100,
            linear_level_bonus: 0,
        });
    let f = stealth_fixture(data, 100.3, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    assert!(
        !super::npc::can_see_target(&server, &npc, &me),
        "attribute 94 -100 must zero the detection range"
    );
    assert!(
        !aggros(&server, 40).await,
        "the NPC aggroed a stealthed player at 0.3 m with range 0"
    );
}

/// `buffs.detect_stealth` on the observer reveals stealthed units at any distance.
#[tokio::test]
async fn detect_stealth_buff_reveals_at_long_range() {
    let f = stealth_fixture(data(), 115.0, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    apply(&server, &npc, DETECT_STEALTH_BUFF);
    assert!(
        super::npc::can_see_target(&server, &npc, &me),
        "a detect_stealth observer must see the stealthed player"
    );
    assert!(aggros(&server, 60).await, "the NPC never aggroed at 15 m");
}

/// `buffs.anti_stealth` on the target (the client's "cannot hide" state) makes it visible.
#[tokio::test]
async fn anti_stealth_buff_reveals_at_long_range() {
    let f = stealth_fixture(data(), 115.0, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    apply(&server, &me, ANTI_STEALTH_BUFF);
    assert!(
        super::npc::can_see_target(&server, &npc, &me),
        "an anti_stealth target must be visible"
    );
    assert!(aggros(&server, 60).await, "the NPC never aggroed at 15 m");
}

/// `stealth_level` is the highest `ab_level` of the active stealth buffs (the client's
/// `ClientBuffMan` +4): level 50 halves formula 4 (12 m -> 6 m), so 7 m is outside it.
#[tokio::test]
async fn stealth_level_from_the_buff_ab_level_shrinks_the_range() {
    let f = stealth_fixture(data(), 107.0, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    me.with_base_unit(|b| {
        b.buffs_mut()
            .effects
            .last_mut()
            .expect("stealth buff")
            .ab_level = 50;
    });
    assert!(
        !super::npc::can_see_target(&server, &npc, &me),
        "stealth level 50 halves the 12 m front range"
    );
    assert!(!aggros(&server, 40).await, "the NPC aggroed at 7 m");
}

/// A positive `DetectStealthRangeMul` widens the range: +100 doubles 11.76 m to 23.5 m.
#[tokio::test]
async fn detect_stealth_range_mul_widens_the_range() {
    let mut data = data();
    data.systems
        .npcs
        .npc
        .templates
        .get_mut(&NPC_TEMPLATE)
        .expect("template")
        .bonuses
        .push(BonusTemplate {
            attribute: UnitAttribute::DetectStealthRangeMul,
            modifier_type: UnitModifierType::Value,
            value: 100,
            linear_level_bonus: 0,
        });
    let f = stealth_fixture(data, 115.0, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    assert!(
        super::npc::can_see_target(&server, &npc, &me),
        "attribute 94 +100 widens formula 4 to 23.5 m"
    );
    assert!(aggros(&server, 60).await, "the NPC never aggroed at 15 m");
}

/// [`data`] with a helper template whose aggro-link sight check is enabled and whose special
/// rule 1 (same faction) requires the helper itself to see the abuser.
fn help_sight_data() -> GameData {
    let mut data = data();
    let helper = data
        .systems
        .npcs
        .npc
        .templates
        .get_mut(&HELPER_TEMPLATE)
        .expect("helper template");
    helper.accept_aggro_link = true;
    helper.aggro_link_sight_check = true;
    helper.aggro_link_special_rule_id = 1;
    helper.aggro_link_help_dist = 30.0;
    data
}

/// A primary hold-position NPC at (125, 100) (15 m from the helper, outside its own 11.76 m
/// front range) and a helper at (110, 100) facing +y; the player is at the caller's position.
fn help_fixture(data: GameData, player_x: f32, player_y: f32) -> (Fixture, NpcRef) {
    let server = server_with_data(data);
    let client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Runner", player_x, player_y);
    c.unit.hp = 1000;
    c.unit.max_hp = 1000;
    let player = spawn_player(&server, &client, c, PLAYER_OBJ);
    let primary = spawn_npc(
        &server,
        NPC_OBJ,
        NPC_TEMPLATE,
        125.0,
        100.0,
        AiKind::HoldPosition,
    );
    primary.data().unit.transform.rot_z = -std::f32::consts::FRAC_PI_2;
    let helper = spawn_npc(
        &server,
        HELPER_OBJ,
        HELPER_TEMPLATE,
        110.0,
        100.0,
        AiKind::HoldPosition,
    );
    helper.data().unit.transform.rot_z = 0.0; // faces +y
    (
        Fixture {
            server,
            client,
            player,
            npc: primary,
        },
        helper,
    )
}

/// `UpdateAggroHelp` asks the *helper* `CanSeeTarget(abuser)`, not the primary NPC: the stealthed
/// abuser 5 m in front of the helper is seen, while the primary 15.8 m away is outside its own
/// 11.76 m front range. The fixture's stealth buff has `ab_level` 1, so formula 4 is
/// 12 / 1.02 = 11.76 m.
#[tokio::test]
async fn aggro_help_uses_the_helper_npcs_stealth_detection_front() {
    let (f, helper) = help_fixture(help_sight_data(), 110.0, 105.0);
    let server = f.server.clone();
    let abuser = WorldObject::from(&f.player);
    apply(&server, &abuser, STEALTH_COLUMN_BUFF);
    post(&server, NPC_OBJ, move |c| {
        super::behaviors::update_aggro_help(c, &abuser, 200.0);
    });
    tick_all(&server, 100);
    assert!(
        helper
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&PLAYER_OBJ),
        "the helper did not join although it can see the stealthed abuser"
    );
}

/// The same call with the abuser 5 m behind the helper: formula 10 is 0.5 m, so the helper does
/// not see it and does not join.
#[tokio::test]
async fn aggro_help_uses_the_helper_npcs_stealth_detection_behind() {
    let (f, helper) = help_fixture(help_sight_data(), 110.0, 95.0);
    let server = f.server.clone();
    let abuser = WorldObject::from(&f.player);
    apply(&server, &abuser, STEALTH_COLUMN_BUFF);
    post(&server, NPC_OBJ, move |c| {
        super::behaviors::update_aggro_help(c, &abuser, 200.0);
    });
    tick_all(&server, 100);
    assert!(
        helper.data().unit.skill_state.aggro_table.is_empty(),
        "the helper joined although the stealthed abuser is behind it"
    );
}

/// A missing formula row uses the same visible fallback as a broken one: a stealthed target
/// must not become permanently invisible just because the data is absent.
#[tokio::test]
async fn stealth_detection_missing_formula_keeps_target_visible() {
    let mut data = data();
    data.formulas = FormulaManager::empty();
    let f = stealth_fixture(data, 115.0, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    assert!(
        super::npc::can_see_target(&server, &npc, &me),
        "a missing formula must leave stealthed units visible"
    );
    assert!(aggros(&server, 60).await, "the NPC never aggroed at 15 m");
}

/// A formula that fails to evaluate (here: an undefined variable) also uses the visible
/// fallback and must not be re-evaluated with an `error!` log on every scan.
#[tokio::test]
async fn stealth_detection_broken_formula_keeps_target_visible() {
    let mut data = data();
    data.formulas = broken_stealth_formulas();
    let f = stealth_fixture(data, 115.0, 100.0, FACE_PLUS_X);
    let server = f.server.clone();
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    apply(&server, &me, STEALTH_COLUMN_BUFF);
    assert!(
        super::npc::can_see_target(&server, &npc, &me),
        "a formula that fails to evaluate must leave stealthed units visible"
    );
    assert!(aggros(&server, 60).await, "the NPC never aggroed at 15 m");
}

// ===========================================================================================
// WP-5a: FollowUnit / WP-5b: ai_command_sets.can_interact
// ===========================================================================================

fn set_command(cmd_set_id: u32, cmd_id: AiCommandCategory, param1: u32) -> AiCommands {
    AiCommands {
        id: 0,
        cmd_set_id,
        cmd_id,
        param1,
        param2: String::new(),
    }
}

/// The bodies of the frames of one opcode (level-2 frames keep the `0x8000` tag).
fn drain_op(client: &mut TestClient, op: u16) -> Vec<Vec<u8>> {
    client
        .drain()
        .into_iter()
        .filter(|(o, _)| *o == op)
        .map(|(_, body)| body)
        .collect()
}

/// A FollowUnit command follows the nearest player (20 m away) instead of only warning, and
/// the NPC enters `FollowUnit` and closes the gap.
#[tokio::test]
async fn follow_unit_command_follows_the_nearest_player() {
    let server = server();
    let client = TestClient::new(&server, 1, 1);
    let mut guide = test_character(1, "Guide", 120.0, 100.0);
    guide.unit.hp = 1000;
    guide.unit.max_hp = 1000;
    let player = spawn_player(&server, &client, guide, PLAYER_OBJ);
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(
        settled,
        "the NPC did not settle: {:?}",
        behavior(&server, NPC_OBJ)
    );

    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[set_command(PLAIN_SET, AiCommandCategory::FollowUnit, 1)],
            false,
        );
    });
    let following = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::FollowUnit)
    })
    .await;
    assert!(
        following,
        "the FollowUnit command did not start the follow behavior: {:?}",
        behavior(&server, NPC_OBJ)
    );
    assert_eq!(
        server
            .systems
            .ai
            .ais
            .inspect(NPC_OBJ, |a| a.follow_unit.as_ref().map(WorldObject::obj_id)),
        Some(Some(PLAYER_OBJ)),
        "the fallback must pick the nearby player"
    );
    let start = npc_pos(&npc);
    let moved = tick_until(&server, 100, 5, 60, || npc_pos(&npc).x > start.x + 2.0).await;
    assert!(
        moved,
        "the NPC did not move towards the player: {:?}",
        npc_pos(&npc)
    );
    assert!(player.data().unit.hp > 0);
}

/// Without a `follow_unit` and without any player in 100 m the command does not hang the set:
/// the remaining commands run and the NPC settles back into its default behavior.
#[tokio::test]
async fn follow_unit_command_without_a_player_does_not_hang_the_set() {
    let server = server();
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    // No player exists, so the spawn initialization settles into the unselected Idle state.
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::Idle)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));

    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[
                set_command(PLAIN_SET, AiCommandCategory::FollowUnit, 1),
                set_command(PLAIN_SET, AiCommandCategory::Timeout, 1),
            ],
            false,
        );
    });
    let finished = tick_until(&server, 100, 5, 60, || {
        server.systems.ai.ais.inspect(NPC_OBJ, |a| {
            a.current_command.is_none()
                && a.commands_queue.is_empty()
                && a.follow_unit.is_none()
                // No player is near, so the settled Idle AI is no longer selected and does
                // not run its default transition.
                && a.current_behavior() == Some(B::Idle)
        }) == Some(true)
    })
    .await;
    assert!(
        finished,
        "the set did not finish: behavior {:?}, state {:?}",
        behavior(&server, NPC_OBJ),
        server.systems.ai.ais.inspect(NPC_OBJ, |a| (
            a.current_command.clone(),
            a.commands_queue.len()
        ))
    );
    assert_eq!(npc_pos(&npc), Vec3::new(100.0, 100.0, 100.0));
}

/// With `Interaction.CommandSetLock` off (the default) a `can_interact = 'f'` set locks nothing.
#[tokio::test]
async fn locked_command_set_is_ignored_without_the_config_flag() {
    let server = server();
    server
        .config
        .update(|cfg| cfg.interaction.command_set_lock = false);
    let mut client = TestClient::new(&server, 1, 1);
    let _player = spawn_player(
        &server,
        &client,
        test_character(1, "Talker", 110.0, 100.0),
        PLAYER_OBJ,
    );
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));
    client.drain();
    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[set_command(LOCKED_SET, AiCommandCategory::Timeout, 150)],
            false,
        );
    });
    tick_all(&server, 100);
    assert!(npc.data().interactable);
    assert!(drain_op(&mut client, sc::SCNpcInteractionStatusChangedPacket).is_empty());
}

/// A set with `can_interact = 'f'` sends the status `false` to viewers when it starts, closes a
/// window a player still has open on the NPC (`SCNpcInteractionEndedByZone`), and sends `true`
/// when the set ends.
#[tokio::test]
async fn locked_command_set_publishes_the_interaction_status() {
    let server = server();
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Talker", 110.0, 100.0),
        PLAYER_OBJ,
    );
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));
    client.drain();

    // The player has the NPC's interaction window open when the set starts.
    player.data().unit.current_interaction_object = NPC_OBJ;
    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[set_command(LOCKED_SET, AiCommandCategory::Timeout, 150)],
            false,
        );
    });
    tick_all(&server, 100);
    assert!(!npc.data().interactable, "the locked set did not lock");
    let frames = client.drain();
    let status: Vec<Vec<u8>> = frames
        .iter()
        .filter(|(o, _)| *o == sc::SCNpcInteractionStatusChangedPacket)
        .map(|(_, body)| body.clone())
        .collect();
    let ended_by_zone: Vec<Vec<u8>> = frames
        .iter()
        .filter(|(o, _)| *o == sc::SCNpcInteractionEndedByZonePacket)
        .map(|(_, body)| body.clone())
        .collect();
    assert_eq!(
        status,
        vec![vec![0x00, 0x02, 0x00, 0x00]],
        "viewers must get status false at set start"
    );
    assert_eq!(
        ended_by_zone,
        vec![vec![0x00, 0x02, 0x00]],
        "an open interaction window must be closed"
    );
    assert_eq!(player.data().unit.current_interaction_object, 0);

    let ended = tick_until(&server, 100, 5, 60, || {
        server.systems.ai.ais.inspect(NPC_OBJ, |a| {
            a.current_command.is_none()
                && a.commands_queue.is_empty()
                && a.locked_command_set.is_none()
                && a.current_behavior() == Some(B::HoldPosition)
        }) == Some(true)
    })
    .await;
    assert!(ended, "{:?}", behavior(&server, NPC_OBJ));
    assert!(npc.data().interactable);
    let frames = client.drain();
    let status: Vec<Vec<u8>> = frames
        .iter()
        .filter(|(o, _)| *o == sc::SCNpcInteractionStatusChangedPacket)
        .map(|(_, body)| body.clone())
        .collect();
    assert_eq!(
        status,
        vec![vec![0x00, 0x02, 0x00, 0x01]],
        "viewers must get status true when the set ends"
    );
    assert!(!frames
        .iter()
        .any(|(o, _)| *o == sc::SCNpcInteractionEndedByZonePacket));
}

/// A player that enters sight after the set started still learns the `false` state from the
/// visibility path (the status packet follows `SCUnitStatePacket`).
#[tokio::test]
async fn late_viewer_gets_the_false_interaction_status() {
    let server = server();
    let mut far_client = TestClient::new(&server, 2, 2);
    let far = spawn_player(
        &server,
        &far_client,
        test_character(2, "Latecomer", 1500.0, 1500.0),
        PLAYER_OBJ + 1,
    );
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    // The only player is far away, so the spawn initialization settles into Idle.
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::Idle)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));
    far_client.drain();

    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[set_command(LOCKED_SET, AiCommandCategory::Timeout, 100_000)],
            false,
        );
    });
    tick_all(&server, 100);
    assert!(!npc.data().interactable);
    assert!(
        drain_op(&mut far_client, sc::SCNpcInteractionStatusChangedPacket).is_empty(),
        "a player far away must not receive the status"
    );

    // Enter the NPC's neighbourhood.
    far.data().unit.transform.set_xyz(101.0, 100.0, 100.0);
    let world = server.world.world(0).expect("world");
    world.add_visible_object(&far, &server.view_ctx());
    let frames = far_client.drain();
    let ops: Vec<u16> = frames.iter().map(|(o, _)| *o).collect();
    assert!(
        ops.contains(&sc::SCUnitStatePacket),
        "the late viewer must see the NPC: {ops:?}"
    );
    let status: Vec<Vec<u8>> = frames
        .into_iter()
        .filter(|(o, _)| *o == sc::SCNpcInteractionStatusChangedPacket)
        .map(|(_, body)| body)
        .collect();
    assert_eq!(status, vec![vec![0x00, 0x02, 0x00, 0x00]]);
}

/// `CSStartInteraction` is refused while the locked set runs and accepted once it ended.
#[tokio::test]
async fn start_interaction_is_refused_while_the_set_is_locked() {
    use crate::net::registry::ClientPacket;

    let server = server();
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Clicker", 105.0, 100.0),
        PLAYER_OBJ,
    );
    client.conn.active_char = Some(player.clone());
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));
    client.drain();

    let request = crate::systems::npcs::npc::packets::CSStartInteractionPacket {
        npc_obj_id: NPC_OBJ,
        obj_id: PLAYER_OBJ,
        extra_info: 0,
        pick_id: 0,
        mouse_button: 1,
        modifier_keys: 0,
    };

    // `param1 = 0` ends the Timeout on the following tick (the command then clears).
    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[set_command(LOCKED_SET, AiCommandCategory::Timeout, 0)],
            false,
        );
    });
    tick_all(&server, 100);
    assert!(!npc.data().interactable, "the set must be running");
    client.drain();
    request.handle(&server, &mut client.conn).await.unwrap();
    assert!(
        drain_op(&mut client, sc::SCNpcInteractionSkillListPacket).is_empty(),
        "a non-interactable NPC must not answer CSStartInteraction"
    );

    let ended = tick_until(&server, 100, 5, 60, || {
        server.systems.ai.ais.inspect(NPC_OBJ, |a| {
            a.current_command.is_none()
                && a.commands_queue.is_empty()
                && a.locked_command_set.is_none()
        }) == Some(true)
    })
    .await;
    assert!(ended, "{:?}", behavior(&server, NPC_OBJ));
    assert!(npc.data().interactable);
    client.drain();
    request.handle(&server, &mut client.conn).await.unwrap();
    assert_eq!(
        drain_op(&mut client, sc::SCNpcInteractionSkillListPacket).len(),
        1,
        "the NPC must answer once the set ended"
    );
}

/// Sets with `can_interact = 't'` (and commands that name no set) keep the old behaviour: no
/// interaction status packet at start or end, and the NPC stays interactable.
#[tokio::test]
async fn interactable_command_sets_send_no_interaction_packets() {
    let server = server();
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Talker", 110.0, 100.0),
        PLAYER_OBJ,
    );
    client.conn.active_char = Some(player.clone());
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));
    client.drain();

    for set_id in [OPEN_SET, PLAIN_SET] {
        post(&server, NPC_OBJ, move |c| {
            c.enqueue_ai_commands(
                &[set_command(set_id, AiCommandCategory::Timeout, 150)],
                false,
            );
        });
        let ended = tick_until(&server, 100, 5, 60, || {
            server.systems.ai.ais.inspect(NPC_OBJ, |a| {
                a.current_command.is_none()
                    && a.commands_queue.is_empty()
                    && a.current_behavior() == Some(B::HoldPosition)
            }) == Some(true)
        })
        .await;
        assert!(ended, "set {set_id}: {:?}", behavior(&server, NPC_OBJ));
        assert!(
            npc.data().interactable,
            "set {set_id} must stay interactable"
        );
    }
    let frames = client.drain();
    let ops: Vec<u16> = frames.iter().map(|(o, _)| *o).collect();
    assert!(
        !ops.contains(&sc::SCNpcInteractionStatusChangedPacket)
            && !ops.contains(&sc::SCNpcInteractionEndedByZonePacket),
        "no interaction packets expected, got {ops:?}"
    );
}

/// A run interrupted by combat abandons the command set (the state machine never returns to
/// `RunCommandSet`), so the interaction lock must be released instead of leaving the NPC
/// non-interactable forever.
#[tokio::test]
async fn interrupted_locked_set_releases_the_interaction_status() {
    let server = server();
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Watcher", 110.0, 100.0),
        PLAYER_OBJ,
    );
    client.conn.active_char = Some(player.clone());
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));
    client.drain();

    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[set_command(LOCKED_SET, AiCommandCategory::Timeout, 100_000)],
            false,
        );
    });
    tick_all(&server, 100);
    assert!(!npc.data().interactable);
    client.drain();

    // Aggro pulls the NPC out of RunCommandSet; the set is abandoned, not resumed.
    post(&server, NPC_OBJ, |c| c.go_to_combat());
    tick_all(&server, 100);
    assert!(
        npc.data().interactable,
        "an abandoned set must not leave the NPC locked: {:?}",
        behavior(&server, NPC_OBJ)
    );
    assert_eq!(
        drain_op(&mut client, sc::SCNpcInteractionStatusChangedPacket),
        vec![vec![0x00, 0x02, 0x00, 0x01]]
    );
}

/// A different command set that starts while the locked one is still queued replaces the
/// interaction state: the `can_interact = 't'` set unlocks the NPC before its set ends.
#[tokio::test]
async fn replaced_locked_set_unlocks_the_npc() {
    let server = server();
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Watcher", 110.0, 100.0),
        PLAYER_OBJ,
    );
    client.conn.active_char = Some(player.clone());
    let npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    let settled = tick_until(&server, 100, 5, 60, || {
        behavior(&server, NPC_OBJ) == Some(B::HoldPosition)
    })
    .await;
    assert!(settled, "{:?}", behavior(&server, NPC_OBJ));
    client.drain();

    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[set_command(LOCKED_SET, AiCommandCategory::Timeout, 0)],
            false,
        );
    });
    tick_all(&server, 100);
    assert!(!npc.data().interactable);
    client.drain();

    // The open set is queued behind the locked command.
    post(&server, NPC_OBJ, |c| {
        c.enqueue_ai_commands(
            &[set_command(OPEN_SET, AiCommandCategory::Timeout, 150)],
            false,
        );
    });
    // One tick clears the finished locked command, the next starts the open set.
    tick_all(&server, 100);
    assert!(
        !npc.data().interactable,
        "the locked command is still ending"
    );
    tick_all(&server, 100);
    assert_eq!(
        behavior(&server, NPC_OBJ),
        Some(B::RunCommandSet),
        "the open set must still be running"
    );
    assert!(
        npc.data().interactable,
        "the replacing 't' set must unlock the NPC"
    );
    assert_eq!(
        drain_op(&mut client, sc::SCNpcInteractionStatusChangedPacket),
        vec![vec![0x00, 0x02, 0x00, 0x01]]
    );
}

/// A despawn (`NpcManager.Delete` -> the AI hook) closes the open interaction window, like the
/// death behavior does (INFERRED, see PORTING.md WP-5b).
#[tokio::test]
async fn deleted_npc_closes_the_open_interaction_window() {
    let server = server();
    let mut client = TestClient::new(&server, 1, 1);
    let player = spawn_player(
        &server,
        &client,
        test_character(1, "Talker", 110.0, 100.0),
        PLAYER_OBJ,
    );
    client.conn.active_char = Some(player.clone());
    let _npc = spawn_npc(
        &server,
        NPC_OBJ,
        PASSIVE_TEMPLATE,
        100.0,
        100.0,
        AiKind::HoldPosition,
    );
    client.drain();

    player.data().unit.current_interaction_object = NPC_OBJ;
    server
        .systems
        .hooks
        .ai
        .get()
        .expect("ai hooks")
        .npc_deleted(&server, NPC_OBJ);
    assert_eq!(
        drain_op(&mut client, sc::SCNpcInteractionEndedByZonePacket),
        vec![vec![0x00, 0x02, 0x00]]
    );
    assert_eq!(player.data().unit.current_interaction_object, 0);
}

// --- pull aggro (`Ai.PullAggro*`, added deviation) ---------------------------------------

/// The fixture's players stand 3 m (A, melee), 10 m (B) and 12 m (C) from the NPC; the pull
/// aggro melee range is the default 4 m.
async fn pull_fixture(enabled: bool) -> TauntFixture {
    let f = taunt_fixture();
    f.server
        .config
        .update(|cfg| cfg.ai.pull_aggro_enabled = enabled);
    settle(&f).await;
    f
}

#[tokio::test]
async fn pull_aggro_melee_challenger_needs_the_melee_ratio() {
    let f = pull_fixture(true).await;
    add_threat(&f, &f.player_b, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);

    add_threat(&f, &f.player_a, 105);
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        TAUNTER_OBJ,
        "105% of the target's threat must not pull"
    );
    add_threat(&f, &f.player_a, 10);
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        PLAYER_OBJ,
        "115% from melee range must pull"
    );
}

#[tokio::test]
async fn pull_aggro_ranged_challenger_needs_the_ranged_ratio() {
    let f = pull_fixture(true).await;
    add_threat(&f, &f.player_b, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);

    add_threat(&f, &f.player_c, 125);
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        TAUNTER_OBJ,
        "125% from range must not pull"
    );
    add_threat(&f, &f.player_c, 10);
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        TAUNTER2_OBJ,
        "135% from range must pull"
    );
}

#[tokio::test]
async fn pull_aggro_taunted_challenger_pulls_regardless_of_threat() {
    let f = pull_fixture(true).await;
    add_threat(&f, &f.player_a, 1000);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);

    apply_taunt(&f, TAUNT_BUFF, &f.player_b);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);

    // A stamped current target cannot be pulled by threat.
    add_threat(&f, &f.player_c, 100_000);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);
}

#[tokio::test]
async fn pull_aggro_dead_current_target_switches_to_the_highest_valid_entry() {
    let f = pull_fixture(true).await;
    add_threat(&f, &f.player_a, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, PLAYER_OBJ);
    add_threat(&f, &f.player_b, 50);
    add_threat(&f, &f.player_c, 60);
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        PLAYER_OBJ,
        "neither challenger clears the ratio"
    );

    f.player_a.data().unit.hp = 0;
    tick_all(&f.server, 100);
    assert_eq!(
        f.npc.data().unit.current_target,
        TAUNTER2_OBJ,
        "a dead current target is replaced by the highest valid entry"
    );
}

#[tokio::test]
async fn pull_aggro_disabled_switches_at_101_percent() {
    let f = pull_fixture(false).await;
    add_threat(&f, &f.player_b, 100);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER_OBJ);

    add_threat(&f, &f.player_c, 101);
    tick_all(&f.server, 100);
    assert_eq!(f.npc.data().unit.current_target, TAUNTER2_OBJ);
}
