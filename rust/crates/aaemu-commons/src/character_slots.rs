//! Character-slot arithmetic shared by the login and game servers.
//!
//! The client is told an account's slots in two parts: the login server advertises the base
//! slots (`ACJoinResponse.afs` and `ACAuthResponse.slot_count`), and the game server sends only
//! the purchased slots in `SCGetSlotCount` when the character list is requested. The client adds
//! them; the game server enforces the same sum when a character is created.

/// The default base character slots of an account (`Login:MaxCharactersPerAccount` in
/// aaemu-login, `Characters:MaxPerAccount` in aaemu-game).
pub const DEFAULT_MAX_CHARACTERS_PER_ACCOUNT: u8 = 6;

/// The character creation limit: the configured base slots plus the account's purchased
/// (`accounts.extra_character_slots`) slots. Saturates at `u8::MAX`, like the account column.
pub fn character_slot_limit(base: u8, extra_slots: u8) -> u8 {
    base.saturating_add(extra_slots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_base_plus_purchased_slots() {
        assert_eq!(character_slot_limit(6, 0), 6);
        assert_eq!(character_slot_limit(6, 2), 8);
        assert_eq!(character_slot_limit(8, 2), 10);
        assert_eq!(character_slot_limit(255, 1), 255);
        assert_eq!(DEFAULT_MAX_CHARACTERS_PER_ACCOUNT, 6);
    }
}
