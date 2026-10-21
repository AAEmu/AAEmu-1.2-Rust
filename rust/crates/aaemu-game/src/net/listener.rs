//! `GameNetwork.Start` + the per-session parts of `GameProtocolHandler`
//! (`OnConnect` / `OnReceive` / `OnDisconnect`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aaemu_commons::net::FrameCodec;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::net::connection::{ConnCommand, GameConnection};
use crate::net::outbox::{outbox, OutboxReceiver};
use crate::server::GameServer;

/// Frames written to the socket buffer before one flush.
const WRITE_BATCH_FRAMES: usize = 256;
/// A batch the client does not accept within this time means it stopped reading.
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Binds `Network.Host:Port` (`"*"` = any) and serves connections until the task is dropped.
pub async fn run(server: Arc<GameServer>) -> std::io::Result<()> {
    let cfg = &server.config.network;
    let host = if cfg.host == "*" {
        "0.0.0.0"
    } else {
        cfg.host.as_str()
    };
    let listener = TcpListener::bind((host, cfg.port)).await?;
    tracing::info!("Network started on {}", listener.local_addr()?);
    loop {
        let (socket, peer) = listener.accept().await?;
        let server = server.clone();
        tokio::spawn(async move { serve(server, socket, peer).await });
    }
}

/// Drives one client connection: a reader/dispatcher loop owning the [`GameConnection`] plus a
/// writer task fed by its outbox ([`write_frames`]).
pub async fn serve(server: Arc<GameServer>, socket: TcpStream, peer: SocketAddr) {
    let id = server.next_connection_id();
    tracing::info!("Connect from {} established, session id: {id}", peer.ip());
    let _ = socket.set_nodelay(true);
    let (rd, wr) = socket.into_split();
    let mut reader = FramedRead::new(rd, FrameCodec);

    let (out_tx, out_rx) = outbox();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ConnCommand>();
    let mut writer = tokio::spawn(write_frames(
        id,
        FramedWrite::new(wr, FrameCodec),
        out_rx,
        cmd_tx.clone(),
    ));

    let (mut conn, handle) = GameConnection::new(id, peer, out_tx, cmd_tx);
    server.connections.add(handle.clone());
    // Created before the packet loop: a panic in a packet handler or in `leave_world_task` too
    // must release the session's reservation, remove a half-left world entry and free the
    // account (see `DisconnectCleanup`). It is disarmed once `on_disconnect` returned.
    let mut cleanup = DisconnectCleanup::new(&server, handle);

    while !conn.is_closing() {
        tokio::select! {
            frame = reader.next() => match frame {
                Some(Ok(payload)) => {
                    if let Err(e) = server.handle_frame(&mut conn, &payload).await {
                        // GameProtocolHandler.OnReceive catch-all: connection.Shutdown()
                        tracing::error!("session {id}: {e}");
                        break;
                    }
                }
                Some(Err(e)) => {
                    tracing::debug!("session {id}: read error: {e}");
                    break;
                }
                None => break,
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(ConnCommand::Send(packet)) => conn.send(packet.as_ref()),
                Some(ConnCommand::LeaveWorld(target)) => {
                    let active = conn.active_char.clone();
                    crate::world::session::leave_world_task(&server, Some(&mut conn), target, active).await;
                }
                Some(ConnCommand::ReturnToWorldList) => {
                    crate::world::session::return_to_world_list(&server, &mut conn).await;
                }
                Some(ConnCommand::DetachAndClose) => {
                    conn.active_char = None;
                    break;
                }
                Some(ConnCommand::Close) | None => break,
            },
        }
    }

    on_disconnect(&server, &mut conn).await;
    cleanup.complete();
    drop(cleanup);
    drop(conn); // closes the writer channel; queued frames are flushed first
    if tokio::time::timeout(Duration::from_secs(5), &mut writer)
        .await
        .is_err()
    {
        // The client is not reading: drop what is still queued with the task.
        writer.abort();
    }
    tracing::info!("Client from {} disconnected", peer.ip());
}

/// The writer task: drains the outbox into the socket, up to [`WRITE_BATCH_FRAMES`] frames per
/// flush (one write for many small packets instead of one per packet). A write error or a batch
/// stuck for [`WRITE_STALL_TIMEOUT`] closes the connection.
async fn write_frames(
    id: u32,
    mut sink: FramedWrite<tokio::net::tcp::OwnedWriteHalf, FrameCodec>,
    mut frames: OutboxReceiver,
    commands: mpsc::UnboundedSender<ConnCommand>,
) {
    while let Some(first) = frames.recv().await {
        let batch = async {
            sink.feed(first).await?;
            for _ in 1..WRITE_BATCH_FRAMES {
                let Ok(frame) = frames.try_recv() else { break };
                sink.feed(frame).await?;
            }
            SinkExt::<bytes::Bytes>::flush(&mut sink).await
        };
        match tokio::time::timeout(WRITE_STALL_TIMEOUT, batch).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!("write to session {id} failed: {e}");
                break;
            }
            Err(_) => {
                tracing::warn!(
                    "session {id}: client accepted no data for {} s, closing the connection",
                    WRITE_STALL_TIMEOUT.as_secs()
                );
                break;
            }
        }
    }
    let _ = commands.send(ConnCommand::Close);
}

/// `GameProtocolHandler.OnDisconnect` + `GameConnection.OnDisconnect`.
pub async fn on_disconnect(server: &Arc<GameServer>, conn: &mut GameConnection) {
    let account_id = conn.account_id();
    // The departure save runs before the account is freed: meanwhile `EnterWorldManager`
    // refuses a second login of the account, so a reconnect cannot load the character while its
    // previous session is still writing it. Deviation from the C# (`Remove` first), see
    // PORTING.md.
    crate::world::session::on_disconnect(server, conn).await;
    server.accounts.remove_if_owned(account_id, conn.id);
    server.streams.remove_token(conn.id);
    server.connections.remove(conn.id);
    // AccountManager.UpdateLoginTime(AccountId, UtcNow), fire and forget. Deviation: C# also runs
    // it for connections that never entered (AccountId 0), which inserts an `accounts` row with
    // id 0 via GetAccountDetailsInternal; that is skipped here.
    if account_id != 0 {
        let server = server.clone();
        tokio::spawn(async move {
            crate::db::update_login_time(
                &server.db,
                &server.config,
                account_id,
                chrono::Utc::now().naive_utc(),
            )
            .await;
        });
    }
}

/// Best-effort cleanup after the connection task ends, including a panic anywhere in its packet
/// loop or in [`on_disconnect`] (Server-Backlog: "Disconnect cleanup: no unwind guard if cleanup
/// panics"). It is disarmed by [`DisconnectCleanup::complete`] once the normal disconnect finished;
/// every step is then idempotent anyway. A panic in the reservation release or in the world
/// removal must not abort the unwind, and the reservation is released before the world part runs.
struct DisconnectCleanup {
    server: Arc<GameServer>,
    /// Live account id: `X2EnterWorld` sets it after the connection was accepted.
    connection: crate::net::connection::ConnectionHandle,
    completed: bool,
}

impl DisconnectCleanup {
    fn new(server: &Arc<GameServer>, connection: crate::net::connection::ConnectionHandle) -> Self {
        Self {
            server: server.clone(),
            connection,
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for DisconnectCleanup {
    fn drop(&mut self) {
        if !self.completed {
            let world_cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::world::session::force_release_connection(&self.server, self.connection.id);
            }));
            if world_cleanup.is_err() {
                tracing::error!(
                    "session {}: the fallback world cleanup panicked; the reservation was released first",
                    self.connection.id
                );
            }
        }
        self.server
            .accounts
            .remove_if_owned(self.connection.account_id(), self.connection.id);
        self.server.streams.remove_token(self.connection.id);
        self.server.connections.remove(self.connection.id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::test_support::{spawn_player, test_character, test_server, TestClient};

    /// A panic in the disconnect cleanup must not leave the account registered, the character's
    /// world entry behind or its session reservation held. The guard is dropped while the panic
    /// unwinds, exactly like `serve` drops it after `on_disconnect`.
    #[tokio::test]
    async fn a_panicking_disconnect_cleanup_still_releases_the_session() {
        let server = Arc::new(test_server());
        let client = TestClient::new(&server, 1, 1000);
        let player = spawn_player(
            &server,
            &client,
            test_character(7, "Ghost", 100.0, 100.0),
            0x700,
        );
        assert!(server.accounts.try_add(1000, 1));
        assert!(server.enter_world.reserve_character(7, 1).is_ok());

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _cleanup = super::DisconnectCleanup::new(&server, client.conn.handle());
            panic!("disconnect cleanup panicked");
        }));
        assert!(result.is_err());
        assert_eq!(server.enter_world.character_owner(7), None);
        assert!(server.world.character_by_id(7).is_none());
        assert!(!server.accounts.contains(1000));
        assert!(server.connections.get(1).is_none());
        assert_eq!(server.streams.account_for(1), None);
        drop(player);
    }
}
