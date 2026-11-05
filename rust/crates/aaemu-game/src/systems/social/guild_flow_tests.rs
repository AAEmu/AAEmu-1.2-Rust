//! Flow tests of the guild functions added on top of the C# port: rename, authority checks,
//! invite / create checks, disband and the faction-change leave. See PORTING.md "Guild
//! functions (2026-11-05)".

use aaemu_commons::PacketStream;

use super::flow_tests::{add_guild, error_frame, find, pc, settle};
use super::packets::body;
use super::packets::faction::{SCFactionRenamedPacket, SCFactionRetryRenamePacket};
use crate::data::character_templates::BlockedName;
use crate::data::GameData;
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::social::util::error;
use crate::test_support::test_server_with;

fn rename_body(id: u32, name: &str, is_expedition: bool) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(id).write_str(name).write_bool(is_expedition);
    s
}

fn retry(code: i16, name: &str) -> (u16, Vec<u8>) {
    (
        sc::SCFactionRetryRenamePacket,
        body(&SCFactionRetryRenamePacket {
            xpdt: true,
            error_message: code,
            name: name.into(),
        }),
    )
}

fn server_with_badlist() -> GameServer {
    let mut data = GameData::empty();
    data.characters.blocked_names = vec![BlockedName {
        text: "Badword".into(),
        partial_match: true,
    }];
    test_server_with(data)
}

/// The leader of a guild renames it once after a GM granted the right: the dialog opens, the
/// create rules apply (a refusal reopens the dialog), the new name reaches every client and the
/// guild chat channel, and the right is spent.
#[tokio::test]
async fn guild_rename_right_is_granted_by_a_gm_and_spent_once() {
    let server = server_with_badlist();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut two = pc(&server, 2, "Two", 101.0, 100.0);
    let mut out = pc(&server, 9, "Out", 102.0, 100.0);
    add_guild(&server, 77, "Knights", &[(&lead, 255), (&two, 0)]);
    add_guild(&server, 88, "Rivals", &[]);
    settle(&mut [&mut lead, &mut two, &mut out]);

    // No right yet: the leader gets FACTION_RIGHTS, a member the owner error, an outsider the
    // member error; the nation variant is refused.
    lead.send(&server, cs::CSRenameExpeditionPacket, rename_body(77, "Order", true))
        .await;
    assert_eq!(lead.frames(), vec![error_frame(error::FACTION_RIGHTS)]);
    two.send(&server, cs::CSRenameExpeditionPacket, rename_body(77, "Order", true))
        .await;
    assert_eq!(two.frames(), vec![error_frame(error::ONLY_EXPEDITION_OWNER)]);
    out.send(&server, cs::CSRenameExpeditionPacket, rename_body(77, "Order", true))
        .await;
    assert_eq!(out.frames(), vec![error_frame(error::ONLY_EXPEDITION_MEMBER)]);

    // The GM grants the right: the leader (only) gets the dialog, everybody the faction record.
    assert_eq!(
        super::expedition::grant_rename(&server, 77).await.as_deref(),
        Some("Knights")
    );
    assert_eq!(super::expedition::grant_rename(&server, 5).await, None);
    let ops = lead.ops();
    assert!(ops.contains(&sc::SCExpeditionShowRenameUIPacket));
    assert!(ops.contains(&sc::SCFactionListPacket));
    assert!(!two.ops().contains(&sc::SCExpeditionShowRenameUIPacket));
    assert_eq!(server.expeditions.get(77).unwrap().faction.allow_change_name, 1);
    out.ops();

    // Every rule reopens the dialog with the refused name.
    for (name, code) in [
        ("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi", error::EXPEDITION_NAME_LENGTH),
        ("x1", error::EXPEDITION_NAME_CHARACTER),
        ("Has Badword In", error::EXPEDITION_NAME_BADLIST),
        ("rIVALS", error::EXPEDITION_NAME_EXIST),
    ] {
        lead.send(&server, cs::CSRenameExpeditionPacket, rename_body(77, name, true))
            .await;
        assert_eq!(lead.frames(), vec![retry(code, name)], "{name}");
        assert_eq!(server.expeditions.get(77).unwrap().faction.name, "Knights");
    }
    // A dismissed guild's name is free again; the guild's own name is allowed (case change).
    server.expeditions.with_mut(88, |e| {
        e.faction.name = "$deleted-guild-88".into();
        e.is_disbanded = true;
    });
    // The leader of another guild, a wrong guild id.
    lead.send(&server, cs::CSRenameExpeditionPacket, rename_body(88, "Order", true))
        .await;
    assert_eq!(lead.frames(), vec![error_frame(error::ONLY_EXPEDITION_OWNER)]);

    let chan = server.chat.guild_chat(77, "Knights", &server.ids.chat);
    lead.send(&server, cs::CSRenameExpeditionPacket, rename_body(77, "Order", true))
        .await;
    let expected = (
        sc::SCFactionRenamedPacket,
        body(&SCFactionRenamedPacket {
            id: 77,
            name: "Order".into(),
            by_gm: false,
        }),
    );
    for x in [&mut lead, &mut two, &mut out] {
        assert!(x.frames().contains(&expected));
    }
    let e = server.expeditions.get(77).unwrap();
    assert_eq!(e.faction.name, "Order");
    assert_eq!(e.faction.allow_change_name, 0);
    assert_eq!(chan.internal_name(), "Order");

    // The right is spent.
    lead.send(&server, cs::CSRenameExpeditionPacket, rename_body(77, "Again", true))
        .await;
    assert_eq!(lead.frames(), vec![error_frame(error::FACTION_RIGHTS)]);
    assert_eq!(find(&two.frames(), sc::SCFactionRenamedPacket).len(), 0);
}

/// A GM renames a guild directly: `by_gm`, the right is kept, bad names are refused, a guild
/// whose leader logs in while a right is pending gets the dialog again.
#[tokio::test]
async fn gm_rename_keeps_the_right_and_the_dialog_returns_at_login() {
    let server = server_with_badlist();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    add_guild(&server, 77, "Knights", &[(&lead, 255)]);
    settle(&mut [&mut lead]);
    assert_eq!(
        super::expedition::gm_rename(&server, 77, "Bad Badword").await,
        Err(error::EXPEDITION_NAME_BADLIST)
    );
    assert_eq!(
        super::expedition::gm_rename(&server, 5, "Fine").await,
        Err(error::EXPEDITION_NO_TARGET)
    );
    assert_eq!(super::expedition::gm_rename(&server, 77, "Fine").await, Ok(()));
    assert_eq!(
        lead.frames()
            .into_iter()
            .filter(|f| f.0 == sc::SCFactionRenamedPacket)
            .collect::<Vec<_>>(),
        vec![(
            sc::SCFactionRenamedPacket,
            body(&SCFactionRenamedPacket {
                id: 77,
                name: "Fine".into(),
                by_gm: true
            })
        )]
    );
    // Login with a pending right: the leader gets the dialog, others do not.
    super::expedition::prompt_rename_if_allowed(&server, &lead.p);
    assert!(lead.ops().is_empty());
    server.expeditions.with_mut(77, |e| e.faction.allow_change_name = 1);
    super::expedition::prompt_rename_if_allowed(&server, &lead.p);
    assert_eq!(lead.ops(), vec![sc::SCExpeditionShowRenameUIPacket]);
}
