use crate::server::GameServer;
use crate::systems::instances::encounters::registry as encounters;
use crate::systems::npcs::npc::data::NpcTemplate;
use crate::systems::skills::controllers::calculate_distance;
use crate::systems::skills::state::AggroKind;
use crate::world::{now_ms, WorldObject};

use super::super::combat::{add_unit_aggro, can_attack};
use super::super::framework::AiCtx;

use super::npc;
use super::support::template_scalars;

/// Whether this AI-less NPC has not been reported yet (the help loop runs on every tick, and
/// the crawler produced 429 repeated errors for a handful of ambient NPCs).
fn report_npc_without_ai(obj_id: u32) -> bool {
    static REPORTED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u32>>> =
        std::sync::OnceLock::new();
    // Respawned NPCs get fresh object ids: bound the set so it cannot grow for the life of
    // the process. A clear only lets each NPC warn once more.
    const MAX_REPORTED: usize = 4096;
    let mut reported = REPORTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if reported.len() >= MAX_REPORTED {
        reported.clear();
    }
    reported.insert(obj_id)
}

/// 10.x candidate scan radius of the aggro-link help (`sub_39325250(..., 50.0, ...)`,
/// Z:823753). The guard help of a player / mate / slave victim scans the same radius around the
/// victim; the candidate's own `aggro_link_help_dist` (or its sight check) still applies.
const GUARD_HELP_SCAN_RADIUS: f32 = 50.0;

/// The player-victim guard scan runs at most once per second per `(victim, abuser)` pair:
/// DoT ticks and multi-hit skills would otherwise repeat the 50 m query for the same fight.
pub(super) const GUARD_HELP_SCAN_INTERVAL_MS: i64 = 1_000;

/// Upper bound of the per-pair scan stamps; old entries are pruned at the cap.
const GUARD_HELP_SCAN_MAX_ENTRIES: usize = 4_096;

/// Rate limit of the player-victim guard scan (`server.systems.ai.guard_help_scans`): true when
/// this `(victim, abuser)` pair may scan now, and records the scan otherwise.
fn guard_help_scan_due(server: &GameServer, victim: u32, abuser: u32) -> bool {
    let now = now_ms();
    let mut scans = server
        .systems
        .ai
        .guard_help_scans
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if scans.len() >= GUARD_HELP_SCAN_MAX_ENTRIES {
        scans.retain(|_, at| now - *at < GUARD_HELP_SCAN_INTERVAL_MS);
        if scans.len() >= GUARD_HELP_SCAN_MAX_ENTRIES {
            scans.clear();
        }
    }
    match scans.get(&(victim, abuser)) {
        Some(at) if now - *at < GUARD_HELP_SCAN_INTERVAL_MS => false,
        _ => {
            scans.insert((victim, abuser), now);
            true
        }
    }
}

/// The `aggro_link_special_rule_id` switch of the 10.x help scan (Z:823790-823816) against the
/// victim: 1 = same faction, 2 = friendly, 3 = neutral, 4 = everyone.
fn guard_rule_helps(
    server: &GameServer,
    helper: &WorldObject,
    victim: &WorldObject,
    helper_faction: u32,
    victim_faction: u32,
    template: &NpcTemplate,
) -> bool {
    use crate::systems::skills::types::RelationState as R;
    match template.aggro_link_special_rule_id {
        1 => helper_faction == victim_faction,
        2 => {
            crate::systems::skills::relation::relation_to(
                server,
                &crate::systems::skills::geometry::Target::Obj(helper.clone()),
                &crate::systems::skills::geometry::Target::Obj(victim.clone()),
            ) == R::Friendly
        }
        3 => {
            crate::systems::skills::relation::relation_to(
                server,
                &crate::systems::skills::geometry::Target::Obj(helper.clone()),
                &crate::systems::skills::geometry::Target::Obj(victim.clone()),
            ) == R::Neutral
        }
        4 => true,
        _ => false,
    }
}

/// The guard branch of the 10.x aggro-link help (Z:823880-823909) on top of the rule switch:
/// a `aggro_link_special_guard` answers a victim whose faction has `system_factions.guard_help`,
/// never a duel between the victim and the attacker (`sub_39322ED0`), never an NPC attacker when
/// its template carries `aggro_link_special_ignore_npc_attacker` (0x40, so guards ignore monster
/// attackers), never another `aggro_link_special_guard` (0x20), and only an attacker the guard
/// may attack (`CanAttack`). The 10.x flag bits of the NPC template: 0x2 `accept_aggro_link`,
/// 0x10 `aggro_link_sight_check`, 0x20 `aggro_link_special_guard`,
/// 0x40 `aggro_link_special_ignore_npc_attacker` (inferred from the loader column order,
/// Z:2426767). The 10.x `guard_npc_no_help_zone` relation reason has no 1.2 zone column.
fn guard_may_help(
    server: &GameServer,
    victim: &WorldObject,
    abuser: &WorldObject,
    helper: &WorldObject,
    template: &NpcTemplate,
    victim_faction: u32,
) -> bool {
    if !server
        .game_data
        .factions
        .get(victim_faction)
        .is_some_and(|f| f.guard_help)
    {
        return false;
    }
    if let Some(abuser_npc) = abuser.as_npc() {
        if template.aggro_link_special_ignore_npc_attacker {
            return false;
        }
        if npc::template(server, abuser_npc).is_some_and(|t| t.aggro_link_special_guard) {
            return false;
        }
    }
    // An accepted duel is not helped, including when the attacker or the victim is the
    // duellist's pet or slave (the 10.x check compares the units' relations, and
    // `owner_character` resolves a mate / slave to its owner). `is_dueling_with`, not
    // `is_in_duel_with`: an unanswered challenge (it lives up to 30 s) is no duel yet, so a
    // hostile player cannot silence the guards by sending a duel request before the attack.
    if let (Some(v), Some(a)) = (
        crate::systems::skills::relation::owner_character(server, victim),
        crate::systems::skills::relation::owner_character(server, abuser),
    ) {
        if server
            .systems
            .social
            .duels
            .is_dueling_with(v.character_id, a.character_id)
        {
            return false;
        }
    }
    can_attack(server, helper, abuser)
}

/// 10.x aggro-link guard help for a victim without an NPC AI (player, mate, slave): the damage
/// path calls it when a unit is attacked, so special guards within
/// [`GUARD_HELP_SCAN_RADIUS`] of the victim join the fight against `abuser` (Z:823528-823950).
/// NPC victims keep the C# `UpdateAggroHelp` path (`update_aggro_help`) unchanged, so guards
/// still answer a fellow guard / group NPC attacked by a monster.
pub fn update_guard_help(server: &GameServer, victim: &WorldObject, abuser: &WorldObject) {
    if victim.same(abuser) || !abuser.is_unit() {
        return;
    }
    let Some(victim_faction) = victim.with_unit(|u| u.unit().faction_id) else {
        return;
    };
    // `system_factions.guard_help` is a property of the victim's faction; skip the scan when it
    // is off (monster factions, ...), so the cost is only paid near a protected victim.
    if !server
        .game_data
        .factions
        .get(victim_faction)
        .is_some_and(|f| f.guard_help)
    {
        return;
    }
    // An NPC attacker can only pull guards whose template does not carry the 0x40
    // `aggro_link_special_ignore_npc_attacker` flag; when no loaded guard template lacks it,
    // the scan is pointless (cheap data-level check before the 50 m query).
    if abuser.as_npc().is_some()
        && !server
            .game_data
            .systems
            .npcs
            .npc
            .special_guards_allow_npc_attacker
    {
        return;
    }
    // DoT ticks and repeated hits of the same fight reuse the first scan for one second.
    if !guard_help_scan_due(server, victim.obj_id(), abuser.obj_id()) {
        return;
    }
    let candidates: Vec<WorldObject> = npc::around(server, victim, GUARD_HELP_SCAN_RADIUS, false)
        .into_iter()
        .filter(|o| o.as_npc().is_some())
        .collect();
    for other in candidates {
        let Some(other_npc) = other.as_npc().cloned() else {
            continue;
        };
        let Some(ot) = npc::template(server, &other_npc) else {
            continue;
        };
        if !ot.aggro_link_special_guard || !ot.accept_aggro_link {
            continue;
        }
        if other.same(victim) || other.same(abuser) || npc::is_dead(&other) {
            continue;
        }
        let (in_battle, helper_faction) = {
            let d = other_npc.data();
            (d.unit.is_in_battle, d.unit.faction_id)
        };
        if in_battle {
            continue;
        }
        let distance = npc::distance_to(server, &other, victim, false);
        if distance > GUARD_HELP_SCAN_RADIUS {
            continue;
        }
        // 10.x: with `aggro_link_sight_check` the guard must see the victim and the per-template
        // help distance does not gate it; without it, `aggro_link_help_dist` does (Z:823780-790).
        let in_range = if ot.aggro_link_sight_check {
            npc::can_see_target(server, &other, victim)
        } else {
            distance <= ot.aggro_link_help_dist
        };
        if !in_range
            || !guard_rule_helps(server, &other, victim, helper_faction, victim_faction, ot)
            || !guard_may_help(server, victim, abuser, &other, ot, victim_faction)
        {
            continue;
        }
        if !super::manager::has_ai(server, other_npc.obj_id) {
            continue;
        }
        add_unit_aggro(server, &other, AggroKind::Damage, abuser, 1);
        super::manager::post(server, other_npc.obj_id, |o| o.on_aggro_target_changed());
        tracing::debug!(
            guard = other_npc.obj_id,
            victim = victim.obj_id(),
            abuser = abuser.obj_id(),
            "guard help: joined the fight"
        );
    }
}

/// `Behavior.UpdateAggroHelp(abuser, radius)`.
pub fn update_aggro_help(c: &mut AiCtx<'_>, abuser: &WorldObject, radius: f32) {
    // `npc_groups.aggro_rule_id == 2` mirrors the aggro table; rule 1 members answer the help
    // call through the group-link path below (10.x `NpcAggroTable::CollectSharedAggroMembers`
    // and the link-help group-instance relation bypass).
    super::group::share_aggro_with_group(c, abuser);
    let t = template_scalars(c);
    let (my_template_id, my_group_instance, my_group_rule, my_faction) = {
        let d = c.npc.data();
        (
            d.unit.template_id,
            d.npc_group_instance,
            d.npc_group_aggro_rule_id,
            d.unit.faction_id,
        )
    };
    let link_data = &c.server.game_data.systems.npcs.npc.aggro_links;
    let nearby_dungeon_help =
        c.server.world.world_of(&c.obj).is_some_and(|world| {
            encounters::helps_only_near_the_pull(world.template.name.as_str())
        });
    let attacker_pos = nearby_dungeon_help.then(|| npc::world_pos(c.server, abuser));
    let npcs: Vec<WorldObject> =
        npc::around(c.server, &c.obj, t.attack_start_range_scale * radius, false)
            .into_iter()
            .filter(|o| o.as_npc().is_some())
            .collect();
    for other in npcs {
        let Some(other_npc) = other.as_npc().cloned() else {
            continue;
        };
        let Some(ot) = npc::template(c.server, &other_npc) else {
            continue;
        };
        let (in_battle, faction, other_group_instance) = {
            let d = other_npc.data();
            (d.unit.is_in_battle, d.unit.faction_id, d.npc_group_instance)
        };
        // 10.x link help with `npc_groups.aggro_rule_id == 1` (Z:823640-823646 /
        // 823849-823851): a candidate of the same spawned batch is accepted even when its own
        // template relation test fails, so the group mates answer the help call together. The
        // scan still requires the candidate's `accept_aggro_link` (Z:823774), so a template that
        // declines aggro links keeps declining (43 of the 376 rule-1 member rows). The batch's
        // rule 2 members are already on the table (mirrored above); rule 0 neither mirrors nor
        // answers by group.
        let group_link = my_group_instance != 0
            && my_group_rule == 1
            && other_group_instance == my_group_instance
            && ot.accept_aggro_link;
        // 1.2 data: the members of an `aggro_links` group help each other even when their own
        // template (`accept_aggro_link`, `aggression` / `aggro_link_special_rule_id`) would
        // decline. No consumer was found in the 10.x decompile, so this rule is data-inferred
        // (capture-pending AGGROLINK-1). Non-members keep the template rules exactly.
        let same_group = link_data.share_group(my_template_id, ot.id);
        if in_battle || (!ot.accept_aggro_link && !same_group) {
            continue;
        }
        // Rule-1 batch mates answer out to their own aggression radius as a fallback: the 1.2
        // `aggro_link_help_dist` is 6 m for 91 of the 101 rule-1 groups (376 member rows:
        // median 6, max 50, none 0), too short once the batch spreads beyond its 2-3 m
        // formation offsets. The 10.x link-help scan collects candidates within 50 m
        // (`sub_39325250(..., 50.0, ...)`, Z:823753) and then applies the per-template
        // distance, so a group-link mate uses `max(aggro_link_help_dist,
        // attack_start_range_scale * 10)` (the `check_aggression` scan radius).
        let help_dist = if group_link {
            ot.aggro_link_help_dist
                .max(ot.attack_start_range_scale * 10.0)
        } else {
            ot.aggro_link_help_dist
        };
        if npc::distance_to(c.server, &other, &c.obj, false) > help_dist {
            continue;
        }
        // The help distance is horizontal: the Howling Abyss Medusa packs (30 m) answered the
        // Knights fighting on the walkway 15 m above their room.
        if encounters::floor_bound(other.transform().zone_id, ot.id)
            && (npc::world_pos(c.server, &other).z - npc::world_pos(c.server, abuser).z).abs()
                > encounters::FLOOR_GAP
        {
            continue;
        }
        // Keep incidental assistance near the pull. Authored groups have their own rules.
        if let Some(attacker) = attacker_pos.filter(|_| !group_link) {
            let helper = npc::world_pos(c.server, &other);
            if (helper.z - attacker.z).abs() > 4.0
                || calculate_distance(helper, attacker, true) > help_dist.max(10.0)
                || !npc::can_see_target(c.server, &other, abuser)
                || super::collision::line_of_sight(
                    c.server,
                    other.transform().instance_id,
                    [helper.x, helper.y, helper.z + 1.0],
                    [attacker.x, attacker.y, attacker.z + 1.0],
                ) == Some(false)
            {
                continue;
            }
        }
        // The template path is the C# `UpdateAggroHelp` (Behavior.cs, `a2ab80abd^`): the
        // special-rule branch requires `aggro_link_sight_check` to be enabled AND the abuser
        // visible; a template with sight check off never helps through rules 1..4.
        let template_helps = if ot.aggression && ot.aggro_link_special_rule_id == 0 {
            // Deviation from the C# (which helped unconditionally here): a helper of another
            // faction joins only a fight it could start itself. The system-neutral Gossiper
            // Banshees (faction 2) of the Requiem Cape fort stood among faction-115 Bloodhands,
            // took their attackers into the aggro table, could not attack them and stayed in
            // combat until the player relogged. Same-faction packs still help each other.
            faction == my_faction || can_attack(c.server, &other, abuser)
        } else {
            if !(ot.aggro_link_sight_check && npc::can_see_target(c.server, &other, abuser)) {
                false
            } else {
                let relation = || {
                    crate::systems::skills::relation::relation_to(
                        c.server,
                        &crate::systems::skills::geometry::Target::Obj(other.clone()),
                        &crate::systems::skills::geometry::Target::Obj(c.obj.clone()),
                    )
                };
                use crate::systems::skills::types::RelationState as R;
                match ot.aggro_link_special_rule_id {
                    1 => faction == my_faction,
                    2 => relation() == R::Friendly,
                    3 => relation() == R::Neutral,
                    4 => true,
                    _ => false,
                }
            }
        };
        // Link path (AGGROLINK-1): the sight check applies only when the helper enables it.
        let link_helps = same_group
            && (!ot.aggro_link_sight_check || npc::can_see_target(c.server, &other, abuser));
        // Rule-1 group mates that accept aggro links answer the help call (the relation test
        // above is bypassed); the helper's own sight check still applies, as in the 10.x
        // candidate scan.
        let group_link_helps = group_link
            && (!ot.aggro_link_sight_check || npc::can_see_target(c.server, &other, abuser));
        if !template_helps && !link_helps && !group_link_helps {
            continue;
        }
        // npc.Ai.Owner.AddUnitAggro(...) + npc.Ai.OnAggroTargetChanged(): an NPC without an
        // AI threw a NullReferenceException in C#, which ended the whole help loop. Keep
        // helping the remaining linked NPCs and report each AI-less one once.
        if !super::manager::has_ai(c.server, other_npc.obj_id) {
            if report_npc_without_ai(other_npc.obj_id) {
                tracing::warn!(
                    "UpdateAggroHelp: npc {} has no AI; skipped (C# threw a NullReferenceException)",
                    other_npc.obj_id
                );
            }
            continue;
        }
        let already_helping = other_npc
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&abuser.obj_id());
        // Some helpers have no in-combat event skill, so is_in_battle can remain false
        // while they chase. Repeated help calls must not restart that chase.
        if already_helping {
            continue;
        }
        add_unit_aggro(c.server, &other, AggroKind::Damage, abuser, 1);
        if !other_npc
            .data()
            .unit
            .skill_state
            .aggro_table
            .contains_key(&abuser.obj_id())
        {
            continue;
        }
        if (link_helps || group_link_helps) && !template_helps {
            tracing::debug!(
                helper = other_npc.obj_id,
                leader = c.npc.obj_id,
                abuser = abuser.obj_id(),
                group_link = group_link_helps,
                "aggro link: group member joined the fight"
            );
        }
        super::manager::post(c.server, other_npc.obj_id, |o| o.on_aggro_target_changed());
    }
}
