//! Dungeon clear notice: when the final boss configured for an instance world dies, the boss
//! says a line in local NPC chat and every player in that instance gets an on-screen notice.
//!
//! The bosses and their texts come from `Data/indun_clear_notices.json`; a missing file turns
//! the feature off. The hook is the NPC death path in [`super::events::on_do_die`].

use std::path::Path;

use crate::server::GameServer;
use crate::systems::npcs::json;
use crate::systems::npcs::npc::packets::{npc_chat_name, SCNpcChatMessagePacket};
use crate::systems::services::commands::output::Color;
use crate::systems::services::shutdown::SCNoticeMessagePacket;
use crate::world::WorldObject;

/// One `Data/indun_clear_notices.json` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearNotice {
    /// World template name the boss belongs to (`instance_cuttingwind_deadmine`, ...).
    pub world: String,
    /// Final boss NPC template id.
    pub npc_template_id: u32,
    /// The line the boss says and the text of the on-screen notice.
    pub text: String,
}

/// `ChatType.White` (say).
const CHAT_TYPE_SAY: i16 = 0;
/// `Color.Gold`.
const GOLD: Color = Color::argb(255, 255, 215, 0);
/// How long the on-screen notice stays up.
const NOTICE_VIS_TIME_MS: i32 = 8000;
/// The `SCNoticeMessagePacket` type the GM `announce` command uses by default.
const NOTICE_KIND: u8 = 3;

/// Loads `<app>/Data/indun_clear_notices.json`. A missing file means no notices; a bad entry is
/// skipped.
pub fn load(data_folder: &Path) -> Vec<ClearNotice> {
    let path = data_folder.join("indun_clear_notices.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        tracing::debug!(
            "Missing file: {} (dungeon clear notices off)",
            path.display()
        );
        return Vec::new();
    };
    if contents.trim().is_empty() {
        tracing::warn!("File {} is empty.", path.display());
        return Vec::new();
    }
    match parse(contents.trim_start_matches('\u{feff}')) {
        Ok(list) => {
            tracing::info!("Loaded {} dungeon clear notices", list.len());
            list
        }
        Err(e) => {
            tracing::error!("Error parsing {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// Parses the list (`[{ "World", "NpcTemplateId", "Text" }]`); a bad entry is skipped with a
/// warning.
pub fn parse(text: &str) -> Result<Vec<ClearNotice>, String> {
    let v = json::parse(text).map_err(|e| e.to_string())?;
    let items = json::array(&v)?;
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            tracing::warn!("indun_clear_notices.json entry {i}: expected an object");
            continue;
        };
        let entry = (|| -> Result<ClearNotice, String> {
            Ok(ClearNotice {
                world: json::str_or(obj, "World", "")?,
                npc_template_id: json::u32_or(obj, "NpcTemplateId", 0)?,
                text: json::str_or(obj, "Text", "")?,
            })
        })();
        match entry {
            Ok(n) if n.world.is_empty() || n.text.is_empty() || n.npc_template_id == 0 => {
                tracing::warn!(
                    "indun_clear_notices.json entry {i}: missing World / NpcTemplateId / Text"
                );
            }
            Ok(n) => out.push(n),
            Err(e) => tracing::warn!("indun_clear_notices.json entry {i}: {e}"),
        }
    }
    Ok(out)
}

/// The NPC death hook ([`super::events::on_do_die`]): when the dying NPC is the final boss of
/// the instance world it stands in, the boss says its line to the players around it and every
/// player in the instance gets the on-screen notice. Runs once per NPC death (the death itself
/// is claimed once per life).
pub fn on_npc_died(server: &GameServer, victim: &WorldObject) {
    let Some(npc) = victim.as_npc() else {
        return;
    };
    let Some(world) = server.world.world_of(victim) else {
        return;
    };
    let template_id = npc.data().unit.template_id;
    let Some(notice) = server
        .game_data
        .systems
        .instances
        .clear_notices
        .iter()
        .find(|n| n.npc_template_id == template_id && n.world == world.template.name)
    else {
        return;
    };
    let (npc_name, faction_id) = {
        let n = npc.data();
        (n.unit.name.clone(), n.unit.faction_id)
    };
    let name = npc_chat_name(server, &npc_name, template_id);
    for p in world.players_around(victim) {
        p.send(SCNpcChatMessagePacket {
            chat_type: CHAT_TYPE_SAY,
            faction_id,
            npc_obj_id: npc.obj_id,
            npc_name: name.clone(),
            character_obj_id: p.obj_id,
            kind: 0,
            type_: 0,
            message: notice.text.clone(),
        });
    }
    for p in world.all_characters() {
        p.send(SCNoticeMessagePacket::new(
            NOTICE_KIND,
            GOLD,
            NOTICE_VIS_TIME_MS,
            &notice.text,
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use aaemu_commons::PacketStream;

    use super::*;
    use crate::data::world_templates::WorldTemplate;
    use crate::data::GameData;
    use crate::models::npc::Npc;
    use crate::opcodes::sc;
    use crate::systems::skills::types::KillReason;
    use crate::test_support::{test_character, test_server_with, TestClient, TEST_ZONE};
    use crate::world::{Entity, NpcRef, Player, PlayerRef, WorldInstance};

    const BOSS: u32 = 12188; // Okape the Slayer (Sharpwind Mines)
    const TEXT: &str = "Thanks for testing Raziel's code!";
    const WORLD: &str = "instance_cuttingwind_deadmine";

    fn data_with_notice() -> GameData {
        let mut data = GameData::empty();
        data.systems.instances.clear_notices = vec![ClearNotice {
            world: WORLD.into(),
            npc_template_id: BOSS,
            text: TEXT.into(),
        }];
        data
    }

    fn instance(server: &GameServer, name: &str, id: u32) -> Arc<WorldInstance> {
        server.world.create_world_instance(
            Arc::new(WorldTemplate::synthetic(id, name, 2, 2, TEST_ZONE)),
            id,
        )
    }

    fn player_in(
        server: &GameServer,
        client: &TestClient,
        world: &WorldInstance,
        obj_id: u32,
        x: f32,
        y: f32,
    ) -> PlayerRef {
        let mut ch = test_character(obj_id, "Alpha", x, y);
        ch.obj_id = obj_id;
        let p = Player::new(ch, client.conn.handle());
        server.world.try_add_character(&p);
        world.spawn(&p, &server.view_ctx());
        p
    }

    fn boss_in(
        server: &GameServer,
        world: &WorldInstance,
        obj_id: u32,
        template_id: u32,
    ) -> NpcRef {
        let mut npc = Npc::new(template_id);
        npc.unit.name = "Okape".into();
        npc.unit.faction_id = 0x10094;
        npc.unit.transform.set_xyz(105.0, 100.0, 100.0);
        let npc = Entity::new(obj_id, npc);
        world.spawn(&npc, &server.view_ctx());
        npc
    }

    /// The real `Unit.DoDie` path the server runs for a lethal hit.
    fn kill(server: &GameServer, npc: &NpcRef, killer: &PlayerRef) {
        npc.data().unit.hp = 0;
        crate::systems::skills::combat::do_die(
            server,
            &WorldObject::from(npc),
            Some(&WorldObject::from(killer)),
            KillReason::Damage,
        );
    }

    /// The body of the one `SCNpcChatMessagePacket` in `frames`, decoded field by field.
    fn chat_body(frames: &[(u16, Vec<u8>)]) -> Option<Vec<u8>> {
        frames
            .iter()
            .find(|(op, _)| *op == sc::SCNpcChatMessagePacket)
            .map(|(_, b)| b.clone())
    }

    fn notice_bodies(frames: &[(u16, Vec<u8>)]) -> Vec<Vec<u8>> {
        frames
            .iter()
            .filter(|(op, _)| *op == sc::SCNoticeMessagePacket)
            .map(|(_, b)| b.clone())
            .collect()
    }

    /// The final boss's death says the line to the players around it and sends the notice to
    /// every player in the instance (a player across the map gets no chat but the notice).
    #[tokio::test]
    async fn final_boss_death_sends_the_npc_line_and_the_instance_notice() {
        let server = test_server_with(data_with_notice());
        let world = instance(&server, WORLD, 5);
        let mut near = TestClient::new(&server, 1, 1001);
        let mut far = TestClient::new(&server, 2, 1002);
        let pa = player_in(&server, &near, &world, 0x100, 100.0, 100.0);
        let pb = player_in(&server, &far, &world, 0x101, 1500.0, 1500.0);
        let npc = boss_in(&server, &world, 0x200, BOSS);
        near.drain();
        far.drain();

        kill(&server, &npc, &pa);

        let frames = near.drain();
        let body = chat_body(&frames).expect("the boss said its line");
        let mut s = PacketStream::from_bytes(body);
        assert_eq!(s.read_i16().unwrap(), 0, "ChatType.White");
        assert_eq!(s.read_i16().unwrap(), 0x0094);
        assert_eq!(s.read_u32().unwrap(), 0x10094);
        assert_eq!(s.read_bc().unwrap(), npc.obj_id);
        assert_eq!(s.read_string().unwrap(), "Okape");
        assert_eq!(s.read_bc().unwrap(), pa.obj_id);
        assert_eq!(s.read_u8().unwrap(), 0);
        assert_eq!(s.read_string().unwrap(), TEXT);
        assert_eq!(s.remaining(), 0);

        let notices = notice_bodies(&frames);
        assert_eq!(notices.len(), 1, "one notice for the nearby player");
        let mut s = PacketStream::from_bytes(notices[0].clone());
        assert_eq!(s.read_u8().unwrap(), 3);
        assert_eq!(s.read_string().unwrap(), "FF");
        assert_eq!(s.read_i32().unwrap(), 8000);
        assert_eq!(s.read_string().unwrap(), format!("FFD700{TEXT}"));
        assert_eq!(s.remaining(), 0);

        let far_frames = far.drain();
        assert!(chat_body(&far_frames).is_none(), "chat is local");
        let far_notices = notice_bodies(&far_frames);
        assert_eq!(
            far_notices.len(),
            1,
            "the notice reaches the whole instance"
        );
        assert_eq!(far_notices, notices);
        assert!(pb.data().unit.transform.instance_id == world.id);
    }

    /// Another NPC's death in the dungeon world, and the boss's death outside its world, send
    /// no chat line and no notice.
    #[tokio::test]
    async fn other_npcs_and_other_worlds_send_nothing() {
        let server = test_server_with(data_with_notice());
        let world = instance(&server, WORLD, 5);
        let mut client = TestClient::new(&server, 1, 1001);
        let p = player_in(&server, &client, &world, 0x100, 100.0, 100.0);
        // A matching world with a different NPC.
        let other = boss_in(&server, &world, 0x200, BOSS + 1);
        client.drain();
        kill(&server, &other, &p);
        let frames = client.drain();
        assert!(chat_body(&frames).is_none());
        assert!(notice_bodies(&frames).is_empty());

        // The right NPC in another world.
        let main = server.world.world(0).expect("main_world");
        let mut main_client = TestClient::new(&server, 2, 1002);
        let p2 = player_in(&server, &main_client, &main, 0x101, 100.0, 100.0);
        let boss = boss_in(&server, &main, 0x201, BOSS);
        main_client.drain();
        kill(&server, &boss, &p2);
        let frames = main_client.drain();
        assert!(chat_body(&frames).is_none());
        assert!(notice_bodies(&frames).is_empty());
    }

    #[test]
    fn a_bad_entry_is_skipped() {
        let list = parse(
            r#"[{"World": "instance_a", "NpcTemplateId": 1, "Text": "A"},
                {"World": "instance_b", "NpcTemplateId": "x", "Text": "B"},
                {"World": "", "NpcTemplateId": 3, "Text": "C"},
                {"World": "instance_d", "NpcTemplateId": 4, "Text": "D"}]"#,
        )
        .unwrap();
        assert_eq!(
            list,
            vec![
                ClearNotice {
                    world: "instance_a".into(),
                    npc_template_id: 1,
                    text: "A".into()
                },
                ClearNotice {
                    world: "instance_d".into(),
                    npc_template_id: 4,
                    text: "D".into()
                }
            ]
        );
    }

    #[test]
    fn a_missing_file_turns_the_feature_off() {
        let dir = std::env::temp_dir().join(format!("aaemu-clear-notice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load(&dir).is_empty());
    }

    /// The shipped data names the four final bosses (the NPCs whose `OnDeath` np_skill opens the
    /// exit) in their instance worlds.
    #[test]
    fn shipped_data_has_the_four_final_bosses() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("app/Data");
        let mut list = load(&dir);
        list.sort_by(|a, b| a.world.cmp(&b.world));
        assert_eq!(
            list.iter()
                .map(|n| (n.world.as_str(), n.npc_template_id, n.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("instance_burntcastle_armory", 8962, TEXT),
                ("instance_cradle_of_destruction", 11171, TEXT),
                ("instance_cuttingwind_deadmine", 12188, TEXT),
                ("instance_hadir_farm", 9798, TEXT),
            ]
        );
    }
}
