//! Player nations: a top-level user faction (`mother_id == 0`) created from an expedition by
//! the 1.2 "declare independence" skill (18540, special effect 71).
//!
//! Evidence and unknowns are recorded in `PORTING.md` (NATION-1..): the client creates the
//! new faction from `SCFactionIndependence` (`FUN_392fb2d0`), so the server keeps the nation
//! record, the founding expedition's `mother` and the members' `faction_id` in sync; the
//! nation name carrier is unknown and the founding expedition's name is used (NATION-1).
//!
//! Nation ids come from the expedition id manager: the 1.2 client keeps a single faction map
//! and `FUN_398a5280` refuses a duplicate id (`managers/id.rs`, `EXPEDITION_ID` also extracts
//! `nations.id`). A new nation copies faction 166's `system_faction_relations` rows (NATION-2).

use std::collections::{HashMap, HashSet};
use std::sync::{PoisonError, RwLock};

use chrono::Utc;
use sqlx::{MySql, MySqlPool, Row, Transaction};

use crate::data::items::{backpack_type, TemplateKind};
use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::error::Result;
use crate::models::container::equip_slot;
use crate::packets::world::{SCFactionListPacket, SCFactionRelationListPacket};
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::skills::effects::EffectArgs;
use crate::systems::skills::types::{CastAction, SkillCaster};
use crate::world::PlayerRef;

use super::faction;
use super::packets::faction::SCFactionIndependencePacket;
use super::util::send_error;

/// Skill "국가 독립" (declare independence), 20 s cast, CASTLE_CLAIM backpack (equip slot 26).
pub const DECLARE_INDEPENDENCE_SKILL: u32 = 18_540;
/// `system_factions` 166 "default faction setup for independence": the relation template of a
/// new nation (46 rows in the 1.2 compact database).
pub const INDEPENDENCE_TEMPLATE_FACTION: u32 = 166;
/// `item_backpacks.backpack_type_id` of the CASTLE_CLAIM backpacks (skill 18540's
/// `link_backpack_type_id`).
pub const CASTLE_CLAIM_BACKPACK_TYPE: u32 = backpack_type::CASTLE_CLAIM;

/// One relation row of a nation. `other_faction_id` is always the other side (the stored rows
/// are normalised); `expire_at` / `next_state` are reserved for the stage-3 relation expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NationRelation {
    pub other_faction_id: u32,
    pub state: u8,
    pub expire_at: i64,
    pub next_state: u8,
}

impl NationRelation {
    /// The `FactionRelation` row the login / independence relation lists carry.
    pub fn wire(&self, nation_id: u32) -> FactionRelation {
        FactionRelation {
            id: nation_id,
            id2: self.other_faction_id,
            state: self.state,
            exp_time: self.expire_at,
        }
    }
}

/// A player nation (`nations` row plus its relation overlay).
#[derive(Debug, Clone, PartialEq)]
pub struct Nation {
    pub id: u32,
    /// The founding expedition; its `mother` becomes this nation.
    pub expedition_id: u32,
    /// The king: the founding expedition's owner.
    pub owner_character_id: u32,
    pub owner_name: String,
    pub name: String,
    pub created_at: i64,
    pub allow_change_name: u8,
    pub relations: Vec<NationRelation>,
    /// Expedition members that were offline when the nation was created. Their
    /// `characters.faction_id` is rewritten with the founding expedition's `mother`
    /// (runtime only; a loaded nation does not need it again).
    pub offline_members: Vec<u32>,
}

impl Nation {
    /// The `SystemFaction` record the client receives (`SCFactionList`). The client's
    /// independence handler clones the founding expedition and sets `mother = 0`, the name,
    /// the created time and `diplomacy_target = 1` (`FUN_392fb2d0`); the remaining fields
    /// follow the cloned expedition record.
    pub fn system_faction(&self) -> SystemFaction {
        SystemFaction {
            id: self.id,
            name: self.name.clone(),
            owner_id: self.owner_character_id,
            owner_name: self.owner_name.clone(),
            mother_id: 0,
            unit_owner_type: 0,
            political_system: 1,
            diplomacy_target: true,
            aggro_link: false,
            guard_help: false,
            allow_change_name: self.allow_change_name,
            created: self.created_at,
        }
    }

    /// The relation rows of this nation in wire form (`(nation, other, state)`).
    pub fn wire_relations(&self) -> Vec<FactionRelation> {
        self.relations.iter().map(|r| r.wire(self.id)).collect()
    }
}

/// `NationManager`: the runtime nations and their relation overlay.
#[derive(Debug, Default)]
pub struct NationManager {
    nations: RwLock<Vec<Nation>>,
}

impl NationManager {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Nation>> {
        self.nations.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<Nation>> {
        self.nations.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Loads `nations` and `nation_relations`. A relation row for an unknown nation is skipped
    /// (a bad row must not fail the catalog).
    pub async fn load(&self, pool: &MySqlPool) -> Result<()> {
        let mut nations = Vec::new();
        for r in sqlx::query(
            "SELECT CAST(id AS SIGNED) AS id, CAST(expedition_id AS SIGNED) AS expedition_id, \
             CAST(owner_character_id AS SIGNED) AS owner_character_id, owner_name, name, \
             created_at, CAST(allow_change_name AS SIGNED) AS allow_change_name FROM nations ORDER BY id",
        )
        .fetch_all(pool)
        .await?
        {
            nations.push(Nation {
                id: r.try_get::<i64, _>("id")? as u32,
                expedition_id: r.try_get::<i64, _>("expedition_id")? as u32,
                owner_character_id: r.try_get::<i64, _>("owner_character_id")? as u32,
                owner_name: r.try_get("owner_name")?,
                name: r.try_get("name")?,
                created_at: r.try_get("created_at")?,
                allow_change_name: r.try_get::<i64, _>("allow_change_name")? as u8,
                relations: Vec::new(),
                offline_members: Vec::new(),
            });
        }
        let mut index = HashMap::new();
        for (i, nation) in nations.iter().enumerate() {
            index.insert(nation.id, i);
        }
        for r in sqlx::query(
            "SELECT CAST(nation_id AS SIGNED) AS nation_id, \
             CAST(other_faction_id AS SIGNED) AS other_faction_id, \
             CAST(state AS SIGNED) AS state, expire_at, CAST(next_state AS SIGNED) AS next_state \
             FROM nation_relations ORDER BY nation_id, other_faction_id",
        )
        .fetch_all(pool)
        .await?
        {
            let nation_id = r.try_get::<i64, _>("nation_id")? as u32;
            let Some(&i) = index.get(&nation_id) else {
                tracing::warn!("nation_relations row for unknown nation {nation_id} skipped");
                continue;
            };
            nations[i].relations.push(NationRelation {
                other_faction_id: r.try_get::<i64, _>("other_faction_id")? as u32,
                state: r.try_get::<i64, _>("state")? as u8,
                expire_at: r.try_get("expire_at")?,
                next_state: r.try_get::<i64, _>("next_state")? as u8,
            });
        }
        let count = nations.len();
        *self.write() = nations;
        tracing::info!("Loaded {count} nations");
        Ok(())
    }

    pub fn all(&self) -> Vec<Nation> {
        self.read().clone()
    }

    pub fn get(&self, id: u32) -> Option<Nation> {
        self.read().iter().find(|n| n.id == id).cloned()
    }

    pub fn is_nation(&self, id: u32) -> bool {
        self.read().iter().any(|n| n.id == id)
    }

    /// The nation founded by `expedition_id`.
    pub fn for_expedition(&self, expedition_id: u32) -> Option<Nation> {
        self.read()
            .iter()
            .find(|n| n.expedition_id == expedition_id)
            .cloned()
    }

    /// The `SystemFaction` of a nation id (`None` for every other id).
    pub fn faction(&self, id: u32) -> Option<SystemFaction> {
        self.get(id).map(|n| n.system_faction())
    }

    /// The stored relation between `a` and `b` when either side is a nation (the rows are
    /// mirrored for lookups, the client list stays directional).
    pub fn relation(&self, a: u32, b: u32) -> Option<u8> {
        let nations = self.read();
        for nation in nations.iter() {
            if nation.id == a {
                return nation
                    .relations
                    .iter()
                    .find(|r| r.other_faction_id == b)
                    .map(|r| r.state);
            }
            if nation.id == b {
                return nation
                    .relations
                    .iter()
                    .find(|r| r.other_faction_id == a)
                    .map(|r| r.state);
            }
        }
        None
    }

    /// The relation rows of every nation in wire form, for the login `SCFactionRelationList`.
    pub fn relation_rows(&self) -> Vec<FactionRelation> {
        self.read()
            .iter()
            .flat_map(Nation::wire_relations)
            .collect()
    }

    /// `SCFactionList` chunks (20 records each, as the C# `SendFactions`).
    pub fn faction_chunks(&self) -> Vec<Vec<SystemFaction>> {
        let nations: Vec<SystemFaction> = self.read().iter().map(Nation::system_faction).collect();
        nations.chunks(20).map(<[SystemFaction]>::to_vec).collect()
    }

    /// `SCFactionRelationList` chunks (200 rows each; the packet count is a byte).
    pub fn relation_chunks(&self) -> Vec<Vec<FactionRelation>> {
        let rows = self.relation_rows();
        rows.chunks(200).map(<[FactionRelation]>::to_vec).collect()
    }

    pub fn add(&self, nation: Nation) {
        let mut nations = self.write();
        match nations.iter_mut().find(|n| n.id == nation.id) {
            Some(slot) => *slot = nation,
            None => nations.push(nation),
        }
    }

    /// After the creation write committed, the offline members need no further rewrites (their
    /// `characters.faction_id` is already the nation). A failed creation write keeps the list so
    /// the periodic save retries it.
    pub fn clear_offline_members(&self, id: u32) {
        if let Some(nation) = self.write().iter_mut().find(|n| n.id == id) {
            nation.offline_members.clear();
        }
    }

    /// This subsystem's part of `SaveManager.DoSave`: nations, their relation rows, the
    /// founding expedition's `mother` and the offline members' `faction_id`, in one
    /// transaction.
    pub async fn save(
        &self,
        server: &GameServer,
        tx: &mut Transaction<'_, MySql>,
    ) -> Result<usize> {
        let nations = self.all();
        let online: HashSet<u32> = server
            .world
            .all_characters()
            .iter()
            .map(|p| p.character_id)
            .collect();
        let mut written = 0;
        for nation in &nations {
            written += write_nation(tx, nation, &online).await?;
        }
        Ok(written)
    }
}

/// Writes a newly created nation immediately (the GM command awaits it; the independence effect
/// spawns it). The periodic save writes the same rows again.
pub async fn save_creation(server: &GameServer, nation: &Nation) -> Result<()> {
    let online: HashSet<u32> = server
        .world
        .all_characters()
        .iter()
        .map(|p| p.character_id)
        .collect();
    let mut tx = server.db.begin().await?;
    write_nation(&mut tx, nation, &online).await?;
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
async fn write_nation(
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
    // The founding expedition's mother is the nation; keep both membership pointers in the
    // same transaction (the expedition save writes the same value).
    sqlx::query("UPDATE expeditions SET mother = ? WHERE id = ?")
        .bind(nation.id)
        .bind(nation.expedition_id)
        .execute(&mut **tx)
        .await?;
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
    }
    Ok(2 + nation.relations.len() + offline.len())
}

/// `(id, mother_id)` of a static faction or a runtime nation (`None` = an unknown faction).
pub fn faction_of_id(server: &GameServer, id: u32) -> Option<(u32, u32)> {
    if let Some(f) = server.game_data.factions.get(id) {
        return Some((f.id, f.mother_id));
    }
    server
        .systems
        .social
        .nations
        .get(id)
        .map(|nation| (nation.id, 0))
}

/// Whether `id` is a static faction or a runtime nation.
pub fn faction_exists(server: &GameServer, id: u32) -> bool {
    server.game_data.factions.get(id).is_some() || server.systems.social.nations.is_nation(id)
}

/// The relation between `a` and `b`, preferring a nation's runtime rows over the static table.
/// Both loaded forms of the static table are consulted (the social manager's map and the raw
/// `FactionData` rows), because fixtures and loaders fill one or the other.
pub fn relation_between(server: &GameServer, a: u32, b: u32) -> Option<u8> {
    if let Some(state) = server.systems.social.nations.relation(a, b) {
        return Some(state);
    }
    if let Some(state) = server.game_data.systems.social.relations.get(a, b) {
        return Some(state);
    }
    server
        .game_data
        .factions
        .relations
        .iter()
        .find(|r| (r.id == a && r.id2 == b) || (r.id == b && r.id2 == a))
        .map(|r| r.state)
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
        || server
            .game_data
            .factions
            .factions
            .iter()
            .any(|f| f.name == name)
        || server
            .systems
            .social
            .nations
            .all()
            .iter()
            .any(|n| n.name == name)
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
    server.systems.social.nations.add(nation.clone());
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

/// The `SCFactionList` records of all nations (`None` when there is no nation).
pub fn login_factions(server: &GameServer) -> Vec<Vec<SystemFaction>> {
    server.systems.social.nations.faction_chunks()
}

/// The `SCFactionRelationList` rows of all nations (`uiRequest = false`).
pub fn login_relations(server: &GameServer) -> Vec<Vec<FactionRelation>> {
    server.systems.social.nations.relation_chunks()
}

/// Sends the nation records and relation rows of the login burst after the static lists.
pub fn send_login_lists(server: &GameServer, conn: &mut crate::net::connection::GameConnection) {
    for chunk in login_factions(server) {
        conn.send(&SCFactionListPacket { factions: chunk });
    }
    for chunk in login_relations(server) {
        conn.send(&SCFactionRelationListPacket { relations: chunk });
    }
}
