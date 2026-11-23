use super::{State, TeamMemberInfo};

/// `ForcedResyncIntervalMs`.
const FORCED_RESYNC_INTERVAL_MS: i64 = 5000;
/// `MemberSyncState`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct MemberSyncState {
    hp: i32,
    max_hp: i32,
    mp: i32,
    max_mp: i32,
    x: f32,
    y: f32,
    z: f32,
    obj_id: u32,
    last_sync_ms: i64,
}

impl MemberSyncState {
    /// `PositionEpsilon`.
    const POSITION_EPSILON: f32 = 0.01;

    /// `HasSameSnapshotAs`.
    fn same_snapshot(&self, o: &Self) -> bool {
        self.hp == o.hp
            && self.max_hp == o.max_hp
            && self.mp == o.mp
            && self.max_mp == o.max_mp
            && (self.x - o.x).abs() < Self::POSITION_EPSILON
            && (self.y - o.y).abs() < Self::POSITION_EPSILON
            && (self.z - o.z).abs() < Self::POSITION_EPSILON
            && self.obj_id == o.obj_id
    }
}

/// `TryClaimSnapshot(recipient, source, now)`.
pub(super) fn try_claim_snapshot(
    st: &mut State,
    recipient: &TeamMemberInfo,
    source: &TeamMemberInfo,
    now_ms: i64,
) -> bool {
    let key = (recipient.id, source.id);
    let next = MemberSyncState {
        hp: source.hp,
        max_hp: source.max_hp,
        mp: source.mp,
        max_mp: source.max_mp,
        x: source.position.0,
        y: source.position.1,
        z: source.position.2,
        obj_id: source.obj_id,
        last_sync_ms: now_ms,
    };
    if let Some(prev) = st.last_sync.get(&key) {
        if prev.same_snapshot(&next) && now_ms - prev.last_sync_ms < FORCED_RESYNC_INTERVAL_MS {
            return false;
        }
    }
    st.last_sync.insert(key, next);
    true
}

/// `ClearSyncState(characterId)`.
pub(super) fn clear_sync_state(st: &mut State, character_id: u32) {
    st.last_sync
        .retain(|&(r, s), _| r != character_id && s != character_id);
}
