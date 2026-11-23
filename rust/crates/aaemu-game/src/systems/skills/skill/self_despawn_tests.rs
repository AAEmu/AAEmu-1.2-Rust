use super::*;
use crate::data::world_data::SystemFaction;
use crate::models::npc::Npc;
use crate::opcodes::sc;
use crate::systems::skills::data::effects::KillNpcWithoutCorpseEffect;
use crate::systems::skills::tests::{damage_effect, skill_effect, skill_template};
use crate::test_support::{spawn_player, test_character, TestClient};
use crate::world::Entity;

#[tokio::test]
async fn self_destruct_removes_only_its_caster_when_summons_overlap() {
    let mut data = crate::data::GameData::empty();
    data.factions.add(SystemFaction {
        id: 115,
        ..Default::default()
    });
    let skills = &mut data.systems.skills;
    skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(damage_effect(2728, 10))),
        15714,
    );
    skills.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(damage_effect(2729, 1))),
        15717,
    );
    skills.add_effect(
        EffectKind::KillNpcWithoutCorpseEffect,
        EffectTemplate::KillNpcWithoutCorpse(KillNpcWithoutCorpseEffect {
            id: 647,
            npc_id: 986,
            radius: 0.0,
            give_exp: false,
            vanish: true,
        }),
        0,
    );
    let mut self_damage = skill_effect(15717, EffectKind::DamageEffect, 2729);
    self_damage.application_method = ApplicationMethod::SourceOnce as u32;
    self_damage.friendly = true;
    let mut template = skill_template(16_444);
    template.target_type = 0;
    template.target_selection = SkillTargetSelection::Source as i32;
    template.target_area_radius = 5;
    let mut blast = skill_effect(15714, EffectKind::DamageEffect, 2728);
    blast.non_friendly = true;
    template.effects.push(blast);
    template.effects.push(self_damage);
    let skill = SkillRun::new(Arc::new(template), 1);
    let server = crate::test_support::test_server_with(data);
    crate::systems::npcs::install_hooks(&server);
    let world = server.world.world(0).expect("test world");
    let mut client = TestClient::new(&server, 1, 1001);
    let mut character = test_character(1, "Observer", 101.0, 100.0);
    character.unit.hp = 100;
    character.unit.max_hp = 100;
    let victim = spawn_player(&server, &client, character, 600);
    let first = WorldObject::from(Entity::new(601, Npc::new(986)));
    let second = WorldObject::from(Entity::new(602, Npc::new(986)));
    for caster in [&first, &second] {
        {
            let mut npc = caster.as_npc().unwrap().data();
            npc.unit.hp = 100;
            npc.unit.faction_id = 115;
            npc.unit.transform.set_xyz(100.0, 100.0, 100.0);
        }
        world.spawn(caster, &server.view_ctx());
    }
    for caster in [&first, &second] {
        assert_eq!(
            nearest_kill_npc(&server, caster, 986, 0.0).unwrap().obj_id,
            caster.obj_id()
        );
    }
    client.drain();
    apply_effects(
        &server,
        &skill,
        &CastCtx {
            caster: second.clone(),
            caster_caster: SkillCaster::Unit(second.obj_id()),
            target: Target::Obj(second.clone()),
            target_caster: SkillCastTarget::Unit(second.obj_id()),
            skill_object: SkillObject::None,
        },
    );
    assert!(world.object(first.obj_id()).is_some());
    assert!(world.object(second.obj_id()).is_none());
    assert!(victim.data().unit.hp < 100);
    let [lo, hi] = sc::SCUnitDamagedPacket.to_le_bytes();
    let hit = [0, 0, lo, hi];
    assert!(client.drain().into_iter().any(|(opcode, body)| {
        opcode == 0x8001
            && super::super::packets::inflate_stored(&body)
                .is_some_and(|payload| payload.starts_with(&hit))
    }));
}
