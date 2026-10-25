//! Native 1.2 zone-group restrictions (`x2game12.c:1646640`, LoadZoneGroupBannedTagDescs).

use std::collections::HashMap;

use rusqlite::Connection;

use crate::data::sqlite::for_each_row;
use crate::server::GameServer;
use crate::world::WorldObject;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BannedTag {
    pub id: u32,
    pub tag_id: u32,
    pub banned_periods_id: u32,
}

#[derive(Debug, Default)]
pub struct ZoneRules {
    /// Keep conditional rows intact: an unresolved schedule must not become an always-on ban.
    pub banned_tags: HashMap<u32, Vec<BannedTag>>,
    pub item_tags: HashMap<u32, Vec<u32>>,
}

impl ZoneRules {
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut data = Self::default();
        for_each_row(conn, "SELECT * FROM zone_group_banned_tags", [], |row| {
            data.banned_tags
                .entry(row.u32("zone_group_id")?)
                .or_default()
                .push(BannedTag {
                    id: row.u32("id")?,
                    tag_id: row.u32("tag_id")?,
                    banned_periods_id: row.u32("banned_periods_id")?,
                });
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM tagged_items", [], |row| {
            data.item_tags
                .entry(row.u32("item_id")?)
                .or_default()
                .push(row.u32("tag_id")?);
            Ok(())
        })?;
        let conditional = data
            .banned_tags
            .values()
            .flatten()
            .filter(|rule| rule.banned_periods_id != 0)
            .count();
        if conditional != 0 {
            // 1.2 exports nine guard-summon rows with period 24 but no period definition.
            // Native loader evidence establishes the field, not its scheduling semantics.
            tracing::warn!(
                conditional,
                "Scheduled zone banned tags retained but not enforced: native period semantics unresolved"
            );
        }
        Ok(data)
    }

    pub fn bans_any(&self, group: u32, tags: &[u32]) -> bool {
        self.banned_tags.get(&group).is_some_and(|rules| {
            rules
                .iter()
                .any(|rule| rule.banned_periods_id == 0 && tags.contains(&rule.tag_id))
        })
    }

    pub fn bans_item(&self, group: u32, item: u32) -> bool {
        self.item_tags
            .get(&item)
            .is_some_and(|tags| self.bans_any(group, tags))
    }
}

/// Resolve the current sector in the actual instance's template, including attached players.
/// Cached Transform.ZoneId/WorldId may still describe the previous zone or a different world.
pub(crate) fn current_group(server: &GameServer, caster: &WorldObject) -> Option<u32> {
    let world = server.world.world(caster.transform().instance_id)?;
    let pos = world.world_pos_rot(caster);
    server
        .game_data
        .zones
        .zone_by_key(world.template.zone_id_at(pos.x, pos.y))
        .map(|zone| zone.group_id)
}

#[cfg(test)]
#[path = "zone_rules_tests.rs"]
mod tests;
