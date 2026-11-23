use super::*;
use aaemu_commons::net::FrameCodec;
use aaemu_game::opcodes::cs;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::codec::Framed;

fn frame(opcode: u16, body: &[u8]) -> Vec<u8> {
    let mut frame = vec![0xdd, 1, 0, 0];
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(body);
    frame
}

#[tokio::test]
async fn teleport_completes_under_continuous_ambient_traffic() {
    use crate::record::{Channel, Dir};

    let (io, peer) = tokio::io::duplex(64 * 1024);
    let rec = Recorder::new(true);
    let mut game = GameClient::from_io(io, rec.clone());
    game.state.self_obj = 42;
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut peer = Framed::new(peer, FrameCodec);
        let chat = peer.next().await.unwrap().unwrap();
        assert_eq!(
            u16::from_le_bytes([chat[4], chat[5]]),
            cs::CSSendChatMessagePacket
        );
        let mut teleport = PacketStream::new();
        teleport
            .write_u8(0)
            .write_i16(0)
            .write_position(10.0, 20.0, 30.0)
            .write_f32(30.0);
        peer.send(frame(sc::SCTeleportUnitPacket, teleport.as_bytes()))
            .await
            .unwrap();
        let ambient = frame(sc::SCErrorMsgPacket, &[7, 0]);
        let mut sent = 0;
        // Guarantee traffic after the teleport without depending on timer resolution.
        for _ in 0..2 {
            peer.send(ambient.clone()).await.unwrap();
            sent += 1;
        }
        let (mut writer, mut reader) = peer.split();
        let mut acks = Vec::new();
        let mut stop_requested = false;
        {
            // Keep a pending send alive across ACKs; backpressure must not block reads/stop.
            let sending = async {
                loop {
                    writer.send(ambient.clone()).await.unwrap();
                    sent += 1;
                    tokio::task::yield_now().await;
                }
            };
            tokio::pin!(sending);
            loop {
                if stop_requested && acks.len() == 2 {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = &mut stopped, if !stop_requested => stop_requested = true,
                    incoming = reader.next() => {
                        let bytes = incoming.unwrap().unwrap();
                        acks.push(u16::from_le_bytes([bytes[4], bytes[5]]));
                    },
                    _ = &mut sending => unreachable!("ambient sender runs until stopped"),
                }
            }
        }
        (sent, acks)
    });
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    tokio::time::timeout(
        Duration::from_secs(1),
        control_tp(&mut game, 10.0, 20.0, 30.0, 1500, until),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        rec.events()
            .iter()
            .filter(|event| {
                event.channel == Channel::Game
                    && event.dir == Dir::S2C
                    && event.level == crate::game::LEVEL_GAME
            })
            .skip_while(|event| event.opcode != sc::SCTeleportUnitPacket)
            .skip(1)
            .filter(|event| {
                event.opcode == sc::SCErrorMsgPacket && event.body_bytes() == [7, 0]
            })
            .count()
            >= 2
    );
    // The fixture keeps draining acknowledgments after this stop signal.
    let _ = stop.send(());
    let (sent, acks) = tokio::time::timeout(Duration::from_secs(1), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(sent > 1);
    assert_eq!(acks, [cs::CSTeleportEndedPacket, cs::CSMoveUnitPacket]);
    // Assert the server's decoded position, including native Z quantization.
    assert_eq!(game.state.observed_self_pos, Some((10.0, 20.0, 29.9995)));
    game.disconnect_hard().await;
}

#[tokio::test]
async fn hard_close_releases_real_game_reader_writer_and_stream_without_logout() {
    // Ephemeral loopback fixtures only: no AAEmu process, login handshake, or database.
    let game_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let login_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rec = Recorder::new(true);
    let mut game = GameClient::connect(
        &game_listener.local_addr().unwrap().to_string(),
        rec.clone(),
    )
    .await
    .unwrap();
    let mut game_peer = game_listener.accept().await.unwrap().0;
    let stream = StreamClient::connect(
        &stream_listener.local_addr().unwrap().to_string(),
        rec.clone(),
    )
    .await
    .unwrap();
    let mut stream_peer = stream_listener.accept().await.unwrap().0;
    let login = LoginClient::connect(
        &login_listener.local_addr().unwrap().to_string(),
        rec.clone(),
    )
    .await
    .unwrap();
    let mut login_peer = login_listener.accept().await.unwrap().0;
    let packet = frame(sc::SCErrorMsgPacket, &[7, 0]);
    game_peer
        .write_all(&(packet.len() as u16).to_le_bytes())
        .await
        .unwrap();
    game_peer.write_all(&packet).await.unwrap();
    game.recv_within(Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    let session = ControlSession {
        game,
        stream,
        _login: login,
    };
    tokio::time::timeout(Duration::from_secs(1), session.disconnect_hard())
        .await
        .unwrap();
    for peer in [&mut game_peer, &mut stream_peer, &mut login_peer] {
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(
            bytes.is_empty(),
            "close must emit no application logout or other packets"
        );
    }
    assert_eq!(rec.events().len(), 1);
    assert_eq!(rec.events()[0].opcode, sc::SCErrorMsgPacket);
}
