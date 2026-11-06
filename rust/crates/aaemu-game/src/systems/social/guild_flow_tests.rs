//! Flow tests of the guild functions added on top of the C# port: rename, authority checks,
//! invite / create checks, disband and the faction-change leave. See PORTING.md "Guild
//! functions (2026-11-05)".

use aaemu_commons::PacketStream;

use super::flow_tests::{add_guild, error_frame, find, name_body, pc, settle, Pc};
use super::packets::body;
use super::packets::faction::{SCFactionRenamedPacket, SCFactionRetryRenamePacket};
use crate::data::character_templates::BlockedName;
use crate::data::GameData;
use crate::opcodes::{cs, sc};
use crate::server::GameServer;
use crate::systems::social::util::error;
use crate::test_support::{test_server, test_server_with};

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
    lead.send(
        &server,
        cs::CSRenameExpeditionPacket,
        rename_body(77, "Order", true),
    )
    .await;
    assert_eq!(lead.frames(), vec![error_frame(error::FACTION_RIGHTS)]);
    two.send(
        &server,
        cs::CSRenameExpeditionPacket,
        rename_body(77, "Order", true),
    )
    .await;
    assert_eq!(
        two.frames(),
        vec![error_frame(error::ONLY_EXPEDITION_OWNER)]
    );
    out.send(
        &server,
        cs::CSRenameExpeditionPacket,
        rename_body(77, "Order", true),
    )
    .await;
    assert_eq!(
        out.frames(),
        vec![error_frame(error::ONLY_EXPEDITION_MEMBER)]
    );

    // The GM grants the right: the leader (only) gets the dialog, everybody the faction record.
    assert_eq!(
        super::expedition::grant_rename(&server, 77)
            .await
            .as_deref(),
        Some("Knights")
    );
    assert_eq!(super::expedition::grant_rename(&server, 5).await, None);
    let ops = lead.ops();
    assert!(ops.contains(&sc::SCExpeditionShowRenameUIPacket));
    assert!(ops.contains(&sc::SCFactionListPacket));
    assert!(!two.ops().contains(&sc::SCExpeditionShowRenameUIPacket));
    assert_eq!(
        server
            .expeditions
            .get(77)
            .unwrap()
            .faction
            .allow_change_name,
        1
    );
    out.ops();

    // Every rule reopens the dialog with the refused name.
    for (name, code) in [
        (
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi",
            error::EXPEDITION_NAME_LENGTH,
        ),
        ("x1", error::EXPEDITION_NAME_CHARACTER),
        ("Has Badword In", error::EXPEDITION_NAME_BADLIST),
        ("rIVALS", error::EXPEDITION_NAME_EXIST),
    ] {
        lead.send(
            &server,
            cs::CSRenameExpeditionPacket,
            rename_body(77, name, true),
        )
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
    lead.send(
        &server,
        cs::CSRenameExpeditionPacket,
        rename_body(88, "Order", true),
    )
    .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::ONLY_EXPEDITION_OWNER)]
    );

    let chan = server.chat.guild_chat(77, "Knights", &server.ids.chat);
    lead.send(
        &server,
        cs::CSRenameExpeditionPacket,
        rename_body(77, "Order", true),
    )
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
    lead.send(
        &server,
        cs::CSRenameExpeditionPacket,
        rename_body(77, "Again", true),
    )
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
    assert_eq!(
        super::expedition::gm_rename(&server, 77, "Fine").await,
        Ok(())
    );
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
    server
        .expeditions
        .with_mut(77, |e| e.faction.allow_change_name = 1);
    super::expedition::prompt_rename_if_allowed(&server, &lead.p);
    assert_eq!(lead.ops(), vec![sc::SCExpeditionShowRenameUIPacket]);
}

fn create_body(name: &str, sponsor: u32) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_str(name).write_u32(sponsor);
    s
}

fn promote_body(role: u8, target: u32) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u8(role).write_u32(target);
    s
}

fn id_body(id: u32) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_u32(id);
    s
}

fn policy_body(role: u8, name: &str, flags: [bool; 9]) -> PacketStream {
    use super::expedition::ExpeditionRolePolicy;
    let p = ExpeditionRolePolicy {
        expedition_id: 77,
        role,
        name: name.into(),
        dominion_declare: flags[0],
        invite: flags[1],
        expel: flags[2],
        promote: flags[3],
        dismiss: flags[4],
        chat: flags[5],
        manager_chat: flags[6],
        siege_master: flags[7],
        join_siege: flags[8],
    };
    let mut s = PacketStream::new();
    aaemu_commons::Marshal::write(&p, &mut s);
    s
}

/// The dominion of group 25 owned by `owner`, with a siege registered by `attacker` (0 = none).
fn register_siege(server: &GameServer, owner: u32, attacker: u32) {
    use crate::systems::housing::dominion::state::{
        DominionState, SiegeOutcome, SiegeState, SiegeStatus, TowerIdentity,
    };
    let mut state = DominionState::unowned(
        25,
        TowerIdentity {
            house_id: 500,
            instance_id: 0,
            guard_tower_setting_id: 6,
        },
    );
    state.owner_expedition_id = owner;
    state.ownership_generation = 1;
    state.reign_start_at = 1000;
    if attacker != 0 {
        state.last_siege_id = 1;
        state.siege = Some(SiegeState {
            siege_id: 1,
            attacker_expedition_id: attacker,
            defender_expedition_id: owner,
            ownership_generation: 1,
            status: SiegeStatus::Scheduled,
            outcome: SiegeOutcome::Pending,
            scheduled_at: 1000,
            starts_at: 4_000_000_000,
            ends_at: 4_000_007_200,
            ended_at: 0,
            reinforced: false,
            defender_ni: 100,
            defender_nr: 100,
            outcome_credited: false,
            materialised_from_declaration: false,
            calendar_cycle: 0,
            participants: Default::default(),
        });
    }
    server
        .systems
        .housing
        .dominions
        .restore(vec![state])
        .expect("dominion state");
}

/// Owner changes: every refusal the client's validators have is answered, nothing else is sent.
#[tokio::test]
async fn change_owner_failures_are_reported() {
    let server = test_server();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut cmdr = pc(&server, 2, "Cmdr", 101.0, 100.0);
    let mut out = pc(&server, 9, "Out", 102.0, 100.0);
    add_guild(&server, 77, "Knights", &[(&lead, 255), (&cmdr, 3)]);
    add_guild(&server, 88, "Rivals", &[(&out, 255)]);
    settle(&mut [&mut lead, &mut cmdr, &mut out]);
    let before = server.expeditions.get(77).unwrap();

    cmdr.send(&server, cs::CSChangeExpeditionOwnerPacket, id_body(2))
        .await;
    assert_eq!(cmdr.frames(), vec![error_frame(error::FACTION_RIGHTS)]);
    lead.send(&server, cs::CSChangeExpeditionOwnerPacket, id_body(1))
        .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::EXPEDITION_CHANGE_OWNER_SELF)]
    );
    // Not a member of this guild (a member of another one; or nobody).
    for target in [9, 99] {
        lead.send(&server, cs::CSChangeExpeditionOwnerPacket, id_body(target))
            .await;
        assert_eq!(
            lead.frames(),
            vec![error_frame(error::EXPEDITION_CHANGE_OWNER_MY_EXPEDITION)]
        );
    }
    // During a siege.
    register_siege(&server, 88, 77);
    lead.send(&server, cs::CSChangeExpeditionOwnerPacket, id_body(2))
        .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(
            error::EXPEDITION_OWNER_CANNOT_CHANGE_DURING_SIEGE
        )]
    );
    assert_eq!(server.expeditions.get(77).unwrap(), before);
    assert!(cmdr.ops().is_empty() && out.ops().is_empty());
    // Without the siege it works.
    register_siege(&server, 88, 0);
    lead.send(&server, cs::CSChangeExpeditionOwnerPacket, id_body(2))
        .await;
    assert_eq!(server.expeditions.get(77).unwrap().faction.owner_id, 2);
}

/// Member role changes: the Promote right, a target and a new role strictly below the caller.
#[tokio::test]
async fn change_member_role_failures_are_reported() {
    use crate::systems::social::util::error::{
        EXPEDITION_NO_TARGET, FACTION_CHANGE_MEMBER_ROLE_RIGHTS as DENIED,
    };
    let server = test_server();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut cmdr = pc(&server, 2, "Cmdr", 101.0, 100.0);
    let mut off = pc(&server, 3, "Off", 102.0, 100.0);
    let mut rookie = pc(&server, 4, "Rookie", 103.0, 100.0);
    add_guild(
        &server,
        77,
        "Knights",
        &[(&lead, 255), (&cmdr, 3), (&off, 2), (&rookie, 0)],
    );
    server.expeditions.with_mut(77, |e| {
        for p in e.policies.iter_mut().filter(|p| p.role == 3) {
            p.promote = true;
        }
    });
    settle(&mut [&mut lead, &mut cmdr, &mut off, &mut rookie]);
    let role_of = |id: u32| server.expeditions.get(77).unwrap().member(id).unwrap().role;

    // No promote right.
    off.send(
        &server,
        cs::CSChangeExpeditionMemberRolePacket,
        promote_body(1, 4),
    )
    .await;
    assert_eq!(off.frames(), vec![error_frame(DENIED)]);
    // The commander (3) cannot touch the leader, equals, or hand out its own / a higher role.
    for (role, target) in [(1, 1), (3, 4), (255, 4), (9, 4)] {
        cmdr.send(
            &server,
            cs::CSChangeExpeditionMemberRolePacket,
            promote_body(role, target),
        )
        .await;
        assert_eq!(cmdr.frames(), vec![error_frame(DENIED)], "{role} {target}");
    }
    // The leader cannot make somebody a second leader or give a role without a policy.
    for role in [255, 9] {
        lead.send(
            &server,
            cs::CSChangeExpeditionMemberRolePacket,
            promote_body(role, 4),
        )
        .await;
        assert_eq!(lead.frames(), vec![error_frame(DENIED)], "{role}");
    }
    cmdr.send(
        &server,
        cs::CSChangeExpeditionMemberRolePacket,
        promote_body(1, 99),
    )
    .await;
    assert_eq!(cmdr.frames(), vec![error_frame(EXPEDITION_NO_TARGET)]);
    assert_eq!((role_of(3), role_of(4)), (2, 0));
    // A valid change still works and is broadcast.
    cmdr.send(
        &server,
        cs::CSChangeExpeditionMemberRolePacket,
        promote_body(1, 3),
    )
    .await;
    assert_eq!(role_of(3), 1);
    for x in [&mut lead, &mut cmdr, &mut off, &mut rookie] {
        assert_eq!(x.ops(), vec![sc::SCExpeditionRoleChangedPacket]);
    }
}

/// Role policies: only the leader edits, only the roles below it, the role name is validated,
/// flags the 1.2 dialog does not show keep their value, and the stored policy is broadcast.
#[tokio::test]
async fn role_policy_edits_are_leader_only_and_broadcast_the_stored_policy() {
    use super::expedition::ExpeditionRolePolicy;
    use super::packets::expedition::SCExpeditionRolePolicyChangedPacket;
    use crate::systems::social::util::error::FACTION_CHANGE_MEMBER_ROLE_RIGHTS as DENIED;
    let server = test_server();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut cmdr = pc(&server, 2, "Cmdr", 101.0, 100.0);
    add_guild(&server, 77, "Knights", &[(&lead, 255), (&cmdr, 3)]);
    settle(&mut [&mut lead, &mut cmdr]);
    let before = server.expeditions.get(77).unwrap().policies;
    let flags = [true; 9];

    // The commander has the Expel right but is not the leader: not its own, not the leader's.
    for role in [3, 255, 2] {
        cmdr.send(
            &server,
            cs::CSChangeExpeditionRolePolicyPacket,
            policy_body(role, "Renamed", flags),
        )
        .await;
        assert_eq!(cmdr.frames(), vec![error_frame(DENIED)], "{role}");
    }
    // The leader cannot edit its own policy or a role that does not exist.
    for role in [255, 9] {
        lead.send(
            &server,
            cs::CSChangeExpeditionRolePolicyPacket,
            policy_body(role, "Renamed", flags),
        )
        .await;
        assert_eq!(lead.frames(), vec![error_frame(DENIED)], "{role}");
    }
    // A role name that breaks the name rules.
    lead.send(
        &server,
        cs::CSChangeExpeditionRolePolicyPacket,
        policy_body(2, "x1", flags),
    )
    .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::EXPEDITION_NAME_CHARACTER)]
    );
    assert_eq!(server.expeditions.get(77).unwrap().policies, before);
    assert!(cmdr.ops().is_empty());

    // An edit changes the name and the four flags only, and every member gets the stored policy.
    lead.send(
        &server,
        cs::CSChangeExpeditionRolePolicyPacket,
        policy_body(2, "Lieutenant", flags),
    )
    .await;
    let stored = ExpeditionRolePolicy {
        expedition_id: 77,
        role: 2,
        name: "Lieutenant".into(),
        dominion_declare: true,
        invite: true,
        expel: true,
        promote: true,
        dismiss: false,
        chat: true,
        manager_chat: false,
        siege_master: false,
        join_siege: true,
    };
    assert_eq!(
        server.expeditions.get(77).unwrap().policy_by_role(2),
        Some(&stored)
    );
    let expected = (
        sc::SCExpeditionRolePolicyChangedPacket,
        body(&SCExpeditionRolePolicyChangedPacket {
            policy: stored,
            success: true,
        }),
    );
    assert_eq!(lead.frames(), vec![expected.clone()]);
    assert_eq!(cmdr.frames(), vec![expected]);
}

fn nations() -> GameData {
    let mut data = GameData::empty();
    for (id, mother) in [(101, 148), (102, 148), (105, 148), (108, 149), (114, 0)] {
        data.factions.add(crate::data::world_data::SystemFaction {
            id,
            mother_id: mother,
            ..Default::default()
        });
    }
    data
}

/// Invitations: another alliance (304) and a dead target (307) are refused with a message.
#[tokio::test]
async fn guild_invite_refuses_other_alliances_and_the_dead() {
    let server = test_server_with(nations());
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut same = pc(&server, 2, "SameNation", 101.0, 100.0);
    let mut foe = pc(&server, 3, "Haranya", 102.0, 100.0);
    let mut dead = pc(&server, 4, "Dead", 103.0, 100.0);
    add_guild(&server, 77, "Knights", &[(&lead, 255)]);
    same.p.data().unit.faction_id = 102; // another faction of the same alliance
    foe.p.data().unit.faction_id = 108;
    dead.p.data().unit.hp = 0;
    settle(&mut [&mut lead, &mut same, &mut foe, &mut dead]);

    lead.send(
        &server,
        cs::CSInviteToExpeditionPacket,
        name_body("Haranya"),
    )
    .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::EXPEDITION_BAD_FACTION)]
    );
    assert!(foe.ops().is_empty());
    lead.send(&server, cs::CSInviteToExpeditionPacket, name_body("Dead"))
        .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::EXPEDITION_BAD_STATUS)]
    );
    assert!(dead.ops().is_empty());
    lead.send(
        &server,
        cs::CSInviteToExpeditionPacket,
        name_body("SameNation"),
    )
    .await;
    assert!(lead.ops().is_empty());
    assert_eq!(same.ops(), vec![sc::SCExpeditionInvitationPacket]);
}

/// Creating a guild: the sponsor must belong to the owner's alliance and becomes the guild's
/// `mother_id`, a party member of another alliance is 320, and a team member that is offline
/// does not join (nor count).
#[tokio::test]
async fn guild_create_checks_sponsor_alliance_and_online_members() {
    let server = test_server_with(nations());
    let mut pcs: Vec<Pc> = (1..=5)
        .map(|i| pc(&server, i, &format!("M{i}"), 100.0 + i as f32, 100.0))
        .collect();
    {
        let teams = &server.systems.social.teams;
        for i in 1..5 {
            let name = format!("M{}", i + 1);
            let (lead, rest) = pcs.split_at_mut(i);
            teams.ask_to_join(&server, &lead[0].p, &name, 0, true, None);
            teams.reply_to_join_team(&server, &rest[0].p, 0, true, 1, false, "", false);
        }
    }
    pcs[0].p.data().money = 100_000;
    for x in pcs.iter_mut() {
        x.ops();
    }

    // A sponsor of the other alliance, one that is no sponsor at all.
    for sponsor in [108, 104, 148] {
        pcs[0]
            .send(
                &server,
                cs::CSCreateExpeditionPacket,
                create_body("Knights", sponsor),
            )
            .await;
        assert_eq!(
            pcs[0].frames(),
            vec![error_frame(error::EXPEDITION_CREATE_FACTION)],
            "sponsor {sponsor}"
        );
    }
    // A party member of the other alliance.
    pcs[4].p.data().unit.faction_id = 108;
    pcs[0]
        .send(
            &server,
            cs::CSCreateExpeditionPacket,
            create_body("Knights", 105),
        )
        .await;
    assert_eq!(
        pcs[0].frames(),
        vec![error_frame(error::EXPEDITION_CREATE_MEMBER_FACTION)]
    );
    pcs[4].p.data().unit.faction_id = 101;
    // An offline member neither joins nor counts towards the party size.
    pcs[3].p.data().is_online = false;
    pcs[0]
        .send(
            &server,
            cs::CSCreateExpeditionPacket,
            create_body("Knights", 105),
        )
        .await;
    assert_eq!(
        pcs[0].frames(),
        vec![error_frame(error::EXPEDITION_CREATE_MEMBER)]
    );
    assert_eq!(server.expeditions.all().len(), 0);
    pcs[3].p.data().is_online = true;
    assert_eq!(pcs[0].p.data().money, 100_000);

    pcs[0]
        .send(
            &server,
            cs::CSCreateExpeditionPacket,
            create_body("Knights", 105),
        )
        .await;
    let id = pcs[0].p.data().unit.expedition_id;
    assert_ne!(id, 0);
    let e = server.expeditions.get(id).unwrap();
    assert_eq!(e.faction.mother_id, 105);
    assert_eq!(e.members.len(), 5);
    // The same name in other letters is taken.
    pcs[1].p.data().unit.expedition_id = 0;
    pcs[1]
        .send(
            &server,
            cs::CSCreateExpeditionPacket,
            create_body("kNIGHTS", 0),
        )
        .await;
    assert_eq!(
        find(&pcs[1].frames(), sc::SCErrorMsgPacket),
        vec![body(&crate::packets::world::SCErrorMsgPacket::new(
            error::EXPEDITION_NAME_EXIST,
            0,
            true
        ))]
    );
}

/// Dismissing: territory 301, a siege 469; a dismissed guild is no longer announced.
#[tokio::test]
async fn disband_is_refused_with_territory_or_siege_and_hides_the_guild() {
    let server = test_server();
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut two = pc(&server, 2, "Two", 101.0, 100.0);
    add_guild(&server, 77, "Knights", &[(&lead, 255), (&two, 0)]);
    add_guild(&server, 88, "Rivals", &[]);
    settle(&mut [&mut lead, &mut two]);

    two.send(&server, cs::CSDismissExpeditionPacket, PacketStream::new())
        .await;
    assert_eq!(
        two.frames(),
        vec![error_frame(error::ONLY_EXPEDITION_OWNER)]
    );
    register_siege(&server, 77, 0);
    lead.send(&server, cs::CSDismissExpeditionPacket, PacketStream::new())
        .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::EXPEDITION_OWNER_DISMISS)]
    );
    register_siege(&server, 88, 77);
    lead.send(&server, cs::CSDismissExpeditionPacket, PacketStream::new())
        .await;
    assert_eq!(
        lead.frames(),
        vec![error_frame(error::EXPEDITION_DISMISS_IN_SIEGE)]
    );
    assert_eq!(server.expeditions.get(77).unwrap().members.len(), 2);

    register_siege(&server, 88, 0);
    assert_eq!(server.expeditions.faction_chunks().concat().len(), 2);
    lead.send(&server, cs::CSDismissExpeditionPacket, PacketStream::new())
        .await;
    assert!(server.expeditions.get(77).unwrap().is_disbanded);
    let listed: Vec<u32> = server
        .expeditions
        .faction_chunks()
        .concat()
        .iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(listed, vec![88]);
}

/// A faction change into another alliance removes the character from its guild without any
/// right: a member just leaves, a leader hands the guild to the highest-ranked remaining member
/// (earliest on ties), a sole member's guild is dismissed.
#[tokio::test]
async fn faction_change_removes_the_character_from_the_guild() {
    let server = test_server_with(nations());
    let mut lead = pc(&server, 1, "Lead", 100.0, 100.0);
    let mut a = pc(&server, 2, "Aa", 101.0, 100.0);
    let mut b = pc(&server, 3, "Bb", 102.0, 100.0);
    let mut c = pc(&server, 4, "Cc", 103.0, 100.0);
    add_guild(
        &server,
        77,
        "Knights",
        &[(&lead, 255), (&a, 1), (&b, 2), (&c, 2)],
    );
    settle(&mut [&mut lead, &mut a, &mut b, &mut c]);

    // An ordinary member: leaves, everyone is told with expel = false.
    super::faction::set_faction(&server, &a.p, 108).await;
    assert_eq!(a.p.data().unit.expedition_id, 0);
    let e = server.expeditions.get(77).unwrap();
    assert!(e.member(2).is_none());
    let changed = find(&lead.frames(), sc::SCUnitExpeditionChangedPacket);
    // Once as the character's surroundings, once as a guild member (as a voluntary leave).
    assert_eq!(changed.len(), 2);
    assert!(
        changed.iter().all(|c| *c.last().unwrap() == 0),
        "expel flag"
    );
    // Another faction of the same alliance keeps the guild.
    super::faction::set_faction(&server, &b.p, 105).await;
    assert_eq!(b.p.data().unit.expedition_id, 77);

    // The leader: Bb and Cc rank 2, Bb joined first.
    for x in [&mut lead, &mut b, &mut c] {
        x.ops();
    }
    super::faction::set_faction(&server, &lead.p, 108).await;
    assert_eq!(lead.p.data().unit.expedition_id, 0);
    let e = server.expeditions.get(77).unwrap();
    assert_eq!(e.faction.owner_id, 3);
    assert_eq!(e.faction.owner_name, "Bb");
    assert_eq!(e.member(3).unwrap().role, 255);
    assert_eq!(e.member(4).unwrap().role, 2);
    assert!(e.member(1).is_none());
    assert!(b.ops().contains(&sc::SCExpeditionOwnerChangedPacket));

    // The last members: Bb hands over to Cc, Cc (alone) dismisses the guild.
    super::faction::set_faction(&server, &b.p, 108).await;
    assert_eq!(server.expeditions.get(77).unwrap().faction.owner_id, 4);
    super::faction::set_faction(&server, &c.p, 108).await;
    let e = server.expeditions.get(77).unwrap();
    assert!(e.is_disbanded && e.members.is_empty());
    assert_eq!(c.p.data().unit.expedition_id, 0);
}

/// A faction change also takes an ordinary (non-owner) member out of its party.
#[tokio::test]
async fn faction_change_removes_an_ordinary_party_member() {
    let server = test_server_with(nations());
    let mut a = pc(&server, 1, "Al", 100.0, 100.0);
    let mut b = pc(&server, 2, "Bob", 110.0, 100.0);
    let mut c = pc(&server, 3, "Cy", 120.0, 100.0);
    settle(&mut [&mut a, &mut b, &mut c]);
    super::flow_tests::join_party(&server, &mut a, &mut b, "Bob").await;
    super::flow_tests::join_party(&server, &mut a, &mut c, "Cy").await;
    let teams = &server.systems.social.teams;
    assert!(teams.active_team_by_unit(3).is_some());
    super::faction::set_faction(&server, &c.p, 108).await;
    assert!(teams.active_team_by_unit(3).is_none());
    assert!(teams.active_team_by_unit(1).is_some());
}
