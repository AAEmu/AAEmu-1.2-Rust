//! GM test hook for the guild rename dialog. There is no C# `Scripts/Commands/TestGuild.cs`
//! (only its access-level entry survives); the 1.2 client cannot open the rename dialog on its
//! own, so a server-triggered `SCExpeditionShowRenameUI` is needed to exercise the flow.
//!
//! usage: /testguild rename

use futures::future::BoxFuture;

use super::prelude::*;
use crate::systems::social::{expedition, util};

/// `testguild`.
pub struct TestGuild;

impl TestGuild {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for TestGuild {
    fn command_names(&self) -> Vec<String> {
        names(&["testguild"])
    }

    fn execute(
        &self,
        _ctx: &CommandContext<'_>,
        _args: &[String],
        _out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        anyhow::bail!("TestGuild runs through execute_async")
    }

    fn execute_async<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
        args: &'a [String],
        out: &'a mut dyn MessageOutput,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            match args.first().map(String::as_str) {
                Some("rename") if args.len() == 1 => {
                    let server = ctx.server()?;
                    let player = ctx.player()?;
                    match expedition::offer_expedition_rename_ui(server, &player) {
                        Ok(()) => CommandManager::send_normal_text(
                            self,
                            out,
                            "Guild rename dialog sent; enter the new name in the client.",
                        ),
                        Err(code) => {
                            util::send_error(&player, code);
                            CommandManager::send_error_text(
                                self,
                                out,
                                &format!("Cannot offer a guild rename (error {code})."),
                            );
                        }
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
        "[rename]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Sends the guild rename dialog to the owner of your expedition (the 1.2 client cannot open it without a server packet).".into()
    }
}
