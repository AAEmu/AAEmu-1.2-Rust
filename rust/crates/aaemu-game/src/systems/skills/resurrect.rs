//! `CSResurrectCharacterPacket`: the temple / in-place resurrection of a dead character
//! (respawn point, HP / MP, `SCCharacterResurrectedPacket`, the revival debuffs of
//! `ApplyRevivalDebuffs`, underwater reset and breath refill).

use aaemu_commons::PacketStream;
use futures::future::BoxFuture;
use rand::Rng;

use crate::data::world_data::{conflict_type, Portal};
use crate::error::Result;
use crate::models::character::DEFAULT_INSTANCE_ID;
use crate::net::connection::GameConnection;
use crate::net::registry::ClientPacket;
use crate::packets::world::SCUnitPointsPacket;
use crate::server::GameServer;
use crate::world::{PlayerRef, WorldObject};

use super::packets_special::{SCCharacterResurrectedPacket, SCLoadInstancePacket};
use super::stats::{stat, StatKind};
use super::types::{KillReason, SkillCaster};

/// `RespawnCooldownDurationMs` (5 min).
pub const RESPAWN_COOLDOWN_DURATION_MS: i32 = 300_000;
/// `BuffConstants.WeakenedBody` (PvE death penalty).
pub const WEAKENED_BODY: u32 = 1128;
/// `BuffConstants.RespawnCooldown` (5 min cooldown after temple-revive).
pub const RESPAWN_COOLDOWN: u32 = 2385;
/// `BuffConstants.WarZoneLeech` (PvP death penalty in War zones).
pub const WAR_ZONE_LEECH: u32 = 4424;
/// The "Temple Priestess" NPC template used as respawn point inside instances.
const TEMPLE_PRIESTESS_TEMPLATE_ID: u32 = 502;
/// `FactionsEnum.NuiaAlliance` / `HaranyaAlliance`.
const NUIA_ALLIANCE: u32 = 148;
const HARANYA_ALLIANCE: u32 = 149;

/// `CSResurrectCharacterPacket` (level 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSResurrectCharacterPacket {
    pub in_place: bool,
}

impl ClientPacket for CSResurrectCharacterPacket {
    const NAME: &'static str = "CSResurrectCharacterPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            in_place: s.read_bool()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            tracing::debug!("ResurrectCharacter, InPlace: {}", self.in_place);
            if let Some(p) = conn.active_char.clone() {
                resurrect_character(server, &p, self.in_place);
            }
            Ok(())
        })
    }
}

/// A temple resurrection destination: the portal and the world instance it lives in.
struct RespawnPoint {
    portal: Portal,
    instance_id: u32,
}

impl RespawnPoint {
    fn in_place(instance_id: u32) -> Self {
        Self {
            portal: Portal::default(),
            instance_id,
        }
    }
}

/// The current zone group's `indun_zones` row, if the zone belongs to an instance dungeon.
fn dungeon_zone_of(
    server: &GameServer,
    zone_key: u32,
) -> Option<&crate::systems::instances::data::IndunZone> {
    let group_id = server.game_data.zones.zone_by_key(zone_key)?.group_id;
    server
        .game_data
        .systems
        .instances
        .indun
        .dungeon_zone(group_id)
}

/// The instance's arrival point for `zone_key`: the zone's `world_spawns.json` position, which
/// is exactly what `Dungeon.MoveCharacterToDungeon` / `MoveCharacterToSystemInstance` teleports
/// the party to (`worlds::zone_spawn_position`).
///
/// The `has_graveyard = true` revival ("Revive at the entrance") must send the player back to
/// that point, not to the Temple Priestess statue: the 1.2 client selects the entrance revival
/// itself (`X2Player.IsInIndunWithGraveyard`), and the statue stands beside the arrival pool
/// (Sharpwind's Priestess of Nui at 718.7 / 329.9 / 168.3 is 20 m from the pool's green
/// arrival portal).
fn instance_arrival_point(
    server: &GameServer,
    instance_id: u32,
    world_id: u32,
    zone_key: u32,
) -> Option<crate::data::world_templates::WorldSpawnPosition> {
    let world = server.world.world(instance_id);
    let template = match world.as_deref() {
        Some(w) => &w.template,
        None => server.game_data.worlds.get_world_template_by_id(world_id)?,
    };
    let sp = crate::systems::instances::worlds::zone_spawn_position(server, template, zone_key)?;
    // A zone without a `world_spawns.json` row keeps the default (0, 0, 0); never use it.
    (sp.x != 0.0 || sp.y != 0.0).then_some(sp)
}

/// The respawn point of `CSResurrectCharacterPacket` (`new Portal()` = X 0 when none).
fn respawn_portal(server: &GameServer, p: &PlayerRef) -> RespawnPoint {
    let (instance_id, world_id, zone_key, faction_id) = {
        let c = p.data();
        let t = c.unit.transform;
        (t.instance_id, t.world_id, t.zone_id, c.unit.faction_id)
    };
    if let Some(portal) = crate::systems::housing::dominion::headquarters::respawn_portal(server, p)
    {
        return RespawnPoint {
            portal,
            instance_id,
        };
    }
    if instance_id != DEFAULT_INSTANCE_ID {
        // `indun_zones.has_graveyard = true`: the 1.2 client offers "Revive at the entrance"
        // (`X2Player.IsInIndunWithGraveyard`, `FUN_39495b20` = record+1 != 0). The entrance is
        // the instance's arrival point, the same position the enter-dungeon teleport uses
        // (`worlds::zone_spawn_position`), never the Temple Priestess statue.
        if dungeon_zone_of(server, zone_key).is_some_and(|z| z.has_graveyard) {
            if let Some(sp) = instance_arrival_point(server, instance_id, world_id, zone_key) {
                let yaw = sp.yaw.to_radians();
                return RespawnPoint {
                    portal: Portal {
                        world_id,
                        zone_id: if sp.zone_id != 0 {
                            sp.zone_id
                        } else {
                            zone_key
                        },
                        x: sp.x,
                        y: sp.y,
                        z: sp.z,
                        z_rot: yaw,
                        yaw,
                        ..Portal::default()
                    },
                    instance_id,
                };
            }
            tracing::warn!(
                "No arrival point for {} ({}) in instance {} (has_graveyard = true); \
                 reviving in place",
                p.data().unit.name,
                p.character_id,
                instance_id
            );
            return RespawnPoint::in_place(instance_id);
        }
        // `indun_zones.has_graveyard = false`: the 1.2 client shows the ordinary "Revive"
        // window instead of "Revive at the entrance" (`X2Player.IsInIndunWithGraveyard`,
        // `FUN_39495b20` = record+1 != 0), so the ordinary shrine destination comes first.
        // The exact destination is inferred (`PORTING.md`).
        let no_graveyard = dungeon_zone_of(server, zone_key).is_some_and(|z| !z.has_graveyard);
        if no_graveyard && server.config.dungeons.no_graveyard_use_return_portal {
            let portal = crate::systems::npcs::portal::closest_return_portal(server, p);
            // `id == 0` is `closest_return_portal`'s fail-safe (the current position); a real
            // return point has an id and a main-world zone.
            if portal.id != 0 && portal.zone_id != 0 {
                return RespawnPoint {
                    portal,
                    instance_id: DEFAULT_INSTANCE_ID,
                };
            }
        }
        // Inside dungeons or other instances, just respawn at the nearest Priestess
        let mut portal = Portal::default();
        let world = server.world.world(instance_id);
        let priestess = world.as_ref().and_then(|w| {
            w.all_npcs()
                .into_iter()
                .find(|n| n.data().unit.template_id == TEMPLE_PRIESTESS_TEMPLATE_ID)
                .map(|n| (WorldObject::from(&n), w.clone()))
        });
        if let Some((npc, w)) = priestess {
            let pos = w.world_pos_rot(&npc);
            let zone_id = npc.transform().zone_id;
            let mut rng = rand::thread_rng();
            portal.world_id = world_id;
            portal.zone_id = zone_id;
            portal.x = pos.x + rng.gen_range(1..3) as f32;
            portal.y = pos.y + rng.gen_range(1..3) as f32;
            portal.z = pos.z;
            portal.z_rot = pos.rot_z;
            portal.yaw = pos.rot_z;
            return RespawnPoint {
                portal,
                instance_id,
            };
        }
        if no_graveyard && server.config.dungeons.no_graveyard_use_return_portal {
            // The instance's saved entry position (`MainWorldPosition`) is the exit the code
            // already knows; never fall through to the in-place (0, 0, 0) portal silently.
            if let Some(mwp) = p.data().main_world_position {
                tracing::warn!(
                    "No return portal for {} ({}) in instance {} (has_graveyard = false); \
                     using the instance entry position ({:.1}, {:.1}, {:.1})",
                    p.data().unit.name,
                    p.character_id,
                    instance_id,
                    mwp.x,
                    mwp.y,
                    mwp.z
                );
                return RespawnPoint {
                    portal: Portal {
                        world_id: mwp.world_id,
                        zone_id: mwp.zone_id,
                        x: mwp.x,
                        y: mwp.y,
                        z: mwp.z,
                        z_rot: mwp.rot_z,
                        yaw: mwp.rot_z,
                        ..Portal::default()
                    },
                    instance_id: DEFAULT_INSTANCE_ID,
                };
            }
            tracing::warn!(
                "No graveyard, return portal or entry position for {} ({}) in instance {}; \
                 reviving in place",
                p.data().unit.name,
                p.character_id,
                instance_id
            );
        }
        return RespawnPoint::in_place(instance_id);
    }
    // Check if the current zone is at War and if it has special respawn areas for factions
    let mut use_portal_id = 0;
    if let Some(zone) = server.game_data.zones.zone_by_key(zone_key) {
        let conflict = server.systems.social.conflicts.get(zone.group_id);
        if let Some(c) = conflict.filter(|c| c.current_state == conflict_type::WAR) {
            let mother_id = server
                .game_data
                .factions
                .get(faction_id)
                .map_or(0, |f| f.mother_id);
            match mother_id {
                NUIA_ALLIANCE => use_portal_id = c.nuia_return_point_id,
                HARANYA_ALLIANCE => use_portal_id = c.harihara_return_point_id,
                _ => {}
            }
        }
    }
    // Try to get a faction specific respawn
    let faction_portal = if use_portal_id > 0 {
        crate::systems::npcs::portal::respawn_by_id(server, use_portal_id)
    } else {
        None
    };
    // Find the closest return portal (in the world) for the player if none has been found yet
    RespawnPoint {
        portal: faction_portal
            .unwrap_or_else(|| crate::systems::npcs::portal::closest_return_portal(server, p)),
        instance_id: DEFAULT_INSTANCE_ID,
    }
}

/// Grants a pending in-place resurrection offer: `SCNotifyResurrectionPacket` of the
/// `Resurrection` special effect or the free resurrection below `minimum_exp_loss_level`.
pub fn offer_resurrection(p: &PlayerRef) {
    p.data().skill_char.resurrect_offer = true;
}

/// `CSResurrectCharacterPacket.Read` for the active character.
pub fn resurrect_character(server: &GameServer, p: &PlayerRef, in_place: bool) {
    if p.data().unit.hp > 0 {
        return;
    }
    if in_place && !p.data().skill_char.resurrect_offer {
        tracing::warn!(
            "ResurrectCharacter in place refused, {} ({}): no pending resurrection offer",
            p.data().unit.name,
            p.character_id
        );
        return;
    }
    let respawn = if in_place {
        RespawnPoint::in_place(p.data().unit.transform.instance_id)
    } else {
        respawn_portal(server, p)
    };
    let portal = respawn.portal;
    let me = WorldObject::from(p);
    let hp = {
        let mut c = p.data();
        let (max_hp, max_mp) = (c.unit.max_hp, c.unit.max_mp);
        // Any resurrection consumes the offer, including the temple choice.
        c.skill_char.resurrect_offer = false;
        if in_place {
            let hp_pct = c.skill_char.resurrect_hp_percent as f32 / 100.0;
            let mp_pct = c.skill_char.resurrect_mp_percent as f32 / 100.0;
            c.unit.hp = (max_hp as f32 * hp_pct) as i32;
            c.unit.mp = (max_mp as f32 * mp_pct) as i32;
            c.skill_char.resurrect_hp_percent = 1;
            c.skill_char.resurrect_mp_percent = 1;
        } else {
            c.unit.hp = (f64::from(max_hp) * 0.1) as i32;
            c.unit.mp = (f64::from(max_mp) * 0.1) as i32;
        }
        c.unit.hp
    };
    super::combat::post_update_current_hp(server, &me, Some(&me), 0, hp, KillReason::Unknown);

    let packet = if portal.x != 0.0 {
        SCCharacterResurrectedPacket {
            unit_id: p.obj_id,
            x: portal.x,
            y: portal.y,
            z: portal.z,
            z_rot: portal.z_rot,
        }
    } else {
        let pos = match super::geometry::world_of(server, &me) {
            Some(w) => w.world_pos_rot(&me),
            None => me.transform().local(),
        };
        // GetResurrectablePosition (x2game sub_3925D3B0, rays 0x107 | 0x8F): the ground under
        // the body (collision service; the body's z otherwise)
        let z = crate::systems::ai::collision::ground_height_or(
            server,
            me.transform().instance_id,
            pos.x,
            pos.y,
            pos.z,
            || pos.z,
        );
        SCCharacterResurrectedPacket {
            unit_id: p.obj_id,
            x: pos.x,
            y: pos.y,
            z,
            z_rot: 0.0,
        }
    };
    super::broadcast(server, &me, &packet, true);
    if portal.x != 0.0 {
        // The client does not answer a resurrection with `CSTeleportEnded`: apply the temple /
        // statue position to the server copy now so the destination zone (and its buffs) is
        // re-evaluated even if no movement packet follows.
        let current_instance = p.data().unit.transform.instance_id;
        if respawn.instance_id == current_instance {
            crate::world::session::resurrect_at(
                server,
                p,
                crate::models::transform::Transform {
                    instance_id: current_instance,
                    zone_id: portal.zone_id,
                    x: portal.x,
                    y: portal.y,
                    z: portal.z,
                    rot_z: portal.z_rot,
                    ..Default::default()
                },
            );
        } else {
            // The destination is in another world (an open-world return portal of a
            // `has_graveyard = false` instance, or the saved instance entry). Move the server
            // copy and make the client load the destination world, like
            // `systems::instances::worlds::return_to_main_world`.
            p.data().set_disabled_set_position(true);
            crate::world::session::teleport_character(
                server,
                p,
                crate::models::transform::Transform {
                    instance_id: respawn.instance_id,
                    zone_id: portal.zone_id,
                    x: portal.x,
                    y: portal.y,
                    z: portal.z,
                    rot_z: portal.z_rot,
                    ..Default::default()
                },
            );
            let world_template_id = server
                .world
                .world(respawn.instance_id)
                .map_or(0, |w| w.template.id);
            p.send(SCLoadInstancePacket {
                instance_id: world_template_id,
                zone_id: portal.zone_id,
                x: portal.x,
                y: portal.y,
                z: portal.z,
                ang_x: 0.0,
                ang_y: 0.0,
                ang_z: portal.z_rot,
            });
        }
    }
    let (hp, mp) = {
        let c = p.data();
        (c.unit.hp, c.unit.mp)
    };
    super::broadcast(
        server,
        &me,
        &SCUnitPointsPacket {
            obj_id: p.obj_id,
            hp,
            mp,
        },
        true,
    );

    // Route death-debuffs based on death context (set by Character.DoDie).
    apply_revival_debuffs(server, p, in_place);

    super::water::set_player_under_water(server, p, false);
    let lung = stat(server, &me, StatKind::LungCapacity) as u32;
    p.data().breath = lung;
    // The glider a lost trade pack had replaced (kept while the character was dead).
    crate::systems::economy::inventory::reequip_previous_glider(server, p);
}

/// `ApplyRevivalDebuffs(character, inPlace)`: in place (player-res) → no debuffs; PvP death in
/// a War zone → Leech + Respawn-CD; other PvP death → Respawn-CD only; PvE death → Weakened
/// Body + Respawn-CD.
fn apply_revival_debuffs(server: &GameServer, p: &PlayerRef, in_place: bool) {
    let (war_zone, pvp) = {
        let mut c = p.data();
        let flags = (c.died_in_pvp_war_zone, c.died_in_pvp);
        if in_place || flags.0 {
            c.died_in_pvp_war_zone = false;
            c.died_in_pvp = false;
        } else if flags.1 {
            c.died_in_pvp = false;
        }
        flags
    };
    if in_place {
        return;
    }
    let buffs: &[(u32, i32)] = if war_zone {
        &[
            (WAR_ZONE_LEECH, 0),
            (RESPAWN_COOLDOWN, RESPAWN_COOLDOWN_DURATION_MS),
        ]
    } else if pvp {
        &[(RESPAWN_COOLDOWN, RESPAWN_COOLDOWN_DURATION_MS)]
    } else {
        &[
            (WEAKENED_BODY, 0),
            (RESPAWN_COOLDOWN, RESPAWN_COOLDOWN_DURATION_MS),
        ]
    };
    let me = WorldObject::from(p);
    for &(buff_id, forced) in buffs {
        // ApplyBuff: an unknown template is skipped silently.
        let Some(t) = server.game_data.systems.skills.buff(buff_id).cloned() else {
            continue;
        };
        let b = super::buffs::new_buff(
            server,
            &t,
            Some(&me),
            &SkillCaster::Unit(p.obj_id),
            None,
            crate::world::now_ms(),
        );
        super::buffs::add_buff(server, &me, b, 0, forced.max(0));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::mysql::MySqlPoolOptions;

    use super::*;
    use crate::config::AppConfig;
    use crate::data::world_data::Zone;
    use crate::data::world_templates::{
        WorldSpawnLocation, WorldSpawnPosition, WorldTemplate, WorldTemplates,
    };
    use crate::data::GameData;
    use crate::models::transform::Transform;
    use crate::systems::instances::data::IndunZone;
    use crate::systems::npcs::portal::PortalTables;
    use crate::test_support::{
        spawn_player, test_character, test_server_with, TestClient, TEST_ZONE,
    };

    const MIRAGE_ZONE: u32 = 260;
    const MIRAGE_GROUP: u32 = 49;
    const GRAVEYARD_ZONE: u32 = 261;
    const GRAVEYARD_GROUP: u32 = 55;
    /// The template id of the synthetic graveyard instance in these tests.
    const GRAVEYARD_WORLD_ID: u32 = 100;

    fn data() -> GameData {
        let mut d = GameData::empty();
        for (zone_key, group_id) in [
            (TEST_ZONE, 1),
            (MIRAGE_ZONE, MIRAGE_GROUP),
            (GRAVEYARD_ZONE, GRAVEYARD_GROUP),
        ] {
            d.zones.zones.insert(
                zone_key,
                Zone {
                    id: zone_key,
                    name: format!("zone_{zone_key}"),
                    zone_key,
                    group_id,
                    ..Zone::default()
                },
            );
        }
        d.systems.instances.indun.zones.insert(
            MIRAGE_GROUP,
            IndunZone {
                zone_group_id: MIRAGE_GROUP,
                name: "mirage".into(),
                pvp: false,
                has_graveyard: false,
                ..IndunZone::default()
            },
        );
        d.systems.instances.indun.zones.insert(
            GRAVEYARD_GROUP,
            IndunZone {
                zone_group_id: GRAVEYARD_GROUP,
                name: "graveyard".into(),
                has_graveyard: true,
                ..IndunZone::default()
            },
        );
        d.systems.npcs.portal = PortalTables::from_parts(
            vec![Portal {
                id: 86,
                name: "Respawn".into(),
                x: 150.0,
                y: 100.0,
                z: 10.0,
                sub_zone_id: 1,
                ..Portal::default()
            }],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .expect("portal tables");
        d
    }

    fn dead_player_in(server: &GameServer, zone_key: u32) -> (TestClient, PlayerRef) {
        let client = TestClient::new(server, 1, 1);
        let mut c = test_character(1, "A", 100.0, 100.0);
        c.unit.hp = 0;
        let p = spawn_player(server, &client, c, 0x101);
        {
            let mut d = p.data();
            d.unit.transform.instance_id = GRAVEYARD_WORLD_ID;
            d.unit.transform.world_id = GRAVEYARD_WORLD_ID;
            d.unit.transform.zone_id = zone_key;
        }
        (client, p)
    }

    /// A server whose graveyard instance template carries `spawn` as its zone 261 arrival point
    /// (`world_spawns.json`), like the shipped instance rows. `test_server_with` replaces the
    /// world catalog, so the templates are installed on the `GameData` before `GameServer::new`.
    fn server_with_arrival_point(spawn: Option<WorldSpawnPosition>) -> GameServer {
        let mut d = data();
        let main = WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE);
        let mut instance = WorldTemplate::synthetic(
            GRAVEYARD_WORLD_ID,
            "instance_graveyard",
            2,
            2,
            GRAVEYARD_ZONE,
        );
        let mut lookups = Vec::new();
        if let Some(spawn) = spawn {
            instance
                .xml_world
                .zones
                .get_mut(&GRAVEYARD_ZONE)
                .expect("zone")
                .spawn_position = spawn;
            lookups.push(WorldSpawnLocation {
                name: Some(instance.name.clone()),
                spawn_position: Some(spawn),
            });
        }
        d.worlds =
            WorldTemplates::from_templates(vec![main, instance], lookups).expect("test worlds");
        let mut config = AppConfig::default();
        config.connections.mysql_provider.host = "127.0.0.1".into();
        config.connections.mysql_provider.port = 1; // nothing listens here
        let db = MySqlPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy_with(config.connections.mysql_provider.connect_options());
        GameServer::new(config, db, d)
    }

    #[tokio::test]
    async fn no_graveyard_instance_uses_the_return_portal() {
        let server = test_server_with(data());
        let (_client, p) = dead_player_in(&server, MIRAGE_ZONE);
        let r = respawn_portal(&server, &p);
        assert_eq!(r.instance_id, DEFAULT_INSTANCE_ID);
        assert_eq!(r.portal.id, 86);
        assert_eq!((r.portal.x, r.portal.y), (150.0, 100.0));
        assert_ne!(r.portal.x, 0.0);
    }

    #[tokio::test]
    async fn graveyard_instance_resurrects_at_the_zone_arrival_point() {
        let spawn = WorldSpawnPosition {
            world_id: GRAVEYARD_WORLD_ID,
            zone_id: GRAVEYARD_ZONE,
            x: 739.4,
            y: 326.7,
            z: 166.2,
            yaw: -89.3565,
            ..WorldSpawnPosition::default()
        };
        let server = server_with_arrival_point(Some(spawn));
        let (_client, p) = dead_player_in(&server, GRAVEYARD_ZONE);
        let r = respawn_portal(&server, &p);
        // "Revive at the entrance" (`has_graveyard = true`) is the instance's arrival point,
        // the same position the enter-dungeon teleport uses - not the Temple Priestess.
        assert_eq!(r.instance_id, GRAVEYARD_WORLD_ID);
        assert_eq!((r.portal.x, r.portal.y, r.portal.z), (739.4, 326.7, 166.2));
        assert_eq!(r.portal.zone_id, GRAVEYARD_ZONE);
        assert!((r.portal.z_rot - (-89.3565f32).to_radians()).abs() < 1e-6);
    }

    #[tokio::test]
    async fn graveyard_instance_without_an_arrival_point_revives_in_place() {
        let server = server_with_arrival_point(None);
        let (_client, p) = dead_player_in(&server, GRAVEYARD_ZONE);
        let r = respawn_portal(&server, &p);
        // No spawn row: the fixed code must not fall back to an NPC statue's own position.
        assert_eq!(r.instance_id, GRAVEYARD_WORLD_ID);
        assert_eq!(r.portal.x, 0.0);
    }

    #[tokio::test]
    async fn no_graveyard_without_a_return_portal_uses_the_entry_position() {
        let mut d = data();
        d.systems.npcs.portal =
            PortalTables::from_parts(Vec::new(), Vec::new(), Vec::new(), Vec::new())
                .expect("portal tables");
        let server = test_server_with(d);
        let (_client, p) = dead_player_in(&server, MIRAGE_ZONE);
        p.data().main_world_position = Some(Transform {
            instance_id: DEFAULT_INSTANCE_ID,
            world_id: 1,
            zone_id: TEST_ZONE,
            x: 321.0,
            y: 654.0,
            z: 12.0,
            ..Transform::default()
        });
        let r = respawn_portal(&server, &p);
        assert_eq!(r.instance_id, DEFAULT_INSTANCE_ID);
        assert_eq!(
            (r.portal.x, r.portal.y, r.portal.z, r.portal.zone_id),
            (321.0, 654.0, 12.0, TEST_ZONE)
        );
    }

    #[tokio::test]
    async fn config_off_keeps_the_priestess_behaviour_for_no_graveyard_instances() {
        let server = test_server_with(data());
        server
            .config
            .update(|c| c.dungeons.no_graveyard_use_return_portal = false);
        let (_client, p) = dead_player_in(&server, MIRAGE_ZONE);
        let r = respawn_portal(&server, &p);
        assert_eq!(r.instance_id, 100);
        assert_eq!(r.portal.x, 0.0);
    }
}
