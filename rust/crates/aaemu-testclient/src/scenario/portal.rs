//! `portal`: the portal book and the Memory Tome the way the 1.2 client uses them.
//!
//! 1. `/add_portal` records a book entry (`SCCharacterPortals`), `CSDeletePortal(2, id)` must be
//!    answered with `SCPortalDeleted(2, id)`.
//! 2. Next to the Memory Tome of the Harpa Training Ground (doodad 841, phase 282,
//!    `DoodadFuncBinding` district 15, skill 14602 "Setting recall point", 3 s cast, range 4 m)
//!    the bot casts 14602 with the doodad as the target (`CSStartSkill` caster Unit, target
//!    Doodad). The server must bind the recall point: `SCCharacterReturnDistricts` whose
//!    trailing `returnDistrictId` is return point 1 (district 15 for the Nuian factions), and
//!    the tome enters its timer phase (`SCDoodadPhaseChanged`).
//!
//! 3. Back at the spawn spot's twin: with a Teleport Book (4045) and Hereafter Stones (4052)
//!    from `/item add`, a gate to a recorded spot is opened (skill 11216, `PortalInfo` skill
//!    object) once with a point 20 m away (the server must refuse it: no gate, no stone used)
//!    and once 2.5 m ahead; going through it (`CSUsePortal`) must bring `SCUnitPortalUsed` with
//!    the gate NPC's objId before the `SCTeleportUnit`.
//!
//! GM account (`/add_portal`, `/move`, `/item add`), like `doodad`.

use std::time::{Duration, Instant};

use aaemu_commons::PacketStream;
use aaemu_game::opcodes::{cs, sc};
use anyhow::{bail, ensure, Context};

use super::questitem::{chat_text, gm_command, stack_id};
use super::{skill_response_result, BotConfig, Runner, QUIET};
use crate::game::{GameClient, Packet};

/// Doodad template 841 "Harpa Training Ground Memory Tome".
const TOME: u32 = 841;
/// Skill 14602, the tome's `DoodadFuncBinding` skill.
const SKILL: u32 = 14602;
/// The tome (spawn at 10346.98, 16037.06, 357.88); the bot stands about 2.5 m away.
const POS: [f32; 3] = [10345.0, 16035.5, 358.0];
/// Return point of district 15 for factions 101 / 103 / 104.
const RETURN_POINT: u32 = 1;
/// How long the cast (3 s) and its aftermath are observed.
const CAST_WINDOW: Duration = Duration::from_secs(8);
/// Teleport Book (use skill 11216) and Hereafter Stone (inland reagent).
const BOOK: u32 = 4045;
const STONE: u32 = 4052;
/// Skill 11216 "Opening Worldgate" (10 s cast).
const OPEN_SKILL: u32 = 11216;
/// The entrance gate NPC.
const GATE_NPC: u32 = 3891;

pub(super) async fn run(
    game: &mut GameClient,
    _cfg: &BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    // ---- GM access ----
    let s = r.begin("game.portal.gm");
    let reply = gm_command(game, "revive").await?;
    let texts: Vec<String> = reply.iter().filter_map(chat_text).collect();
    ensure!(
        !texts.iter().any(|t| t.contains("Insufficient privileges")),
        "GM commands are refused: account {} needs access level 100",
        game.account_id
    );
    r.end(s, "GM commands accepted");

    // ---- book: record with /add_portal, delete with CSDeletePortal ----
    let s = r.begin("game.portal.delete");
    game.chat(0, "/add_portal TcSpot").await?;
    let added = game
        .wait_for("SCCharacterPortalsPacket", |p| {
            p.is_sc(sc::SCCharacterPortalsPacket)
        })
        .await?;
    let ids = portal_ids(&added).context("SCCharacterPortals")?;
    let id = *ids.last().context("SCCharacterPortals without an entry")?;
    let mut body = PacketStream::new();
    body.write_u8(2).write_u32(id);
    game.send_cs(cs::CSDeletePortalPacket, body.as_bytes())
        .await?;
    let deleted = game.wait_sc(sc::SCPortalDeletedPacket).await?;
    let mut expected = vec![2u8];
    expected.extend_from_slice(&id.to_le_bytes());
    ensure!(
        deleted.body == expected,
        "SCPortalDeleted {:02x?}, expected {expected:02x?}",
        deleted.body
    );
    r.end(s, format!("entry {id} deleted, SCPortalDeleted(2, {id})"));

    // A spot to open gates to later: here, at the spawn.
    game.chat(0, "/add_portal TcHome").await?;
    let added = game
        .wait_for("SCCharacterPortalsPacket", |p| {
            p.is_sc(sc::SCCharacterPortalsPacket)
        })
        .await?;
    let home = *portal_ids(&added)?
        .last()
        .context("SCCharacterPortals without an entry")?;

    // ---- Memory Tome ----
    let s = r.begin("game.portal.tome");
    let [x, y, z] = POS;
    game.chat(0, &format!("/move {x} {y} {z}")).await?;
    game.wait_for("SCTeleportUnitPacket", |p| {
        p.is_sc(sc::SCTeleportUnitPacket)
    })
    .await?;
    let at = game.state.self_pos;
    game.teleport_ended(at).await?;
    game.move_to(at, 5000).await?;
    game.drain(QUIET, Duration::from_secs(8)).await?;
    let mut waited = 0;
    while !game.state.doodads.values().any(|d| d.template_id == TOME) && waited < 10 {
        game.drain(Duration::from_millis(500), Duration::from_secs(1))
            .await?;
        waited += 1;
    }
    let tome = game
        .state
        .doodads
        .values()
        .filter(|d| d.template_id == TOME)
        .min_by(|a, b| dist(at, a.pos).total_cmp(&dist(at, b.pos)))
        .cloned()
        .with_context(|| format!("Memory Tome {TOME} not visible at {at:?}"))?;
    println!(
        "[portal] tome obj {} group {} at {:?}, {:.2} m away",
        tome.obj_id,
        tome.func_group_id,
        tome.pos,
        dist(at, tome.pos)
    );
    game.change_target(tome.obj_id).await?;
    game.move_to_facing(at, tome.pos, 6000).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(1))
        .await?;
    game.start_doodad_skill(SKILL, tome.obj_id).await?;
    let started = Instant::now();
    let mut bound = None;
    let mut phase = None;
    let mut refused = None;
    while started.elapsed() < CAST_WINDOW {
        let Some(p) = game.recv_within(Duration::from_millis(250)).await? else {
            continue;
        };
        if p.is_sc(sc::SCSkillStartedPacket)
            && super::own_skill_response(&p, SKILL, game.state.self_obj)
        {
            if let Ok(code) = skill_response_result(&p) {
                if code != 0 {
                    refused = Some(code);
                }
            }
        }
        if p.is_sc(sc::SCCharacterReturnDistrictsPacket) {
            bound = Some(return_districts(&p).context("SCCharacterReturnDistricts")?);
        }
        if p.is_sc(sc::SCDoodadPhaseChangedPacket) {
            let mut st = p.stream();
            if st.read_bc().ok() == Some(tome.obj_id) {
                phase = st.read_u32().ok();
            }
        }
    }
    if let Some(code) = refused {
        bail!("skill {SKILL} on the tome was refused: SkillResult {code}");
    }
    let (count, bound_id) = bound.context("no SCCharacterReturnDistricts after the cast")?;
    ensure!(
        bound_id == RETURN_POINT,
        "bound return point {bound_id}, expected {RETURN_POINT}"
    );
    r.end(
        s,
        format!(
            "bound to return point {bound_id} ({count} districts in the book); tome phase {} -> {:?}",
            tome.func_group_id, phase
        ),
    );

    // ---- gate ----
    let s = r.begin("game.portal.gate");
    let reply = gm_command(game, &format!("item add self {BOOK} 1")).await?;
    let book = stack_id(&reply, BOOK).context("no Teleport Book from /item add")?;
    let reply = gm_command(game, &format!("item add self {STONE} 3")).await?;
    stack_id(&reply, STONE).context("no Hereafter Stone from /item add")?;
    let me = game.state.self_pos;
    // Too far (20 m): the effect must refuse the point.
    let far = (me.0 + 20.0, me.1 - 5.0, me.2);
    let gates_before = gate_ids(game);
    game.send_cs(
        cs::CSStartSkillPacket,
        &open_portal(game.state.self_obj, book, home, far),
    )
    .await?;
    // 10 s cast + the 10 s cooldown (it starts when the skill fires).
    game.drain(Duration::from_secs(22), Duration::from_secs(22))
        .await?;
    let far_gate = gate_ids(game)
        .into_iter()
        .any(|g| !gates_before.contains(&g));
    ensure!(!far_gate, "a gate opened 20 m away from the caster");
    // 2.5 m ahead: the gate opens.
    let near = (me.0 + 2.5, me.1, me.2);
    game.send_cs(
        cs::CSStartSkillPacket,
        &open_portal(game.state.self_obj, book, home, near),
    )
    .await?;
    let started = Instant::now();
    let mut gate = None;
    while started.elapsed() < Duration::from_secs(16) && gate.is_none() {
        game.recv_within(Duration::from_millis(250)).await?;
        gate = gate_ids(game)
            .into_iter()
            .find(|g| !gates_before.contains(g));
    }
    let gate = gate.context("no gate NPC 3891 appeared after the 2.5 m cast")?;
    let mut body = PacketStream::new();
    body.write_bc(gate).write_bool(false);
    game.send_cs(cs::CSUsePortalPacket, body.as_bytes()).await?;
    let mut used = None;
    let mut teleported = false;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) && !teleported {
        let Some(p) = game.recv_within(Duration::from_millis(250)).await? else {
            continue;
        };
        if p.is_sc(sc::SCUnitPortalUsedPacket) {
            used = p.stream().read_bc().ok();
        }
        if p.is_sc(sc::SCTeleportUnitPacket) {
            teleported = true;
        }
    }
    ensure!(teleported, "no SCTeleportUnit after CSUsePortal");
    ensure!(
        used == Some(gate),
        "SCUnitPortalUsed carried {used:?}, expected the gate {gate}"
    );
    let at = game.state.self_pos;
    game.teleport_ended(at).await?;
    game.drain(QUIET, Duration::from_secs(3)).await?;
    r.end(
        s,
        format!("far point refused; gate {gate} opened, used: SCUnitPortalUsed({gate}), teleported to {at:?}"),
    );

    // ---- leave ----
    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    Ok(())
}

/// Entrance gates (NPC 3891) the bot can see.
fn gate_ids(game: &GameClient) -> Vec<u32> {
    game.state
        .units
        .values()
        .filter(|u| u.unit_type == 1 && u.id == GATE_NPC)
        .map(|u| u.obj_id)
        .collect()
}

/// `CSStartSkill(11216)` cast from the Teleport Book at ourselves with the `PortalInfo` skill
/// object (`u8 kind, i32 id, long x, long y, f32 z`) of a private book entry, as the client does.
fn open_portal(self_obj: u32, book: u64, portal_id: u32, at: (f32, f32, f32)) -> Vec<u8> {
    use aaemu_commons::helpers::convert_coord_to_long;
    let mut s = PacketStream::new();
    s.write_u32(OPEN_SKILL)
        .write_u8(2) // SkillCasterType.Item
        .write_bc(self_obj)
        .write_u64(book)
        .write_u32(BOOK)
        .write_u8(0)
        .write_u32(0)
        .write_u8(0) // SkillCastUnitTarget
        .write_bc(self_obj)
        .write_u8(1) // SkillObjectPortalInfo
        .write_u8(2) // private entry
        .write_i32(portal_id as i32)
        .write_i64(convert_coord_to_long(at.0))
        .write_i64(convert_coord_to_long(at.1))
        .write_f32(at.2);
    s.into_bytes()
}

fn dist(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let (dx, dy, dz) = (a.0 - b.0, a.1 - b.1, a.2 - b.2);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Skips one `PortalWire` (`u32 id, string name, u32 zone, f32 x, y, z, zRot`), returns its id.
fn read_portal(s: &mut PacketStream) -> anyhow::Result<u32> {
    let id = s.read_u32()?;
    s.read_string()?;
    s.read_u32()?;
    for _ in 0..4 {
        s.read_f32()?;
    }
    Ok(id)
}

/// The entry ids of an `SCCharacterPortals` (`i32 count`, `PortalWire[count]`).
fn portal_ids(p: &Packet) -> anyhow::Result<Vec<u32>> {
    let mut s = p.stream();
    let count = s.read_i32()?;
    (0..count).map(|_| read_portal(&mut s)).collect()
}

/// `SCCharacterReturnDistricts`: (`count`, the trailing bound `returnDistrictId`).
fn return_districts(p: &Packet) -> anyhow::Result<(i32, u32)> {
    let mut s = p.stream();
    let count = s.read_i32()?;
    for _ in 0..count {
        read_portal(&mut s)?;
    }
    Ok((count, s.read_u32()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::LEVEL_GAME;
    use aaemu_commons::Marshal;
    use aaemu_game::packets::world::{
        PortalWire, SCCharacterPortalsPacket, SCCharacterReturnDistrictsPacket,
    };

    fn wire(id: u32) -> PortalWire {
        PortalWire {
            id,
            name: "Spot".into(),
            zone_id: 129,
            off_x: 1.0,
            off_y: 2.0,
            z: 3.0,
            z_rot: 4.0,
        }
    }

    fn packet(opcode: u16, p: &dyn Marshal) -> Packet {
        let mut body = PacketStream::new();
        p.write(&mut body);
        Packet {
            level: LEVEL_GAME,
            opcode,
            body: body.into_bytes(),
        }
    }

    #[test]
    fn reads_portal_lists() {
        let p = packet(
            sc::SCCharacterPortalsPacket,
            &SCCharacterPortalsPacket {
                portals: vec![wire(7), wire(4097)],
            },
        );
        assert_eq!(portal_ids(&p).unwrap(), vec![7, 4097]);
        let p = packet(
            sc::SCCharacterReturnDistrictsPacket,
            &SCCharacterReturnDistrictsPacket {
                portals: vec![wire(5)],
                return_district_id: 1,
            },
        );
        assert_eq!(return_districts(&p).unwrap(), (1, 1));
    }
}
