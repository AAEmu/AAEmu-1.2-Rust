//! `Core/Managers/World/FactionManager` relations + `Models/Game/Faction/SystemFaction`
//! (`GetRelationState`, `BaseUnit.GetRelationStateTo`) and the player side of
//! `Unit.SetFaction` / `Character.SetFaction`. The faction list itself (`SendFactions`,
//! `SendRelations`) is loaded by `data::world_data::FactionData`.

use std::collections::HashMap;

use crate::data::world_data::{factions, FactionData};
use crate::data::GameDataError;
use crate::db_character::CrimeFactionChange;
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
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
/// i.e. a null `Faction`). The pure table form; live code uses [`relation_state_live`], which
/// knows the runtime nations.
pub fn relation_state(
    factions_data: &FactionData,
    relations: &FactionRelations,
    me: u32,
    other: u32,
) -> u8 {
    let me_pair = factions_data.get(me).map(|f| (f.id, f.mother_id));
    let other_pair = factions_data.get(other).map(|f| (f.id, f.mother_id));
    let mother_exists =
        me_pair.is_some_and(|(_, mother)| mother != 0 && factions_data.get(mother).is_some());
    relation_state_from(me_pair, other_pair, mother_exists, |a, b| {
        relations.get(a, b)
    })
}

/// `SystemFaction.GetRelationState(otherFaction)` with the runtime nations overlaid on the
/// static `system_factions` table. This is the one implementation the combat, team and AI
/// paths use; a nation id (or an expedition whose mother is a nation) resolves through the
/// nation relation rows instead of falling back to `Neutral`.
pub fn relation_state_live(server: &GameServer, me: u32, other: u32) -> u8 {
    let me_pair = super::nation::faction_of_id(server, me);
    let other_pair = super::nation::faction_of_id(server, other);
    let mother_exists = me_pair
        .is_some_and(|(_, mother)| mother != 0 && super::nation::faction_exists(server, mother));
    relation_state_from(me_pair, other_pair, mother_exists, |a, b| {
        super::nation::relation_between(server, a, b)
    })
}

/// The common `GetRelationState` body. `me` / `other` are `(id, mother_id)` pairs (`None` =
/// a null `Faction`); `mother_exists` is the C# `GetFaction(MotherId) != null` check.
fn relation_state_from(
    me: Option<(u32, u32)>,
    other: Option<(u32, u32)>,
    mother_exists: bool,
    relation: impl Fn(u32, u32) -> Option<u8>,
) -> u8 {
    let (Some((me_id, me_mother)), Some((other_id, other_mother))) = (me, other) else {
        // `this.Faction?.GetRelationState(..) ?? Neutral`.
        return relation_state::NEUTRAL;
    };
    let faction_id = if me_mother != factions::INVALID {
        me_mother
    } else {
        me_id
    };
    let other_id = if other_mother != 0 {
        other_mother
    } else {
        other_id
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
    if me_mother != 0 && mother_exists {
        // TODO (C#): not found, so enemy (id = [1, 2, 3])?
        return relation(me_mother, other_id).unwrap_or(relation_state::FRIENDLY);
    }
    relation(me_id, other_id).unwrap_or(relation_state::NEUTRAL)
}

/// `BaseUnit.GetRelationStateTo(unit)` for two players.
pub fn player_relation(server: &GameServer, me: &PlayerRef, other: &PlayerRef) -> u8 {
    let a = me.data().unit.faction_id;
    let b = other.data().unit.faction_id;
    relation_state_live(server, a, b)
}

/// `Character.SetFaction(factionId)` (+ `Unit.SetFaction`): broadcasts
/// `SCUnitFactionChangedPacket`, adds / removes the Contemptuous buff, updates owned houses,
/// removes the character from a guild of another alliance (`expedition::leave_forced`; the C#
/// `Kick(p, p)` removed nobody) and from its party
/// (`team::TeamManager::member_remove_for_faction_change`).
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
            if crate::systems::social::util::alliance_id(server, e.faction.mother_id)
                != crate::systems::social::util::alliance_id(server, faction_id)
            {
                crate::systems::social::expedition::leave_forced(server, p).await;
            }
        }
    }
    // Looked up by membership, not `in_party`: the login infamy conversion runs before
    // `UpdateAtLogin` (see `member_remove_for_faction_change`).
    server
        .systems
        .social
        .teams
        .member_remove_for_faction_change(server, p);
}

/// `Character.SetFaction` for a synchronous caller (the GM `/setfaction`, the pirate
/// conversion / pardon): a **persistent** change (saved to `characters.faction_id`, ends any
/// temporary faction) with the character side effects of [`set_faction`] - owned houses, the
/// guild removal (spawned) and party removal.
///
/// Guild owner (PIRATE-1): [`expedition::leave_forced`] hands the leadership to a same-faction
/// successor and removes the owner; only an owner who was the last eligible member dismisses
/// the guild. A returning pirate guild owner therefore passes the pirate guild on and leaves it
/// (the guild's mother faction 161 differs from the home faction); the guild keeps its
/// territory / siege / nation.
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
    let expedition = p.data().unit.expedition_id;
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
        if mother.is_some_and(|m| {
            crate::systems::social::util::alliance_id(server, m)
                != crate::systems::social::util::alliance_id(server, faction_id)
        }) {
            if let Some(s) = server.self_arc() {
                let p = p.clone();
                tokio::spawn(async move {
                    crate::systems::social::expedition::leave_forced(&s, &p).await;
                });
            }
        }
    }
    server
        .systems
        .social
        .teams
        .member_remove_for_faction_change(server, p);
}

/// Applies the faction consequences of a crime-point change once the caller released the
/// character's data lock (the decision is made by
/// [`Character::check_wanted_threshold`](crate::db_character::Character::check_wanted_threshold)):
///
/// * `became_pirate`: `SetFaction(Pirate)`.
/// * `pardoned`: return to the faction of the character's race/gender template - the home
///   faction, exactly what the trial's not-guilty path did (`trial.rs`, now folded into this
///   helper). Position is unchanged; the house / guild / party handling of
///   [`set_faction_persistent`] applies as-is. The 1.2 client prints ui_text 2297
///   `pardon_pirate` itself when the `SCUnitFactionChangedPacket` it receives has old faction
///   161 and a non-pirate new faction (`x2game.dll.c:617340-617367`); the server has no
///   separate ui-text packet on this or the trial path. Then the "Repentant" achievement
///   record (kind 49 `MY_DISHONORABLE_POINT`, `char_records` 2401) is reported so achievement
///   1380 completes.
///
/// Evidence: item 32212's description ("reach 0 Infamy Points to leave the pirate alliance and
/// rejoin your original faction") and the C# trial verdict check (`TrialManager.cs:818`,
/// `a2ab80abd^`); the every-decrease check and the kept position are INFERRED from the item
/// text plus the C# `SetFaction` having no teleport (research `pirate-return.md`).
pub fn apply_crime_faction_change(server: &GameServer, p: &PlayerRef, change: CrimeFactionChange) {
    if change.became_pirate {
        set_faction_persistent(server, p, factions::PIRATE);
    }
    if !change.pardoned {
        return;
    }
    let (race, gender) = {
        let c = p.data();
        (c.race, c.gender)
    };
    match server.game_data.characters.template(race, gender) {
        Some(template) => {
            set_faction_persistent(server, p, template.faction_id);
            crate::systems::report_achievement(
                server,
                p,
                AchievementInput::Absolute {
                    kind_id: char_record_kind::MY_DISHONORABLE_POINT,
                    value: 1,
                    matcher: RecordMatch::Any,
                },
            );
        }
        None => tracing::error!(
            "Pardon: no character template for race {race} / gender {gender} (character {})",
            p.character_id
        ),
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

    /// The pardon returns the character through [`set_faction_persistent`], so its existing
    /// house / guild / party handling still runs: an owned house follows the returned pirate to
    /// the home faction. (Guild owner: see the `set_faction_persistent` doc.)
    #[tokio::test]
    async fn a_pardoned_pirate_takes_the_set_faction_house_handling() {
        use crate::data::character_templates::CharacterTemplate;
        use crate::data::GameData;
        use crate::db_character::CrimeFactionChange;
        use crate::models::house::House;
        use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
        use crate::world::Entity;

        let mut data = GameData::empty();
        for id in [101u32, factions::PIRATE] {
            data.factions.add(SystemFaction {
                id,
                ..SystemFaction::default()
            });
        }
        data.characters.templates.insert(
            0x11,
            CharacterTemplate {
                race: 1,
                gender: 1,
                creatable: true,
                faction_id: 101,
                ..Default::default()
            },
        );
        let server = std::sync::Arc::new(test_server_with(data));
        server.set_self();
        crate::systems::social::install_hooks(&server);

        let mut client = TestClient::new(&server, 1, 1001);
        let mut character = test_character(1, "Pirate", 100.0, 100.0);
        character.unit.faction_id = factions::PIRATE;
        let p = spawn_player(&server, &client, character, 0x100);
        client.drain();

        let mut house = House::new(1, 100);
        house.unit.owner_id = p.character_id;
        house.unit.faction_id = factions::PIRATE;
        let house = Entity::new(0x500, house);
        assert!(server.systems.housing.manager.insert(&house));

        apply_crime_faction_change(
            &server,
            &p,
            CrimeFactionChange {
                pardoned: true,
                ..CrimeFactionChange::default()
            },
        );

        assert_eq!(p.data().unit.faction_id, 101);
        assert_eq!(
            house.data().unit.faction_id,
            101,
            "owned houses follow the new faction"
        );
    }
}
