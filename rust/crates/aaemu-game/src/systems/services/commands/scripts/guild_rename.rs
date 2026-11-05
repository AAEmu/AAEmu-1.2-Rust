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
    let live =
        |e: &&expedition::Expedition| !e.is_disbanded && !e.faction.name.starts_with("$deleted-");
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

#[cfg(test)]
mod tests {
    use super::super::harness;
    use crate::data::world_data::SystemFaction;
    use crate::opcodes::sc;
    use crate::systems::social::expedition::{self, Expedition};

    fn add_guild(server: &crate::server::GameServer, id: u32, name: &str, owner: u32) {
        server.expeditions.add(Expedition {
            faction: SystemFaction {
                id,
                name: name.into(),
                owner_id: owner,
                ..Default::default()
            },
            members: Vec::new(),
            policies: expedition::default_policies(server, id),
            removed_members: Vec::new(),
            is_disbanded: false,
        });
    }

    #[tokio::test]
    async fn guild_rename_commands_grant_the_right_and_rename() {
        let server = harness::server();
        let mut gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        add_guild(&server, 77, "Knights", 1);
        add_guild(&server, 88, "Rivals", 0);
        gm.p.data().unit.expedition_id = 77;
        gm.chat();

        let (_, out) = harness::run(&server, &gm.p, "allowguildrename").await;
        assert_eq!(
            out,
            ["Help for |cFFFFFFFF/allowguildrename|r |cFFEEEEAA<guild id | guild name>|r\n|cFF888888Lets a guild's leader rename the guild once|r"]
        );
        let (_, out) = harness::run(&server, &gm.p, "allowguildrename Nobody").await;
        assert_eq!(
            out,
            [harness::error("allowguildrename", "Guild Nobody not found")]
        );
        // By (case-insensitive) name; the leader (the GM here) gets the dialog.
        let (_, out) = harness::run(&server, &gm.p, "allowguildrename kNIGHTS").await;
        assert_eq!(
            out,
            ["[allowguildrename] Guild 77 may rename itself once; its leader gets the rename dialog"]
        );
        tokio::task::yield_now().await;
        for _ in 0..50 {
            if server
                .expeditions
                .get(77)
                .unwrap()
                .faction
                .allow_change_name
                == 1
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            server
                .expeditions
                .get(77)
                .unwrap()
                .faction
                .allow_change_name,
            1
        );
        assert!(gm
            .client
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCExpeditionShowRenameUIPacket));

        // Direct rename by id: the rules apply.
        let (_, out) = harness::run(&server, &gm.p, "renameguild 77 x1").await;
        assert_eq!(
            out,
            [harness::error(
                "renameguild",
                "Name refused (error message 296)"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "renameguild 77 rivals").await;
        assert_eq!(
            out,
            [harness::error(
                "renameguild",
                "Name refused (error message 299)"
            )]
        );
        let (_, out) = harness::run(&server, &gm.p, "renameguild 77 New Order").await;
        assert_eq!(out, ["[renameguild] Guild 77 renamed to New Order"]);
        for _ in 0..50 {
            if server.expeditions.get(77).unwrap().faction.name == "New Order" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let e = server.expeditions.get(77).unwrap();
        assert_eq!(e.faction.name, "New Order");
        // A GM rename does not spend the leader's right.
        assert_eq!(e.faction.allow_change_name, 1);
        assert!(gm
            .client
            .drain()
            .iter()
            .any(|(op, _)| *op == sc::SCFactionRenamedPacket));
        let (_, out) = harness::run(&server, &gm.p, "renameguild 5 Whatever").await;
        assert_eq!(out, [harness::error("renameguild", "Guild 5 not found")]);
    }
}
