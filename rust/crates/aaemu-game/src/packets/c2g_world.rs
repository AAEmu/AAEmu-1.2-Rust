//! C->G packets of the character lifecycle and the world (`AAEmu.Game.Core.Packets.C2G`):
//! create / delete / restore / select / spawn / notify-in-game, movement, chat and leaving.
//! The behaviour lives in [`crate::managers::character`] and [`crate::world::session`].

use aaemu_commons::PacketStream;
use futures::future::BoxFuture;

use crate::error::Result;
use crate::managers::character::{self, CreateCharacterRequest};
use crate::models::char_data::VisualOptions;
use crate::models::movement::MoveType;
use crate::models::unit_model::UnitCustomModelParams;
use crate::net::connection::GameConnection;
use crate::net::registry::ClientPacket;
use crate::server::GameServer;
use crate::world::session;

/// `CSCreateCharacterPacket` (level 1).
#[derive(Debug, Clone, PartialEq)]
pub struct CSCreateCharacterPacket(pub CreateCharacterRequest);

impl ClientPacket for CSCreateCharacterPacket {
    const NAME: &'static str = "CSCreateCharacterPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        let name = s.read_string()?;
        let race = s.read_u8()?;
        let gender = s.read_u8()?;
        let mut body_items = [0u32; 7];
        for i in body_items.iter_mut() {
            *i = s.read_u32()?;
        }
        let model_params: UnitCustomModelParams = s.read()?;
        Ok(Self(CreateCharacterRequest {
            name,
            race,
            gender,
            body_items,
            model_params,
            ability1: s.read_u8()?,
            ability2: s.read_u8()?,
            ability3: s.read_u8()?,
            level: s.read_u8()?,
        }))
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // A validated enter-world ticket installs the account id. New TCP connections
            // start in Lobby too, so the state alone does not prove authentication.
            if conn.account_id() == 0 {
                conn.shutdown();
                return Ok(());
            }
            character::create(server, conn, self.0).await
        })
    }
}

/// `CSEditCharacterPacket` (level 1, 0x023): the pre-select lobby edit. The body is
/// `CSCreateCharacterPacket`'s with a leading character id and no `introZoneId` (1.2 client
/// serializer `FUN_397c4da0`, writer `FUN_397ae360`).
#[derive(Debug, Clone, PartialEq)]
pub struct CSEditCharacterPacket {
    pub character_id: u32,
    pub request: CreateCharacterRequest,
}

impl ClientPacket for CSEditCharacterPacket {
    const NAME: &'static str = "CSEditCharacterPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        let character_id = s.read_u32()?;
        let name = s.read_string()?;
        let race = s.read_u8()?;
        let gender = s.read_u8()?;
        let mut body_items = [0u32; 7];
        for i in body_items.iter_mut() {
            *i = s.read_u32()?;
        }
        let model_params: UnitCustomModelParams = s.read()?;
        Ok(Self {
            character_id,
            request: CreateCharacterRequest {
                name,
                race,
                gender,
                body_items,
                model_params,
                ability1: s.read_u8()?,
                ability2: s.read_u8()?,
                ability3: s.read_u8()?,
                level: s.read_u8()?,
            },
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // A validated enter-world ticket installs the account id (new TCP connections start
            // in Lobby too, so the state alone does not prove authentication).
            if conn.account_id() == 0 {
                conn.shutdown();
                return Ok(());
            }
            character::edit(server, conn, self.character_id, self.request).await
        })
    }
}

/// `CSDeleteCharacterPacket` (level 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSDeleteCharacterPacket {
    pub character_id: u32,
}

impl ClientPacket for CSDeleteCharacterPacket {
    const NAME: &'static str = "CSDeleteCharacterPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            character_id: s.read_u32()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(
            async move { character::set_delete_character(server, conn, self.character_id).await },
        )
    }
}

/// `CSCancelCharacterDeletePacket` (level 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSCancelCharacterDeletePacket {
    pub character_id: u32,
}

impl ClientPacket for CSCancelCharacterDeletePacket {
    const NAME: &'static str = "CSCancelCharacterDeletePacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            character_id: s.read_u32()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(
            async move { character::set_restore_character(server, conn, self.character_id).await },
        )
    }
}

/// `CSSelectCharacterPacket` (level 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSelectCharacterPacket {
    pub character_id: u32,
    pub gm: bool,
    pub unk: u8,
}

impl ClientPacket for CSSelectCharacterPacket {
    const NAME: &'static str = "CSSelectCharacterPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            character_id: s.read_u32()?,
            gm: s.read_bool()?,
            unk: s.read_u8()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { session::select_character(server, conn, self.character_id).await })
    }
}

/// `CSSpawnCharacterPacket` (level 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSpawnCharacterPacket {
    pub visual_options: VisualOptions,
}

impl ClientPacket for CSSpawnCharacterPacket {
    const NAME: &'static str = "CSSpawnCharacterPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            visual_options: VisualOptions::read(s)?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            session::spawn_character(server, conn, self.visual_options);
            Ok(())
        })
    }
}

/// `CSNotifyInGamePacket` (level 1, no data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSNotifyInGamePacket;

impl ClientPacket for CSNotifyInGamePacket {
    const NAME: &'static str = "CSNotifyInGamePacket";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            session::notify_in_game(server, conn);
            Ok(())
        })
    }
}

/// `CSNotifyInGameCompletedPacket` (level 1, no data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSNotifyInGameCompletedPacket;

impl ClientPacket for CSNotifyInGameCompletedPacket {
    const NAME: &'static str = "CSNotifyInGameCompletedPacket";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            session::notify_in_game_completed(server, conn);
            Ok(())
        })
    }
}

/// `CSMoveUnitPacket` (level 1).
#[derive(Debug, Clone)]
pub struct CSMoveUnitPacket {
    pub obj_id: u32,
    pub move_type: MoveType,
}

impl ClientPacket for CSMoveUnitPacket {
    const NAME: &'static str = "CSMoveUnitPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        let obj_id = s.read_bc()?;
        let ty = s.read_u8()?;
        Ok(Self {
            obj_id,
            move_type: MoveType::read(ty, s)?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            session::move_unit(server, conn, self.obj_id, self.move_type).await;
            Ok(())
        })
    }
}

/// `CSSendChatMessagePacket` (level 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSSendChatMessagePacket {
    pub chat_type: i16,
    pub unk1: i16,
    pub unk2: i32,
    pub target_name: String,
    pub message: String,
    pub language_type: u8,
    pub ability: i32,
}

impl ClientPacket for CSSendChatMessagePacket {
    const NAME: &'static str = "CSSendChatMessagePacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            chat_type: s.read_i16()?,
            unk1: s.read_i16()?,
            unk2: s.read_i32()?,
            target_name: s.read_string()?,
            message: s.read_string()?,
            language_type: s.read_u8()?,
            ability: s.read_i32()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if self.chat_type == crate::systems::social::chat::chat_type::USER {
                crate::systems::social::packets::chat::send_user_chat(server, conn, self).await;
                return Ok(());
            }
            session::send_chat_message(
                server,
                conn,
                self.chat_type,
                &self.target_name,
                &self.message,
                self.language_type,
                self.ability,
            )
            .await;
            Ok(())
        })
    }
}

/// `CSCancelLeaveWorldPacket` (level 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSCancelLeaveWorldPacket;

impl ClientPacket for CSCancelLeaveWorldPacket {
    const NAME: &'static str = "CSCancelLeaveWorldPacket";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        _server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            session::cancel_leave(conn);
            Ok(())
        })
    }
}

/// Registers the C->S packets of this module (character create / edit / delete / select, spawn,
/// movement, chat).
pub fn register(r: &mut crate::net::registry::PacketRegistry) {
    use crate::net::packet::LEVEL_PLAIN;
    use crate::opcodes::cs;
    r.register::<CSCancelLeaveWorldPacket>(LEVEL_PLAIN, cs::CSCancelLeaveWorldPacket)
        .register::<CSCreateCharacterPacket>(LEVEL_PLAIN, cs::CSCreateCharacterPacket)
        .register::<CSEditCharacterPacket>(LEVEL_PLAIN, cs::CSEditCharacterPacket)
        .register::<CSDeleteCharacterPacket>(LEVEL_PLAIN, cs::CSDeleteCharacterPacket)
        .register::<CSCancelCharacterDeletePacket>(LEVEL_PLAIN, cs::CSCancelCharacterDeletePacket)
        .register::<CSSelectCharacterPacket>(LEVEL_PLAIN, cs::CSSelectCharacterPacket)
        .register::<CSSpawnCharacterPacket>(LEVEL_PLAIN, cs::CSSpawnCharacterPacket)
        .register::<CSNotifyInGamePacket>(LEVEL_PLAIN, cs::CSNotifyInGamePacket)
        .register::<CSNotifyInGameCompletedPacket>(LEVEL_PLAIN, cs::CSNotifyInGameCompletedPacket)
        .register::<CSMoveUnitPacket>(LEVEL_PLAIN, cs::CSMoveUnitPacket)
        .register::<CSSendChatMessagePacket>(LEVEL_PLAIN, cs::CSSendChatMessagePacket);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aaemu_commons::Marshal;

    #[tokio::test]
    async fn unauthenticated_create_character_does_not_allocate_character_assets() {
        let server = crate::test_support::test_server();
        let mut client = crate::test_support::TestClient::new(&server, 1, 0);
        assert_eq!(client.conn.account_id(), 0);
        let containers_before = server.items.all_containers().len();
        let request = CSCreateCharacterPacket(CreateCharacterRequest {
            name: "Noauth".into(),
            race: 1,
            gender: 1,
            body_items: [0; 7],
            model_params: UnitCustomModelParams::default(),
            ability1: 1,
            ability2: 11,
            ability3: 11,
            level: 1,
        });
        request
            .handle(&server, &mut client.conn)
            .await
            .expect("rejected before DB access");
        assert!(client.conn.is_closing());
        assert!(client.conn.characters.is_empty());
        assert!(server.names.no_names_registered());
        assert!(!server.names.name_exists("Noauth"));
        assert_eq!(server.items.all_containers().len(), containers_before);
    }

    #[test]
    fn reads_create_character() {
        let mut s = PacketStream::new();
        s.write_str("Abc").write_u8(1).write_u8(2);
        for i in 0..7u32 {
            s.write_u32(i);
        }
        UnitCustomModelParams::default().write(&mut s);
        s.write_u8(1).write_u8(11).write_u8(11).write_u8(1);
        let p = CSCreateCharacterPacket::read(&mut s).unwrap().0;
        assert_eq!(p.name, "Abc");
        assert_eq!(
            (p.race, p.gender, p.ability1, p.ability3, p.level),
            (1, 2, 1, 11, 1)
        );
        assert_eq!(p.body_items, [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(s.remaining(), 0);
    }

    #[tokio::test]
    async fn unauthenticated_edit_character_does_not_touch_the_lobby() {
        let server = crate::test_support::test_server();
        let mut client = crate::test_support::TestClient::new(&server, 1, 0);
        let packet = CSEditCharacterPacket {
            character_id: 1,
            request: CreateCharacterRequest {
                name: "Noauth".into(),
                race: 1,
                gender: 1,
                body_items: [0; 7],
                model_params: UnitCustomModelParams::default(),
                ability1: 1,
                ability2: 11,
                ability3: 11,
                level: 1,
            },
        };
        packet
            .handle(&server, &mut client.conn)
            .await
            .expect("rejected before any lookup");
        assert!(client.conn.is_closing());
        assert!(client.drain().is_empty());
        assert!(client.conn.characters.is_empty());
    }

    #[test]
    fn reads_edit_character_as_the_create_body_with_a_leading_id() {
        // 1.2 client serializer `FUN_397c4da0`: id, name, race, gender, 7 body items, the
        // customizing blob, 3 abilities, level, and no `introZoneId`.
        let mut s = PacketStream::new();
        s.write_u32(0x0102_0304)
            .write_str("Abc")
            .write_u8(1)
            .write_u8(2);
        for i in 0..7u32 {
            s.write_u32(i + 10);
        }
        UnitCustomModelParams::default().write(&mut s);
        s.write_u8(1).write_u8(11).write_u8(11).write_u8(1);
        let p = CSEditCharacterPacket::read(&mut s).unwrap();
        assert_eq!(p.character_id, 0x0102_0304);
        assert_eq!(p.request.name, "Abc");
        assert_eq!(
            (
                p.request.race,
                p.request.gender,
                p.request.ability1,
                p.request.ability3,
                p.request.level
            ),
            (1, 2, 1, 11, 1)
        );
        assert_eq!(p.request.body_items, [10, 11, 12, 13, 14, 15, 16]);
        assert_eq!(s.remaining(), 0);

        // A truncated body is a read error (the previous log-only handler accepted anything).
        let mut s = PacketStream::from_bytes(vec![1, 0, 0, 0]);
        assert!(CSEditCharacterPacket::read(&mut s).is_err());
    }

    #[test]
    fn create_body_ends_with_intro_zone_id_that_the_reader_ignores() {
        // 1.2 create writer `FUN_397ae280` passes -1 as `introZoneId` (`FUN_397c4bf0`).
        let mut s = PacketStream::new();
        s.write_str("Abc").write_u8(1).write_u8(2);
        for i in 0..7u32 {
            s.write_u32(i);
        }
        UnitCustomModelParams::default().write(&mut s);
        s.write_u8(1)
            .write_u8(11)
            .write_u8(11)
            .write_u8(1)
            .write_i32(-1);
        let p = CSCreateCharacterPacket::read(&mut s).unwrap().0;
        assert_eq!(p.name, "Abc");
        assert_eq!(s.remaining(), 4);
    }

    #[test]
    fn reads_select_chat_and_move() {
        let mut s = PacketStream::from_bytes(vec![5, 0, 0, 0, 1, 0]);
        assert_eq!(
            CSSelectCharacterPacket::read(&mut s).unwrap(),
            CSSelectCharacterPacket {
                character_id: 5,
                gm: true,
                unk: 0
            }
        );
        let mut s = PacketStream::new();
        s.write_i16(0)
            .write_i16(0)
            .write_i32(0)
            .write_str("")
            .write_str("hello")
            .write_u8(0)
            .write_i32(3);
        let c = CSSendChatMessagePacket::read(&mut s).unwrap();
        assert_eq!((c.message.as_str(), c.ability), ("hello", 3));
        let mut s = PacketStream::new();
        s.write_bc(0x123).write_u8(5); // Transfer move type, truncated body
        assert!(CSMoveUnitPacket::read(&mut s).is_err());
    }
}
