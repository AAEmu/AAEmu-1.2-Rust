//! Cinema handling: the sequence flags read from the client files, the permission request,
//! and which cinemas make `CSCompletedCinemaPacket` re-send the visible objects (only CUT_SCENE
//! sequences, or a cinema the server cannot identify).

use std::path::Path;

use aaemu_commons::PacketStream;

use super::cinema::{self, parse_sequence_flags, CinemaInfo};
use super::data::QuestData;
use super::engine::Ctx;
use super::events::{self, QuestEvent};
use super::model::kind;
use super::packets::*;
use super::state::CinemaMark;
use super::templates::{ActKind, QuestComponentTemplate, QuestTemplate};
use super::tests::{character, server_with, with_ctx};
use crate::data::client_files::ClientFileManager;
use crate::data::pak::test_util::{build_pak, temp_path};
use crate::data::pak::PakFileType;
use crate::models::doodad::Doodad;
use crate::models::npc::Npc;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::server::GameServer;
use crate::test_support::{spawn_player, TestClient};
use crate::world::{Entity, NpcRef, PlayerRef, WorldObject};

// ----- sequence flags and loading -----

#[test]
fn sequence_flags_are_read_from_the_root_tag_only() {
    // The roots of sl_nu_001 (not a cut scene), fly_west and zq_05_fortune (cut scenes).
    let flags = |xml: &str| parse_sequence_flags(xml);
    assert_eq!(
        flags(
            r#"<Sequence Name="sl_nu_001" MusicPatternName="" TimeOfDayName="" Flags="409616" StartTime="0" EndTime="67.400002" RecordOriginQuat="1,0,0,0" RecordOriginTrans="0,0,0">
 <Nodes>"#
        ),
        Some(409_616)
    );
    assert_eq!(409_616 & cinema::SEQUENCE_FLAG_CUT_SCENE, 0);
    for cut_scene in [409_720u32, 409_656, 131_128] {
        assert_ne!(cut_scene & cinema::SEQUENCE_FLAG_CUT_SCENE, 0);
        assert_eq!(
            flags(&format!(
                r#"<Sequence Name="x" Flags="{cut_scene}" StartTime="0">"#
            )),
            Some(cut_scene)
        );
    }
    // Declaration, line breaks and single quotes around the root tag.
    assert_eq!(
        flags("<?xml version=\"1.0\"?>\r\n<Sequence\r\n Name='a'\r\n Flags='8'>"),
        Some(8)
    );
    // The flags of a track below the root are not the sequence's.
    assert_eq!(
        flags(r#"<Sequence Name="a"><Track ParamId="1" Flags="8"/></Sequence>"#),
        None
    );
}

#[test]
fn malformed_sequence_flags_are_none() {
    for xml in [
        "",
        "not xml",
        "<Sequence>",
        "<Sequence Name=\"a\">",
        "<SequenceX Flags=\"8\">",
        "<Sequence Name=\"a\" XFlags=\"8\">",
        "<Sequence Name=\"a\" Flags=\"abc\">",
        "<Sequence Name=\"a\" Flags=\"\">",
        "<Sequence Name=\"a\" Flags=\"-8\">",
        "<Sequence Name=\"a\" Flags=\"99999999999\">",
        "<Sequence Name=\"a\" Flags=8>",
        "<Sequence Name=\"a\" Flags=\"8\"",
    ] {
        assert_eq!(parse_sequence_flags(xml), None, "{xml:?}");
    }
}

fn cinema_db() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE cinemas (id int PRIMARY KEY, name TEXT, replay NUM);
         INSERT INTO cinemas VALUES (80, 'sl_a', 't'), (16, 'fly_b', 'f'), (17, 'bad', 'f'),
                                    (18, 'nofile', 'f'), (19, 'bom', 't'), (20, 'late', 'f');",
    )
    .unwrap();
    conn
}

const SEQUENCES: [(&str, &str); 5] = [
    (
        "game/worlds/main_world/cinemas/sl_a.xml",
        "<Sequence Name=\"sl_a\" Flags=\"409616\" StartTime=\"0\">",
    ),
    (
        "game/worlds/instance_x/cinemas/fly_b.xml",
        "<Sequence Name=\"fly_b\" Flags=\"409720\" StartTime=\"0\">",
    ),
    (
        "game/worlds/main_world/cinemas/bad.xml",
        "<Sequence Name=\"bad\" Flags=\"x\">",
    ),
    (
        "game/worlds/main_world/cinemas/bom.xml",
        "\u{feff}<Sequence Name=\"bom\" Flags=\"8\">",
    ),
    // The same name in a later world does not override the first one.
    (
        "game/worlds/instance_x/cinemas/sl_a.xml",
        "<Sequence Name=\"sl_a\" Flags=\"8\">",
    ),
];

fn assert_loaded(files: &ClientFileManager) {
    let cinemas = cinema::load_cinemas(&cinema_db(), files).unwrap();
    let cut = |id: u32| cinemas[&id].cut_scene;
    assert_eq!(cut(80), Some(false));
    assert_eq!(cut(16), Some(true));
    assert_eq!(cut(17), None, "unparsable flags");
    assert_eq!(cut(18), None, "no file");
    assert_eq!(cut(19), Some(true), "UTF-8 BOM");
    assert_eq!(cut(20), None);
    assert_eq!(cinemas[&80].name, "sl_a");
    assert!(cinemas[&80].replay);
    assert!(!cinemas[&16].replay);
    assert_eq!(
        cinema::stats(&cinemas),
        cinema::CinemaLoadStats {
            cinemas: 6,
            with_file: 3,
            cut_scenes: 2
        }
    );
}

#[test]
fn cinemas_load_their_flags_from_a_pak() {
    let root = temp_path("cinema_pak");
    std::fs::create_dir_all(&root).unwrap();
    let mut entries: Vec<(&str, &[u8])> = SEQUENCES
        .iter()
        .map(|(path, xml)| (*path, xml.as_bytes()))
        .collect();
    entries.push(("game/worlds/instance_x/world.xml", b"<World/>"));
    let pak = root.join("game_pak");
    std::fs::write(&pak, build_pak(PakFileType::TypeA, &entries, 0)).unwrap();
    let mut files = ClientFileManager::new();
    assert!(files.add_source(pak.to_str().unwrap(), Path::new(".")));
    assert_loaded(&files);
    drop(files);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cinemas_load_their_flags_from_a_directory() {
    let root = temp_path("cinema_dir");
    for (path, xml) in SEQUENCES {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, xml).unwrap();
    }
    std::fs::write(root.join("game/worlds/instance_x/world.xml"), "<World/>").unwrap();
    let mut files = ClientFileManager::new();
    assert!(files.add_source(root.to_str().unwrap(), Path::new(".")));
    assert_loaded(&files);
    drop(files);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cinemas_without_client_files_stay_unknown() {
    let cinemas = cinema::load_cinemas(&cinema_db(), &ClientFileManager::new()).unwrap();
    assert_eq!(cinemas.len(), 6);
    assert!(cinemas.values().all(|c| c.cut_scene.is_none()));
}

/// The real 1.2 data: values checked against `cinema_flags.tsv` (a scan of every cinema xml of
/// the pak).
#[test]
#[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
fn real_cinemas_have_their_sequence_flags() {
    let (Ok(db), Ok(pak)) = (
        std::env::var("AAEMU_COMPACT_DB"),
        std::env::var("AAEMU_GAME_PAK"),
    ) else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut files = ClientFileManager::new();
    assert!(files.add_source(&pak, Path::new(".")));
    let cinemas = cinema::load_cinemas(&conn, &files).unwrap();
    let cut = |id: u32| cinemas[&id].cut_scene;
    assert_eq!(cut(80), Some(false), "sl_nu_001");
    assert_eq!(cut(16), Some(true), "fly_west");
    assert_eq!(cut(17), Some(true), "fly_east");
    assert_eq!(cut(130), Some(true), "zq_05_fortune");
    assert_eq!(cut(11), None, "in_house_01 has no sequence file");
    assert_eq!(
        cinema::stats(&cinemas),
        cinema::CinemaLoadStats {
            cinemas: 149,
            with_file: 142,
            cut_scenes: 28
        }
    );
}

// ----- the permission request packet -----

#[test]
fn permission_request_reads_component_npc_and_doodad() {
    // u32 component id, bc npc obj id (0x200), bc doodad obj id (0).
    let body = vec![0x89, 0x13, 0, 0, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
    let p =
        CSRequestPermissionToPlayCinemaForDirectingMode::read(&mut PacketStream::from_bytes(body))
            .unwrap();
    assert_eq!(
        p,
        CSRequestPermissionToPlayCinemaForDirectingMode {
            component_id: 5001,
            npc_obj_id: 0x200,
            doodad_obj_id: 0,
        }
    );
    // A short body is an error, not a panic.
    for len in 0..10 {
        let body = vec![0x89, 0x13, 0, 0, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00][..len].to_vec();
        assert!(
            CSRequestPermissionToPlayCinemaForDirectingMode::read(&mut PacketStream::from_bytes(
                body
            ))
            .is_err(),
            "{len} bytes"
        );
    }
}

// ----- flows -----

/// Quest 50: components with a plain cinema (80), a cut scene (16), a cinema without a
/// client file (99) and one whose cinema id is not in the table (77). Quest 8: an
/// `ObjCinema` (44) objective.
const COMP_PLAIN: u32 = 5001;
const COMP_CUT: u32 = 5002;
const COMP_NO_FILE: u32 = 5003;
const COMP_NOT_IN_TABLE: u32 = 5004;
const COMP_OBJECTIVE: u32 = 82;

fn quest(id: u32) -> QuestTemplate {
    QuestTemplate {
        id,
        detail_id: super::model::detail::NORMAL,
        ..QuestTemplate::default()
    }
}

fn component(id: u32, quest_id: u32, kind_id: u8, cinema_id: u32) -> QuestComponentTemplate {
    QuestComponentTemplate {
        id,
        quest_id,
        kind_id,
        npc_ai_id: 1,
        cinema_id,
        ..QuestComponentTemplate::default()
    }
}

fn info(id: u32, name: &str, cut_scene: Option<bool>) -> CinemaInfo {
    CinemaInfo {
        id,
        name: name.to_string(),
        replay: false,
        cut_scene,
    }
}

fn cinema_data(d: &mut QuestData) {
    d.add_template(quest(50));
    for (id, cinema_id) in [
        (COMP_PLAIN, 80),
        (COMP_CUT, 16),
        (COMP_NO_FILE, 99),
        (COMP_NOT_IN_TABLE, 77),
    ] {
        d.add_component(component(id, 50, kind::START, cinema_id));
    }
    for c in [
        info(80, "sl_plain", Some(false)),
        info(16, "fly_cut", Some(true)),
        info(99, "no_file", None),
        info(44, "objective", Some(false)),
    ] {
        d.cinemas.insert(c.id, c);
    }
    // Quest 8: Start (auto) -> Progress (watch cinema 44) -> Ready (report to NPC 1).
    d.add_template(quest(8));
    d.add_component(component(81, 8, kind::START, 0));
    d.add_act(81, 81, 81, 0, ActKind::ConAutoComplete);
    d.add_component(component(COMP_OBJECTIVE, 8, kind::PROGRESS, 0));
    d.add_act(
        COMP_OBJECTIVE,
        82,
        82,
        0,
        ActKind::ObjCinema {
            cinema_id: 44,
            use_alias: false,
            alias_id: 0,
        },
    );
    d.add_component(component(83, 8, kind::READY, 0));
    d.add_act(
        83,
        83,
        83,
        0,
        ActKind::ConReportNpc {
            npc_id: 1,
            use_alias: false,
            alias_id: 0,
        },
    );
    progress_quest(d, 70, 16, 600, 2);
    progress_quest(d, 71, 0, 601, 1);
}

/// Quest `qid`: Start (auto) -> Progress (`count` kills of NPC `npc`, component `qid * 10 + 2`
/// plays `cinema_id`, 0 = none) -> Ready (report to NPC 500).
fn progress_quest(d: &mut QuestData, qid: u32, cinema_id: u32, npc: u32, count: i32) {
    d.add_template(quest(qid));
    d.add_component(component(qid * 10 + 1, qid, kind::START, 0));
    d.add_act(
        qid * 10 + 1,
        qid * 10 + 1,
        qid * 10 + 1,
        0,
        ActKind::ConAutoComplete,
    );
    d.add_component(component(qid * 10 + 2, qid, kind::PROGRESS, cinema_id));
    d.add_act(
        qid * 10 + 2,
        qid * 10 + 2,
        qid * 10 + 2,
        count,
        ActKind::ObjMonsterHunt {
            npc_id: npc,
            use_alias: false,
            alias_id: 0,
            highlight_doodad_id: 0,
            highlight_doodad_phase: -1,
        },
    );
    d.add_component(component(qid * 10 + 3, qid, kind::READY, 0));
    d.add_act(
        qid * 10 + 3,
        qid * 10 + 3,
        qid * 10 + 3,
        0,
        ActKind::ConReportNpc {
            npc_id: 500,
            use_alias: false,
            alias_id: 0,
        },
    );
}

struct Scene {
    server: GameServer,
    client: TestClient,
    player: PlayerRef,
    npc: NpcRef,
}

/// A player with an NPC and a doodad next to it, nothing pending on the client.
fn scene() -> Scene {
    let (server, _) = server_with(cinema_data);
    let mut client = TestClient::new(&server, 1, 1001);
    let player = spawn_player(&server, &client, character(), 0x100);
    client.conn.active_char = Some(player.clone());
    let world = server.world.world(0).unwrap();
    let mut n = Npc::new(777);
    n.unit.transform.set_xyz(103.0, 100.0, 100.0);
    let npc = Entity::new(0x200, n);
    world.spawn(&npc, &server.view_ctx());
    let mut d = Doodad::new(778);
    d.transform.set_xyz(104.0, 100.0, 100.0);
    world.spawn(Entity::new(0x300, d), &server.view_ctx());
    client.drain();
    Scene {
        server,
        client,
        player,
        npc,
    }
}

impl Scene {
    async fn send<P: ClientPacket>(&mut self, packet: P) {
        packet
            .handle(&self.server, &mut self.client.conn)
            .await
            .unwrap();
    }

    async fn permission(&mut self, component_id: u32) {
        self.send(CSRequestPermissionToPlayCinemaForDirectingMode {
            component_id,
            npc_obj_id: self.npc.obj_id,
            doodad_obj_id: 0,
        })
        .await;
    }

    fn requested(&self) -> Option<u32> {
        self.player
            .data()
            .quest_state
            .requested_cinema
            .map(|m| m.cinema_id)
    }

    fn playing(&self) -> Option<u32> {
        self.player
            .data()
            .quest_state
            .playing_cinema
            .map(|m| m.cinema_id)
    }

    /// (NPC state re-sent, doodads re-sent) among the frames sent since the last call.
    fn resent(&mut self) -> (bool, bool) {
        let frames = self.client.drain();
        let npc = frames.iter().any(|(op, body)| {
            *op == sc::SCUnitStatePacket
                && PacketStream::from_bytes(body.clone()).read_bc().unwrap() == self.npc.obj_id
        });
        let doodads = frames
            .iter()
            .any(|(op, _)| *op == sc::SCDoodadsCreatedPacket);
        (npc, doodads)
    }
}

#[tokio::test]
async fn a_cinema_that_is_not_a_cut_scene_restores_nothing() {
    let mut s = scene();
    s.permission(COMP_PLAIN).await;
    assert_eq!(s.requested(), Some(80));
    s.send(CSStartedCinemaPacket).await;
    assert_eq!((s.requested(), s.playing()), (None, Some(80)));
    s.send(CSCompletedCinemaPacket).await;
    assert_eq!(s.resent(), (false, false));
    assert_eq!((s.requested(), s.playing()), (None, None));
}

#[tokio::test]
async fn a_cut_scene_restores_units_and_doodads() {
    let mut s = scene();
    s.permission(COMP_CUT).await;
    s.send(CSStartedCinemaPacket).await;
    assert_eq!(s.playing(), Some(16));
    s.send(CSCompletedCinemaPacket).await;
    assert_eq!(s.resent(), (true, true));
    assert_eq!((s.requested(), s.playing()), (None, None));
    // The next cinema is not the cut scene any more.
    s.permission(COMP_PLAIN).await;
    s.send(CSStartedCinemaPacket).await;
    s.send(CSCompletedCinemaPacket).await;
    assert_eq!(s.resent(), (false, false));
}

#[tokio::test]
async fn an_unannounced_cinema_restores_like_a_cut_scene() {
    let mut s = scene();
    s.send(CSStartedCinemaPacket).await;
    assert_eq!(s.playing(), Some(0));
    s.send(CSCompletedCinemaPacket).await;
    assert_eq!(s.resent(), (true, true));
    // A completion without a start is unknown as well.
    s.send(CSCompletedCinemaPacket).await;
    assert_eq!(s.resent(), (true, true));
}

#[tokio::test]
async fn a_cinema_without_a_sequence_file_or_row_restores_like_a_cut_scene() {
    for component_id in [COMP_NO_FILE, COMP_NOT_IN_TABLE] {
        let mut s = scene();
        s.permission(component_id).await;
        s.send(CSStartedCinemaPacket).await;
        s.send(CSCompletedCinemaPacket).await;
        assert_eq!(s.resent(), (true, true), "component {component_id}");
    }
}

#[tokio::test]
async fn a_request_for_an_unknown_component_forgets_the_earlier_one() {
    let mut s = scene();
    s.permission(COMP_PLAIN).await;
    s.permission(9999).await;
    assert_eq!(s.requested(), None);
    s.send(CSStartedCinemaPacket).await;
    s.send(CSCompletedCinemaPacket).await;
    assert_eq!(s.resent(), (true, true));
}

#[tokio::test]
async fn an_old_request_does_not_name_a_later_cinema() {
    let mut s = scene();
    let now = chrono::Utc::now().timestamp_millis();
    s.player.data().quest_state.requested_cinema = Some(CinemaMark {
        cinema_id: 80,
        at_ms: now - cinema::REQUEST_TTL_MS - 1_000,
    });
    s.send(CSStartedCinemaPacket).await;
    assert_eq!(s.playing(), Some(0));
    s.send(CSCompletedCinemaPacket).await;
    assert_eq!(s.resent(), (true, true));
}

#[tokio::test]
async fn the_permission_request_resolves_the_cinema_of_an_objective_act() {
    let mut s = scene();
    let data = &s.server.game_data.systems.quests;
    assert_eq!(data.component_cinema_id(COMP_OBJECTIVE), Some(44));
    assert_eq!(data.component_cinema_id(COMP_PLAIN), Some(80));
    assert_eq!(data.component_cinema_id(81), None);
    assert_eq!(data.component_cinema_id(9999), None);
    s.permission(COMP_OBJECTIVE).await;
    assert_eq!(s.requested(), Some(44));
}

fn objective(s: &Scene) -> (u8, i32) {
    let c = s.player.data();
    (c.quests.active[0].step, c.quests.active[0].objectives[0])
}

fn start_objective_quest(s: &Scene) {
    events::add_quest(&s.server, &s.player, 8, false);
    s.server.systems.quests.run_queued_evaluations(&s.server);
    assert_eq!(objective(s), (kind::PROGRESS, 0));
    assert_eq!(s.player.data().quest_state.currently_playing_cinema_id, 0);
}

#[tokio::test]
async fn an_announced_cinema_of_another_component_does_not_complete_the_objective() {
    let mut s = scene();
    start_objective_quest(&s);
    s.permission(COMP_PLAIN).await;
    s.send(CSStartedCinemaPacket).await;
    assert_eq!(s.player.data().quest_state.currently_playing_cinema_id, 0);
    s.send(CSCompletedCinemaPacket).await;
    s.server.systems.quests.run_queued_evaluations(&s.server);
    assert_eq!(objective(&s), (kind::PROGRESS, 0));

    // The objective's own cinema, announced or not, completes it.
    s.permission(COMP_OBJECTIVE).await;
    s.send(CSStartedCinemaPacket).await;
    assert_eq!(s.player.data().quest_state.currently_playing_cinema_id, 44);
    s.send(CSCompletedCinemaPacket).await;
    s.server.systems.quests.run_queued_evaluations(&s.server);
    assert_eq!(objective(&s), (kind::READY, 1));
    // Cinema 44 is not a cut scene, so nothing was re-sent.
    assert_eq!(s.resent(), (false, false));
}

#[tokio::test]
async fn an_unannounced_cinema_completes_the_objective() {
    let mut s = scene();
    start_objective_quest(&s);
    s.send(CSStartedCinemaPacket).await;
    assert_eq!(s.player.data().quest_state.currently_playing_cinema_id, 44);
    s.send(CSCompletedCinemaPacket).await;
    s.server.systems.quests.run_queued_evaluations(&s.server);
    assert_eq!(objective(&s), (kind::READY, 1));
    // The claimed cinema is the one that ended: 44 is known and not a cut scene.
    assert_eq!(s.resent(), (false, false));
}

#[tokio::test]
async fn the_cinema_effect_names_the_cinema_for_a_character_target() {
    use crate::systems::quests::bridges::QuestsForSkills;
    use crate::systems::skills::data::{EffectKind, EffectTemplate};
    use crate::systems::skills::effects::{EffectArgs, EffectSource};
    use crate::systems::skills::ext::QuestSkillHooks;
    use crate::systems::skills::geometry::Target;
    use crate::systems::skills::types::{CastAction, SkillCastTarget, SkillCaster, SkillObject};

    let mut s = scene();
    let template = EffectTemplate::Cinema {
        id: 1,
        cinema_id: 16,
    };
    let apply = |s: &Scene, target: WorldObject| {
        let caster = WorldObject::from(s.player.clone());
        QuestsForSkills.apply_effect(
            &s.server,
            EffectKind::CinemaEffect,
            &template,
            &EffectArgs {
                caster: Some(caster.clone()),
                caster_caster: SkillCaster::Unit(s.player.obj_id),
                target: Target::Obj(target),
                target_caster: SkillCastTarget::Unit(s.player.obj_id),
                cast: CastAction::Skill {
                    skill_id: 1,
                    tl_id: 0,
                },
                source: EffectSource::default(),
                skill_object: &SkillObject::None,
            },
        )
    };
    // An NPC target is not a viewer.
    assert!(apply(&s, WorldObject::from(&s.npc)));
    assert_eq!(s.requested(), None);
    assert!(apply(&s, WorldObject::from(s.player.clone())));
    assert_eq!(s.requested(), Some(16));
    s.send(CSStartedCinemaPacket).await;
    s.send(CSCompletedCinemaPacket).await;
    assert_eq!(s.resent(), (true, true));
}

// ----- the component that completes a step is named in the update -----

/// The component ids of the `SCQuestContextUpdatedPacket`s in `frames`: the body ends with the
/// component id and the four `para` values.
fn update_components(frames: &[(u16, Vec<u8>)]) -> Vec<u32> {
    frames
        .iter()
        .filter(|(op, _)| *op == sc::SCQuestContextUpdatedPacket)
        .map(|(_, body)| {
            let at = body.len() - 20;
            u32::from_le_bytes(body[at..at + 4].try_into().unwrap())
        })
        .collect()
}

/// Runs the queued evaluations of a data-level context.
fn drain(ctx: &mut Ctx<'_>) {
    while !ctx.st.local_queue.is_empty() {
        let h = ctx.st.local_queue.remove(0);
        if ctx.st.quest(h).is_none() {
            continue;
        }
        ctx.starting_evaluation(h);
        ctx.run_current_step(h);
    }
}

fn kill(ctx: &mut Ctx<'_>, npc_id: u32) {
    ctx.dispatch(&QuestEvent::MonsterHunt { npc_id, count: 1 })
        .unwrap();
    drain(ctx);
}

#[tokio::test]
async fn the_completing_component_is_named_once_in_the_update() {
    let (server, _) = server_with(cinema_data);
    let mut c = character();
    with_ctx(&server, &mut c, Default::default(), |ctx| {
        assert!(ctx.add_quest(70, false, 0, 0));
        drain(ctx);
        let step = |ctx: &Ctx<'_>| ctx.st.active_quest(70).unwrap().step;
        assert_eq!(step(ctx), kind::PROGRESS);
        // Entering the steps: the completed Start step is not named again (its
        // `SCQuestContextStartedPacket` named it), the first evaluation of the Progress step
        // (objective open) names none.
        assert_eq!(
            update_components(&std::mem::take(&mut ctx.sent)),
            vec![0, 0]
        );

        // One of two kills: the objective is not complete, the update is as before.
        kill(ctx, 600);
        assert_eq!(step(ctx), kind::PROGRESS);
        let partial = update_components(&std::mem::take(&mut ctx.sent));
        assert!(!partial.is_empty());
        assert!(partial.iter().all(|c| *c == 0), "{partial:?}");

        // The second kill completes the step: this update names the component, the update of
        // the Ready step evaluation that follows does not repeat it.
        kill(ctx, 600);
        assert_eq!(step(ctx), kind::READY);
        let done = update_components(&std::mem::take(&mut ctx.sent));
        assert_eq!(done, vec![702, 0]);
    });
}

#[tokio::test]
async fn the_named_component_sets_the_requested_cinema() {
    let (server, _) = server_with(cinema_data);
    let mut c = character();
    with_ctx(&server, &mut c, Default::default(), |ctx| {
        assert!(ctx.add_quest(70, false, 0, 0));
        drain(ctx);
        kill(ctx, 600);
        assert_eq!(
            ctx.st.requested_cinema, None,
            "not before the step completes"
        );
        kill(ctx, 600);
        assert_eq!(
            ctx.st.requested_cinema,
            Some(CinemaMark {
                cinema_id: 16,
                at_ms: ctx.now_ms
            })
        );
    });
}

#[tokio::test]
async fn a_component_without_a_cinema_leaves_the_requested_cinema_alone() {
    let (server, _) = server_with(cinema_data);
    for pending in [
        None,
        // The quest dialog of a cinema was opened and is still waiting for its accept.
        Some(CinemaMark {
            cinema_id: 80,
            at_ms: 5,
        }),
    ] {
        let mut c = character();
        c.quest_state.requested_cinema = pending;
        with_ctx(&server, &mut c, Default::default(), |ctx| {
            assert!(ctx.add_quest(71, false, 0, 0));
            drain(ctx);
            std::mem::take(&mut ctx.sent);
            kill(ctx, 601);
            let updates = update_components(&std::mem::take(&mut ctx.sent));
            assert_eq!(updates, vec![712, 0]);
            assert_eq!(ctx.st.requested_cinema, pending);
        });
    }
}

#[tokio::test]
async fn a_progress_cinema_is_announced_to_the_client_and_restores_a_cut_scene() {
    let mut s = scene();
    events::add_quest(&s.server, &s.player, 70, false);
    s.server.systems.quests.run_queued_evaluations(&s.server);
    s.client.drain();
    for _ in 0..2 {
        events::with_quests(&s.server, &s.player, None, |ctx| {
            let _ = ctx.dispatch(&QuestEvent::MonsterHunt {
                npc_id: 600,
                count: 1,
            });
        });
        s.server.systems.quests.run_queued_evaluations(&s.server);
    }
    let frames = s.client.drain();
    assert_eq!(update_components(&frames), vec![0, 702, 0]);
    assert_eq!(s.requested(), Some(16));
    // The client plays it: the cinema is known (a cut scene), so the neighbours come back.
    s.send(CSStartedCinemaPacket).await;
    assert_eq!(s.playing(), Some(16));
    s.send(CSCompletedCinemaPacket).await;
    assert_eq!(s.resent(), (true, true));
}
