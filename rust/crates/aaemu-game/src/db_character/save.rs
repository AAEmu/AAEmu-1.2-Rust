use super::{cooldown_rows, join};
use crate::db::from_unix;
use crate::error::{GameError, Result};
use crate::models::character::Character;
use crate::server::GameServer;
use aaemu_commons::PacketStream;
use chrono::Utc;
use sqlx::mysql::MySqlPool;
use sqlx::{MySql, Transaction};

/// The `faction_id` a character save writes: the active temporary faction (duel Red / Blue
/// team, battlefield corps) is never persisted; its origin faction is written instead. A crash
/// during a duel / battlefield therefore reloads the original faction (RPA-09).
pub fn persisted_faction(c: &Character) -> u32 {
    if c.unit.faction_temporary && c.unit.origin_faction_id != 0 {
        c.unit.origin_faction_id
    } else {
        c.unit.faction_id
    }
}

/// `Character.Save(connection, transaction)`. `now` becomes `Updated`. Errors are returned so
/// `SaveDirectlyToDatabase` can roll back.
pub async fn save_character(
    tx: &mut Transaction<'_, MySql>,
    c: &mut Character,
    now: i64,
) -> Result<()> {
    let mut model = PacketStream::new();
    aaemu_commons::Marshal::write(&c.unit.model_params, &mut model);
    c.updated = now;
    // Position saving rule: inside a non-default instance the instance return point
    // (`MainWorldPosition`) is saved instead of the live transform (systems::instances).
    let pos = match c.main_world_position {
        Some(m)
            if c.unit.transform.instance_id != crate::models::character::DEFAULT_INSTANCE_ID =>
        {
            m
        }
        _ => c.unit.transform,
    };
    // Column order roll,pitch,yaw receives @yaw,@pitch,@roll: `roll` stores Rotation.Z and
    // `yaw` stores Rotation.X; Load reads them back crosswise, so the round trip is consistent.
    // Persist both bot-report counters: REPLACE must not reset the reported character's
    // counter to its schema default when saving unrelated character changes.
    sqlx::query(
        "REPLACE INTO `characters` \
        (`id`,`account_id`,`name`,`access_level`,`race`,`gender`,`unit_model_params`,`level`,`experience`,`recoverable_exp`,\
        `hp`,`mp`,`consumed_lp`,`ability1`,`ability2`,`ability3`,\
        `world_id`,`zone_id`,`x`,`y`,`z`,`roll`,`pitch`,`yaw`,\
        `faction_id`,`faction_name`,`expedition_id`,`family`,`dead_count`,`dead_time`,`rez_wait_duration`,`rez_time`,`rez_penalty_duration`,`leave_time`,`chat_restricted_until`,\
        `money`,`money2`,`honor_point`,`vocation_point`,`crime_point`,`crime_record`,`jury_point`,\
        `hostile_faction_kills`,`pvp_honor`,`died_in_pvp`,`died_in_pvp_war_zone`,\
        `delete_request_time`,`transfer_request_time`,`delete_time`,`auto_use_aapoint`,`prev_point`,`point`,`gift`,\
        `num_inv_slot`,`num_bank_slot`,`expanded_expert`,`slots`,`created_at`,`updated_at`,`return_district`,`online_time`,\
        `arrest_count`, `accept_guilty_count`, `accept_trial_count`, `not_guilty_count`, `guilty_count`, `evidence_reported_count`, `bot_reported_count`, `reported_as_bot_count`,\
        `offline_guilty_time`,`offline_guilty_region`) VALUES \
        (?,?,?,?,?,?,?,?,?,?, ?,?,?,?,?,?, ?,?,?,?,?,?,?,?, ?,?,?,?,?,?,?,?,?,?,?, ?,?,?,?,?,?,?, ?,?,?,?, ?,?,?,?,?,?,?, ?,?,?,?,?,?,?,?, ?,?,?,?,?,?,?,?, ?,?)",
    )
    .bind(c.id)
    .bind(c.account_id)
    .bind(&c.unit.name)
    .bind(c.access_level)
    .bind(c.race)
    .bind(c.gender)
    .bind(model.into_bytes())
    .bind(c.unit.level)
    .bind(c.experience)
    .bind(c.recoverable_exp)
    .bind(c.unit.hp)
    .bind(c.unit.mp)
    .bind(c.consumed_labor_power)
    .bind(c.ability1)
    .bind(c.ability2)
    .bind(c.ability3)
    .bind(c.server_id)
    .bind(pos.zone_id)
    .bind(pos.x)
    .bind(pos.y)
    .bind(pos.z)
    .bind(pos.rot_z) // `roll` <- @yaw
    .bind(pos.rot_y)
    .bind(pos.rot_x) // `yaw` <- @roll
    .bind(persisted_faction(c))
    .bind(&c.faction_name)
    .bind(c.unit.expedition_id)
    .bind(c.family)
    .bind(c.dead_count)
    .bind(from_unix(c.dead_time))
    .bind(c.rez_wait_duration)
    .bind(from_unix(c.rez_time))
    .bind(c.rez_penalty_duration)
    .bind(from_unix(c.leave_time))
    .bind(from_unix(c.chat_restricted_until))
    .bind(c.money)
    .bind(c.money2)
    .bind(c.honor_point)
    .bind(c.vocation_point)
    .bind(c.crime_point)
    .bind(c.infamy_point)
    .bind(c.jury_point)
    .bind(c.hostile_faction_kills)
    .bind(c.honor_gained_in_combat)
    .bind(c.died_in_pvp)
    .bind(c.died_in_pvp_war_zone)
    .bind(from_unix(c.delete_request_time))
    .bind(from_unix(c.transfer_request_time))
    .bind(from_unix(c.delete_time))
    .bind(c.auto_use_aa_point)
    .bind(c.prev_point)
    .bind(c.point)
    .bind(c.gift)
    .bind(c.num_inventory_slots)
    .bind(c.num_bank_slots)
    .bind(c.expanded_expert)
    .bind(c.action_slots_blob())
    .bind(from_unix(c.created))
    .bind(from_unix(c.updated))
    .bind(c.return_district_id)
    .bind(f64::from(c.online_time))
    .bind(c.arrest_count)
    .bind(c.accept_guilty_count)
    .bind(c.accept_trial_count)
    .bind(c.not_guilty_count)
    .bind(c.guilty_count)
    .bind(c.evidence_reported_count)
    .bind(c.bot_reported_count)
    .bind(c.reported_as_bot_count)
    .bind(c.offline_guilty_time)
    .bind(c.offline_guilty_region)
    .execute(&mut **tx)
    .await?;

    // Labor is account-level (`accounts.labor`): flush the delta applied to the cached
    // `labor_power` since the last save in this transaction, so play-spent labor is persisted
    // and never double-counted (the online regeneration tick only changes the cache).
    if c.labor_power_unsaved != 0 {
        sqlx::query("UPDATE accounts SET labor = labor + ? WHERE account_id = ?")
            .bind(c.labor_power_unsaved)
            .bind(c.account_id)
            .execute(&mut **tx)
            .await?;
        c.labor_power_unsaved = 0;
    }

    for (k, v) in &c.options {
        sqlx::query("REPLACE INTO `options` (`key`,`value`,`owner`) VALUES (?,?,?)")
            .bind(k)
            .bind(v)
            .bind(c.id)
            .execute(&mut **tx)
            .await?;
    }
    for a in &c.abilities.abilities {
        sqlx::query("REPLACE INTO abilities(`id`,`exp`,`owner`) VALUES (?, ?, ?)")
            .bind(a.id)
            .bind(a.exp)
            .bind(c.id)
            .execute(&mut **tx)
            .await?;
    }
    for a in &c.actability.actabilities {
        sqlx::query("REPLACE INTO actabilities(`id`,`point`,`step`,`owner`) VALUES (?, ?, ?, ?)")
            .bind(a.id as u8) // C# binds (byte)actability.Id
            .bind(a.point)
            .bind(a.step)
            .bind(c.id)
            .execute(&mut **tx)
            .await?;
    }
    for &a in &c.appellations.appellations {
        sqlx::query("REPLACE INTO appellations(`id`,`active`,`owner`) VALUES (?, ?, ?)")
            .bind(a)
            .bind(c.appellations.active == a)
            .bind(c.id)
            .execute(&mut **tx)
            .await?;
    }
    save_active_buffs(tx, c).await;
    save_cooldowns(tx, c, Utc::now().timestamp_millis()).await;
    save_portals(tx, c).await?;
    // Friends
    if !c.removed_friends.is_empty() {
        let list = join(&c.removed_friends);
        sqlx::query(&format!(
            "DELETE FROM friends WHERE owner = ? AND friend_id IN({list})"
        ))
        .bind(c.id)
        .execute(&mut **tx)
        .await?;
        c.removed_friends.clear();
    }
    for f in &c.friends {
        sqlx::query("REPLACE INTO friends(`id`,`friend_id`,`owner`) VALUES (?, ?, ?)")
            .bind(f.id)
            .bind(f.friend_id)
            .bind(f.owner)
            .execute(&mut **tx)
            .await?;
    }
    if !c.removed_blocked.is_empty() {
        let list = join(&c.removed_blocked);
        sqlx::query(&format!(
            "DELETE FROM blocked WHERE owner = ? AND blocked_id IN({list})"
        ))
        .bind(c.id)
        .execute(&mut **tx)
        .await?;
        c.removed_blocked.clear();
    }
    for (owner, blocked) in &c.blocked {
        sqlx::query("REPLACE INTO blocked(`owner`,`blocked_id`) VALUES (?, ?)")
            .bind(owner)
            .bind(blocked)
            .execute(&mut **tx)
            .await?;
    }
    // Skills
    if !c.skills.removed.is_empty() {
        let list = join(&c.skills.removed);
        sqlx::query(&format!(
            "DELETE FROM skills WHERE owner = ? AND id IN({list})"
        ))
        .bind(c.id)
        .execute(&mut **tx)
        .await?;
        c.skills.removed.clear();
    }
    for s in &c.skills.skills {
        sqlx::query("REPLACE INTO skills(`id`,`level`,`type`,`owner`) VALUES (?, ?, ?, ?)")
            .bind(s.id)
            .bind(s.level)
            .bind(1u8) // SkillType.Skill (enum index)
            .bind(c.id)
            .execute(&mut **tx)
            .await?;
    }
    for b in &c.skills.passive_buffs {
        sqlx::query("REPLACE INTO skills(`id`,`level`,`type`,`owner`) VALUES (?, ?, ?, ?)")
            .bind(b)
            .bind(1)
            .bind(2u8) // SkillType.Buff
            .bind(c.id)
            .execute(&mut **tx)
            .await?;
    }
    // Quests
    if !c.quests.removed.is_empty() {
        let list = join(&c.quests.removed);
        sqlx::query(&format!(
            "DELETE FROM quests WHERE owner = ? AND template_id IN({list})"
        ))
        .bind(c.id)
        .execute(&mut **tx)
        .await?;
        c.quests.removed.clear();
    }
    for q in &c.quests.completed {
        sqlx::query("REPLACE INTO completed_quests(`id`,`data`,`owner`) VALUES(?,?,?)")
            .bind(q.id)
            .bind(q.body8().to_vec())
            .bind(c.id)
            .execute(&mut **tx)
            .await?;
    }
    for q in &c.quests.active {
        sqlx::query(
            "REPLACE INTO quests(`id`,`template_id`,`data`,`status`,`owner`) VALUES(?,?,?,?,?)",
        )
        .bind(q.id)
        .bind(q.template_id)
        .bind(q.write_data())
        .bind(q.status)
        .bind(c.id)
        .execute(&mut **tx)
        .await?;
    }
    // Achievements (records + progress). Written after the `characters` REPLACE above, so its
    // ON DELETE CASCADE cannot drop the rows this snapshot re-inserts.
    crate::systems::social::achievement_persistence::save_character_achievements(tx, c).await?;
    // Learned crafts (`character_crafts`; only the ids added since the last committed save).
    crate::systems::economy::craft_learning::save_character_crafts(
        tx,
        c.id,
        &c.learned_crafts_unsaved,
    )
    .await?;
    // Later phase: Mates.Save.
    Ok(())
}

async fn save_portals(tx: &mut Transaction<'_, MySql>, c: &Character) -> Result<()> {
    for p in &c.portals.private_portals {
        sqlx::query("REPLACE INTO portal_book_coords(`id`,`name`,`x`,`y`,`z`,`zone_id`,`z_rot`,`sub_zone_id`,`owner`) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(p.id)
            .bind(&p.name)
            .bind(p.x)
            .bind(p.y)
            .bind(p.z)
            .bind(p.zone_id)
            .bind(p.z_rot)
            .bind(p.sub_zone_id)
            .bind(p.owner)
            .execute(&mut **tx)
            .await?;
    }
    for v in &c.portals.visited_districts {
        sqlx::query(
            "REPLACE INTO portal_visited_district(`id`,`subzone`,`owner`) VALUES (?, ?, ?)",
        )
        .bind(v.id)
        .bind(v.sub_zone)
        .bind(v.owner)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Save the character's running cooldowns: rewrite the rows each save, dropping expired ones.
/// Errors are logged instead of failing the whole character save, like the active-buff rows.
async fn save_cooldowns(tx: &mut Transaction<'_, MySql>, c: &Character, now_ms: i64) {
    let res: Result<usize> = async {
        sqlx::query("DELETE FROM `character_cooldowns` WHERE `character_id` = ?")
            .bind(c.id)
            .execute(&mut **tx)
            .await?;
        let mut saved = 0;
        for row in cooldown_rows(c, now_ms) {
            sqlx::query(
                "INSERT INTO `character_cooldowns` (`character_id`, `cooldown_type`, `cooldown_id`, \
                 `expires_at_ms`) VALUES (?, ?, ?, ?)",
            )
            .bind(c.id)
            .bind(row.kind)
            .bind(row.id)
            .bind(row.expires_at_ms)
            .execute(&mut **tx)
            .await?;
            saved += 1;
        }
        Ok(saved)
    }
    .await;
    match res {
        Ok(n) if n > 0 => tracing::info!("Saved {n} active cooldown(s) for character {}", c.id),
        Ok(_) => {}
        Err(e) => tracing::error!("Failed to save cooldowns for character {}: {e}", c.id),
    }
}

/// `Buffs.SaveActiveBuffs` (errors are logged, as in C#).
async fn save_active_buffs(tx: &mut Transaction<'_, MySql>, c: &Character) {
    let res: Result<usize> = async {
        sqlx::query("DELETE FROM `character_active_buffs` WHERE `character_id` = ?")
            .bind(c.id)
            .execute(&mut **tx)
            .await?;
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let mut saved = 0;
        for b in &c.unit.buffs.effects {
            if !b.should_persist(now_ms) {
                continue;
            }
            let time_left = b.time_left(now_ms) as i32;
            if time_left <= 0 {
                continue;
            }
            sqlx::query(
                "INSERT INTO `character_active_buffs` (`character_id`, `buff_id`, `caster_id`, `skill_id`, `ab_level`, \
                 `duration`, `time_left`, `charge`, `stack_count`, `real_time`, `saved_at`) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE `caster_id`=VALUES(`caster_id`), `skill_id`=VALUES(`skill_id`), `ab_level`=VALUES(`ab_level`), \
                 `duration`=VALUES(`duration`), `time_left`=VALUES(`time_left`), `charge`=VALUES(`charge`), \
                 `stack_count`=VALUES(`stack_count`), `real_time`=VALUES(`real_time`), `saved_at`=VALUES(`saved_at`)",
            )
            .bind(c.id)
            .bind(b.template.id)
            .bind(b.caster.map_or(c.id, |x| x.0))
            .bind(0u32)
            .bind(b.ab_level)
            .bind(b.duration)
            .bind(time_left)
            .bind(b.charge)
            .bind(1)
            .bind(u8::from(b.template.real_time))
            .bind(now.naive_utc())
            .execute(&mut **tx)
            .await?;
            saved += 1;
        }
        Ok(saved)
    }
    .await;
    match res {
        Ok(n) if n > 0 => tracing::debug!("Saved {n} active buff(s) for character {}", c.id),
        Ok(_) => {}
        Err(e) => tracing::error!("Failed to save active buffs for character {}: {e}", c.id),
    }
}

/// `Character.SaveDirectlyToDatabase`. All-or-nothing: the transaction is committed only when
/// every statement succeeded; any statement error rolls it back and the caller's character keeps
/// the state it had before the call (`save_character` advances `updated` and clears the removed
/// lists while it works, so it runs on a clone that is applied only after the commit).
pub async fn save_directly_to_database(pool: &MySqlPool, c: &mut Character) -> bool {
    save_directly(pool, c, None, None).await
}

/// Character creation: [`save_directly_to_database`] plus the new character's containers and
/// items in the same transaction (the C# left them to the next world save, so a stop or crash
/// before it kept the `characters` row but lost the starting supplies; the next select then
/// built empty containers).
/// The caller holds `server.save.transaction_lock()` from before inventory registration
/// until commit or cleanup, excluding world snapshots of a partially created character.
pub(crate) async fn save_new_character_locked(server: &GameServer, c: &mut Character) -> bool {
    let batch = server.items.prepare_save_for_owner(c.id);
    save_directly(&server.db, c, Some((&server.items, batch)), None).await
}

/// The lobby character edit: the rebuilt starting kit replaces everything the creation-time save
/// wrote for this character. In one transaction the old item/container rows and the create-time
/// ability/skill rows are deleted, then the `characters` row and the rebuilt items are written.
/// The caller holds `server.save.transaction_lock()`.
pub(crate) async fn save_edited_character_locked(
    server: &GameServer,
    c: &mut Character,
    batch: crate::managers::item::ItemSaveBatch,
) -> bool {
    save_directly(&server.db, c, Some((&server.items, batch)), Some(c.id)).await
}

/// Deletes the rows a lobby edit rebuilds. Only for a never-played character, and only the
/// creation containers (equipment, bag, bank; `mate_id = 0`): a never-played character can
/// already own mail attachments, auction/trade/system items and mate/slave equipment, whose
/// container and item rows must survive the rebuild.
async fn clear_rebuilt_character_rows(tx: &mut Transaction<'_, MySql>, owner: u32) -> Result<()> {
    use crate::models::container::slot_type;
    let creation_slots = format!(
        "{}, {}, {}",
        slot_type::EQUIPMENT,
        slot_type::INVENTORY,
        slot_type::BANK
    );
    sqlx::query(&format!(
        concat!(
            "DELETE i FROM `items` i JOIN `item_containers` ic ",
            "ON ic.container_id = i.container_id ",
            "WHERE ic.owner_id = ? AND ic.mate_id = 0 AND ic.slot_type IN ({})"
        ),
        creation_slots
    ))
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    sqlx::query(&format!(
        concat!(
            "DELETE FROM `item_containers` ",
            "WHERE `owner_id` = ? AND `mate_id` = 0 AND `slot_type` IN ({})"
        ),
        creation_slots
    ))
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    for table in ["skills", "abilities", "actabilities", "appellations"] {
        sqlx::query(&format!("DELETE FROM `{table}` WHERE `owner` = ?"))
            .bind(owner)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn save_directly(
    pool: &MySqlPool,
    c: &mut Character,
    items: Option<(
        &crate::managers::item::ItemManager,
        crate::managers::item::ItemSaveBatch,
    )>,
    rebuild_owner: Option<u32>,
) -> bool {
    let mut saved = c.clone();
    let manager = items.as_ref().map(|(manager, _)| *manager);
    let res: Result<()> = async {
        let mut tx = pool.begin().await?;
        let now = Utc::now().timestamp();
        let written = async {
            if let Some(owner) = rebuild_owner {
                clear_rebuilt_character_rows(&mut tx, owner).await?;
            }
            save_character(&mut tx, &mut saved, now).await?;
            if let Some((manager, batch)) = items {
                manager.write_prepared(&mut tx, batch).await?;
            }
            Ok::<(), GameError>(())
        }
        .await;
        match written {
            Ok(()) => {
                tx.commit().await?;
                if let Some(manager) = manager {
                    manager.commit_save();
                }
                Ok(())
            }
            Err(e) => {
                // The C# committed the partial transaction here; that can lose achievement
                // receipts the `characters` REPLACE already cascaded away.
                if let Err(rollback) = tx.rollback().await {
                    tracing::error!(
                        "Character save rollback failed for {} - {}: {rollback}",
                        saved.id,
                        saved.unit.name
                    );
                }
                Err(e)
            }
        }
    }
    .await;
    match res {
        Ok(()) => {
            // The direct save committed the rows this snapshot carried; drop them so the next
            // snapshot does not rewrite them.
            saved.learned_crafts_unsaved.clear();
            *c = saved;
            true
        }
        Err(e) => {
            tracing::error!("Character save failed for {} - {}: {e}", c.id, c.unit.name);
            false
        }
    }
}
