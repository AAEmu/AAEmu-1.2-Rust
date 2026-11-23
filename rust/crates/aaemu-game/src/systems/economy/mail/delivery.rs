use crate::models::char_data::CountUnreadMail;
use crate::models::container::{expire_item, slot_type, MIN_DATE_TIME};
use crate::models::item::UnixTime;
use crate::models::item_task::{ItemFx, ItemFxList};
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::super::inventory::ItemTxn;
use super::super::mail_packets::{SCGotMailPacket, SCMailDeletedPacket, SCMailReturnedPacket};
use super::{mail_status, mail_type, now, relocate_attachments, BaseMail, MailRetention};

/// `NotifyNewMailByNameIfOnline(m, receiverName)`.
pub fn notify_new_mail_by_name_if_online(
    server: &GameServer,
    fx: &mut ItemFxList,
    m: &mut BaseMail,
    receiver_name: &str,
) -> bool {
    notify_new_mail_at(server, fx, m, receiver_name, now())
}

fn notify_new_mail_at(
    server: &GameServer,
    fx: &mut ItemFxList,
    m: &mut BaseMail,
    receiver_name: &str,
    time: UnixTime,
) -> bool {
    tracing::trace!("NotifyNewMailByNameIfOnline() - {receiver_name}");
    // If unread and ready to deliver
    if m.header.status != mail_status::READ && m.body.recv_date <= time && !m.is_delivered {
        if let Some(player) = server.world.character_by_name(receiver_name) {
            // Mia mail (types 11 MIA_SEND / 12 MIA_RECV) is the separate commercial mailbox in
            // the 1.2 client (`mailbox/comercialmail/*`, `X2MiaMail`, `unreadMiaMail`), fed by
            // the cash shop path, not by this notifier. The C# branch was empty as well.
            // Whether this packet should carry `unreadMiaMail` instead of the ordinary count is
            // capture-pending (case `MIA-MAIL-1`); the existing `update_received` routing
            // (MIA_RECV -> mia_received, everything else -> received) is therefore unchanged.
            let add_body = m.mail_type == mail_type::CHARGED;
            let count = {
                let mut c = player.data();
                c.unread_mail.update_received(m.mail_type, 1);
                c.unread_mail
            };
            fx.send(
                player.character_id,
                SCGotMailPacket {
                    mail: m.header_wire(),
                    count,
                    is_cancel: false,
                    body: add_body.then(|| m.body_wire(server)),
                },
            );
            m.is_delivered = true;
            return true;
        }
    }
    false
}

/// `NotifyDeleteMailByNameIfOnline(m, receiverName)`.
pub fn notify_delete_mail_by_name_if_online(
    server: &GameServer,
    fx: &mut ItemFxList,
    m: &BaseMail,
    receiver_name: &str,
) -> bool {
    tracing::trace!("NotifyDeleteMailByNameIfOnline() - {receiver_name}");
    let Some(player) = server.world.character_by_name(receiver_name) else {
        return false;
    };
    let count = {
        let mut c = player.data();
        if m.header.status != mail_status::READ {
            c.unread_mail.update_received(m.mail_type, -1);
        }
        c.unread_mail
    };
    fx.send(
        player.character_id,
        SCMailDeletedPacket {
            is_sent: false,
            mail_id: m.id,
            is_unread_mail_count_modified: true,
            count,
        },
    );
    true
}

/// `GetCurrentMailList(characterId)`: the ids of the delivered mails the character sent or
/// received. When the character is online its unread counters are recomputed and an
/// `SCGotMailPacket` is sent for every unread mail; every unread mail gets `IsDelivered`.
pub fn get_current_mail_list(txn: &mut ItemTxn<'_>, character_id: u32) -> Vec<i64> {
    let server = txn.server;
    let now = now();
    let retention = server.systems.economy.mail.retention();
    expire_due_mail_at(txn, now, &retention);
    // Try to grab the actual online Character object to send live updates
    let character = server.world.character_by_id(character_id);
    let mut st = server.systems.economy.mail.state();
    let ids: Vec<i64> = st
        .all
        .iter()
        .filter(|(_, m)| {
            m.body.recv_date <= now
                && (m.header.receiver_id == character_id || m.header.sender_id == character_id)
        })
        .map(|(k, _)| k)
        .collect();
    if let Some(p) = &character {
        p.data().unread_mail.reset_received();
    }
    for id in &ids {
        let m = st.all.get_mut(*id).expect("listed");
        if m.header.status != mail_status::READ && m.header.receiver_id == character_id {
            if let Some(p) = &character {
                let count = {
                    let mut c = p.data();
                    c.unread_mail.update_received(m.mail_type, 1);
                    c.unread_mail
                };
                let add_body = m.mail_type == mail_type::CHARGED;
                txn.env.fx.send(
                    p.character_id,
                    SCGotMailPacket {
                        mail: m.header_wire(),
                        count,
                        is_cancel: false,
                        body: add_body.then(|| m.body_wire(server)),
                    },
                );
            }
            m.is_delivered = true;
        }
    }
    ids
}

/// [`get_current_mail_list`] in its own operation (character select: `Character.Load` calls
/// it before the character is added to the world, so only the `IsDelivered` flags change).
pub fn get_current_mail_list_for(server: &GameServer, character_id: u32) -> Vec<i64> {
    let mut txn = ItemTxn::begin(server);
    get_current_mail_list(&mut txn, character_id)
}

/// The unread counters `get_current_mail_list` would set for the character if it were online:
/// every mail received by now, addressed to it and not read. Used at enter world, where the
/// character is not registered yet (the C# reported 0 there, so offline mail never lit the alert).
pub fn count_unread_mail_for(server: &GameServer, character_id: u32) -> CountUnreadMail {
    let now = now();
    let mut count = CountUnreadMail::default();
    count.reset_received();
    let st = server.systems.economy.mail.state();
    for (_, m) in st.all.iter() {
        if m.body.recv_date <= now
            && m.header.receiver_id == character_id
            && m.header.status != mail_status::READ
        {
            count.update_received(m.mail_type, 1);
        }
    }
    count
}

/// `CheckAllMailTimings()` (the `MailDeliveryTask`).
pub fn check_all_mail_timings(txn: &mut ItemTxn<'_>) {
    check_all_mail_timings_at(txn, now());
}

/// The timer and deterministic lifecycle tests share the same operation and clock value.
pub fn check_all_mail_timings_at(txn: &mut ItemTxn<'_>, time: UnixTime) {
    tracing::trace!("CheckAllMailTimings");
    // Expire before delivery: logging in after the deadline must not first announce stale mail.
    let server = txn.server;
    let retention = server.systems.economy.mail.retention();
    expire_due_mail_at(txn, time, &retention);
    let mut st = server.systems.economy.mail.state();
    let undelivered: Vec<i64> = st
        .all
        .iter()
        // Expired mail that survived `expire_due_mail_at` is kept on purpose (escrow it
        // cannot return): it is still delivered, or it would wait in the database forever.
        .filter(|(_, m)| m.body.recv_date <= time && !m.is_delivered)
        .map(|(k, _)| k)
        .collect();
    let mut delivered = 0;
    for id in &undelivered {
        let m = st.all.get_mut(*id).expect("listed");
        let name = m.receiver_name.clone();
        if notify_new_mail_at(server, &mut txn.env.fx, m, &name, time) {
            delivered += 1;
        }
    }
    if delivered > 0 {
        tracing::debug!("{delivered}/{} mail(s) delivered", undelivered.len());
    }
}

fn expire_due_mail_at(txn: &mut ItemTxn<'_>, time: UnixTime, retention: &MailRetention) {
    let ids: Vec<_> = txn
        .server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .filter(|(_, m)| m.is_expired_at(time, retention))
        .map(|(id, _)| id)
        .collect();
    for id in ids {
        expire_mail_at(txn, id, time);
    }
}

/// is_delivered is reconstructed from dates on restart, while a newly selected
/// character's counters start at zero. Recount after removal/return instead of
/// decrementing a counter that might never have included this letter.
fn refresh_received_count(
    server: &GameServer,
    player: &PlayerRef,
    time: UnixTime,
) -> CountUnreadMail {
    let mut received = CountUnreadMail::default();
    {
        let st = server.systems.economy.mail.state();
        for (_, m) in st.all.iter() {
            if m.header.receiver_id == player.character_id
                && m.is_delivered
                && m.body.recv_date <= time
                && m.header.status != mail_status::READ
            {
                received.update_received(m.mail_type, 1);
            }
        }
    }
    let mut c = player.data();
    c.unread_mail.received = received.received;
    c.unread_mail.mia_received = received.mia_received;
    c.unread_mail.commercial_received = received.commercial_received;
    c.unread_mail
}

/// `mail_guide` (`ui_texts` 4465) expiry action for a due mail: unread player mail, and any
/// player mail with unclaimed attachments, is returned to the sender; system mail with
/// unclaimed attachments is kept (there is no sender to return it to); mail whose escrow
/// cannot be returned (REGISTERED, deleted sender, already returned) is kept, never destroyed.
/// Only attachment-free mail is deleted, as the guide says.
///
/// Returns true when the mail left the receiver (returned or deleted) or a return is pending
/// on a damaged escrow, so packet operations must not claim/delete/return it between timer
/// ticks. Returns false when the mail stays valid (kept escrow or no expiry).
/// All mutations use the caller's ItemTxn, like sends, takes and financial save snapshots.
pub(in crate::systems::economy) fn expire_mail_at(
    txn: &mut ItemTxn<'_>,
    id: i64,
    time: UnixTime,
) -> bool {
    let server = txn.server;
    let retention = server.systems.economy.mail.retention();
    let Some(mail) = server.systems.economy.mail.get_mail_by_id(id) else {
        return false;
    };
    if !mail.is_expired_at(time, &retention) {
        return false;
    }
    let unclaimed = mail.has_unclaimed_attachments();
    // Unread player mail goes back to the sender (`CanReturnMail`), with or without
    // attachments, exactly as before.
    if mail.can_return_mail()
        && server
            .names
            .get_character_name(mail.header.sender_id)
            .is_some()
    {
        // A failed relocation leaves the original letter intact for retry, never deletes
        // escrow that was meant to be refunded, and never credits a wallet directly.
        if !return_to_sender_at(txn, id, time) {
            tracing::debug!(
                mail_id = id,
                "Expired mail return deferred: escrow needs repair"
            );
        }
        return true;
    }
    if unclaimed {
        if mail.is_normal_mail() {
            // Read player mail with unclaimed attachments: the guide returns the attachments
            // to the sender, so use the expiry return path even though the letter was opened.
            if mail.can_return_expired_mail()
                && server
                    .names
                    .get_character_name(mail.header.sender_id)
                    .is_some()
            {
                if !return_expired_to_sender_at(txn, id, time) {
                    tracing::debug!(
                        mail_id = id,
                        "Expired mail return deferred: escrow needs repair"
                    );
                }
                return true;
            }
            // REGISTERED mail, a deleted sender, a self-addressed letter or one already
            // returned once cannot be returned again. Keep the escrow: destroying it would
            // delete items/coins the player can still claim. The warning is logged once per
            // mail id (the expiry tick runs every 5 seconds).
            server
                .systems
                .economy
                .mail
                .warn_kept_once(id, mail.mail_type, mail.header.sender_id);
            return false;
        }
        // System mail has no player sender to return the escrow to. The default keeps it
        // until the receiver claims it; only the operator's strict `mail_guide` mode below
        // deletes it. `is_expired_at` already filters the default case, so this is reached
        // only with `DeleteSystemMailWithAttachments = true`.
        tracing::warn!(
            mail_id = id,
            mail_type = mail.mail_type,
            items = mail.body.attachments.len(),
            "Expired system mail with unclaimed attachments is deleted (strict config)"
        );
    }

    // Attachment-free mail (or system mail in the operator's strict mode) expires terminally:
    // the guide says the letter is deleted after its window. Mail with unclaimed attachments
    // never reaches the destroy loop below unless the operator opted into strict system-mail
    // deletion: player attachments are returned to the sender, system attachments are kept.
    for &item_id in &mail.body.attachments {
        let Some(mut item) = server.items.item_by_id(item_id) else {
            continue;
        };
        if item.owner_id != u64::from(mail.header.receiver_id) || item.slot_type != slot_type::MAIL
        {
            continue;
        }
        if let Some(container) = server.items.holding_container(item_id) {
            // Mail escrow expiry is unconditional, like item expiry; CanDestroy (for an
            // active summon item) must not orphan an attachment when the letter disappears.
            container
                .lock()
                .expect("container poisoned")
                .remove_item(item_id);
        } else {
            server.items.take_loose(item_id);
        }
        if item.expiration_online_minutes_left > 0.0
            || item.expiration_time > MIN_DATE_TIME
            || item.unpack_time > MIN_DATE_TIME
        {
            txn.env
                .fx
                .push(expire_item(&mut item).with_owner(mail.header.receiver_id));
        }
        txn.env.fx.push(ItemFx::ReleaseId(item_id));
    }
    server.systems.economy.mail.delete_mail(server, id);
    if let Some(receiver) = server.world.character_by_id(mail.header.receiver_id) {
        let modified = mail.is_delivered && mail.header.status != mail_status::READ;
        let count = refresh_received_count(server, &receiver, time);
        txn.env.fx.send(
            receiver.character_id,
            SCMailDeletedPacket {
                is_sent: false,
                mail_id: id,
                is_unread_mail_count_modified: modified,
                count,
            },
        );
    }
    mail.notify_sent_copy_removed(&mut txn.env.fx);
    true
}

/// Return the existing mail once, preserving its item IDs and attached money.
pub fn return_to_sender(txn: &mut ItemTxn<'_>, mail_id: i64) -> bool {
    return_to_sender_at(txn, mail_id, now())
}

pub(super) fn return_to_sender_at(txn: &mut ItemTxn<'_>, mail_id: i64, time: UnixTime) -> bool {
    return_to_sender_with(txn, mail_id, time, false)
}

/// Expiry return: also valid for a read player letter that still has unclaimed attachments
/// (`can_return_expired_mail`). The letter flips once and the escrow moves once.
fn return_expired_to_sender_at(txn: &mut ItemTxn<'_>, mail_id: i64, time: UnixTime) -> bool {
    return_to_sender_with(txn, mail_id, time, true)
}

fn return_to_sender_with(
    txn: &mut ItemTxn<'_>,
    mail_id: i64,
    time: UnixTime,
    expired: bool,
) -> bool {
    let server = txn.server;
    let mails = &server.systems.economy.mail;
    let Some(mail) = mails.get_mail_by_id(mail_id) else {
        return false;
    };
    let can_return = if expired {
        mail.can_return_expired_mail()
    } else {
        mail.can_return_mail()
    };
    if !can_return {
        return false;
    }
    let Some(original_sender) = server.names.get_character_name(mail.header.sender_id) else {
        return false;
    };
    if mail.body.attachments.iter().any(|id| {
        server.items.item_by_id(*id).is_none_or(|item| {
            item.owner_id != u64::from(mail.header.receiver_id) || item.slot_type != slot_type::MAIL
        })
    }) {
        return false;
    }
    let mut swapped = mail.clone();
    swapped.header.receiver_id = mail.header.sender_id;
    swapped.receiver_name = original_sender.clone();
    swapped.is_dirty = true;
    swapped.header.sender_id = mail.header.receiver_id;
    swapped.header.sender_name = mail.receiver_name.clone();
    swapped.header.returned = true;
    swapped.header.status = mail_status::UNREAD;
    swapped.open_date = 0;
    swapped.body.recv_date = time;
    swapped.is_delivered = false;
    swapped.header.attachments = swapped.get_total_attachment_count();
    // Move escrow before publishing the new owner or sending any success packet. The
    // mail containers do not stack attachments. Failure keeps the original row retryable.
    if !relocate_attachments(txn, &swapped) {
        relocate_attachments(txn, &mail);
        return false;
    }
    if let Some(r) = server.world.character_by_id(mail.header.receiver_id) {
        txn.env.fx.send(
            r.character_id,
            SCMailReturnedPacket {
                mail_id,
                mail: mail.header_wire(),
            },
        );
    }
    mails.with_mail_mut(mail_id, |m| *m = swapped);
    // The original sender receives the same id in their inbox. Remove the old
    // sent row before announcing the returned letter.
    mail.notify_sent_copy_removed(&mut txn.env.fx);
    if mail.is_delivered && mail.header.status == mail_status::UNREAD {
        if let Some(receiver) = server.world.character_by_id(mail.header.receiver_id) {
            refresh_received_count(server, &receiver, time);
            super::mail_character::send_unread_mail_count(&mut txn.env.fx, &receiver);
        }
    }
    mails.with_mail_mut(mail_id, |m| {
        notify_new_mail_at(server, &mut txn.env.fx, m, &original_sender, time)
    });
    true
}
