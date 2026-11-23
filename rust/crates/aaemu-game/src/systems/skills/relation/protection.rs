use crate::data::world_data::conflict_type;
use crate::server::GameServer;
use crate::systems::social::nation;
use crate::world::{PlayerRef, WorldObject};

use super::super::geometry::Target;
use super::super::types::{buff_constants, RelationState};
use super::ownership::acting_faction;
use super::{faction_of, faction_relation, is_actively_hostile, owner_character, FACTION_NEUTRAL};

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
    if target
        .as_obj()
        .is_some_and(super::buffs::nerta_bomb_drop_active)
    {
        return false;
    }
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
