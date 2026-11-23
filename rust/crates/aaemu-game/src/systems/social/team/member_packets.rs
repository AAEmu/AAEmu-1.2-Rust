use super::{
    util, GameServer, PlayerRef, SCJoinedTeamPacket, SCRefreshTeamMemberPacket, Team, TeamInfo,
    TeamMember, TeamMemberInfo,
};

/// A member snapshot for `TeamMember.Write` / `WritePerson`.
pub fn member_info(server: &GameServer, m: &TeamMember) -> TeamMemberInfo {
    let pos = util::world_position(server, &m.character);
    let c = m.character.data();
    TeamMemberInfo {
        id: c.id,
        name: c.unit.name.clone(),
        race: c.race,
        gender: c.gender,
        level: c.unit.level,
        role: m.role,
        obj_id: m.character.obj_id,
        is_online: c.is_online,
        zone_id: c.unit.transform.zone_id,
        instance_id: c.unit.transform.instance_id,
        hp: c.unit.hp,
        max_hp: c.unit.max_hp,
        mp: c.unit.mp,
        max_mp: c.unit.max_mp,
        position: pos,
        abilities: [c.ability1, c.ability2, c.ability3],
    }
}

/// `Team.Write` data.
pub fn team_info(team: &Team) -> TeamInfo {
    TeamInfo {
        id: team.id,
        owner_id: team.owner_id,
        is_party: team.is_party,
        party_counts: team.party_counts(),
        slots: team
            .members
            .iter()
            .map(|m| match m {
                Some(m) => (m.character.character_id, m.online),
                None => (0, false),
            })
            .collect(),
        marks: team.marks,
        looting_rule: team.looting_rule,
    }
}

/// `new SCJoinedTeamPacket(team)`.
pub(super) fn joined_team_packet(server: &GameServer, team: &Team) -> SCJoinedTeamPacket {
    SCJoinedTeamPacket {
        team: team_info(team),
        members: team
            .members
            .iter()
            .flatten()
            .map(|m| member_info(server, m))
            .collect(),
    }
}

/// `AreInRenderDistance(a, b)`: the same or a neighbouring region (2-region neighbourhood).
pub fn are_in_render_distance(a: &PlayerRef, b: &PlayerRef) -> bool {
    let (Some(ra), Some(rb)) = (a.region(), b.region()) else {
        return false;
    };
    ra.0 == rb.0 && (ra.1 - rb.1).abs() <= 2 && (ra.2 - rb.2).abs() <= 2
}

/// Sends `packet` to the online members captured under the state lock, without cloning a
/// [`Team`] just to broadcast after the guard is dropped.
pub(super) fn send_to_players(
    recipients: &[PlayerRef],
    packet: &dyn crate::net::packet::ServerPacket,
) {
    let mut out = crate::net::connection::Fanout::new(packet);
    for p in recipients {
        out.send(&p.conn);
    }
}

/// `RefreshTeamMemberObjIds(team, newMember, force)`.
pub(super) fn refresh_team_member_obj_ids(team: &Team, new_member: &PlayerRef, force: bool) {
    for m in team.members.iter().flatten() {
        let c = &m.character;
        if c.character_id == new_member.character_id || !m.online {
            continue;
        }
        if !force && are_in_render_distance(c, new_member) {
            continue;
        }
        c.send(SCRefreshTeamMemberPacket {
            team_id: team.id,
            member_id: new_member.character_id,
            obj_id: new_member.obj_id,
        });
        new_member.send(SCRefreshTeamMemberPacket {
            team_id: team.id,
            member_id: c.character_id,
            obj_id: c.obj_id,
        });
    }
}

/// `RefreshAllTeamMemberObjIds(team)` (upper-triangle pairs).
pub(super) fn refresh_all_team_member_obj_ids(team: &Team) {
    let online = team.online_members();
    for i in 0..online.len() {
        for j in i + 1..online.len() {
            if are_in_render_distance(&online[i], &online[j]) {
                continue;
            }
            online[i].send(SCRefreshTeamMemberPacket {
                team_id: team.id,
                member_id: online[j].character_id,
                obj_id: online[j].obj_id,
            });
            online[j].send(SCRefreshTeamMemberPacket {
                team_id: team.id,
                member_id: online[i].character_id,
                obj_id: online[i].obj_id,
            });
        }
    }
}
