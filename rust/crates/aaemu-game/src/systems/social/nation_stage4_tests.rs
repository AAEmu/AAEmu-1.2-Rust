//! Stage 4 of the player nations: rename (`RENAME-7`), national tax (`RENAME-8`) and the
//! nation monument (`RENAME-9`).

use aaemu_commons::PacketStream;
use chrono::Utc;

use super::nation::{self, Nation, NationRelation};
use super::packets::faction::SCNationShowRenameUIPacket;
use crate::data::character_templates::CharacterTemplate;
use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::data::GameData;
use crate::net::packet::encode_raw;
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::housing::data::SiegeZone;
use crate::systems::housing::dominion::state::{DominionState, TowerIdentity};
use crate::systems::npcs::doodad::templates::DoodadTemplate;
use crate::systems::social::expedition::{self, Expedition};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::PlayerRef;

const NATION: u32 = 9_000;
const OTHER_NATION: u32 = 9_001;
const EXPEDITION: u32 = 1_000;
const OTHER_EXPEDITION: u32 = 1_001;
const ZONE: u32 = 25;
const MONUMENT_DOODAD: u32 = 7_229;

fn data() -> GameData {
    let mut data = GameData::empty();
    for (id, mother, name) in [
        (101, 0, "Faction101"),
        (148, 0, "Faction148"),
        (149, 0, "Faction149"),
        (166, 0, "Faction166"),
        (177, 0, "Faction177"),
        (200, 0, "TakenStatic"),
    ] {
        data.factions.add(SystemFaction {
            id,
            mother_id: mother,
            name: name.into(),
            ..SystemFaction::default()
        });
    }
    data.factions.relations.push(FactionRelation {
        id: 148,
        id2: 166,
        state: 1,
        exp_time: 0,
    });
    data.zones.zones.insert(
        crate::test_support::TEST_ZONE,
        crate::data::world_data::Zone {
            id: 1,
            zone_key: crate::test_support::TEST_ZONE,
            group_id: ZONE,
            ..Default::default()
        },
    );
    data.characters.templates.insert(
        16 + 1,
        CharacterTemplate {
            race: 1,
            gender: 1,
            faction_id: 101,
            ..CharacterTemplate::default()
        },
    );
    // The six 1.2 castle zone groups carry one territory-statue doodad each (7229..7234);
    // zone 25 is the test fixture's dominion.
    data.systems.housing.castles.siege_zones.insert(
        ZONE,
        SiegeZone {
            id: 1,
            zone_group_id: ZONE,
            monument_doodad_id: MONUMENT_DOODAD,
            ..SiegeZone::default()
        },
    );
    data.systems.npcs.doodad.templates.insert(
        MONUMENT_DOODAD,
        DoodadTemplate {
            id: MONUMENT_DOODAD,
            ..DoodadTemplate::default()
        },
    );
    data.systems
        .npcs
        .doodad
        .template_order
        .push(MONUMENT_DOODAD);
    data
}

struct Fixture {
    server: GameServer,
    king_client: TestClient,
    subject_client: TestClient,
    king: PlayerRef,
    subject: PlayerRef,
}

impl Fixture {
    fn new() -> Self {
        let server = test_server_with(data());
        crate::systems::social::install_hooks(&server);
        let mut king_client = TestClient::new(&server, 1, 1001);
        let king = spawn_player(
            &server,
            &king_client,
            test_character(1, "King", 100.0, 100.0),
            0x100,
        );
        let mut subject_client = TestClient::new(&server, 2, 1002);
        let subject = spawn_player(
            &server,
            &subject_client,
            test_character(2, "Subject", 101.0, 100.0),
            0x101,
        );
        server.names.add_character(1, "King", 1001);
        server.names.add_character(2, "Subject", 1002);
        server.systems.social.nations.add(Nation {
            id: NATION,
            expedition_id: EXPEDITION,
            owner_character_id: 1,
            owner_name: "King".into(),
            name: "Kingdom".into(),
            created_at: 1,
            allow_change_name: 0,
            relations: vec![NationRelation {
                other_faction_id: 148,
                state: 1,
                expire_at: 0,
                next_state: 0,
            }],
            offline_members: Vec::new(),
        });
        let faction = SystemFaction {
            id: EXPEDITION,
            mother_id: NATION,
            name: "Guild".into(),
            owner_id: 1,
            owner_name: "King".into(),
            ..SystemFaction::default()
        };
        server.expeditions.add(Expedition {
            faction,
            members: vec![
                expedition::member_from_character(&server, EXPEDITION, &king, true),
                expedition::member_from_character(&server, EXPEDITION, &subject, false),
            ],
            policies: Vec::new(),
            removed_members: Vec::new(),
            is_disbanded: false,
        });
        // `spawn_player` uses explicit object ids; reserve them so the doodad allocator
        // cannot hand the same id to the monument.
        server.ids.object.initialize_with(&[0x100, 0x101]);
        king.data().unit.expedition_id = EXPEDITION;
        subject.data().unit.expedition_id = EXPEDITION;
        king.data().unit.faction_id = NATION;
        subject.data().unit.faction_id = NATION;
        king_client.conn.active_char = Some(king.clone());
        subject_client.conn.active_char = Some(subject.clone());
        king_client.drain();
        subject_client.drain();
        Self {
            server,
            king_client,
            subject_client,
            king,
            subject,
        }
    }

    fn give_dominion(&self, owner_expedition_id: u32) {
        let mut state = DominionState::unowned(
            ZONE,
            TowerIdentity {
                house_id: 500,
                instance_id: 0,
                guard_tower_setting_id: 6,
            },
        );
        state.owner_expedition_id = owner_expedition_id;
        state.ownership_generation = 1;
        state.reign_start_at = 1_000;
        self.server
            .systems
            .housing
            .dominions
            .restore(vec![state])
            .expect("dominion fixture");
    }

    async fn send(&mut self, opcode: u16, body: PacketStream) {
        self.server
            .handle_frame(
                &mut self.king_client.conn,
                &encode_raw(1, opcode, body.as_bytes()),
            )
            .await
            .expect("handler");
    }

    fn state(&self) -> DominionState {
        self.server
            .systems
            .housing
            .dominions
            .get(ZONE)
            .expect("dominion")
    }
}

fn rename_body(id: u32, name: &str, is_expedition: bool) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(id).write_str(name).write_bool(is_expedition);
    s
}

fn tax_body(zone: u16, rate: u32) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u16(zone).write_u32(rate);
    s
}

fn frames_of(frames: &[(u16, Vec<u8>)], opcode: u16) -> Vec<Vec<u8>> {
    frames
        .iter()
        .filter(|(op, _)| *op == opcode)
        .map(|(_, body)| body.clone())
        .collect()
}

fn error_of(frames: &[(u16, Vec<u8>)], opcode: u16) -> Option<i16> {
    frames_of(frames, opcode)
        .first()
        .map(|body| i16::from_le_bytes([body[0], body[1]]))
}

fn parse_renamed(body: &[u8]) -> (u32, String, bool) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_u32().expect("id"),
        s.read_string().expect("name"),
        s.read_bool().expect("byGm"),
    )
}

fn parse_retry(body: &[u8]) -> (bool, i16, String) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_bool().expect("xpdt"),
        s.read_i16().expect("error"),
        s.read_string().expect("name"),
    )
}

fn parse_tax_rate(body: &[u8]) -> (u16, i32, i32, i64) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_u16().expect("zone"),
        s.read_i32().expect("rate"),
        s.read_i32().expect("prev"),
        s.read_i64().expect("time"),
    )
}

fn parse_monument(body: &[u8]) -> (u16, i64, f32, f32, f32) {
    let mut s = PacketStream::from_bytes(body.to_vec());
    (
        s.read_u16().expect("zone"),
        s.read_i64().expect("db id"),
        (s.read_i64().expect("x") >> 32) as f32 / 4096.0,
        (s.read_i64().expect("y") >> 32) as f32 / 4096.0,
        s.read_f32().expect("z"),
    )
}

#[test]
fn nation_show_rename_ui_is_empty() {
    use crate::systems::social::packets::body;

    assert!(body(&SCNationShowRenameUIPacket {}).is_empty());
}

#[tokio::test]
async fn king_renames_the_nation_and_every_client_sees_it() {
    let mut f = Fixture::new();
    nation::offer_nation_rename_ui(&f.server, &f.king).expect("offer");
    let king_frames = f.king_client.drain();
    assert_eq!(king_frames.len(), 1);
    assert_eq!(king_frames[0].0, sc::SCNationShowRenameUIPacket);
    assert!(king_frames[0].1.is_empty());
    assert!(f.server.systems.social.nations.has_rename_offer(1, NATION));

    f.send(
        cs::CSRenameExpeditionPacket,
        rename_body(NATION, "NewKingdom", false),
    )
    .await;

    assert_eq!(
        f.server.systems.social.nations.get(NATION).unwrap().name,
        "NewKingdom"
    );
    assert!(!f.server.systems.social.nations.has_rename_offer(1, NATION));
    for client in [&mut f.king_client, &mut f.subject_client] {
        let frames = client.drain();
        let renamed = frames_of(&frames, sc::SCFactionRenamedPacket);
        assert_eq!(renamed.len(), 1, "every online client learns the new name");
        assert_eq!(
            parse_renamed(&renamed[0]),
            (NATION, "NewKingdom".into(), false)
        );
        // Literal layout: u32 id, 2-byte length + name, bool byGm.
        let mut expected = vec![0x28, 0x23, 0, 0, 10, 0];
        expected.extend_from_slice(b"NewKingdom");
        expected.push(0);
        assert_eq!(renamed[0], expected);
    }
}

#[tokio::test]
async fn nation_rename_requires_the_offer_and_uses_the_shared_namespace() {
    let mut f = Fixture::new();
    // A guild, a static faction and another nation occupy the shared namespace.
    let guild = SystemFaction {
        id: 7_000,
        mother_id: 101,
        name: "TakenGuild".into(),
        owner_id: 9,
        ..SystemFaction::default()
    };
    f.server.expeditions.add(Expedition {
        faction: guild,
        members: Vec::new(),
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    f.server.systems.social.nations.add(Nation {
        id: OTHER_NATION,
        expedition_id: 0,
        owner_character_id: 9,
        owner_name: "Other".into(),
        name: "TakenNation".into(),
        created_at: 1,
        allow_change_name: 0,
        relations: Vec::new(),
        offline_members: Vec::new(),
    });

    // Without a ShowRenameUI offer the request is refused before any check.
    f.send(
        cs::CSRenameExpeditionPacket,
        rename_body(NATION, "Fine", false),
    )
    .await;
    let frames = f.king_client.drain();
    assert_eq!(
        error_of(&frames, sc::SCErrorMsgPacket),
        Some(crate::systems::economy::error_message::ONLY_NATION_OWNER)
    );
    assert_eq!(
        f.server.systems.social.nations.get(NATION).unwrap().name,
        "Kingdom"
    );

    nation::offer_nation_rename_ui(&f.server, &f.king).expect("offer");
    f.king_client.drain();
    for (name, code) in [
        ("TakenGuild", 299i16),
        ("TakenNation", 299),
        ("TakenStatic", 299),
        (&"x".repeat(33), 297),
    ] {
        f.send(
            cs::CSRenameExpeditionPacket,
            rename_body(NATION, name, false),
        )
        .await;
        let frames = f.king_client.drain();
        let retry = frames_of(&frames, sc::SCFactionRetryRenamePacket);
        assert_eq!(retry.len(), 1, "{name}: one retry dialog");
        let (xpdt, error, echoed) = parse_retry(&retry[0]);
        assert!(!xpdt, "the nation retry re-opens the nation dialog");
        assert_eq!(error, code, "{name}");
        assert_eq!(echoed, name);
        assert_eq!(
            f.server.systems.social.nations.get(NATION).unwrap().name,
            "Kingdom",
            "{name}: nothing changed"
        );
    }
    // The rejected name keeps the offer usable; the corrected name succeeds.
    f.send(
        cs::CSRenameExpeditionPacket,
        rename_body(NATION, "Fine", false),
    )
    .await;
    assert_eq!(
        f.server.systems.social.nations.get(NATION).unwrap().name,
        "Fine"
    );
}

#[tokio::test]
async fn guild_creation_and_rename_see_the_nation_namespace() {
    let mut f = Fixture::new();
    // Guild creation checks the shared namespace before the party requirements.
    f.king.data().unit.expedition_id = 0;
    expedition::create_expedition(&f.server, &f.king, "Kingdom", 0).await;
    let frames = f.king_client.drain();
    assert_eq!(
        error_of(&frames, sc::SCErrorMsgPacket),
        Some(crate::systems::economy::error_message::EXPEDITION_NAME_EXIST)
    );
    assert_eq!(f.server.expeditions.all().len(), 1, "no guild was created");

    // Guild rename checks it too.
    f.king.data().unit.expedition_id = EXPEDITION;
    expedition::offer_expedition_rename_ui(&f.server, &f.king).expect("offer");
    f.king_client.drain();
    f.send(
        cs::CSRenameExpeditionPacket,
        rename_body(EXPEDITION, "Kingdom", true),
    )
    .await;
    let frames = f.king_client.drain();
    let retry = frames_of(&frames, sc::SCFactionRetryRenamePacket);
    assert_eq!(retry.len(), 1);
    let (xpdt, error, _) = parse_retry(&retry[0]);
    assert!(xpdt);
    assert_eq!(
        error,
        crate::systems::economy::error_message::EXPEDITION_NAME_EXIST
    );
    assert_eq!(
        f.server.expeditions.get(EXPEDITION).unwrap().faction.name,
        "Guild"
    );
}

#[tokio::test]
async fn independence_checks_guild_names_too() {
    let f = Fixture::new();
    f.server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: 7_001,
            mother_id: 101,
            name: "TakenGuild".into(),
            ..SystemFaction::default()
        },
        members: Vec::new(),
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    // A second guild of the king without a nation, so independence reaches the name check.
    let founder_guild = 7_002;
    f.server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: founder_guild,
            mother_id: 101,
            name: "Founders".into(),
            owner_id: 1,
            owner_name: "King".into(),
            ..SystemFaction::default()
        },
        members: vec![expedition::member_from_character(
            &f.server,
            founder_guild,
            &f.king,
            true,
        )],
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    f.king.data().unit.expedition_id = founder_guild;
    let error =
        nation::declare_independence(&f.server, &f.king, founder_guild, "TakenGuild", false)
            .expect_err("a guild already carries the name");
    assert_eq!(
        error,
        crate::systems::economy::error_message::FACTION_DUPLICATE_NAME
    );
}

#[tokio::test]
async fn national_tax_mirrors_the_client_validator_and_broadcasts() {
    let mut f = Fixture::new();
    f.give_dominion(EXPEDITION);

    // rate > 1000 -> 684 (`dominion_national_tax_invalid_rate`).
    f.send(
        cs::CSUpdateNationalTaxRatePacket,
        tax_body(ZONE as u16, 1001),
    )
    .await;
    let frames = f.king_client.drain();
    assert_eq!(
        error_of(&frames, sc::SCErrorMsgPacket),
        Some(crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_INVALID_RATE)
    );
    assert_eq!(f.state().national_tax_rate, 0);

    // Not the king -> 685 (`dominion_national_tax_no_permission`).
    f.subject_client.conn.active_char = Some(f.subject.clone());
    f.server
        .handle_frame(
            &mut f.subject_client.conn,
            &encode_raw(
                1,
                cs::CSUpdateNationalTaxRatePacket,
                tax_body(ZONE as u16, 500).as_bytes(),
            ),
        )
        .await
        .expect("handler");
    let frames = f.subject_client.drain();
    assert_eq!(
        error_of(&frames, sc::SCErrorMsgPacket),
        Some(crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION)
    );

    // The king sets 500 permille: both clients receive SCNationalTaxRate.
    let before = Utc::now().timestamp();
    f.send(
        cs::CSUpdateNationalTaxRatePacket,
        tax_body(ZONE as u16, 500),
    )
    .await;
    let saved = f.state();
    assert_eq!(saved.national_tax_rate, 500);
    assert!(saved.last_national_tax_rate_changed_at >= before);
    for client in [&mut f.king_client, &mut f.subject_client] {
        let frames = client.drain();
        let rates = frames_of(&frames, sc::SCNationalTaxRatePacket);
        assert_eq!(rates.len(), 1);
        let (zone, rate, prev, time) = parse_tax_rate(&rates[0]);
        assert_eq!((zone, rate, prev), (ZONE as u16, 500, 0));
        assert_eq!(time, saved.last_national_tax_rate_changed_at);
    }
    for receiver in [1u32, 2] {
        let notice = f
            .server
            .systems
            .economy
            .mail
            .state()
            .all
            .iter()
            .find(|(_, m)| {
                m.header.receiver_id == receiver
                    && m.mail_type == crate::systems::economy::mail::mail_type::NATION_TAX_RATE
            })
            .map(|(_, m)| m.clone())
            .expect("each online citizen gets the national tax notice");
        assert_eq!(notice.title, "title(25)");
        assert_eq!(
            notice.body.text,
            format!(
                "body('{NATION}','{ZONE}','500','0','{}')",
                saved.last_national_tax_rate_changed_at
            )
        );
        assert_eq!(
            notice.header.sender_name,
            crate::systems::economy::mail_system::NATION_TAX_RATE_SENDER
        );
    }

    // The same rate is ignored (the client shows `equal_tax_rate_warning`).
    f.send(
        cs::CSUpdateNationalTaxRatePacket,
        tax_body(ZONE as u16, 500),
    )
    .await;
    assert!(f.king_client.drain().is_empty());

    // A second change within 86400 s -> 725 (`dominion_national_tax_later`).
    f.send(
        cs::CSUpdateNationalTaxRatePacket,
        tax_body(ZONE as u16, 400),
    )
    .await;
    let frames = f.king_client.drain();
    assert_eq!(
        error_of(&frames, sc::SCErrorMsgPacket),
        Some(crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_LATER)
    );
    assert_eq!(f.state().national_tax_rate, 500);

    // An unowned dominion is not the king's -> 685.
    let mut unowned = f.state();
    unowned.owner_expedition_id = 0;
    unowned.reign_start_at = 0;
    unowned.ownership_generation = 0;
    f.server
        .systems
        .housing
        .dominions
        .restore(vec![unowned])
        .expect("unowned fixture");
    f.server
        .systems
        .housing
        .dominions
        .set_national_tax_rate(ZONE, 0, 500, 1)
        .expect_err("unowned");
    f.send(
        cs::CSUpdateNationalTaxRatePacket,
        tax_body(ZONE as u16, 100),
    )
    .await;
    let frames = f.king_client.drain();
    assert_eq!(
        error_of(&frames, sc::SCErrorMsgPacket),
        Some(crate::systems::economy::error_message::DOMINION_NATIONAL_TAX_NO_PERMISSION)
    );
}

#[tokio::test]
async fn monument_place_remove_spawns_the_territory_statue_and_broadcasts() {
    let mut f = Fixture::new();
    f.give_dominion(EXPEDITION);
    let (x, y, z) = (100.0, 100.0, 100.0);

    nation::place_national_monument(&f.server, &f.king, ZONE as u16, x, y, z)
        .await
        .expect("placement");
    let saved = f.state();
    assert!(saved.national_monument_db_id > 0);
    assert_eq!(
        (
            saved.national_monument_x,
            saved.national_monument_y,
            saved.national_monument_z
        ),
        (x, y, z)
    );
    let world = f.server.world.world(0).expect("world");
    let doodad = world
        .doodad_by_db_id(saved.national_monument_db_id as u32)
        .expect("statue doodad");
    assert_eq!(doodad.data().template_id, MONUMENT_DOODAD);
    assert!(doodad.data().is_persistent);
    for client in [&mut f.king_client, &mut f.subject_client] {
        let frames = client.drain();
        let monuments = frames_of(&frames, sc::SCNationalMonumentChangedPacket);
        assert_eq!(monuments.len(), 1);
        let (zone, db_id, px, py, pz) = parse_monument(&monuments[0]);
        assert_eq!(zone, ZONE as u16);
        assert_eq!(db_id, saved.national_monument_db_id);
        assert!((px - x).abs() < 0.01 && (py - y).abs() < 0.01 && (pz - z).abs() < 0.01);
    }

    // One statue per dominion -> 750.
    let error = nation::place_national_monument(&f.server, &f.king, ZONE as u16, x, y, z)
        .await
        .expect_err("already placed");
    assert_eq!(
        error,
        crate::systems::economy::error_message::NO_MORE_NATIONAL_MONUMENT
    );

    // Removal clears the state, deletes the doodad and broadcasts db id 0.
    nation::remove_national_monument(&f.server, &f.king, ZONE as u16)
        .await
        .expect("removal");
    let saved = f.state();
    assert_eq!(saved.national_monument_db_id, 0);
    assert!(world.doodad_by_db_id(doodad.data().db_id).is_none());
    for client in [&mut f.king_client, &mut f.subject_client] {
        let frames = client.drain();
        let monuments = frames_of(&frames, sc::SCNationalMonumentChangedPacket);
        assert_eq!(monuments.len(), 1);
        let (zone, db_id, _, _, _) = parse_monument(&monuments[0]);
        assert_eq!((zone, db_id), (ZONE as u16, 0));
    }
    // A second removal is a no-op.
    nation::remove_national_monument(&f.server, &f.king, ZONE as u16)
        .await
        .expect("idempotent");
    assert!(f.king_client.drain().is_empty());
}

#[tokio::test]
async fn monument_gates_king_dominion_position_and_siege_period() {
    let f = Fixture::new();
    f.give_dominion(EXPEDITION);
    let (x, y, z) = (100.0, 100.0, 100.0);

    // Not a nation king -> 748.
    let error = nation::place_national_monument(&f.server, &f.subject, ZONE as u16, x, y, z)
        .await
        .expect_err("not the king");
    assert_eq!(
        error,
        crate::systems::economy::error_message::ONLY_NATION_OWNER
    );

    // The king must be at the point (skill max_range 20).
    let error = nation::place_national_monument(&f.server, &f.king, ZONE as u16, 121.0, y, z)
        .await
        .expect_err("too far");
    assert_eq!(
        error,
        crate::systems::economy::error_message::NATION_OWNER_AT_POS_ONLY
    );

    // A kingdom without this dominion -> 757.
    let mut other = f.state();
    other.owner_expedition_id = OTHER_EXPEDITION;
    f.server
        .systems
        .housing
        .dominions
        .restore(vec![other])
        .expect("other owner");
    f.server.systems.social.nations.add(Nation {
        id: OTHER_NATION,
        expedition_id: OTHER_EXPEDITION,
        owner_character_id: 9,
        owner_name: "Other".into(),
        name: "OtherKingdom".into(),
        created_at: 1,
        allow_change_name: 0,
        relations: Vec::new(),
        offline_members: Vec::new(),
    });
    f.server.expeditions.add(Expedition {
        faction: SystemFaction {
            id: OTHER_EXPEDITION,
            mother_id: OTHER_NATION,
            name: "OtherGuild".into(),
            owner_id: 9,
            ..SystemFaction::default()
        },
        members: Vec::new(),
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    });
    let error = nation::place_national_monument(&f.server, &f.king, ZONE as u16, x, y, z)
        .await
        .expect_err("another kingdom's dominion");
    assert_eq!(
        error,
        crate::systems::economy::error_message::NATION_OWNER_AT_POS_ONLY
    );

    // Restore the king's dominion, then schedule a siege -> 751.
    f.give_dominion(EXPEDITION);
    let now = Utc::now().timestamp();
    f.server
        .systems
        .housing
        .dominions
        .schedule_siege(ZONE, OTHER_EXPEDITION, now + 60, now + 600, now)
        .expect("siege");
    let error = nation::place_national_monument(&f.server, &f.king, ZONE as u16, x, y, z)
        .await
        .expect_err("siege period");
    assert_eq!(
        error,
        crate::systems::economy::error_message::CANT_PUT_UP_NATIONAL_MONUMENT_DURING_SIEGE_PERIOD
    );
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary tables"]
async fn private_mysql_nation_rename_is_persisted() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TEMPORARY TABLE nations (\
         id int unsigned NOT NULL, expedition_id int unsigned NOT NULL,\
         owner_character_id int unsigned NOT NULL, owner_name varchar(128) NOT NULL,\
         name varchar(128) NOT NULL, created_at bigint NOT NULL DEFAULT 0,\
         allow_change_name tinyint unsigned NOT NULL DEFAULT 0,\
         PRIMARY KEY (id), UNIQUE KEY nations_name (name)) ENGINE=InnoDB",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO nations VALUES (9000, 1000, 1, 'King', 'Kingdom', 1, 0)")
        .execute(&pool)
        .await
        .unwrap();

    let mut f = Fixture::new();
    f.server.db = pool.clone();
    nation::offer_nation_rename_ui(&f.server, &f.king).expect("offer");
    f.king_client.drain();
    f.send(
        cs::CSRenameExpeditionPacket,
        rename_body(NATION, "Renamed", false),
    )
    .await;

    let saved: String = sqlx::query_scalar("SELECT name FROM nations WHERE id = 9000")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(saved, "Renamed");
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary tables"]
async fn private_mysql_dominion_tax_and_monument_round_trip() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    // The real base-schema definitions, so a column added to SQL/aaemu_game.sql is picked up
    // here (the old hand-written DDL was missing siege_reinforced / siege_outcome_credited).
    let schema = include_str!("../../../../../../SQL/aaemu_game.sql");
    for table in ["dominions", "dominion_siege_participants"] {
        let marker = format!("CREATE TABLE IF NOT EXISTS `{table}`");
        let start = schema
            .find(&marker)
            .unwrap_or_else(|| panic!("{table} missing from SQL/aaemu_game.sql"));
        let end = start + schema[start..].find(';').expect("DDL statement end");
        let ddl =
            schema[start..=end].replacen("CREATE TABLE IF NOT EXISTS", "CREATE TEMPORARY TABLE", 1);
        sqlx::query(&ddl).execute(&pool).await.unwrap();
    }

    let mut f = Fixture::new();
    f.server.db = pool.clone();
    f.give_dominion(EXPEDITION);
    f.server
        .systems
        .housing
        .dominions
        .set_national_tax_rate(ZONE, EXPEDITION, 750, 1_700_000_000)
        .expect("tax");
    f.server
        .systems
        .housing
        .dominions
        .place_national_monument(ZONE, EXPEDITION, 77, 10.5, 20.0, 30.0)
        .expect("monument");

    let mut tx = f.server.db.begin().await.unwrap();
    f.server
        .systems
        .housing
        .dominions
        .save(&mut tx)
        .await
        .expect("save");
    tx.commit().await.unwrap();
    f.server.systems.housing.dominions.commit_save();

    let rows = crate::systems::housing::dominion::db::load_rows(&f.server.db)
        .await
        .expect("load");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.national_tax_rate, 750);
    assert_eq!(row.last_national_tax_rate_changed_at, 1_700_000_000);
    assert_eq!(row.national_monument_db_id, 77);
    assert_eq!(row.national_monument_x, 10.5);
    assert_eq!(row.national_monument_y, 20.0);
    assert_eq!(row.national_monument_z, 30.0);
}

#[tokio::test]
async fn releasing_the_owner_clears_the_monument_and_deletes_the_statue() {
    let mut f = Fixture::new();
    f.give_dominion(EXPEDITION);
    let (x, y, z) = (100.0, 100.0, 100.0);
    nation::place_national_monument(&f.server, &f.king, ZONE as u16, x, y, z)
        .await
        .expect("placement");
    let previous = f.state();
    let db_id = previous.national_monument_db_id;
    assert!(db_id > 0);
    let world = f.server.world.world(0).expect("world");
    assert!(world.doodad_by_db_id(db_id as u32).is_some());
    f.king_client.drain();
    f.subject_client.drain();

    let released = f
        .server
        .systems
        .housing
        .dominions
        .release_expedition(EXPEDITION, Utc::now().timestamp());
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].0.national_monument_db_id, db_id);
    assert_eq!(released[0].1.national_monument_db_id, 0);
    super::dominion::apply_released(&f.server, &released[0].0, &released[0].1);

    let saved = f.state();
    assert_eq!(saved.owner_expedition_id, 0);
    assert_eq!(saved.national_monument_db_id, 0);
    assert!(world.doodad_by_db_id(db_id as u32).is_none());
    for client in [&mut f.king_client, &mut f.subject_client] {
        let frames = client.drain();
        let monuments = frames_of(&frames, sc::SCNationalMonumentChangedPacket);
        assert_eq!(monuments.len(), 1);
        let (zone, cleared, px, py, pz) = parse_monument(&monuments[0]);
        assert_eq!((zone, cleared), (ZONE as u16, 0));
        assert_eq!((px, py, pz), (0.0, 0.0, 0.0));
    }
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary tables"]
async fn private_mysql_national_tax_update_leaves_a_dirty_treasury_alone() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TEMPORARY TABLE dominions (\
         zone_group_id int unsigned NOT NULL PRIMARY KEY,\
         national_tax_rate smallint unsigned NOT NULL DEFAULT 0,\
         last_national_tax_rate_changed_at bigint NOT NULL DEFAULT 0,\
         national_monument_db_id bigint NOT NULL DEFAULT 0,\
         national_monument_x float NOT NULL DEFAULT 0,\
         national_monument_y float NOT NULL DEFAULT 0,\
         national_monument_z float NOT NULL DEFAULT 0,\
         cur_house_tax_money int NOT NULL DEFAULT 0) ENGINE=InnoDB",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO dominions (zone_group_id) VALUES (25), (26)")
        .execute(&pool)
        .await
        .unwrap();

    let mut f = Fixture::new();
    f.server.db = pool.clone();
    let tower = |house_id: u32| TowerIdentity {
        house_id,
        instance_id: 0,
        guard_tower_setting_id: 6,
    };
    let state = |zone: u32, house_id: u32| {
        let mut state = DominionState::unowned(zone, tower(house_id));
        state.owner_expedition_id = EXPEDITION;
        state.ownership_generation = 1;
        state.reign_start_at = 1_000;
        state
    };
    f.server
        .systems
        .housing
        .dominions
        .restore(vec![state(25, 525), state(26, 526)])
        .expect("states");
    f.server
        .systems
        .housing
        .dominions
        .credit_treasury(
            26,
            crate::systems::housing::dominion::state::Treasury {
                cur_house_tax_money: 1_234,
                ..Default::default()
            },
        )
        .expect("dirty treasury");
    f.server
        .systems
        .housing
        .dominions
        .set_national_tax_rate(25, EXPEDITION, 750, 1_700_000_000)
        .expect("tax");

    super::dominion::persist_national_tax(&f.server, 25, 750, 1_700_000_000)
        .await
        .expect("targeted tax update");

    let tax: u16 =
        sqlx::query_scalar("SELECT national_tax_rate FROM dominions WHERE zone_group_id = 25")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tax, 750);
    let treasury: i32 =
        sqlx::query_scalar("SELECT cur_house_tax_money FROM dominions WHERE zone_group_id = 26")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        treasury, 0,
        "a tax-rate write must not commit another zone's treasury"
    );
    assert_eq!(
        f.server
            .systems
            .housing
            .dominions
            .prepare_save()
            .changed_count(),
        2,
        "both in-memory rows stay dirty for the periodic financial save"
    );
}

#[tokio::test]
#[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; uses connection-local temporary tables"]
async fn private_mysql_monument_row_is_written_before_the_dominion_reference() {
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::str::FromStr;

    let Ok(url) = std::env::var("AAEMU_TEST_MYSQL") else {
        return;
    };
    let options = MySqlConnectOptions::from_str(&url).expect("valid private MySQL URL");
    assert_eq!(
        options.get_port(),
        3317,
        "Only private MySQL3317 is allowed"
    );
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    // The doodad writer takes this pool's single connection; the temp table is visible there.
    sqlx::query(
        "CREATE TEMPORARY TABLE doodads (\
         id int unsigned NOT NULL AUTO_INCREMENT, owner_id int DEFAULT NULL,\
         owner_type tinyint unsigned DEFAULT 255, attach_point int unsigned NULL DEFAULT 0,\
         template_id int NOT NULL, current_phase_id int NOT NULL, plant_time datetime NOT NULL,\
         growth_time datetime NOT NULL, phase_time datetime NOT NULL, x float NOT NULL,\
         y float NOT NULL, z float NOT NULL, roll float NOT NULL, pitch float NOT NULL,\
         yaw float NOT NULL, scale float NOT NULL DEFAULT 1, item_id bigint unsigned NOT NULL DEFAULT 0,\
         house_id int unsigned NOT NULL DEFAULT 0, parent_doodad int unsigned NOT NULL DEFAULT 0,\
         item_template_id int unsigned NOT NULL DEFAULT 0,\
         item_container_id bigint unsigned NOT NULL DEFAULT 0, data int NOT NULL DEFAULT 0,\
         farm_type int NOT NULL DEFAULT 0, interaction_counts text NULL, PRIMARY KEY (id)) ENGINE=InnoDB",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TEMPORARY TABLE dominions (\
         zone_group_id int unsigned NOT NULL PRIMARY KEY,\
         national_monument_db_id bigint NOT NULL DEFAULT 0,\
         national_monument_x float NOT NULL DEFAULT 0,\
         national_monument_y float NOT NULL DEFAULT 0,\
         national_monument_z float NOT NULL DEFAULT 0) ENGINE=InnoDB",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO dominions (zone_group_id) VALUES (25)")
        .execute(&pool)
        .await
        .unwrap();

    let f = Fixture::new();
    let mut server = f.server;
    server.db = pool.clone();
    let mut state = DominionState::unowned(
        ZONE,
        TowerIdentity {
            house_id: 500,
            instance_id: 0,
            guard_tower_setting_id: 6,
        },
    );
    state.owner_expedition_id = EXPEDITION;
    state.ownership_generation = 1;
    state.reign_start_at = 1_000;
    server
        .systems
        .housing
        .dominions
        .restore(vec![state])
        .expect("dominion fixture");
    let server = std::sync::Arc::new(server);
    server.set_self();

    nation::place_national_monument(&server, &f.king, ZONE as u16, 100.0, 100.0, 100.0)
        .await
        .expect("placement");
    let state = server.systems.housing.dominions.get(ZONE).expect("state");
    assert!(state.national_monument_db_id > 0);
    let doodads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM doodads WHERE id = ?")
        .bind(state.national_monument_db_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(doodads, 1, "the statue row must exist");
    let reference: i64 = sqlx::query_scalar(
        "SELECT national_monument_db_id FROM dominions WHERE zone_group_id = 25",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reference, state.national_monument_db_id);
}
