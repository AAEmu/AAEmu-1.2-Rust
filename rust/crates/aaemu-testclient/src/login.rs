//! Login server client (C2L / L2C), as spoken by the 1.2 (r208022) Trion client.
//!
//! Frames are `[u16 len][u16 opcode][body]`. The 1.2 client started by a launcher authenticates
//! with `CARequestAuthTrionPacket`: an XML ticket carrying the user name and the hex SHA-256 of
//! the password.

use std::net::Ipv4Addr;
use std::time::Duration;

use aaemu_commons::net::FrameCodec;
use aaemu_commons::PacketStream;
use anyhow::{anyhow, bail, Context};
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

use crate::record::{Channel, Dir, Recorder};

/// C2L opcodes.
pub mod ca {
    pub const REQUEST_AUTH_TRION: u16 = 0x004;
    pub const OTP_NUMBER: u16 = 0x008;
    pub const PC_CERT_NUMBER: u16 = 0x00a;
    pub const LIST_WORLD: u16 = 0x00b;
    pub const ENTER_WORLD: u16 = 0x00c;
    pub const CANCEL_ENTER_WORLD: u16 = 0x00d;
    pub const REQUEST_RECONNECT: u16 = 0x00e;
}

/// L2C opcodes.
pub mod ac {
    pub const JOIN_RESPONSE: u16 = 0x000;
    pub const CHALLENGE: u16 = 0x002;
    pub const AUTH_RESPONSE: u16 = 0x003;
    pub const CHALLENGE2: u16 = 0x004;
    pub const ENTER_OTP: u16 = 0x005;
    pub const SHOW_ARS: u16 = 0x006;
    pub const ENTER_PC_CERT: u16 = 0x007;
    pub const WORLD_LIST: u16 = 0x008;
    pub const WORLD_QUEUE: u16 = 0x009;
    pub const WORLD_COOKIE: u16 = 0x00a;
    pub const ENTER_WORLD_DENIED: u16 = 0x00b;
    pub const LOGIN_DENIED: u16 = 0x00c;
    pub const ACCOUNT_WARNED: u16 = 0x00d;
}

pub fn ac_name(op: u16) -> &'static str {
    match op {
        ac::JOIN_RESPONSE => "ACJoinResponsePacket",
        ac::CHALLENGE => "ACChallengePacket",
        ac::AUTH_RESPONSE => "ACAuthResponsePacket",
        ac::CHALLENGE2 => "ACChallenge2Packet",
        ac::ENTER_OTP => "ACEnterOtpPacket",
        ac::SHOW_ARS => "ACShowArsPacket",
        ac::ENTER_PC_CERT => "ACEnterPcCertPacket",
        ac::WORLD_LIST => "ACWorldListPacket",
        ac::WORLD_QUEUE => "ACWorldQueuePacket",
        ac::WORLD_COOKIE => "ACWorldCookiePacket",
        ac::ENTER_WORLD_DENIED => "ACEnterWorldDeniedPacket",
        ac::LOGIN_DENIED => "ACLoginDeniedPacket",
        ac::ACCOUNT_WARNED => "ACAccountWarnedPacket",
        _ => "ACUnknown",
    }
}

pub fn ca_name(op: u16) -> &'static str {
    match op {
        ca::REQUEST_AUTH_TRION => "CARequestAuthTrionPacket",
        ca::OTP_NUMBER => "CAOtpNumberPacket",
        ca::PC_CERT_NUMBER => "CAPcCertNumberPacket",
        ca::LIST_WORLD => "CAListWorldPacket",
        ca::ENTER_WORLD => "CAEnterWorldPacket",
        ca::CANCEL_ENTER_WORLD => "CACancelEnterWorldPacket",
        ca::REQUEST_RECONNECT => "CARequestReconnectPacket",
        _ => "CAUnknown",
    }
}

/// A world of `ACWorldListPacket`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct World {
    pub id: u8,
    pub name: String,
    pub active: bool,
}

/// A character of `ACWorldListPacket`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginCharacter {
    pub account_id: u32,
    pub gs_id: u8,
    pub id: u32,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct WorldList {
    pub worlds: Vec<World>,
    pub characters: Vec<LoginCharacter>,
}

/// `ACWorldCookiePacket`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldCookie {
    pub cookie: u32,
    pub ip: Ipv4Addr,
    pub port: u16,
}

/// One `ACWorldQueuePacket` received while entering a world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldQueueUpdate {
    pub world_id: u8,
    pub is_premium: bool,
    /// 1-based position; 0 ends the wait.
    pub my_turn: u16,
    pub normal_length: u16,
    pub premium_length: u16,
}

pub struct LoginClient {
    framed: Framed<TcpStream, FrameCodec>,
    rec: Recorder,
    pub timeout: Duration,
    /// How long a queued client waits for the world cookie after `ACWorldQueuePacket`.
    pub queue_timeout: Duration,
    /// Every queue update seen by [`Self::enter_world`], in arrival order.
    pub queue_updates: Vec<WorldQueueUpdate>,
    pub account_id: u32,
}

/// The launcher's Trion ticket for `user` / `password`.
pub fn trion_ticket(user: &str, password: &str) -> String {
    let hash = hex::encode(Sha256::digest(password.as_bytes()));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <authTicket version=\"1.2\"><storeToken>1</storeToken>\
         <username>{user}</username><password>{hash}</password></authTicket>"
    )
}

impl LoginClient {
    pub async fn connect(addr: &str, rec: Recorder) -> anyhow::Result<Self> {
        let sock = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connecting to login server {addr}"))?;
        sock.set_nodelay(true)?;
        Ok(Self {
            framed: Framed::new(sock, FrameCodec),
            rec,
            timeout: Duration::from_secs(15),
            queue_timeout: Duration::from_secs(600),
            queue_updates: Vec::new(),
            account_id: 0,
        })
    }

    pub async fn send(&mut self, opcode: u16, body: &[u8]) -> anyhow::Result<()> {
        self.rec
            .push(Channel::Login, Dir::C2S, 0, opcode, ca_name(opcode), body);
        let mut payload = Vec::with_capacity(body.len() + 2);
        payload.extend_from_slice(&opcode.to_le_bytes());
        payload.extend_from_slice(body);
        self.framed.send(payload).await?;
        Ok(())
    }

    /// Next L2C packet `(opcode, body)`.
    pub async fn recv(&mut self) -> anyhow::Result<(u16, PacketStream)> {
        self.recv_with_timeout(self.timeout).await
    }

    /// [`Self::recv`] with an explicit timeout (the world queue uses its own deadline without
    /// changing the client's handshake timeout).
    async fn recv_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> anyhow::Result<(u16, PacketStream)> {
        let frame = tokio::time::timeout(timeout, self.framed.next())
            .await
            .map_err(|_| anyhow!("timed out waiting for a login packet"))?
            .ok_or_else(|| anyhow!("login server closed the connection"))??;
        if frame.len() < 2 {
            bail!("short login frame");
        }
        let opcode = u16::from_le_bytes([frame[0], frame[1]]);
        self.rec.push(
            Channel::Login,
            Dir::S2C,
            0,
            opcode,
            ac_name(opcode),
            &frame[2..],
        );
        let mut s = PacketStream::from_bytes(frame);
        s.set_pos(2);
        Ok((opcode, s))
    }

    async fn expect(&mut self, want: u16) -> anyhow::Result<PacketStream> {
        let (op, s) = self.recv().await?;
        if op == ac::LOGIN_DENIED {
            let mut s = s;
            let reason = s.read_u8().unwrap_or(0);
            bail!("login denied (reason {reason})");
        }
        if op != want {
            bail!(
                "expected {} but got {} (0x{op:03x})",
                ac_name(want),
                ac_name(op)
            );
        }
        Ok(s)
    }

    /// `CAOtpNumberPacket`: answers `ACEnterOtpPacket` with the 8-digit code.
    pub async fn submit_otp(&mut self, code: &str) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_str(code);
        self.send(ca::OTP_NUMBER, s.as_bytes()).await
    }

    /// `CAPcCertNumberPacket`: answers `ACEnterPcCertPacket` with the 8-digit PIN.
    pub async fn submit_pc_cert(&mut self, pin: &str) -> anyhow::Result<()> {
        let mut s = PacketStream::new();
        s.write_str(pin);
        self.send(ca::PC_CERT_NUMBER, s.as_bytes()).await
    }

    /// `CARequestAuthTrionPacket` -> `ACJoinResponsePacket` + `ACAuthResponsePacket`.
    pub async fn auth_trion(&mut self, user: &str, password: &str) -> anyhow::Result<u32> {
        self.auth_trion_with_otp(user, password, None).await
    }

    /// Trion login, answering an `ACEnterOtpPacket` prompt with `otp`.
    ///
    /// Without `otp`, an account that requires the second factor stops here with a clear error
    /// (the server never sends the auth success).
    pub async fn auth_trion_with_otp(
        &mut self,
        user: &str,
        password: &str,
        otp: Option<&str>,
    ) -> anyhow::Result<u32> {
        let mut s = PacketStream::new();
        s.write_u32(0) // pFrom
            .write_u32(0) // pTo
            .write_bool(false) // dev
            .write_bytes_sized(&[0x00, 0x16, 0x3e, 0x00, 0x00, 0x01]) // mac
            .write_str(&trion_ticket(user, password))
            .write_str("") // signature
            .write_bool(true); // isLast
        self.send(ca::REQUEST_AUTH_TRION, s.as_bytes()).await?;

        let (mut op, mut packet) = self.recv().await?;
        match op {
            ac::ENTER_OTP => {
                let Some(otp) = otp else {
                    bail!("login server requested an OTP (2FA); pass --otp <code>");
                };
                self.submit_otp(otp).await?;
                (op, packet) = self.recv().await?;
            }
            ac::ENTER_PC_CERT => {
                bail!(
                    "login server requested a PC certificate PIN; the testclient supports OTP only"
                )
            }
            ac::SHOW_ARS => {
                bail!("login server requested ARS phone verification; the testclient supports OTP only")
            }
            ac::LOGIN_DENIED => {
                bail!("login denied (reason {})", packet.read_u8().unwrap_or(0));
            }
            ac::JOIN_RESPONSE => {}
            other => bail!(
                "expected {} but got {} (0x{other:03x})",
                ac_name(ac::JOIN_RESPONSE),
                ac_name(other)
            ),
        }
        if op == ac::LOGIN_DENIED {
            bail!(
                "login denied after the OTP (reason {})",
                packet.read_u8().unwrap_or(0)
            );
        }
        if op != ac::JOIN_RESPONSE {
            bail!(
                "expected {} after the OTP but got {} (0x{op:03x})",
                ac_name(ac::JOIN_RESPONSE),
                ac_name(op)
            );
        }
        let mut join = packet;
        let reason = join.read_u16()?;
        if reason != 0 {
            bail!("ACJoinResponse reason {reason}");
        }
        let mut auth = self.expect(ac::AUTH_RESPONSE).await?;
        self.account_id = auth.read_u32()?;
        Ok(self.account_id)
    }

    /// `CARequestReconnectPacket` -> `ACJoinResponsePacket` + `ACAuthResponsePacket`.
    pub async fn auth_reconnect(
        &mut self,
        account_id: u32,
        gs_id: u8,
        cookie: u32,
    ) -> anyhow::Result<u32> {
        let mut s = PacketStream::new();
        s.write_u32(0) // pFrom
            .write_u32(0) // pTo
            .write_u32(account_id)
            .write_u8(gs_id)
            .write_u32(cookie)
            .write_u16(0); // mac
        self.send(ca::REQUEST_RECONNECT, s.as_bytes()).await?;
        let mut join = self.expect(ac::JOIN_RESPONSE).await?;
        let reason = join.read_u16()?;
        if reason != 0 {
            bail!("ACJoinResponse reason {reason}");
        }
        let mut auth = self.expect(ac::AUTH_RESPONSE).await?;
        self.account_id = auth.read_u32()?;
        Ok(self.account_id)
    }

    /// `CAListWorldPacket` -> `ACWorldListPacket`.
    pub async fn list_worlds(&mut self) -> anyhow::Result<WorldList> {
        let mut s = PacketStream::new();
        s.write_u64(0);
        self.send(ca::LIST_WORLD, s.as_bytes()).await?;
        let mut r = self.expect(ac::WORLD_LIST).await?;
        let mut out = WorldList::default();
        let n = r.read_u8()?;
        for _ in 0..n {
            let id = r.read_u8()?;
            let name = r.read_string()?;
            let active = r.read_bool()?;
            if active {
                r.read_bytes(10)?; // con + 9 race congestion values
            }
            out.worlds.push(World { id, name, active });
        }
        let n = r.read_u8()?;
        for _ in 0..n {
            let account_id = r.read_u32()?;
            let gs_id = r.read_u8()?;
            let id = r.read_u32()?;
            let name = r.read_string()?;
            r.read_bytes(2)?; // race, gender
            r.read_bytes_sized()?; // guid
            r.read_u64()?; // v
            out.characters.push(LoginCharacter {
                account_id,
                gs_id,
                id,
                name,
            });
        }
        Ok(out)
    }

    /// `CAEnterWorldPacket` -> `ACWorldQueuePacket`* -> `ACWorldCookiePacket` (or
    /// `ACEnterWorldDeniedPacket`). A full world queues the client; this waits for the world
    /// cookie, recording every queue update and printing its position.
    pub async fn enter_world(&mut self, gs_id: u8) -> anyhow::Result<WorldCookie> {
        let mut s = PacketStream::new();
        s.write_u64(0).write_u8(gs_id);
        self.send(ca::ENTER_WORLD, s.as_bytes()).await?;
        self.queue_updates.clear();
        // A queued client may wait minutes; the handshake timeout is unaffected. The deadline
        // only starts after the first queue update, so an unqueued enter still uses
        // `self.timeout` exactly as before.
        let queue_deadline = tokio::time::Instant::now() + self.queue_timeout;
        let mut queued = false;
        loop {
            let timeout = if queued {
                queue_deadline.saturating_duration_since(tokio::time::Instant::now())
            } else {
                self.timeout
            };
            if timeout.is_zero() {
                bail!("timed out waiting for the world queue");
            }
            let (op, mut r) = self.recv_with_timeout(timeout).await?;
            match op {
                ac::WORLD_QUEUE => {
                    let update = WorldQueueUpdate {
                        world_id: r.read_u8()?,
                        is_premium: r.read_bool()?,
                        my_turn: r.read_u16()?,
                        normal_length: r.read_u16()?,
                        premium_length: r.read_u16()?,
                    };
                    println!(
                        "world queue: world {} position {}/{} (premium {} {})",
                        update.world_id,
                        update.my_turn,
                        update.normal_length,
                        update.is_premium,
                        update.premium_length
                    );
                    self.queue_updates.push(update);
                    queued = true;
                }
                ac::ENTER_WORLD_DENIED => {
                    let reason = r.read_u8().unwrap_or(0);
                    bail!("enter world denied (reason {reason})");
                }
                ac::WORLD_COOKIE => {
                    let cookie = r.read_u32()?;
                    let ip = r.read_bytes(4)?;
                    let port = r.read_u16()?;
                    return Ok(WorldCookie {
                        cookie,
                        // The address is written with its octets reversed.
                        ip: Ipv4Addr::new(ip[3], ip[2], ip[1], ip[0]),
                        port,
                    });
                }
                other => bail!("expected ACWorldCookiePacket, got {}", ac_name(other)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn queue_frame(world_id: u8, my_turn: u16, normal_length: u16) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(world_id);
        body.push(0); // isPremium
        body.extend(my_turn.to_le_bytes());
        body.extend(normal_length.to_le_bytes());
        body.extend(0u16.to_le_bytes()); // premiumLength
        let mut frame = Vec::new();
        frame.extend((body.len() as u16 + 2).to_le_bytes());
        frame.extend(ac::WORLD_QUEUE.to_le_bytes());
        frame.extend(body);
        frame
    }

    fn cookie_frame(cookie: u32) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(cookie.to_le_bytes());
        body.extend([1, 0, 0, 127]); // the client reverses the octets
        body.extend(1239u16.to_le_bytes());
        let mut frame = Vec::new();
        frame.extend((body.len() as u16 + 2).to_le_bytes());
        frame.extend(ac::WORLD_COOKIE.to_le_bytes());
        frame.extend(body);
        frame
    }

    async fn accept_enter_world(listener: TcpListener) -> tokio::net::TcpStream {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut len = [0u8; 2];
        sock.read_exact(&mut len).await.unwrap();
        let mut payload = vec![0u8; u16::from_le_bytes(len) as usize];
        sock.read_exact(&mut payload).await.unwrap();
        assert_eq!(
            u16::from_le_bytes([payload[0], payload[1]]),
            ca::ENTER_WORLD
        );
        sock
    }

    #[tokio::test]
    async fn enter_world_records_queue_updates_without_changing_the_handshake_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let mut sock = accept_enter_world(listener).await;
            sock.write_all(&queue_frame(1, 1, 1)).await.unwrap();
            sock.write_all(&queue_frame(1, 0, 0)).await.unwrap();
            sock.write_all(&cookie_frame(42)).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });

        let mut client = LoginClient::connect(&addr, Recorder::new(true))
            .await
            .unwrap();
        let handshake_timeout = client.timeout;
        let cookie = client.enter_world(1).await.unwrap();
        assert_eq!(cookie.cookie, 42);
        assert_eq!(cookie.ip, Ipv4Addr::new(127, 0, 0, 1));
        assert_eq!(cookie.port, 1239);
        assert_eq!(
            client.timeout, handshake_timeout,
            "the queue must not rewrite the client's handshake timeout"
        );
        assert_eq!(
            client.queue_updates,
            vec![
                WorldQueueUpdate {
                    world_id: 1,
                    is_premium: false,
                    my_turn: 1,
                    normal_length: 1,
                    premium_length: 0,
                },
                WorldQueueUpdate {
                    world_id: 1,
                    is_premium: false,
                    my_turn: 0,
                    normal_length: 0,
                    premium_length: 0,
                },
            ]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn enter_world_uses_the_queue_deadline_only_after_a_queue_update() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let mut sock = accept_enter_world(listener).await;
            sock.write_all(&queue_frame(1, 1, 1)).await.unwrap();
            // The world cookie never comes; the client must give up at its queue deadline, not
            // at the 15 s handshake timeout.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let mut client = LoginClient::connect(&addr, Recorder::new(true))
            .await
            .unwrap();
        client.queue_timeout = Duration::from_millis(50);
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(2), client.enter_world(1))
            .await
            .expect("the queue deadline must end the wait before the handshake timeout")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("timed out waiting for a login packet"),
            "unexpected error: {error}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(client.queue_updates.len() == 1);
        server.abort();
    }

    #[test]
    fn ticket_contains_hashed_password() {
        let t = trion_ticket("bob", "secret");
        assert!(t.contains("<username>bob</username>"));
        assert!(t.contains(
            "<password>2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b</password>"
        ));
    }
}
