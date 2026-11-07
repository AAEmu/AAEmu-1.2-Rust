//! Mail features outside the C# `MailManager` port: the GM `/mail admin` helper and the
//! `quest_mails` / `quest_mail_attachments` catalog.
//!
//! `quest_mails` is a 1.2 client table (one test row in the shipped data); the server does not
//! send these mails yet. `SCQuestMailSent` (0x150) is defined in `systems::quests::packets`
//! from the client reader `FUN_397b5f00`, but no trigger is implemented: whether the u32 is a
//! `quest_mails.id` and which quest step sends it is capture-pending (case `QUEST-MAIL-1`).
//! The catalog is loaded read-only so a future trigger has the data, and it is never consulted
//! by the existing mail flows.

use std::collections::HashMap;
use std::fmt;

use crate::data::sqlite::for_each_row;
use crate::data::GameDataError;
use crate::managers::name::normalize_name;
use crate::models::item::Item;
use crate::server::GameServer;
use crate::systems::DataLoadCtx;

use super::inventory::ItemTxn;
use super::mail::{self, mail_type, MAX_MAIL_ATTACHMENTS};

/// Sender name of [`send_admin_mail`]. The 1.2 client has no special case for an `ADMIN` (5)
/// sender name, so the mail shows this literal name; there is no C# command to copy.
pub const ADMIN_MAIL_SENDER: &str = "Admin";

/// One `quest_mail_attachments` row (only a display name; the table has no item column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestMailAttachment {
    pub id: u32,
    pub name: String,
}

/// One `quest_mails` row. Column names follow the 1.2 table (`COMMENT` is stored upper case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestMail {
    pub id: u32,
    pub name: String,
    pub comment: String,
    pub category_id: u32,
    pub text: String,
    pub npc_id: u32,
    pub sender_name: String,
    pub send_money: i32,
    pub attachment_id: u32,
}

/// The loaded `quest_mails` tables (`server.game_data.systems.economy.quest_mails`).
#[derive(Debug, Default)]
pub struct QuestMailData {
    pub mails: HashMap<u32, QuestMail>,
    pub attachments: HashMap<u32, QuestMailAttachment>,
}

impl QuestMailData {
    /// Loads both tables. A mail whose non-zero `quest_mail_attachment_id` has no
    /// `quest_mail_attachments` row is skipped with a warning: the attachment name would
    /// otherwise resolve to nothing.
    pub fn load(ctx: &DataLoadCtx<'_>) -> Result<Self, GameDataError> {
        let mut d = Self::default();
        for_each_row(ctx.conn, "SELECT * FROM quest_mail_attachments", [], |r| {
            let a = QuestMailAttachment {
                id: r.u32("id")?,
                name: r.string_or("name", "")?,
            };
            d.attachments.insert(a.id, a);
            Ok(())
        })?;
        let mut skipped = 0usize;
        for_each_row(ctx.conn, "SELECT * FROM quest_mails", [], |r| {
            let m = QuestMail {
                id: r.u32("id")?,
                name: r.string_or("name", "")?,
                comment: r.string_or("COMMENT", "")?,
                category_id: r.u32_or("category_id", 0)?,
                text: r.string_or("text", "")?,
                npc_id: r.u32_or("npc_id", 0)?,
                sender_name: r.string_or("sender_name", "")?,
                send_money: r.i32_or("send_money", 0)?,
                attachment_id: r.u32_or("quest_mail_attachment_id", 0)?,
            };
            if m.attachment_id != 0 && !d.attachments.contains_key(&m.attachment_id) {
                tracing::warn!(
                    quest_mail_id = m.id,
                    attachment_id = m.attachment_id,
                    "quest_mails: attachment row is missing; skipping the mail"
                );
                skipped += 1;
                return Ok(());
            }
            d.mails.insert(m.id, m);
            Ok(())
        })?;
        tracing::info!(
            quest_mails = d.mails.len(),
            attachments = d.attachments.len(),
            skipped,
            "loaded quest_mails"
        );
        Ok(d)
    }

    /// A quest mail template by `quest_mails.id`.
    pub fn mail(&self, id: u32) -> Option<&QuestMail> {
        self.mails.get(&id)
    }

    /// The display name of `quest_mail_attachments.id` (empty -> `None`).
    pub fn attachment_name(&self, id: u32) -> Option<&str> {
        self.attachments
            .get(&id)
            .map(|a| a.name.as_str())
            .filter(|n| !n.is_empty())
    }
}

/// Why [`send_admin_mail`] refused or failed. `Display` is the command's error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminMailError {
    /// No character id is registered for the name (online or offline).
    UnknownReceiver(String),
    /// The attached copper amount is negative.
    NegativeMoney,
    /// More than [`MAX_MAIL_ATTACHMENTS`] item stacks were requested.
    TooManyAttachments(usize),
    /// The item template does not exist.
    UnknownItem(u32),
    /// The item count is zero or negative.
    InvalidItemCount(u32),
    /// An item id could not be allocated.
    OutOfItemIds,
    /// The mail store refused the mail (name/id mismatch or attachment relocation failed).
    MailFailed,
}

impl fmt::Display for AdminMailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownReceiver(name) => write!(f, "Character not found: {name}"),
            Self::NegativeMoney => write!(f, "Money must not be negative"),
            Self::TooManyAttachments(n) => write!(
                f,
                "Too many attachments ({n}); at most {MAX_MAIL_ATTACHMENTS} are allowed"
            ),
            Self::UnknownItem(id) => write!(f, "Item template {id} does not exist"),
            Self::InvalidItemCount(id) => write!(f, "Item {id} needs a count of at least 1"),
            Self::OutOfItemIds => write!(f, "No item id available"),
            Self::MailFailed => write!(f, "The mail could not be stored"),
        }
    }
}

/// Sends an `ADMIN` (5) system mail (sender id 0) to a character by name, online or offline,
/// with optional copper and item stacks, through `mail::send_system_mail`. The items are
/// created once here and referenced by the stored mail; on failure every created item is
/// discarded again so no loose copy is left behind.
pub fn send_admin_mail(
    txn: &mut ItemTxn<'_>,
    receiver_name: &str,
    title: &str,
    text: &str,
    copper: i32,
    items: &[(u32, i32)],
) -> Result<(), AdminMailError> {
    let server = txn.server;
    if copper < 0 {
        return Err(AdminMailError::NegativeMoney);
    }
    if items.len() > MAX_MAIL_ATTACHMENTS {
        return Err(AdminMailError::TooManyAttachments(items.len()));
    }
    let receiver_id = server
        .world
        .character_by_name(receiver_name)
        .map(|p| p.character_id)
        .or_else(|| {
            let id = server
                .names
                .get_character_id(&normalize_name(receiver_name));
            (id != 0).then_some(id)
        })
        .ok_or_else(|| AdminMailError::UnknownReceiver(receiver_name.to_string()))?;

    // Validate every request before creating anything, so a bad template never leaves items.
    let mut created: Vec<Item> = Vec::with_capacity(items.len());
    for &(template_id, count) in items {
        let Some(template) = server.game_data.items.template(template_id) else {
            discard_items(server, &created);
            return Err(AdminMailError::UnknownItem(template_id));
        };
        if count <= 0 {
            discard_items(server, &created);
            return Err(AdminMailError::InvalidItemCount(template_id));
        }
        let count = count.min(template.max_count.max(1));
        let grade = template.fixed_grade.max(0) as u8;
        let Some(item) = txn.env.create(template_id, count, grade) else {
            discard_items(server, &created);
            return Err(AdminMailError::OutOfItemIds);
        };
        created.push(item);
    }

    let item_ids: Vec<u64> = created.iter().map(|i| i.id).collect();
    let sent = mail::send_system_mail(
        txn,
        mail_type::ADMIN,
        ADMIN_MAIL_SENDER,
        receiver_id,
        title,
        text,
        copper,
        created,
    );
    if !sent {
        discard_items_by_id(server, &item_ids);
        return Err(AdminMailError::MailFailed);
    }
    Ok(())
}

fn discard_items(server: &GameServer, items: &[Item]) {
    for item in items {
        server.items.release_id(item.id, &server.ids.item);
    }
}

fn discard_items_by_id(server: &GameServer, ids: &[u64]) {
    for &id in ids {
        if let Some(container) = server.items.holding_container(id) {
            container
                .lock()
                .expect("container poisoned")
                .remove_item(id);
        }
        server.items.release_id(id, &server.ids.item);
    }
}

// ---------------------------------------------------------------------------------------------
// Tax-rate notices
// ---------------------------------------------------------------------------------------------

/// The 1.2 client resolves a sender name that starts with `.` through
/// `locale.mail.<name>.sender` (`GetMailText`); `.houseTax` is the shipped example
/// (`systems/housing/manager.rs`). The rate mail locale tables are `taxRateChange` and
/// `nationalTaxRate` (`x2ui/baselib/locale_helper.alb`).
pub const TAX_RATE_CHANGED_SENDER: &str = ".taxRateChange";
pub const NATION_TAX_RATE_SENDER: &str = ".nationalTaxRate";

/// `locale.mail.taxRateChange.body(old_tax_rate, new_tax_rate)` (proto 79 of
/// `x2ui/baselib/locale_helper.alb`). The client displays the two percentages and derives the
/// new tax breakdown from `X2House.GetTaxations(new_tax_rate)`.
pub fn tax_rate_changed_text(old_rate: i32, new_rate: i32) -> String {
    format!("body('{old_rate}','{new_rate}')")
}

/// `locale.mail.nationalTaxRate.body(factionId, zoneGroupType, newTaxRate, oldTaxRate, day)`
/// (proto 110). The parameter order is the client's body parser's, not the display order:
/// `read_mail.alb` reads `oldTaxRate`/`newTaxRate` from the resulting table.
pub fn nation_tax_rate_text(
    nation_id: u32,
    zone_group_id: u16,
    new_rate: u16,
    old_rate: u16,
    changed_at: i64,
) -> String {
    format!("body('{nation_id}','{zone_group_id}','{new_rate}','{old_rate}','{changed_at}')")
}

/// Stores one `TAXRATE_CHANGED` (8) notice for a property owner in the territory.
pub fn send_tax_rate_changed_mail(
    txn: &mut ItemTxn<'_>,
    receiver_id: u32,
    zone_group_id: u16,
    old_rate: i32,
    new_rate: i32,
) -> bool {
    mail::send_system_mail(
        txn,
        mail_type::TAXRATE_CHANGED,
        TAX_RATE_CHANGED_SENDER,
        receiver_id,
        &format!("title({zone_group_id})"),
        &tax_rate_changed_text(old_rate, new_rate),
        0,
        Vec::new(),
    )
}

/// Stores one `NATION_TAX_RATE` (23) notice for a citizen of `nation_id`.
#[allow(clippy::too_many_arguments)]
pub fn send_nation_tax_rate_mail(
    txn: &mut ItemTxn<'_>,
    receiver_id: u32,
    nation_id: u32,
    zone_group_id: u16,
    new_rate: u16,
    old_rate: u16,
    changed_at: i64,
) -> bool {
    mail::send_system_mail(
        txn,
        mail_type::NATION_TAX_RATE,
        NATION_TAX_RATE_SENDER,
        receiver_id,
        &format!("title({zone_group_id})"),
        &nation_tax_rate_text(nation_id, zone_group_id, new_rate, old_rate, changed_at),
        0,
        Vec::new(),
    )
}

/// The owners of the houses in `zone_group_id`, deduplicated. The 1.2 `TAXRATE_CHANGED` mail
/// shows the property-tax breakdown (`X2House.GetTaxations`), so the affected residents are the
/// property owners of the territory. This recipient set is INFERRED from the client body
/// function and the existing house-tax bill path (`MailForTax` also addresses house owners);
/// the exact 1.2 server audience is capture-pending (`TAX-RATE-MAIL-1`).
pub fn tax_rate_changed_recipients(server: &GameServer, zone_group_id: u32) -> Vec<u32> {
    let mut owners = Vec::new();
    for house in server.systems.housing.manager.all() {
        let h = house.data();
        let owner = h.unit.owner_id;
        if owner == 0 || owners.contains(&owner) {
            continue;
        }
        if server
            .game_data
            .zones
            .zone_by_key(h.unit.transform.zone_id)
            .is_some_and(|zone| zone.group_id == zone_group_id)
        {
            owners.push(owner);
        }
    }
    owners
}

/// Sends the `TAXRATE_CHANGED` (8) notice to the property owners of the territory. Returns the
/// number of stored mails. The caller must not hold the housing operation lock (the mail store
/// is an economy lock).
pub fn send_tax_rate_changed_mails(
    server: &GameServer,
    zone_group_id: u16,
    old_rate: i32,
    new_rate: i32,
) -> usize {
    let recipients = tax_rate_changed_recipients(server, u32::from(zone_group_id));
    if recipients.is_empty() {
        return 0;
    }
    let mut txn = ItemTxn::begin(server);
    let mut sent = 0;
    for id in recipients {
        if send_tax_rate_changed_mail(&mut txn, id, zone_group_id, old_rate, new_rate) {
            sent += 1;
        }
    }
    sent
}

/// Online citizens of a nation (the same membership test as `nation::online_nation_members`).
fn online_nation_members(server: &GameServer, nation_id: u32) -> Vec<u32> {
    server
        .world
        .all_characters()
        .iter()
        .filter(|p| p.data().unit.faction_id == nation_id)
        .map(|p| p.character_id)
        .collect()
}

/// Sends the `NATION_TAX_RATE` (23) notice to the nation's online citizens and stores it for
/// the offline ones. The client body carries the nation id and the zone group
/// (`X2Nation.GetNationName` / `X2Dominion.GetZoneGroupName`), so the audience is the nation's
/// citizens; the exact 1.2 audience is INFERRED and capture-pending (`NATION-TAX-MAIL-1`).
pub fn send_nation_tax_rate_mails(
    server: &GameServer,
    nation_id: u32,
    zone_group_id: u16,
    new_rate: u16,
    old_rate: u16,
    changed_at: i64,
) {
    let online = online_nation_members(server, nation_id);
    if !online.is_empty() {
        let mut txn = ItemTxn::begin(server);
        for id in &online {
            send_nation_tax_rate_mail(
                &mut txn,
                *id,
                nation_id,
                zone_group_id,
                new_rate,
                old_rate,
                changed_at,
            );
        }
    }
    // Offline citizens need the same notice. The query runs after the caller released its
    // housing lock; the online set is skipped because those mails are already stored.
    let Some(server) = server.self_arc() else {
        return;
    };
    tokio::spawn(async move {
        let rows = sqlx::query_scalar::<_, i64>(
            "SELECT CAST(id AS SIGNED) FROM characters WHERE faction_id = ? AND deleted = 0",
        )
        .bind(i64::from(nation_id))
        .fetch_all(&server.db)
        .await;
        let Ok(ids) = rows else {
            tracing::warn!(
                nation_id,
                "could not load the offline citizens for the national tax notice"
            );
            return;
        };
        let offline: Vec<u32> = ids
            .into_iter()
            .map(|id| id as u32)
            .filter(|id| !online.contains(id))
            .collect();
        if offline.is_empty() {
            return;
        }
        let mut txn = ItemTxn::begin(&server);
        for id in offline {
            send_nation_tax_rate_mail(
                &mut txn,
                id,
                nation_id,
                zone_group_id,
                new_rate,
                old_rate,
                changed_at,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::QuestMailData;
    use crate::data::client_files::ClientFileManager;
    use crate::systems::DataLoadCtx;

    fn load(conn: &rusqlite::Connection) -> QuestMailData {
        let files = ClientFileManager::from_sources(&[], Path::new("."));
        QuestMailData::load(&DataLoadCtx {
            conn,
            app_path: Path::new("."),
            files: &files,
            language: "en_us",
        })
        .unwrap()
    }

    #[test]
    fn loader_reads_both_tables_and_skips_a_missing_attachment() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE quest_mail_attachments (id int PRIMARY KEY, name TEXT);
             INSERT INTO quest_mail_attachments VALUES (1, 'first'), (2, 'second');
             CREATE TABLE quest_mails (
                 id int PRIMARY KEY,
                 name TEXT,
                 COMMENT TEXT,
                 category_id INT,
                 text TEXT,
                 npc_id INT,
                 sender_name TEXT,
                 send_money INT,
                 quest_mail_attachment_id INT
             );
             INSERT INTO quest_mails VALUES
                 (1, 'Letter', 'note', 55, 'body', 13144, 'Sender', 42, 2),
                 (2, 'No attachment', NULL, 55, 'plain', 13144, '', 0, 0),
                 (3, 'Bad attachment', NULL, 55, 'lost', 13144, '', 0, 99);",
        )
        .unwrap();
        let d = load(&conn);
        assert_eq!(d.attachments.len(), 2);
        assert_eq!(d.attachment_name(2), Some("second"));
        assert_eq!(d.attachment_name(99), None);
        assert!(d.mail(3).is_none(), "missing attachment row skips the mail");
        let m = d.mail(1).expect("mail 1");
        assert_eq!(
            (
                m.name.as_str(),
                m.comment.as_str(),
                m.category_id,
                m.text.as_str(),
                m.npc_id,
                m.sender_name.as_str(),
                m.send_money,
                m.attachment_id,
            ),
            ("Letter", "note", 55, "body", 13144, "Sender", 42, 2)
        );
        let m = d.mail(2).expect("mail 2");
        assert_eq!((m.attachment_id, m.send_money), (0, 0));
    }

    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    fn real_compact_db_quest_mails() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let d = load(&conn);
        // The shipped 1.2 data has one test mail (category 55, npc 13144) and two attachment
        // names; the mail references attachment 2.
        assert_eq!(d.mails.len(), 1);
        assert_eq!(d.attachments.len(), 2);
        let m = d.mail(1).expect("quest mail 1");
        assert_eq!(
            (m.category_id, m.npc_id, m.send_money, m.attachment_id),
            (55, 13144, 0, 2)
        );
        assert!(d.attachment_name(2).is_some_and(|n| !n.is_empty()));
    }
}
