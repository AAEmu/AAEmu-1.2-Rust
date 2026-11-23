use super::*;
use crate::record::Event;
use aaemu_game::net::packet::encode;
use aaemu_game::packets::g2c::{SCReconnectAuthPacket, X2EnterWorldResponsePacket};
use aaemu_game::systems::skills::packets::*;
use aaemu_game::systems::skills::types::{
    CastAction, HealHitType, HealType, PlotObject, SkillCaster,
};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Bytes(Arc<Mutex<Vec<u8>>>);
impl Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn safe_native_batch() -> CompressedGamePackets {
    let mut batch = CompressedGamePackets::new();
    batch.add(&SCSkillCooldownResetPacket {
        obj_id: 42,
        skill_id: 15307,
        tag_id: 0,
        gcd: false,
    });
    batch.add(&SCPlotEventPacket {
        tl: 7,
        event_id: 8,
        skill_id: 15307,
        caster: PlotObject::Unit(42),
        target: PlotObject::Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
            roll: 0,
            pitch: 0,
            yaw: 0,
        },
        obj_id: 43,
        casting_time: 100,
        flag: 8,
        item_id: 100,
        target_unit_count: 2,
    });
    batch.add(&SCUnitDamagedPacket::new(
        CastAction::Plot {
            plot_id: 1,
            tl_id: 7,
            event_id: 8,
            skill_id: 15307,
        },
        SkillCaster::Item {
            obj_id: 42,
            item_id: 100,
            item_template_id: 23398,
            type1: 0,
            type2: 0,
        },
        42,
        43,
        20000,
        1000,
    ));
    batch.add(&SCUnitHealedPacket {
        cast_action: CastAction::Buff {
            buff_id: 1,
            owner_obj_id: 42,
            index: 2,
        },
        skill_caster: SkillCaster::Mount {
            obj_id: 42,
            mount_skill_template_id: 3,
        },
        target_id: 43,
        heal_type: HealType::Health,
        heal_hit_type: HealHitType::HealHit,
        value: 400,
    });
    batch.add(&SCBuffCreatedPacket {
        skill_caster: SkillCaster::Unit(42),
        caster_id: 1,
        owner_obj_id: 42,
        index: 2,
        buff_id: 3,
        caster_level: 10,
        ab_level: 10,
        skill_id: 15307,
        charge: 300,
        duration: 20000,
        tick: 100.0,
    });
    batch.add(&SCSkillEndedPacket { tl: 7 });
    batch.add(&SCBuffRemovedPacket {
        obj_id: 42,
        index: 2,
    });
    batch.add(&aaemu_game::packets::world::SCErrorMsgPacket::new(
        7, 0, false,
    ));
    let mut started = SCSkillStartedPacket::new(
        15307,
        7,
        SkillCaster::Unit(42),
        aaemu_game::systems::skills::types::SkillCastTarget::Position {
            obj_id: 0,
            x: 1.0,
            y: 2.0,
            z: 3.0,
            rot: 0.0,
            obj_id1: 0,
            obj_id2: 0,
        },
        aaemu_game::systems::skills::types::SkillObject::Text("safe".into()),
    );
    started
        .set_skill_result(2)
        .set_result_ushort(300)
        .set_result_uint(400);
    batch.add(&started);
    batch.add(&SCSkillFiredPacket {
        id: 15307,
        tl: 7,
        caster: SkillCaster::Unit(42),
        target: aaemu_game::systems::skills::types::SkillCastTarget::Doodad(43),
        skill_object: aaemu_game::systems::skills::types::SkillObject::None,
        computed_delay: 100,
        channeling_time: 0,
        fire_anim_id: 1,
    });
    batch
}

fn client(rec: Recorder) -> GameClient {
    let (io, _peer) = tokio::io::duplex(64 * 1024);
    GameClient::from_io(io, rec)
}

fn control_events(frame: Vec<u8>) -> (Vec<Event>, Vec<Event>) {
    let sink = Bytes::default();
    let rec = Recorder::incremental_game_to(sink.clone());
    rec.control_game_ready(true);
    let mut game = client(rec.clone());
    game.decode(frame).unwrap();
    while let Some(packet) = game.expanded.pop_front() {
        game.accept(packet);
    }
    let output = rec.take_live_events().unwrap();
    let capture = String::from_utf8(sink.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (output, capture)
}

#[tokio::test]
async fn real_repo_encoded_plot_combat_and_cooldown_batches_reach_both_sinks() {
    let batch = safe_native_batch();
    let (events, capture) = control_events(encode(&batch));
    assert_eq!(events, capture);
    assert_eq!(events.len(), batch.len());
    for (event, (opcode, body)) in events.iter().zip(&batch.packets) {
        assert_eq!(event.level, LEVEL_GAME);
        assert_eq!(event.opcode, *opcode);
        assert_eq!(event.body_bytes(), *body);
    }
    let mut large = CompressedGamePackets::new();
    for skill in 0..1024 {
        large.add(&SCSkillCooldownResetPacket {
            obj_id: 42,
            skill_id: skill,
            tag_id: 0,
            gcd: false,
        });
    }
    assert_eq!(control_events(encode(&large)).0.len(), 1024);
    assert!(control_events(encode(&CompressedGamePackets::new()))
        .0
        .is_empty());
}

#[tokio::test]
async fn mixed_auth_batches_only_publish_the_safe_inner_packets() {
    const TOKEN: u32 = u32::from_le_bytes(*b"KEY!");
    let safe = safe_native_batch();
    let mut mixed = CompressedGamePackets::new();
    mixed.add(&SCReconnectAuthPacket { token: TOKEN });
    mixed.packets.extend(safe.packets.clone());
    mixed.add(&X2EnterWorldResponsePacket {
        reason: 0,
        gm: true,
        token: TOKEN,
        port: 25250,
        world_time: 0,
    });
    let (events, capture) = control_events(encode(&mixed));
    assert_eq!(events, capture);
    assert_eq!(events.len(), safe.len());
    for event in &events {
        assert_eq!(event.level, 1);
        assert!(!matches!(
            event.opcode,
            sc::SCReconnectAuthPacket | sc::X2EnterWorldResponsePacket
        ));
        assert!(!event.body.contains("4b455921"));
    }
    // A recorder receiving an already-inflated aggregate uses the identical splitter.
    let sink = Bytes::default();
    let rec = Recorder::incremental_game_to(sink.clone());
    rec.control_game_ready(true);
    rec.push(
        Channel::Game,
        Dir::S2C,
        4,
        mixed.len() as u16,
        "CompressedGamePackets",
        &mixed.raw_payload(),
    );
    let direct = rec.take_live_events().unwrap();
    assert_eq!(
        direct
            .iter()
            .map(|e| (e.opcode, e.body.clone()))
            .collect::<Vec<_>>(),
        events
            .iter()
            .map(|e| (e.opcode, e.body.clone()))
            .collect::<Vec<_>>()
    );
    assert!(!String::from_utf8_lossy(&sink.0.lock().unwrap()).contains("4b455921"));
    // C2S's credential transition is X2EnterWorld (native 29-byte body).
    let mut auth = PacketStream::new();
    auth.write_u32(0)
        .write_u32(0)
        .write_u32(1)
        .write_u32(TOKEN)
        .write_i32(0)
        .write_u8(0)
        .write_u64(0);
    let mut inbound = CompressedGamePackets::new();
    inbound
        .packets
        .push((cs::X2EnterWorldPacket, auth.into_bytes()));
    let sink = Bytes::default();
    let rec = Recorder::incremental_game_to(sink.clone());
    rec.control_game_ready(true);
    rec.push(
        Channel::Game,
        Dir::C2S,
        4,
        1,
        "CompressedGamePackets",
        &inbound.raw_payload(),
    );
    assert!(rec.take_live_events().unwrap().is_empty());
    assert!(sink.0.lock().unwrap().is_empty());
    // Ordinary private scenario captures preserve their existing inflated aggregate.
    let ordinary = Recorder::new(true);
    let mut game = client(ordinary.clone());
    let packet = game.decode(encode(&mixed)).unwrap().unwrap();
    assert_eq!(packet.level, 4);
    assert_eq!(ordinary.events()[0].body_bytes(), mixed.raw_payload());
    assert_eq!(ordinary.events().len(), 1);
}

fn redacted(error: &anyhow::Error) {
    for text in [
        format!("{error}"),
        format!("{error:#}"),
        format!("{error:?}"),
    ] {
        for secret in ["KEY!", "4b455921", "75, 69, 89, 33"] {
            assert!(!text.contains(secret), "payload in formatted parser error");
        }
    }
}

#[tokio::test]
async fn malformed_compressed_auth_never_reaches_errors_or_debug_logs() {
    let mut raw = PacketStream::new();
    raw.write_u16(0)
        .write_u16(sc::SCReconnectAuthPacket)
        .write_raw(b"KEY!");
    let mut bad = deflate_stored(raw.as_bytes());
    bad[0] = 0; // valid non-final stored block produces the secret before the invalid block
    bad.push(7); // final reserved BTYPE=3
    let mut frame = vec![0xdd, 4, 1, 0];
    frame.extend_from_slice(&bad);
    let logs = Bytes::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let sink = Bytes::default();
    let rec = Recorder::incremental_game_to(sink.clone());
    rec.control_game_ready(true);
    let mut game = client(rec.clone());
    let error = game.decode(frame).unwrap_err();
    assert_eq!(
        error.downcast_ref::<BatchError>(),
        Some(&BatchError::Inflate)
    );
    redacted(&error);
    tracing::debug!(debug=?error,display=%error,"sanitized batch error");
    // The observation path also avoids formatting errors with retained native input.
    game.decode(aaemu_game::net::packet::encode_raw(
        1,
        sc::X2EnterWorldResponsePacket,
        b"KEY!",
    ))
    .unwrap();
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("sanitized batch error"));
    for secret in ["KEY!", "4b455921", "75, 69, 89, 33"] {
        assert!(!logs.contains(secret));
    }
    assert!(rec.take_live_events().unwrap().is_empty());
    assert!(sink.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn count_nested_unknown_truncated_and_trailing_batches_fail_atomically() {
    let safe = safe_native_batch();
    let raw = safe.raw_payload();
    let mut nonzero = raw.clone();
    nonzero[0] = 1;
    let mut trailing = raw.clone();
    trailing.push(0xff);
    let mut inner = CompressedGamePackets::new();
    inner.add(&SCSkillEndedPacket { tl: 7 });
    let mut nested = CompressedGamePackets::new();
    nested.add(&inner);
    let cases = [
        (safe.len() as u16, nonzero),
        (safe.len() as u16, trailing),
        (safe.len() as u16 + 1, raw.clone()),
        (safe.len() as u16 - 1, raw.clone()),
        (safe.len() as u16, raw[..raw.len() - 1].to_vec()),
        (1, vec![0, 0, 0xff, 0xff, b'K', b'E', b'Y', b'!']),
        (1, nested.raw_payload()),
        (1, encode(&inner)), // nested DD04 level header is not an inner zero/opcode header
    ];
    for (count, body) in cases {
        let sink = Bytes::default();
        let rec = Recorder::incremental_game_to(sink.clone());
        rec.control_game_ready(true);
        let mut game = client(rec.clone());
        let mut frame = vec![0xdd, 4];
        frame.extend_from_slice(&count.to_le_bytes());
        frame.extend_from_slice(&deflate_stored(&body));
        redacted(&game.decode(frame).unwrap_err());
        assert!(rec.take_live_events().unwrap().is_empty());
        assert!(sink.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn inflater_refuses_bombs_and_frame_count_limits_before_unbounded_growth() {
    // Only cap+1 plaintext bytes are needed to test a bomb; no huge fixture allocation.
    let bomb = miniz_oxide::deflate::compress_to_vec(&vec![b'A'; MAX_BATCH_BYTES + 1], 9);
    assert!(bomb.len() < MAX_GAME_FRAME - 4);
    assert_eq!(inflate_batch(&bomb).unwrap_err(), BatchError::Limit);
    let exact = miniz_oxide::deflate::compress_to_vec(&vec![b'A'; MAX_BATCH_BYTES], 9);
    assert_eq!(inflate_batch(&exact).unwrap().len(), MAX_BATCH_BYTES);
    let mut trailing = exact;
    trailing.extend_from_slice(&[0xaa; 64]);
    assert_eq!(inflate_batch(&trailing).unwrap_err(), BatchError::Inflate);
    let mut game = client(Recorder::new(false));
    let error = game.decode(vec![0; MAX_GAME_FRAME + 1]).unwrap_err();
    redacted(&error);
    let mut frame = vec![0xdd, 4];
    frame.extend_from_slice(&((MAX_BATCH_PACKETS + 1) as u16).to_le_bytes());
    frame.extend_from_slice(&deflate_stored(&[]));
    assert_eq!(
        game.decode(frame).unwrap_err().downcast_ref::<BatchError>(),
        Some(&BatchError::Limit)
    );
    assert_eq!(
        split_game_batch(Dir::S2C, 0, &vec![0; MAX_BATCH_BYTES + 1]).unwrap_err(),
        BatchError::Limit
    );
}
