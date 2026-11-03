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
                _ => {
                    CommandManager::send_default_help_text(self, out);
                    Ok(())
                }
            }
        })
    }

    fn get_command_line_help(&self) -> String {
        "[info|create [force]]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Lists player nations, or creates one from your expedition as its king. The expedition must own a dominion; create force skips that check for tests.".into()
    }
}
