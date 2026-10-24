//! `lunafrost`: live check of the lunastone (lunafrost) enchant, skill 11285 (GM account needed).
//!
//! The lunafrost item (24692 "Whole Insightful Lunafrost", `use_skill_as_reagent`) casts the skill
//! 11285 whose only effect is an `InteractionEffect` with `MAGICAL_ENCHANT`. The client sends the
//! lunafrost as the `SkillItem` caster and the gear as a `SkillCastItemTarget`. The server must
//!
//! * answer with `SCItemSocketingLunastoneResult` (result true, the gear id, the rune template),
//! * send an `SCItemTaskSuccess` of type `EnchantMagical` (49) that updates the gear with the rune,
//! * consume exactly one lunafrost.
//!
//! The scenario enchants a bag item first (a head armor: 24692 fits equip slot group 5 = head),
//! then equips it and enchants the worn item with a second lunafrost. Server-side stat effects of
//! the worn rune are checked in the server unit tests, not on the wire.

use std::time::Duration;

use aaemu_game::opcodes::sc;
use anyhow::{bail, ensure, Context};

use super::fixes::gm_access;
use super::questitem::{chat_text, gm_command, stack_location};
use super::{own_skill_response, skill_response_result, Runner};
use crate::game::{GameClient, Packet};

/// The lunafrost use skill.
const SKILL: u32 = 11285;
/// "Whole Insightful Lunafrost": `item_enchanting_gems.equip_slot_group_id` 5 (head).
const LUNAFROST: u32 = 24692;
/// "First Steps Leather Cap": a head armor, level 1.
const HEAD_ARMOR: u32 = 6735;
/// `ItemTaskType.EnchantMagical`.
const ENCHANT_MAGICAL: u8 = 49;
/// `slot_type` of the equipment and bag containers; the head is equipment slot 0.
const EQUIPMENT: u8 = 1;
const BAG: u8 = 2;
/// Packets that fill the log of a cast but say nothing about it.
const NOISE: [&str; 4] = [
    "SCOneUnitMovementPacket",
    "SCUnitModelPostureChangedPacket",
    "SCUnitPointsPacket",
    "PongPacket",
];

/// What one lunafrost cast produced on the wire.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// `SkillResult` of the skill response (`None` when the server sent none).
    skill_result: Option<u8>,
    /// `(result, item id, template id)` of `SCItemSocketingLunastoneResult`.
    lunastone: Option<(bool, u64, u32)>,
    /// An `EnchantMagical` item task that updates the gear and carries `LUNAFROST` in its details.
    enchant_task: bool,
    /// Item tasks (other than `EnchantMagical`) that mention the lunafrost stack: the consumption.
    consumed: usize,
    /// The task types of all item task packets, for the report.
    task_types: Vec<u8>,
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn analyze(packets: &[Packet], caster: u32, gear: u64, stack: u64) -> anyhow::Result<Outcome> {
    let mut out = Outcome {
        skill_result: None,
        lunastone: None,
        enchant_task: false,
        consumed: 0,
        task_types: Vec::new(),
    };
    for p in packets {
        if own_skill_response(p, SKILL, caster) {
            if out.skill_result.is_none() {
                out.skill_result = Some(skill_response_result(p)?);
            }
        } else if p.is_sc(sc::SCItemSocketingLunastoneResultPacket) {
            let mut s = p.stream();
            out.lunastone = Some((s.read_bool()?, s.read_u64()?, s.read_u32()?));
        } else if p.is_sc(sc::SCItemTaskSuccessPacket) {
            let kind = *p.body.first().context("empty item task")?;
            out.task_types.push(kind);
            if kind == ENCHANT_MAGICAL {
                out.enchant_task |= contains(&p.body, &gear.to_le_bytes())
                    && contains(&p.body, &LUNAFROST.to_le_bytes());
            } else if contains(&p.body, &stack.to_le_bytes()) {
                out.consumed += 1;
            }
        }
    }
    Ok(out)
}

/// Casts the lunafrost `stack` on `gear` and checks the answer.
async fn enchant(
    game: &mut GameClient,
    r: &mut Runner<'_>,
    label: &str,
    stack: u64,
    gear: u64,
) -> anyhow::Result<()> {
    let s = r.begin(&format!("game.lunafrost.{label}"));
    // The skill has a short cooldown; let the previous cast settle.
    tokio::time::sleep(Duration::from_secs(3)).await;
    game.start_item_skill_on_item(SKILL, stack, LUNAFROST, gear)
        .await?;
    let packets = game
        .drain(Duration::from_secs(2), Duration::from_secs(12))
        .await?;
    let caster = game.state.self_obj;
    let o = analyze(&packets, caster, gear, stack)?;
    if let Some(result) = o.skill_result {
        ensure!(
            result == 0,
            "{label}: skill {SKILL} answered SkillResult {result}"
        );
    }
    let Some((ok, item, template)) = o.lunastone else {
        bail!(
            "{label}: no SCItemSocketingLunastoneResult (skill result {:?}, item task types {:?}, \
             packets {:?})",
            o.skill_result,
            o.task_types,
            packets
                .iter()
                .map(Packet::name)
                .filter(|n| !NOISE.contains(n))
                .collect::<Vec<_>>()
        );
    };
    ensure!(
        ok && item == gear && template == LUNAFROST,
        "{label}: lunastone result ({ok}, item {item}, template {template}), expected \
         (true, {gear}, {LUNAFROST})"
    );
    ensure!(
        o.enchant_task,
        "{label}: no EnchantMagical item task updating {gear} with rune {LUNAFROST} (task types {:?})",
        o.task_types
    );
    ensure!(
        o.consumed == 1,
        "{label}: the lunafrost stack {stack} was consumed {} times (task types {:?}), expected once",
        o.consumed,
        o.task_types
    );
    r.end(
        s,
        format!(
            "SkillResult {:?}, lunastone result true for item {gear}, EnchantMagical task, \
             lunafrost consumed once (item task types {:?})",
            o.skill_result, o.task_types
        ),
    );
    Ok(())
}

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    let s = r.begin("game.lunafrost.gm");
    gm_access(game).await?;
    r.end(s, "GM commands accepted");

    let s = r.begin("game.lunafrost.setup");
    let reply = gm_command(game, &format!("item add self {HEAD_ARMOR} 1")).await?;
    let (gear, gear_type, gear_slot) = stack_location(&reply, HEAD_ARMOR).with_context(|| {
        format!(
            "no item {HEAD_ARMOR} was added (GM replies: {:?})",
            reply.iter().filter_map(chat_text).collect::<Vec<_>>()
        )
    })?;
    let reply = gm_command(game, &format!("item add self {LUNAFROST} 2")).await?;
    let (stack, _, _) = stack_location(&reply, LUNAFROST).with_context(|| {
        format!(
            "no item {LUNAFROST} was added (GM replies: {:?})",
            reply.iter().filter_map(chat_text).collect::<Vec<_>>()
        )
    })?;
    ensure!(
        gear_type == BAG,
        "the armor was not added to the bag ({gear_type})"
    );
    r.end(
        s,
        format!("armor {gear} in bag slot {gear_slot}, 2 x lunafrost stack {stack}"),
    );

    enchant(game, r, "bag", stack, gear).await?;

    let s = r.begin("game.lunafrost.equip");
    game.swap_slots((gear, BAG, gear_slot), (0, EQUIPMENT, 0))
        .await?;
    let packets = game
        .drain(Duration::from_secs(1), Duration::from_secs(5))
        .await?;
    ensure!(
        packets.iter().any(|p| p.is_sc(sc::SCItemTaskSuccessPacket)),
        "equipping the armor was not acknowledged"
    );
    r.end(s, format!("armor {gear} equipped in the head slot"));

    enchant(game, r, "worn", stack, gear).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::LEVEL_GAME;

    fn packet(opcode: u16, body: Vec<u8>) -> Packet {
        Packet {
            level: LEVEL_GAME,
            opcode,
            body,
        }
    }

    fn task(kind: u8, mention: &[u64], rune: bool) -> Packet {
        let mut body = vec![kind, 1];
        for id in mention {
            body.extend_from_slice(&id.to_le_bytes());
        }
        if rune {
            body.extend_from_slice(&LUNAFROST.to_le_bytes());
        }
        packet(sc::SCItemTaskSuccessPacket, body)
    }

    fn lunastone(ok: bool, item: u64) -> Packet {
        let mut body = vec![u8::from(ok)];
        body.extend_from_slice(&item.to_le_bytes());
        body.extend_from_slice(&LUNAFROST.to_le_bytes());
        packet(sc::SCItemSocketingLunastoneResultPacket, body)
    }

    #[test]
    fn a_complete_enchant_is_recognised() {
        let packets = [
            lunastone(true, 77),
            task(ENCHANT_MAGICAL, &[77], true),
            task(41, &[900], false),
        ];
        let o = analyze(&packets, 1, 77, 900).unwrap();
        assert_eq!(o.lunastone, Some((true, 77, LUNAFROST)));
        assert!(o.enchant_task);
        assert_eq!(o.consumed, 1);
        assert_eq!(o.task_types, vec![ENCHANT_MAGICAL, 41]);
    }

    #[test]
    fn a_cast_without_the_interaction_effect_shows_nothing() {
        // The pre-fix server only consumed the item: no lunastone result, no EnchantMagical task.
        let o = analyze(&[task(41, &[900], false)], 1, 77, 900).unwrap();
        assert_eq!(o.lunastone, None);
        assert!(!o.enchant_task);
        assert_eq!(o.consumed, 1);
    }

    #[test]
    fn an_enchant_task_without_the_rune_is_not_enough() {
        let o = analyze(&[task(ENCHANT_MAGICAL, &[77], false)], 1, 77, 900).unwrap();
        assert!(!o.enchant_task);
    }
}
