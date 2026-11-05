//! Guild (expedition) rename controls. Not in the C# (it never sent the rename dialog).
//!
//! * `/allowguildrename <guild id | guild name>`: gives the guild the right to rename itself
//!   once. Its leader gets the client's rename dialog now (online) or at the next login.
//! * `/renameguild <guild id> <new name>`: renames the guild directly (`SCFactionRenamed` with
//!   `by_gm`), with the create rules (length, characters, block list, unique name).

use super::prelude::*;
use crate::systems::social::expedition;

/// Finds a live guild by id or (case-insensitive) name.
fn find_guild(server: &crate::server::GameServer, text: &str) -> Option<u32> {
    let all = server.expeditions.all();
    let live = |e: &&expedition::Expedition| !e.is_disbanded && !e.faction.name.starts_with("$deleted-");
    if let Some(id) = try_parse::<u32>(text) {
        if let Some(e) = all.iter().filter(live).find(|e| e.id() == id) {
            return Some(e.id());
        }
    }
    let folded = text.to_lowercase();
    all.iter()
        .filter(live)
        .find(|e| e.faction.name.to_lowercase() == folded)
        .map(|e| e.id())
}

/// `AllowGuildRename`.
pub struct AllowGuildRename;

impl AllowGuildRename {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for AllowGuildRename {
    fn command_names(&self) -> Vec<String> {
        names(&["allowguildrename", "allow_guild_rename"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if args.is_empty() {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        }
        let server = ctx.server()?;
        let text = args.join(" ");
        let Some(id) = find_guild(server, &text) else {
            CommandManager::send_error_text(self, out, &format!("Guild {text} not found"));
            return Ok(());
        };
        let Some(s) = server.self_arc() else {
            anyhow::bail!("the server handle is not registered");
        };
        tokio::spawn(async move {
            expedition::grant_rename(&s, id).await;
        });
        CommandManager::send_normal_text(
            self,
            out,
            &format!("Guild {id} may rename itself once; its leader gets the rename dialog"),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<guild id | guild name>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Lets a guild's leader rename the guild once".into()
    }
}

/// `RenameGuild`.
pub struct RenameGuild;

impl RenameGuild {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for RenameGuild {
    fn command_names(&self) -> Vec<String> {
        names(&["renameguild", "rename_guild"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        let (Some(first), true) = (args.first(), args.len() >= 2) else {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        };
        let server = ctx.server()?;
        let Some(id) = try_parse::<u32>(first).and_then(|id| find_guild(server, &id.to_string()))
        else {
            CommandManager::send_error_text(self, out, &format!("Guild {first} not found"));
            return Ok(());
        };
        let name = args[1..].join(" ");
        if let Err(code) = expedition::validate_name(server, &name, id) {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Name refused (error message {code})"),
            );
            return Ok(());
        }
        let Some(s) = server.self_arc() else {
            anyhow::bail!("the server handle is not registered");
        };
        let new_name = name.clone();
        tokio::spawn(async move {
            if let Err(code) = expedition::gm_rename(&s, id, &new_name).await {
                tracing::warn!("RenameGuild {id}: refused ({code})");
            }
        });
        CommandManager::send_normal_text(self, out, &format!("Guild {id} renamed to {name}"));
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<guild id> <new name>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Renames a guild".into()
    }
}
