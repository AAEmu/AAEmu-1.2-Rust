//! The `LocalizationManager` lookups the mail code needs (`items.name` for auction / trade
//! pack mails, `quest_contexts.name` for quest reward mails, `ui_texts.text` for the specialty
//! mail titles), loaded from `localized_texts` in `DefaultLanguage` like
//! `LocalizationManager.Load` (first row of a key wins, `TryAdd`).

use std::collections::HashMap;

use crate::data::sqlite::for_each_row;
use crate::data::GameDataError;
use crate::server::GameServer;
use crate::systems::DataLoadCtx;

/// Mail game data (`server.game_data.systems.economy.mail`).
#[derive(Debug, Default)]
pub struct MailData {
    /// `items` / `name` translations by template id.
    pub item_names: HashMap<u32, String>,
    /// `quest_contexts` / `name` translations by quest template id.
    pub quest_names: HashMap<u32, String>,
    /// `ui_texts.key` -> the selected language's `text` (falling back to the row's default).
    pub ui_texts: HashMap<String, String>,
}

impl MailData {
    pub fn load(ctx: &DataLoadCtx<'_>) -> Result<Self, GameDataError> {
        let mut d = Self::default();
        // The column name comes from the configuration; quote it as an identifier.
        let sql = format!(
            "SELECT tbl_name, idx, \"{}\" AS txt FROM localized_texts \
             WHERE tbl_column_name = 'name' AND tbl_name IN ('items', 'quest_contexts')",
            ctx.language.replace('"', "")
        );
        for_each_row(ctx.conn, &sql, [], |r| {
            let tbl = r.string("tbl_name")?;
            let idx = r.i64("idx")? as u32;
            let text = r.string_or("txt", "")?;
            let map = if tbl == "items" {
                &mut d.item_names
            } else {
                &mut d.quest_names
            };
            map.entry(idx).or_insert(text);
            Ok(())
        })?;
        // `ui_texts` are the strings the 1.2 client localizes (e.g. the specialty mail titles
        // `mail_sell_backpack_title_seller` / `_crafter`); `localized_texts` holds the per
        // language text for `tbl_name = 'ui_texts'`, `tbl_column_name = 'text'`.
        let sql = format!(
            "SELECT t.`key` AS k, l.\"{}\" AS txt \
             FROM `ui_texts` t LEFT JOIN `localized_texts` l \
             ON l.`tbl_name` = 'ui_texts' AND l.`tbl_column_name` = 'text' AND l.`idx` = t.`id`",
            ctx.language.replace('"', "")
        );
        for_each_row(ctx.conn, &sql, [], |r| {
            let key = r.string("k")?;
            let text = r.string_or("txt", "")?;
            d.ui_texts.entry(key).or_insert(text);
            Ok(())
        })?;
        Ok(d)
    }

    /// `LocalizationManager.Get("items", "name", templateId, fallback)` (empty -> fallback).
    pub fn item_name(&self, template_id: u32, fallback: &str) -> String {
        get(&self.item_names, template_id, fallback)
    }

    /// `LocalizationManager.Get("quest_contexts", "name", questId, fallback)`.
    pub fn quest_name(&self, quest_id: u32, fallback: &str) -> String {
        get(&self.quest_names, quest_id, fallback)
    }

    /// A localized `ui_texts` string by key (empty -> fallback).
    pub fn ui_text(&self, key: &str, fallback: &str) -> String {
        match self.ui_texts.get(key) {
            Some(t) if !t.is_empty() => t.clone(),
            _ => fallback.to_string(),
        }
    }
}

/// The loaded `MailData` of `server` (`server.game_data.systems.economy.mail`).
pub fn localized_ui_text(server: &GameServer, key: &str, fallback: &str) -> String {
    server.game_data.systems.economy.mail.ui_text(key, fallback)
}

fn get(map: &HashMap<u32, String>, idx: u32, fallback: &str) -> String {
    match map.get(&idx) {
        Some(t) if !t.is_empty() => t.clone(),
        _ => fallback.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::MailData;

    #[test]
    fn empty_or_missing_ui_text_falls_back_to_the_english_string() {
        let mut d = MailData::default();
        d.ui_texts
            .insert("mail_sell_backpack_title_seller".into(), String::new());
        assert_eq!(
            d.ui_text(
                "mail_sell_backpack_title_seller",
                "Specialty Payment [Delivery]"
            ),
            "Specialty Payment [Delivery]"
        );
        assert_eq!(
            d.ui_text("missing_key", "Specialty Payment"),
            "Specialty Payment"
        );
    }
}
