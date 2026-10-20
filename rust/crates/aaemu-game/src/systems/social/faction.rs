//! `Core/Managers/World/FactionManager` relations + `Models/Game/Faction/SystemFaction`
//! (`GetRelationState`, `BaseUnit.GetRelationStateTo`) and the player side of
//! `Unit.SetFaction` / `Character.SetFaction`. The faction list itself (`SendFactions`,
//! `SendRelations`) is loaded by `data::world_data::FactionData`.

use std::collections::HashMap;

use crate::data::world_data::{factions, FactionData};
use crate::data::GameDataError;
use crate::server::GameServer;
use crate::systems::DataLoadCtx;
use crate::world::PlayerRef;

/// `RelationState`.
pub mod relation_state {
    pub const HOSTILE: u8 = 1;
    pub const NEUTRAL: u8 = 2;
    pub const FRIENDLY: u8 = 3;
}

/// `BuffConstants.Contemptuous`.
pub const BUFF_CONTEMPTUOUS: u32 = 4832;

/// `SystemFaction.Relations` of every faction (`faction -> other -> state`), built like
/// `FactionManager.Load` (each `system_faction_relations` row is added to both factions;
/// `Dictionary.Add` throws on a repeated pair).
#[derive(Debug, Clone, Default)]
pub struct FactionRelations {
    relations: HashMap<u32, HashMap<u32, u8>>,
}

impl FactionRelations {
    /// The relation part of `FactionManager.Load` (`system_faction_relations`).
    pub fn load(ctx: &DataLoadCtx<'_>) -> Result<Self, GameDataError> {
        let mut rows = Vec::new();
        let mut stmt = ctx
            .conn
            .prepare("SELECT faction1_id, faction2_id, state_id FROM system_faction_relations")?;
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            let a: i64 = r.get(0)?;
            let b: i64 = r.get(1)?;
            let s: i64 = r.get(2)?;
            rows.push((a as u32, b as u32, s as u8));
        }
        Self::from_rows(&rows)
            .map_err(|e| GameDataError::File(format!("system_faction_relations: {e}")))
    }

    /// Builds the table from `(faction1, faction2, state)` rows.
    pub fn from_rows(rows: &[(u32, u32, u8)]) -> Result<Self, String> {
        let mut t = Self::default();
        for &(a, b, s) in rows {
            for (x, y) in [(a, b), (b, a)] {
                if t.relations.entry(x).or_default().insert(y, s).is_some() {
                    return Err(format!("duplicate relation {x} -> {y}"));
                }
            }
        }
        Ok(t)
    }

    /// `faction.Relations.TryGetValue(other)`.
    pub fn get(&self, faction: u32, other: u32) -> Option<u8> {
        self.relations.get(&faction)?.get(&other).copied()
    }

    pub fn len(&self) -> usize {
        self.relations.values().map(HashMap::len).sum::<usize>() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.relations.is_empty()
    }
}

/// `SystemFaction.GetRelationState(otherFaction)` for two faction ids (`None` = unknown faction,
/// i.e. a null `Faction`).
pub fn relation_state(
    factions_data: &FactionData,
    relations: &FactionRelations,
    me: u32,
    other: u32,
) -> u8 {
    let Some(me) = factions_data.get(me) else {
        // `this.Faction?.GetRelationState(..) ?? Neutral`.
        return relation_state::NEUTRAL;
    };
    let Some(other) = factions_data.get(other) else {
        return relation_state::NEUTRAL;
    };
    let faction_id = if me.mother_id != factions::INVALID {
        me.mother_id
    } else {
        me.id
    };
    let other_id = if other.mother_id != 0 {
        other.mother_id
    } else {
        other.id
    };
    for id in [faction_id, other_id] {
        match id {
            factions::NEUTRAL => return relation_state::NEUTRAL,
            factions::FRIENDLY => return relation_state::FRIENDLY,
            factions::HOSTILE => return relation_state::HOSTILE,
            _ => {}
        }
    }
    if faction_id == other_id {
        return relation_state::FRIENDLY;
    }
    if me.mother_id != 0 && factions_data.get(me.mother_id).is_some() {
        // TODO (C#): not found, so enemy (id = [1, 2, 3])?
        return relations
            .get(me.mother_id, other_id)
            .unwrap_or(relation_state::FRIENDLY);
    }
    relations
        .get(me.id, other_id)
        .unwrap_or(relation_state::NEUTRAL)
}

/// `BaseUnit.GetRelationStateTo(unit)` for two players.
pub fn player_relation(server: &GameServer, me: &PlayerRef, other: &PlayerRef) -> u8 {
    let a = me.data().unit.faction_id;
    let b = other.data().unit.faction_id;
    relation_state(
        &server.game_data.factions,
        &server.game_data.systems.social.relations,
        a,
        b,
    )
}

/// `Character.SetFaction(factionId)` (+ `Unit.SetFaction`): broadcasts
/// `SCUnitFactionChangedPacket`, adds / removes the Contemptuous buff, updates owned houses,
/// kicks the character from a guild of another mother faction (`ExpeditionManager.Kick` with
/// its own connection) and from its party.
pub async fn set_faction(server: &GameServer, p: &PlayerRef, faction_id: u32) {
    let (old, exp) = {
        let c = p.data();
        (c.unit.faction_id, c.unit.expedition_id)
    };
    // Character.SetFaction: `if (Faction.Id == factionId) return;`
    if old == faction_id {
        return;
    }
    {
        let mut c = p.data();
        c.origin_faction_name = c.faction_name.clone();
    }
    let obj = crate::world::WorldObject::from(p);
    crate::systems::skills::combat::set_faction_with_temp(server, &obj, faction_id, false);
    crate::systems::dispatch_buff_events(server, &obj);
    // Character.SetFaction: houses, guild, party.
    server
        .systems
        .social
        .hooks
        .get()
        .update_owned_housing_faction(server, p.character_id, faction_id);
    if exp != 0 {
        if let Some(e) = server.expeditions.get(exp) {
            if e.faction.mother_id != faction_id {
                crate::systems::social::expedition::kick(server, p, p.character_id).await;
            }
        }
    }
    if p.data().in_party {
        server.systems.social.teams.member_remove_from_team(
            server,
            p,
            p,
            crate::systems::social::team::risky_action::KICK,
        );
    }
}

/// `Character.SetFaction` for a synchronous caller (the GM `/setfaction`): a **persistent**
/// change (saved to `characters.faction_id`, ends any temporary faction) with the character
/// side effects of [`set_faction`] - owned houses, guild kick (spawned, it needs the
/// connection-less expedition path) and party removal.
pub fn set_faction_persistent(server: &GameServer, p: &PlayerRef, faction_id: u32) {
    // One short lock: a second `p.data()` in the same expression would self-deadlock.
    let (current, temporary) = {
        let c = p.data();
        (c.unit.faction_id, c.unit.faction_temporary)
    };
    if current == faction_id && !temporary {
        return;
    }
    let obj = crate::world::WorldObject::from(p);
    crate::systems::skills::combat::set_faction_with_temp(server, &obj, faction_id, false);
    crate::systems::dispatch_buff_events(server, &obj);
    let (expedition, in_party) = {
        let c = p.data();
        (c.unit.expedition_id, c.in_party)
    };
    server
        .systems
        .social
        .hooks
        .get()
        .update_owned_housing_faction(server, p.character_id, faction_id);
    if expedition != 0 {
        let mother = server
            .expeditions
            .get(expedition)
            .map(|e| e.faction.mother_id);
        if mother.is_some_and(|m| m != faction_id) {
            if let Some(s) = server.self_arc() {
                let p = p.clone();
                tokio::spawn(async move {
                    crate::systems::social::expedition::kick(&s, &p, p.character_id).await;
                });
            }
        }
    }
    if in_party {
        server.systems.social.teams.member_remove_from_team(
            server,
            p,
            p,
            crate::systems::social::team::risky_action::KICK,
        );
    }
}

/// A temporary faction change (duel Red / Blue team, battlefield corps) that is **not**
/// persisted and has none of `Character.SetFaction`'s side effects: no housing faction update,
/// no expedition / guild kick and no party removal. The broadcast carries `temp = true`; the
/// first origin faction survives nested temporary changes.
///
/// Deviation from the 1.2 C# (RPA-09): the C# reused `Character.SetFaction`, so a duel or
/// battlefield changed the saved `characters.faction_id`, updated houses and could expel a
/// guild member whose guild mother faction differed.
pub fn set_temporary_faction(server: &GameServer, p: &PlayerRef, faction_id: u32) {
    if p.data().unit.faction_id == faction_id {
        return;
    }
    let obj = crate::world::WorldObject::from(p);
    crate::systems::skills::combat::set_faction_with_temp(server, &obj, faction_id, true);
    crate::systems::dispatch_buff_events(server, &obj);
}

/// Restores the faction of an active temporary change (`set_temporary_faction`). A no-op when
/// no temporary change is active (already restored on end / disconnect / leave).
pub fn restore_temporary_faction(server: &GameServer, p: &PlayerRef) {
    let origin = {
        let c = p.data();
        if !c.unit.faction_temporary {
            return;
        }
        c.unit.origin_faction_id
    };
    if origin == 0 {
        tracing::error!(
            "RestoreFaction: {} ({}) has a temporary faction without an origin",
            p.data().unit.name,
            p.character_id
        );
        return;
    }
    let obj = crate::world::WorldObject::from(p);
    crate::systems::skills::combat::set_faction_with_temp(server, &obj, origin, false);
    crate::systems::dispatch_buff_events(server, &obj);
}

/// Ends one temporary change that was started from `saved` (the faction the player had when it
/// began). When `saved` is the origin faction this is `restore_temporary_faction`; when it is
/// itself temporary (a duel inside a battlefield) the player returns to it and stays temporary
/// until the outer change ends. A no-op when no temporary change is active any more (the
/// battlefield was left or the player disconnected during the duel).
pub fn restore_temporary_faction_to(server: &GameServer, p: &PlayerRef, saved: u32) {
    let (temporary, origin, current) = {
        let c = p.data();
        (
            c.unit.faction_temporary,
            c.unit.origin_faction_id,
            c.unit.faction_id,
        )
    };
    if !temporary {
        return;
    }
    if saved == origin || saved == 0 {
        restore_temporary_faction(server, p);
    } else if saved != current {
        set_temporary_faction(server, p, saved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::world_data::SystemFaction;

    fn data() -> (FactionData, FactionRelations) {
        let mut d = FactionData::default();
        for (id, mother) in [
            (1, 0),
            (2, 0),
            (3, 0),
            (101, 148),
            (102, 149),
            (148, 0),
            (149, 0),
            (161, 0),
            (200, 2),
            (300, 0),
        ] {
            d.add(SystemFaction {
                id,
                mother_id: mother,
                ..SystemFaction::default()
            });
        }
        let r =
            FactionRelations::from_rows(&[(148, 149, 1), (148, 161, 1), (300, 148, 3)]).unwrap();
        (d, r)
    }

    #[test]
    fn relation_states_follow_the_csharp_rules() {
        let (d, r) = data();
        use relation_state::*;
        // Same mother faction.
        assert_eq!(relation_state(&d, &r, 101, 148), FRIENDLY);
        // Mother relation table (both sides added).
        assert_eq!(relation_state(&d, &r, 101, 102), HOSTILE);
        assert_eq!(relation_state(&d, &r, 102, 101), HOSTILE);
        // Mother faction without an entry -> Friendly.
        assert_eq!(relation_state(&d, &r, 101, 300), FRIENDLY);
        // Own relations without a mother.
        assert_eq!(relation_state(&d, &r, 300, 101), FRIENDLY);
        assert_eq!(relation_state(&d, &r, 300, 161), NEUTRAL);
        // Root factions.
        assert_eq!(relation_state(&d, &r, 200, 148), NEUTRAL);
        assert_eq!(relation_state(&d, &r, 148, 3), HOSTILE);
        assert_eq!(relation_state(&d, &r, 1, 149), FRIENDLY);
        // Unknown factions.
        assert_eq!(relation_state(&d, &r, 999, 148), NEUTRAL);
        assert_eq!(relation_state(&d, &r, 148, 999), NEUTRAL);
    }

    #[test]
    fn duplicate_relations_are_rejected() {
        assert!(FactionRelations::from_rows(&[(1, 2, 1), (2, 1, 1)]).is_err());
        let r = FactionRelations::from_rows(&[(1, 2, 1), (1, 3, 2)]).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r.get(3, 1), Some(2));
    }

    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    fn real_compact_db_social_relations() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn = rusqlite::Connection::open(path).expect("open compact db");
        let files = crate::data::client_files::ClientFileManager::from_sources(
            &[],
            std::path::Path::new("."),
        );
        let ctx = DataLoadCtx {
            conn: &conn,
            app_path: std::path::Path::new("."),
            files: &files,
            language: "en_us",
        };
        let r = FactionRelations::load(&ctx).expect("relations");
        assert!(!r.is_empty());
        // Nuia and Haranya are hostile in 1.2.
        assert_eq!(r.get(148, 149), Some(relation_state::HOSTILE));
    }
}
