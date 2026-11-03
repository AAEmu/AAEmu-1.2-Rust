//! Admin recovery for the 1.2 second password (Rust only; the C# has no second password).

use futures::future::BoxFuture;

use super::prelude::*;
use crate::systems::services::second_password;

/// `SecondPassword`: clears an account's second password, failure counter and lock.
pub struct SecondPassword;

impl SecondPassword {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for SecondPassword {
    fn command_names(&self) -> Vec<String> {
        names(&["secondpassword", "secondpass", "clearsecondpassword"])
    }

    fn execute(
        &self,
        _ctx: &CommandContext<'_>,
        _args: &[String],
        _out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        anyhow::bail!("SecondPassword runs through execute_async")
    }

    fn execute_async<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
        args: &'a [String],
        out: &'a mut dyn MessageOutput,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let server = ctx.server()?;
            let account_id = match args.first().map(String::as_str) {
                None | Some("self") => ctx.player()?.data().account_id,
                Some(text) => match try_parse::<u32>(text) {
                    Some(id) => id,
                    None => {
                        let Some(target) = server.world.character_by_name(text) else {
                            CommandManager::send_error_text(
                                self,
                                out,
                                &format!("Account or character '{text}' not found"),
                            );
                            return Ok(());
                        };
                        let account_id = target.data().account_id;
                        account_id
                    }
                },
            };
            if !second_password::clear_account(server, account_id).await {
                CommandManager::send_error_text(
                    self,
                    out,
                    &format!("Could not clear the second password of account {account_id}"),
                );
                return Ok(());
            }
            tracing::info!("second password: account {account_id} cleared by GM command");
            CommandManager::send_normal_text(
                self,
                out,
                &format!("Second password of account {account_id} cleared"),
            );
            Ok(())
        })
    }

    fn get_command_line_help(&self) -> String {
        "[<account id|character name|self>]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Clears an account's second password, failure counter and lock (admin recovery)".into()
    }
}
