//! `Scripts/Commands/Despawn.cs`.

use super::prelude::*;
use crate::systems::npcs::spawn_manager;

/// `Despawn`.
pub struct Despawn;

impl Despawn {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

/// `ObjectIdManager.Instance.ReleaseId(obj.ObjId); obj.Delete();`
pub(crate) fn release_and_delete(server: &GameServer, obj: &WorldObject) {
    if let WorldObject::Npc(npc) = obj {
        // Deviation: an NPC that still has a queued respawn / corpse timer (a dead spawner NPC)
        // keeps its object id until the spawn manager's timers are done with it. Releasing it
        // here would let a new object reuse the id while the timers (and the spawner's
        // `_scheduledCount` credit behind the respawn) are keyed by it; the respawn then still
        // happens and the count stays backed. Serialized with the timer tick.
        let _serial = server.systems.npcs.spawns.spawn_lock();
        let timers_own_id = spawn_manager::manager_of(server, obj, None)
            .is_some_and(|m| m.has_pending_timers(npc.obj_id));
        spawn_manager::delete_object(server, obj, None);
        if !timers_own_id {
            server.ids.object.release_id(npc.obj_id);
        }
        return;
    }
    server.ids.object.release_id(obj.obj_id());
    spawn_manager::delete_object(server, obj, None);
}

impl Command for Despawn {
    fn command_names(&self) -> Vec<String> {
        names(&["despawn"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if args.len() < 2 {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        }

        let action = args[0].as_str();
        let mut radius = 30f32;
        let use_radius = args.len() >= 3;
        let Some(unit_id) = try_parse::<u32>(&args[1]) else {
            if use_radius {
                CommandManager::send_error_text(self, out, "<templateId> parse error");
            } else {
                CommandManager::send_error_text(self, out, "<objId> parse error");
            }
            return Ok(());
        };

        if args.len() >= 3 {
            match parse_f32(&args[2]) {
                Some(r) => radius = r,
                None => {
                    CommandManager::send_error_text(self, out, "<radius> parse error");
                    return Ok(());
                }
            }
        }

        let server = ctx.server()?;
        let character = ctx.player()?;
        if !use_radius {
            let world = parent_world(server, &character).ok_or_else(nre)?;
            match action {
                "doodad" => match world.doodad(unit_id) {
                    Some(my_doodad) => {
                        let template_id = my_doodad.data().template_id;
                        CommandManager::send_normal_text(
                            self,
                            out,
                            &format!(
                                "Removing Doodad with ID {} - @DOODAD_NAME({template_id})",
                                my_doodad.obj_id
                            ),
                        );
                        release_and_delete(server, &WorldObject::from(&my_doodad));
                    }
                    None => CommandManager::send_error_text(
                        self,
                        out,
                        &format!("Doodad with Id {unit_id} does not exist"),
                    ),
                },
                "npc" => match world.npc(unit_id) {
                    Some(my_npc) => {
                        let template_id = my_npc.data().unit.template_id;
                        CommandManager::send_normal_text(
                            self,
                            out,
                            &format!(
                                "Removing NPC with ID {} - @NPC_NAME({template_id})",
                                my_npc.obj_id
                            ),
                        );
                        release_and_delete(server, &WorldObject::from(&my_npc));
                    }
                    None => CommandManager::send_error_text(
                        self,
                        out,
                        &format!("NPC with objectId {unit_id} does not exist"),
                    ),
                },
                "unit" => match world.object(unit_id) {
                    Some(my_unit) => {
                        CommandManager::send_normal_text(
                            self,
                            out,
                            &format!("Removing Transfer with ID {}", my_unit.obj_id()),
                        );
                        release_and_delete(server, &my_unit);
                    }
                    None => CommandManager::send_error_text(
                        self,
                        out,
                        &format!("Unit with objectId {unit_id} does not exist"),
                    ),
                },
                _ => {
                    CommandManager::send_error_text(self, out, &format!("Unknown action {action}"))
                }
            }
        } else {
            let mut removed_count = 0;
            // Use radius
            match action {
                "doodad" => {
                    let my_doodads =
                        around_of(server, &character, radius, |o| o.as_doodad().cloned());
                    for doodad in &my_doodads {
                        if doodad.data().template_id == unit_id {
                            release_and_delete(server, &WorldObject::from(doodad));
                            removed_count += 1;
                        }
                    }
                    CommandManager::send_normal_text(
                        self,
                        out,
                        &format!(
                            "Removed {removed_count} Doodad(s) with TemplateID {unit_id} - @DOODAD_NAME({unit_id})"
                        ),
                    );
                }
                "npc" => {
                    let my_npcs = around_of(server, &character, radius, |o| o.as_npc().cloned());
                    for npc in &my_npcs {
                        if npc.data().unit.template_id == unit_id {
                            release_and_delete(server, &WorldObject::from(npc));
                            removed_count += 1;
                        }
                    }
                    CommandManager::send_normal_text(
                        self,
                        out,
                        &format!(
                            "Removed {removed_count} NPC(s) with TemplateID {unit_id} - @NPC_NAME({unit_id})"
                        ),
                    );
                }
                _ => {
                    CommandManager::send_error_text(self, out, &format!("Unknown action {action}"))
                }
            }
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<npc || doodad> < <objId> || <templateId> <radius> >".into()
    }

    fn get_command_help_text(&self) -> String {
        "Despawns a npc or doodad using by <objId> or <templateId> inside a <radius> range.".into()
    }
}

#[cfg(test)]
mod tests {
    use super::super::harness;
    use super::super::util::objects_test_support as ts;

    /// From PR #3: an NPC removed by the GM command leaves no loot behind for the next NPC that
    /// gets its object id (the loot container is dropped when the id is released).
    #[tokio::test]
    async fn despawn_npc_retires_loot_before_object_id_reuse() {
        use crate::systems::economy::looting::LootManager;
        use crate::world::WorldObject;
        use std::sync::Arc;

        for by_radius in [false, true] {
            let server = ts::server();
            let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
            let world = server.world.world(0).unwrap();
            let npc = ts::spawn_npc(&server, ts::NPC, 110.0, 100.0);
            LootManager::generate_loot(
                &server,
                &WorldObject::from(&npc),
                Some(&WorldObject::from(&gm.p)),
            );
            let old_loot = server.systems.economy.loot.get(npc.obj_id).unwrap();
            assert!(old_loot.lock().unwrap().already_generated);
            LootManager::open_bag(&server, &gm.p, npc.obj_id, false);
            let command = if by_radius {
                format!("despawn npc {} 20", ts::NPC)
            } else {
                format!("despawn npc {}", npc.obj_id)
            };
            let (handled, _) = harness::run(&server, &gm.p, &command).await;
            assert!(handled);
            assert!(world.npc(npc.obj_id).is_none());
            assert!(server.systems.economy.loot.get(npc.obj_id).is_none());
            let replacement = ts::spawn_npc(&server, ts::NPC, 110.0, 100.0);
            assert_eq!(replacement.obj_id, npc.obj_id);
            LootManager::generate_loot(
                &server,
                &WorldObject::from(&replacement),
                Some(&WorldObject::from(&gm.p)),
            );
            let new_loot = server.systems.economy.loot.get(replacement.obj_id).unwrap();
            assert!(!Arc::ptr_eq(&old_loot, &new_loot));
            assert!(new_loot.lock().unwrap().opened_by.is_empty());
        }
    }

    /// An NPC with a queued corpse / respawn timer keeps its object id until the spawn manager
    /// is done with it, so the timers cannot hit a new object that reused the id.
    #[tokio::test]
    async fn despawn_npc_with_pending_timers_keeps_its_object_id_until_they_run() {
        use crate::systems::npcs::spawn_manager;
        use crate::world::WorldObject;
        use std::time::Duration;

        let server = ts::server();
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let world = server.world.world(0).unwrap();
        let manager = server.systems.npcs.spawns.get_or_create(&world);
        let npc = ts::spawn_npc(&server, ts::NPC, 110.0, 100.0);
        manager.add_despawn(
            &server,
            WorldObject::from(&npc),
            Some(spawn_manager::add_secs(spawn_manager::now(), 5.0)),
        );
        let (handled, _) =
            harness::run(&server, &gm.p, &format!("despawn npc {}", npc.obj_id)).await;
        assert!(handled);
        assert!(world.npc(npc.obj_id).is_none());
        assert_eq!(manager.pending_despawns().len(), 1);
        // The id is still reserved: a new object does not get it.
        let other = ts::spawn_npc(&server, ts::NPC, 110.0, 100.0);
        assert_ne!(other.obj_id, npc.obj_id);
        // The timer runs against the missing object, then releases the id.
        spawn_manager::advance_test_clock(Duration::from_secs(6));
        manager.check_respawns_tick(&server);
        assert!(manager.pending_despawns().is_empty());
        assert!(
            world.npc(other.obj_id).is_some(),
            "the other NPC is untouched"
        );
        let again = ts::spawn_npc(&server, ts::NPC, 110.0, 100.0);
        assert_eq!(again.obj_id, npc.obj_id);
    }

    #[tokio::test]
    async fn despawn_by_id_and_radius() {
        let server = ts::server();
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let world = server.world.world(0).unwrap();
        let npc = ts::spawn_npc(&server, ts::NPC, 110.0, 100.0);
        let d1 = ts::spawn_doodad(&server, ts::DOODAD, 102.0, 100.0);
        let d2 = ts::spawn_doodad(&server, ts::DOODAD, 103.0, 100.0);

        let (_, out) = harness::run(&server, &gm.p, &format!("despawn npc {}", npc.obj_id)).await;
        assert_eq!(
            out,
            [harness::normal(
                "despawn",
                &format!(
                    "Removing NPC with ID {} - @NPC_NAME({})",
                    npc.obj_id,
                    ts::NPC
                )
            )]
        );
        assert!(world.npc(npc.obj_id).is_none());

        let (_, out) =
            harness::run(&server, &gm.p, &format!("despawn doodad {} 10", ts::DOODAD)).await;
        assert_eq!(
            out,
            [harness::normal(
                "despawn",
                &format!(
                    "Removed 2 Doodad(s) with TemplateID {} - @DOODAD_NAME({})",
                    ts::DOODAD,
                    ts::DOODAD
                )
            )]
        );
        assert!(world.doodad(d1.obj_id).is_none() && world.doodad(d2.obj_id).is_none());

        let (_, out) = harness::run(&server, &gm.p, "despawn npc 5").await;
        assert_eq!(
            out,
            [harness::error(
                "despawn",
                "NPC with objectId 5 does not exist"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "despawn npc x 3").await;
        assert_eq!(out, [harness::error("despawn", "<templateId> parse error")]);
        let (_, out) = harness::run(&server, &gm.p, "despawn npc 1 r").await;
        assert_eq!(out, [harness::error("despawn", "<radius> parse error")]);
        let (_, out) = harness::run(&server, &gm.p, "despawn foo 1").await;
        assert_eq!(out, [harness::error("despawn", "Unknown action foo")]);
    }
}
