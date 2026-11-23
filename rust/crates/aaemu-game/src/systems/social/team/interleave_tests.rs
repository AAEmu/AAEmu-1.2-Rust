//! Operations that run concurrently on the server, landed deterministically through the
//! `interleave` seam (no thread, no timing): the deferred join effects (chat channels,
//! `InParty`) are applied after the guard is dropped and then verified against the team
//! state (module header), and a reply reads the inviter's online flag between two lock
//! sections. The hook runs on the test thread exactly between the two steps.
use std::cell::Cell;
use std::rc::Rc;

use super::*;
use crate::managers::chat::ChannelRef;
use crate::opcodes::sc;
use crate::test_support::{spawn_player, test_character, test_server, TestClient};

/// Three online characters (ids 1..=3, owner first) in render distance of each other.
fn players(server: &GameServer) -> (Vec<TestClient>, Vec<PlayerRef>) {
    let mut clients = Vec::new();
    let mut players = Vec::new();
    for id in 1..=3u32 {
        let client = TestClient::new(server, id, 1000 + id);
        let p = spawn_player(
            server,
            &client,
            test_character(id, &format!("P{id}"), 100.0 + id as f32, 100.0),
            0x100 + id,
        );
        p.data().is_online = true;
        clients.push(client);
        players.push(p);
    }
    (clients, players)
}

/// `owner` invites `target`, who accepts (the production flow, no hook installed).
fn invite_and_accept(server: &GameServer, owner: &PlayerRef, target: &PlayerRef) {
    let teams = &server.systems.social.teams;
    teams.ask_to_join(server, owner, "", 0, true, Some(target));
    teams.reply_to_join_team(
        server,
        target,
        0,
        true,
        owner.character_id,
        false,
        "",
        false,
    );
}

/// The hook installed on this thread; it is removed when the value is dropped.
struct Installed;

fn install(hook: impl Fn(&TeamManager, Interleave) + 'static) -> Installed {
    let hook: InterleaveHook = Rc::new(hook);
    INTERLEAVE.with(|h| *h.borrow_mut() = Some(hook));
    Installed
}

impl Drop for Installed {
    fn drop(&mut self) {
        INTERLEAVE.with(|h| *h.borrow_mut() = None);
    }
}

/// Removes `victim` from its team the first time one of its join effects is verified (what
/// a kick on another thread does between the apply and the verify).
struct KickWindow {
    fired: Rc<Cell<bool>>,
    _installed: Installed,
}

impl KickWindow {
    fn open(victim: u32) -> Self {
        let fired = Rc::new(Cell::new(false));
        let seen = fired.clone();
        let installed = install(move |teams, at| {
            let Interleave::JoinApplied {
                team_id,
                character_id,
            } = at
            else {
                return;
            };
            if character_id != victim || seen.replace(true) {
                return;
            }
            let removed = teams
                .lock()
                .teams
                .get_mut(&team_id)
                .expect("team")
                .remove_member(victim);
            assert!(removed, "the victim was a member until the kick");
        });
        Self {
            fired,
            _installed: installed,
        }
    }

    /// Whether a join effect of the victim reached its verify (the seam was exercised).
    fn fired(&self) -> bool {
        self.fired.get()
    }
}

/// Party 0 chat channel of the team (character 1 owns it; the owner only names the channel).
fn party_zero(server: &GameServer, team_id: u32) -> ChannelRef {
    party_channel(server, team_id, 0, 1)
}

/// The chat frames (join / leave) of a drained connection, in order.
fn chat_ops(client: &mut TestClient) -> Vec<u16> {
    client
        .drain_ops()
        .into_iter()
        .filter(|op| *op == sc::SCJoinedChatChannelPacket || *op == sc::SCLeavedChatChannelPacket)
        .collect()
}

#[tokio::test]
async fn is_member_now_follows_the_team_state() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_id = teams.active_team_by_unit(1).expect("team").id;
    assert!(teams.is_member_now(team_id, 1));
    assert!(teams.is_member_now(team_id, 2));
    assert!(!teams.is_member_now(team_id, 3), "not invited");
    assert!(!teams.is_member_now(team_id + 100, 1), "no such team");
    assert!(!teams.is_member_now(0, 1), "team 0 is nobody's team");
    teams.ask_risky_team(&server, &p[0], team_id, 2, risky_action::KICK);
    assert!(!teams.is_member_now(team_id, 2), "kicked");
}

/// A member that stays a member keeps every effect (the verify must not undo the normal
/// path): the party chat and `InParty` of the whole party.
#[tokio::test]
async fn effects_of_members_are_kept() {
    let server = test_server();
    let (mut clients, p) = players(&server);
    invite_and_accept(&server, &p[0], &p[1]);
    invite_and_accept(&server, &p[0], &p[2]);
    let team_id = server
        .systems
        .social
        .teams
        .active_team_by_unit(1)
        .expect("team")
        .id;
    let channel = party_zero(&server, team_id);
    for (client, player) in clients.iter_mut().zip(&p) {
        assert!(player.data().in_party);
        assert!(channel.contains(player));
        assert_eq!(
            chat_ops(client),
            vec![sc::SCJoinedChatChannelPacket],
            "joined once and never left"
        );
    }
}

/// `reply_to_join_team` into an existing team: the accepted member is kicked after its
/// state change and before its effects are verified.
#[tokio::test]
async fn a_late_join_of_a_member_that_left_meanwhile_is_taken_back() {
    let server = test_server();
    let (mut clients, p) = players(&server);
    invite_and_accept(&server, &p[0], &p[1]);
    let team_id = server
        .systems
        .social
        .teams
        .active_team_by_unit(1)
        .expect("team")
        .id;
    clients.iter_mut().for_each(|c| {
        c.drain();
    });
    let window = KickWindow::open(3);
    invite_and_accept(&server, &p[0], &p[2]);
    assert!(window.fired(), "the late joiner reached its verify");
    let channel = party_zero(&server, team_id);
    assert!(!p[2].data().in_party, "InParty is taken back");
    assert!(!channel.contains(&p[2]), "the party chat is taken back");
    assert_eq!(
        chat_ops(&mut clients[2]),
        vec![sc::SCJoinedChatChannelPacket, sc::SCLeavedChatChannelPacket]
    );
    // The members that are still in the team keep theirs.
    assert!(channel.contains(&p[0]) && channel.contains(&p[1]));
    assert!(p[0].data().in_party && p[1].data().in_party);
}

/// `CreateNewTeam` through `reply_to_join_team`: the invited member is kicked before its
/// effects are verified; the owner keeps its own.
#[tokio::test]
async fn a_new_team_member_that_left_meanwhile_is_taken_back() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    let window = KickWindow::open(2);
    invite_and_accept(&server, &p[0], &p[1]);
    assert!(window.fired());
    let team_id = teams.active_team_by_unit(1).expect("team").id;
    let channel = party_zero(&server, team_id);
    assert!(!p[1].data().in_party && !channel.contains(&p[1]));
    assert!(p[0].data().in_party && channel.contains(&p[0]));
}

/// `UpdateAtLogin` of a member that was kicked in the meantime.
#[tokio::test]
async fn a_login_of_a_member_that_left_meanwhile_is_taken_back() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_id = teams.active_team_by_unit(1).expect("team").id;
    let window = KickWindow::open(2);
    teams.update_at_login(&server, &p[1]);
    assert!(window.fired());
    let channel = party_zero(&server, team_id);
    assert!(!p[1].data().in_party && !channel.contains(&p[1]));
    assert!(p[0].data().in_party && channel.contains(&p[0]));
}

/// `ConvertToRaid` joins every member to the raid chat; one was kicked meanwhile.
#[tokio::test]
async fn a_raid_conversion_skips_a_member_that_left_meanwhile() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_id = teams.active_team_by_unit(1).expect("team").id;
    let window = KickWindow::open(2);
    teams.convert_to_raid(&server, &p[0], team_id);
    assert!(window.fired());
    let raid = raid_channel(&server, team_id, 1);
    assert!(raid.contains(&p[0]));
    assert!(!raid.contains(&p[1]), "the raid chat is taken back");
}

/// `CreateSoloTeam` (GM `soloparty`) for a character that left its team meanwhile.
#[tokio::test]
async fn a_solo_team_member_that_left_meanwhile_is_taken_back() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    let window = KickWindow::open(1);
    teams.create_solo_team(&server, &p[0], true);
    assert!(window.fired());
    assert!(!p[0].data().in_party);
    let channels = server.chat.list_all_channels();
    assert!(channels.iter().all(|c| !c.contains(&p[0])));
}

/// `MoveTeamMember` joins the two characters the client names to their new party chat; the
/// client can name any online character, so only members may keep the join.
#[tokio::test]
async fn a_member_move_never_joins_a_non_member_to_the_party_chat() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_id = teams.active_team_by_unit(1).expect("team").id;
    // Slot 1 <-> 0 with a member and an outsider (character 3) named as the two targets.
    teams.move_team_member(&server, &p[0], team_id, 2, 3, 1, 0);
    let channel = party_zero(&server, team_id);
    assert!(channel.contains(&p[1]), "the named member is re-joined");
    assert!(!channel.contains(&p[2]), "the outsider is not");
    // The kicked-in-the-window variant: the named member leaves before the verify.
    let window = KickWindow::open(2);
    teams.move_team_member(&server, &p[0], team_id, 2, 0, 0, 1);
    assert!(window.fired());
    assert!(!channel.contains(&p[1]), "the move join is taken back");
}

/// A `MoveTeamMember` that moves nobody (a slot out of range) used to take its two characters
/// out of their party chat anyway; the verify of the leave puts them back.
#[tokio::test]
async fn a_move_that_moves_nobody_keeps_the_party_chat() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_id = teams.active_team_by_unit(1).expect("team").id;
    let channel = party_zero(&server, team_id);
    assert!(channel.contains(&p[0]) && channel.contains(&p[1]));
    teams.move_team_member(&server, &p[0], team_id, 1, 2, 0, 200);
    assert!(
        channel.contains(&p[0]) && channel.contains(&p[1]),
        "nobody moved, nobody left the party chat"
    );
}

/// A pending invitation of `inviter` for `target` (what `invite_locked` inserts).
fn invitation(inviter: &PlayerRef, target: &PlayerRef) -> Invitation {
    Invitation {
        team_id: 0,
        owner: inviter.clone(),
        target: target.clone(),
        time_ms: util::now_ms(),
        is_party: true,
        is_area: false,
    }
}

/// `ReplyToJoinTeam` reads the inviter's online flag between two lock sections. When the
/// invitation is replaced by another inviter's in between, the flag belongs to the old
/// inviter: the reply reads it again instead of answering `TEAM_INVITOR_OFFLINE` and
/// dropping the fresh invitation.
#[tokio::test]
async fn a_reply_reads_the_inviter_flag_again_when_the_invitation_was_replaced() {
    let server = test_server();
    let (mut clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    // P1 invites P2 and goes offline: its flag is what the first read sees.
    teams.ask_to_join(&server, &p[0], "", 0, true, Some(&p[1]));
    p[0].data().is_online = false;
    // P3's invitation replaces it between the two lock sections of P2's reply.
    let replaced = Rc::new(Cell::new(false));
    let seen = replaced.clone();
    let (invitee, inviter) = (p[1].clone(), p[2].clone());
    let _installed = install(move |teams, at| {
        if at == (Interleave::InviterRead { target_id: 2 }) && !seen.replace(true) {
            let fresh = invitation(&inviter, &invitee);
            teams.lock().invitations.insert(2, fresh);
        }
    });
    clients[1].drain();
    teams.reply_to_join_team(&server, &p[1], 0, true, 3, false, "", false);
    assert!(
        replaced.get(),
        "the replacement landed between the sections"
    );
    let team = teams.active_team_by_unit(2).expect("P2 joined P3's team");
    assert!(team.is_member(3) && !team.is_member(1));
    assert!(
        !clients[1].drain_ops().contains(&sc::SCErrorMsgPacket),
        "no TEAM_INVITOR_OFFLINE for the stale flag"
    );
    assert_eq!(teams.invitation_count(), 0, "the invitation was consumed");
}

/// An invitation that keeps changing under the reply is not answered at all (like the
/// answer of a stale popup): no team, no error, the pending invitation stays.
#[tokio::test]
async fn a_reply_is_ignored_when_the_invitation_keeps_changing() {
    let server = test_server();
    let (mut clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    teams.ask_to_join(&server, &p[0], "", 0, true, Some(&p[1]));
    let reads = Rc::new(Cell::new(0u32));
    let counted = reads.clone();
    let (invitee, inviters) = (p[1].clone(), [p[2].clone(), p[0].clone()]);
    let _installed = install(move |teams, at| {
        if at != (Interleave::InviterRead { target_id: 2 }) {
            return;
        }
        // P3, then P1 again: the invitation differs from every snapshot.
        let n = counted.replace(counted.get() + 1) as usize;
        let fresh = invitation(&inviters[n % 2], &invitee);
        teams.lock().invitations.insert(2, fresh);
    });
    clients[1].drain();
    teams.reply_to_join_team(&server, &p[1], 0, true, 3, false, "", false);
    assert_eq!(reads.get(), 2, "one retry, then it gives up");
    assert!(teams.active_team_by_unit(2).is_none());
    assert!(clients[1].drain().is_empty());
    assert_eq!(teams.invitation_count(), 1, "the pending invitation stays");
}

/// What another team's accept does between a removal's state change and the removal's
/// effects: the victim becomes a member of a new team (`team_id`, owned by `owner`) and that
/// team's join effects ran (`InParty`). Fires once, when the effects of the next operation
/// start.
struct AdoptWindow {
    fired: Rc<Cell<bool>>,
    _installed: Installed,
}

impl AdoptWindow {
    fn open(victim: &PlayerRef, owner: &PlayerRef, team_id: u32) -> Self {
        let fired = Rc::new(Cell::new(false));
        let seen = fired.clone();
        let (victim, owner) = (victim.clone(), owner.clone());
        let installed = install(move |teams, at| {
            if at != Interleave::EffectsStart || seen.replace(true) {
                return;
            }
            let mut team = Team::new(team_id, owner.character_id, true);
            team.add_member(&owner);
            team.add_member(&victim);
            teams.lock().teams.insert(team_id, team);
            victim.data().in_party = true;
        });
        Self {
            fired,
            _installed: installed,
        }
    }

    fn fired(&self) -> bool {
        self.fired.get()
    }
}

/// A kick's effects run after its guard is dropped, so another team can take the character
/// first. The cleanup of the old team must not clear the flag of the team that has it now.
#[tokio::test]
async fn a_kick_cleanup_keeps_the_flag_of_the_team_that_took_the_character_meanwhile() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_a = teams.active_team_by_unit(1).expect("team").id;
    let window = AdoptWindow::open(&p[1], &p[2], 900);
    teams.ask_risky_team(&server, &p[0], team_a, 2, risky_action::KICK);
    assert!(window.fired(), "team 900 took P2 before the effects ran");
    assert!(teams.is_member_now(900, 2) && !teams.is_member_now(team_a, 2));
    assert!(
        p[1].data().in_party,
        "InParty follows the current membership"
    );
    // The party fell apart with its last follower: the kicker's flag is cleared.
    assert!(!p[0].data().in_party);
}

/// The same for a dismissal (the disband loop of `ask_risky_team`).
#[tokio::test]
async fn a_dismissal_cleanup_keeps_the_flag_of_the_team_that_took_the_character_meanwhile() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_a = teams.active_team_by_unit(1).expect("team").id;
    let window = AdoptWindow::open(&p[1], &p[2], 900);
    teams.ask_risky_team(&server, &p[0], team_a, 0, risky_action::DISMISS);
    assert!(window.fired(), "team 900 took P2 before the effects ran");
    assert!(teams.active_team(team_a).is_none());
    assert!(
        p[1].data().in_party,
        "InParty follows the current membership"
    );
    assert!(!p[0].data().in_party);
}

/// The same for the zombie-team disband of the 500 ms tick.
#[tokio::test]
async fn a_zombie_disband_cleanup_keeps_the_flag_of_the_team_that_took_the_character_meanwhile() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_a = teams.active_team_by_unit(1).expect("team").id;
    crate::world::session::set_online(&server, &p[0], false);
    crate::world::session::set_online(&server, &p[1], false);
    let now = util::now_ms();
    teams.update_all_teams(&server, now);
    assert!(
        teams.active_team(team_a).is_some(),
        "inside the grace period"
    );
    let window = AdoptWindow::open(&p[1], &p[2], 900);
    teams.update_all_teams(&server, now + OFFLINE_TEAM_DISBAND_GRACE_MS);
    assert!(window.fired(), "team 900 took P2 before the effects ran");
    assert!(teams.active_team(team_a).is_none());
    assert!(
        p[1].data().in_party,
        "InParty follows the current membership"
    );
    assert!(!p[0].data().in_party);
}

/// A join effect that is undone because its character is no longer a member of the team must
/// leave `InParty` at the truth too: the character is a member of another team now.
#[tokio::test]
async fn an_undone_join_keeps_the_flag_of_the_team_the_character_is_in_now() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_a = teams.active_team_by_unit(1).expect("team").id;
    // P3's accept into team A is verified after P3 was kicked and taken by team 900.
    let moved = Rc::new(Cell::new(false));
    let seen = moved.clone();
    let victim = p[2].clone();
    let _installed = install(move |teams, at| {
        let Interleave::JoinApplied {
            team_id,
            character_id: 3,
        } = at
        else {
            return;
        };
        if seen.replace(true) {
            return;
        }
        let mut st = teams.lock();
        assert!(st.teams.get_mut(&team_id).expect("team").remove_member(3));
        let mut team = Team::new(900, victim.character_id, true);
        team.add_member(&victim);
        st.teams.insert(900, team);
    });
    invite_and_accept(&server, &p[0], &p[2]);
    assert!(moved.get());
    assert!(teams.is_member_now(900, 3) && !teams.is_member_now(team_a, 3));
    assert!(p[2].data().in_party, "a member of team 900 stays in party");
}

/// A kick's chat leaves run after its guard is dropped, so the member can be invited back
/// first (another marked raid member invites it and it accepts). The cleanup must not remove
/// the chat memberships that newer join restored.
#[tokio::test]
async fn a_kick_cleanup_keeps_the_chat_a_member_regained_meanwhile() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_id = teams.active_team_by_unit(1).expect("team").id;
    teams.convert_to_raid(&server, &p[0], team_id);
    let raid = raid_channel(&server, team_id, 1);
    let party = party_zero(&server, team_id);
    assert!(raid.contains(&p[1]) && party.contains(&p[1]));
    // The newer join: P2 is a member again (the freed slot, so the same party) and its join
    // effects ran before the kick's effects start.
    let landed = Rc::new(Cell::new(false));
    let seen = landed.clone();
    let (rejoined, raid_now, party_now) = (p[1].clone(), raid.clone(), party.clone());
    let _installed = install(move |teams, at| {
        if at != Interleave::EffectsStart || seen.replace(true) {
            return;
        }
        let slot = teams
            .lock()
            .teams
            .get_mut(&team_id)
            .expect("team")
            .add_member(&rejoined);
        assert!(slot.is_some(), "the kicked member's slot is free");
        raid_now.join(&rejoined);
        party_now.join(&rejoined);
    });
    teams.ask_risky_team(&server, &p[0], team_id, 2, risky_action::KICK);
    assert!(landed.get(), "the rejoin landed before the kick's effects");
    assert!(teams.is_member_now(team_id, 2));
    assert!(
        raid.contains(&p[1]) && party.contains(&p[1]),
        "the old cleanup keeps the memberships of the newer join"
    );
}

/// `settle_in_party` reads the membership again after its write: a team that took the
/// character in between (its own effects not run yet) is followed, not overwritten.
#[tokio::test]
async fn settling_follows_a_membership_change_that_lands_after_the_write() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let landed = Rc::new(Cell::new(false));
    let seen = landed.clone();
    let victim = p[1].clone();
    let _installed = install(move |teams, at| {
        if at != (Interleave::InPartyWritten { character_id: 2 }) || seen.replace(true) {
            return;
        }
        let mut team = Team::new(900, 2, true);
        team.add_member(&victim);
        teams.lock().teams.insert(900, team);
    });
    // The flag of a team the character has left.
    p[1].data().in_party = true;
    settle_in_party(&server, &p[1]);
    assert!(
        landed.get(),
        "a team took the character after the first write"
    );
    assert!(
        p[1].data().in_party,
        "the second pass wrote the new membership"
    );
}

/// A raid with P1 and P2 where P2 sits in party 1 (slot 5): a party chat join prepared for
/// party 0 (the slot P2 had when the join was queued) is stale, one for party 1 is not, and a
/// character that is no member never keeps a join.
#[tokio::test]
async fn a_party_chat_join_for_a_party_the_member_left_is_taken_back() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_id = teams.active_team_by_unit(1).expect("team").id;
    teams.convert_to_raid(&server, &p[0], team_id);
    assert!(teams
        .lock()
        .teams
        .get_mut(&team_id)
        .expect("team")
        .move_member(1, 5));
    assert_eq!(teams.party_number_now(team_id, 2), Some(1));
    assert_eq!(teams.party_number_now(team_id, 3), None, "not a member");
    let (zero, one) = (
        party_channel(&server, team_id, 0, 1),
        party_channel(&server, team_id, 1, 1),
    );
    // The move's own effects: P2 left its old party chat.
    zero.leave(&p[1]);

    join_party_chat_verified(&server, team_id, 1, 0, &p[1]);
    assert!(
        !zero.contains(&p[1]),
        "the stale join of party 0 is taken back"
    );
    join_party_chat_verified(&server, team_id, 1, 1, &p[1]);
    assert!(one.contains(&p[1]), "the join of its current party stays");
    join_party_chat_verified(&server, team_id, 1, 0, &p[2]);
    assert!(!zero.contains(&p[2]), "a non-member keeps nothing");
}

/// The accept of a raid member runs its effects while the raid owner moves the member to
/// another party: the move's leave of the old party chat has already run, so the accept's
/// join of the old party (from its team copy) is stale and is taken back.
#[tokio::test]
async fn an_accept_does_not_keep_the_old_party_chat_of_a_member_moved_meanwhile() {
    let server = test_server();
    let (_clients, p) = players(&server);
    let teams = &server.systems.social.teams;
    invite_and_accept(&server, &p[0], &p[1]);
    let team_id = teams.active_team_by_unit(1).expect("team").id;
    teams.convert_to_raid(&server, &p[0], team_id);
    let zero = party_channel(&server, team_id, 0, 1);
    let moved = Rc::new(Cell::new(false));
    let seen = moved.clone();
    let (channel, joiner) = (zero.clone(), p[2].clone());
    let _installed = install(move |teams, at| {
        let Interleave::JoinApplied {
            team_id,
            character_id: 3,
        } = at
        else {
            return;
        };
        // The stale join of party 0 was just applied; the raid owner's move lands now.
        if !channel.contains(&joiner) || seen.replace(true) {
            return;
        }
        let mut st = teams.lock();
        let team = st.teams.get_mut(&team_id).expect("team");
        let slot = team.index_of(3).expect("member");
        assert!(team.move_member(slot as u8, 5));
    });
    invite_and_accept(&server, &p[0], &p[2]);
    assert!(
        moved.get(),
        "the move landed between the join and its verify"
    );
    assert_eq!(teams.party_number_now(team_id, 3), Some(1));
    assert!(!zero.contains(&p[2]), "the old party chat is not kept");
}
