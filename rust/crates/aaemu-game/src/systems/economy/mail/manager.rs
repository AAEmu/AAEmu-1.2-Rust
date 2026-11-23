use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use sqlx::Row;

use crate::db::{dt, from_unix, int, unix};
use crate::models::item::UnixTime;
use crate::server::GameServer;
use crate::systems::HookSlot;

use super::super::inventory::ItemTxn;
use super::{
    check_all_mail_timings, mail_type, mutate_item, now, relocate_attachments, BaseMail,
    DotNetDict, MailBody, MailHeader, MailRetention, MailSendHistory,
    DEFAULT_TRADE_PACK_MAIL_DELAY_MINUTES, MAIL_REPEAT_LIMIT, MAX_MAIL_ATTACHMENTS,
};

// ---------------------------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------------------------

/// What `MailManager.PayChargeMoney` needs from the housing subsystem (`HousingManager`).
/// Installed by the housing owner in its `System::start`; the defaults behave as if no house
/// exists (there is no housing without that subsystem).
pub trait MailHousingHooks: Send + Sync {
    /// `HousingManager.GetHouseById(houseId) != null`.
    fn house_exists(&self, _server: &GameServer, _house_id: u32) -> bool {
        false
    }

    /// The current owner of an existing house (tax bills cannot follow a former owner).
    fn house_owner_id(&self, _server: &GameServer, _house_id: u32) -> Option<u32> {
        None
    }

    /// `HousingManager.PayWeeklyTax(house)` (adds `DaysForTaxPayment` to the protection end).
    fn pay_weekly_tax(&self, _server: &GameServer, _house_id: u32) -> bool {
        false
    }

    /// Rechecks ownership when applying the payment. Implementations should validate and
    /// extend protection under the same house lock.
    fn pay_weekly_tax_for(&self, server: &GameServer, house_id: u32, payer_id: u32) -> bool {
        self.house_owner_id(server, house_id) == Some(payer_id)
            && self.pay_weekly_tax(server, house_id)
    }
}

/// The mail hook slots (`server.systems.economy.mail.hooks`).
#[derive(Debug, Default)]
pub struct MailHooks {
    pub housing: HookSlot<dyn MailHousingHooks>,
}

// ---------------------------------------------------------------------------------------------
// MailManager
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct MailState {
    /// `_allPlayerMails`.
    pub all: DotNetDict<BaseMail>,
    /// `_deletedMailIds`.
    pub deleted: Vec<i64>,
    // A recycled ID can be deleted again while an older save is pending. Keep the
    // deletion identity alive in that batch so its acknowledgment cannot clear a
    // later deletion of the same ID.
    deletion_versions: HashMap<i64, Arc<()>>,
}

impl MailState {
    pub(crate) fn clear_deletion(&mut self, id: i64) {
        self.deleted.retain(|deleted| *deleted != id);
        self.deletion_versions.remove(&id);
    }
}

/// `MailManager` (`server.systems.economy.mail`).
#[derive(Debug)]
pub struct MailManager {
    state: Mutex<MailState>,
    pending_save: Mutex<Option<MailSaveBatch>>,
    pub hooks: MailHooks,
    /// `AppConfiguration.Specialty.TradePackMailDelayInMinutes` (`/settradepackmaildelay`).
    trade_pack_mail_delay_minutes: Mutex<f64>,
    /// `Mail` retention (`Configurations/Mail.json`, the 1.2 `mail_guide` windows).
    retention: Mutex<MailRetention>,
    /// Last body text per sender (`MAIL_REPETITION`), see [MailSendHistory].
    send_history: Mutex<HashMap<u32, MailSendHistory>>,
    /// Mail ids whose "kept, cannot be returned" warning was already logged, so the 5-second
    /// expiry tick does not repeat it.
    kept_warned: Mutex<HashSet<i64>>,
}

#[derive(Debug)]
pub(crate) struct MailSaveBatch {
    deleted: Vec<(i64, Arc<()>)>,
    pub(super) dirty: Vec<BaseMail>,
}

impl Default for MailManager {
    fn default() -> Self {
        Self {
            state: Mutex::new(MailState::default()),
            pending_save: Mutex::new(None),
            hooks: MailHooks::default(),
            trade_pack_mail_delay_minutes: Mutex::new(DEFAULT_TRADE_PACK_MAIL_DELAY_MINUTES),
            retention: Mutex::new(MailRetention::default()),
            send_history: Mutex::new(HashMap::new()),
            kept_warned: Mutex::new(HashSet::new()),
        }
    }
}

impl MailManager {
    /// The mail store (see the module docs for the lock order).
    pub fn state(&self) -> MutexGuard<'_, MailState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn trade_pack_mail_delay_minutes(&self) -> f64 {
        *self
            .trade_pack_mail_delay_minutes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_trade_pack_mail_delay_minutes(&self, minutes: f64) {
        *self
            .trade_pack_mail_delay_minutes
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = minutes;
    }

    /// The active retention windows (`Configurations/Mail.json`).
    pub fn retention(&self) -> MailRetention {
        *self.retention.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_retention(&self, retention: MailRetention) {
        *self.retention.lock().unwrap_or_else(|e| e.into_inner()) = retention;
    }

    /// Whether sending this mail now would be the fourth identical mail in a row
    /// (`MAIL_REPETITION`). The key is (receiver, title, body): the same text sent to different
    /// characters is not the same mail, and a body-less item-only mail has no text to compare,
    /// so it is never refused here. The caller records the send after it succeeds.
    pub fn would_repeat_send(
        &self,
        character_id: u32,
        receiver_id: u32,
        title: &str,
        text: &str,
    ) -> bool {
        if text.is_empty() {
            return false;
        }
        let history = self.send_history.lock().unwrap_or_else(|e| e.into_inner());
        history.get(&character_id).is_some_and(|h| {
            h.receiver_id == receiver_id
                && h.title == title
                && h.text == text
                && h.repeats >= MAIL_REPEAT_LIMIT
        })
    }

    /// Records a successful player-to-player send for [`Self::would_repeat_send`]. An
    /// empty-body send is recorded too, so it breaks the "in a row" streak of a text mail.
    pub fn record_send(&self, character_id: u32, receiver_id: u32, title: &str, text: &str) {
        let mut history = self.send_history.lock().unwrap_or_else(|e| e.into_inner());
        let entry = history.entry(character_id).or_default();
        if entry.receiver_id == receiver_id && entry.title == title && entry.text == text {
            entry.repeats = entry.repeats.saturating_add(1);
        } else {
            entry.receiver_id = receiver_id;
            entry.title = title.to_string();
            entry.text = text.to_string();
            entry.repeats = 1;
        }
    }

    /// Logs the "kept, cannot be returned" warning once per mail id.
    pub fn warn_kept_once(&self, mail_id: i64, mail_type: u8, sender_id: u32) {
        let mut warned = self.kept_warned.lock().unwrap_or_else(|e| e.into_inner());
        if !warned.insert(mail_id) {
            return;
        }
        tracing::warn!(
            mail_id,
            mail_type,
            sender_id,
            "Expired player mail with unclaimed attachments cannot be returned; keeping it"
        );
    }

    /// A deleted mail id may be recycled; let the new letter log its own warning.
    pub fn forget_kept_warning(&self, mail_id: i64) {
        self.kept_warned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&mail_id);
    }

    /// Forgets the sender's repetition streak (leave world).
    pub fn clear_send_history(&self, character_id: u32) {
        self.send_history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&character_id);
    }

    /// `GetMailById(id)` (a copy).
    pub fn get_mail_by_id(&self, id: i64) -> Option<BaseMail> {
        self.state().all.get(id).cloned()
    }

    /// Runs `f` on the stored mail `id`.
    pub fn with_mail_mut<R>(&self, id: i64, f: impl FnOnce(&mut BaseMail) -> R) -> Option<R> {
        self.state().all.get_mut(id).map(f)
    }

    /// Updates the cached sender / receiver names of a renamed character. Returns the number of
    /// changed name fields. The caller owns `SaveManager::transaction_lock`, so no save can
    /// snapshot the old names after this swap.
    pub fn rename_character(&self, character_id: u32, new_name: &str) -> usize {
        let mut st = self.state();
        let mut changed = 0;
        for (_, mail) in st.all.iter_mut() {
            if mail.header.sender_id == character_id && mail.header.sender_name != new_name {
                mail.header.sender_name = new_name.to_string();
                changed += 1;
            }
            if mail.header.receiver_id == character_id && mail.receiver_name != new_name {
                mail.receiver_name = new_name.to_string();
                changed += 1;
            }
        }
        changed
    }

    /// `AllPlayerMails.Count`.
    pub fn count(&self) -> usize {
        self.state().all.len()
    }

    /// `GetNewMailId()`.
    pub fn get_new_mail_id(&self, server: &GameServer) -> u32 {
        let mut st = self.state();
        new_mail_id(server, &mut st)
    }

    /// `DeleteMail(long id)`: queues the row deletion, releases the id and forgets the mail.
    pub fn delete_mail(&self, server: &GameServer, id: i64) -> bool {
        let mut st = self.state();
        delete_mail_in(server, &mut st, id)
    }

    /// `GetMyHouseMails(houseId)`: the ids of the tax mails of that house.
    pub fn get_my_house_mails(&self, house_id: u32) -> Vec<i64> {
        let st = self.state();
        st.all
            .iter()
            .filter(|(_, m)| {
                m.mail_type == mail_type::BILLING
                    && m.header.sender_id == 0
                    && m.header.sender_name == ".houseTax"
                    && extract_extra_for_house(m.header.extra).1 == house_id
            })
            .map(|(_, m)| m.id)
            .collect()
    }

    /// `Load()` (the MySQL part): every row of `mails`. The delivery task is scheduled by
    /// [`start_delivery_task`].
    pub async fn load(&self, server: &GameServer) -> crate::Result<()> {
        tracing::info!("Loading player mails ...");
        let rows = sqlx::query(MAIL_SELECT).fetch_all(&server.db).await?;
        let mut loaded = Vec::with_capacity(rows.len());
        for r in &rows {
            let mut attachment_ids = [0u64; MAX_MAIL_ATTACHMENTS];
            for (i, a) in attachment_ids.iter_mut().enumerate() {
                *a = r.try_get::<u64, _>(format!("attachment{i}").as_str())?;
            }
            loaded.push(MailRow {
                id: int(r, "id")?,
                title: r.try_get("title")?,
                mail_type: int(r, "type")?,
                receiver_name: r.try_get("receiver_name")?,
                open_date: unix(dt(r, "open_date")),
                status: int(r, "status")?,
                sender_id: int(r, "sender_id")?,
                sender_name: r.try_get("sender_name")?,
                attachment_count: int(r, "attachment_count")?,
                receiver_id: int(r, "receiver_id")?,
                returned: int(r, "returned")?,
                extra: int(r, "extra")?,
                text: r.try_get("text")?,
                money1: int(r, "money_amount_1")?,
                money2: int(r, "money_amount_2")?,
                money3: int(r, "money_amount_3")?,
                send_date: unix(dt(r, "send_date")),
                received_date: unix(dt(r, "received_date")),
                attachment_ids,
            });
        }
        self.load_rows(server, loaded, now());
        tracing::info!("Loaded {} player mails", self.count());
        Ok(())
    }

    /// The row handling of `Load()` (DB-free).
    pub fn load_rows(&self, server: &GameServer, rows: Vec<MailRow>, now: UnixTime) {
        let mut txn = ItemTxn::begin(server);
        let mut st = self.state();
        st.all = DotNetDict::default();
        st.deleted.clear();
        st.deletion_versions.clear();
        let mut repaired = 0usize;
        for r in rows {
            let mut m = BaseMail {
                id: i64::from(r.id as i32),
                title: r.title,
                mail_type: r.mail_type as i32 as u8,
                receiver_name: r.receiver_name,
                open_date: r.open_date,
                ..BaseMail::default()
            };
            m.header = MailHeader {
                status: r.status as i32 as u8,
                sender_id: r.sender_id as u32,
                sender_name: r.sender_name,
                attachments: r.attachment_count as i32 as u8,
                receiver_id: r.receiver_id as u32,
                returned: r.returned as i32 != 0,
                extra: r.extra,
            };
            m.body = MailBody {
                text: r.text,
                copper_coins: r.money1 as i32,
                billing_amount: r.money2 as i32,
                money_amount2: r.money3 as i32,
                send_date: r.send_date,
                recv_date: r.received_date,
                attachments: Vec::new(),
            };
            // Read/Load Items
            for item_id in r.attachment_ids {
                if item_id == 0 {
                    continue;
                }
                let receiver = u64::from(m.header.receiver_id);
                if mutate_item(server, item_id, |i| {
                    i.owner_id = receiver;
                    i.dirty = true;
                }) {
                    m.body.attachments.push(item_id);
                } else {
                    tracing::warn!(
                        "Found orphaned itemId {item_id} in mailId {}, not loaded!",
                        m.id
                    );
                }
            }
            if !relocate_attachments(&mut txn, &m) {
                tracing::error!("Unable to restore attachments for mail {}", m.id);
            }
            let mut attachment_count = m.body.attachments.len() as i32;
            if m.body.copper_coins > 0 {
                attachment_count += 1;
            }
            if m.body.billing_amount > 0 {
                attachment_count += 1;
            }
            if m.body.money_amount2 > 0 {
                attachment_count += 1;
            }
            if attachment_count != i32::from(m.header.attachments) {
                tracing::warn!(
                    "Attachment count listed in mailId {} did not match the number of attachments, possible mail or item corruption !",
                    m.id
                );
            }
            // Reset the attachment counter
            m.header.attachments = attachment_count as u8;
            // Set internal delivered flag
            m.is_delivered = m.body.recv_date <= now;
            // Marketplace mails written before their item names were escaped: rewrite them and
            // let the next save persist the fix.
            m.is_dirty = super::mail_template::repair_charged_mail(&mut m);
            repaired += usize::from(m.is_dirty);
            // Remove from delete list if it's a recycled Id
            let id = m.id;
            st.clear_deletion(id);
            if !st.all.add(id, m) {
                // C#: Dictionary.Add throws (the load aborts).
                tracing::error!("Duplicate mail id {id} in mails table");
            }
        }
        if repaired > 0 {
            tracing::info!("Repaired {repaired} marketplace mail(s) with unescaped parentheses");
        }
    }

    /// `Save(connection, transaction)`: `(updated, deleted)`.
    pub async fn save(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    ) -> crate::Result<(usize, usize)> {
        self.write_prepared(tx, self.prepare_save()).await
    }

    pub(crate) fn prepare_save(&self) -> MailSaveBatch {
        let (deleted, dirty) = {
            let mut st = self.state();
            let deleted = st
                .deleted
                .clone()
                .into_iter()
                .map(|id| {
                    let version = st
                        .deletion_versions
                        .entry(id)
                        .or_insert_with(|| Arc::new(()))
                        .clone();
                    (id, version)
                })
                .collect();
            let mut dirty = Vec::new();
            for (_, m) in st.all.iter() {
                if m.is_dirty {
                    dirty.push(m.clone());
                }
            }
            (deleted, dirty)
        };
        MailSaveBatch { deleted, dirty }
    }

    pub(crate) async fn write_prepared(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        batch: MailSaveBatch,
    ) -> crate::Result<(usize, usize)> {
        *self.pending_save.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let MailSaveBatch { deleted, dirty } = batch;
        let deleted_count = deleted.len();
        if !deleted.is_empty() {
            let list = deleted
                .iter()
                .map(|(id, _)| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            sqlx::query(&format!("DELETE FROM mails WHERE `id` IN({list})"))
                .execute(&mut **tx)
                .await?;
        }
        let mut updated = 0;
        for m in &dirty {
            let mut q = sqlx::query(MAIL_REPLACE)
                .bind(m.id)
                .bind(i32::from(m.mail_type))
                .bind(i32::from(m.header.status))
                .bind(&m.title)
                .bind(&m.body.text)
                .bind(m.header.sender_id)
                .bind(&m.header.sender_name)
                .bind(i32::from(m.header.attachments))
                .bind(m.header.receiver_id)
                .bind(&m.receiver_name)
                .bind(from_unix(m.open_date))
                .bind(from_unix(m.body.send_date))
                .bind(from_unix(m.body.recv_date))
                .bind(i32::from(m.header.returned))
                .bind(m.header.extra)
                .bind(m.body.copper_coins)
                .bind(m.body.billing_amount)
                .bind(m.body.money_amount2);
            for i in 0..MAX_MAIL_ATTACHMENTS {
                q = q.bind(m.body.attachments.get(i).copied().unwrap_or(0));
            }
            q.execute(&mut **tx).await?;
            updated += 1;
        }
        *self.pending_save.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(MailSaveBatch { deleted, dirty });
        Ok((updated, deleted_count))
    }

    /// Acknowledge only committed, unchanged mail rows. Rollback leaves all bills,
    /// attachments, sale receipts and pending deletions available for retry.
    pub fn commit_save(&self) {
        let batch = self
            .pending_save
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(batch) = batch else { return };
        self.acknowledge_committed(batch);
    }

    pub(crate) fn acknowledge_committed(&self, batch: MailSaveBatch) {
        let mut st = self.state();
        for (id, version) in batch.deleted {
            if st
                .deletion_versions
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &version))
            {
                st.clear_deletion(id);
            }
        }
        for snapshot in batch.dirty {
            if let Some(live) = st.all.get_mut(snapshot.id) {
                if *live == snapshot {
                    live.is_dirty = false;
                }
            }
        }
    }
}

/// A `mails` row.
#[derive(Debug, Clone, Default)]
pub struct MailRow {
    pub id: i64,
    pub mail_type: i64,
    pub status: i64,
    pub title: String,
    pub text: String,
    pub sender_id: i64,
    pub sender_name: String,
    pub attachment_count: i64,
    pub receiver_id: i64,
    pub receiver_name: String,
    pub open_date: UnixTime,
    pub send_date: UnixTime,
    pub received_date: UnixTime,
    pub returned: i64,
    pub extra: i64,
    pub money1: i64,
    pub money2: i64,
    pub money3: i64,
    pub attachment_ids: [u64; MAX_MAIL_ATTACHMENTS],
}

const MAIL_SELECT: &str = "SELECT CAST(id AS SIGNED) AS id, CAST(`type` AS SIGNED) AS `type`, \
     CAST(status AS SIGNED) AS status, title, `text`, CAST(sender_id AS SIGNED) AS sender_id, sender_name, \
     CAST(attachment_count AS SIGNED) AS attachment_count, CAST(receiver_id AS SIGNED) AS receiver_id, \
     receiver_name, open_date, send_date, received_date, CAST(returned AS SIGNED) AS returned, \
     CAST(extra AS SIGNED) AS extra, CAST(money_amount_1 AS SIGNED) AS money_amount_1, \
     CAST(money_amount_2 AS SIGNED) AS money_amount_2, CAST(money_amount_3 AS SIGNED) AS money_amount_3, \
     CAST(attachment0 AS UNSIGNED) AS attachment0, CAST(attachment1 AS UNSIGNED) AS attachment1, \
     CAST(attachment2 AS UNSIGNED) AS attachment2, CAST(attachment3 AS UNSIGNED) AS attachment3, \
     CAST(attachment4 AS UNSIGNED) AS attachment4, CAST(attachment5 AS UNSIGNED) AS attachment5, \
     CAST(attachment6 AS UNSIGNED) AS attachment6, CAST(attachment7 AS UNSIGNED) AS attachment7, \
     CAST(attachment8 AS UNSIGNED) AS attachment8, CAST(attachment9 AS UNSIGNED) AS attachment9 \
     FROM mails";

const MAIL_REPLACE: &str = "REPLACE INTO mails(\
     `id`,`type`,`status`,`title`,`text`,`sender_id`,`sender_name`,\
     `attachment_count`,`receiver_id`,`receiver_name`,`open_date`,`send_date`,`received_date`,\
     `returned`,`extra`,`money_amount_1`,`money_amount_2`,`money_amount_3`,\
     `attachment0`,`attachment1`,`attachment2`,`attachment3`,`attachment4`,`attachment5`,\
     `attachment6`,`attachment7`,`attachment8`,`attachment9`\
     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

/// `GetNewMailId()` on a locked store.
pub(super) fn new_mail_id(server: &GameServer, st: &mut MailState) -> u32 {
    let id = match server.ids.mail.get_next_id() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("{e}");
            0
        }
    };
    st.clear_deletion(i64::from(id));
    id
}

/// `DeleteMail(long id)` on a locked store.
pub fn delete_mail_in(server: &GameServer, st: &mut MailState, id: i64) -> bool {
    if st.all.remove(id).is_none() {
        return false;
    }
    if !st.deleted.contains(&id) {
        st.deleted.push(id);
        st.deletion_versions.insert(id, Arc::new(()));
    }
    server.ids.mail.release_id(id as u32);
    server.systems.economy.mail.forget_kept_warning(id);
    true
}

/// `ExtractExtraForHouse(extra, out zoneGroupId, out houseId)` -> `(zoneGroupId, houseId)`.
pub fn extract_extra_for_house(extra: i64) -> (u16, u32) {
    let house_id = (extra & 0xFFFF_FFFF) as u32;
    let zone_group_id = ((extra >> 48) & 0xFFFF) as u16;
    (zone_group_id, house_id)
}

/// Schedules the `MailDeliveryTask` (30 s, then every 5 s).
pub fn start_delivery_task(server: &Arc<GameServer>) {
    let weak = Arc::downgrade(server);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tick.tick().await;
            let Some(server) = weak.upgrade() else { break };
            let mut txn = ItemTxn::begin(&server);
            check_all_mail_timings(&mut txn);
        }
    });
}
