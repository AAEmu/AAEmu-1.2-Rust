use super::{
    error_message, faction, inventory, mail, mail_character, mail_status, mail_type, send_error,
    task_type, top_level_faction, BaseMail, DateTime, GameServer, HashSet, ItemTxn, Nation,
    PendingTreatyMail, PlayerRef, PoisonError, RelationExpiry, Result, SCChargeMoneyPaidPacket,
    SCFactionDeclareHostileResultPacket, SCFactionRelationExpiredPacket,
    SCFactionSetRelationStatePacket, SCMailDeletedPacket, SystemFaction, Utc,
};

// ---- Diplomacy (stage 3): declare hostility, friendly treaty mails, relation expiry ----

fn hours_to_secs(hours: u32) -> i64 {
    i64::from(hours) * 3600
}

fn utc(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// The online players of `faction`'s top-level side (a nation, or an alliance such as 148 /
/// 149 whose race factions and guilds belong to it). Duplicates are removed.
fn side_members(server: &GameServer, faction: u32) -> Vec<PlayerRef> {
    let mut seen = HashSet::new();
    server
        .world
        .all_characters()
        .into_iter()
        .filter(|p| {
            let saved = crate::db_character::persisted_faction(&p.data());
            top_level_faction(server, saved) == Some(faction) && seen.insert(p.character_id)
        })
        .collect()
}

fn send_relation_state(
    server: &GameServer,
    nation_id: u32,
    other_faction_id: u32,
    state: u8,
    expire_at: i64,
    next_state: u8,
) {
    let packet = SCFactionSetRelationStatePacket {
        id: nation_id,
        id2: other_faction_id,
        state,
        expire_time: utc(expire_at),
        next_state,
    };
    for member in side_members(server, nation_id) {
        member.send(packet.clone());
    }
    for member in side_members(server, other_faction_id) {
        member.send(packet.clone());
    }
}

/// Sends the expired notification (prev -> curr) and the new relation state to both sides.
/// The order is SetRelationState first so the client stores the new state before its message
/// (`FUN_392fcae0` shows `hostile_nation_msg` / `friendly_nation_msg` for `curr`, NATION-17).
fn notify_relation_expiry(server: &GameServer, event: &RelationExpiry) {
    send_relation_state(
        server,
        event.nation_id,
        event.other_faction_id,
        event.state,
        0,
        event.next_state,
    );
    if event.prev_state != event.state {
        let packet = SCFactionRelationExpiredPacket {
            id: event.nation_id,
            id2: event.other_faction_id,
            prev_state: event.prev_state,
            curr_state: event.state,
        };
        for member in side_members(server, event.nation_id) {
            member.send(packet.clone());
        }
        for member in side_members(server, event.other_faction_id) {
            member.send(packet.clone());
        }
    }
}

/// `CSFactionDeclareHostile` (0x019): the nation king declares hostility toward a diplomacy
/// target. The checks mirror the 1.2 client's pre-send validator (`FUN_392f9430` ->
/// `FUN_398a4750` / `FUN_398a3660`): not the king -> 32 `FACTION_RIGHTS`; anything that is not
/// an `is_diplomacy_tgt` system faction (or is already hostile) -> 718
/// `FACTION_DIPLOMACY_INVALID_SUBJECT` (NATION-16). Success sends the result to the king and
/// the new state to both sides.
pub fn declare_hostile(
    server: &GameServer,
    king: &PlayerRef,
    target: u32,
) -> std::result::Result<(), i16> {
    if !server.config.player_nations_enabled() {
        return Err(error_message::FACTION_RIGHTS);
    }
    let Some(nation) = server.systems.social.nations.owned_by(king.character_id) else {
        return Err(error_message::FACTION_RIGHTS);
    };
    let Some(target_faction) = server.game_data.factions.get(target).cloned() else {
        return Err(error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
    };
    if !target_faction.diplomacy_target || target_faction.id == nation.id {
        return Err(error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
    }
    let nations = &server.systems.social.nations;
    if nations.relation(nation.id, target) == Some(faction::relation_state::HOSTILE) {
        return Err(error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
    }
    let settings = server.game_data.systems.social.nation;
    let now = Utc::now().timestamp();
    let expire_at = now + hours_to_secs(settings.hostile_period_hours);
    // The no-treaty window ends at `expire_at`; the relation stays hostile until a treaty is
    // accepted, so `next_state` is hostile again (NATION-17).
    if !nations.set_relation(
        nation.id,
        target,
        faction::relation_state::HOSTILE,
        expire_at,
        faction::relation_state::HOSTILE,
    ) {
        return Err(error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
    }
    if let Some(old) = nations.take_treaty(nation.id, target) {
        server.systems.economy.mail.delete_mail(server, old.mail_id);
    }
    nations.set_offer_at(nation.id, target, now);
    schedule_persist_offer(server, nation.id, target, now);
    king.send(SCFactionDeclareHostileResultPacket {
        result: true,
        id: nation.id,
        id2: target,
    });
    send_relation_state(
        server,
        nation.id,
        target,
        faction::relation_state::HOSTILE,
        expire_at,
        faction::relation_state::HOSTILE,
    );
    schedule_persist_relation(
        server,
        nation.id,
        target,
        faction::relation_state::HOSTILE,
        expire_at,
        faction::relation_state::HOSTILE,
    );
    Ok(())
}

/// `content_configs` values for a target faction's relation (`None` = not a diplomacy target).
fn diplomacy_target(server: &GameServer, id: u32) -> Option<SystemFaction> {
    server
        .game_data
        .factions
        .get(id)
        .filter(|f| f.diplomacy_target)
        .cloned()
}

/// The treaty-request mail (type 25, `MAIL_NATION_REQUEST_FRIEND`) to the king, with the cost
/// `content_configs` 51 as its charge. Returns the mail id.
fn send_friend_treaty_mail(
    server: &GameServer,
    nation: &Nation,
    target: &SystemFaction,
    now: i64,
) -> Option<i64> {
    let receiver_name = server.names.get_character_name(nation.owner_character_id)?;
    let settings = server.game_data.systems.social.nation;
    let cost = i32::try_from(settings.friend_cost_copper).unwrap_or(i32::MAX);
    let mut mail = BaseMail {
        mail_type: mail_type::NATION_REQUEST_FRIEND,
        title: "Relation treaty request".into(),
        receiver_name,
        ..BaseMail::default()
    };
    mail.header.sender_id = 0;
    mail.header.sender_name = target.name.clone();
    mail.header.receiver_id = nation.owner_character_id;
    mail.header.status = mail_status::UNPAID;
    // The target faction is not a mail column; `extra` is the free tag (the house tax uses it
    // for its zone group / house id). INFERRED carrier, NATION-15.
    mail.header.extra = i64::from(target.id);
    mail.body.text = format!(
        "Make friendly relations with {}? Accepting pays {} copper.",
        target.name, settings.friend_cost_copper
    );
    mail.body.billing_amount = cost;
    mail.body.send_date = now;
    mail.body.recv_date = now;
    mail.open_date = now;
    mail.is_dirty = true;
    let mut txn = ItemTxn::begin(server);
    mail::send_mail(&mut txn, mail)
}

/// Sends one treaty-request mail now (the GM test hook; the periodic scheduler is
/// [`send_due_treaty_mails`]). Returns the mail id.
pub fn send_treaty_mail(server: &GameServer, nation_id: u32, target_id: u32) -> Option<i64> {
    if !server.config.player_nations_enabled() {
        return None;
    }
    let nation = server.systems.social.nations.get(nation_id)?;
    let target = diplomacy_target(server, target_id)?;
    let now = Utc::now().timestamp();
    if let Some(old) = server
        .systems
        .social
        .nations
        .take_treaty(nation_id, target_id)
    {
        server.systems.economy.mail.delete_mail(server, old.mail_id);
    }
    let mail_id = send_friend_treaty_mail(server, &nation, &target, now)?;
    server.systems.social.nations.add_treaty(
        PendingTreatyMail {
            nation_id,
            other_faction_id: target_id,
            mail_id,
            deadline: now + hours_to_secs(server.game_data.systems.social.nation.treaty_lead_hours),
        },
        now,
    );
    server
        .systems
        .social
        .nations
        .set_offer_at(nation_id, target_id, now);
    schedule_persist_offer(server, nation_id, target_id, now);
    Some(mail_id)
}

/// Sends the treaty-request mails that became due: `treaty_lead_hours` before a friendly
/// relation expires, and once a declared hostile window has really ended (a hostile row with
/// the hostile `next_state` marker). The faction-166 template rows (`next_state` 0) are never
/// offered. A per-(nation, target) offer cooldown of at least `treaty_lead_hours` (persisted)
/// stops a deleted / declined mail from being re-sent every tick (NATION-15).
pub fn send_due_treaty_mails(server: &GameServer, now: i64) -> usize {
    if !server.config.player_nations_enabled() {
        return 0;
    }
    let nations = &server.systems.social.nations;
    let mails = &server.systems.economy.mail;
    let settings = server.game_data.systems.social.nation;
    let lead = hours_to_secs(settings.treaty_lead_hours);

    // Snapshot the tracking, then query the mail store without the treaty lock (accepting a
    // mail holds the mail lock and takes the treaty lock).
    let tracked: Vec<PendingTreatyMail> = nations
        .treaties
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .copied()
        .collect();
    let mut stale = Vec::new();
    for entry in &tracked {
        let Some(mail) = mails.get_mail_by_id(entry.mail_id) else {
            stale.push(*entry);
            continue;
        };
        let receiver_ok = nations
            .get(entry.nation_id)
            .is_some_and(|n| n.owner_character_id == mail.header.receiver_id);
        if mail.mail_type != mail_type::NATION_REQUEST_FRIEND
            || mail.header.sender_id != 0
            || mail.header.extra != i64::from(entry.other_faction_id)
            || !receiver_ok
            || (entry.deadline > 0 && entry.deadline <= now)
        {
            stale.push(*entry);
            continue;
        }
        // A tracked mail id can be reused by a different mail after a reload.
        if mail.id != entry.mail_id {
            stale.push(*entry);
        }
    }
    if !stale.is_empty() {
        {
            let mut treaties = nations
                .treaties
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            treaties.retain(|p| {
                !stale
                    .iter()
                    .any(|s| s.nation_id == p.nation_id && s.other_faction_id == p.other_faction_id)
            });
        }
        for entry in &stale {
            mails.delete_mail(server, entry.mail_id);
        }
    }

    let mut sent = 0;
    for nation in nations.all() {
        for relation in &nation.relations {
            let Some(target) = diplomacy_target(server, relation.other_faction_id) else {
                continue;
            };
            let due = match relation.state {
                faction::relation_state::FRIENDLY => {
                    relation.expire_at > now && relation.expire_at - lead <= now
                }
                // Only a relation whose declaration window ended carries this marker; the
                // faction-166 template rows do not (NATION-17).
                faction::relation_state::HOSTILE => {
                    relation.expire_at == 0
                        && relation.next_state == faction::relation_state::HOSTILE
                }
                _ => false,
            };
            if !due || nations.treaty(nation.id, target.id).is_some() {
                continue;
            }
            if let Some(last) = nations.last_offer_at(nation.id, target.id) {
                if now < last + lead {
                    continue;
                }
            }
            let Some(mail_id) = send_friend_treaty_mail(server, &nation, &target, now) else {
                continue;
            };
            nations.add_treaty(
                PendingTreatyMail {
                    nation_id: nation.id,
                    other_faction_id: target.id,
                    mail_id,
                    // A friendly mail is valid until the applied period ends; a hostile-side
                    // offer is valid for one lead period.
                    deadline: if relation.state == faction::relation_state::FRIENDLY {
                        relation.expire_at
                    } else {
                        now + lead
                    },
                },
                now,
            );
            nations.set_offer_at(nation.id, target.id, now);
            schedule_persist_offer(server, nation.id, target.id, now);
            sent += 1;
        }
    }
    sent
}

/// Pays a nation treaty-request mail (`CSPayChargeMoney` 0x0a0, the button's
/// `X2Mail.PayCurrentMailChargeMoney` in the 1.2 `read_mail.alb`): debits the cost, deletes the
/// mail and moves the relation friendly for `friend_period_hours`, with the hostile state as
/// the next state. Only a live tracked request can be paid: a stale mail (old lead mail after a
/// hostile declaration, expired offer, reused mail id, wrong owner) is deleted and refused. The
/// mail fields beyond the charge (target tag in `extra`) are INFERRED (NATION-15).
pub fn pay_friend_treaty(server: &GameServer, payer: &PlayerRef, mail_id: i64) -> bool {
    if !server.config.player_nations_enabled() {
        // Same refusal a stale treaty mail gets; a disabled era has no treaties to pay.
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    }
    let mails = &server.systems.economy.mail;
    let Some(mail) = mails.get_mail_by_id(mail_id) else {
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    };
    let now = Utc::now().timestamp();
    if mail.mail_type != mail_type::NATION_REQUEST_FRIEND
        || mail.header.sender_id != 0
        || mail.header.receiver_id != payer.character_id
        || mail.header.status != mail_status::UNPAID
        || mail.body.billing_amount < 0
        || mail.body.recv_date > now
    {
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    }
    let Some(target_id) = u32::try_from(mail.header.extra).ok().filter(|id| *id != 0) else {
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    };
    let Some(nation) = server.systems.social.nations.owned_by(payer.character_id) else {
        send_error(payer, error_message::FACTION_RIGHTS);
        return false;
    };
    if diplomacy_target(server, target_id).is_none() {
        send_error(payer, error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
        return false;
    }
    let nations = &server.systems.social.nations;

    // An offer sent before a hostile declaration must not skip the no-treaty window.
    if let Some(relation) = nations.relation_row(nation.id, target_id) {
        if relation.state == faction::relation_state::HOSTILE && relation.expire_at > now {
            nations.take_treaty(nation.id, target_id);
            mails.delete_mail(server, mail_id);
            send_error(payer, error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
            return false;
        }
    }

    // A paid mail must still be the tracked offer for this nation / target.
    let Some(tracked) = nations.treaty(nation.id, target_id) else {
        mails.delete_mail(server, mail_id);
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    };
    if tracked.mail_id != mail_id || tracked.deadline == 0 || tracked.deadline <= now {
        nations.take_treaty(nation.id, target_id);
        mails.delete_mail(server, mail_id);
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    }

    let mut txn = ItemTxn::begin(server);
    let mut st = mails.state();
    let Some(current) = st.all.get(mail_id) else {
        drop(st);
        nations.take_treaty(nation.id, target_id);
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    };
    if current.mail_type != mail_type::NATION_REQUEST_FRIEND
        || current.header.sender_id != 0
        || current.header.status != mail_status::UNPAID
        || current.header.receiver_id != payer.character_id
        || current.header.extra != i64::from(target_id)
        || current.body.billing_amount != mail.body.billing_amount
    {
        drop(st);
        nations.take_treaty(nation.id, target_id);
        mails.delete_mail(server, mail_id);
        send_error(payer, error_message::MAIL_INVALID);
        return false;
    }
    // INVENTORY -> NONE checks and debits under the character lock (the `subtract_money`
    // helper's NONE -> INVENTORY form does not check the balance, NATION-15).
    if !inventory::change_money(
        &mut txn.out(),
        payer,
        crate::models::container::slot_type::INVENTORY,
        crate::models::container::slot_type::NONE,
        mail.body.billing_amount,
        task_type::DEPOSIT_MONEY,
    ) {
        drop(st);
        return false;
    }
    let settings = server.game_data.systems.social.nation;
    let expire_at = now + hours_to_secs(settings.friend_period_hours);
    if !nations.set_relation(
        nation.id,
        target_id,
        faction::relation_state::FRIENDLY,
        expire_at,
        faction::relation_state::HOSTILE,
    ) {
        // The nation vanished between the checks: refund and keep the mail payable.
        inventory::add_money(
            &mut txn.out(),
            payer,
            crate::models::container::slot_type::INVENTORY,
            mail.body.billing_amount,
            task_type::DEPOSIT_MONEY,
        );
        drop(st);
        send_error(payer, error_message::FACTION_DIPLOMACY_INVALID_SUBJECT);
        return false;
    }
    nations.take_treaty(nation.id, target_id);
    let count = {
        let mut character = payer.data();
        if st
            .all
            .get(mail_id)
            .is_some_and(|m| m.header.status != mail_status::READ)
        {
            character
                .unread_mail
                .update_received(mail_type::NATION_REQUEST_FRIEND, -1);
        }
        if let Some(m) = st.all.get_mut(mail_id) {
            m.header.status = mail_status::READ;
        }
        character.unread_mail
    };
    txn.env
        .fx
        .send(payer.character_id, SCChargeMoneyPaidPacket { mail_id });
    txn.env.fx.send(
        payer.character_id,
        SCMailDeletedPacket {
            is_sent: false,
            mail_id,
            is_unread_mail_count_modified: false,
            count,
        },
    );
    mail::delete_mail_in(server, &mut st, mail_id);
    drop(st);
    mail_character::send_unread_mail_count(&mut txn.env.fx, payer);
    send_relation_state(
        server,
        nation.id,
        target_id,
        faction::relation_state::FRIENDLY,
        expire_at,
        faction::relation_state::HOSTILE,
    );
    schedule_persist_relation(
        server,
        nation.id,
        target_id,
        faction::relation_state::FRIENDLY,
        expire_at,
        faction::relation_state::HOSTILE,
    );
    true
}

/// The periodic nation tick (subscribed to the existing `TickManager`, NATION-17): applies due
/// relation expiries, persists them, and sends the treaty-request mails that became due.
pub fn run_periodic(server: &GameServer) {
    let now = Utc::now().timestamp();
    let expiries = server.systems.social.nations.expire_relations(now);
    for event in &expiries {
        notify_relation_expiry(server, event);
        let nations = &server.systems.social.nations;
        if let Some(old) = nations.take_treaty(event.nation_id, event.other_faction_id) {
            server.systems.economy.mail.delete_mail(server, old.mail_id);
        }
        // The lead mail (if any) is gone with the period; a fresh offer must wait another
        // full lead period (NATION-15).
        nations.set_offer_at(event.nation_id, event.other_faction_id, now);
        schedule_persist_offer(server, event.nation_id, event.other_faction_id, now);
    }
    if !expiries.is_empty() {
        if let Some(server) = server.self_arc() {
            tokio::spawn(async move {
                if let Err(error) = persist_expiries(&server, &expiries).await {
                    tracing::error!("Persisting nation relation expiries failed: {error}");
                }
            });
        }
    }
    let sent = send_due_treaty_mails(server, now);
    if sent > 0 {
        tracing::info!("Nation: sent {sent} treaty request mail(s)");
    }
}

/// Rebuilds the type-25 treaty-mail tracking from the mail store after a restart: entries are
/// live only when the stored mail still matches the king, the target tag and the relation's
/// current offer window. Stale mails are deleted and their send date still seeds the offer
/// cooldown (NATION-15).
pub async fn rebuild_treaty_mail_tracking(server: &GameServer) -> Result<()> {
    let mut mails: Vec<BaseMail> = server
        .systems
        .economy
        .mail
        .state()
        .all
        .iter()
        .filter(|(_, m)| m.mail_type == mail_type::NATION_REQUEST_FRIEND)
        .map(|(_, m)| m.clone())
        .collect();
    // Newest first: an old duplicate of the same (nation, target) is deleted, not tracked.
    mails.sort_by_key(|m| std::cmp::Reverse((m.body.send_date, m.id)));
    let now = Utc::now().timestamp();
    let lead = hours_to_secs(server.game_data.systems.social.nation.treaty_lead_hours);
    let nations = &server.systems.social.nations;
    let mut stale = Vec::new();
    for mail in mails {
        let Some(target_id) = u32::try_from(mail.header.extra).ok().filter(|id| *id != 0) else {
            stale.push(mail.id);
            continue;
        };
        let Some(nation) = nations.owned_by(mail.header.receiver_id) else {
            stale.push(mail.id);
            continue;
        };
        if mail.header.sender_id != 0
            || mail.header.status != mail_status::UNPAID
            || diplomacy_target(server, target_id).is_none()
        {
            stale.push(mail.id);
            continue;
        }
        let sent = mail
            .body
            .send_date
            .max(mail.body.recv_date)
            .max(mail.open_date)
            .max(0);
        if sent > 0 {
            nations.set_offer_at(nation.id, target_id, sent);
        }
        if nations.treaty(nation.id, target_id).is_some() {
            stale.push(mail.id);
            continue;
        }
        let deadline = match nations.relation_row(nation.id, target_id) {
            Some(relation)
                if relation.state == faction::relation_state::FRIENDLY
                    && relation.expire_at > now
                    && sent > 0
                    && sent >= relation.expire_at - lead =>
            {
                relation.expire_at
            }
            Some(relation)
                if relation.state == faction::relation_state::HOSTILE
                    && relation.expire_at == 0
                    && relation.next_state == faction::relation_state::HOSTILE
                    && sent > 0
                    && sent + lead > now =>
            {
                sent + lead
            }
            _ => {
                stale.push(mail.id);
                continue;
            }
        };
        nations.add_treaty(
            PendingTreatyMail {
                nation_id: nation.id,
                other_faction_id: target_id,
                mail_id: mail.id,
                deadline,
            },
            now,
        );
    }
    for id in stale {
        server.systems.economy.mail.delete_mail(server, id);
    }
    if let Some(server) = server.self_arc() {
        persist_offers(&server).await?;
    }
    Ok(())
}

/// Writes one relation row immediately after a diplomacy change (the periodic save writes it
/// again). The row must already belong to a saved nation; a nation created in the same tick is
/// written by `save_creation` first.
pub(crate) async fn persist_relation(
    server: &GameServer,
    nation_id: u32,
    other_faction_id: u32,
    state: u8,
    expire_at: i64,
    next_state: u8,
) -> Result<()> {
    let _save = server.save.transaction_lock().await;
    sqlx::query(
        "INSERT INTO nation_relations(`nation_id`,`other_faction_id`,`state`,`expire_at`,          `next_state`) VALUES (?, ?, ?, ?, ?) ON DUPLICATE KEY UPDATE `state` = ?,          `expire_at` = ?, `next_state` = ?",
    )
    .bind(nation_id)
    .bind(other_faction_id)
    .bind(state)
    .bind(expire_at)
    .bind(next_state)
    .bind(state)
    .bind(expire_at)
    .bind(next_state)
    .execute(&server.db)
    .await?;
    Ok(())
}

pub(crate) async fn persist_offer(
    server: &GameServer,
    nation_id: u32,
    other_faction_id: u32,
    at: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO nation_relation_offers(`nation_id`,`other_faction_id`,`last_offer_at`) \
         VALUES (?, ?, ?) ON DUPLICATE KEY UPDATE `last_offer_at` = \
         GREATEST(`last_offer_at`, ?)",
    )
    .bind(nation_id)
    .bind(other_faction_id)
    .bind(at)
    .bind(at)
    .execute(&server.db)
    .await?;
    Ok(())
}

pub(crate) async fn persist_offers(server: &GameServer) -> Result<()> {
    let offers = server.systems.social.nations.offers_snapshot();
    if offers.is_empty() {
        return Ok(());
    }
    let _save = server.save.transaction_lock().await;
    let mut tx = server.db.begin().await?;
    for ((nation_id, other), at) in offers {
        sqlx::query(
            "INSERT INTO nation_relation_offers(`nation_id`,`other_faction_id`,`last_offer_at`) \
             VALUES (?, ?, ?) ON DUPLICATE KEY UPDATE `last_offer_at` = \
             GREATEST(`last_offer_at`, ?)",
        )
        .bind(nation_id)
        .bind(other)
        .bind(at)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

fn schedule_persist_offer(server: &GameServer, nation_id: u32, other_faction_id: u32, at: i64) {
    let Some(server) = server.self_arc() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = persist_offer(&server, nation_id, other_faction_id, at).await {
            tracing::error!(
                "Persisting nation offer {nation_id} -> {other_faction_id} failed: {error}"
            );
        }
    });
}

fn schedule_persist_relation(
    server: &GameServer,
    nation_id: u32,
    other_faction_id: u32,
    state: u8,
    expire_at: i64,
    next_state: u8,
) {
    let Some(server) = server.self_arc() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(error) = persist_relation(
            &server,
            nation_id,
            other_faction_id,
            state,
            expire_at,
            next_state,
        )
        .await
        {
            tracing::error!(
                "Persisting nation relation {nation_id} -> {other_faction_id} failed: {error}"
            );
        }
    });
}

/// Writes the expired relation rows immediately (the periodic save writes them again).
pub(crate) async fn persist_expiries(
    server: &GameServer,
    expiries: &[RelationExpiry],
) -> Result<()> {
    let _save = server.save.transaction_lock().await;
    let mut tx = server.db.begin().await?;
    for event in expiries {
        sqlx::query(
            "UPDATE nation_relations SET state = ?, expire_at = 0, next_state = 0 \
             WHERE nation_id = ? AND other_faction_id = ?",
        )
        .bind(event.state)
        .bind(event.nation_id)
        .bind(event.other_faction_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
