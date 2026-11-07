//! `Scripts/Commands/Nation.cs` (stage-1 testing hook): lists the runtime player nations and
//! creates one from the caller's expedition through the same independence service the 1.2
//! skill 18540 uses (`systems::social::nation`).
//!
//! The normal independence prerequisites still apply (the caller owns the expedition and its
//! expedition owns a dominion). `create force` skips only the dominion check, for test
//! fixtures that cannot seed a castle tower (PORTING.md, NATION-4).

use futures::future::BoxFuture;

use super::prelude::*;
use crate::systems::social::nation;

/// `Nation`.
pub struct Nation;

impl Nation {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for Nation {
    fn command_names(&self) -> Vec<String> {
        names(&["nation", "nations"])
    }

    fn execute(
        &self,
        _ctx: &CommandContext<'_>,
        _args: &[String],
        _out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        anyhow::bail!("Nation runs through execute_async")
    }

    fn execute_async<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
        args: &'a [String],
        out: &'a mut dyn MessageOutput,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let server = ctx.server()?;
            let action = args.first().map(String::as_str).unwrap_or("info");
            match action {
                "info" if args.len() == 1 => {
                    let nations = server.systems.social.nations.all();
                    if nations.is_empty() {
                        CommandManager::send_normal_text(self, out, "No nations.");
                        return Ok(());
                    }
                    for nation in nations {
                        CommandManager::send_normal_text(
                            self,
                            out,
                            &format!(
                                "Nation {} ({}): king {} ({}), expedition {}, {} relations",
                                nation.name,
                                nation.id,
                                nation.owner_name,
                                nation.owner_character_id,
                                nation.expedition_id,
                                nation.relations.len()
                            ),
                        );
                    }
                    Ok(())
                }
                "create" => {
                    let mut force = false;
                    for arg in &args[1..] {
                        if arg.eq_ignore_ascii_case("force") {
                            force = true;
                        } else {
                            CommandManager::send_default_help_text(self, out);
                            return Ok(());
                        }
                    }
                    let player = ctx.player()?;
                    let expedition_id = player.data().unit.expedition_id;
                    if expedition_id == 0 {
                        CommandManager::send_error_text(
                            self,
                            out,
                            "You must be in an expedition to declare independence",
                        );
                        return Ok(());
                    }
                    let Some(expedition) = server.expeditions.get(expedition_id) else {
                        CommandManager::send_error_text(
                            self,
                            out,
                            "You must be in an expedition to declare independence",
                        );
                        return Ok(());
                    };
                    let name = expedition.faction.name.clone();
                    match nation::declare_independence(
                        server,
                        &player,
                        expedition_id,
                        &name,
                        !force,
                    ) {
                        Ok(nation) => {
                            if let Err(error) = nation::save_creation(server, &nation).await {
                                tracing::error!(
                                    "Nation {} created in memory but not saved: {error}",
                                    nation.id
                                );
                                CommandManager::send_error_text(
                                    self,
                                    out,
                                    &format!(
                                        "Nation {} created (id {}) but not persisted: {error}",
                                        nation.name, nation.id
                                    ),
                                );
                            } else {
                                CommandManager::send_normal_text(
                                    self,
                                    out,
                                    &format!(
                                        "Nation {} created (id {}, expedition {})",
                                        nation.name, nation.id, nation.expedition_id
                                    ),
                                );
                            }
                            Ok(())
                        }
                        Err(code) => {
                            crate::systems::social::util::send_error(&player, code);
                            CommandManager::send_error_text(
                                self,
                                out,
                                &format!("Nation creation failed (error {code})"),
                            );
                            Ok(())
                        }
                    }
                }
                "hostile" if args.len() == 2 => {
                    let target: u32 = match args[1].parse() {
                        Ok(id) => id,
                        Err(_) => {
                            CommandManager::send_default_help_text(self, out);
                            return Ok(());
                        }
                    };
                    let player = ctx.player()?;
                    match nation::declare_hostile(server, &player, target) {
                        Ok(()) => CommandManager::send_normal_text(
                            self,
                            out,
                            &format!("Declared hostile to faction {target}"),
                        ),
                        Err(code) => {
                            crate::systems::social::util::send_error(&player, code);
                            CommandManager::send_error_text(
                                self,
                                out,
                                &format!("Hostile declaration failed (error {code})"),
                            );
                        }
                    }
                    Ok(())
                }
                // Test hook, not a 1.2 command: force a relation with an expiry so a live run
                // can observe the stage-3 expiry without waiting hours (NATION-17).
                "relation" if args.len() == 5 => {
                    let (Ok(target), Ok(state), Ok(minutes), Ok(next)) = (
                        args[1].parse::<u32>(),
                        args[2].parse::<u8>(),
                        args[3].parse::<i64>(),
                        args[4].parse::<u8>(),
                    ) else {
                        CommandManager::send_default_help_text(self, out);
                        return Ok(());
                    };
                    let player = ctx.player()?;
                    let Some(nation) = server.systems.social.nations.owned_by(player.character_id)
                    else {
                        CommandManager::send_error_text(self, out, "You are not a nation king");
                        return Ok(());
                    };
                    let expire_at = chrono::Utc::now().timestamp() + minutes * 60;
                    if !server
                        .systems
                        .social
                        .nations
                        .set_relation(nation.id, target, state, expire_at, next)
                    {
                        CommandManager::send_error_text(self, out, "Unknown nation");
                        return Ok(());
                    }
                    CommandManager::send_normal_text(
                        self,
                        out,
                        &format!(
                            "Relation {nation_id} <-> {target}: state {state}, expires in {minutes} min, next {next}",
                            nation_id = nation.id
                        ),
                    );
                    Ok(())
                }
                // Rename hook (RENAME-7): no 1.2 server packet opens the nation dialog; the
                // optional name runs the same service the dialog's CS packet reaches.
                "rename" => {
                    let player = ctx.player()?;
                    match nation::offer_nation_rename_ui(server, &player) {
                        Ok(()) => {}
                        Err(code) => {
                            crate::systems::social::util::send_error(&player, code);
                            CommandManager::send_error_text(
                                self,
                                out,
                                &format!("Cannot offer a nation rename (error {code})"),
                            );
                            return Ok(());
                        }
                    }
                    if args.len() == 1 {
                        CommandManager::send_normal_text(
                            self,
                            out,
                            "Nation rename dialog sent; enter the new name in the client.",
                        );
                        return Ok(());
                    }
                    let name = args[1..].join(" ");
                    let Some(nation) = server.systems.social.nations.owned_by(player.character_id)
                    else {
                        CommandManager::send_error_text(self, out, "You are not a nation king");
                        return Ok(());
                    };
                    nation::rename_nation(server, &player, nation.id, &name).await;
                    CommandManager::send_normal_text(
                        self,
                        out,
                        &format!("Nation rename requested: {name}"),
                    );
                    Ok(())
                }
                // National tax hook (RENAME-8): the client's CS 0x013 confirm path.
                "tax" if args.len() == 3 => {
                    let (Ok(zone), Ok(rate)) = (args[1].parse::<u16>(), args[2].parse::<u32>())
                    else {
                        CommandManager::send_default_help_text(self, out);
                        return Ok(());
                    };
                    let player = ctx.player()?;
                    crate::systems::social::dominion::update_national_tax_rate(
                        server, &player, zone, rate,
                    );
                    CommandManager::send_normal_text(
                        self,
                        out,
                        &format!("National tax rate {rate} requested for zone {zone}"),
                    );
                    Ok(())
                }
                // Test hook, not a 1.2 command: seed an in-memory-only dominion for the
                // caller's guild so the national tax / monument paths can be driven without a
                // castle tower. The placeholder tower identity would fail startup validation,
                // so the state is marked ephemeral and never reaches the `dominions` table.
                "dominion" if args.len() == 2 => {
                    let Ok(zone) = args[1].parse::<u16>() else {
                        CommandManager::send_default_help_text(self, out);
                        return Ok(());
                    };
                    let player = ctx.player()?;
                    let expedition_id = player.data().unit.expedition_id;
                    if expedition_id == 0 {
                        CommandManager::send_error_text(self, out, "You must be in an expedition");
                        return Ok(());
                    }
                    let instance_id = player.data().unit.transform.instance_id;
                    let tower = crate::systems::housing::dominion::state::TowerIdentity {
                        house_id: 500 + u32::from(zone),
                        instance_id,
                        guard_tower_setting_id: 6,
                    };
                    let now = chrono::Utc::now().timestamp();
                    match server.systems.housing.dominions.declare_unowned_with(
                        u32::from(zone),
                        tower,
                        expedition_id,
                        now,
                        || true,
                    ) {
                        Ok(state) => {
                            server
                                .systems
                                .housing
                                .dominions
                                .mark_ephemeral(u32::from(zone));
                            CommandManager::send_normal_text(
                                self,
                                out,
                                &format!(
                                    "Dominion {} is owned by expedition {} (in memory only)",
                                    state.zone_group_id, expedition_id
                                ),
                            )
                        }
                        Err(error) => CommandManager::send_error_text(
                            self,
                            out,
                            &format!("Dominion {zone} not seeded: {error}"),
                        ),
                    }
                    Ok(())
                }
                // Nation monument hook (RENAME-9): the placement skill trigger is
                // capture-pending; this runs the same service with the king at the point.
                "monument" if args.len() >= 2 => {
                    let player = ctx.player()?;
                    let Some(zone) = args[1].parse::<u16>().ok() else {
                        CommandManager::send_default_help_text(self, out);
                        return Ok(());
                    };
                    let action = args[0].as_str();
                    let result = match action {
                        "place" if args.len() == 2 => {
                            let object = crate::world::WorldObject::from(&player);
                            let Some(world) = server.world.world_of(&object) else {
                                CommandManager::send_error_text(
                                    self,
                                    out,
                                    "You are not in a world",
                                );
                                return Ok(());
                            };
                            let pos = world.world_pos_rot(&object);
                            nation::place_national_monument(
                                server, &player, zone, pos.x, pos.y, pos.z,
                            )
                            .await
                        }
                        "place" if args.len() == 5 => {
                            let (Ok(x), Ok(y), Ok(z)) = (
                                args[2].parse::<f32>(),
                                args[3].parse::<f32>(),
                                args[4].parse::<f32>(),
                            ) else {
                                CommandManager::send_default_help_text(self, out);
                                return Ok(());
                            };
                            nation::place_national_monument(server, &player, zone, x, y, z).await
                        }
                        "remove" if args.len() == 2 => {
                            nation::remove_national_monument(server, &player, zone).await
                        }
                        _ => {
                            CommandManager::send_default_help_text(self, out);
                            return Ok(());
                        }
                    };
                    match result {
                        Ok(()) => CommandManager::send_normal_text(
                            self,
                            out,
                            &format!("Nation monument {action} ok for zone {zone}"),
                        ),
                        Err(code) => {
                            crate::systems::social::util::send_error(&player, code);
                            CommandManager::send_error_text(
                                self,
                                out,
                                &format!("Nation monument {action} failed (error {code})"),
                            );
                        }
                    }
                    Ok(())
                }
                "treaty" if args.len() == 2 => {
                    let target: u32 = match args[1].parse() {
                        Ok(id) => id,
                        Err(_) => {
                            CommandManager::send_default_help_text(self, out);
                            return Ok(());
                        }
                    };
                    let player = ctx.player()?;
                    let Some(nation) = server.systems.social.nations.owned_by(player.character_id)
                    else {
                        CommandManager::send_error_text(self, out, "You are not a nation king");
                        return Ok(());
                    };
                    match nation::send_treaty_mail(server, nation.id, target) {
                        Some(mail_id) => CommandManager::send_normal_text(
                            self,
                            out,
                            &format!("Treaty request mail {mail_id} sent"),
                        ),
                        None => CommandManager::send_error_text(
                            self,
                            out,
                            "Treaty request mail not sent (unknown king name or target)",
                        ),
                    }
                    Ok(())
                }
                _ => {
                    CommandManager::send_default_help_text(self, out);
                    Ok(())
                }
            }
        })
    }

    fn get_command_line_help(&self) -> String {
        "[info|create [force]|hostile <faction>|relation <faction> <state> <minutes> <next>|treaty <faction>|rename [name]|tax <zone> <rate>|monument place <zone> [x y z]|monument remove <zone>|dominion <zone>]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Lists player nations, or creates one from your expedition as its king. The expedition must own a dominion; create force skips that check for tests. hostile declares a diplomacy target hostile; relation and treaty are test hooks for the expiry and mail flows. rename offers the nation rename dialog (or applies a name), tax sets the national tax rate, monument places or removes the territory statue at your position, and dominion seeds a test dominion for the caller's guild.".into()
    }
}
