//! `/rename <old> <new>` (Rust only): GM character rename (WP-AC2).
//!
//! The 1.2 client has no character-rename trigger (`CSRenameExpedition` is the guild / nation
//! rename), so the GM command is the only production entry point. It works online and offline:
//! the target is resolved through the name cache, and the rename writes the database and the
//! name cache whether or not the character is in the world.

use futures::future::BoxFuture;

use super::prelude::*;
use crate::managers::name::normalize_name;
use crate::systems::services::character_rename;

/// `Rename` (Rust only).
pub struct Rename;

impl Rename {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for Rename {
    fn command_names(&self) -> Vec<String> {
        names(&["rename"])
    }

    fn execute(
        &self,
        _ctx: &CommandContext<'_>,
        _args: &[String],
        _out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        anyhow::bail!("Rename runs through execute_async")
    }

    fn execute_async<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
        args: &'a [String],
        out: &'a mut dyn MessageOutput,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if args.len() != 2 {
                CommandManager::send_default_help_text(self, out);
                return Ok(());
            }
            let server = ctx.server()?;
            let old_name = normalize_name(&args[0]);
            let character_id = server.names.get_character_id(&old_name);
            if character_id == 0 {
                CommandManager::send_error_text(
                    self,
                    out,
                    &format!("character {old_name:?} not found"),
                );
                return Ok(());
            }
            match character_rename::rename_character(server, character_id, &args[1]).await {
                Ok(old_name) => CommandManager::send_normal_text(
                    self,
                    out,
                    &format!(
                        "renamed {old_name} ({character_id}) to {}",
                        normalize_name(&args[1])
                    ),
                ),
                Err(error) => CommandManager::send_error_text(self, out, &error.to_string()),
            }
            Ok(())
        })
    }

    fn get_command_line_help(&self) -> String {
        "<old> <new>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Renames a character (online or offline)".into()
    }
}
