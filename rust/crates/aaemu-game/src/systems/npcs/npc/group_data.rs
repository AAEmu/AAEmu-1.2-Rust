//! `NpcGroupGameData` (`GameData/NpcGroupGameData.cs`): the `npc_groups` and
//! `npc_group_members` tables.
//!
//! The C# loads both tables (`NpcGameData`/`NpcGroupGameData`) but never uses them: its
//! `NpcSpawnerNpc.SpawnNpcGroup` just called `SpawnNpc` with the group id as an NPC template id.
//! Here the NPC spawner expands an `"NpcGroup"` member row into the group's member NPCs (see
//! `npc::spawner::spawn_npc_group`); the group row itself is not a spawnable unit.
//!
//! `is_leader` is loaded only; the runtime membership (`is_move_leader`, the formation offsets /
//! tension and `aggro_rule_id`) is attached to the spawned NPCs by `npc/group.rs` and read by
//! `systems::ai::group` (1.2 `formation_GoToOnPoint` / `NpcAggroTable::CollectSharedAggroMembers`).
//! `enable_respawn` stays unused (no consumer was found); see `PORTING.md`.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::data::sqlite::for_each_row;
use crate::systems::DataLoadCtx;

/// `NpcGroup` (`npc_groups` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpcGroup {
    pub id: u32,
    pub name: String,
    /// `NpcGroupAggroRuleKind` (raw; 0 / 1 / 2 in the shipped data).
    pub aggro_rule_id: i32,
    pub enable_respawn: bool,
}

/// `NpcGroupMember` (`npc_group_members` row).
#[derive(Debug, Clone, PartialEq)]
pub struct NpcGroupMember {
    pub id: u32,
    pub npc_group_id: u32,
    /// NPC template id of this member.
    pub npc_id: u32,
    pub is_leader: bool,
    pub is_move_leader: bool,
    pub formation_offset_x: f32,
    pub formation_offset_y: f32,
    pub formation_offset_z: f32,
    pub formation_tension: f32,
}

/// `NpcGroupGameData` (`server.game_data.systems.npcs.npc_groups`).
#[derive(Debug, Default)]
pub struct NpcGroupData {
    groups: HashMap<u32, NpcGroup>,
    /// Members by group id, in row (`id`) order, with a lookup by member row id.
    members: HashMap<u32, Vec<NpcGroupMember>>,
    member_ids: HashSet<u32>,
}

impl NpcGroupData {
    /// Loads both tables from `ctx`. Bad rows are skipped (the catalog keeps loading), which
    /// deviates from the C# `Dictionary` exception: the shipped tables are clean today, but a
    /// single malformed row must not abort the whole game-data load.
    pub fn load(ctx: &DataLoadCtx<'_>) -> Result<Self, crate::data::GameDataError> {
        Ok(Self::load_from(ctx.conn)?)
    }

    /// [`Self::load`] from a compact DB connection.
    pub fn load_from(conn: &Connection) -> rusqlite::Result<Self> {
        let mut groups = HashMap::new();
        for_each_row(conn, "SELECT * FROM npc_groups", [], |r| {
            let id = r.u32("id")?;
            if id == 0 {
                tracing::warn!("npc_groups: skipping row with id 0");
                return Ok(());
            }
            if groups.contains_key(&id) {
                tracing::warn!("npc_groups: skipping duplicate id {id}");
                return Ok(());
            }
            groups.insert(
                id,
                NpcGroup {
                    id,
                    name: r.string_or("name", "")?,
                    aggro_rule_id: r.i32("aggro_rule_id")?,
                    enable_respawn: r.bool_str("enable_respawn")?,
                },
            );
            Ok(())
        })?;

        let mut members: HashMap<u32, Vec<NpcGroupMember>> = HashMap::new();
        let mut member_ids = HashSet::new();
        for_each_row(
            conn,
            "SELECT * FROM npc_group_members ORDER BY id",
            [],
            |r| {
                let id = r.u32("id")?;
                let npc_group_id = r.u32("npc_group_id")?;
                let npc_id = r.u32("npc_id")?;
                if !groups.contains_key(&npc_group_id) {
                    tracing::warn!(
                        "npc_group_members {id}: skipping unknown npc_group_id {npc_group_id}"
                    );
                    return Ok(());
                }
                if npc_id == 0 {
                    tracing::warn!("npc_group_members {id}: skipping npc_id 0");
                    return Ok(());
                }
                if !member_ids.insert(id) {
                    tracing::warn!("npc_group_members: skipping duplicate id {id}");
                    return Ok(());
                }
                members
                    .entry(npc_group_id)
                    .or_default()
                    .push(NpcGroupMember {
                        id,
                        npc_group_id,
                        npc_id,
                        is_leader: r.bool_str("is_leader")?,
                        is_move_leader: r.bool_str("is_move_leader")?,
                        formation_offset_x: r.f32("formation_offset_x")?,
                        formation_offset_y: r.f32("formation_offset_y")?,
                        formation_offset_z: r.f32("formation_offset_z")?,
                        formation_tension: r.f32("formation_tension")?,
                    });
                Ok(())
            },
        )?;

        Ok(Self {
            groups,
            members,
            member_ids,
        })
    }

    /// Builds the tables from rows (tests / fixtures). Rows of an unknown group, duplicate ids
    /// and members without an NPC template are skipped, like [`Self::load_from`].
    pub fn from_parts(groups: Vec<NpcGroup>, members: Vec<NpcGroupMember>) -> Self {
        let mut group_map: HashMap<u32, NpcGroup> = HashMap::new();
        for group in groups {
            if group.id == 0 || group_map.contains_key(&group.id) {
                continue;
            }
            group_map.insert(group.id, group);
        }
        let mut member_map: HashMap<u32, Vec<NpcGroupMember>> = HashMap::new();
        let mut member_ids = HashSet::new();
        for member in members {
            if !member_ids.insert(member.id)
                || member.npc_id == 0
                || !group_map.contains_key(&member.npc_group_id)
            {
                continue;
            }
            member_map
                .entry(member.npc_group_id)
                .or_default()
                .push(member);
        }
        Self {
            groups: group_map,
            members: member_map,
            member_ids,
        }
    }

    /// `GetNpcGroup(id)`.
    pub fn group(&self, id: u32) -> Option<&NpcGroup> {
        self.groups.get(&id)
    }

    /// `GetNpcGroupMembers(npcGroupId)` in row order (empty when the group is unknown).
    pub fn members(&self, npc_group_id: u32) -> &[NpcGroupMember] {
        self.members.get(&npc_group_id).map_or(&[], Vec::as_slice)
    }

    /// Number of loaded groups (diagnostics / tests).
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Number of loaded member rows (diagnostics / tests).
    pub fn member_count(&self) -> usize {
        self.member_ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::systems::npcs::npc::spawner_data::NpcSpawnerData;
    use rusqlite::OpenFlags;

    fn in_memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE npc_groups (
                id INT, name TEXT, aggro_rule_id INT, enable_respawn NUM
            );
            CREATE TABLE npc_group_members (
                id INT, npc_group_id INT, npc_id INT, is_leader NUM, is_move_leader NUM,
                formation_offset_x REAL, formation_offset_y REAL, formation_offset_z REAL,
                formation_tension REAL
            );
            INSERT INTO npc_groups VALUES
                (50, 'sea worms', 1, 'f'),
                (50, 'duplicate', 0, 't'),
                (0, 'zero id', 0, 'f'),
                (60, 'orphan members', 0, 't');
            INSERT INTO npc_group_members VALUES
                (191, 50, 8564, 't', 't', 0.0, 0.0, 0.0, 2.0),
                (192, 50, 8566, 'f', 'f', 2.0, -3.0, 0.0, 1.0),
                (193, 50, 8566, 'f', 'f', -2.0, -3.0, 0.0, 1.0),
                (194, 50, 0, 'f', 'f', 0.0, 0.0, 0.0, 1.0),
                (195, 999, 8567, 'f', 'f', 0.0, 0.0, 0.0, 1.0),
                (192, 50, 8568, 'f', 'f', 0.0, 0.0, 0.0, 1.0);
            "#,
        )
        .unwrap();
        conn
    }

    #[test]
    fn loads_members_and_skips_bad_rows() {
        let d = NpcGroupData::load_from(&in_memory()).unwrap();
        assert_eq!(d.group_count(), 2, "duplicate / zero group ids are skipped");
        assert_eq!(d.member_count(), 3, "bad member rows are skipped");
        let group = d.group(50).expect("group 50");
        assert_eq!(group.name, "sea worms");
        assert!(!group.enable_respawn);
        assert_eq!(
            d.members(50).iter().map(|m| m.npc_id).collect::<Vec<_>>(),
            vec![8564, 8566, 8566]
        );
        assert!(d.members(50)[0].is_leader && d.members(50)[0].is_move_leader);
        assert_eq!(d.members(50)[1].formation_offset_x, 2.0);
        assert!(d.members(60).is_empty());
        assert!(d.members(999).is_empty());
    }

    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    fn real_compact_db_npc_groups() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let groups = NpcGroupData::load_from(&conn).unwrap();
        let spawners = NpcSpawnerData::load_from(&conn).unwrap();

        let mut group_rows = 0usize;
        let mut member_rows = 0usize;
        for nsn in spawners.all_npc_spawner_npcs() {
            if nsn.member_type != "NpcGroup" {
                continue;
            }
            group_rows += 1;
            let group = groups.group(nsn.member_id).unwrap_or_else(|| {
                panic!("spawner row {}: group {} missing", nsn.id, nsn.member_id)
            });
            let members = groups.members(group.id);
            assert!(
                !members.is_empty(),
                "spawner row {}: group {} has no members",
                nsn.id,
                group.id
            );
            for member in members {
                member_rows += 1;
                let exists: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM npcs WHERE id = ?1)",
                        [member.npc_id],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert!(
                    exists,
                    "spawner row {}: group {} member npc {} does not exist",
                    nsn.id, group.id, member.npc_id
                );
            }
        }
        assert_eq!(group_rows, 147, "NpcGroup spawner member rows");
        assert_eq!(member_rows, 531, "expanded member NPCs");
        assert_eq!(groups.group_count(), 158, "npc_groups rows");
        assert_eq!(groups.member_count(), 571, "npc_group_members rows");
        println!(
            "npc_groups={} npc_group_members={} NpcGroup spawner rows={} expanded members={}",
            groups.group_count(),
            groups.member_count(),
            group_rows,
            member_rows
        );
    }
}
