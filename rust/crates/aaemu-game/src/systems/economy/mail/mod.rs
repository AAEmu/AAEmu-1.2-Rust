//! Port of `Models/Game/Mails/*` (`BaseMail`, `MailHeader`, `MailBody`, `CountUnreadMail`
//! helpers, the enums) and `Core/Managers/MailManager` (mail store, `Send`, delivery,
//! `PayChargeMoney`, house tax mails, quest reward mails, MySQL load / save).
//!
//! Mails are kept in a [`DotNetDict`] (the C# `Dictionary<long, BaseMail>` iteration order,
//! including the slot reuse after removals). Attachments are item ids: the item objects stay in
//! whatever container holds them (normally the sender's / receiver's `SlotType.Mail`
//! container, or a loose item in the `ItemManager`), exactly like the C# `Body.Attachments`
//! references.
//!
//! Lock order: economy operation lock ([`ItemTxn`]) -> mail store -> character data / item
//! containers (leaf). Every mutating mail operation runs inside an [`ItemTxn`]; packets are
//! queued in `txn.env.fx` so they stay ordered with the item task packets.

use std::collections::HashMap;

use aaemu_commons::{Marshal, PacketStream};
use chrono::Utc;

use crate::models::char_data::CountUnreadMail;
use crate::models::container::slot_type;
use crate::models::item::{Item, UnixTime};
use crate::models::item_task::{task_type, ItemFxList};
use crate::server::GameServer;

use self::manager::new_mail_id;
use super::fx;
use super::inventory::ItemTxn;
use super::mail_character;
use super::mail_packets::{SCGotMailPacket, SCMailReceiverOpenedPacket, SCMailRemovedPacket};
use super::mail_template;

mod attachments;
mod delivery;
mod housing_tax;
mod manager;

pub use self::attachments::{
    add_or_move_existing_item, delete_mail_trash, mail_container, mutate_item, renumber_slots,
};
pub(super) use self::delivery::expire_mail_at;
#[cfg(test)]
use self::delivery::return_to_sender_at;
pub use self::delivery::{
    check_all_mail_timings, check_all_mail_timings_at, count_unread_mail_for,
    get_current_mail_list, get_current_mail_list_for, notify_delete_mail_by_name_if_online,
    notify_new_mail_by_name_if_online, return_to_sender,
};
pub use self::housing_tax::{delete_house_mails, pay_charge_money, pay_housing_tax};
pub(crate) use self::manager::MailSaveBatch;
pub use self::manager::{
    delete_mail_in, extract_extra_for_house, start_delivery_task, MailHooks, MailHousingHooks,
    MailManager, MailRow, MailState,
};
#[cfg(test)]
use std::sync::{Arc, Mutex};

#[cfg(test)]
use super::inventory::Inv;

/// `MailType`.
pub mod mail_type {
    pub const INVALID_MAIL_TYPE: u8 = 0;
    pub const NORMAL: u8 = 1;
    pub const EXPRESS: u8 = 2;
    pub const REGISTERED: u8 = 3;
    pub const SPAM: u8 = 4;
    pub const ADMIN: u8 = 5;
    pub const BILLING: u8 = 6;
    pub const BALANCE_RECEIPT: u8 = 7;
    pub const TAXRATE_CHANGED: u8 = 8;
    pub const CHARGED: u8 = 9;
    pub const PROMOTION: u8 = 10;
    pub const MIA_SEND: u8 = 11;
    pub const MIA_RECV: u8 = 12;
    pub const AUC_OFF_CANCEL: u8 = 13;
    pub const AUC_OFF_SUCCESS: u8 = 14;
    pub const AUC_OFF_FAIL: u8 = 15;
    pub const AUC_BID_WIN: u8 = 16;
    pub const AUC_BID_FAIL: u8 = 17;
    pub const SYS_EXPRESS: u8 = 18;
    pub const SYS_SELL_BACKPACK: u8 = 19;
    pub const DEMOLISH_WITH_PENALTY: u8 = 20;
    pub const HOUSING_SALE: u8 = 21;
    pub const USER_BILL: u8 = 22;
    pub const NATION_TAX_RATE: u8 = 23;
    pub const NATION_TAX_RECEIPT: u8 = 24;
    pub const NATION_REQUEST_FRIEND: u8 = 25;
    pub const KICKED_FROM_BEGINNER_EXPED: u8 = 26;
    pub const MOBILIZATION_GIVE_ITEM: u8 = 27;
    pub const HERO_CANDIDATE_ALARM: u8 = 28;
    pub const HERO_ELECTION_ITEM: u8 = 29;
    pub const HOUSING_REBUILD: u8 = 30;
    pub const DEMOLISH: u8 = 31;
    pub const SIEGE_WIN_GIVE_ITEM: u8 = 32;
    pub const SIEGE_LOSE_GIVE_ITEM: u8 = 33;
    pub const TAX_IN_KIND_RECEIPT: u8 = 34;
    pub const NATION_RELATION_FRIEND: u8 = 35;
    pub const NATION_RELATION_FRIEND_SUCCESS: u8 = 36;
    pub const NATION_RELATION_FRIEND_REJECT: u8 = 37;
    pub const NATION_RELATION_FRIEND_REJECT_ITEM: u8 = 38;
    pub const NATION_RELATION_HOSTILE: u8 = 39;
    pub const NATION_RELATION_HOSTILE_SUCCESS: u8 = 40;
    pub const NATION_RELATION_HOSTILE_REJECT: u8 = 41;
    pub const SIEGE_AUCTION_START: u8 = 42;
    pub const EXPEDITION_IMMIGRATION_REJECT: u8 = 43;
    pub const RESIDENT_BALANCE: u8 = 44;
}

/// `MailStatus`.
pub mod mail_status {
    pub const UNREAD: u8 = 0;
    pub const READ: u8 = 1;
    pub const UNPAID: u8 = 2;
}

/// `MailResult`.
pub mod mail_result {
    pub const SUCCESS: u8 = 0;
    pub const INSUFFICIENT_COINS: u8 = 1;
    pub const CAN_NOT_BE_MAILED: u8 = 2;
    pub const INVALID_SLOT: u8 = 3;
    pub const CAN_NOT_FIND_MAIL: u8 = 4;
    pub const NO_ATTACHED_MONEY: u8 = 5;
    pub const NO_ATTACHED_ITEMS: u8 = 6;
    pub const INSUFFICIENT_COINS_2: u8 = 7;
    pub const RETURNS_NOT_ALLOWED: u8 = 8;
    pub const SUBJECT_LENGTH_LIMITED: u8 = 9;
    pub const TEXT_LENGTH_LIMITED: u8 = 10;
    pub const MAIL_NOT_INITIALIZED: u8 = 11;
    pub const INVALID_LETTER_FORMAT: u8 = 12;
    pub const INCORRECT_ITEM_INFORMATION: u8 = 13;
    pub const UNABLE_TO_ACCESS_SELECTED_MAIL: u8 = 14;
    pub const UNABLE_TO_FIND_RECIPIENT: u8 = 15;
    pub const NO_MAILBOX_NEARBY: u8 = 16;
    pub const INSUFFICIENT_COINS_3: u8 = 17;
    pub const FEATURE_UNDER_MAINTENANCE: u8 = 18;
    pub const BOUND_ITEM: u8 = 19;
    pub const NOT_AVAILABLE_IN_FREE_TRIAL: u8 = 20;
    pub const NOT_AVAILABLE_FOR_FREE_ACCOUNTS: u8 = 21;
    pub const LEVEL_TOO_LOW: u8 = 22;
    pub const YOU_MUST_PAY_FIRST: u8 = 23;
    pub const CHARACTERS_TRANSFERRING_SERVERS: u8 = 24;
    pub const SECONDARY_PASSWORD_REQUIRED: u8 = 25;
    pub const ITEM_LOCKED: u8 = 26;
    pub const CANNOT_PERFORM_SENSITIVE_ACTIONS: u8 = 27;
    pub const SENDING_MAILS_TOO_FREQUENT: u8 = 28;
    pub const SENT_SAME_MAIL_TOO_MANY_TIMES: u8 = 29;
    pub const MAIL_ERROR_OCCURRED: u8 = 30;
}

/// `CommercialMailTypes` (values of [`mail_type`]).
pub mod commercial_mail_types {
    use super::mail_type;
    pub const CHARGED: u8 = mail_type::CHARGED;
    pub const MIA_RECV: u8 = mail_type::MIA_RECV;
    pub const MIA_SEND: u8 = mail_type::MIA_SEND;
    pub const PROMOTION: u8 = mail_type::PROMOTION;
}

/// `MiaMailTypes` (values of [`mail_type`]).
pub mod mia_mail_types {
    use super::mail_type;
    pub const MIA_RECV: u8 = mail_type::MIA_RECV;
    pub const MIA_SEND: u8 = mail_type::MIA_SEND;
}

/// `MailBody.MaxMailAttachments`.
pub const MAX_MAIL_ATTACHMENTS: usize = 10;

/// `MailManager.CostNormal` and friends (static, settable in C#; constants here).
pub const COST_NORMAL: i32 = 50;
pub const COST_NORMAL_ATTACHMENT: i32 = 30;
pub const COST_EXPRESS: i32 = 100;
pub const COST_EXPRESS_ATTACHMENT: i32 = 80;
pub const COST_FREE_ATTACHMENT_COUNT: i32 = 1;
/// `MailManager.NormalMailDelay` (30 minutes), in seconds.
pub const NORMAL_MAIL_DELAY_SECS: i64 = 30 * 60;
/// The 1.2 client's mail retention windows (`mail_guide`, `ui_texts` 4465): normal mail is
/// kept 5 days after it was read or 30 days unread; system mail 5 days after it was read or
/// 90 days unread. `Configurations/Mail.json` / [`crate::systems::economy::config::MailConfig`]
/// override these; the values here are the client defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailRetention {
    /// Read normal (player) mail.
    pub read_secs: i64,
    /// Unread normal (player) mail, and any mail whose attachments are still unclaimed.
    pub unread_secs: i64,
    /// Read system mail.
    pub system_read_secs: i64,
    /// Unread system mail.
    pub system_unread_secs: i64,
    /// Strict `mail_guide` deletion for system mail with unclaimed attachments. Default false:
    /// system mail has no player sender to return the escrow to, so it is kept until claimed.
    pub delete_system_mail_with_attachments: bool,
    /// First start that installed the retention windows (unix seconds). Mail that existed then
    /// gets `deadline = max(rule, this + window)`; 0 disables the upgrade grace.
    pub grace_start_unix: i64,
}

/// Seconds in one day.
pub const SECONDS_PER_DAY: i64 = 24 * 3600;

/// Minimum character level to send player mail. The 1.2 client receives
/// `SCLevelRestrictionConfigPacket` with `mail = 10` (`packets/proxy.rs`, the same hard-coded
/// value as the C#); the 10.x server's `LevelRestrictions.MailLevel` default is also 10.
pub const MAIL_MIN_SEND_LEVEL: u8 = 10;

/// `mail_result::SENT_SAME_MAIL_TOO_MANY_TIMES`: `MAIL_REPETITION` (`ui_texts` 6315) says the
/// same mail was sent "3 or more times in a row". Three consecutive identical body texts are
/// allowed; the fourth is refused. The exact boundary is INFERRED from the text, and
/// `MAIL_COOL_TIME` (`ui_texts` 6314) names no interval at all, so that check is not enforced.
pub const MAIL_REPEAT_LIMIT: u32 = 3;

/// The last player-to-player mail sent in a row (`MAIL_REPETITION`). The key is the receiver,
/// title and body text, so item-only mails with an empty body never match each other, and the
/// same text sent to different characters is not "the same mail". Empty-body sends still reset
/// the streak (they are different content), they just cannot be refused by this check.
#[derive(Debug, Clone, Default)]
pub struct MailSendHistory {
    pub receiver_id: u32,
    pub title: String,
    pub text: String,
    pub repeats: u32,
}

impl Default for MailRetention {
    fn default() -> Self {
        Self {
            read_secs: 5 * SECONDS_PER_DAY,
            unread_secs: 30 * SECONDS_PER_DAY,
            system_read_secs: 5 * SECONDS_PER_DAY,
            system_unread_secs: 90 * SECONDS_PER_DAY,
            delete_system_mail_with_attachments: false,
            grace_start_unix: 0,
        }
    }
}

impl MailRetention {
    /// The configured windows. Negative day counts are clamped to 0 (expire immediately).
    pub fn from_config(c: &super::config::MailConfig) -> Self {
        fn days(days: i64) -> i64 {
            days.max(0).saturating_mul(SECONDS_PER_DAY)
        }
        Self {
            read_secs: days(c.read_retention_days),
            unread_secs: days(c.unread_retention_days),
            system_read_secs: days(c.system_read_retention_days),
            system_unread_secs: days(c.system_unread_retention_days),
            delete_system_mail_with_attachments: c.delete_system_mail_with_attachments,
            grace_start_unix: c.retention_grace_start_unix.max(0),
        }
    }
}

/// `AppConfiguration.Specialty.TradePackMailDelayInMinutes` default. The 1.2 client says the
/// trade-pack sale price, with 5 % interest, arrives by mail after 22 hours
/// (`SOLD_BACKPACK_REFUND_SENT_BY_MAIL`, `ui_texts` 4743; `sell_backpack_content`, 6195). The
/// C# default was 480 minutes (8 hours).
pub const DEFAULT_TRADE_PACK_MAIL_DELAY_MINUTES: f64 = 22.0 * 60.0;

/// `Item.Coins`, `Item.TaxCertificate`, `Item.BoundTaxCertificate`.
pub const ITEM_COINS: u32 = 500;
pub const ITEM_TAX_CERTIFICATE: u32 = 31891;
pub const ITEM_BOUND_TAX_CERTIFICATE: u32 = 31892;
/// `ActabilityType.Commerce`.
pub const ACTABILITY_COMMERCE: i32 = 31;

/// `DateTime.UtcNow` (unix seconds).
pub fn now() -> UnixTime {
    Utc::now().timestamp()
}

// ---------------------------------------------------------------------------------------------
// CountUnreadMail
// ---------------------------------------------------------------------------------------------

impl CountUnreadMail {
    /// `ResetReceived()`.
    pub fn reset_received(&mut self) {
        self.received = 0;
        self.mia_received = 0;
        self.commercial_received = 0;
    }

    /// `UpdateReceived(mailType, amount)`.
    pub fn update_received(&mut self, mail_type: u8, amount: i32) {
        if mail_type == mail_type::CHARGED || mail_type == mail_type::PROMOTION {
            self.commercial_received += amount;
        } else if mail_type == mail_type::MIA_RECV {
            self.mia_received += amount;
        } else {
            self.received += amount;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// BaseMail / MailHeader / MailBody
// ---------------------------------------------------------------------------------------------

/// The stored members of `MailHeader` (the others are read from the parent [`BaseMail`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailHeader {
    pub status: u8,
    pub sender_id: u32,
    pub sender_name: String,
    pub attachments: u8,
    pub receiver_id: u32,
    pub returned: bool,
    pub extra: i64,
}

/// The stored members of `MailBody`. `attachments` are item ids (`List<Item>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailBody {
    pub text: String,
    pub copper_coins: i32,
    pub billing_amount: i32,
    pub money_amount2: i32,
    pub send_date: UnixTime,
    pub recv_date: UnixTime,
    pub attachments: Vec<u64>,
}

/// `BaseMail`. The C# setters of `Id`, `MailType`, `Title`, `ReceiverName`, `OpenDate` and of
/// the body's `Text`, money and dates set `IsDirty`; the callers here set [`BaseMail::is_dirty`]
/// where the C# goes through one of those setters (header fields do not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseMail {
    pub id: i64,
    pub mail_type: u8,
    pub title: String,
    pub receiver_name: String,
    pub open_date: UnixTime,
    pub header: MailHeader,
    pub body: MailBody,
    /// Local helper: the receiver was notified.
    pub is_delivered: bool,
    pub is_dirty: bool,
}

impl Default for BaseMail {
    /// `new BaseMail()` (the constructor's `Header = ...` / `Body = ...` set `IsDirty`).
    fn default() -> Self {
        Self {
            id: 0,
            mail_type: mail_type::INVALID_MAIL_TYPE,
            title: String::new(),
            receiver_name: String::new(),
            open_date: 0,
            header: MailHeader::default(),
            body: MailBody::default(),
            is_delivered: false,
            is_dirty: true,
        }
    }
}

impl BaseMail {
    pub fn new() -> Self {
        Self::default()
    }

    /// `GetTotalAttachmentCount()`.
    pub fn get_total_attachment_count(&self) -> u8 {
        let mut res = self.body.attachments.len() as u8;
        if self.body.copper_coins != 0 {
            res = res.wrapping_add(1);
        }
        if self.body.billing_amount != 0 {
            res = res.wrapping_add(1);
        }
        if self.body.money_amount2 != 0 {
            res = res.wrapping_add(1);
        }
        res
    }

    /// `AttachMoney(copperCoinsAmount, money1Amount, money2Amount)`: does not take the money from
    /// anyone.
    pub fn attach_money(&mut self, copper: i32, money1: i32, money2: i32) {
        self.body.copper_coins = copper;
        self.body.billing_amount = money1;
        self.body.money_amount2 = money2;
        self.is_dirty = true;
        self.header.attachments = self.get_total_attachment_count();
    }

    /// The sent copy belongs to a different character. Self-addressed mail is
    /// listed in the inbox only, and system mail has no character sender.
    fn sent_copy_owner(&self) -> Option<u32> {
        let sender = self.header.sender_id;
        (sender != 0 && sender != self.header.receiver_id).then_some(sender)
    }

    /// Record a first read and queue the sender's live receipt.
    pub(super) fn mark_received_read(&mut self, fx: &mut ItemFxList, time: UnixTime) -> bool {
        if self.header.status != mail_status::UNREAD {
            return false;
        }
        self.header.status = mail_status::READ;
        if self.open_date == 0 {
            self.open_date = time;
        }
        self.is_dirty = true;
        if let Some(sender) = self.sent_copy_owner() {
            fx.send(
                sender,
                SCMailReceiverOpenedPacket {
                    mail_id: self.id,
                    open_date: self.open_date,
                },
            );
        }
        true
    }

    pub(super) fn notify_sent_copy_removed(&self, fx: &mut ItemFxList) {
        if let Some(sender) = self.sent_copy_owner() {
            fx.send(
                sender,
                SCMailRemovedPacket {
                    is_sent: true,
                    mail_id: self.id,
                },
            );
        }
    }

    /// `CanReturnMail()`.
    pub fn can_return_mail(&self) -> bool {
        !self.header.returned
            && self.header.status == mail_status::UNREAD
            && self.header.sender_id != self.header.receiver_id
            && self.header.sender_id > 0
            && (self.mail_type == mail_type::NORMAL || self.mail_type == mail_type::EXPRESS)
    }

    /// The expiry return path: an expired mail with unclaimed attachments goes back to its
    /// sender even after it was read (the guide promises the attachments are returned). A
    /// REGISTERED letter, a self-addressed one or a letter already returned once stays put;
    /// the caller keeps it instead of destroying the escrow.
    pub fn can_return_expired_mail(&self) -> bool {
        self.has_unclaimed_attachments()
            && !self.header.returned
            && self.header.sender_id != self.header.receiver_id
            && self.header.sender_id > 0
            && (self.mail_type == mail_type::NORMAL || self.mail_type == mail_type::EXPRESS)
    }

    /// Whether this mail keeps its own timer. The commercial mailbox (cash shop / ArcheBill)
    /// has a separate guide (`comercial_mail_guide`, `ui_texts` 4464: read mail is deleted at
    /// once, unread mail is kept), so its types never expire through the ordinary mailbox timer.
    pub fn has_retention(&self) -> bool {
        !matches!(
            self.mail_type,
            mail_type::INVALID_MAIL_TYPE
                | mail_type::CHARGED
                | mail_type::PROMOTION
                | mail_type::MIA_SEND
                | mail_type::MIA_RECV
        )
    }

    /// Whether this mail is ordinary player mail (`mail_guide` "normal mail") rather than the
    /// system mail bucket. Types 1-3 are the player-to-player letters; every other type that
    /// reaches the ordinary mailbox is system mail.
    pub fn is_normal_mail(&self) -> bool {
        matches!(
            self.mail_type,
            mail_type::NORMAL | mail_type::EXPRESS | mail_type::REGISTERED | mail_type::SPAM
        )
    }

    /// `mail_guide` (`ui_texts` 4465): a mail whose attachments are still unclaimed keeps the
    /// unread deadline, whether or not it was opened.
    fn has_unclaimed_attachments(&self) -> bool {
        self.get_total_attachment_count() > 0
    }

    /// Mail expiry. Normal mail: 5 days after it was read, or 30 days unread. System mail:
    /// 5 days after it was read, or 90 days unread. Mail with unclaimed attachments keeps the
    /// unread deadline. System mail with unclaimed attachments is kept until claimed: there is
    /// no player sender to return the escrow to, and destroying it would lose the items,
    /// coins or billing. The strict `mail_guide` deletion is an explicit operator option.
    /// The read clock is `open_date` (set when the mail is opened or when an attachment is
    /// claimed); mails stored before those writes have no read time and use the receipt date,
    /// like the 10.x server. Mail that existed before the retention update gets one full window
    /// from the first start that installed it (the upgrade grace).
    fn is_expired_at(&self, time: UnixTime, retention: &MailRetention) -> bool {
        if !self.has_retention() || self.header.receiver_id == 0 || self.body.recv_date > time {
            return false;
        }
        let unclaimed = self.has_unclaimed_attachments();
        if unclaimed && !self.is_normal_mail() && !retention.delete_system_mail_with_attachments {
            return false;
        }
        let (read_secs, unread_secs) = if self.is_normal_mail() {
            (retention.read_secs, retention.unread_secs)
        } else {
            (retention.system_read_secs, retention.system_unread_secs)
        };
        let (window, base) = if self.header.status == mail_status::READ && !unclaimed {
            let read_at = if self.open_date > 0 {
                self.open_date
            } else {
                self.body.recv_date
            };
            (read_secs, read_at)
        } else {
            (unread_secs, self.body.recv_date)
        };
        let mut deadline = base.saturating_add(window);
        if retention.grace_start_unix > 0 && self.body.recv_date < retention.grace_start_unix {
            deadline = deadline.max(retention.grace_start_unix.saturating_add(window));
        }
        time >= deadline
    }

    /// `MailHeader` as written on the wire.
    pub fn header_wire(&self) -> MailHeaderWire {
        MailHeaderWire {
            mail_id: self.id,
            mail_type: self.mail_type,
            status: self.header.status,
            title: self.title.clone(),
            sender_name: self.header.sender_name.clone(),
            attachments: self.header.attachments,
            receiver_name: self.receiver_name.clone(),
            open_date: self.open_date,
            returned: self.header.returned,
            extra: self.header.extra,
        }
    }

    /// `MailBody` as written on the wire (attachment items resolved through the item store).
    pub fn body_wire(&self, server: &GameServer) -> MailBodyWire {
        let items = (0..MAX_MAIL_ATTACHMENTS)
            .map(|i| {
                self.body
                    .attachments
                    .get(i)
                    .and_then(|id| server.items.item_by_id(*id))
            })
            .collect();
        MailBodyWire {
            mail_id: self.id,
            mail_type: self.mail_type,
            receiver_name: self.receiver_name.clone(),
            title: self.title.clone(),
            text: self.body.text.clone(),
            copper_coins: self.body.copper_coins,
            billing_amount: self.body.billing_amount,
            money_amount2: self.body.money_amount2,
            send_date: self.body.send_date,
            recv_date: self.body.recv_date,
            open_date: self.open_date,
            items,
        }
    }
}

/// A `MailHeader` snapshot (`MailHeader.Write`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailHeaderWire {
    pub mail_id: i64,
    pub mail_type: u8,
    pub status: u8,
    pub title: String,
    pub sender_name: String,
    pub attachments: u8,
    pub receiver_name: String,
    pub open_date: UnixTime,
    pub returned: bool,
    pub extra: i64,
}

impl Marshal for MailHeaderWire {
    fn write(&self, s: &mut PacketStream) {
        s.write_i64(self.mail_id)
            .write_u8(self.mail_type)
            .write_u8(self.status)
            .write_str(&self.title)
            .write_str(&self.sender_name)
            .write_u8(self.attachments)
            .write_str(&self.receiver_name)
            .write_i64(self.open_date)
            .write_bool(self.returned)
            .write_i64(self.extra);
    }
}

/// A `MailBody` snapshot (`MailBody.Write`); `items` has [`MAX_MAIL_ATTACHMENTS`] entries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MailBodyWire {
    pub mail_id: i64,
    pub mail_type: u8,
    pub receiver_name: String,
    pub title: String,
    pub text: String,
    pub copper_coins: i32,
    pub billing_amount: i32,
    pub money_amount2: i32,
    pub send_date: UnixTime,
    pub recv_date: UnixTime,
    pub open_date: UnixTime,
    pub items: Vec<Option<Item>>,
}

impl Marshal for MailBodyWire {
    fn write(&self, s: &mut PacketStream) {
        s.write_i64(self.mail_id)
            .write_u8(self.mail_type)
            .write_str(&self.receiver_name)
            .write_str(&self.title)
            .write_str(&self.text)
            .write_i32(self.copper_coins)
            .write_i32(self.billing_amount)
            .write_i32(self.money_amount2)
            .write_i64(self.send_date)
            .write_i64(self.recv_date)
            .write_i64(self.open_date);
        for i in 0..MAX_MAIL_ATTACHMENTS {
            match self.items.get(i).and_then(Option::as_ref) {
                Some(item) => item.write(s),
                None => {
                    s.write_i32(0);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// DotNetDict
// ---------------------------------------------------------------------------------------------

/// A `Dictionary<long, V>` with the .NET enumeration order: entries live in an array, removed
/// entries go on a LIFO free list that the next `Add` reuses.
#[derive(Debug, Clone)]
pub struct DotNetDict<V> {
    entries: Vec<Option<(i64, V)>>,
    free: Vec<usize>,
    index: HashMap<i64, usize>,
}

impl<V> Default for DotNetDict<V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            free: Vec::new(),
            index: HashMap::new(),
        }
    }
}

impl<V> DotNetDict<V> {
    /// `Add` (`false` where the C# throws for a duplicate key).
    pub fn add(&mut self, key: i64, value: V) -> bool {
        if self.index.contains_key(&key) {
            return false;
        }
        let idx = match self.free.pop() {
            Some(i) => {
                self.entries[i] = Some((key, value));
                i
            }
            None => {
                self.entries.push(Some((key, value)));
                self.entries.len() - 1
            }
        };
        self.index.insert(key, idx);
        true
    }

    pub fn remove(&mut self, key: i64) -> Option<V> {
        let idx = self.index.remove(&key)?;
        let (_, v) = self.entries[idx].take()?;
        self.free.push(idx);
        Some(v)
    }

    pub fn get(&self, key: i64) -> Option<&V> {
        let idx = *self.index.get(&key)?;
        self.entries[idx].as_ref().map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: i64) -> Option<&mut V> {
        let idx = *self.index.get(&key)?;
        self.entries[idx].as_mut().map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: i64) -> bool {
        self.index.contains_key(&key)
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Enumeration in .NET order.
    pub fn iter(&self) -> impl Iterator<Item = (i64, &V)> {
        self.entries
            .iter()
            .filter_map(|e| e.as_ref().map(|(k, v)| (*k, v)))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (i64, &mut V)> {
        self.entries
            .iter_mut()
            .filter_map(|e| e.as_mut().map(|(k, v)| (*k, v)))
    }

    /// The keys in .NET order.
    pub fn keys(&self) -> Vec<i64> {
        self.iter().map(|(k, _)| k).collect()
    }
}

// ---------------------------------------------------------------------------------------------
// MailManager operations
// ---------------------------------------------------------------------------------------------

/// `BaseMail.Send()`: recount the attachments, renumber their slots, `MailManager.Send`.
pub fn send(txn: &mut ItemTxn<'_>, mail: BaseMail) -> bool {
    send_mail(txn, mail).is_some()
}

/// [`send`] returning the id of the stored mail.
pub fn send_mail(txn: &mut ItemTxn<'_>, mut mail: BaseMail) -> Option<i64> {
    mail.header.attachments = mail.get_total_attachment_count();
    renumber_slots(txn.server, &mail);
    manager_send(txn, mail)
}

/// `MailManager.Send(mail)`: verifies the receiver, assigns an id, stores the mail and notifies
/// the receiver when online. Returns the mail id (`None` = `false`).
pub(crate) fn store_quest_rewards_locked(
    txn: &mut ItemTxn<'_>,
    c: &mut crate::models::character::Character,
    mut mails: Vec<BaseMail>,
) -> bool {
    let mut st = txn.server.systems.economy.mail.state();
    for m in &mut mails {
        m.id = i64::from(new_mail_id(txn.server, &mut st));
        if m.id <= 0 {
            for allocated in &mails {
                if allocated.id > 0 {
                    txn.server.ids.mail.release_id(allocated.id as u32);
                }
            }
            drop(st);
            if let Some((_, container)) = c
                .inventory
                .containers
                .iter()
                .find(|(kind, _)| *kind == slot_type::MAIL)
            {
                let mut container = container.lock().expect("container poisoned");
                for m in mails {
                    for id in m.body.attachments {
                        container.remove_item_task(task_type::INVALID, id, true, &mut txn.env);
                    }
                }
            }
            return false;
        }
    }
    for mut m in mails {
        m.is_dirty = true;
        m.is_delivered = true;
        c.unread_mail.update_received(m.mail_type, 1);
        txn.env.fx.send(
            c.id,
            SCGotMailPacket {
                mail: m.header_wire(),
                count: c.unread_mail,
                is_cancel: false,
                body: None,
            },
        );
        renumber_slots(txn.server, &m);
        st.all.add(m.id, m);
    }
    true
}

pub fn manager_send(txn: &mut ItemTxn<'_>, mut mail: BaseMail) -> Option<i64> {
    let server = txn.server;
    // Verify Receiver
    let target_name = server.names.get_character_name(mail.header.receiver_id);
    let target_id = server.names.get_character_id(&mail.receiver_name);
    let names_match = match &target_name {
        Some(n) => n.to_lowercase() == mail.receiver_name.to_lowercase(),
        None => false,
    };
    if !names_match {
        tracing::debug!(
            "Send() - Failed to verify receiver name {:?} != {}",
            target_name,
            mail.receiver_name
        );
        return None; // Name mismatch
    }
    if target_id != mail.header.receiver_id {
        tracing::debug!(
            "Send() - Failed to verify receiver id {target_id} != {}",
            mail.header.receiver_id
        );
        return None; // Id mismatch
    }
    let target_name = target_name.unwrap_or_default();
    let mail_manager = &server.systems.economy.mail;
    let mut st = mail_manager.state();
    // Assign a Id if we didn't have one yet
    if mail.id <= 0 {
        tracing::trace!("Send() - Assign new mail Id");
        mail.id = i64::from(new_mail_id(server, &mut st));
        mail.is_dirty = true;
    }
    let id = mail.id;
    if id <= 0 || st.all.contains_key(id) || !relocate_attachments(txn, &mail) {
        return None;
    }
    if !st.all.add(id, mail) {
        // C#: Dictionary.Add throws ArgumentException.
        tracing::error!("Send() - mail {id} is already stored");
        return None;
    }
    let m = st.all.get_mut(id).expect("just added");
    notify_new_mail_by_name_if_online(server, &mut txn.env.fx, m, &target_name);
    Some(id)
}

/// Put every attached item in the receiver's persistent mail escrow. This also repairs
/// legacy rows whose item container still belonged to the sender or auction seller.
fn relocate_attachments(txn: &mut ItemTxn<'_>, mail: &BaseMail) -> bool {
    let mut seen = std::collections::HashSet::new();
    if mail.body.attachments.len() > MAX_MAIL_ATTACHMENTS
        || mail
            .body
            .attachments
            .iter()
            .any(|id| !seen.insert(*id) || txn.server.items.item_by_id(*id).is_none())
    {
        return false;
    }
    if mail.body.attachments.is_empty() {
        return true;
    }
    let server = txn.server;
    let target = server
        .world
        .character_by_id(mail.header.receiver_id)
        .and_then(|p| mail_container(&p))
        .unwrap_or_else(|| {
            server.items.container_for_character(
                mail.header.receiver_id,
                slot_type::MAIL,
                0,
                &server.ids.container,
            )
        });
    for &id in &mail.body.attachments {
        if !add_or_move_existing_item(txn, &target, task_type::INVALID, id, -1) {
            return false;
        }
    }
    renumber_slots(server, mail);
    true
}

/// The money / item part `send_system_mail` callers commonly need: a ready-to-send system mail
/// (sender id 0) to `receiver_id` with `copper` attached and already-created loose `items`
/// (at most [`MAX_MAIL_ATTACHMENTS`]). Builds the same mail the C# callers build by hand
/// (`new BaseMail { ... }`, `AttachMoney`, `Body.Attachments.Add`) and `Send`s it.
#[allow(clippy::too_many_arguments)]
pub fn send_system_mail(
    txn: &mut ItemTxn<'_>,
    mail_type: u8,
    sender_name: &str,
    receiver_id: u32,
    title: &str,
    text: &str,
    copper: i32,
    items: Vec<Item>,
) -> bool {
    let server = txn.server;
    let Some(receiver_name) = server.names.get_character_name(receiver_id) else {
        for i in items {
            server.items.add_loose(i);
        }
        return false;
    };
    let mut mail = BaseMail {
        mail_type,
        title: title.to_string(),
        receiver_name: receiver_name.clone(),
        ..BaseMail::default()
    };
    mail.header.sender_id = 0;
    mail.header.sender_name = sender_name.to_string();
    mail.header.receiver_id = receiver_id;
    mail.body.text = text.to_string();
    let n = now();
    mail.body.send_date = n;
    mail.body.recv_date = n;
    mail.attach_money(copper, 0, 0);
    // Items go to the receiver's mail container when online, else stay loose.
    let target = server
        .world
        .character_by_name(&receiver_name)
        .and_then(|p| mail_container(&p));
    for mut item in items {
        item.owner_id = u64::from(receiver_id);
        item.slot_type = slot_type::MAIL;
        item.dirty = true;
        let id = item.id;
        match &target {
            Some(c) => {
                let res = c.lock().expect("container poisoned").add_item(
                    task_type::INVALID,
                    item,
                    -1,
                    &server.game_data.items,
                    &mut txn.env.fx,
                );
                if let Err(item) = res {
                    server.items.add_loose(item);
                }
            }
            None => server.items.add_loose(item),
        }
        mail.body.attachments.push(id);
    }
    send(txn, mail)
}

#[cfg(test)]
mod expiry_tests;
