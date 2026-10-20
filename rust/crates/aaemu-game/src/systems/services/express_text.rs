//! `Core/Managers/ExpressTextManager` (`express_texts`: emotion id -> animation id) and
//! `CSExpressEmotionPacket` / `SCEmotionExpressedPacket`.

use std::collections::HashMap;

use aaemu_commons::{Marshal, PacketStream};
use futures::future::BoxFuture;
use rusqlite::Connection;

use crate::data::sqlite::for_each_row;
use crate::error::Result;
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::packets::packet_def;
use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{broadcast, WorldObject};

/// `ExpressTextManager` data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpressTexts {
    /// `_expressTexts` (first row of an id wins).
    texts: HashMap<u32, u32>,
}

impl ExpressTexts {
    /// `Load()`: `SELECT * FROM express_texts`.
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        tracing::info!("Loading express text...");
        let mut texts = HashMap::new();
        for_each_row(conn, "SELECT * FROM express_texts", [], |r| {
            texts.entry(r.u32("id")?).or_insert(r.u32("anim_id")?);
            Ok(())
        })?;
        Ok(Self { texts })
    }

    /// `GetExpressAnimId(emotionId)` (0 when unknown).
    pub fn get_express_anim_id(&self, emotion_id: u32) -> u32 {
        self.texts.get(&emotion_id).copied().unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.texts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }
}

/// `SCEmotionExpressedPacket(objId, objId2, emotionId)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCEmotionExpressedPacket {
    pub obj_id: u32,
    pub obj_id2: u32,
    pub emotion_id: u32,
}
packet_def!(
    SCEmotionExpressedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCEmotionExpressedPacket
);
impl Marshal for SCEmotionExpressedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bc(self.obj_id)
            .write_bc(self.obj_id2)
            .write_u32(self.emotion_id);
    }
}

/// `CSExpressEmotionPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSExpressEmotionPacket {
    pub character_obj_id: u32,
    pub npc_obj_id: u32,
    pub emotion_id: u32,
}

impl ClientPacket for CSExpressEmotionPacket {
    const NAME: &'static str = "CSExpressEmotionPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            character_obj_id: s.read_bc()?,
            npc_obj_id: s.read_bc()?,
            emotion_id: s.read_u32()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::warn!(
                "ExpressEmotion, ObjId: {}, Obj2Id: {}, EmotionId: {}",
                self.character_obj_id,
                self.npc_obj_id,
                self.emotion_id
            );
            let Some(p) = conn.active_char.clone() else {
                return Ok(());
            };
            if self.character_obj_id != p.obj_id {
                tracing::warn!(
                    "ExpressEmotion for object {} sent by {} (object {}); ignored",
                    self.character_obj_id,
                    p.data().unit.name,
                    p.obj_id
                );
                return Ok(());
            }
            let me = WorldObject::from(&p);
            if let Some(world) = server.world.world_of(&me) {
                broadcast(
                    &world,
                    &me,
                    &SCEmotionExpressedPacket {
                        obj_id: self.character_obj_id,
                        obj_id2: self.npc_obj_id,
                        emotion_id: self.emotion_id,
                    },
                    true,
                );
            }
            let anim_id = server
                .game_data
                .systems
                .services
                .express_texts
                .get_express_anim_id(self.emotion_id);
            crate::systems::quests::events::on_express_fire(
                server,
                &p,
                anim_id,
                self.character_obj_id,
                self.npc_obj_id,
            );
            // NPC_EMOTION: the raw emotion id (value2) expressed at the npc template (value1).
            let npc_template = server
                .world
                .world_of(&me)
                .and_then(|w| w.npc(self.npc_obj_id))
                .map(|npc| npc.data().unit.template_id);
            if let Some(value1) = npc_template {
                crate::systems::report_achievement(
                    server,
                    &p,
                    AchievementInput::Increment {
                        kind_id: char_record_kind::NPC_EMOTION,
                        amount: 1,
                        matcher: RecordMatch::Value1And2 {
                            value1,
                            value2: self.emotion_id,
                        },
                    },
                );
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_and_lookup() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE express_texts (id INT, anim_id INT, name TEXT);
             INSERT INTO express_texts VALUES (1, 100, 'a'), (2, 200, 'b'), (1, 999, 'dup');",
        )
        .unwrap();
        let t = ExpressTexts::load(&c).unwrap();
        assert_eq!(t.get_express_anim_id(1), 100);
        assert_eq!(t.get_express_anim_id(2), 200);
        assert_eq!(t.get_express_anim_id(3), 0);
        let mut s = PacketStream::new();
        s.write_bc(0x10).write_bc(0x20).write_u32(5);
        let p = CSExpressEmotionPacket::read(&mut PacketStream::from_bytes(s.clone().into_bytes()))
            .unwrap();
        assert_eq!(
            (p.character_obj_id, p.npc_obj_id, p.emotion_id),
            (0x10, 0x20, 5)
        );
        assert_eq!(
            crate::net::packet::encode(&SCEmotionExpressedPacket {
                obj_id: 0x10,
                obj_id2: 0x20,
                emotion_id: 5
            })[6..]
                .to_vec(),
            s.into_bytes()
        );
    }

    #[tokio::test]
    async fn emote_for_another_object_id_is_ignored() {
        use crate::test_support::{spawn_player, test_character, test_server, TestClient};
        let server = test_server();
        let mut a = TestClient::new(&server, 1, 1001);
        let mut b = TestClient::new(&server, 2, 1002);
        let pa = spawn_player(&server, &a, test_character(1, "Alpha", 100.0, 100.0), 0x100);
        let _pb = spawn_player(&server, &b, test_character(2, "Beta", 105.0, 100.0), 0x101);
        a.conn.active_char = Some(pa);
        a.drain();
        b.drain();

        // Legitimate: own object id is broadcast to the other player.
        let ok = CSExpressEmotionPacket {
            character_obj_id: 0x100,
            npc_obj_id: 0,
            emotion_id: 5,
        };
        ok.handle(&server, &mut a.conn).await.unwrap();
        let expected = crate::net::packet::encode(&SCEmotionExpressedPacket {
            obj_id: 0x100,
            obj_id2: 0,
            emotion_id: 5,
        })[6..]
            .to_vec();
        assert_eq!(b.drain(), vec![(sc::SCEmotionExpressedPacket, expected)]);

        // The broadcast included the sender.
        a.drain();

        // Spoofed: another unit's object id is dropped; nobody receives anything.
        let spoof = CSExpressEmotionPacket {
            character_obj_id: 0x101,
            ..ok
        };
        spoof.handle(&server, &mut a.conn).await.unwrap();
        assert!(b.drain().is_empty());
        assert!(a.drain().is_empty());
    }
}
