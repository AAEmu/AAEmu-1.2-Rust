//! `stackednpcs` (Rust only, server owner's request 2026-11-08): lists NPCs standing on top of
//! each other and separates them.
//!
//! * `stackednpcs [radius]`: the stacks within `radius` m of the player (default 100).
//! * `stackednpcs all`: world-wide counts (and the first few stacks).
//! * `stackednpcs fix [radius|all]`: keeps the first NPC of every stack in range and moves the
//!   others with the spawn scatter logic (`systems::npcs::stacked_npcs`).

use super::prelude::*;
use crate::systems::npcs::stacked_npcs::{
    describe_stack, find_stacks, separate_npc, ScatterParams,
};

/// Default list / fix radius (m).
const DEFAULT_RADIUS: f32 = 100.0;
/// Stacks listed per command before the output is cut.
const MAX_LISTED: usize = 10;

/// `StackedNpcs`.
pub struct StackedNpcs;

impl StackedNpcs {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for StackedNpcs {
    fn command_names(&self) -> Vec<String> {
        names(&["stackednpcs", "npcstacks"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        let server = ctx.server()?;
        let character = ctx.player()?;
        let fix = args.first().is_some_and(|a| a.eq_ignore_ascii_case("fix"));
        let range_arg = if fix { args.get(1) } else { args.first() };
        // `None` = the whole world.
        let radius = match range_arg {
            None => Some(DEFAULT_RADIUS),
            Some(a) if a.eq_ignore_ascii_case("all") => None,
            Some(a) => match parse_f32(a) {
                Some(r) if r > 0.0 => Some(r),
                _ => {
                    CommandManager::send_error_text(self, out, "Error parsing radius !");
                    return Ok(());
                }
            },
        };
        let world = parent_world(server, &character).ok_or_else(nre)?;
        let me = world_pos(server, &WorldObject::from(&character));
        let cfg = &server.config.world;
        let stacks: Vec<_> = find_stacks(&world, cfg.stacked_npc_radius)
            .into_iter()
            .filter(|s| {
                radius.is_none_or(|r| {
                    s.iter().any(|m| {
                        let (dx, dy, dz) = (m.pos.0 - me.x, m.pos.1 - me.y, m.pos.2 - me.z);
                        (dx * dx + dy * dy + dz * dz).sqrt() <= r
                    })
                })
            })
            .collect();
        let scope = radius.map_or_else(|| "the world".to_string(), |r| format!("{r} m"));

        if fix {
            let params = ScatterParams::for_fix(cfg, cfg.stacked_npc_radius);
            let (mut moved, mut kept) = (0, 0);
            for stack in &stacks {
                // The first member stays; the others are separated one by one (a moved NPC is a
                // neighbour of the next).
                for m in stack.iter().skip(1) {
                    match separate_npc(server, &world, &m.npc, params) {
                        Some(_) => moved += 1,
                        None => kept += 1,
                    }
                }
            }
            CommandManager::send_normal_text(
                self,
                out,
                &format!(
                    "Separated {moved} NPC(s) of {} stack(s) within {scope} ({kept} could not be moved)",
                    stacks.len()
                ),
            );
            return Ok(());
        }

        let npc_count: usize = stacks.iter().map(Vec::len).sum();
        let mut shared = 0;
        for (i, stack) in stacks.iter().enumerate() {
            let (infos, same) = describe_stack(server, stack);
            if same {
                shared += 1;
            }
            if i >= MAX_LISTED {
                continue;
            }
            out.send_message(&format!(
                "#{} -> {} NPCs, {}",
                i + 1,
                infos.len(),
                if same {
                    "one placement"
                } else {
                    "different placements"
                }
            ));
            for m in &infos {
                out.send_message(&format!(
                    "    BcId: {} NpcTemplateId: {} - @NPC_NAME({}), SpawnerId: {} (template {}), at ({:.1}, {:.1}, {:.1})",
                    m.obj_id,
                    m.template_id,
                    m.template_id,
                    m.spawner_id,
                    m.spawner_template_id,
                    m.pos.0,
                    m.pos.1,
                    m.pos.2
                ));
            }
        }
        if stacks.len() > MAX_LISTED {
            out.send_message(&format!(
                "... and {} more stack(s)",
                stacks.len() - MAX_LISTED
            ));
        }
        CommandManager::send_normal_text(
            self,
            out,
            &format!(
                "{} stack(s), {npc_count} NPC(s) within {scope} ({shared} from one placement)",
                stacks.len()
            ),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "[<radius> || all] || fix [<radius> || all]".into()
    }

    fn get_command_help_text(&self) -> String {
        format!(
            "Lists NPCs standing on top of each other around you (default {DEFAULT_RADIUS} m) or              in the whole world, or separates them with 'fix'."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness;
    use crate::models::npc::Npc;
    use crate::server::GameServer;
    use crate::world::{Entity, NpcRef};

    fn put_npc(server: &GameServer, template_id: u32, x: f32, y: f32) -> NpcRef {
        let mut n = Npc::new(template_id);
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.transform.set_xyz(x, y, 100.0);
        // The harness gives players the fixed object ids 0x100 + character id.
        let mut id = server.ids.object.get_next_id().unwrap();
        while id < 0x200 {
            id = server.ids.object.get_next_id().unwrap();
        }
        let npc = Entity::new(id, n);
        let world = server.world.world(0).unwrap();
        crate::systems::npcs::npc::spawner::spawn_into_world(server, &world, &npc);
        npc
    }

    #[tokio::test]
    async fn lists_and_fixes_a_stack() {
        let server = harness::server();
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let a = put_npc(&server, 500, 100.0, 101.0);
        let b = put_npc(&server, 501, 100.0, 101.0);
        let c = put_npc(&server, 502, 100.0, 101.0);
        // A lone NPC and a stack far away.
        put_npc(&server, 503, 110.0, 100.0);
        let far_a = put_npc(&server, 504, 300.0, 300.0);
        put_npc(&server, 505, 300.0, 300.0);

        let (handled, out) = harness::run(&server, &gm.p, "stackednpcs").await;
        assert!(handled);
        let text = out.join("\n");
        assert!(text.contains("1 stack(s), 3 NPC(s) within 100 m"), "{text}");
        for npc in [&a, &b, &c] {
            assert!(text.contains(&format!("BcId: {}", npc.obj_id)), "{text}");
        }
        assert!(text.contains("NpcTemplateId: 501"), "{text}");
        assert!(text.contains("SpawnerId: 0"), "{text}");
        assert!(!text.contains(&format!("BcId: {}", far_a.obj_id)), "{text}");

        let (_, all) = harness::run(&server, &gm.p, "npcstacks all").await;
        assert!(
            all.join("\n")
                .contains("2 stack(s), 5 NPC(s) within the world"),
            "{all:?}"
        );
        let (_, bad) = harness::run(&server, &gm.p, "stackednpcs x").await;
        assert_eq!(
            bad,
            [harness::error("stackednpcs", "Error parsing radius !")]
        );

        let (_, fixed) = harness::run(&server, &gm.p, "stackednpcs fix").await;
        assert_eq!(
            fixed,
            [harness::normal(
                "stackednpcs",
                "Separated 2 NPC(s) of 1 stack(s) within 100 m (0 could not be moved)"
            )]
        );
        let pos = |n: &NpcRef| {
            let t = n.data().unit.transform;
            (t.x, t.y, t.z)
        };
        assert_eq!(pos(&a), (100.0, 101.0, 100.0), "the first NPC stays");
        let d = |p: (f32, f32, f32), q: (f32, f32, f32)| {
            ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2) + (p.2 - q.2).powi(2)).sqrt()
        };
        assert!(d(pos(&a), pos(&b)) >= 1.5);
        assert!(d(pos(&a), pos(&c)) >= 1.5);
        assert!(d(pos(&b), pos(&c)) >= 1.5);
        let (_, after) = harness::run(&server, &gm.p, "stackednpcs").await;
        assert!(
            after.join("\n").contains("0 stack(s), 0 NPC(s)"),
            "{after:?}"
        );
        // The far stack was out of range and is untouched.
        let (_, far) = harness::run(&server, &gm.p, "stackednpcs all").await;
        assert!(
            far.join("\n")
                .contains("1 stack(s), 2 NPC(s) within the world"),
            "{far:?}"
        );
    }
}
