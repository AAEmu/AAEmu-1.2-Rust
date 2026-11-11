//! Faction relations and attack permission: `SystemFaction.GetRelationState`,
//! `BaseUnit.GetRelationStateTo`, `BaseUnit.CanAttack`, `GetOwnerCharacter`,
//! `SkillTargetingUtil.FilterWithRelation` / `IsRelationValid`.

use crate::data::world_data::conflict_type;
use crate::server::GameServer;
use crate::systems::social::nation;
use crate::world::{now_ms, GimmickRef, PlayerRef, WorldObject};

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
///
/// Inside the area of a running siege the two siege parties decide first: the attacking and the
/// defending participants are `Hostile` to each other (even two characters of one faction), the
/// participants of one party are `Friendly` (even of hostile factions), and everybody who is no
/// participant, or stands outside the area, keeps the faction relation
/// (`housing::dominion::hostility`). Every reader of the relation sees it: the skill target
/// filters, the hostile-only / friendly-only effect checks, the crime check and the PvP honor.
pub fn relation_to(server: &GameServer, a: &Target, b: &Target) -> RelationState {
    match crate::systems::housing::dominion::hostility::relation(server, a, b) {
        Some(siege) => siege.relation_state(),
        None => faction_relation(server, faction_of(a), faction_of(b)),
    }
}

/// How many spawner links [`gimmick_owner`] follows. A gimmick can be spawned by another gimmick
/// (a bomb whose own skill spawns fragments) and object ids are reused as soon as they are
/// released, so two live gimmicks can name each other: the chain is bounded, not trusted.
const GIMMICK_OWNER_HOPS: usize = 4;

/// The character a gimmick acts for: the owner of the unit that spawned it
/// (`Gimmick.SpawnerUnitId`, set from the caster by `SpawnGimmickEffect`), resolved like a mate /
/// slave owner. `None` when the spawner is gone or is a doodad / NPC with no owning character.
fn gimmick_owner(server: &GameServer, gimmick: &GimmickRef) -> Option<PlayerRef> {
    let mut spawner = gimmick.data().spawner_unit_id;
    for _ in 0..GIMMICK_OWNER_HOPS {
        if spawner == 0 {
            return None;
        }
        match server.world.object(spawner)? {
            WorldObject::Gimmick(g) => spawner = g.data().spawner_unit_id,
            other => return owner_character(server, &other),
        }
    }
    None
}

/// `GetOwnerCharacter()`: the character itself, the owner of a mate / slave, the owner of the unit
/// that spawned a gimmick, else `None`.
///
/// **Inferred (no 1.2 server evidence):** the gimmick arm. The 1.2 C# cast a gimmick's skill from
/// its spawner unit, so every rule keyed on the character applied to it; the gimmick is now the
/// cast source (its skills need a gimmick source, see `skills/validation.rs`), and this mapping
/// keeps those rules (Peace, no-PvP, siege hostility, tagging, crime evidence) on the thrower.
pub fn owner_character(server: &GameServer, o: &WorldObject) -> Option<PlayerRef> {
    match o {
        WorldObject::Player(p) => Some(p.clone()),
        WorldObject::Gimmick(g) => gimmick_owner(server, g),
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
            if id == 0 {
                return world_slave_driver(server, s);
            }
            server
                .world
                .object(id)
                .and_then(|o| owner_character(server, &o))
        }
        _ => None,
    }
}

/// The character a hit or kill by `unit` is credited to (tagging, kill experience, loot, quest
/// events, PvP honor): the player itself, or for a gimmick the character it acts for. A mate or
/// slave keeps its own credit rules, so this is not [`owner_character`]. The death packet and
/// every other broadcast keep naming the unit that really dealt the blow.
pub fn credited_player(server: &GameServer, unit: &WorldObject) -> Option<PlayerRef> {
    match unit {
        WorldObject::Player(p) => Some(p.clone()),
        WorldObject::Gimmick(g) => gimmick_owner(server, g),
        _ => None,
    }
}

/// The character in the gunner / driver seat of a slave nobody owns: a world cannon placed by a
/// `slave_spawns` entry, such as the Bloodhand Base Cannon (slave 57, faction 2 "Neutral") on
/// the Marooned Isle rampart that "Rout the Bloodhands" (2325) sends players to. Such a slave
/// acts for that character (its faction, owner and kill credit); `None` for a summoned or owned
/// slave. Inferred (no 1.2 server evidence): the C# had no owner for these slaves, so a manned
/// world cannon stayed Neutral and could damage nothing.
pub fn world_slave_driver(
    server: &GameServer,
    slave: &crate::world::SlaveRef,
) -> Option<PlayerRef> {
    let driver = {
        let d = slave.data();
        if d.summoner_obj_id != 0 || d.owner_obj_id != 0 {
            return None;
        }
        d.attached_characters
            .iter()
            .find(|(point, _)| *point == crate::models::unit::attach_point::DRIVER)
            .map(|(_, id)| *id)
    }?;
    server.world.character_by_obj_id(driver)
}

/// The faction `me` attacks with: a manned world slave fights with its gunner's faction
/// ([`world_slave_driver`]); every other unit with its own.
fn acting_faction(server: &GameServer, me: &Target) -> u32 {
    if let Some(driver) = me
        .as_obj()
        .and_then(WorldObject::as_slave)
        .and_then(|s| world_slave_driver(server, s))
    {
        return driver.data().unit.faction_id;
    }
    faction_of(me)
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
/// pirates are fully protected too (a pirate cannot be attacked and cannot attack). The client
/// text of `ui_texts` 5195 `state_peace_tooltip` says "except for pirates" (EN / DE / FR); the
/// owner's recollection is that this is about the permanently safe faction-controlled home
/// zones, which have no conflict cycle at all, where pirate attacks are ordinary hostile attacks
/// that must not be blocked. That is the owner's call: revisit if a capture contradicts it (see
/// `PORTING.md`).
///
/// The remaining exceptions are the owner's clarification:
/// * the two characters are duelling each other: an **accepted** duel
///   (`DuelManager::is_dueling_with`). A pending, unanswered challenge registers both characters
///   for up to 30 s but agrees nothing, so it must not lift the lockdown;
/// * their factions are in a hostile diplomacy relation that a nation **declared**
///   (`CSFactionDeclareHostile`, see [`nation::declared_hostile`]). Not every Hostile nation
///   row counts: every new nation copies the faction-166 template rows, which are hostile to
///   both alliances from the start without any declaration, and the static
///   `system_faction_relations` table makes Nuia and Haranya hostile everywhere; either would
///   leave Peace blocking nothing.
///
/// Force attack / Retribution never reach this function's callers' shortcuts: the caller checks
/// this rule before those flags turn a friendly pair attackable.
fn peace_allows_attack(server: &GameServer, attacker: &PlayerRef, target: &PlayerRef) -> bool {
    // (a) An accepted duel between exactly these two characters. The duel team factions are
    // hostile, so this must run before the peace refusal (and it covers the duel's own units
    // too).
    if server
        .systems
        .social
        .duels
        .is_dueling_with(attacker.character_id, target.character_id)
    {
        return true;
    }
    // (b) A hostile diplomacy relation that a nation declared (a runtime nation row with the
    // declared marker, not a faction-166 template row).
    let nations = &server.systems.social.nations;
    let a = attacker.data().unit.faction_id;
    let b = target.data().unit.faction_id;
    (nations.is_nation(a) || nations.is_nation(b)) && nation::declared_hostile(server, a, b)
}

/// The rules that protect a target from a **player-controlled** attacker whatever the faction
/// relation says: Peace, no-PvP instances and the faction-controlled home zones. Callers apply
/// it to the targets of hostile skills (a `Hostile` skill's single cast, its target list and its
/// non-friendly effects). In Peace it therefore also drops allied players from a `Hostile`
/// skill's list, not only hostile ones.
///
/// Non-damage effects use this instead of the whole [`can_attack`]: a Neutral target (faction 2
/// interaction NPCs, an NPC caster's trap) keeps the damage-free C# behaviour, while a protected
/// player is still skipped.
pub fn protected_from(server: &GameServer, me: &Target, target: &Target) -> bool {
    let me_owner = me.as_obj().and_then(|o| owner_character(server, o));
    let target_owner = target.as_obj().and_then(|o| owner_character(server, o));
    if let (Some(attacker), Some(defender)) = (&me_owner, &target_owner) {
        if is_peace_zone(server, target) && !peace_allows_attack(server, attacker, defender) {
            return true;
        }
    }
    if server.config.dungeons.enforce_no_pvp
        && me_owner.is_some()
        && target_owner.is_some()
        && is_no_pvp_zone(server, target)
    {
        return true;
    }
    home_zone_protected(server, me, target)
}

/// The faction-controlled home-zone protection of `can_attack`: a zone mother faction protects
/// its own natives, and a zone-faction player is safe from a non-hostile, non-Retribution
/// attacker of the same zone.
fn home_zone_protected(server: &GameServer, me: &Target, target: &Target) -> bool {
    let me_char = me.as_obj().and_then(|o| o.as_player().cloned());
    let target_faction = faction_of(target);
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
    let factions = &server.game_data.factions;
    let zone_faction = factions
        .get(zone_faction_id)
        .or_else(|| factions.get(FACTION_NEUTRAL));
    let target_mother = nation::faction_of_id(server, target_faction).map_or(0, |f| f.1);
    if me_char.is_some() && target_mother != 0 {
        if let Some(z) = zone_faction {
            if target_mother == z.mother_id || target_mother == z.id {
                return true;
            }
        }
    }
    if let (Some(me_c), Some(owner)) = (
        me_char.as_ref(),
        target.as_obj().and_then(|o| owner_character(server, o)),
    ) {
        let (owner_faction, owner_obj) = {
            let d = owner.data();
            (d.unit.faction_id, d.obj_id)
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
            return true;
        }
    }
    false
}

/// The hull-collision protection between two player-owned ships: Peace and no-PvP only (the
/// home-zone faction protection is about player attacks on zone natives, not ship collisions).
pub fn ramming_protected(
    server: &GameServer,
    attacker: &WorldObject,
    target: &WorldObject,
) -> bool {
    let (Some(attacker), Some(target_char)) = (
        owner_character(server, attacker),
        owner_character(server, target),
    ) else {
        return false;
    };
    let target_t = Target::Obj(target.clone());
    if is_peace_zone(server, &target_t) && !peace_allows_attack(server, &attacker, &target_char) {
        return true;
    }
    server.config.dungeons.enforce_no_pvp && is_no_pvp_zone(server, &target_t)
}

/// `BaseUnit.CanAttack(target)`.
pub fn can_attack(server: &GameServer, me: &Target, target: &Target) -> bool {
    if let Some(allowed) =
        crate::systems::housing::dominion::runtime::can_attack_tower(server, me, target)
    {
        return allowed;
    }
    // The two character owners are needed by the Peace rule, the own-mount check, the no-PvP
    // instance rule and the zone-protection branches: resolve each once, defender first, so the
    // zone-state lookup only runs for a player-controlled pair.
    let me_owner = me.as_obj().and_then(|o| owner_character(server, o));
    let target_owner = target.as_obj().and_then(|o| owner_character(server, o));
    // Peace period: a player-controlled attacker (player, pet, summon, slave, siege weapon)
    // cannot harm another player or that player's units. This runs before every shortcut that
    // would otherwise allow the pair (force attack, Retribution) and before the faction checks,
    // so it also covers an unknown / null faction. NPC attackers are unchanged (their
    // `owner_character` is `None`).
    if let (Some(attacker), Some(defender)) = (&me_owner, &target_owner) {
        if is_peace_zone(server, target) && !peace_allows_attack(server, attacker, defender) {
            return false;
        }
    }
    let my_faction = acting_faction(server, me);
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
    if let (Some(me_owner), Some(target_owner)) = (&me_owner, &target_owner) {
        if me_owner.obj_id == target_owner.obj_id {
            return false;
        }
    }
    // `indun_zones.pvp = false`: the 1.2 client resolves a character-owned pair as
    // non-hostile before faction logic (`FUN_390281c0` stores `pvp == 0` in the relation
    // context; `FUN_39837a40` returns that flag and `FUN_39838880` then yields relation
    // kind 2 and returns early). The server rule is inferred from that; `EnforceNoPvp`
    // restores the faction-only decision.
    if server.config.dungeons.enforce_no_pvp
        && me_owner.is_some()
        && target_owner.is_some()
        && is_no_pvp_zone(server, target)
    {
        return false;
    }
    // The siege rule of `relation_to` (inside a running siege's area the two parties are
    // Hostile, a party is Friendly to itself): it comes after the Peace / no-PvP / own-mount
    // refusals above, so it never weakens those protections, and the faction / zone logic below
    // reads the same relation as every skill path. Outside a siege the faction relation uses
    // the attacker's acting faction (a manned world cannon fights with its gunner's).
    let relation = match crate::systems::housing::dominion::hostility::relation(server, me, target)
    {
        Some(siege) => siege.relation_state(),
        None => faction_relation(server, my_faction, target_faction),
    };
    let me_char = me.as_obj().and_then(|o| o.as_player().cloned());
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
    if let (Some(me_c), Some(owner)) = (&me_char, target_owner) {
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
    use crate::systems::skills::types::SkillTargetType;

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

    /// A world cannon nobody owns (the Bloodhand Base Cannon, slave 57, faction 2 "Neutral")
    /// fights for the player in its gunner seat: unmanned it can attack nothing; manned by a
    /// West Ishvara player (109) it can attack the Monstrosity Bloodhand Pillagers (115), it is
    /// that player's unit (`owner_character`), and its damage tags the gunner, so the kill
    /// credits the gunner's quest. A summoned slave keeps its summoner as its owner.
    #[tokio::test]
    async fn a_manned_world_cannon_fights_for_its_gunner() {
        use crate::models::npc::Npc;
        use crate::models::slave::Slave;
        use crate::models::unit::attach_point;
        use crate::test_support::{spawn_player, test_character, TestClient};
        use crate::world::Entity;

        let mut data = GameData::empty();
        for f in [
            faction(2, 0),
            faction(109, 149),
            faction(115, 0),
            faction(149, 0),
        ] {
            data.factions.add(f);
        }
        data.factions.relations.push(FactionRelation {
            id: 149,
            id2: 115,
            state: RelationState::Hostile as u8,
            exp_time: 0,
        });
        let server = crate::test_support::test_server_with(data);
        let client = TestClient::new(&server, 1, 1001);
        let mut gunner = test_character(1, "Gunner", 100.0, 100.0);
        gunner.unit.faction_id = 109;
        let player = spawn_player(&server, &client, gunner, 0x100);
        let world = server.world.world(0).unwrap();
        let mut n = Npc::new(8014);
        n.unit.faction_id = 115;
        n.unit.hp = 100;
        n.unit.max_hp = 100;
        n.unit.transform.set_xyz(140.0, 100.0, 100.0);
        let pillager = Entity::new(0x200, n);
        world.spawn(&pillager, &server.view_ctx());
        let mut s = Slave::new(0, 57);
        s.unit.faction_id = 2;
        s.unit.hp = 100;
        s.unit.transform.set_xyz(101.0, 100.0, 100.0);
        let cannon = Entity::new(0x300, s);
        world.spawn(&cannon, &server.view_ctx());
        let cannon_obj = WorldObject::from(&cannon);
        let cannon_t = Target::Obj(cannon_obj.clone());
        let pillager_obj = WorldObject::from(&pillager);
        let pillager_t = Target::Obj(pillager_obj.clone());

        assert!(
            !can_attack(&server, &cannon_t, &pillager_t),
            "unmanned: Neutral"
        );
        assert!(owner_character(&server, &cannon_obj).is_none());

        cannon
            .data()
            .attached_characters
            .push((attach_point::DRIVER, player.obj_id));
        assert!(can_attack(&server, &cannon_t, &pillager_t));
        assert_eq!(
            owner_character(&server, &cannon_obj).map(|p| p.obj_id),
            Some(player.obj_id)
        );
        crate::systems::skills::combat::add_damage_aggro(
            &server,
            &pillager_obj,
            &cannon_obj,
            10,
            10,
        );
        assert_eq!(
            pillager.data().unit.skill_state.tagging.tagger,
            player.obj_id,
            "the cannon's damage tags its gunner"
        );

        // A summoned slave is its summoner's unit, whoever sits in the seat.
        cannon.data().summoner_obj_id = 0x999;
        assert!(world_slave_driver(&server, &cannon).is_none());
        assert!(
            !can_attack(&server, &cannon_t, &pillager_t),
            "own faction again"
        );
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

    /// A gimmick (a thrown bomb) spawned by `spawner_obj_id` on `faction_id`'s side, registered in
    /// the world so a gimmick spawned by a gimmick can be resolved through it.
    fn gimmick_of(
        server: &GameServer,
        obj_id: u32,
        spawner_obj_id: u32,
        faction_id: u32,
    ) -> crate::world::GimmickRef {
        let mut g = crate::models::gimmick::Gimmick::new(900);
        g.spawner_unit_id = spawner_obj_id;
        g.unit.faction_id = faction_id;
        g.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let g = crate::world::Entity::new(obj_id, g);
        let world = server
            .world
            .worlds()
            .into_iter()
            .next()
            .expect("a world instance");
        world.spawn(&g, &server.view_ctx());
        g
    }

    /// A gimmick acts for the character that spawned it, so the Peace lockdown covers a bomb a
    /// player threw. Without the owner it would be an ownerless attacker decided by the static
    /// faction relation alone (Nuia against Haranya: hostile). In Conflict the same pair is
    /// attackable, and a gimmick whose spawner is gone falls back to the faction decision.
    #[tokio::test]
    async fn peace_refuses_a_gimmick_thrown_by_a_player() {
        let server = crate::test_support::test_server_with(pvp_data());
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let cd = crate::test_support::TestClient::new(&server, 4, 4);
        let thrower = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let victim = player_in(&server, &cb, 0x109, 109, PEACE_ZONE, 0);
        let bomb = gimmick_of(&server, 0x301, thrower.obj_id, 101);
        let (tbomb, tvictim) = (Target::Obj(WorldObject::from(&bomb)), obj_target(&victim));
        assert_eq!(
            relation_to(&server, &tbomb, &tvictim),
            RelationState::Hostile
        );
        assert!(
            !can_attack(&server, &tbomb, &tvictim),
            "Peace: a thrown bomb cannot hurt another player"
        );
        assert!(
            protected_from(&server, &tbomb, &tvictim),
            "Peace also protects the victim from the bomb's non-damage effects"
        );

        // Conflict: the same pair is an ordinary hostile attack.
        let conflict_thrower = player_in(&server, &cc, 0x201, 101, CONFLICT_ZONE, 0);
        let conflict_victim = player_in(&server, &cd, 0x209, 109, CONFLICT_ZONE, 0);
        let conflict_bomb = gimmick_of(&server, 0x302, conflict_thrower.obj_id, 101);
        let (tbomb, tvictim) = (
            Target::Obj(WorldObject::from(&conflict_bomb)),
            obj_target(&conflict_victim),
        );
        assert!(can_attack(&server, &tbomb, &tvictim));
        assert!(!protected_from(&server, &tbomb, &tvictim));

        // As for a mate or slave, never the character it acts for, whatever the faction relation
        // says (the own-unit rule of `can_attack`): a gimmick of the hostile side 109 still
        // cannot hurt its own thrower (side 101).
        let turncoat = gimmick_of(&server, 0x304, conflict_thrower.obj_id, 109);
        assert!(!can_attack(
            &server,
            &Target::Obj(WorldObject::from(&turncoat)),
            &obj_target(&conflict_thrower)
        ));

        // No owning character (the spawner is gone): the faction decision alone, as before.
        let orphan = gimmick_of(&server, 0x303, 0x7777, 101);
        assert!(can_attack(
            &server,
            &Target::Obj(WorldObject::from(&orphan)),
            &obj_target(&victim)
        ));
    }

    /// `owner_character` / `credited_player` for gimmicks: the character behind the spawner
    /// (also through a gimmick that spawned a gimmick), nobody for a doodad spawner, a missing
    /// spawner or a chain that loops (object ids are reused), and a mate / slave keeps its own
    /// credit rules.
    #[tokio::test]
    async fn a_gimmick_resolves_to_the_character_that_spawned_it() {
        let server = crate::test_support::test_server_with(pvp_data());
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let thrower = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let owner_of = |o: &WorldObject| owner_character(&server, o).map(|p| p.obj_id);
        let credit_of = |o: &WorldObject| credited_player(&server, o).map(|p| p.obj_id);

        let bomb = gimmick_of(&server, 0x301, thrower.obj_id, 101);
        assert_eq!(owner_of(&WorldObject::from(&bomb)), Some(thrower.obj_id));
        assert_eq!(credit_of(&WorldObject::from(&bomb)), Some(thrower.obj_id));
        // A bomb whose own skill spawned a fragment.
        let fragment = gimmick_of(&server, 0x302, bomb.obj_id, 101);
        assert_eq!(
            owner_of(&WorldObject::from(&fragment)),
            Some(thrower.obj_id)
        );
        assert_eq!(
            credit_of(&WorldObject::from(&fragment)),
            Some(thrower.obj_id)
        );

        // A doodad spawner is no character.
        let doodad = crate::world::Entity::new(0x303, crate::models::doodad::Doodad::new(1));
        server
            .world
            .worlds()
            .into_iter()
            .next()
            .expect("a world instance")
            .spawn(&doodad, &server.view_ctx());
        let from_doodad = gimmick_of(&server, 0x304, 0x303, 0);
        assert_eq!(owner_of(&WorldObject::from(&from_doodad)), None);
        assert_eq!(credit_of(&WorldObject::from(&from_doodad)), None);
        // No spawner at all, and a spawner that is gone.
        let none = gimmick_of(&server, 0x305, 0, 0);
        let gone = gimmick_of(&server, 0x306, 0x7777, 101);
        assert_eq!(owner_of(&WorldObject::from(&none)), None);
        assert_eq!(owner_of(&WorldObject::from(&gone)), None);
        // Two gimmicks that name each other (a reused object id): bounded, no owner.
        let a = gimmick_of(&server, 0x307, 0x308, 101);
        let b = gimmick_of(&server, 0x308, 0x307, 101);
        assert_eq!(owner_of(&WorldObject::from(&a)), None);
        assert_eq!(owner_of(&WorldObject::from(&b)), None);

        // The player credits itself; an NPC credits nobody.
        assert_eq!(
            credit_of(&WorldObject::from(&thrower)),
            Some(thrower.obj_id)
        );
        let npc = crate::world::Entity::new(0x309, crate::models::npc::Npc::new(1));
        assert_eq!(credit_of(&WorldObject::from(&npc)), None);
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
    /// duel ends the same pair is refused again. Only an **accepted** duel counts: while the
    /// request is unanswered the lockdown still holds.
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
        assert!(
            !server
                .systems
                .social
                .duels
                .is_dueling_with(a.character_id, b.character_id),
            "a pending request is no duel"
        );
        assert!(!can_attack(&server, &ta, &tb), "the request is unanswered");
        assert!(!can_attack(&server, &tb, &ta), "the request is unanswered");
        server
            .systems
            .social
            .duels
            .duel_accepted(&server, &b, a.character_id)
            .await;
        assert!(server
            .systems
            .social
            .duels
            .is_dueling_with(a.character_id, b.character_id));
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

    /// Exception (b): a hostile diplomacy relation that a nation *declared*
    /// (`CSFactionDeclareHostile`) lets it fight the declared alliance in Peace: while the
    /// declaration window is open and after it ended (the row then has `expire_at` 0 again but
    /// keeps the hostile `next_state` marker). Hostility nobody declared does not except: not the
    /// faction-166 template rows every new nation copies (hostile to both alliances from the
    /// start, `next_state` 0), and not the static Nuia/Haranya hostility.
    #[tokio::test]
    async fn peace_allows_factions_in_a_declared_hostile_diplomacy_relation() {
        use crate::systems::social::nation::{Nation, NationRelation};
        let server = crate::test_support::test_server_with(pvp_data());
        init_conflicts(&server);
        let hostile = RelationState::Hostile as u8;
        let nation = |id: u32, relations: Vec<NationRelation>| Nation {
            id,
            expedition_id: 0,
            owner_character_id: 0,
            owner_name: "King".into(),
            name: format!("Kingdom {id}"),
            created_at: 1,
            allow_change_name: 0,
            relations,
            offline_members: Vec::new(),
        };
        let row = |other_faction_id: u32, expire_at: i64, next_state: u8| NationRelation {
            other_faction_id,
            state: hostile,
            expire_at,
            next_state,
        };
        // 5000 declared hostility toward Nuia (window still open) and only inherited the
        // template row toward Haranya; 5001's declaration toward Nuia ran out its window.
        server.systems.social.nations.add(nation(
            5000,
            vec![row(148, 4_000_000_000, hostile), row(149, 0, 0)],
        ));
        server
            .systems
            .social
            .nations
            .add(nation(5001, vec![row(148, 0, hostile)]));
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let cd = crate::test_support::TestClient::new(&server, 4, 4);
        let citizen = player_in(&server, &ca, 0x101, 5000, PEACE_ZONE, 0);
        let nuian = player_in(&server, &cb, 0x109, 101, PEACE_ZONE, 0);
        let harani = player_in(&server, &cc, 0x20A, 109, PEACE_ZONE, 0);
        let veteran = player_in(&server, &cd, 0x30B, 5001, PEACE_ZONE, 0);
        // Every row below makes the pair Hostile; only the declared ones except.
        assert_eq!(faction_relation(&server, 5000, 101), RelationState::Hostile);
        assert_eq!(faction_relation(&server, 5000, 109), RelationState::Hostile);
        assert_eq!(faction_relation(&server, 5001, 101), RelationState::Hostile);
        let (tcit, tnu, thar, tvet) = (
            obj_target(&citizen),
            obj_target(&nuian),
            obj_target(&harani),
            obj_target(&veteran),
        );
        assert!(can_attack(&server, &tcit, &tnu), "declared (window open)");
        assert!(can_attack(&server, &tnu, &tcit), "declared (window open)");
        assert!(can_attack(&server, &tvet, &tnu), "declared (window ended)");
        assert!(can_attack(&server, &tnu, &tvet), "declared (window ended)");
        assert!(
            !can_attack(&server, &tcit, &thar),
            "an inherited template row is not a declaration"
        );
        assert!(
            !can_attack(&server, &thar, &tcit),
            "an inherited template row is not a declaration"
        );
        // The static alliance hostility alone does not except either.
        assert!(!can_attack(&server, &tnu, &thar));
    }

    /// A duel request nobody answered yet is not a duel: for the 30 s the challenge lives, the
    /// lockdown still holds for a hostile pair and, with Ctrl+F, for a same-faction pair.
    #[tokio::test]
    async fn peace_refuses_attacks_while_a_duel_request_is_unanswered() {
        let server = crate::test_support::test_server_with(pvp_data());
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let cd = crate::test_support::TestClient::new(&server, 4, 4);
        let nuian = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let harani = player_in(&server, &cb, 0x109, 109, PEACE_ZONE, 0);
        let friend_a = player_in(&server, &cc, 0x102, 101, PEACE_ZONE, 0);
        let friend_b = player_in(&server, &cd, 0x10A, 101, PEACE_ZONE, 0);
        let duels = &server.systems.social.duels;
        duels.duel_request(&server, &nuian, harani.character_id);
        duels.duel_request(&server, &friend_a, friend_b.character_id);
        for (x, y) in [(&nuian, &harani), (&friend_a, &friend_b)] {
            assert!(
                duels.is_in_duel_with(x.character_id, y.character_id),
                "the request is registered"
            );
            assert!(
                !duels.is_dueling_with(x.character_id, y.character_id),
                "but nobody accepted it"
            );
        }
        friend_a.data().unit.force_attack = true;
        let (tn, th, tfa, tfb) = (
            obj_target(&nuian),
            obj_target(&harani),
            obj_target(&friend_a),
            obj_target(&friend_b),
        );
        assert_eq!(relation_to(&server, &tn, &th), RelationState::Hostile);
        assert!(!can_attack(&server, &tn, &th));
        assert!(!can_attack(&server, &th, &tn));
        assert!(
            !can_attack(&server, &tfa, &tfb),
            "Ctrl+F does not turn an unanswered request into a duel"
        );
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

    /// Adds a buff template plus a one-effect skill that applies it; returns the skill template.
    #[allow(clippy::too_many_arguments)]
    fn add_buff_skill(
        data: &mut GameData,
        skill_id: u32,
        buff_id: u32,
        kind: i32,
        stun: bool,
        target_type: u8,
        friendly: bool,
        non_friendly: bool,
    ) -> std::sync::Arc<crate::systems::skills::data::SkillTemplate> {
        use crate::systems::skills::data::effects::BuffEffect;
        use crate::systems::skills::data::{BuffTemplate, EffectKind, EffectTemplate};
        let skills = &mut data.systems.skills;
        skills.add_buff(BuffTemplate {
            id: buff_id,
            kind,
            duration: 10_000,
            stack_rule: 1,
            stun,
            ..Default::default()
        });
        skills.add_effect(
            EffectKind::BuffEffect,
            EffectTemplate::Buff(BuffEffect {
                id: 1,
                buff_id: Some(buff_id),
                chance: 100,
                stack: 0,
                ab_level: 0,
            }),
            900_010,
        );
        let mut template = super::super::tests::skill_template(skill_id);
        template.target_type = i32::from(target_type);
        let mut effect = super::super::tests::skill_effect(900_010, EffectKind::BuffEffect, 1);
        effect.always_hit = true;
        effect.friendly = friendly;
        effect.non_friendly = non_friendly;
        template.effects.push(effect);
        let template = std::sync::Arc::new(template);
        skills.add_skill((*template).clone());
        template
    }

    /// Adds two buffs (good 50_002 / bad 50_003) and a skill with one dispel effect.
    fn add_dispel_skill(
        data: &mut GameData,
        skill_id: u32,
        friendly: bool,
        non_friendly: bool,
        target_type: u8,
    ) -> std::sync::Arc<crate::systems::skills::data::SkillTemplate> {
        use crate::systems::skills::data::effects::DispelEffect;
        use crate::systems::skills::data::{BuffTemplate, EffectKind, EffectTemplate};
        let skills = &mut data.systems.skills;
        for (id, kind) in [(50_002u32, 1i32), (50_003u32, 2i32)] {
            skills.add_buff(BuffTemplate {
                id,
                kind,
                duration: 10_000,
                stack_rule: 1,
                ..Default::default()
            });
        }
        skills.add_effect(
            EffectKind::DispelEffect,
            EffectTemplate::Dispel(DispelEffect {
                id: 1,
                dispel_count: 1,
                cure_count: 1,
                buff_tag_id: 0,
            }),
            900_011,
        );
        let mut template = super::super::tests::skill_template(skill_id);
        template.target_type = i32::from(target_type);
        let mut effect = super::super::tests::skill_effect(900_011, EffectKind::DispelEffect, 1);
        effect.always_hit = true;
        effect.friendly = friendly;
        effect.non_friendly = non_friendly;
        template.effects.push(effect);
        let template = std::sync::Arc::new(template);
        skills.add_skill((*template).clone());
        template
    }

    /// A stun is an attack: a single-target hostile cast on a protected player is refused
    /// (Peace and a no-PvP instance), and still lands in Conflict.
    #[tokio::test]
    async fn peace_refuses_a_single_target_stun() {
        use crate::systems::skills::skill::use_skill;
        use crate::systems::skills::state::SkillRun;
        use crate::systems::skills::types::{
            skill_result, SkillCastTarget, SkillCaster, SkillObject,
        };

        let mut data = pvp_data();
        let template = add_buff_skill(
            &mut data,
            10_005,
            50_001,
            2,
            true,
            SkillTargetType::Hostile as u8,
            /* friendly */ false,
            /* non_friendly */ true,
        );
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let cd = crate::test_support::TestClient::new(&server, 4, 4);
        let ce = crate::test_support::TestClient::new(&server, 5, 5);
        let cf = crate::test_support::TestClient::new(&server, 6, 6);
        let peace_a = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let peace_b = player_in(&server, &cb, 0x109, 109, PEACE_ZONE, 0);
        let conflict_a = player_in(&server, &cc, 0x201, 101, CONFLICT_ZONE, 0);
        let conflict_b = player_in(&server, &cd, 0x209, 109, CONFLICT_ZONE, 0);
        let mirage_a = player_in(&server, &ce, 0x20A, 101, MIRAGE_ZONE, 100);
        let mirage_b = player_in(&server, &cf, 0x30B, 109, MIRAGE_ZONE, 100);

        let mut target = SkillCastTarget::Unit(peace_b.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(template.clone(), 1),
            &WorldObject::from(&peace_a),
            SkillCaster::Unit(peace_a.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, skill_result::NO_TARGET);
        assert!(
            !peace_b.data().unit.buffs.check_buff(50_001),
            "the protected player must not be stunned"
        );

        // A no-PvP instance has the same hole and the same fix.
        let mut target = SkillCastTarget::Unit(mirage_b.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(template.clone(), 1),
            &WorldObject::from(&mirage_a),
            SkillCaster::Unit(mirage_a.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, skill_result::NO_TARGET);
        assert!(!mirage_b.data().unit.buffs.check_buff(50_001));

        let mut target = SkillCastTarget::Unit(conflict_b.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(template, 1),
            &WorldObject::from(&conflict_a),
            SkillCaster::Unit(conflict_a.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, 0);
        assert!(
            conflict_b.data().unit.buffs.check_buff(50_001),
            "the Conflict cast still stuns"
        );
    }

    /// A non-friendly buff effect on an `AnyUnit` skill (not the Hostile target type) is also
    /// filtered: the per-effect gate must skip it on a protected player, not only the cast
    /// validation.
    #[tokio::test]
    async fn any_unit_skill_cannot_apply_a_hostile_buff_in_peace() {
        use crate::systems::skills::skill::use_skill;
        use crate::systems::skills::state::SkillRun;
        use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};

        let mut data = pvp_data();
        let template = add_buff_skill(
            &mut data,
            10_009,
            50_006,
            2,
            true,
            SkillTargetType::AnyUnit as u8,
            /* friendly */ false,
            /* non_friendly */ true,
        );
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let cd = crate::test_support::TestClient::new(&server, 4, 4);
        let peace_a = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let peace_b = player_in(&server, &cb, 0x109, 109, PEACE_ZONE, 0);
        let conflict_a = player_in(&server, &cc, 0x201, 101, CONFLICT_ZONE, 0);
        let conflict_b = player_in(&server, &cd, 0x209, 109, CONFLICT_ZONE, 0);

        let mut target = SkillCastTarget::Unit(peace_b.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(template.clone(), 1),
            &WorldObject::from(&peace_a),
            SkillCaster::Unit(peace_a.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, 0, "the AnyUnit cast itself is not refused");
        assert!(
            !peace_b.data().unit.buffs.check_buff(50_006),
            "the per-effect gate skips the hostile buff"
        );

        let mut target = SkillCastTarget::Unit(conflict_b.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(template, 1),
            &WorldObject::from(&conflict_a),
            SkillCaster::Unit(conflict_a.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, 0);
        assert!(conflict_b.data().unit.buffs.check_buff(50_006));
    }

    /// An area hostile effect skips the protected player and still lands on the target in
    /// Conflict; a friendly buff is never filtered.
    #[tokio::test]
    async fn peace_skips_an_aoe_stun_but_applies_it_in_conflict() {
        use crate::systems::skills::skill::use_skill;
        use crate::systems::skills::state::SkillRun;
        use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};

        let mut data = pvp_data();
        let template = add_buff_skill(
            &mut data,
            10_006,
            50_004,
            2,
            true,
            SkillTargetType::Hostile as u8,
            /* friendly */ false,
            /* non_friendly */ true,
        );
        let mut area = (*template).clone();
        area.target_area_radius = 6;
        area.target_selection = 2; // Target
        let area = std::sync::Arc::new(area);
        data.systems.skills.add_skill((*area).clone());
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let caster = player_in(&server, &ca, 0x101, 101, CONFLICT_ZONE, 0);
        let protected = player_in(&server, &cb, 0x109, 109, PEACE_ZONE, 0);
        let exposed = player_in(&server, &cc, 0x209, 109, CONFLICT_ZONE, 0);
        let mut target = SkillCastTarget::Unit(exposed.obj_id);
        let skill = SkillRun::new(area, 1);
        let (result, _) = use_skill(
            &server,
            &skill,
            &WorldObject::from(&caster),
            SkillCaster::Unit(caster.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, 0);
        // The protected player is dropped before the combat-dice / durability pass.
        assert!(!skill.state().hit_types.contains_key(&protected.obj_id));
        assert!(skill.state().hit_types.contains_key(&exposed.obj_id));
        assert!(
            exposed.data().unit.buffs.check_buff(50_004),
            "the Conflict target is stunned by the area"
        );
        assert!(
            !protected.data().unit.buffs.check_buff(50_004),
            "the Peace target is skipped by the area"
        );

        // A friendly buff cast on a friendly player in Peace is unchanged.
        let mut friendly_data = pvp_data();
        let friendly = add_buff_skill(
            &mut friendly_data,
            10_007,
            50_005,
            1,
            false,
            SkillTargetType::Friendly as u8,
            /* friendly */ true,
            /* non_friendly */ false,
        );
        crate::test_support::open_client_skills(&mut friendly_data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(friendly_data));
        super::super::bind(&server);
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let a = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let b = player_in(&server, &cb, 0x109, 101, PEACE_ZONE, 0);
        let mut target = SkillCastTarget::Unit(b.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(friendly, 1),
            &WorldObject::from(&a),
            SkillCaster::Unit(a.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, 0);
        assert!(
            b.data().unit.buffs.check_buff(50_005),
            "a friendly buff still lands in Peace"
        );
    }

    /// A hostile dispel on a protected player does nothing (it must not cure the target); on an
    /// attackable target it still purges the good buffs.
    #[tokio::test]
    async fn peace_hostile_dispel_does_not_cure() {
        use crate::systems::skills::buffs;
        use crate::systems::skills::skill::use_skill;
        use crate::systems::skills::state::SkillRun;
        use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};

        // Both-true effect (usable on either side) with one dispel and one cure count: this is
        // the shape whose cure branch used to run on a protected target.
        let mut data = pvp_data();
        let template = add_dispel_skill(
            &mut data,
            10_008,
            /* friendly */ true,
            /* non_friendly */ true,
            SkillTargetType::AnyUnit as u8,
        );
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let cb = crate::test_support::TestClient::new(&server, 2, 2);
        let cc = crate::test_support::TestClient::new(&server, 3, 3);
        let cd = crate::test_support::TestClient::new(&server, 4, 4);
        let peace_a = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let peace_b = player_in(&server, &cb, 0x109, 109, PEACE_ZONE, 0);
        let conflict_a = player_in(&server, &cc, 0x201, 101, CONFLICT_ZONE, 0);
        let conflict_b = player_in(&server, &cd, 0x209, 109, CONFLICT_ZONE, 0);
        for p in [&peace_b, &conflict_b] {
            let obj = WorldObject::from(p);
            buffs::add_buff_by_id(&server, &obj, 50_002, &obj, 0); // good
            buffs::add_buff_by_id(&server, &obj, 50_003, &obj, 0); // bad
        }

        let mut target = SkillCastTarget::Unit(peace_b.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(template.clone(), 1),
            &WorldObject::from(&peace_a),
            SkillCaster::Unit(peace_a.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, 0);
        assert!(
            peace_b.data().unit.buffs.check_buff(50_003),
            "the protected player keeps the bad buff (no cure)"
        );
        assert!(
            peace_b.data().unit.buffs.check_buff(50_002),
            "the protected player keeps the good buff (no purge)"
        );

        let mut target = SkillCastTarget::Unit(conflict_b.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(template, 1),
            &WorldObject::from(&conflict_a),
            SkillCaster::Unit(conflict_a.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, 0);
        assert!(
            !conflict_b.data().unit.buffs.check_buff(50_002),
            "the hostile dispel still purges the good buff in Conflict"
        );
        assert!(
            conflict_b.data().unit.buffs.check_buff(50_003),
            "the hostile dispel does not cure the bad buff"
        );
    }

    /// A Neutral NPC target (faction 2) keeps main's behaviour: a non-damage hostile effect
    /// still lands. Only the player-protection cases are filtered.
    #[tokio::test]
    async fn a_neutral_npc_still_takes_a_non_damage_hostile_effect() {
        use crate::models::npc::Npc;
        use crate::systems::skills::skill::use_skill;
        use crate::systems::skills::state::SkillRun;
        use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
        use crate::world::Entity;

        let mut data = pvp_data();
        data.factions.add(faction(2, 0)); // FactionsEnum.Neutral
        let template = add_buff_skill(
            &mut data,
            10_010,
            50_007,
            2,
            true,
            SkillTargetType::Hostile as u8,
            /* friendly */ false,
            /* non_friendly */ true,
        );
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let caster = player_in(&server, &ca, 0x101, 101, crate::test_support::TEST_ZONE, 0);
        let mut npc = Npc::new(900);
        npc.unit.faction_id = 2;
        npc.unit.hp = 5_000;
        npc.unit.max_hp = 5_000;
        npc.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let npc = Entity::new(0x401, npc);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&npc, &server.view_ctx());
        let tn = Target::Obj(WorldObject::from(&npc));
        let tc = obj_target(&caster);
        assert_eq!(relation_to(&server, &tc, &tn), RelationState::Neutral);
        assert!(
            !can_attack(&server, &tc, &tn),
            "a neutral NPC is not attackable"
        );
        assert!(!protected_from(&server, &tc, &tn));

        let mut target = SkillCastTarget::Unit(npc.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(template, 1),
            &WorldObject::from(&caster),
            SkillCaster::Unit(caster.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, 0);
        assert!(
            npc.data().unit.buffs.check_buff(50_007),
            "the neutral NPC still receives the hostile effect"
        );
    }

    /// A faction-2 NPC caster's trap still lands on a player: an NPC attacker is not subject to
    /// the player-protection rules.
    #[tokio::test]
    async fn a_neutral_npc_casters_trap_still_lands_on_a_player() {
        use crate::models::npc::Npc;
        use crate::systems::skills::skill::unit_use_skill;
        use crate::world::Entity;

        let mut data = pvp_data();
        data.factions.add(faction(2, 0));
        let _template = add_buff_skill(
            &mut data,
            10_011,
            50_008,
            2,
            true,
            SkillTargetType::Hostile as u8,
            /* friendly */ false,
            /* non_friendly */ true,
        );
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let player = player_in(&server, &ca, 0x101, 101, crate::test_support::TEST_ZONE, 0);
        let mut npc = Npc::new(900);
        npc.unit.faction_id = 2;
        npc.unit.hp = 5_000;
        npc.unit.max_hp = 5_000;
        npc.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let npc = Entity::new(0x401, npc);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&npc, &server.view_ctx());
        let result = unit_use_skill(
            &server,
            &WorldObject::from(&npc),
            10_011,
            &WorldObject::from(&player),
        );
        assert_eq!(result, 0);
        assert!(
            player.data().unit.buffs.check_buff(50_008),
            "the NPC's trap applies to the player"
        );
    }

    /// A hostile skill may carry friendly-only Target effects for its own caster: the area
    /// target filter keeps the caster even when the caster is protected by Peace.
    #[tokio::test]
    async fn a_hostile_skill_keeps_friendly_target_effects_on_its_caster() {
        use crate::models::npc::Npc;
        use crate::systems::skills::skill::use_skill;
        use crate::systems::skills::state::SkillRun;
        use crate::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
        use crate::world::Entity;

        let mut data = pvp_data();
        data.factions.add(faction(2, 0));
        let mut template = (*add_buff_skill(
            &mut data,
            10_012,
            50_009,
            1,
            false,
            SkillTargetType::Hostile as u8,
            /* friendly */ true,
            /* non_friendly */ false,
        ))
        .clone();
        template.target_area_radius = 5;
        template.target_selection = 2; // Target
        let template = std::sync::Arc::new(template);
        data.systems.skills.add_skill((*template).clone());
        crate::test_support::open_client_skills(&mut data);
        let server = std::sync::Arc::new(crate::test_support::test_server_with(data));
        super::super::bind(&server);
        init_conflicts(&server);
        let ca = crate::test_support::TestClient::new(&server, 1, 1);
        let caster = player_in(&server, &ca, 0x101, 101, PEACE_ZONE, 0);
        let mut npc = Npc::new(900);
        npc.unit.faction_id = 2;
        npc.unit.hp = 5_000;
        npc.unit.max_hp = 5_000;
        npc.unit.transform.set_xyz(100.0, 100.0, 100.0);
        let npc = Entity::new(0x401, npc);
        server
            .world
            .world(0)
            .unwrap()
            .spawn(&npc, &server.view_ctx());
        let mut target = SkillCastTarget::Unit(npc.obj_id);
        let (result, _) = use_skill(
            &server,
            &SkillRun::new(template, 1),
            &WorldObject::from(&caster),
            SkillCaster::Unit(caster.obj_id),
            &mut target,
            Some(SkillObject::None),
            true,
        );
        assert_eq!(result, 0);
        assert!(
            caster.data().unit.buffs.check_buff(50_009),
            "the friendly-only effect still reaches the caster"
        );
    }
}
