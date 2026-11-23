use super::{
    equip_slot, error_message, faction, send_error, CastAction, EffectArgs, GameServer, HashSet,
    MySql, Nation, NationRelation, PlayerRef, PoisonError, Result, SCFactionIndependencePacket,
    SCFactionRelationListPacket, SkillCaster, TemplateKind, Transaction, Utc,
    CASTLE_CLAIM_BACKPACK_TYPE, DECLARE_INDEPENDENCE_SKILL, INDEPENDENCE_TEMPLATE_FACTION,
};

/// Writes a newly created nation immediately (the GM command awaits it; the independence effect
/// spawns it). The periodic save writes the same rows again.
pub async fn save_creation(server: &GameServer, nation: &Nation) -> Result<()> {
    let online: HashSet<u32> = server
        .world
        .all_characters()
        .iter()
        .map(|p| p.character_id)
        .collect();
    // The periodic save must not touch `expeditions` (a reused guild id would be captured);
    // only the creation transaction writes the founding guild's mother.
    let _save = server.save.transaction_lock().await;
    let mut tx = server.db.begin().await?;
    write_nation(server, &mut tx, nation, &online).await?;
    if nation.expedition_id != 0 {
        sqlx::query("UPDATE expeditions SET mother = ? WHERE id = ?")
            .bind(nation.id)
            .bind(nation.expedition_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    server
        .systems
        .social
        .nations
        .clear_offline_members(nation.id);
    Ok(())
}

/// `REPLACE nations` + the nation's relation rows + `expeditions.mother` + the offline
/// members' `characters.faction_id`. Called inside the caller's transaction so the membership
/// pointers move together.
pub(super) async fn write_nation(
    server: &GameServer,
    tx: &mut Transaction<'_, MySql>,
    nation: &Nation,
    online: &HashSet<u32>,
) -> Result<usize> {
    sqlx::query(
        "REPLACE INTO nations(`id`,`expedition_id`,`owner_character_id`,`owner_name`,`name`,\
         `created_at`,`allow_change_name`) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(nation.id)
    .bind(nation.expedition_id)
    .bind(nation.owner_character_id)
    .bind(&nation.owner_name)
    .bind(&nation.name)
    .bind(nation.created_at)
    .bind(nation.allow_change_name)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM nation_relations WHERE nation_id = ?")
        .bind(nation.id)
        .execute(&mut **tx)
        .await?;
    for relation in &nation.relations {
        sqlx::query(
            "INSERT INTO nation_relations(`nation_id`,`other_faction_id`,`state`,`expire_at`,\
             `next_state`) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(nation.id)
        .bind(relation.other_faction_id)
        .bind(relation.state)
        .bind(relation.expire_at)
        .bind(relation.next_state)
        .execute(&mut **tx)
        .await?;
    }
    let offline: Vec<u32> = nation
        .offline_members
        .iter()
        .copied()
        .filter(|id| !online.contains(id))
        .collect();
    if !offline.is_empty() {
        let list = offline
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        sqlx::query(&format!(
            "UPDATE characters SET faction_id = ? WHERE id IN ({list})"
        ))
        .bind(nation.id)
        .execute(&mut **tx)
        .await?;
        sqlx::query(&format!(
            "UPDATE housings SET faction_id = ? WHERE owner IN ({list})"
        ))
        .bind(nation.id)
        .execute(&mut **tx)
        .await?;
        // Every `housings` row is also loaded into the housing manager at startup, so an
        // offline owner has live HouseRefs that must move too; otherwise the next house save
        // writes the old faction back (NATION-5 review follow-up).
        for member in &offline {
            crate::systems::housing::update_owned_housing_faction(server, *member, nation.id);
        }
    }
    Ok(1 + nation.relations.len() + offline.len())
}

/// The relation template of a new nation: the `system_faction_relations` rows that name faction
/// 166, normalised to `(nation, other)` and with the template itself removed.
pub fn template_relations(server: &GameServer) -> Vec<NationRelation> {
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for relation in &server.game_data.factions.relations {
        let other = if relation.id == INDEPENDENCE_TEMPLATE_FACTION {
            relation.id2
        } else if relation.id2 == INDEPENDENCE_TEMPLATE_FACTION {
            relation.id
        } else {
            continue;
        };
        if other == INDEPENDENCE_TEMPLATE_FACTION || !seen.insert(other) {
            continue;
        }
        rows.push(NationRelation {
            other_faction_id: other,
            state: relation.state,
            expire_at: 0,
            next_state: 0,
        });
    }
    rows
}

/// `declare_independence`: validation, the nation record, the founding expedition's mother,
/// the online members' faction change and the independence / relation packets. Returns the
/// created nation. The caller persists it (`save_creation` / the periodic save).
///
/// `require_dominion` is the 1.2 skill prerequisite (the caster's expedition owns a dominion);
/// the GM command may skip it for tests (see `PORTING.md`, NATION-4).
pub fn declare_independence(
    server: &GameServer,
    founder: &PlayerRef,
    expedition_id: u32,
    name: &str,
    require_dominion: bool,
) -> std::result::Result<Nation, i16> {
    // Player nations are KR-only content (no Trion 1.2 evidence; `PORTING.md`, NATION row).
    // The refusal is the same one a failed ownership precondition uses, so no new error id is
    // introduced.
    if !server.config.player_nations_enabled() {
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    }
    let (character_id, current_expedition) = {
        let c = founder.data();
        (c.id, c.unit.expedition_id)
    };
    if expedition_id == 0 || current_expedition != expedition_id {
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    }
    let Some(expedition) = server
        .expeditions
        .get(expedition_id)
        .filter(|e| !e.is_disbanded)
    else {
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    };
    if expedition.faction.owner_id != character_id {
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    }
    if server
        .systems
        .social
        .nations
        .for_expedition(expedition_id)
        .is_some()
    {
        // FACTION_SEPARATE_MOTHER: the expedition already belongs to a nation.
        return Err(error_message::FACTION_SEPARATE_MOTHER);
    }
    if require_dominion
        && !server
            .systems
            .housing
            .dominions
            .all()
            .iter()
            .any(|dominion| dominion.owner_expedition_id == expedition_id)
    {
        // No dedicated 1.2 error id is known for this check (PORTING.md, NATION-4).
        return Err(error_message::FACTION_SEPARATE_RIGHTS);
    }
    if name.is_empty()
        || super::rename::faction_name_in_use(
            server,
            name,
            Some(super::rename::NameOwner::Expedition(expedition_id)),
        )
    {
        return Err(error_message::FACTION_DUPLICATE_NAME);
    }
    let id = loop {
        let id = match server.ids.expedition.get_next_id() {
            Ok(id) => id,
            Err(error) => {
                tracing::error!("{error}");
                return Err(error_message::FACTION_CREATE);
            }
        };
        // The allocator normally knows both tables; the checks keep a test fixture that adds
        // an expedition directly from handing out a duplicate faction id.
        if !server.expeditions.exists(id) && server.systems.social.nations.get(id).is_none() {
            break id;
        }
    };
    let members = expedition.member_ids();
    let online: HashSet<u32> = server
        .world
        .all_characters()
        .iter()
        .map(|p| p.character_id)
        .collect();
    let now = Utc::now();
    let nation = Nation {
        id,
        expedition_id,
        owner_character_id: expedition.faction.owner_id,
        owner_name: expedition.faction.owner_name.clone(),
        name: name.to_string(),
        created_at: now.timestamp(),
        allow_change_name: 0,
        relations: template_relations(server),
        offline_members: members
            .iter()
            .copied()
            .filter(|id| !online.contains(id))
            .collect(),
    };
    {
        // Guild / nation / static names share one namespace; the lock keeps the final check
        // and the insert together (RENAME-3 / RENAME-6).
        let _names = super::rename::lock_names(server);
        if super::rename::faction_name_in_use(
            server,
            name,
            Some(super::rename::NameOwner::Expedition(expedition_id)),
        ) || !server
            .systems
            .social
            .nations
            .add_if_name_free(nation.clone())
        {
            return Err(error_message::FACTION_DUPLICATE_NAME);
        }
    }
    // The client's handler rewrites the founding expedition's mother; the server must do the
    // same before re-factioning its members or `set_faction` sees a mother mismatch and kicks.
    server
        .expeditions
        .with_mut(expedition_id, |e| e.faction.mother_id = id);
    server
        .world
        .broadcast_to_server(&SCFactionIndependencePacket {
            id: expedition_id,
            id2: id,
            name: nation.name.clone(),
            c_time: now,
        });
    server
        .world
        .broadcast_to_server(&SCFactionRelationListPacket {
            relations: nation.wire_relations(),
        });
    for member_id in members {
        if let Some(member) = server.world.character_by_id(member_id) {
            faction::set_faction_persistent(server, &member, id);
        }
    }
    Ok(nation)
}

/// Special effect 71 `DeclareIndependence`: validates the accepted cast of skill 18540 and the
/// CASTLE_CLAIM backpack, then runs [`declare_independence`] and persists the new nation.
pub fn declare_independence_effect(server: &GameServer, args: &EffectArgs<'_>) {
    if !server.config.player_nations_enabled() {
        if let Some(player) = args.caster.as_ref().and_then(|c| c.as_player()) {
            send_error(player, error_message::FACTION_SEPARATE_RIGHTS);
        }
        return;
    }
    let Some(caster) = args.caster.as_ref() else {
        return;
    };
    let Some(player) = caster.as_player() else {
        return;
    };
    let Some(source) = args.source.skill.as_ref() else {
        return;
    };
    let Some(canonical) = server
        .game_data
        .systems
        .skills
        .skill(DECLARE_INDEPENDENCE_SKILL)
    else {
        return;
    };
    let accepted = {
        let state = source.state();
        state.generation != 0 && !state.cancelled
    };
    if !std::sync::Arc::ptr_eq(canonical, &source.template)
        || source.id != DECLARE_INDEPENDENCE_SKILL
        || args.source.buff.is_some()
        || args.source.is_trigger
        || !accepted
        || !matches!(args.caster_caster, SkillCaster::Unit(id) if id == player.obj_id)
        || !matches!(
            args.cast,
            CastAction::Skill { skill_id, tl_id }
                if skill_id == DECLARE_INDEPENDENCE_SKILL && tl_id == source.tl_id()
        )
    {
        return;
    }
    if !has_castle_claim_backpack(server, player) {
        send_error(player, error_message::FACTION_SEPARATE_RIGHTS);
        return;
    }
    let expedition_id = player.data().unit.expedition_id;
    let Some(expedition) = server.expeditions.get(expedition_id) else {
        send_error(player, error_message::FACTION_SEPARATE_RIGHTS);
        return;
    };
    let name = expedition.faction.name.clone();
    match declare_independence(server, player, expedition_id, &name, true) {
        Ok(nation) => {
            if let Some(server) = server.self_arc() {
                tokio::spawn(async move {
                    if let Err(error) = save_creation(&server, &nation).await {
                        tracing::error!("Nation {} save failed: {error}", nation.id);
                    }
                });
            }
        }
        Err(error) => send_error(player, error),
    }
}

/// The skill's CASTLE_CLAIM backpack (type 1 in the equip slot 26). Skill 18540's `unit_reqs`
/// row only checks that slot 26 is occupied (`UnitReq` 43379, kind 8); the backpack type is the
/// authored restriction.
fn has_castle_claim_backpack(server: &GameServer, player: &PlayerRef) -> bool {
    let equipment = player.data().inventory.equipment().clone();
    let item = equipment
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_item_by_slot(equip_slot::BACKPACK)
        .cloned();
    item.and_then(|item| server.game_data.items.template(item.template_id))
        .is_some_and(|template| {
            matches!(template.kind, TemplateKind::Backpack { backpack_type } if backpack_type == CASTLE_CLAIM_BACKPACK_TYPE)
        })
}
