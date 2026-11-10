//! Faction relations and attack permission: `SystemFaction.GetRelationState`,
//! `BaseUnit.GetRelationStateTo`, `BaseUnit.CanAttack`, `GetOwnerCharacter`,
//! `SkillTargetingUtil.FilterWithRelation` / `IsRelationValid`.

use crate::data::world_data::conflict_type;
use crate::server::GameServer;
use crate::systems::social::nation;
use crate::world::{now_ms, PlayerRef, WorldObject};

use super::geometry::Target;
use super::types::{buff_constants, RelationState, SkillTargetRelation};

/// `FactionsEnum` root factions.
const FACTION_NEUTRAL: u32 = 2;

/// `WorldManager.DefaultCombatTimeout` (seconds).
pub const DEFAULT_COMBAT_TIMEOUT_MS: i64 = 15_000;

fn relation_from_raw(v: u8) -> RelationState {
    RelationState::from_raw(i64::from(v)).unwrap_or(RelationState::Neutral)
}

/// `SystemFaction.GetRelationState(other)` for faction ids (0 / unknown ids = null factions).
/// Runtime nations are resolved by the social subsystem's shared implementation.
pub fn faction_relation(server: &GameServer, own: u32, other: u32) -> RelationState {
    relation_from_raw(crate::systems::social::faction::relation_state_live(
        server, own, other,
    ))
}

/// The faction id of a target (0 for position units / no faction).
pub fn faction_of(t: &Target) -> u32 {
    match t {
        Target::Obj(o) => o.with_base_unit(|b| b.faction_id()),
        Target::Pos(_) => 0,
    }
}

/// `BaseUnit.GetRelationStateTo(unit)`.
pub fn relation_to(server: &GameServer, a: &Target, b: &Target) -> RelationState {
    faction_relation(server, faction_of(a), faction_of(b))
}

/// `GetOwnerCharacter()`: the character itself, the owner of a mate / slave, else `None`.
pub fn owner_character(server: &GameServer, o: &WorldObject) -> Option<PlayerRef> {
    match o {
        WorldObject::Player(p) => Some(p.clone()),
        WorldObject::Mate(m) => {
            let owner = m.data().owner_obj_id;
            (owner > 0)
                .then(|| server.world.object(owner))
                .flatten()
                .and_then(|o| owner_character(server, &o))
        }
        WorldObject::Slave(s) => {
            let (summoner, owner) = {
                let d = s.data();
                (d.summoner_obj_id, d.owner_obj_id)
            };
            let id = if summoner != 0 { summoner } else { owner };
            (id > 0)
                .then(|| server.world.object(id))
                .flatten()
                .and_then(|o| owner_character(server, &o))
        }
        _ => None,
    }
}

/// `Character.IsActivelyHostile(target)`.
pub fn is_actively_hostile(me: &PlayerRef, target_obj_id: u32) -> bool {
    let now = now_ms();
    me.data()
        .skill_char
        .hostile_players
        .iter()
        .find(|(id, _)| *id == target_obj_id)
        .is_some_and(|(_, t)| t + DEFAULT_COMBAT_TIMEOUT_MS > now)
}

/// The target's zone group is an instance whose `indun_zones` row has `pvp = false`.
fn is_no_pvp_zone(server: &GameServer, target: &Target) -> bool {
    let (_, zone_key) = super::geometry::instance_and_zone(target);
    let Some(group_id) = server
        .game_data
        .zones
        .zone_by_key(zone_key)
        .map(|z| z.group_id)
    else {
        return false;
    };
    server
        .game_data
        .systems
        .instances
        .indun
        .dungeon_zone(group_id)
        .is_some_and(|zone| !zone.pvp)
}

/// The target's zone group is in the `Peace` state of the zone-conflict cycle
/// (`conflict_zones`, `systems/social/zone_conflict.rs`). The zone is read from the target,
/// like every other zone rule in [`can_attack`] (`is_no_pvp_zone`, the zone-faction protection
/// and the C# `BaseUnit.CanAttack`): the region being protected is the target's.
fn is_peace_zone(server: &GameServer, target: &Target) -> bool {
    let (_, zone_key) = super::geometry::instance_and_zone(target);
    let Some(group_id) = server
        .game_data
        .zones
        .zone_by_key(zone_key)
        .map(|z| z.group_id)
    else {
        return false;
    };
    server
        .systems
        .social
        .conflicts
        .get(group_id)
        .is_some_and(|c| c.current_state == conflict_type::PEACE)
}

/// Whether the cycling-zone Peace rule lets `attacker` harm `target` (both resolved to their
/// characters; a pet / slave / siege weapon counts as its owner's attack).
///
/// The rule is the owner's 2026-11-10 recollection of EU 1.2 and covers only the cycling
/// conflict zones (`conflict_zones`): a zone in the Peace state is an absolute PvP lockdown and
/// pirates are fully protected too (a pirate cannot be attacked and cannot attack). The
/// "except for pirates" of `ui_texts` 5195 `state_peace_tooltip` is about the permanently safe
/// faction-controlled home zones, which have no conflict cycle at all; pirate attacks there are
/// ordinary hostile attacks and must not be blocked (see `PORTING.md`).
///
/// The remaining exceptions are the owner's clarification:
/// * the two characters are duelling each other (`DuelManager.IsInDuelWith`);
/// * their factions have an explicit hostile diplomacy relation. Only the runtime nation rows
///   (`NationManager.relation`, written by `CSFactionDeclareHostile`) count: the static
///   `system_faction_relations` table makes Nuia and Haranya hostile everywhere, and using it
///   would leave Peace blocking nothing but friendly fire.
///
/// Force attack / Retribution never reach this function's callers' shortcuts: the caller checks
/// this rule before those flags turn a friendly pair attackable.
fn peace_allows_attack(server: &GameServer, attacker: &PlayerRef, target: &PlayerRef) -> bool {
    // (a) A duel between exactly these two characters. The duel team factions are hostile, so
    // this must run before the peace refusal (and it covers the duel's own units too).
    if server
        .systems
        .social
        .duels
        .is_in_duel_with(attacker.character_id, target.character_id)
    {
        return true;
    }
    // (b) A declared hostile diplomacy relation (a runtime nation row).
    let nations = &server.systems.social.nations;
    let a = attacker.data().unit.faction_id;
    let b = target.data().unit.faction_id;
    (nations.is_nation(a) || nations.is_nation(b))
        && faction_relation(server, a, b) == RelationState::Hostile
}

/// `BaseUnit.CanAttack(target)`.
pub fn can_attack(server: &GameServer, me: &Target, target: &Target) -> bool {
    if let Some(allowed) =
        crate::systems::housing::dominion::runtime::can_attack_tower(server, me, target)
    {
        return allowed;
    }
    // Peace period: a player-controlled attacker (player, pet, summon, slave, siege weapon)
    // cannot harm another player or that player's units. This runs before every shortcut that
    // would otherwise allow the pair (force attack, Retribution) and before the faction checks,
    // so it also covers an unknown / null faction. NPC attackers are unchanged (their
    // `owner_character` is `None`).
    if let Some(attacker) = me.as_obj().and_then(|o| owner_character(server, o)) {
        if is_peace_zone(server, target) {
            if let Some(defender) = target.as_obj().and_then(|o| owner_character(server, o)) {
                if !peace_allows_attack(server, &attacker, &defender) {
                    return false;
                }
            }
        }
    }
    let my_faction = faction_of(me);
    let target_faction = faction_of(target);
    let factions = &server.game_data.factions;
    if !nation::faction_exists(server, my_faction)
        || !nation::faction_exists(server, target_faction)
    {
        return true; // this.Faction == null || target.Faction == null
    }
    if me.obj_id() == target.obj_id() {
        return false;
    }
    // Never your own mount / slave (or yourself through one), not even with Ctrl+F force attack
    // or a Retribution flag, which would otherwise make the friendly pair attackable and raise
    // Retribution on the owner (Arc Lightning hitting the caster's own mount).
    if let (Some(me_owner), Some(target_owner)) = (
        me.as_obj().and_then(|o| owner_character(server, o)),
        target.as_obj().and_then(|o| owner_character(server, o)),
    ) {
        if me_owner.obj_id == target_owner.obj_id {
            return false;
        }
    }
    // `indun_zones.pvp = false`: the 1.2 client resolves a character-owned pair as
    // non-hostile before faction logic (`FUN_390281c0` stores `pvp == 0` in the relation
    // context; `FUN_39837a40` returns that flag and `FUN_39838880` then yields relation
    // kind 2 and returns early). The server rule is inferred from that; `EnforceNoPvp`
    // restores the faction-only decision.
    if server.config.dungeons.enforce_no_pvp {
        let me_owner = me.as_obj().and_then(|o| owner_character(server, o));
        let target_owner = target.as_obj().and_then(|o| owner_character(server, o));
        if me_owner.is_some() && target_owner.is_some() && is_no_pvp_zone(server, target) {
            return false;
        }
    }
    let relation = relation_to(server, me, target);
    let me_char = me.as_obj().and_then(|o| o.as_player().cloned());
    let target_owner = target.as_obj().and_then(|o| owner_character(server, o));
    let zone_key = super::geometry::instance_and_zone(target).1;
    let zone_faction_raw = server
        .game_data
        .zones
        .zone_by_key(zone_key)
        .map_or(FACTION_NEUTRAL, |z| z.faction_id);
    let zone_faction_id = if zone_faction_raw == 0 {
        FACTION_NEUTRAL
    } else {
        zone_faction_raw
    };
    let zone_faction = factions
        .get(zone_faction_id)
        .or_else(|| factions.get(FACTION_NEUTRAL));
    let target_mother = nation::faction_of_id(server, target_faction).map_or(0, |f| f.1);
    if me_char.is_some() && target_mother != 0 {
        if let Some(z) = zone_faction {
            if target_mother == z.mother_id || target_mother == z.id {
                return false; // protected by the mother zone
            }
        }
    }
    if let (Some(me_c), Some(owner)) = (&me_char, &target_owner) {
        let (owner_faction, owner_id, owner_obj) = {
            let d = owner.data();
            (d.unit.faction_id, d.id, owner.obj_id)
        };
        let trg_flagged = owner
            .data()
            .unit
            .buffs
            .check_buff(buff_constants::RETRIBUTION);
        let owner_mother = nation::faction_of_id(server, owner_faction).map_or(0, |f| f.1);
        if owner_mother != 0
            && owner_mother == zone_faction_id
            && !is_actively_hostile(me_c, owner_obj)
            && !trg_flagged
        {
            return false;
        }
        let (me_id, force_attack) = {
            let d = me_c.data();
            (d.id, d.unit.force_attack)
        };
        let is_team = server
            .systems
            .skills
            .ext
            .social()
            .are_team_members(server, me_id, owner_id);
        // flagged (Retribution) target, or Ctrl+F force attack, on a friendly non-team member
        if (trg_flagged || force_attack) && !is_team && relation == RelationState::Friendly {
            return true;
        }
    }
    relation == RelationState::Hostile
}

/// `SkillTargetingUtil.FilterWithRelation(relation, caster, units)`.
pub fn filter_with_relation(
    server: &GameServer,
    relation: SkillTargetRelation,
    caster: &Target,
    units: Vec<WorldObject>,
) -> Vec<WorldObject> {
    match relation {
        SkillTargetRelation::Any | SkillTargetRelation::Party => units,
        SkillTargetRelation::Friendly => units
            .into_iter()
            .filter(|o| {
                let t = Target::Obj(o.clone());
                relation_to(server, caster, &t) == RelationState::Friendly
                    && !can_attack(server, caster, &t)
            })
            .collect(),
        SkillTargetRelation::Hostile => units
            .into_iter()
            .filter(|o| can_attack(server, caster, &Target::Obj(o.clone())))
            .collect(),
        SkillTargetRelation::Raid => {
            let social = server.systems.skills.ext.social();
            let team = social.team_of(server, caster.obj_id());
            let mut out: Vec<WorldObject> = if team == 0 {
                // units.Where(o => o.ObjId == mate?.ObjId): the first active mate
                let mate = caster
                    .as_obj()
                    .and_then(WorldObject::as_player)
                    .and_then(|p| crate::systems::vehicles::mates::first_active_mate(server, p))
                    .map(|m| m.obj_id);
                units
                    .into_iter()
                    .filter(|o| Some(o.obj_id()) == mate)
                    .collect()
            } else {
                units
                    .into_iter()
                    .filter(|o| social.is_team_obj_member(server, team, o.obj_id()))
                    .collect()
            };
            if let Target::Obj(c) = caster {
                out.push(c.clone()); // .Append(caster)
            }
            out
        }
        SkillTargetRelation::Others => units
            .into_iter()
            .filter(|o| {
                relation_to(server, caster, &Target::Obj(o.clone())) == RelationState::Neutral
            })
            .collect(),
    }
}

/// `tick_area_relation_id` 6 (Arc Lightning / Electric Shock 250, 5847, 7465: "Inflicts damage on
/// you and allies within 2m"). It is not a `SkillTargetRelation` of the C# enum (the C# fell
/// through `default => units`, so every unit in the radius, the caster and its mount included,
/// received the spread DoT and the damage). The buff text says the DoT jumps from its holder to
/// the HOLDER's allies, so the relation is read from the holder, not from the buff's caster.
pub const TICK_AREA_HOLDER_ALLIES: u32 = 6;

/// `tick_area_relation_id == TICK_AREA_HOLDER_ALLIES`: the holder itself and the units around it
/// that are friendly to the holder. The caster, its mates / slaves and anything else owned by the
/// caster's character never qualify, and the caster must be allowed to attack the unit (a harmful
/// spread never lands on a unit the caster may not hit, so it cannot raise Retribution).
pub fn filter_holder_allies(
    server: &GameServer,
    holder: &WorldObject,
    caster: &WorldObject,
    units: Vec<WorldObject>,
) -> Vec<WorldObject> {
    let holder_t = Target::Obj(holder.clone());
    let caster_t = Target::Obj(caster.clone());
    let caster_owner = owner_character(server, caster).map(|p| p.obj_id);
    units
        .into_iter()
        .filter(|u| {
            if u.same(holder) {
                return true;
            }
            if u.same(caster)
                || caster_owner.is_some_and(|c| {
                    u.obj_id() == c || owner_character(server, u).is_some_and(|o| o.obj_id == c)
                })
            {
                return false;
            }
            let t = Target::Obj(u.clone());
            relation_to(server, &holder_t, &t) == RelationState::Friendly
                && !can_attack(server, &holder_t, &t)
                && can_attack(server, &caster_t, &t)
        })
        .collect()
}

/// `SkillTargetingUtil.IsRelationValid(relation, caster, target)`.
pub fn is_relation_valid(
    server: &GameServer,
    relation: SkillTargetRelation,
    caster: &Target,
    target: &Target,
) -> bool {
    match relation {
        SkillTargetRelation::Any | SkillTargetRelation::Party => true,
        SkillTargetRelation::Friendly => {
            relation_to(server, caster, target) == RelationState::Friendly
                && !can_attack(server, caster, target)
        }
        SkillTargetRelation::Hostile => can_attack(server, caster, target),
        SkillTargetRelation::Raid => {
            if caster.same(target) {
                return true;
            }
            let social = server.systems.skills.ext.social();
            let team = social.team_of(server, caster.obj_id());
            team != 0 && social.is_team_obj_member(server, team, target.obj_id())
        }
        SkillTargetRelation::Others => caster.obj_id() != target.obj_id(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::world_data::{factions, FactionRelation, SystemFaction};
    use crate::data::GameData;

    fn faction(id: u32, mother: u32) -> SystemFaction {
        SystemFaction {
            id,
            mother_id: mother,
            ..SystemFaction::default()
        }
    }

    #[tokio::test]
    async fn relation_rules() {
        let mut data = GameData::empty();
        for f in [
            faction(1, 0),
            faction(2, 0),
            faction(3, 0),
            faction(101, 0),
            faction(109, 0),
            faction(148, 0),
            faction(150, 101),
        ] {
            data.factions.add(f);
        }
        data.factions.relations.push(FactionRelation {
            id: 101,
            id2: 109,
            state: RelationState::Hostile as u8,
            exp_time: 0,
        });
        let server = crate::test_support::test_server_with(data);
        assert_eq!(faction_relation(&server, 101, 101), RelationState::Friendly);
        assert_eq!(faction_relation(&server, 109, 101), RelationState::Hostile);
        assert_eq!(faction_relation(&server, 101, 148), RelationState::Neutral);
        assert_eq!(faction_relation(&server, 101, 3), RelationState::Hostile);
        assert_eq!(faction_relation(&server, 2, 3), RelationState::Neutral);
        // mother faction 101 is known: its relation table decides, default Friendly
        assert_eq!(faction_relation(&server, 150, 109), RelationState::Hostile);
        assert_eq!(faction_relation(&server, 150, 148), RelationState::Friendly);
        assert_eq!(faction_relation(&server, 0, 101), RelationState::Neutral);
    }

    /// The real 1.2 factions of the owner-session report: a Harani player (109) sees the
    /// neutral machine Amutar (3443, faction 2) as neutral, the West Ishvara Mutani (5772,
    /// faction 109) and the friendly guide Lucius (12078, faction 1) as friendly, and the
    /// kobold miners (3350, faction 115) as hostile. This is the mapping the client's target
    /// frame colours by (`GetCombatRelationshipStr`, decompile `FUN_394c0d80` -> `FUN_39838880`).
    #[tokio::test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    async fn real_compact_db_faction_relation_for_harani_player() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        use crate::data::world_data::{FactionData, Localization};
        use rusqlite::{Connection, OpenFlags};
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let loc = Localization::load(&conn, "en_us").unwrap();
        let mut data = GameData::empty();
        data.factions = FactionData::load(&conn, &loc).unwrap();
        let server = crate::test_support::test_server_with(data);
        assert_eq!(faction_relation(&server, 109, 2), RelationState::Neutral); // Amutar
        assert_eq!(faction_relation(&server, 109, 109), RelationState::Friendly); // Mutani
        assert_eq!(faction_relation(&server, 109, 1), RelationState::Friendly); // Lucius
        assert_eq!(faction_relation(&server, 109, 115), RelationState::Hostile); // kobolds
        assert_eq!(faction_relation(&server, 109, 149), RelationState::Friendly); // nation
                                                                                  // Nuia, via mothers 149/148
        assert_eq!(faction_relation(&server, 109, 101), RelationState::Hostile);
    }

    /// The owner-session mapping with the real mother chains: a Harani player (West Ishvara
    /// 109, mother Haranya Alliance 149) against the neutral machine Amutar (2), the same
    /// faction Mutani (109), the friendly guide Lucius (1), the kobold miners (115) and Nuia
    /// (Crescent Throne 101, mother Nuia Alliance 148). The 1.2 client follows the same mother
    /// chains in `FUN_398a3660` (decompile: `FUN_398a37a0` walks `faction+0x20` to the root).
    #[tokio::test]
    async fn harani_player_relation_mapping() {
        let mut data = GameData::empty();
        for f in [
            faction(1, 0),
            faction(2, 0),
            faction(3, 0),
            faction(101, 148),
            faction(109, 149),
            faction(115, 0),
            faction(148, 0),
            faction(149, 0),
        ] {
            data.factions.add(f);
        }
        for (id, id2, state) in [
            (148, 149, RelationState::Hostile),
            (149, 115, RelationState::Hostile),
            (101, 109, RelationState::Hostile),
        ] {
            data.factions.relations.push(FactionRelation {
                id,
                id2,
                state: state as u8,
                exp_time: 0,
            });
        }
        let server = crate::test_support::test_server_with(data);
        assert_eq!(faction_relation(&server, 109, 2), RelationState::Neutral);
        assert_eq!(faction_relation(&server, 109, 109), RelationState::Friendly);
        assert_eq!(faction_relation(&server, 109, 1), RelationState::Friendly);
        assert_eq!(faction_relation(&server, 109, 115), RelationState::Hostile);
        assert_eq!(faction_relation(&server, 109, 101), RelationState::Hostile);
    }

    const MIRAGE_ZONE: u32 = 260;
    const MIRAGE_GROUP: u32 = 49;
    const NACHASHGAR_ZONE: u32 = 261;
    const NACHASHGAR_GROUP: u32 = 55;
    /// Open-world zone groups with a `conflict_zones` row (14 = `e_steppe_belt`).
    const PEACE_ZONE: u32 = 300;
    const PEACE_GROUP: u32 = 14;
    const CONFLICT_ZONE: u32 = 301;
    const CONFLICT_GROUP: u32 = 15;
    /// The duel team factions of `DuelManager` (`FactionsEnum.RedTeam` / `BlueTeam`).
    const RED_TEAM: u32 = 159;
    const BLUE_TEAM: u32 = 160;

    fn pvp_data() -> GameData {
        use crate::data::world_data::Zone;
        use crate::systems::instances::data::IndunZone;
        let mut data = GameData::empty();
        for f in [
            faction(1, 0),
            faction(2, 0),
            faction(101, 148),
            faction(109, 149),
            faction(148, 0),
            faction(149, 0),
            faction(115, 0),
            faction(RED_TEAM, 0),
            faction(BLUE_TEAM, 0),
            faction(factions::PIRATE, 0),
        ] {
            data.factions.add(f);
        }
        for (id, id2, state) in [
            (148, 149, RelationState::Hostile),
            (148, 115, RelationState::Hostile),
            (148, factions::PIRATE, RelationState::Hostile),
            (149, factions::PIRATE, RelationState::Hostile),
            (RED_TEAM, BLUE_TEAM, RelationState::Hostile),
        ] {
            data.factions.relations.push(FactionRelation {
                id,
                id2,
                state: state as u8,
                exp_time: 0,
            });
        }
        for (zone_key, group_id) in [
            (crate::test_support::TEST_ZONE, 1),
            (MIRAGE_ZONE, MIRAGE_GROUP),
            (NACHASHGAR_ZONE, NACHASHGAR_GROUP),
            (PEACE_ZONE, PEACE_GROUP),
            (CONFLICT_ZONE, CONFLICT_GROUP),
        ] {
            data.zones.zones.insert(
                zone_key,
                Zone {
                    id: zone_key,
                    name: format!("zone_{zone_key}"),
                    zone_key,
                    group_id,
                    ..Zone::default()
                },
            );
        }
        data.systems.instances.indun.zones.insert(
            MIRAGE_GROUP,
            IndunZone {
                zone_group_id: MIRAGE_GROUP,
                name: "mirage".into(),
                pvp: false,
                ..IndunZone::default()
            },
        );
        data.systems.instances.indun.zones.insert(
            NACHASHGAR_GROUP,
            IndunZone {
                zone_group_id: NACHASHGAR_GROUP,
                name: "nachashgar".into(),
                pvp: true,
                ..IndunZone::default()
            },
        );
        data
    }

    /// A player of `faction_id` standing in `zone_key` / instance `instance_id`.
    fn player_in(
        server: &GameServer,
        client: &crate::test_support::TestClient,
        obj_id: u32,
        faction_id: u32,
        zone_key: u32,
        instance_id: u32,
    ) -> PlayerRef {
        let mut c = crate::test_support::test_character(obj_id & 0xff, "P", 100.0, 100.0);
        c.unit.faction_id = faction_id;
        c.unit.hp = 5_000;
        c.unit.max_hp = 5_000;
        c.unit.mp = 1_000;
        c.unit.max_mp = 1_000;
        let p = crate::test_support::spawn_player(server, client, c, obj_id);
        {
            let mut d = p.data();
            d.unit.transform.zone_id = zone_key;
            d.unit.transform.instance_id = instance_id;
        }
        p
    }

    #[tokio::test]
    async fn no_pvp_instance_blocks_character_and_owned_unit_attacks() {
        use crate::models::mate::Mate;
        use crate::models::slave::Slave;
        use crate::world::{Entity, WorldObject};
        let server = crate::test_support::test_server_with(pvp_data());
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let a = player_in(&server, &ca, 0x101, 101, MIRAGE_ZONE, 100);
        let b = player_in(&server, &cb, 0x109, 109, MIRAGE_ZONE, 100);
        let ta = Target::Obj(WorldObject::from(&a));
        let tb = Target::Obj(WorldObject::from(&b));
        // The faction relation itself is unchanged; only `can_attack` is gated.
        assert_eq!(relation_to(&server, &ta, &tb), RelationState::Hostile);
        assert!(!can_attack(&server, &ta, &tb));
        assert!(!can_attack(&server, &tb, &ta));

        // A pet / slave owned by a player is character-owned too.
        let mut mate = Mate::new(0x201, 1);
        mate.owner_obj_id = a.obj_id;
        mate.unit.faction_id = 101;
        mate.unit.transform.set_xyz(105.0, 100.0, 100.0);
        mate.unit.transform.zone_id = MIRAGE_ZONE;
        mate.unit.transform.instance_id = 100;
        let mate = Entity::new(0x201, mate);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&mate, &server.view_ctx());
        {
            let mut d = mate.data();
            d.unit.transform.zone_id = MIRAGE_ZONE;
            d.unit.transform.instance_id = 100;
        }
        let t_mate = Target::Obj(WorldObject::from(&mate));
        assert!(!can_attack(&server, &t_mate, &tb));
        assert!(!can_attack(&server, &ta, &t_mate));

        let mut slave = Slave::new(0x301, 1);
        slave.summoner_obj_id = b.obj_id;
        slave.unit.faction_id = 109;
        slave.unit.transform.set_xyz(106.0, 100.0, 100.0);
        slave.unit.transform.zone_id = MIRAGE_ZONE;
        slave.unit.transform.instance_id = 100;
        let slave = Entity::new(0x301, slave);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&slave, &server.view_ctx());
        {
            let mut d = slave.data();
            d.unit.transform.zone_id = MIRAGE_ZONE;
            d.unit.transform.instance_id = 100;
        }
        let t_slave = Target::Obj(WorldObject::from(&slave));
        assert!(!can_attack(&server, &ta, &t_slave));
        assert!(!can_attack(&server, &t_slave, &ta));

        // NPC targets and `relation_to` are untouched.
        let npc = Entity::new(0x401, crate::models::npc::Npc::new(900));
        {
            let mut n = npc.data();
            n.unit.faction_id = 109;
            n.unit.transform.set_xyz(105.0, 100.0, 100.0);
            n.unit.transform.zone_id = MIRAGE_ZONE;
        }
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&npc, &server.view_ctx());
        let t_npc = Target::Obj(WorldObject::from(&npc));
        assert!(can_attack(&server, &ta, &t_npc));
    }

    #[tokio::test]
    async fn pvp_instances_and_the_open_world_keep_faction_attacks() {
        use crate::world::WorldObject;
        let server = crate::test_support::test_server_with(pvp_data());
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        // Nachashgar (55) has `pvp = true`.
        let a = player_in(&server, &ca, 0x101, 101, NACHASHGAR_ZONE, 100);
        let b = player_in(&server, &cb, 0x109, 109, NACHASHGAR_ZONE, 100);
        let ta = Target::Obj(WorldObject::from(&a));
        let tb = Target::Obj(WorldObject::from(&b));
        assert!(can_attack(&server, &ta, &tb));
        assert!(can_attack(&server, &tb, &ta));

        // A zone with no `indun_zones` record (the open world) is unchanged.
        {
            let mut d = b.data();
            d.unit.transform.zone_id = crate::test_support::TEST_ZONE;
            d.unit.transform.instance_id = 0;
        }
        assert!(can_attack(&server, &ta, &tb));
    }

    #[tokio::test]
    async fn skill_casts_are_refused_in_a_no_pvp_instance() {
        use super::super::data::effects::EffectTemplate;
        use super::super::data::EffectKind;
        use super::super::skill::use_skill;
        use super::super::state::SkillRun;
        use super::super::tests::{damage_effect, skill_effect, skill_template};
        use super::super::types::{SkillCastTarget, SkillCaster, SkillObject};
        let mut data = pvp_data();
        let skills = &mut data.systems.skills;
        skills.add_effect(
            EffectKind::DamageEffect,
            EffectTemplate::Damage(Box::new(damage_effect(1, 600))),
            900_001,
        );
        let mut dmg = skill_template(10_001);
        dmg.effects
            .push(skill_effect(900_001, EffectKind::DamageEffect, 1));
        let template = std::sync::Arc::new(dmg);
        skills.add_skill((*template).clone());
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let a = player_in(&server, &ca, 0x101, 101, MIRAGE_ZONE, 100);
        let b = player_in(&server, &cb, 0x109, 109, MIRAGE_ZONE, 100);
        let b_hp = b.data().unit.hp;
        // Mirage Isle (`pvp = false`): the skill effect path deals no damage, the same check
        // the auto-attack tick uses (`skill.rs` `can_attack`).
        let skill = SkillRun::new(template.clone(), 1);
        use_skill(
            &server,
            &skill,
            &WorldObject::from(&a),
            SkillCaster::Unit(a.obj_id),
            &mut SkillCastTarget::Unit(b.obj_id),
            Some(SkillObject::None),
            true,
        );
        assert_eq!(b.data().unit.hp, b_hp);

        // Nachashgar (`pvp = true`): the same cast damages the enemy player.
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let cd = crate::test_support::TestClient::new(&server, 4, 4);
        let c = player_in(&server, &cc, 0x201, 101, NACHASHGAR_ZONE, 100);
        let d = player_in(&server, &cd, 0x209, 109, NACHASHGAR_ZONE, 100);
        let d_hp = d.data().unit.hp;
        let skill = SkillRun::new(template, 1);
        use_skill(
            &server,
            &skill,
            &WorldObject::from(&c),
            SkillCaster::Unit(c.obj_id),
            &mut SkillCastTarget::Unit(d.obj_id),
            Some(SkillObject::None),
            true,
        );
        assert!(
            d.data().unit.hp < d_hp,
            "the pvp zone cast must damage ({} -> {})",
            d_hp,
            d.data().unit.hp
        );
    }

    #[tokio::test]
    async fn config_off_restores_the_faction_only_decision() {
        use crate::world::WorldObject;
        let server = crate::test_support::test_server_with(pvp_data());
        server.config.update(|c| c.dungeons.enforce_no_pvp = false);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let a = player_in(&server, &ca, 0x101, 101, MIRAGE_ZONE, 100);
        let b = player_in(&server, &cb, 0x109, 109, MIRAGE_ZONE, 100);
        assert!(can_attack(
            &server,
            &Target::Obj(WorldObject::from(&a)),
            &Target::Obj(WorldObject::from(&b))
        ));
    }

    /// The two fixture conflict groups at a fixed state (`PEACE_GROUP` = 14 is Peace,
    /// `CONFLICT_GROUP` = 15 is Conflict).
    fn init_conflicts(server: &GameServer) {
        use crate::data::world_data::ZoneConflict;
        server.systems.social.conflicts.init(
            &[
                ZoneConflict {
                    zone_group_id: PEACE_GROUP as u16,
                    current_state: conflict_type::PEACE,
                    ..ZoneConflict::default()
                },
                ZoneConflict {
                    zone_group_id: CONFLICT_GROUP as u16,
                    current_state: conflict_type::CONFLICT,
                    ..ZoneConflict::default()
                },
            ],
            now_ms(),
        );
    }

    fn obj_target(p: &PlayerRef) -> Target {
        Target::Obj(WorldObject::from(p))
    }

    /// Peace: a player cannot harm a player of another faction; the same pair is attackable in
    /// Conflict. The relation itself is unchanged (`relation_to` stays Hostile) — only
    /// `can_attack` is gated, like the no-PvP instance rule.
    #[tokio::test]
    async fn peace_blocks_player_attacks_and_conflict_allows_them() {
        let server = crate::test_support::test_server_with(pvp_data());
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let cd = crate::test_support::TestClient::new(&server, 4, 4);
        let peace_a = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let peace_b = player_in(&server, &cb, 0x109, 109, PEACE_ZONE, 0);
        let conflict_c = player_in(&server, &cc, 0x201, 101, CONFLICT_ZONE, 0);
        let conflict_d = player_in(&server, &cd, 0x209, 109, CONFLICT_ZONE, 0);
        let (ta, tb) = (obj_target(&peace_a), obj_target(&peace_b));
        assert_eq!(relation_to(&server, &ta, &tb), RelationState::Hostile);
        assert!(!can_attack(&server, &ta, &tb));
        assert!(!can_attack(&server, &tb, &ta));
        let (tc, td) = (obj_target(&conflict_c), obj_target(&conflict_d));
        assert_eq!(relation_to(&server, &tc, &td), RelationState::Hostile);
        assert!(can_attack(&server, &tc, &td));
        assert!(can_attack(&server, &td, &tc));
    }

    /// A cycling zone in Peace is an absolute PvP lockdown: pirates are protected like every
    /// other player, and a pirate cannot attack players either. In Conflict the same pairs are
    /// attackable again (the relation stays Hostile).
    #[tokio::test]
    async fn peace_refuses_pirates_attacking_and_being_attacked() {
        let server = crate::test_support::test_server_with(pvp_data());
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let nuian = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let pirate = player_in(&server, &cb, 0x109, factions::PIRATE, PEACE_ZONE, 0);
        let harani = player_in(&server, &cc, 0x201, 109, PEACE_ZONE, 0);
        let (tnu, tpir, thar) = (obj_target(&nuian), obj_target(&pirate), obj_target(&harani));
        assert_eq!(relation_to(&server, &tnu, &tpir), RelationState::Hostile);
        assert!(
            !can_attack(&server, &tnu, &tpir),
            "a pirate target is protected in Peace"
        );
        assert!(
            !can_attack(&server, &tpir, &tnu),
            "a pirate attacker is refused in Peace"
        );
        assert!(
            !can_attack(&server, &tpir, &thar),
            "a pirate cannot attack any player in Peace"
        );

        // Conflict: the same pairs are ordinary hostile attacks.
        let cd = crate::test_support::TestClient::new(&server, 4, 4);
        let ce = crate::test_support::TestClient::new(&server, 5, 5);
        let conflict_nuian = player_in(&server, &cd, 0x20A, 101, CONFLICT_ZONE, 0);
        let conflict_pirate = player_in(&server, &ce, 0x30B, factions::PIRATE, CONFLICT_ZONE, 0);
        let (tnu2, tpir2) = (obj_target(&conflict_nuian), obj_target(&conflict_pirate));
        assert!(can_attack(&server, &tnu2, &tpir2));
        assert!(can_attack(&server, &tpir2, &tnu2));
    }

    /// The permanently safe faction-controlled home zones have no conflict cycle: a player
    /// attacks a pirate there as an ordinary hostile target, with no crime state.
    #[tokio::test]
    async fn home_zone_players_attack_pirates_without_crime() {
        use crate::systems::skills::data::effects::EffectTemplate;
        use crate::systems::skills::data::EffectKind;
        use crate::systems::skills::skill::use_skill;
        use crate::systems::skills::state::SkillRun;
        use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};

        let mut data = pvp_data();
        let skills = &mut data.systems.skills;
        skills.add_buff(crate::systems::skills::data::BuffTemplate {
            id: 2_167, // BuffConstants.Retribution
            kind: 2,
            duration: 60_000,
            stack_rule: 1,
            ..Default::default()
        });
        let mut effect = super::super::tests::damage_effect(1, 600);
        effect.check_crime = true;
        skills.add_effect(
            EffectKind::DamageEffect,
            EffectTemplate::Damage(Box::new(effect)),
            900_001,
        );
        let mut template = super::super::tests::skill_template(10_004);
        template.effects.push(super::super::tests::skill_effect(
            900_001,
            EffectKind::DamageEffect,
            1,
        ));
        let template = std::sync::Arc::new(template);
        skills.add_skill((*template).clone());
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        // No conflict state is initialised for the home zone (group 1): Peace never applies.
        assert!(!server
            .systems
            .social
            .conflicts
            .get(1)
            .is_some_and(|c| c.current_state == conflict_type::PEACE));

        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let attacker = player_in(&server, &ca, 0x101, 101, crate::test_support::TEST_ZONE, 0);
        let pirate = player_in(
            &server,
            &cb,
            0x109,
            factions::PIRATE,
            crate::test_support::TEST_ZONE,
            0,
        );
        let (ta, tp) = (obj_target(&attacker), obj_target(&pirate));
        assert_eq!(relation_to(&server, &ta, &tp), RelationState::Hostile);
        assert!(
            can_attack(&server, &ta, &tp),
            "a pirate is attackable in a home zone"
        );

        let hp = pirate.data().unit.hp;
        let mut target = SkillCastTarget::Unit(pirate.obj_id);
        use_skill(
            &server,
            &SkillRun::new(template, 1),
            &WorldObject::from(&attacker),
            SkillCaster::Unit(attacker.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert!(pirate.data().unit.hp < hp, "the attack lands");
        assert!(
            !attacker.data().unit.buffs.check_buff(2_167),
            "attacking a pirate raises no Retribution"
        );
        assert!(
            attacker.data().skill_char.assaulted_by.is_empty(),
            "attacking a pirate creates no crime evidence/assault record"
        );
        assert_eq!(attacker.data().crime_point, 0);
    }

    /// Two duellists may keep fighting in Peace (owner recollection 2026-11-10); after the
    /// duel ends the same pair is refused again.
    #[tokio::test]
    async fn peace_allows_a_duel_between_the_two_players() {
        use crate::systems::social::duel::duel_det_type;
        let server = crate::test_support::test_server_with(pvp_data());
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let a = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let b = player_in(&server, &cb, 0x109, 109, PEACE_ZONE, 0);
        let (ta, tb) = (obj_target(&a), obj_target(&b));
        assert!(!can_attack(&server, &ta, &tb));
        server
            .systems
            .social
            .duels
            .duel_request(&server, &a, b.character_id);
        assert!(server
            .systems
            .social
            .duels
            .is_in_duel_with(a.character_id, b.character_id));
        server
            .systems
            .social
            .duels
            .duel_accepted(&server, &b, a.character_id)
            .await;
        assert_eq!(a.data().unit.faction_id, RED_TEAM);
        assert_eq!(b.data().unit.faction_id, BLUE_TEAM);
        assert!(can_attack(&server, &ta, &tb));
        assert!(can_attack(&server, &tb, &ta));
        server
            .systems
            .social
            .duels
            .duel_stop(&server, a.character_id, duel_det_type::DRAW, 0)
            .await;
        assert!(!server
            .systems
            .social
            .duels
            .is_in_duel_with(a.character_id, b.character_id));
        assert!(!can_attack(&server, &ta, &tb), "the peace rule is back");
    }

    /// Exception (c): a nation's `CSFactionDeclareHostile` relation row lets it fight the
    /// declared faction in Peace. The static Nuia/Haranya hostility is not a declaration and
    /// does not except.
    #[tokio::test]
    async fn peace_allows_factions_in_a_declared_hostile_diplomacy_relation() {
        use crate::systems::social::nation::{Nation, NationRelation};
        let server = crate::test_support::test_server_with(pvp_data());
        init_conflicts(&server);
        server.systems.social.nations.add(Nation {
            id: 5000,
            expedition_id: 0,
            owner_character_id: 0,
            owner_name: "King".into(),
            name: "Kingdom".into(),
            created_at: 1,
            allow_change_name: 0,
            relations: vec![NationRelation {
                other_faction_id: 148,
                state: RelationState::Hostile as u8,
                expire_at: 0,
                next_state: RelationState::Hostile as u8,
            }],
            offline_members: Vec::new(),
        });
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let citizen = player_in(&server, &ca, 0x101, 5000, PEACE_ZONE, 0);
        let nuian = player_in(&server, &cb, 0x109, 101, PEACE_ZONE, 0);
        let harani = player_in(&server, &cc, 0x201, 109, PEACE_ZONE, 0);
        assert_eq!(faction_relation(&server, 5000, 101), RelationState::Hostile);
        let (tcit, tnu, thar) = (
            obj_target(&citizen),
            obj_target(&nuian),
            obj_target(&harani),
        );
        assert!(can_attack(&server, &tcit, &tnu));
        assert!(can_attack(&server, &tnu, &tcit));
        // The relation with Haranya was not declared: the static alliance hostility alone does
        // not except, and neither does the plain alliance pair.
        assert_eq!(faction_relation(&server, 5000, 109), RelationState::Neutral);
        assert!(!can_attack(&server, &tcit, &thar));
        assert!(!can_attack(&server, &tnu, &thar));
    }

    /// Ctrl+F force attack (bloodlust) makes friendly players attackable, but not in Peace.
    #[tokio::test]
    async fn peace_is_not_bypassed_by_force_attack() {
        let server = crate::test_support::test_server_with(pvp_data());
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let a = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let b = player_in(&server, &cb, 0x109, 101, PEACE_ZONE, 0);
        a.data().unit.force_attack = true;
        let (ta, tb) = (obj_target(&a), obj_target(&b));
        assert_eq!(relation_to(&server, &ta, &tb), RelationState::Friendly);
        assert!(!can_attack(&server, &ta, &tb));
        // Outside Peace the flag still makes the friendly pair attackable.
        b.data().unit.transform.zone_id = CONFLICT_ZONE;
        assert!(can_attack(&server, &ta, &tb));
    }

    /// An area skill in Peace skips a protected player but still damages the Conflict-zone
    /// player and the hostile NPC in the same radius.
    #[tokio::test]
    async fn peace_aoe_skips_protected_players_and_still_hits_npcs() {
        use crate::models::npc::Npc;
        use crate::systems::skills::data::effects::EffectTemplate;
        use crate::systems::skills::data::EffectKind;
        use crate::systems::skills::skill::use_skill;
        use crate::systems::skills::state::SkillRun;
        use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
        use crate::world::Entity;

        let mut data = pvp_data();
        let skills = &mut data.systems.skills;
        skills.add_effect(
            EffectKind::DamageEffect,
            EffectTemplate::Damage(Box::new(super::super::tests::damage_effect(1, 600))),
            900_001,
        );
        let mut template = super::super::tests::skill_template(10_001);
        template.target_area_radius = 5;
        template.target_selection = 2; // Target
        template.target_relation = SkillTargetRelation::Hostile as i32;
        template.effects.push(super::super::tests::skill_effect(
            900_001,
            EffectKind::DamageEffect,
            1,
        ));
        let template = std::sync::Arc::new(template);
        skills.add_skill((*template).clone());
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        init_conflicts(&server);

        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let caster = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let protected = player_in(&server, &cb, 0x109, 109, PEACE_ZONE, 0);
        let exposed = player_in(&server, &cc, 0x209, 109, CONFLICT_ZONE, 0);
        let mut npc = Npc::new(900);
        npc.unit.faction_id = 115;
        npc.unit.hp = 5_000;
        npc.unit.max_hp = 5_000;
        npc.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let npc = Entity::new(0x401, npc);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&npc, &server.view_ctx());

        let (protected_hp, exposed_hp, npc_hp) = (
            protected.data().unit.hp,
            exposed.data().unit.hp,
            npc.data().unit.hp,
        );
        let mut target = SkillCastTarget::Unit(exposed.obj_id);
        use_skill(
            &server,
            &SkillRun::new(template, 1),
            &WorldObject::from(&caster),
            SkillCaster::Unit(caster.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(
            protected.data().unit.hp,
            protected_hp,
            "the Peace-zone player must not be hit by the area"
        );
        assert!(
            exposed.data().unit.hp < exposed_hp,
            "the Conflict-zone player is hit"
        );
        assert!(npc.data().unit.hp < npc_hp, "the NPC is hit");
    }
}
