use super::*;
use std::time::Duration;

#[tokio::test]
async fn stale_auto_attack_callback_cannot_clear_a_replacements_slot() {
    for current_callback in [false, true] {
        let (server, _client, player, npc) =
            super::super::cast_validation_tests::fixture(crate::data::GameData::empty());
        let skill = SkillRun::new(Arc::new(super::super::tests::skill_template(1)), 1);
        let current = CancelToken::new();
        let scheduled = if current_callback {
            current.clone()
        } else {
            CancelToken::new()
        };
        npc.data().unit.hp = 0;
        {
            let mut p = player.data();
            p.unit.current_target = npc.obj_id;
            p.unit.is_auto_attack = true;
            p.unit.skill_state.auto_attack = Some(AutoAttackSlot {
                skill: skill.clone(),
                cancel: current.clone(),
            });
        }
        schedule_auto_attack(&server, player.obj_id, skill, scheduled.clone(), 10);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(scheduled.is_cancelled());
        assert_eq!(current.is_cancelled(), current_callback);
        assert_eq!(
            player.data().unit.skill_state.auto_attack.is_some(),
            !current_callback
        );
        assert_eq!(player.data().unit.is_auto_attack, !current_callback);
    }
}

#[tokio::test]
async fn buff_removal_autoattack_triggers_when_a_basic_attack_fires() {
    let mut data = crate::data::GameData::empty();
    data.systems
        .skills
        .add_buff(crate::systems::skills::data::BuffTemplate {
            id: 656,
            kind: 3,
            remove_on_auto_attack: true,
            ..Default::default()
        });
    let (server, _client, player, npc) = super::super::cast_validation_tests::fixture(data);
    let caster = WorldObject::from(&player);
    super::buffs::add_buff_by_id(&server, &caster, 656, &caster, 0);
    let mut target = SkillCastTarget::Unit(npc.obj_id);
    let skill = SkillRun::new(Arc::new(super::super::tests::skill_template(2)), 1);
    assert_eq!(
        use_skill(
            &server,
            &skill,
            &caster,
            SkillCaster::Unit(caster.obj_id()),
            &mut target,
            None,
            false,
        )
        .0,
        skill_result::SUCCESS
    );
    assert!(!player.data().unit.buffs.check_buff(656));
}
