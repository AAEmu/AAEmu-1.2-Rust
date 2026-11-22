//! Per-session chat diagnostics for nearby skill casts.

use super::prelude::*;

pub struct SkillDebug;

impl Command for SkillDebug {
    fn command_names(&self) -> Vec<String> {
        names(&["skilldebug"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if !args.is_empty() {
            CommandManager::send_error_text(self, out, "Use /skilldebug without arguments.");
            return Ok(());
        }

        let debug = &ctx.server()?.systems.skills.debug_casts;
        let enabled = debug.toggle(&ctx.player()?);
        let state = if enabled { "on" } else { "off" };
        CommandManager::send_normal_text(
            self,
            out,
            &format!("Nearby skill cast debug is {state}."),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        String::new()
    }

    fn get_command_help_text(&self) -> String {
        "Toggle private chat diagnostics for skill casts within 100 meters.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{add_xp::tests::spawn_npc, harness};
    use super::*;
    use crate::systems::skills::{
        data::SkillTemplate,
        skill,
        state::SkillRun,
        types::{skill_result, SkillCastTarget, SkillCaster},
    };

    #[tokio::test(start_paused = true)]
    async fn skilldebug_shows_accepted_casts_only_to_nearby_subscribers() {
        let server = harness::server();
        let mut observer = harness::pc(&server, 1, "Observer", 100.0, 100.0);
        let mut nearby = harness::pc(&server, 2, "Nearby", 110.0, 100.0);
        let mut distant = harness::pc(&server, 3, "Distant", 211.0, 100.0);
        let mut other_instance = harness::pc(&server, 4, "Other", 100.0, 100.0);
        let world = server.world.world(0).unwrap();
        let other_world = server
            .world
            .create_world_instance(world.template.clone(), 1);
        let other = WorldObject::from(&other_instance.p);
        world.transfer_out(&other);
        other_instance.p.data().unit.transform.instance_id = 1;
        other_world.spawn(&other, &server.view_ctx());

        let npc = spawn_npc(&server, 0x500, 10, 105.0, 100.0, 1_000);
        let npc = WorldObject::from(&npc);
        let cast = |caster: &WorldObject, id, casting_time| {
            let skill = SkillRun::new(
                Arc::new(SkillTemplate {
                    id,
                    casting_time,
                    level_rule_no_consideration: true,
                    ignore_global_cooldown: true,
                    ..SkillTemplate::default()
                }),
                1,
            );
            let (result, _) = skill::use_skill(
                &server,
                &skill,
                caster,
                SkillCaster::Unit(caster.obj_id()),
                &mut SkillCastTarget::Unit(caster.obj_id()),
                None,
                true,
            );
            assert_eq!(result, skill_result::SUCCESS);
        };

        cast(&npc, 501, 0);
        assert!(observer.chat().is_empty());

        let (handled, messages) = harness::run_at(&server, &nearby.p, "skilldebug", 0).await;
        assert!(handled);
        assert!(messages[0].contains("Insufficient privileges"));

        for player in [&observer.p, &distant.p, &other_instance.p] {
            let (_, messages) = harness::run(&server, player, "skilldebug").await;
            assert_eq!(messages, ["[skilldebug] Nearby skill cast debug is on."]);
        }
        observer.chat();
        nearby.chat();
        distant.chat();
        other_instance.chat();

        cast(&npc, 502, 750);
        let lines = observer.chat();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("[SkillDebug] Skill 502 (TL "));
        assert!(lines[0].contains(", cast 750 ms)"));
        assert!(lines[0].contains("Npc10 (Npc 1280, template 10) -> Npc10"));
        assert!(nearby.chat().is_empty());
        assert!(distant.chat().is_empty());
        assert!(other_instance.chat().is_empty());

        let me = WorldObject::from(&observer.p);
        cast(&me, 503, 0);
        let lines = observer.chat();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("[SkillDebug] Skill 503 (TL "));
        assert!(lines[0].contains(", cast 0 ms) Observer"));

        let (_, messages) = harness::run(&server, &observer.p, "skilldebug").await;
        assert_eq!(messages, ["[skilldebug] Nearby skill cast debug is off."]);
        observer.chat();
        cast(&me, 504, 0);
        assert!(observer.chat().is_empty());
    }
}
