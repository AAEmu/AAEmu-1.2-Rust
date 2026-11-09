//! Login-server link: port of `LoginNetwork`, `LoginProtocolHandler`, `LoginConnection` and the
//! `LoadTask` (game server is the TCP client).

pub mod packets;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aaemu_commons::net::FrameCodec;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::managers::AddAccountResult;
use crate::packets::g2c::SCReconnectAuthPacket;
use crate::server::GameServer;
use packets::{
    encode, GLGameServerLoadPacket, GLPlayerEnterPacket, GLRegisterGameServerPacket,
    GLRequestInfoPacket, LoginInPacket, LoginOutPacket,
};

/// Delay before reconnecting after the link drops or a connect attempt fails.
/// (C# restarts the client immediately on disconnect and does not retry a failed initial connect.)
pub const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// `LoadTask` repeat interval.
pub const LOAD_INTERVAL: Duration = Duration::from_secs(60);

/// Reports the current load level and slot occupancy to the login server. Sent on the periodic
/// tick (the first tick fires immediately after registration) and right after an account leaves
/// the game, so the login queue head moves without waiting for the next tick.
pub fn report_load(server: &GameServer) {
    let (sequence, online) = server.next_occupancy();
    server.login.send(&GLGameServerLoadPacket {
        load: server.load_level(),
        online,
        sequence,
    });
}

/// Sender side of the login link (`LoginNetwork.Instance.GetConnection()`).
#[derive(Debug, Default)]
pub struct LoginLink {
    tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
}

impl LoginLink {
    /// `LoginConnection.SendPacket`. Returns `false` if the link is down (C# would throw a
    /// NullReferenceException on the null connection).
    pub fn send<P: LoginOutPacket>(&self, packet: &P) -> bool {
        tracing::debug!("LoginPacket: G->L {} {:?}", P::NAME, packet);
        let guard = self.tx.lock().expect("login link poisoned");
        match guard.as_ref() {
            Some(tx) => tx.send(encode(packet)).is_ok(),
            None => {
                tracing::error!("LoginServer link is down; dropping {}", P::NAME);
                false
            }
        }
    }

    pub(crate) fn set(&self, tx: Option<mpsc::UnboundedSender<Vec<u8>>>) {
        *self.tx.lock().expect("login link poisoned") = tx;
    }

    pub fn is_connected(&self) -> bool {
        self.tx.lock().expect("login link poisoned").is_some()
    }
}

/// `LoginNetwork.Start` + reconnect loop. Runs forever.
pub async fn run(server: Arc<GameServer>) {
    loop {
        let cfg = &server.config.login_network;
        let addr = format!("{}:{}", cfg.host, cfg.port);
        match TcpStream::connect(&addr).await {
            Ok(stream) => {
                tracing::info!("Connect to {addr} established");
                if let Err(e) = session(&server, stream).await {
                    tracing::error!("LoginServer link error: {e}");
                }
                tracing::info!("Connection to LoginServer has been lost");
            }
            Err(e) => tracing::error!("Failed to connect to LoginServer at {addr}: {e}"),
        }
        server.login.set(None);
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn session(server: &Arc<GameServer>, stream: TcpStream) -> std::io::Result<()> {
    let (rd, wr) = stream.into_split();
    let mut reader = FramedRead::new(rd, FrameCodec);
    let mut writer = FramedWrite::new(wr, FrameCodec);
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    server.login.set(Some(tx));

    // LoginConnection.OnConnect
    server.login.send(&GLRegisterGameServerPacket {
        secret_key: server.config.secret_key.clone(),
        gs_id: server.config.id,
        additional_gs_ids: server.config.additionales_id.clone(),
    });

    let mut load_tick = tokio::time::interval(LOAD_INTERVAL);
    loop {
        tokio::select! {
            frame = reader.next() => match frame {
                Some(Ok(payload)) => {
                    if !handle(server, &payload).await {
                        return Ok(()); // registration refused: Connection.Close()
                    }
                }
                Some(Err(e)) => return Err(e),
                None => return Ok(()),
            },
            out = rx.recv() => match out {
                Some(bytes) => writer.send(bytes).await?,
                None => return Ok(()),
            },
            _ = load_tick.tick() => {
                // LoadTask.Execute
                report_load(server);
            }
        }
    }
}

/// Handles one L2G packet. Returns `false` when the link must be closed.
pub(crate) async fn handle(server: &Arc<GameServer>, payload: &[u8]) -> bool {
    let packet = match LoginInPacket::decode(payload) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Malformed packet from LoginServer: {e}");
            return true;
        }
    };
    tracing::debug!("LoginPacket: L->G {:?}", packet);
    match packet {
        LoginInPacket::RegisterGameServer {
            result,
            pre_select_character_period,
        } => {
            if result != 0 {
                tracing::error!("Error registering on LoginServer");
                return false; // TODO (C#): or shutdown?
            }
            // The login advertises the pre-select period to the clients (`ACJoinResponse.afs`),
            // so its value is authoritative here: a mismatch would let the client offer an edit
            // this server refuses (or refuse a world entry the client still tries).
            match pre_select_character_period {
                Some(flag) => {
                    let configured = server.config.characters.pre_select_period;
                    if configured != flag {
                        tracing::warn!(
                            "Characters:PreSelectPeriod is {configured} but the login server advertises {flag}; using the login value"
                        );
                    }
                    server
                        .config
                        .update(|config| config.characters.pre_select_period = flag);
                }
                None => tracing::warn!(
                    "The login server did not report the pre-select period; using Characters:PreSelectPeriod"
                ),
            }
            tracing::info!("Successfully registered on LoginServer");
            // Prime the login server's world-queue occupancy right away (the first periodic
            // tick would also fire immediately, but this is explicit and independent of it).
            report_load(server);
        }
        LoginInPacket::PlayerEnter {
            account_id,
            connection_id,
        } => {
            // EnterWorldManager.AddAccount
            match server.enter_world.add_account(
                account_id,
                connection_id,
                server.accounts.contains(account_id),
            ) {
                AddAccountResult::AlreadyConnected => {
                    // C# `AddAccount`: `GLPlayerEnter(..., 1)` when the account is already
                    // connected; the login server answers the client's pending enter-world
                    // request with `ACEnterWorldDenied(1)`.
                    tracing::warn!(
                        "LGPlayerEnter refused: account {account_id} is already connected (session {}); replying 1",
                        server.accounts.connection_of(account_id).unwrap_or(0)
                    );
                    let (sequence, online) = server.next_occupancy();
                    server.login.send(&GLPlayerEnterPacket {
                        connection_id,
                        gs_id: server.config.id,
                        result: 1,
                        online,
                        sequence,
                        cookie: 0,
                    });
                }
                AddAccountResult::Registered { cookie } => {
                    let (sequence, online) = server.next_occupancy();
                    server.login.send(&GLPlayerEnterPacket {
                        connection_id,
                        gs_id: server.config.id,
                        result: 0,
                        online,
                        sequence,
                        cookie,
                    });
                }
                // C#: `_accounts.Add` throws ArgumentException inside LGPlayerEnterPacket.Read
                // (no reply, the login link dies). Found by live testing: a client that takes
                // its world cookie but never connects leaves the entry behind, and the login
                // server hands the same (pooled) connection id to the next client.
                AddAccountResult::ReplacedStale {
                    stale_account,
                    cookie,
                } => {
                    tracing::warn!(
                        "LGPlayerEnter: connection id {connection_id} was still pending for account {stale_account} (never entered); replaced by account {account_id}"
                    );
                    let (sequence, online) = server.next_occupancy();
                    server.login.send(&GLPlayerEnterPacket {
                        connection_id,
                        gs_id: server.config.id,
                        result: 0,
                        online,
                        sequence,
                        cookie,
                    });
                }
            }
        }
        LoginInPacket::PlayerReconnect { token } => {
            if let Some(conn) = server.connections.get_by_reconnect_token(token) {
                tracing::debug!(
                    "LGPlayerReconnect: SCReconnectAuth to session {} (token {token})",
                    conn.id
                );
                conn.send(SCReconnectAuthPacket { token });
            } else {
                tracing::warn!("LGPlayerReconnect for a token no live session owns: {token}");
            }
        }
        LoginInPacket::RequestInfo {
            connection_id,
            request_id,
            account_id,
        } => {
            let characters = if account_id != 0 {
                match crate::db::load_login_characters(&server.db, account_id).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("LoadCharacters({account_id}) failed: {e}");
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            server.login.send(&GLRequestInfoPacket {
                connection_id,
                request_id,
                characters,
            });
        }
        LoginInPacket::Unknown { opcode, body } => {
            let dump: String = body.iter().map(|b| format!("{b:02x} ")).collect();
            tracing::error!("Unknown packet 0x{opcode:02x} from LoginServer:\n{dump}");
        }
    }
    true
}
