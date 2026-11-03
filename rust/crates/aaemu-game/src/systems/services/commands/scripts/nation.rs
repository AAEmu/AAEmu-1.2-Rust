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
        "[info|create [force]|hostile <faction>|relation <faction> <state> <minutes> <next>|treaty <faction>]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Lists player nations, or creates one from your expedition as its king. The expedition must own a dominion; create force skips that check for tests. hostile declares a diplomacy target hostile; relation and treaty are test hooks for the expiry and mail flows.".into()
    }
}
