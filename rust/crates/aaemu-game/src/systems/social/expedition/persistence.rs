use super::{from_unix, Expedition, GameServer, MySql, MySqlPool, Nation, Result, Transaction};

/// `ExpeditionManager.Save(expedition)` (`Expedition.Save` in its own transaction).
pub async fn save(server: &GameServer, id: u32) {
    let Some(e) = server.expeditions.with_mut(id, |e| {
        let snapshot = e.clone();
        e.removed_members.clear();
        snapshot
    }) else {
        return;
    };
    if let Err(err) = save_expedition(&server.db, &e).await {
        tracing::error!("Expedition {id} save failed: {err}");
        // Keep the removed member ids for the next save attempt.
        server.expeditions.with_mut(id, |x| {
            for r in e.removed_members.iter().rev() {
                x.removed_members.insert(0, *r);
            }
        });
    }
}

/// The owner / nation / dominion rows that change together with one forced guild removal, in
/// one transaction. Snapshots the current expedition and nation rows first, so a concurrent
/// hand-over is not overwritten with an older king.
pub(super) async fn save_expedition_state(
    server: &GameServer,
    id: u32,
    nation_ids: &[u32],
    dominions: Option<crate::systems::housing::dominion::state::SaveBatch>,
) {
    let Some(e) = server.expeditions.with_mut(id, |e| {
        let snapshot = e.clone();
        e.removed_members.clear();
        snapshot
    }) else {
        return;
    };
    let nations: Vec<Nation> = nation_ids
        .iter()
        .filter_map(|n| server.systems.social.nations.get(*n))
        .collect();
    let had_dominions = dominions.is_some();
    let result: Result<()> = async {
        let mut tx = server.db.begin().await?;
        write_expedition(&mut tx, &e).await?;
        for nation in &nations {
            crate::systems::social::nation::NationManager::write_nation_owner(&mut tx, nation)
                .await?;
        }
        if let Some(batch) = dominions {
            server
                .systems
                .housing
                .dominions
                .save_prepared(&mut tx, batch)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            if had_dominions {
                server.systems.housing.dominions.commit_save();
            }
        }
        Err(err) => {
            tracing::error!("Expedition {id} save failed: {err}");
            // Keep the removed member ids for the next save attempt.
            server.expeditions.with_mut(id, |x| {
                for r in e.removed_members.iter().rev() {
                    x.removed_members.insert(0, *r);
                }
            });
        }
    }
}

/// `Expedition.Save(connection, transaction)`.
pub async fn save_expedition(pool: &MySqlPool, e: &Expedition) -> Result<()> {
    let mut tx = pool.begin().await?;
    write_expedition(&mut tx, e).await?;
    tx.commit().await?;
    Ok(())
}

/// The `Expedition.Save` body inside the caller's transaction.
async fn write_expedition(tx: &mut Transaction<'_, MySql>, e: &Expedition) -> Result<()> {
    if !e.removed_members.is_empty() {
        let list = e
            .removed_members
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        sqlx::query(&format!(
            "DELETE FROM expedition_members WHERE character_id IN ({list})"
        ))
        .execute(&mut **tx)
        .await?;
        sqlx::query(&format!(
            "UPDATE characters SET expedition_id = 0 WHERE `characters`.`id` IN ({list})"
        ))
        .execute(&mut **tx)
        .await?;
    }
    if e.is_disbanded {
        sqlx::query("DELETE FROM expeditions WHERE `id` = ?")
            .bind(e.faction.id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM expedition_role_policies WHERE `expedition_id` = ?")
            .bind(e.faction.id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("DELETE FROM expedition_members WHERE `expedition_id` = ?")
            .bind(e.faction.id)
            .execute(&mut **tx)
            .await?;
    } else {
        sqlx::query("REPLACE INTO expeditions(`id`,`owner`,`owner_name`,`name`,`mother`,`allow_change_name`,`created_at`) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(e.faction.id)
            .bind(e.faction.owner_id)
            .bind(&e.faction.owner_name)
            .bind(&e.faction.name)
            .bind(e.faction.mother_id)
            .bind(e.faction.allow_change_name)
            .bind(from_unix(e.faction.created))
            .execute(&mut **tx)
            .await?;
        for m in &e.members {
            sqlx::query("REPLACE INTO expedition_members(`character_id`,`expedition_id`,`name`,`level`,`role`,`last_leave_time`,`ability1`,`ability2`,`ability3`, `memo`) VALUES (?,?,?,?,?,?,?,?,?,?)")
                .bind(m.character_id)
                .bind(m.expedition_id)
                .bind(&m.name)
                .bind(m.level)
                .bind(m.role)
                .bind(from_unix(m.last_world_leave_time))
                .bind(m.abilities[0])
                .bind(m.abilities[1])
                .bind(m.abilities[2])
                .bind(&m.memo)
                .execute(&mut **tx)
                .await?;
        }
        for p in &e.policies {
            sqlx::query("REPLACE INTO expedition_role_policies(`expedition_id`,`role`,`name`,`dominion_declare`,`invite`,`expel`,`promote`,`dismiss`, `chat`, `manager_chat`, `siege_master`, `join_siege`) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)")
                .bind(p.expedition_id)
                .bind(p.role)
                .bind(&p.name)
                .bind(p.dominion_declare)
                .bind(p.invite)
                .bind(p.expel)
                .bind(p.promote)
                .bind(p.dismiss)
                .bind(p.chat)
                .bind(p.manager_chat)
                .bind(p.siege_master)
                .bind(p.join_siege)
                .execute(&mut **tx)
                .await?;
        }
    }
    Ok(())
}
