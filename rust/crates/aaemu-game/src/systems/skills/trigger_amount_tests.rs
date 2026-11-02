//! `buff_triggers.use_damage_amount`: the fixed heal / mana values are per-mille of the event's
//! damage amount (GAP 1), and the `Damage` event (kind 3, damage dealt by the owner) passes the
//! amount too (GAP 2).
//!
//! Real 1.2 rows covered here:
//! * 753 "Converts 3% of received damage to mana": event 4, RestoreManaEffect 20 fixed 30.
//! * 806 Baleful Recharge: event 3, RestoreManaEffect 22 fixed 20 ("of the damage inflicted").
//! * 4257 Executor's Cruelty: event 3, HealEffect 337 fixed 30.

use std::sync::Arc;

use crate::data::GameData;
use crate::models::npc::Npc;
use crate::server::GameServer;
use crate::test_support::{spawn_player, test_character, test_server_with};
use crate::world::{Entity, NpcRef, PlayerRef, WorldObject};

use super::data::effects::{HealEffect, RestoreManaEffect};
use super::data::{BuffTemplate, BuffTriggerTemplate, EffectKind, EffectTemplate};
use super::effects::{EffectArgs, EffectSource};
use super::geometry::Target;
use super::types::{BuffEventTriggerKind, CastAction, SkillCastTarget, SkillCaster, SkillObject};

const PLAYER: u32 = 0x100;
const NPC: u32 = 0x200;

fn trigger(
    id: u32,
    kind: BuffEventTriggerKind,
    effect: (EffectKind, u32),
    use_damage_amount: bool,
) -> BuffTriggerTemplate {
    BuffTriggerTemplate {
        id,
        kind: kind as u16,
        effect_on_source: kind == BuffEventTriggerKind::Damage,
        effect,
        use_damage_amount,
        use_original_source: false,
        target_buff_tag_id: 0,
        target_no_buff_tag_id: 0,
        synergy: false,
    }
}

fn data() -> GameData {
    let mut data = GameData::empty();
    // Starting a buff refreshes the cached pools from the character formulas.
    data.formulas = crate::systems::vehicles::stats::tests::formulas(
        &[(1, "1000", 14, 0), (2, "1000", 15, 0)],
        &[],
    );
    for id in [753, 806, 4257] {
        data.systems.skills.add_buff(BuffTemplate {
            id,
            kind: 1,
            duration: 60_000,
            stack_rule: 1,
            ..BuffTemplate::default()
        });
    }
    data.systems.skills.add_effect(
        EffectKind::RestoreManaEffect,
        EffectTemplate::RestoreMana(RestoreManaEffect {
            id: 20,
            use_fixed_value: true,
            fixed_min: 30,
            fixed_max: 30,
            use_level_value: false,
            level_md: 0.0,
            level_va_start: 0,
            level_va_end: 0,
            percent: true,
        }),
        0,
    );
    data.systems.skills.add_effect(
        EffectKind::RestoreManaEffect,
        EffectTemplate::RestoreMana(RestoreManaEffect {
            id: 22,
            use_fixed_value: true,
            fixed_min: 20,
            fixed_max: 20,
            use_level_value: false,
            level_md: 0.0,
            level_va_start: 0,
            level_va_end: 0,
            percent: true,
        }),
        0,
    );
    data.systems.skills.add_effect(
        EffectKind::HealEffect,
        EffectTemplate::Heal(HealEffect {
            id: 337,
            use_fixed_heal: true,
            fixed_min: 30,
            fixed_max: 30,
            use_level_heal: false,
            level_md: 0.0,
            level_va_start: 0,
            level_va_end: 0,
            percent: true,
            use_charged_buff: false,
            charged_buff_id: 0,
            charged_mul: 0.0,
            slave_applicable: false,
            ignore_heal_aggro: true,
            dps_multiplier: 0.0,
            actability_group_id: 0,
            actability_step: 0,
            actability_mul: 0.0,
            actability_add: 0.0,
        }),
        0,
    );
    data.systems.skills.buff_triggers.insert(
        753,
        vec![trigger(
            220,
            BuffEventTriggerKind::Damaged,
            (EffectKind::RestoreManaEffect, 20),
            true,
        )],
    );
    data.systems.skills.buff_triggers.insert(
        806,
        vec![trigger(
            344,
            BuffEventTriggerKind::Damage,
            (EffectKind::RestoreManaEffect, 22),
            true,
        )],
    );
    data.systems.skills.buff_triggers.insert(
        4257,
        vec![trigger(
            2490,
            BuffEventTriggerKind::Damage,
            (EffectKind::HealEffect, 337),
            true,
        )],
    );
    data
}

fn fixture() -> (Arc<GameServer>, PlayerRef, NpcRef) {
    let server = Arc::new(test_server_with(data()));
    super::bind(&server);
    let mut client = crate::test_support::TestClient::new(&server, 1, 1);
    let mut character = test_character(1, "Caster", 100.0, 100.0);
    character.unit.hp = 900;
    character.unit.max_hp = 1000;
    character.unit.mp = 0;
    character.unit.max_mp = 1000;
    let player = spawn_player(&server, &client, character, PLAYER);
    client.conn.active_char = Some(player.clone());
    let mut npc = Npc::new(1);
    npc.unit.hp = 1000;
    npc.unit.max_hp = 1000;
    npc.unit.transform.set_xyz(105.0, 100.0, 100.0);
    let npc = Entity::new(NPC, npc);
    server
        .world
        .world(0)
        .expect("world")
        .spawn(&npc, &server.view_ctx());
    (server, player, npc)
}

fn args(caster: &WorldObject, target: &WorldObject) -> EffectArgs<'static> {
    EffectArgs {
        caster: Some(caster.clone()),
        caster_caster: SkillCaster::Unit(caster.obj_id()),
        target: Target::Obj(target.clone()),
        target_caster: SkillCastTarget::Unit(target.obj_id()),
        cast: CastAction::Skill {
            skill_id: 1,
            tl_id: 1,
        },
        source: EffectSource::default(),
        skill_object: &SkillObject::None,
    }
}

fn hit(server: &GameServer, caster: &WorldObject, target: &WorldObject, amount: i32) {
    super::effects::combat::damage(
        server,
        &super::tests::damage_effect(1, amount),
        &args(caster, target),
        None,
    );
}

#[tokio::test]
async fn damage_event_and_damaged_event_triggers_use_the_per_mille_amount() {
    let (server, player, npc) = fixture();
    let player_obj = WorldObject::from(&player);
    let npc_obj = WorldObject::from(&npc);
    // 753 is the victim's Damaged trigger; 806/4257 are the attacker's Damage triggers.
    super::buffs::add_buff_by_id(&server, &player_obj, 753, &player_obj, 0);
    super::buffs::add_buff_by_id(&server, &player_obj, 806, &player_obj, 0);
    super::buffs::add_buff_by_id(&server, &player_obj, 4257, &player_obj, 0);

    // The player deals 200: Damage event -> heal 30/1000*200 = 6, mana 20/1000*200 = 4.
    hit(&server, &player_obj, &npc_obj, 200);
    assert_eq!(npc.data().unit.hp, 800);
    assert_eq!(player.data().unit.hp, 906);
    assert_eq!(player.data().unit.mp, 4);

    // The NPC deals 200 back: Damaged event 753 -> mana 30/1000*200 = 6.
    hit(&server, &npc_obj, &player_obj, 200);
    assert_eq!(player.data().unit.hp, 706);
    assert_eq!(player.data().unit.mp, 10);
}
