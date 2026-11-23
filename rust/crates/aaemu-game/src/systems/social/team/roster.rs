use super::{looting_method, member_role, PlayerRef};

/// `LootingRule`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LootingRule {
    pub loot_method: u8,
    pub minimum_grade: u8,
    pub loot_master: u32,
    pub roll_for_bind_on_pickup: bool,
}

impl Default for LootingRule {
    fn default() -> Self {
        Self {
            loot_method: looting_method::ROTATE_WINNER,
            minimum_grade: 2, // Grand+
            loot_master: 0,
            roll_for_bind_on_pickup: true,
        }
    }
}

impl LootingRule {
    /// `LootingRule.Write`.
    pub fn write(&self, s: &mut aaemu_commons::PacketStream) {
        s.write_u8(self.loot_method)
            .write_u8(self.minimum_grade)
            .write_u32(self.loot_master)
            .write_bool(self.roll_for_bind_on_pickup);
    }

    /// `LootingRule.Read`.
    pub fn read(
        s: &mut aaemu_commons::PacketStream,
    ) -> std::result::Result<Self, aaemu_commons::MarshalError> {
        Ok(Self {
            loot_method: s.read_u8()?,
            minimum_grade: s.read_u8()?,
            loot_master: s.read_u32()?,
            roll_for_bind_on_pickup: s.read_bool()?,
        })
    }
}

/// `TeamMember`.
#[derive(Debug, Clone)]
pub struct TeamMember {
    /// `Character` (the object of the last session; replaced on reconnect by `ChangeStatus`).
    pub character: PlayerRef,
    /// The member's `Character.IsOnline` as last reported to the manager by
    /// `UpdateAtLogin` / `SetOffline` (a joining member is online). Kept here so the manager can
    /// decide and broadcast without taking a character data lock.
    pub online: bool,
    pub role: u8,
    pub has_gone_round_robin: bool,
}

/// `Team`.
#[derive(Debug, Clone)]
pub struct Team {
    pub id: u32,
    pub owner_id: u32,
    pub is_party: bool,
    /// `Members` (50 slots).
    pub members: Vec<Option<TeamMember>>,
    pub looting_rule: LootingRule,
    /// `MarksList` (type, obj / character id).
    pub marks: [(u8, u32); 12],
    /// `PingPosition` (x, y, z).
    pub ping_position: (f32, f32, f32),
    /// `AllOfflineSince` (unix ms; `None` = `DateTime.MinValue`).
    pub all_offline_since_ms: Option<i64>,
}

impl Team {
    pub fn new(id: u32, owner_id: u32, is_party: bool) -> Self {
        Self {
            id,
            owner_id,
            is_party,
            members: vec![None; 50],
            looting_rule: LootingRule::default(),
            marks: [(0, 0); 12],
            ping_position: (0.0, 0.0, 0.0),
            all_offline_since_ms: None,
        }
    }

    /// `ResetMarks`.
    pub fn reset_marks(&mut self) {
        self.marks = [(0, 0); 12];
    }

    /// `IsMarked(id)`.
    pub fn is_marked(&self, id: u32) -> bool {
        self.marks.iter().any(|&(_, obj)| obj == id)
    }

    /// `MembersCount`.
    pub fn members_count(&self) -> usize {
        self.members.iter().flatten().count()
    }

    /// Whether the team takes no more members (`MembersCount() >= (IsParty ? 5 : 50)`).
    pub fn is_full(&self) -> bool {
        self.members_count() >= if self.is_party { 5 } else { 50 }
    }

    /// `MembersOnlineCount`.
    pub fn members_online_count(&self) -> usize {
        self.members.iter().flatten().filter(|m| m.online).count()
    }

    /// `IsMember(id)`.
    pub fn is_member(&self, id: u32) -> bool {
        self.members
            .iter()
            .flatten()
            .any(|m| m.character.character_id == id)
    }

    /// `GetNewOwner` (first online member that is not the owner; 0 = none).
    pub fn new_owner(&self) -> u32 {
        self.members
            .iter()
            .flatten()
            .find(|m| m.character.character_id != self.owner_id && m.online)
            .map_or(0, |m| m.character.character_id)
    }

    /// `ChangeRole(id, role)`.
    pub fn change_role(&mut self, id: u32, role: u8) -> bool {
        for m in self.members.iter_mut().flatten() {
            if m.character.character_id != id {
                continue;
            }
            if m.role == role {
                return false;
            }
            m.role = role;
            return true;
        }
        false
    }

    /// `AddMember(unit)`: (slot index, party index) of the new member.
    pub fn add_member(&mut self, p: &PlayerRef) -> Option<(usize, usize)> {
        let i = self.members.iter().position(Option::is_none)?;
        self.members[i] = Some(TeamMember {
            character: p.clone(),
            // A slot is only ever filled from a live invitation / login / GM session.
            online: true,
            role: member_role::UNDECIDED,
            has_gone_round_robin: false,
        });
        Some((i, Self::party_of(i)))
    }

    /// `RemoveMember(id)`.
    pub fn remove_member(&mut self, id: u32) -> bool {
        match self.index_of(id) {
            Some(i) => {
                self.members[i] = None;
                true
            }
            None => false,
        }
    }

    /// `MoveMember(id, id2, from, to)` (out of range indices fail like the C# `catch`).
    pub fn move_member(&mut self, from: u8, to: u8) -> bool {
        let (from, to) = (usize::from(from), usize::from(to));
        if from >= self.members.len() || to >= self.members.len() {
            return false;
        }
        self.members.swap(from, to);
        true
    }

    /// `ChangeStatus(unit)`: points the member at the character's current object. The caller
    /// updates [`TeamMember::online`] with the login / logout it is handling.
    pub fn change_status(&mut self, p: &PlayerRef) -> Option<usize> {
        let i = self.index_of(p.character_id)?;
        if let Some(m) = self.members[i].as_mut() {
            m.character = p.clone();
        }
        Some(i)
    }

    /// `GetIndex(id)`.
    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.members
            .iter()
            .position(|m| m.as_ref().is_some_and(|m| m.character.character_id == id))
    }

    /// `Team.GetParty(index)`.
    pub fn party_of(index: usize) -> usize {
        if index < 5 {
            0
        } else {
            index / 5
        }
    }

    /// `GetPartyCounts`.
    pub fn party_counts(&self) -> [u8; 10] {
        let mut res = [0u8; 10];
        for (i, m) in self.members.iter().enumerate() {
            if m.is_some() {
                res[Self::party_of(i)] += 1;
            }
        }
        res
    }

    /// The online member characters (slot order).
    pub fn online_members(&self) -> Vec<PlayerRef> {
        self.members
            .iter()
            .flatten()
            .filter(|m| m.online)
            .map(|m| m.character.clone())
            .collect()
    }

    /// `BroadcastPacket(packet, id)`: every online member except `exclude`.
    pub fn broadcast(&self, packet: &dyn crate::net::packet::ServerPacket, exclude: u32) {
        let mut out = crate::net::connection::Fanout::new(packet);
        for m in self.members.iter().flatten() {
            if m.character.character_id == exclude || !m.online {
                continue;
            }
            out.send(&m.character.conn);
        }
    }

    /// The party number of a character (`GetPartyChat`: slot / 5, 0 when not a member).
    pub fn party_number_of(&self, character_id: u32) -> u32 {
        self.index_of(character_id).map_or(0, |i| (i / 5) as u32)
    }
}
