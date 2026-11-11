//! The 1.2 client's post-cast (persistent-mana-regen) window.
//!
//! `X2::Unit::UpdateManaRegen` (`FUN_390b63f0` @ 0x390b63f0) uses `persistent_mana_regen`
//! (attribute 68) and only resumes `mana_regen` (attribute 12) 5 s after `LastCast`.
//! `LastCast` is started by `OnSkillFired` (`FUN_3939b010` @ 0x3939b010, DEC 750714) when the
//! skill has a `timing_id` (`+0x1bc`) and no `keep_mana_regen` (`+0xdd`), by the
//! `StopManaRegen` plot effect (`FUN_3938e6b0`, case 0x34), and at the end of a channel
//! (`FUN_39397ff0` @ 0x39397ff0, DEC 748451). The item-use skills (regrade scrolls 22520 /
//! 22522 and every other `keep_mana_regen` row) keep normal mana regen.

use std::sync::Arc;

use crate::data::GameData;
use crate::models::unit::{BonusTemplate, UnitAttribute, UnitModifierType};
use crate::opcodes::sc;
use crate::world::{now_ms, WorldObject};

use super::cast_validation_tests::fixture;
use super::data::SkillTemplate;
use super::skill::{end_channeling, use_skill};
use super::state::SkillRun;
use super::types::{skill_result, SkillCastTarget, SkillCaster, SkillObject};

const NPC_OBJ: u32 = 0x200;

/// `MaxHealth` 5000 / `MaxMana` 1000 (the fixture's cached fields are not the getters), `ManaRegen` 25 and
/// `PersistentManaRegen` 5 (composed: normal 26, post-cast 1).
fn add_regen_bonuses(player: &crate::world::PlayerRef) {
    let mut c = player.data();
    for (index, attribute, value) in [
        (900, UnitAttribute::MaxHealth, 5000),
        (901, UnitAttribute::MaxMana, 1000),
        (902, UnitAttribute::ManaRegen, 25),
        (903, UnitAttribute::PersistentManaRegen, 5),
    ] {
        c.unit.bonuses.add_bonus(
            index,
            crate::models::unit::Bonus {
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
}

fn item_cast(
    server: &crate::GameServer,
    player: &crate::world::PlayerRef,
    template: SkillTemplate,
) {
    let skill = SkillRun::new(Arc::new(template), 1);
    let caster = WorldObject::from(player);
    let (r, _) = use_skill(
        server,
        &skill,
        &caster,
        SkillCaster::Item {
            obj_id: player.obj_id,
            item_id: 1,
            item_template_id: 28298,
            type1: 0,
            type2: 0,
        },
        &mut SkillCastTarget::Unit(NPC_OBJ),
        Some(SkillObject::None),
        false,
    );
    assert_eq!(r, skill_result::SUCCESS);
}

/// The regrade scrolls (weapon / armor regrade, skills 22520 / 22522) are ability-less,
/// 0-mana item skills with `timing_id 1` **and** `keep_mana_regen` true: the client neither
/// starts its post-cast window nor does the server send a zero-change `SCUnitPoints`. The
/// character keeps the normal `ManaRegen` rate.
#[tokio::test]
async fn regrade_item_cast_keeps_normal_mana_regen() {
    let (server, mut client, player, _npc) = fixture(GameData::empty());
    client.drain();
    item_cast(
        &server,
        &player,
        SkillTemplate {
            id: 22520,
            ability_id: 0,
            mana_cost: 0,
            mana_level_md: 0.0,
            timing_id: 1,
            keep_mana_regen: true,
            ..SkillTemplate::default()
        },
    );
    {
        let p = player.data();
        assert!(!p.skill_char.is_in_post_cast, "the client keeps mana regen");
        assert_eq!(p.skill_char.last_cast_ms, 0);
        assert_eq!(p.unit.mp, 100);
    }
    let ops = client.drain_ops();
    assert!(
        !ops.contains(&sc::SCUnitPointsPacket),
        "a 0-mana cast must not send SCUnitPoints: {ops:x?}"
    );
    add_regen_bonuses(&player);
    super::regen::active_region_tick(&server);
    let p = player.data();
    assert_eq!(p.unit.mp, 126, "normal ManaRegen (25) + persistent (1)");
}

/// Spending mana is not what starts the client's window: a 0-cost skill without
/// `keep_mana_regen` still starts it (`OnSkillFired` checks `timing_id` / `keep_mana_regen`
/// only), and a skill without a `timing_id` never does.
#[tokio::test]
async fn zero_cost_skill_starts_the_window_only_with_a_timing_id() {
    for (timing_id, expected) in [(1, true), (0, false)] {
        let (server, _client, player, _npc) = fixture(GameData::empty());
        item_cast(
            &server,
            &player,
            SkillTemplate {
                id: 9001,
                ability_id: 0,
                mana_cost: 0,
                mana_level_md: 0.0,
                timing_id,
                ..SkillTemplate::default()
            },
        );
        assert_eq!(player.data().skill_char.is_in_post_cast, expected);
    }
}

/// A class skill that spends mana still enters the 5 s post-cast window (persistent rate),
/// then returns to the normal rate after `LastCast + 5000`.
#[tokio::test]
async fn class_skill_that_spends_mana_enters_post_cast_for_five_seconds() {
    let (server, mut client, player, _npc) = fixture(GameData::empty());
    client.drain();
    let skill = SkillRun::new(
        Arc::new(SkillTemplate {
            id: 9002,
            ability_id: 1,
            mana_cost: 25,
            timing_id: 1,
            ..SkillTemplate::default()
        }),
        1,
    );
    let caster = WorldObject::from(&player);
    let (r, _) = use_skill(
        &server,
        &skill,
        &caster,
        SkillCaster::Unit(player.obj_id),
        &mut SkillCastTarget::Unit(NPC_OBJ),
        Some(SkillObject::None),
        false,
    );
    assert_eq!(r, skill_result::SUCCESS);
    {
        let p = player.data();
        assert_eq!(p.unit.mp, 75, "the skill's 25 mana was taken");
        assert!(p.skill_char.is_in_post_cast);
        assert!(p.skill_char.last_cast_ms > 0);
    }
    let ops = client.drain_ops();
    assert!(
        ops.contains(&sc::SCUnitPointsPacket),
        "a mana-changing cast syncs the bar: {ops:x?}"
    );
    add_regen_bonuses(&player);
    super::regen::active_region_tick(&server);
    assert_eq!(
        player.data().unit.mp,
        76,
        "persistent regen (1) in post-cast"
    );
    player.data().skill_char.last_cast_ms = now_ms() - 5001;
    super::regen::active_region_tick(&server);
    let p = player.data();
    assert!(!p.skill_char.is_in_post_cast, "the window ended after 5 s");
    assert_eq!(p.unit.mp, 102, "normal ManaRegen (25) + persistent (1)");
}

/// A channel refreshes `LastCast` when it ends (`FUN_39397ff0`), not only when it starts, unless
/// the skill has `keep_mana_regen`. The channel is ended directly (no timer, no sleep): the
/// window is first reset to "long ago", so only the channel end can set it again.
#[tokio::test]
async fn channel_refreshes_the_window_at_its_end() {
    for (keep_mana_regen, refreshed) in [(false, true), (true, false)] {
        let (server, _client, player, _npc) = fixture(GameData::empty());
        let skill = SkillRun::new(
            Arc::new(SkillTemplate {
                id: 9010,
                ability_id: 1,
                timing_id: 1,
                channeling_time: 50,
                keep_mana_regen,
                ..SkillTemplate::default()
            }),
            1,
        );
        let caster = WorldObject::from(&player);
        let (r, _) = use_skill(
            &server,
            &skill,
            &caster,
            SkillCaster::Unit(player.obj_id),
            &mut SkillCastTarget::Unit(NPC_OBJ),
            Some(SkillObject::None),
            false,
        );
        assert_eq!(r, skill_result::SUCCESS);
        {
            let mut c = player.data();
            c.skill_char.last_cast_ms = 1;
            c.skill_char.is_in_post_cast = false;
        }
        end_channeling(
            &server,
            &skill,
            &caster,
            0,
            &SkillCaster::Unit(player.obj_id),
            true,
        );
        let p = player.data();
        assert_eq!(
            p.skill_char.is_in_post_cast, refreshed,
            "keep_mana_regen {keep_mana_regen}: the channel end starts the window"
        );
        assert_eq!(
            p.skill_char.last_cast_ms > 1,
            refreshed,
            "keep_mana_regen {keep_mana_regen}: the channel end refreshes LastCast"
        );
    }
}

/// `ReduceCurrentMp` sends no `SCUnitPoints` when the change is zero: the client predicts
/// its own regen between packets (`FUN_390b63f0`), and a packet for an unchanged value snaps
/// the bar back to the server's older value.
#[tokio::test]
async fn reduce_current_mp_sends_no_packet_for_a_zero_change() {
    let (server, mut client, player, _npc) = fixture(GameData::empty());
    client.drain();
    let caster = WorldObject::from(&player);
    super::combat::reduce_current_mp(&server, &caster, 0);
    assert!(client.drain_ops().is_empty(), "no packet for a 0 change");
    player.data().unit.mp = 0;
    super::combat::reduce_current_mp(&server, &caster, 7);
    assert!(client.drain_ops().is_empty(), "no packet when Mp stays 0");
    player.data().unit.mp = 10;
    super::combat::reduce_current_mp(&server, &caster, 7);
    assert!(client.drain_ops().contains(&sc::SCUnitPointsPacket));
    assert_eq!(player.data().unit.mp, 3);
}
