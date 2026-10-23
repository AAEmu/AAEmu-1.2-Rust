use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use tokio::sync::mpsc;

use super::*;
use crate::auth::{AuthFlowResult, ReconnectAuthFlow};
use crate::models::{AccountId, GameServerId, GsLoad, LoginCharacterInfo, LoginDeniedReason};
use crate::packets::g2l::{
    GlGameServerLoad, GlPlayerEnter, GlPlayerReconnect, GlRegisterGameServer, GlRequestInfo,
};
use crate::packets::l2c::opcodes as l2c_op;
use crate::packets::l2g::{opcodes as l2g_op, LgPlayerReconnect, LgRequestInfo};
use crate::packets::split_opcode;
use crate::test_support::{
    test_config, test_connection, test_server, MockBackend, RecordingClient,
};

const ACCOUNT: AccountId = AccountId(42);
const WORLD: GameServerId = GameServerId(1);
const OTHER_WORLD: GameServerId = GameServerId(2);

async fn fixture() -> Arc<LoginServer> {
    let config = test_config(serde_json::json!({
        "GameServers": [
            { "Id": 1, "Name": "World", "Host": "127.0.0.1", "Port": 1239 },
            { "Id": 2, "Name": "Other", "Host": "127.0.0.1", "Port": 1239 }
        ]
    }));
    let server = test_server(config, Arc::new(MockBackend::default()));
    server.games.load(&server.config).await;
    server
}

fn connection() -> (Arc<InternalConnection>, mpsc::UnboundedReceiver<Vec<u8>>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Arc::new(InternalConnection::new(IpAddr::V4(Ipv4Addr::LOCALHOST), tx)),
        rx,
    )
}

async fn register(server: &Arc<LoginServer>, conn: &Arc<InternalConnection>, secret: &str) {
    register_as(server, conn, WORLD, vec![], secret).await;
}

async fn register_as(
    server: &Arc<LoginServer>,
    conn: &Arc<InternalConnection>,
    gs_id: GameServerId,
    mirrors: Vec<GameServerId>,
    secret: &str,
) {
    handle_g2l(
        server,
        conn,
        G2lPacket::RegisterGameServer(GlRegisterGameServer {
            secret_key: secret.into(),
            gs_id,
            mirrors,
        }),
    )
    .await
    .unwrap();
}

async fn reconnect_result(server: &LoginServer, gs_id: GameServerId, token: u32) -> AuthFlowResult {
    AuthFlow::Reconnect(ReconnectAuthFlow {
        gs_id,
        account_id: ACCOUNT,
        token,
    })
    .start(server, &RecordingClient::default())
    .await
    .unwrap()
}

async fn assert_unauthorized_token_rejected(secret: Option<&str>) {
    let server = fixture().await;
    let (conn, mut rx) = connection();
    if let Some(secret) = secret {
        register(&server, &conn, secret).await;
    }
    let result = handle_g2l(
        &server,
        &conn,
        G2lPacket::PlayerReconnect(GlPlayerReconnect {
            gs_id: WORLD,
            account_id: ACCOUNT,
            token: 777,
        }),
    )
    .await;

    assert!(
        matches!(
            reconnect_result(&server, WORLD, 777).await,
            AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE)
        ),
        "unauthenticated internal peer seeded a usable reconnect token"
    );
    assert!(result.is_err());
    assert!(
        rx.try_recv().is_err(),
        "must not acknowledge an unauthorized token"
    );
}

#[tokio::test]
async fn unregistered_connection_cannot_seed_reconnect_token() {
    assert_unauthorized_token_rejected(None).await;
}

#[tokio::test]
async fn bad_secret_connection_cannot_seed_reconnect_token() {
    assert_unauthorized_token_rejected(Some("wrong")).await;
}

#[tokio::test]
async fn registered_connection_cannot_seed_an_unrelated_world_token() {
    let server = fixture().await;
    let (conn, mut rx) = connection();
    register(&server, &conn, "test").await;
    rx.try_recv().unwrap();
    let result = handle_g2l(
        &server,
        &conn,
        G2lPacket::PlayerReconnect(GlPlayerReconnect {
            gs_id: OTHER_WORLD,
            account_id: ACCOUNT,
            token: 777,
        }),
    )
    .await;

    assert!(
        matches!(
            reconnect_result(&server, OTHER_WORLD, 777).await,
            AuthFlowResult::Denied(LoginDeniedReason::BAD_RESPONSE)
        ),
        "server A seeded a usable reconnect token for server B"
    );
    assert!(result.is_err());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn registered_parent_and_mirror_can_seed_reconnect_tokens() {
    let server = fixture().await;
    let (conn, mut rx) = connection();
    register_as(&server, &conn, WORLD, vec![OTHER_WORLD], "test").await;
    rx.try_recv().unwrap();
    for (gs_id, token) in [(WORLD, 777), (OTHER_WORLD, 888)] {
        handle_g2l(
            &server,
            &conn,
            G2lPacket::PlayerReconnect(GlPlayerReconnect {
                gs_id,
                account_id: ACCOUNT,
                token,
            }),
        )
        .await
        .unwrap();
        let (op, mut s) = split_opcode(rx.try_recv().unwrap()).unwrap();
        assert_eq!(op, l2g_op::LG_PLAYER_RECONNECT);
        assert_eq!(LgPlayerReconnect::read(&mut s).unwrap().token, token);
        assert_eq!(
            reconnect_result(&server, gs_id, token).await,
            AuthFlowResult::Success {
                account_id: ACCOUNT,
                username: None
            }
        );
        if gs_id == WORLD {
            // A parent's token is still accepted for its mirror before a mirror token table exists.
            assert_eq!(
                reconnect_result(&server, OTHER_WORLD, token).await,
                AuthFlowResult::Success {
                    account_id: ACCOUNT,
                    username: None
                }
            );
        }
    }
}

#[tokio::test]
async fn replaced_connection_cannot_mutate_internal_state() {
    let server = fixture().await;
    let (old, mut old_rx) = connection();
    register(&server, &old, "test").await;
    old_rx.try_recv().unwrap();
    let (current, _rx) = connection();
    register(&server, &current, "test").await;

    for packet in [
        G2lPacket::GameServerLoad(GlGameServerLoad { load: GsLoad::HIGH }),
        G2lPacket::PlayerReconnect(GlPlayerReconnect {
            gs_id: WORLD,
            account_id: ACCOUNT,
            token: 777,
        }),
        G2lPacket::PlayerEnter(GlPlayerEnter {
            connection_id: crate::models::ConnectionId(1),
            gs_id: WORLD,
            result: 0,
        }),
        G2lPacket::RequestInfo(GlRequestInfo {
            connection_id: crate::models::ConnectionId(1),
            request_id: 1,
            characters: vec![],
        }),
    ] {
        assert!(
            handle_g2l(&server, &old, packet).await.is_err(),
            "replaced connection still authorized"
        );
    }
    assert_eq!(
        server.games.get_game_server(WORLD).unwrap().load,
        GsLoad::default()
    );
    assert!(matches!(
        reconnect_result(&server, WORLD, 777).await,
        AuthFlowResult::Denied(_)
    ));
    assert!(old_rx.try_recv().is_err());
}

#[tokio::test]
async fn unauthorized_enter_response_cannot_complete_pending_world_entry() {
    let server = fixture().await;
    let (owner, mut owner_rx) = connection();
    register_as(&server, &owner, OTHER_WORLD, vec![], "test").await;
    owner_rx.try_recv().unwrap();
    handle_g2l(
        &server,
        &owner,
        G2lPacket::PlayerReconnect(GlPlayerReconnect {
            gs_id: OTHER_WORLD,
            account_id: ACCOUNT,
            token: 777,
        }),
    )
    .await
    .unwrap();
    owner_rx.try_recv().unwrap();
    let (client, mut sent) = test_connection(owner.ip);
    server.connections.add(client.clone()).unwrap();
    client
        .authenticate(
            &server,
            AuthFlow::Reconnect(ReconnectAuthFlow {
                gs_id: OTHER_WORLD,
                account_id: ACCOUNT,
                token: 777,
            }),
        )
        .await
        .unwrap();
    sent.drain();
    client
        .initiate_enter_world(&server, OTHER_WORLD)
        .await
        .unwrap();
    owner_rx.try_recv().unwrap();

    let (unregistered, _rx) = connection();
    let (unrelated, _rx) = connection();
    register(&server, &unrelated, "test").await;
    let response = G2lPacket::PlayerEnter(GlPlayerEnter {
        connection_id: client.id(),
        gs_id: OTHER_WORLD,
        result: 0,
    });
    for attacker in [&unregistered, &unrelated] {
        assert!(handle_g2l(&server, attacker, response.clone())
            .await
            .is_err());
    }
    assert!(sent.drain().is_empty());
    handle_g2l(&server, &owner, response).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        client.take_enter_world_task().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(sent.opcodes(), vec![l2c_op::AC_WORLD_COOKIE]);
}

#[tokio::test]
async fn unregistered_empty_info_cannot_complete_request() {
    let server = fixture().await;
    let (conn, _rx) = connection();
    let (ids, wait) = server.requests.create(1, Duration::from_secs(30)).unwrap();
    let response = G2lPacket::RequestInfo(GlRequestInfo {
        connection_id: crate::models::ConnectionId(1),
        request_id: ids[0],
        characters: vec![],
    });
    let result = handle_g2l(&server, &conn, response).await;
    assert!(
        wait.now_or_never().is_none(),
        "unauthenticated peer completed a pending request"
    );
    assert!(result.is_err());
    server.requests.release_ids(ids);
}

async fn receive_info(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> LgRequestInfo {
    let payload = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let (op, mut s) = split_opcode(payload).unwrap();
    assert_eq!(op, l2g_op::LG_REQUEST_INFO);
    LgRequestInfo::read(&mut s).unwrap()
}

fn character() -> LoginCharacterInfo {
    LoginCharacterInfo {
        id: 1,
        account_id: ACCOUNT.0,
        gs_id: 0,
        name: "Bob".into(),
        race: 1,
        gender: 2,
    }
}

#[tokio::test]
async fn info_response_must_match_pending_client_and_server() {
    let server = fixture().await;
    let (a, mut a_rx) = connection();
    let (b, mut b_rx) = connection();
    register(&server, &a, "test").await;
    register_as(&server, &b, OTHER_WORLD, vec![], "test").await;
    a_rx.try_recv().unwrap();
    b_rx.try_recv().unwrap();
    let (client, _sent) = test_connection(a.ip);
    server.connections.add(client.clone()).unwrap();
    let list = tokio::spawn({
        let server = server.clone();
        let client = client.clone();
        async move { server.games.get_world_list(&client).await.unwrap() }
    });
    let a_request = receive_info(&mut a_rx).await;
    let b_request = receive_info(&mut b_rx).await;
    for characters in [vec![], vec![character()]] {
        let forged = G2lPacket::RequestInfo(GlRequestInfo {
            connection_id: client.id(),
            request_id: b_request.request_id,
            characters,
        });
        assert!(
            handle_g2l(&server, &a, forged).await.is_err(),
            "server A completed server B's request"
        );
    }
    let forged = G2lPacket::RequestInfo(GlRequestInfo {
        connection_id: crate::models::ConnectionId(client.id().0 + 1),
        request_id: b_request.request_id,
        characters: vec![],
    });
    assert!(handle_g2l(&server, &b, forged).await.is_err());
    assert!(client.get_characters().is_empty());
    assert!(!list.is_finished());
    for (conn, request, characters) in [(&a, a_request, vec![]), (&b, b_request, vec![character()])]
    {
        handle_g2l(
            &server,
            conn,
            G2lPacket::RequestInfo(GlRequestInfo {
                connection_id: client.id(),
                request_id: request.request_id,
                characters,
            }),
        )
        .await
        .unwrap();
    }
    let result = tokio::time::timeout(Duration::from_secs(5), list)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.characters,
        vec![LoginCharacterInfo {
            gs_id: OTHER_WORLD.0,
            ..character()
        }]
    );
}

#[tokio::test]
async fn info_responses_for_authorized_mirrors_complete_world_list() {
    let server = fixture().await;
    let (conn, mut rx) = connection();
    register_as(&server, &conn, WORLD, vec![OTHER_WORLD], "test").await;
    rx.try_recv().unwrap();
    let (client, _sent) = test_connection(conn.ip);
    server.connections.add(client.clone()).unwrap();
    let list = tokio::spawn({
        let server = server.clone();
        let client = client.clone();
        async move { server.games.get_world_list(&client).await.unwrap() }
    });
    for _ in 0..2 {
        let request = receive_info(&mut rx).await;
        handle_g2l(
            &server,
            &conn,
            G2lPacket::RequestInfo(GlRequestInfo {
                connection_id: client.id(),
                request_id: request.request_id,
                characters: vec![character()],
            }),
        )
        .await
        .unwrap();
    }
    let result = tokio::time::timeout(Duration::from_secs(5), list)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result
            .characters
            .iter()
            .map(|c| c.gs_id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn earlier_world_list_cleanup_does_not_remove_reused_request_id() {
    let server = fixture().await;
    let (conn, mut rx) = connection();
    register(&server, &conn, "test").await;
    rx.try_recv().unwrap();
    let (client, _sent) = test_connection(conn.ip);
    server.connections.add(client.clone()).unwrap();

    let mut first = Box::pin(server.games.get_world_list(&client));
    assert!(first.as_mut().now_or_never().is_none());
    let first_request = receive_info(&mut rx).await;
    handle_g2l(
        &server,
        &conn,
        G2lPacket::RequestInfo(GlRequestInfo {
            connection_id: client.id(),
            request_id: first_request.request_id,
            characters: vec![],
        }),
    )
    .await
    .unwrap();

    // Start another world list before the first waiter resumes and cleans up its ids.
    let mut second = Box::pin(server.games.get_world_list(&client));
    assert!(second.as_mut().now_or_never().is_none());
    let second_request = receive_info(&mut rx).await;
    assert_eq!(first_request.request_id, second_request.request_id);
    first.as_mut().now_or_never().unwrap().unwrap();
    handle_g2l(
        &server,
        &conn,
        G2lPacket::RequestInfo(GlRequestInfo {
            connection_id: client.id(),
            request_id: second_request.request_id,
            characters: vec![character()],
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        second
            .as_mut()
            .now_or_never()
            .unwrap()
            .unwrap()
            .characters
            .len(),
        1
    );
}

#[tokio::test]
async fn unsolicited_character_response_cannot_modify_client() {
    let server = fixture().await;
    let (conn, _rx) = connection();
    register(&server, &conn, "test").await;
    let (client, _sent) = test_connection(conn.ip);
    server.connections.add(client.clone()).unwrap();
    let response = G2lPacket::RequestInfo(GlRequestInfo {
        connection_id: client.id(),
        request_id: 123,
        characters: vec![character()],
    });
    assert!(handle_g2l(&server, &conn, response).await.is_err());
    assert!(client.get_characters().is_empty());
}
