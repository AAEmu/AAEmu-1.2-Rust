//! `SpecialEffect.AddCharacterSlot` (special 91) and the account slot count sent when the
//! client lists its characters.
//!
//! Data: item 8000002 `캐릭터 슬롯확장 주문서` -> skill 8000002. The former C# body only logged.
//! The game server stores the purchased count on `accounts.extra_character_slots`; the login
//! server still advertises its fixed base slot count.

use crate::packets::g2c::SCGetSlotCountPacket;
use crate::server::GameServer;
use crate::world::PlayerRef;

/// `accounts.extra_character_slots` is `TINYINT UNSIGNED`; adding saturates there.
pub const MAX_EXTRA_CHARACTER_SLOTS: u8 = u8::MAX;

/// One more purchased slot, saturating at [`MAX_EXTRA_CHARACTER_SLOTS`].
pub fn next_slot_count(current: u8) -> u8 {
    current.saturating_add(1)
}

/// Adds one character slot to the caster's account, persists it and tells the client the new
/// `SCGetSlotCountPacket` value. The DB write runs as a task (skill effects are synchronous).
pub fn apply(server: &GameServer, player: &PlayerRef) {
    let account_id = player.data().account_id;
    let (Some(srv), Ok(handle)) = (server.self_arc(), tokio::runtime::Handle::try_current()) else {
        tracing::warn!("AddCharacterSlot: no Tokio runtime for the account slot update");
        return;
    };
    let player = player.clone();
    handle.spawn(async move {
        match crate::db::add_extra_character_slot(&srv.db, account_id).await {
            Some(count) => player.send(SCGetSlotCountPacket { sc: count }),
            None => tracing::error!("AddCharacterSlot: failed to update account {account_id}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_count_saturates() {
        assert_eq!(next_slot_count(0), 1);
        assert_eq!(next_slot_count(5), 6);
        assert_eq!(
            next_slot_count(MAX_EXTRA_CHARACTER_SLOTS),
            MAX_EXTRA_CHARACTER_SLOTS
        );
    }

    /// Requires the private MySQL on 3317 (`AAEMU_TEST_MYSQL`); the account table is a
    /// connection-local temporary one.
    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses a connection-local temporary accounts table"]
    async fn extra_character_slots_increment_and_persist() {
        use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
        let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
            return;
        };
        let options: MySqlConnectOptions = url.parse().expect("AAEMU_TEST_MYSQL");
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect");
        sqlx::query(
            "CREATE TEMPORARY TABLE accounts (account_id INT PRIMARY KEY, \
             extra_character_slots TINYINT UNSIGNED NOT NULL DEFAULT 0)",
        )
        .execute(&pool)
        .await
        .expect("temporary accounts");
        assert_eq!(crate::db::extra_character_slots(&pool, 7).await, 0);
        assert_eq!(crate::db::add_extra_character_slot(&pool, 7).await, Some(1));
        assert_eq!(crate::db::add_extra_character_slot(&pool, 7).await, Some(2));
        assert_eq!(crate::db::extra_character_slots(&pool, 7).await, 2);
        sqlx::query("DROP TEMPORARY TABLE accounts")
            .execute(&pool)
            .await
            .expect("drop");
    }
}
