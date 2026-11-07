//! `socket`: live check of lunagem socketing, skill 23728 (GM account needed).
//!
//! The 1.2 client checks the socket rules itself before sending the skill, so a retail server
//! must apply them too. The scenario adds a grade-4 chest armor and two lunagems with the GM
//! `item add` command, then casts the socketing skill 23728 with each gem as the `SkillItem`
//! caster and the armor as the `SkillCastItemTarget`:
//!
//! * a hands lunagem (group 18) on a chest (slot type 3) is rejected with `SCErrorMsg` 18
//!   (`invalid_target`) and is not consumed: a second cast of the same stack is rejected too;
//! * a chest lunagem (group 17) is accepted: `SCItemSocketingLunagemResult` (result 1, the armor
//!   id, the gem template, install true), an `SCItemTaskSuccess` of type 99 (`Socketing`) that
//!   updates the armor, and exactly one `SkillReagents` task that consumes the gem stack.
//!
//! The chest armor 19327 is item level 53 with a level-50 requirement; its grade-4 socket limit
//! (`item_socket_num_limits` slot 3 / grade 4) is 3.

use std::time::Duration;

use aaemu_game::opcodes::sc;
use anyhow::{bail, ensure, Context};

use super::fixes::gm_access;
use super::questitem::{chat_text, gm_command, stack_location};
use super::{skill_response_result, Runner};
use crate::game::{GameClient, Packet};

/// The lunagem socketing use skill.
const SKILL: u32 = 23728;
/// A chest armor: slot type 3, item level 53, level requirement 50.
const ARMOR: u32 = 19327;
/// "Clear Earth Lunagem: Effervescence": `item_sockets` group 17 (chest / legs), no level limit.
const CHEST_GEM: u32 = 30958;
/// "Clear Earth Lunagem: Finesse": `item_sockets` group 18 (hands), no level limit.
const HANDS_GEM: u32 = 30959;
/// The armor's grade: slot type 3 / grade 4 allows 3 gems.
const ARMOR_GRADE: u8 = 4;
/// `ItemTaskType.Socketing`.
const SOCKETING: u8 = 99;
/// `ItemTaskType.SkillReagents`.
const SKILL_REAGENTS: u8 = 40;
/// `ErrorMessageType.InvalidTarget` (wrong slot group).
const INVALID_TARGET: i16 = 18;

/// What one socketing cast produced on the wire.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// `(result, item id, template id, install)` of `SCItemSocketingLunagemResult`.
    lunagem: Option<(u8, u64, u32, bool)>,
    /// `SCErrorMsg` codes seen in the cast.
    errors: Vec<i16>,
    /// An item task of type `Socketing` that updates the armor.
    socket_task: bool,
    /// Item tasks (other than `Socketing`) that mention the gem stack: take / restore / consume.
    source_tasks: usize,
    /// The task types of all item task packets, for the report.
    task_types: Vec<u8>,
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn analyze(packets: &[Packet], armor: u64, stack: u64) -> anyhow::Result<Outcome> {
    let mut out = Outcome {
        lunagem: None,
        errors: Vec::new(),
        socket_task: false,
        source_tasks: 0,
        task_types: Vec::new(),
    };
    for p in packets {
        if p.is_sc(sc::SCErrorMsgPacket) {
            out.errors.push(p.stream().read_i16()?);
        } else if p.is_sc(sc::SCItemSocketingLunagemResultPacket) {
            let mut s = p.stream();
            out.lunagem = Some((s.read_u8()?, s.read_u64()?, s.read_u32()?, s.read_bool()?));
        } else if p.is_sc(sc::SCItemTaskSuccessPacket) {
            let kind = *p.body.first().context("empty item task")?;
            out.task_types.push(kind);
            if kind == SOCKETING {
                out.socket_task |= contains(&p.body, &armor.to_le_bytes());
            } else if kind == SKILL_REAGENTS && contains(&p.body, &stack.to_le_bytes()) {
                out.source_tasks += 1;
            }
        }
    }
    Ok(out)
}

/// Casts the lunagem `stack` on `armor` and drains the answer.
async fn cast(
    game: &mut GameClient,
    r: &mut Runner<'_>,
    label: &str,
    stack: u64,
    gem: u32,
    armor: u64,
) -> anyhow::Result<Outcome> {
    let s = r.begin(&format!("game.socket.{label}"));
    game.start_item_skill_on_item(SKILL, stack, gem, armor)
        .await?;
    let packets = game
        .drain(Duration::from_secs(2), Duration::from_secs(12))
        .await?;
    let o = analyze(&packets, armor, stack)?;
    let result = packets
        .iter()
        .find(|p| p.is_sc(sc::SCSkillStartedPacket))
        .map(skill_response_result)
        .transpose()?;
    r.end(
        s,
        format!(
            "SkillResult {result:?}, errors {:?}, lunagem {:?}, Socketing task {}, gem tasks {}, \
             item task types {:?}",
            o.errors, o.lunagem, o.socket_task, o.source_tasks, o.task_types
        ),
    );
    Ok(o)
}

pub(super) async fn run(game: &mut GameClient, r: &mut Runner<'_>) -> anyhow::Result<()> {
    let s = r.begin("game.socket.gm");
    gm_access(game).await?;
    r.end(s, "GM commands accepted");

    let s = r.begin("game.socket.setup");
    let reply = gm_command(game, &format!("item add self {ARMOR} 1 {ARMOR_GRADE}")).await?;
    let (armor, armor_type, armor_slot) = stack_location(&reply, ARMOR).with_context(|| {
        format!(
            "no item {ARMOR} was added (GM replies: {:?})",
            reply.iter().filter_map(chat_text).collect::<Vec<_>>()
        )
    })?;
    let reply = gm_command(game, &format!("item add self {HANDS_GEM} 1")).await?;
    let (hands, _, _) = stack_location(&reply, HANDS_GEM).with_context(|| {
        format!(
            "no item {HANDS_GEM} was added (GM replies: {:?})",
            reply.iter().filter_map(chat_text).collect::<Vec<_>>()
        )
    })?;
    let reply = gm_command(game, &format!("item add self {CHEST_GEM} 1")).await?;
    let (chest, _, _) = stack_location(&reply, CHEST_GEM).with_context(|| {
        format!(
            "no item {CHEST_GEM} was added (GM replies: {:?})",
            reply.iter().filter_map(chat_text).collect::<Vec<_>>()
        )
    })?;
    ensure!(
        armor_type == 2,
        "the armor was not added to the bag (slot type {armor_type})"
    );
    r.end(
        s,
        format!(
            "armor {armor} (bag slot {armor_slot}, grade {ARMOR_GRADE}), hands gem {hands}, \
             chest gem {chest}"
        ),
    );

    // Wrong slot group: rejected, and the gem survives for a second attempt.
    for attempt in 1..=2 {
        let o = cast(
            game,
            r,
            &format!("wrong-group-{attempt}"),
            hands,
            HANDS_GEM,
            armor,
        )
        .await?;
        ensure!(
            o.errors.contains(&INVALID_TARGET),
            "wrong-group-{attempt}: expected SCErrorMsg {INVALID_TARGET}, got {:?} (task types {:?})",
            o.errors,
            o.task_types
        );
        ensure!(
            o.lunagem.is_none() && !o.socket_task,
            "wrong-group-{attempt}: a rejected socketing must not install a gem or send Socketing"
        );
    }

    // A chest gem fits the chest armor.
    let o = cast(game, r, "valid", chest, CHEST_GEM, armor).await?;
    let s = r.begin("game.socket.valid.assert");
    ensure!(
        o.errors.is_empty(),
        "valid: unexpected errors {:?} (task types {:?})",
        o.errors,
        o.task_types
    );
    let Some((result, item, template, install)) = o.lunagem else {
        bail!(
            "valid: no SCItemSocketingLunagemResult (task types {:?})",
            o.task_types
        );
    };
    ensure!(
        result == 1 && item == armor && template == CHEST_GEM && install,
        "valid: lunagem result ({result}, item {item}, template {template}, install {install}), \
         expected (1, {armor}, {CHEST_GEM}, true)"
    );
    ensure!(
        o.socket_task,
        "valid: no Socketing item task updating {armor} (task types {:?})",
        o.task_types
    );
    ensure!(
        o.source_tasks == 1,
        "valid: the gem stack {chest} had {} item tasks, expected one consumption (task types {:?})",
        o.source_tasks,
        o.task_types
    );
    r.end(
        s,
        format!(
            "lunagem result true for item {armor}, Socketing task, gem {CHEST_GEM} consumed once \
             (task types {:?})",
            o.task_types
        ),
    );
    Ok(())
}
