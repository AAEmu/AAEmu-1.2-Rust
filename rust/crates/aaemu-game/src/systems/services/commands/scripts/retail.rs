//! Shared lookups for the retail-only GM commands of WP-SL6b (tax / crime / social).

use super::prelude::*;

/// A character name to id: an online player first, then the name manager (offline characters).
pub fn character_id(server: &GameServer, name: &str) -> Option<u32> {
    if let Some(player) = server.world.character_by_name(name) {
        return Some(player.character_id);
    }
    let id = server
        .names
        .get_character_id(&crate::managers::name::normalize_name(name));
    (id != 0).then_some(id)
}

/// The canonical character name for `id` (`None` falls back to the id).
pub fn character_name(server: &GameServer, id: u32) -> String {
    server
        .names
        .get_character_name(id)
        .unwrap_or_else(|| id.to_string())
}

/// The online player `name`, or `None`.
pub fn online(server: &GameServer, name: &str) -> Option<PlayerRef> {
    server.world.character_by_name(name)
}

#[cfg(test)]
mod tests {
    use super::super::harness;

    const COMMANDS: [(&str, &str); 7] = [
        ("delay_tax_due_date", "delay_tax_due_date Nobody"),
        ("resend_house_tax_mail", "resend_house_tax_mail Nobody"),
        ("set_bounty_money", "set_bounty_money Nobody 1"),
        ("clear_crime_status", "clear_crime_status Nobody"),
        ("set_crime_value", "set_crime_value Nobody 1"),
        ("set_jury", "set_jury Nobody 1"),
        ("imprison", "imprison Nobody"),
    ];

    #[tokio::test]
    async fn retail_commands_require_admin_access() {
        use crate::config::AppConfig;

        let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
        if !app.join("Config.json").exists() {
            return;
        }
        // The shipped Config.json keeps %login_port% / %db_port% placeholders.
        let args: Vec<String> = [
            "--LoginNetwork:Port=1234",
            "/Connections:MySQLProvider:Port",
            "3306",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let config = AppConfig::load_from(&app, None, Vec::new(), &args).unwrap();
        let server = harness::server();
        server
            .systems
            .services
            .commands
            .access_levels
            .load(&config.access_level);
        let mut player = harness::pc(&server, 1, "Player", 100.0, 100.0);

        for (name, text) in COMMANDS {
            assert_eq!(config.access_level_of(name), Some(100), "{name}");
            let (handled, out) = harness::run_at(&server, &player.p, text, 0).await;
            assert!(handled, "{name} was not handled");
            assert_eq!(out, ["|cFFFF0000Insufficient privileges.|r"], "{name}");
            let frames = player.frames();
            assert!(
                frames
                    .iter()
                    .all(|(op, _)| *op == crate::opcodes::sc::SCChatMessagePacket),
                "{name} ran without access: {frames:?}"
            );
        }
    }
}
