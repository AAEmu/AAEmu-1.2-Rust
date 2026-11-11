//! `SphereQuestManager` (quest sign spheres from `quest_sign_sphere.g`, per-player quest
//! sphere triggers, quest-starting spheres, the 500 ms tick), `SphereQuest` /
//! `SphereQuestTrigger` / `SphereQuestStarter` and the sphere part of `SphereGameData`
//! (`IsInsideAreaSphere`, `GetSpheresForQuest`).
//!
//! The C# has one manager per `WorldInstance` (with a static sphere list loaded from the first
//! world); only `main_world` exists here, so there is one manager for the server.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::data::world_data::ZoneData;
use crate::data::world_templates::WorldTemplates;

use super::data::{QuestData, RawSignSphere};
use super::model::sphere_trigger;
use super::packets::SCAreaTeamMessagePacket;
use super::templates::ActKind;
use super::ui_packets::SCQuestAcceptConditionalPacket;
use crate::server::GameServer;
use crate::systems::social::packets::duel::SCAreaChatBubblePacket;
use crate::world::{PlayerRef, WorldObject};

/// `SphereQuest` (one `quest_sign_sphere.g` area, world coordinates).
#[derive(Debug, Clone, PartialEq)]
pub struct SphereQuest {
    pub zone_id: u32,
    pub world_id: String,
    pub quest_id: u32,
    pub component_id: u32,
    pub radius: f32,
    pub xyz: (f32, f32, f32),
}

impl SphereQuest {
    /// `Contains(pos)`: 3D distance (computed in double) <= radius.
    pub fn contains(&self, pos: (f32, f32, f32)) -> bool {
        distance3(self.xyz, pos) <= self.radius
    }
}

/// `MathUtil.CalculateDistance(a, b, true)`.
pub fn distance3(a: (f32, f32, f32), b: (f32, f32, f32)) -> f32 {
    let dx = f64::from(a.0) - f64::from(b.0);
    let dy = f64::from(a.1) - f64::from(b.1);
    let dz = f64::from(a.2) - f64::from(b.2);
    (dx * dx + dy * dy + dz * dz).sqrt() as f32
}

/// One area of `Data/Worlds/<world>/sphere_area_fill.json` (world coordinates).
///
/// The file is produced offline from the 10.x `world_server/quest_area_sphere.g` by
/// `rust/tools/sphere_area_import/sphere_area_import.py`; the converter already dropped blocks
/// whose world/zone/sphere the 1.2 data does not contain. `stype` is the 1.2 `spheres.id`.
#[derive(Debug, Clone, PartialEq)]
pub struct SphereArea {
    pub world: String,
    pub zone: u32,
    pub sphere_id: u32,
    pub xyz: (f32, f32, f32),
    pub radius: f32,
}

impl SphereArea {
    /// 3D distance (computed in double) <= radius, like [`SphereQuest::contains`].
    pub fn contains(&self, pos: (f32, f32, f32)) -> bool {
        distance3(self.xyz, pos) <= self.radius
    }
}

/// Slack for a client sphere start (`CSStartQuestContext`): the client can send its accept
/// shortly after entering, before the server has its latest position, so the start is refused
/// only when the character is clearly outside the sphere (INFERRED; 5 m is a few client ticks
/// of running, well below the 6-30 m radius of the shipped offer spheres).
pub const START_TOLERANCE: f32 = 5.0;

/// Reads every `<app_path>/Data/Worlds/*/sphere_area_fill.json`; a missing file is empty and a
/// malformed row is skipped (a bad data file must not fail the catalog). `known_sphere` is the
/// runtime data-version guard (the 1.2 `spheres` table of this build).
pub fn load_sphere_areas(app_path: &Path, known_sphere: impl Fn(u32) -> bool) -> Vec<SphereArea> {
    let dir = app_path.join("Data").join("Worlds");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = entries.flatten().filter(|e| e.path().is_dir()).collect();
    dirs.sort_by_key(|e| e.file_name());
    let mut out = Vec::new();
    let mut files = 0usize;
    let mut skipped = 0usize;
    for entry in dirs {
        let path = entry.path().join("sphere_area_fill.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let world = entry.file_name().to_string_lossy().to_string();
        let rows: Vec<serde_json::Value> = match serde_json::from_str(&text) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("{}: {e} (ignored)", path.display());
                continue;
            }
        };
        files += 1;
        for row in rows {
            let number = |key: &str| row.get(key).and_then(serde_json::Value::as_f64);
            let (Some(sphere_id), Some(zone), Some(x), Some(y), Some(z), Some(radius)) = (
                number("sphere_id"),
                number("zone"),
                number("x"),
                number("y"),
                number("z"),
                number("radius"),
            ) else {
                skipped += 1;
                continue;
            };
            let (sphere_id, zone) = (sphere_id as u32, zone as u32);
            if radius <= 0.0 || !known_sphere(sphere_id) {
                skipped += 1;
                continue;
            }
            out.push(SphereArea {
                world: world.clone(),
                zone,
                sphere_id,
                xyz: (x as f32, y as f32, z as f32),
                radius: radius as f32,
            });
        }
    }
    if files > 0 {
        tracing::info!(
            "Loaded {} sphere area triggers from {files} world files ({skipped} skipped)",
            out.len()
        );
    }
    out
}

/// Reads every `<app_path>/Data/Worlds/*/quest_sign_sphere_fill.json` (the 10.x-derived
/// geometry of `Docs/wiki/Quest-Audit.md` quests that the 1.2 client pack has no sign sphere
/// for). The file is written by `rust/tools/quest_sign_sphere_import`; unlike the client
/// `quest_sign_sphere.g` areas its rows already hold world coordinates (`origin * 1024 + local`
/// was applied offline by the converter).
///
/// Runtime data-version guards, repeated here so a stale or hand-edited data file cannot
/// inject a sphere the loaded 1.2 data does not know:
/// * the quest template and component exist and the component has an objective sphere act
///   (`QuestActObjSphere` / `QuestActCheckSphere`) for the same `sphere_id`;
/// * the `spheres` row is a `SphereQuest` whose `sphere_quests.quest_id` is the row's quest;
/// * no 1.2 client sign sphere exists for the same `(quest_id, component_id)` (the client pack
///   always wins);
/// * the zone belongs to the named 1.2 world and its `zones.closed` is false;
/// * the radius is finite and positive, and the row is not an exact duplicate.
///
/// A missing file, a malformed row or an unknown field is skipped: a bad data file must not
/// fail the catalog.
pub fn load_sign_sphere_fill(
    app_path: &Path,
    data: &QuestData,
    zones: &ZoneData,
    worlds: &WorldTemplates,
) -> Vec<(String, RawSignSphere)> {
    let dir = app_path.join("Data").join("Worlds");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = entries.flatten().filter(|e| e.path().is_dir()).collect();
    dirs.sort_by_key(|e| e.file_name());
    let mut out = Vec::new();
    let mut files = 0usize;
    let mut skipped = 0usize;
    let mut seen = std::collections::HashSet::new();
    for entry in dirs {
        let world = entry.file_name().to_string_lossy().to_string();
        let path = entry.path().join("quest_sign_sphere_fill.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let rows: Vec<serde_json::Value> = match serde_json::from_str(&text) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("{}: {e} (ignored)", path.display());
                continue;
            }
        };
        files += 1;
        for row in rows {
            let number = |key: &str| row.get(key).and_then(serde_json::Value::as_f64);
            let (Some(quest_id), Some(component_id), Some(sphere_id)) = (
                number("quest_id"),
                number("component_id"),
                number("sphere_id"),
            ) else {
                skipped += 1;
                continue;
            };
            let (Some(zone), Some(x), Some(y), Some(z), Some(radius)) = (
                number("zone"),
                number("x"),
                number("y"),
                number("z"),
                number("radius"),
            ) else {
                skipped += 1;
                continue;
            };
            let (quest_id, component_id, sphere_id, zone) = (
                quest_id as u32,
                component_id as u32,
                sphere_id as u32,
                zone as u32,
            );
            if !radius.is_finite() || radius <= 0.0 {
                skipped += 1;
                continue;
            }
            let Some(component) = data.component(component_id) else {
                skipped += 1;
                continue;
            };
            if component.quest_id != quest_id {
                skipped += 1;
                continue;
            }
            let objective = component.acts.iter().any(|&a| {
                matches!(
                    data.acts[a].kind,
                    ActKind::ObjSphere { sphere_id: id, .. } | ActKind::CheckSphere { sphere_id: id }
                        if id == sphere_id
                )
            });
            if !objective {
                skipped += 1;
                continue;
            }
            let Some(sphere) = data.spheres.get(&sphere_id) else {
                skipped += 1;
                continue;
            };
            if sphere.sphere_detail_type != "SphereQuest" {
                skipped += 1;
                continue;
            }
            let Some(row_quest) = data.sphere_quests.get(&sphere.sphere_detail_id) else {
                skipped += 1;
                continue;
            };
            if row_quest.quest_id != quest_id {
                skipped += 1;
                continue;
            }

            if data
                .sign_spheres
                .iter()
                .any(|s| s.quest_id == quest_id && s.component_id == component_id)
            {
                skipped += 1;
                continue;
            }
            if !zones.zone_by_key(zone).is_some_and(|z| !z.closed) {
                skipped += 1;
                continue;
            }
            if worlds
                .get_world_template_by_zone_key(zone)
                .is_none_or(|w| w.name != world)
            {
                skipped += 1;
                continue;
            }
            let key = (
                quest_id,
                component_id,
                sphere_id,
                zone,
                (x as f32).to_bits(),
                (y as f32).to_bits(),
                (z as f32).to_bits(),
                (radius as f32).to_bits(),
            );
            if !seen.insert(key) {
                skipped += 1;
                continue;
            }
            out.push((
                world.clone(),
                RawSignSphere {
                    zone_id: zone,
                    quest_id,
                    component_id,
                    // The fill file is world coordinates; `Spheres::load` must not add a zone
                    // origin again.
                    local: [x as f32, y as f32, z as f32],
                    radius: radius as f32,
                },
            ));
        }
    }
    if files > 0 {
        tracing::info!(
            "Loaded {} imported quest sign spheres from {files} world files ({skipped} skipped)",
            out.len()
        );
    }
    out
}

/// `SphereQuestTrigger`.
#[derive(Debug, Clone, PartialEq)]
pub struct SphereQuestTrigger {
    /// Identity (the C# compares the objects).
    pub id: u64,
    pub sphere: SphereQuest,
    /// `Owner.Id`.
    pub character_id: u32,
    /// `Quest.TemplateId`.
    pub quest_template_id: u32,
    pub npc_template: u32,
    pub last_check_location: (f32, f32, f32),
    pub tick_rate: i32,
    /// `LastTick` (unix ms, `DateTime.MinValue` = i64::MIN).
    pub last_tick_ms: i64,
}

/// `SphereQuestStarter`.
#[derive(Debug, Clone, PartialEq)]
pub struct SphereQuestStarter {
    pub sphere: SphereQuest,
    pub quest_template_id: u32,
    pub sphere_id: u32,
}

#[derive(Debug, Default)]
struct Spheres {
    /// `_sphereQuests`: component id -> spheres (dictionary insertion order).
    by_component: Vec<(u32, Vec<SphereQuest>)>,
    /// `_questStartingSpheres`.
    starters: Vec<SphereQuestStarter>,
    /// `_questSpheresBasic`.
    basic: Vec<SphereQuestStarter>,
}

fn push_sphere(s: &mut Spheres, sphere_world: &str, r: &RawSignSphere, xyz: (f32, f32, f32)) {
    let sphere = SphereQuest {
        zone_id: r.zone_id,
        world_id: sphere_world.to_string(),
        quest_id: r.quest_id,
        component_id: r.component_id,
        radius: r.radius,
        xyz,
    };
    match s
        .by_component
        .iter_mut()
        .find(|(c, _)| *c == r.component_id)
    {
        Some((_, v)) => v.push(sphere),
        None => s.by_component.push((r.component_id, vec![sphere])),
    }
}

/// The per-character area-sphere state (kept while the character stays in the world).
#[derive(Debug, Default, Clone)]
struct AreaState {
    /// Area indexes the character was inside at the last tick (ascending).
    inside: Vec<usize>,
    /// Unix ms of the last `SCQuestAcceptConditional` offer per sphere id (edge cooldown).
    offered_ms: HashMap<u32, i64>,
    /// Whether the sphere offered a startable quest at the last evaluation (per sphere id).
    /// A false -> true transition while inside offers again (a level-up among others), while
    /// staying eligible does not repeat the offer.
    eligible: HashMap<u32, bool>,
}

/// Minimum interval between two `SCQuestAcceptConditional` offers of the same sphere to the
/// same character (INFERRED; suppresses the offer flapping when the character walks the area
/// edge). It does not gate an eligibility transition while inside (e.g. a level-up).
pub const OFFER_COOLDOWN_MS: i64 = 30_000;

#[derive(Debug, Default)]
struct Triggers {
    list: Vec<SphereQuestTrigger>,
    add_queue: Vec<SphereQuestTrigger>,
    remove_queue: Vec<u64>,
    next_id: u64,
}

/// `SphereQuestManager`.
#[derive(Debug, Default)]
pub struct SphereQuestManager {
    spheres: OnceLock<Spheres>,
    triggers: Mutex<Triggers>,
    /// `_questStartingLastPositionChecks` (character id -> position).
    last_positions: Mutex<HashMap<u32, (f32, f32, f32)>>,
    /// `Data/Worlds/<world>/sphere_area_fill.json` (world coordinates, read-only).
    areas: OnceLock<Vec<SphereArea>>,
    /// Per-character area-sphere state (character id -> inside indexes / offer bookkeeping).
    area_state: Mutex<HashMap<u32, AreaState>>,
    /// Serializes ticks.
    tick_lock: Mutex<()>,
}

impl SphereQuestManager {
    /// `LoadQuestSpheres` (zone-local -> world coordinates with the zone origin cells) +
    /// `Load` (quest starters). `origin(zone)` is `ZoneManager.GetZoneOriginCell`.
    ///
    /// `raw` are the 1.2 client `quest_sign_sphere.g` rows (zone-local). `fill` rows are already
    /// world coordinates (the converter applied the origin cell), so no origin is added to them.
    pub fn load(
        &self,
        data: &QuestData,
        raw: &[RawSignSphere],
        fill: &[(String, RawSignSphere)],
        world_name: &str,
        origin: impl Fn(u32) -> (i32, i32),
    ) {
        let mut s = Spheres::default();
        for r in raw {
            let (ox, oy) = origin(r.zone_id);
            push_sphere(
                &mut s,
                world_name,
                r,
                (
                    ox as f32 * 1024.0 + r.local[0],
                    oy as f32 * 1024.0 + r.local[1],
                    r.local[2],
                ),
            );
        }
        for (sphere_world, r) in fill {
            push_sphere(
                &mut s,
                sphere_world,
                r,
                (r.local[0], r.local[1], r.local[2]),
            );
        }
        // Link quest starters to spheres
        for (component_id, list) in &s.by_component {
            let Some(component) = data.component(*component_id) else {
                continue;
            };
            let sphere_id = data.sphere_id_from_quest(component.quest_id);
            if sphere_id == 0 {
                continue;
            }
            for sphere in list {
                let starter = SphereQuestStarter {
                    sphere: sphere.clone(),
                    quest_template_id: component.quest_id,
                    sphere_id,
                };
                s.basic.push(starter.clone());
                for &a in &component.acts {
                    if matches!(data.acts[a].kind, ActKind::ConAcceptSphere { .. }) {
                        s.starters.push(starter.clone());
                    }
                }
            }
        }
        tracing::info!(
            "Loaded {} quest sign spheres ({} starters)",
            raw.len(),
            s.starters.len()
        );
        let _ = self.spheres.set(s);
    }

    fn spheres(&self) -> Option<&Spheres> {
        self.spheres.get()
    }

    /// Installs the loaded `sphere_area_fill.json` table (once).
    pub fn set_areas(&self, areas: Vec<SphereArea>) {
        let _ = self.areas.set(areas);
    }

    /// Loaded sphere area blocks (tests / diagnostics).
    pub fn area_count(&self) -> usize {
        self.areas.get().map_or(0, Vec::len)
    }

    /// `GetQuestSpheres(componentId)`.
    pub fn quest_spheres(&self, component_id: u32) -> Vec<SphereQuest> {
        self.spheres()
            .and_then(|s| s.by_component.iter().find(|(c, _)| *c == component_id))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// `GetSpheresForQuest(questId)`.
    pub fn spheres_for_quest(&self, quest_id: u32) -> Vec<SphereQuest> {
        self.spheres()
            .map(|s| {
                s.by_component
                    .iter()
                    .flat_map(|(_, v)| v.iter())
                    .filter(|x| x.quest_id == quest_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The quest-starting spheres.
    pub fn starters(&self) -> Vec<SphereQuestStarter> {
        self.spheres()
            .map(|s| s.starters.clone())
            .unwrap_or_default()
    }

    /// Whether any world has area or sign-sphere geometry for `sphere_id`. A sphere without
    /// geometry cannot be position-checked, so [`Self::is_inside_sphere`] does not apply to it.
    pub fn has_sphere_geometry(&self, sphere_id: u32) -> bool {
        self.areas
            .get()
            .is_some_and(|areas| areas.iter().any(|a| a.sphere_id == sphere_id))
            || self.starters().iter().any(|s| s.sphere_id == sphere_id)
    }

    /// Whether `pos` is inside the sphere `sphere_id` of `world_name` right now: a shipped
    /// `sphere_area_fill.json` area or a `quest_sign_sphere.g` starter area for that sphere,
    /// with `tolerance` metres of slack. This is the server-side presence check a client
    /// `CSStartQuestContext` sphere start must pass (both ids come from the client).
    pub fn is_inside_sphere(
        &self,
        world_name: &str,
        sphere_id: u32,
        pos: (f32, f32, f32),
        tolerance: f32,
    ) -> bool {
        if let Some(areas) = self.areas.get() {
            if areas.iter().any(|a| {
                a.world == world_name
                    && a.sphere_id == sphere_id
                    && distance3(a.xyz, pos) <= a.radius + tolerance
            }) {
                return true;
            }
        }
        self.starters().iter().any(|s| {
            s.sphere.world_id == world_name
                && s.sphere_id == sphere_id
                && distance3(s.sphere.xyz, pos) <= s.sphere.radius + tolerance
        })
    }

    /// `SphereGameData.IsInsideAreaSphere(sphereId, value2, worldPosition, requiredComponentId)`.
    pub fn is_inside_area_sphere(
        &self,
        data: &QuestData,
        sphere_id: u32,
        _value2: u32,
        pos: (f32, f32, f32),
        required_component_id: u32,
    ) -> Option<SphereQuest> {
        let db = data.spheres.get(&sphere_id)?;
        if db.sphere_detail_type != "SphereQuest" {
            return None;
        }
        let sq = data.sphere_quests.get(&db.sphere_detail_id)?;
        let required_quest = (required_component_id > 0)
            .then(|| data.component(required_component_id).map(|c| c.quest_id))
            .flatten();
        for s in self.spheres_for_quest(sq.quest_id) {
            let mut matches = required_component_id == 0 || s.component_id == required_component_id;
            if !matches
                && db.trigger_condition_id == sphere_trigger::TRIGGER_EVERY_N_TIME_AFTER
                && required_quest == Some(sq.quest_id)
            {
                matches = true;
            }
            if s.contains(pos) && matches {
                return Some(s);
            }
        }
        None
    }

    /// The `AreaSphere` unit requirement (kind 35) of skills and quest components:
    /// [`Self::is_inside_area_sphere`] (the client sign spheres of the sphere's quest, as in the
    /// C#), or else the requirement sphere's own shipped `sphere_area_fill.json` area.
    ///
    /// Rust addition: the C# only knew the sign spheres, so a requirement whose sphere has no sign
    /// sphere of its `sphere_quests` quest never passed. Quest 2273 "March of Respect" places its
    /// offering with skill 15204, which needs sphere 1152 at Prince Reander's Monument; the 1.2
    /// data links 1152 to quest 2907 (`sphere_quests` 660), which has no sign sphere there, while
    /// the area fill has 1152 (radius 6 m) around the altar. The areas carry no world filter
    /// here (the requirement callers have only a position); sphere ids are unique, so a match
    /// needs the caster at that sphere's coordinates.
    pub fn inside_requirement_sphere(
        &self,
        data: &QuestData,
        sphere_id: u32,
        value2: u32,
        pos: (f32, f32, f32),
    ) -> bool {
        if self
            .is_inside_area_sphere(data, sphere_id, value2, pos, 0)
            .is_some()
        {
            return true;
        }
        data.spheres
            .get(&sphere_id)
            .is_some_and(|db| db.sphere_detail_type == "SphereQuest")
            && self.areas.get().is_some_and(|areas| {
                areas
                    .iter()
                    .any(|a| a.sphere_id == sphere_id && a.contains(pos))
            })
    }

    /// `AddSphereQuestTriggers(owner, quest, componentId, npcTemplateId)`: queued for the next
    /// tick. Returns the number of triggers.
    pub fn add_triggers(
        &self,
        character_id: u32,
        quest_template_id: u32,
        component_id: u32,
        npc_template: u32,
    ) -> usize {
        let spheres = self.quest_spheres(component_id);
        let mut t = self.triggers.lock().expect("triggers poisoned");
        for sphere in &spheres {
            if t.list.iter().chain(&t.add_queue).any(|x| {
                x.character_id == character_id
                    && x.quest_template_id == quest_template_id
                    && x.sphere == *sphere
                    && x.npc_template == npc_template
                    && !t.remove_queue.contains(&x.id)
            }) {
                continue;
            }
            t.next_id += 1;
            let id = t.next_id;
            t.add_queue.push(SphereQuestTrigger {
                id,
                sphere: sphere.clone(),
                character_id,
                quest_template_id,
                npc_template,
                last_check_location: (0.0, 0.0, 0.0),
                tick_rate: 500,
                last_tick_ms: i64::MIN,
            });
        }
        spheres.len()
    }

    /// `RemoveSphereQuestTriggers(ownerId, questId)` (0 = all of the owner): queues the
    /// matching triggers of the *live* list (triggers still in the add queue are not found).
    pub fn remove_triggers(&self, character_id: u32, quest_id: u32) {
        let mut t = self.triggers.lock().expect("triggers poisoned");
        t.add_queue.retain(|x| {
            !(x.character_id == character_id && (quest_id == 0 || x.quest_template_id == quest_id))
        });
        let ids: Vec<u64> = t
            .list
            .iter()
            .filter(|x| {
                x.character_id == character_id && (quest_id == 0 || x.quest_template_id == quest_id)
            })
            .map(|x| x.id)
            .collect();
        t.remove_queue.extend(ids);
    }

    /// Drops everything the sphere manager keeps for a character that leaves the world: its
    /// triggers and its last starter-sphere position. Kept across a logout they made a
    /// re-login inside a sphere look like "was already inside" and never fired the enter
    /// event. (Deviation: the C# kept both until the character was removed from the trigger
    /// list by a quest change.)
    pub fn forget_character(&self, character_id: u32) {
        self.remove_triggers(character_id, 0);
        self.last_positions
            .lock()
            .expect("positions poisoned")
            .remove(&character_id);
        self.area_state
            .lock()
            .expect("area state poisoned")
            .remove(&character_id);
    }

    /// `GetSphereQuestTriggers()` (live list).
    pub fn triggers(&self) -> Vec<SphereQuestTrigger> {
        self.triggers
            .lock()
            .expect("triggers poisoned")
            .list
            .clone()
    }

    /// Enter / leave of the imported area spheres (`sphere_area_fill.json`).
    ///
    /// INFERRED trigger (CAP-UI-7): the 1.2 client only receives `SCAreaChatBubble` for a bubble
    /// sphere on entering / leaving its area; no 1.2 sender was captured. A `SphereBubble` row
    /// sends `SCAreaChatBubble(enter, the player's unit, sphere_detail_id)` and any row with a
    /// non-empty `team_msg` sends `SCAreaTeamMessage(enter, sphere id)` to that player only
    /// (CAP-UI-6: `is_personal_msg` is true for every shipped `team_msg` row; the non-personal
    /// audience is UNKNOWN). On leave only a row with `enter_or_leave` set fires.
    /// `SphereDoodadInteract` casts its authored skill on the doodads inside the area (see
    /// [`dispatch_doodad_interact`]); the other types (SphereSkill / SphereSound / SphereBuff)
    /// are loaded but have no effect here (a separate content decision).
    fn tick_area_spheres(&self, server: &GameServer, now: i64) {
        let Some(areas) = self.areas.get() else {
            return;
        };
        if areas.is_empty() {
            return;
        }
        let data = &server.game_data.systems.quests;
        let mut next: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut inside_players: Vec<(PlayerRef, Vec<usize>)> = Vec::new();
        let mut events: Vec<(PlayerRef, usize, bool)> = Vec::new();
        for player in server.world.all_characters() {
            if player.region().is_none() {
                continue;
            }
            let obj = WorldObject::from(player.clone());
            let Some(world) = server.world.world_of(&obj) else {
                continue;
            };
            let pos = super::events::world_position(server, &obj);
            let inside: Vec<usize> = areas
                .iter()
                .enumerate()
                .filter(|(_, a)| a.world == world.template.name && a.contains(pos))
                .map(|(i, _)| i)
                .collect();
            let previous = self
                .area_state
                .lock()
                .expect("area state poisoned")
                .get(&player.character_id)
                .map(|st| st.inside.clone())
                .unwrap_or_default();
            for &i in &inside {
                if !previous.contains(&i) {
                    events.push((player.clone(), i, true));
                }
            }
            for &i in &previous {
                if inside.binary_search(&i).is_err() {
                    events.push((player.clone(), i, false));
                }
            }
            next.insert(player.character_id, inside.clone());
            inside_players.push((player.clone(), inside));
        }
        {
            let mut states = self.area_state.lock().expect("area state poisoned");
            // Drop characters that left the world; keep their offer bookkeeping while inside.
            states.retain(|id, _| next.contains_key(id));
            for (id, inside) in next {
                states.entry(id).or_default().inside = inside;
            }
        }
        let entered: HashSet<(u32, usize)> = events
            .iter()
            .filter(|(_, _, enter)| *enter)
            .map(|(p, i, _)| (p.character_id, *i))
            .collect();
        for (player, area, enter) in events {
            let Some(sphere) = data.spheres.get(&areas[area].sphere_id) else {
                continue;
            };
            if !enter && !sphere.enter_or_leave {
                continue;
            }
            if sphere.sphere_detail_type == "SphereBubble" {
                player.send(SCAreaChatBubblePacket {
                    enter,
                    unit_obj_id: player.obj_id,
                    bubble_type: sphere.sphere_detail_id,
                });
            }
            if !sphere.team_msg.trim().is_empty() {
                player.send(SCAreaTeamMessagePacket {
                    enter,
                    sphere_id: sphere.id,
                });
            }
            if enter && sphere.sphere_detail_type == "SphereDoodadInteract" {
                dispatch_doodad_interact(server, &player, &areas[area], sphere.sphere_detail_id);
            }
        }
        // `SphereAcceptQuest` offers: on entry and when a character already inside becomes
        // eligible (a level-up, a schedule window opening, ...). The 0x14a packet replaces the
        // automatic start; the client answers with `CSStartQuestContext`, whose position is
        // re-validated by `events::add_quest_from_sphere_impl`. The entry trigger is INFERRED
        // (CAP-QU-2): the area comes from the shipped `sphere_area_fill.json` and the link
        // from `sphere_accept_quest_quests`, but no capture shows the retail send.
        for (player, inside) in inside_players {
            for &area in &inside {
                let Some(sphere) = data.spheres.get(&areas[area].sphere_id) else {
                    continue;
                };
                if sphere.sphere_detail_type != "SphereAcceptQuest" {
                    continue;
                }
                let entered_this_tick = entered.contains(&(player.character_id, area));
                let (was_eligible, last_offer) = self
                    .area_state
                    .lock()
                    .expect("area state poisoned")
                    .get(&player.character_id)
                    .map_or((false, None), |st| {
                        (
                            st.eligible.get(&sphere.id).copied().unwrap_or(false),
                            st.offered_ms.get(&sphere.id).copied(),
                        )
                    });
                let (id, or) = (sphere.id, sphere.or_unit_reqs);
                // `with_quests` returns `None` when the character's quest state is unavailable
                // this tick; the sphere is then skipped without touching its offer state, so a
                // transient failure cannot read as an eligibility transition (and bypass the
                // offer cooldown) on the next tick.
                let Some(can) = super::events::with_quests(server, &player, None, move |ctx| {
                    ctx.can_trigger_sphere(id, or)
                }) else {
                    continue;
                };
                let mut offers: Vec<u32> = Vec::new();
                let mut unavailable = false;
                if can {
                    for quest_id in data.accept_quest_quests(sphere.sphere_detail_id) {
                        // The same rules as a normal quest start, without its refusal
                        // logging / packets: only offer a quest the character could start.
                        match super::events::with_quests(server, &player, None, move |ctx| {
                            ctx.can_offer_quest(quest_id, id)
                        }) {
                            Some(true) => offers.push(quest_id),
                            Some(false) => {}
                            None => {
                                unavailable = true;
                                break;
                            }
                        }
                    }
                }
                if unavailable {
                    continue;
                }
                let eligible_now = !offers.is_empty();
                let offer = eligible_now
                    && if !entered_this_tick && !was_eligible {
                        // An eligibility transition while inside is server-driven (level-up,
                        // schedule window), not edge flapping: offer it without the cooldown.
                        true
                    } else {
                        last_offer.is_none_or(|last| now - last >= OFFER_COOLDOWN_MS)
                    };
                if offer {
                    for quest_id in offers {
                        player.send(SCQuestAcceptConditionalPacket {
                            sphere_id: id,
                            quest_id,
                        });
                    }
                }
                let mut states = self.area_state.lock().expect("area state poisoned");
                let st = states.entry(player.character_id).or_default();
                st.eligible.insert(id, eligible_now);
                if offer {
                    st.offered_ms.insert(id, now);
                }
            }
        }
    }

    /// One `Tick` (every 500 ms). A throwing quest event handler aborts the rest of the tick,
    /// like the C# exception caught around the whole tick.
    pub fn tick(&self, server: &GameServer) {
        self.tick_at_impl(server, chrono::Utc::now().timestamp_millis());
    }

    /// [`Self::tick`] at a caller-chosen time (tests: the offer cooldown is wall-clock).
    #[cfg(test)]
    pub(crate) fn tick_at(&self, server: &GameServer, now: i64) {
        self.tick_at_impl(server, now);
    }

    fn tick_at_impl(&self, server: &GameServer, now: i64) {
        let _serial = self.tick_lock.lock().expect("tick poisoned");
        self.tick_area_spheres(server, now);
        // Add new player specific triggers
        let list = {
            let mut t = self.triggers.lock().expect("triggers poisoned");
            let remove = std::mem::take(&mut t.remove_queue);
            t.list.retain(|x| !remove.contains(&x.id));
            let added = std::mem::take(&mut t.add_queue);
            t.list.extend(added);
            t.list.clone()
        };
        // Handle player specific Triggers
        for trigger in list {
            let Some(player) = server.world.character_by_id(trigger.character_id) else {
                continue;
            };
            if player.region().is_none() {
                continue;
            }
            match tick_trigger(server, &player, &trigger, now) {
                Ok(Some(last)) => {
                    let mut t = self.triggers.lock().expect("triggers poisoned");
                    if let Some(x) = t.list.iter_mut().find(|x| x.id == trigger.id) {
                        x.last_check_location = last;
                        x.last_tick_ms = now;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!("Error in SphereQuestTrigger tick ! {e}");
                    return;
                }
            }
        }
        // Remove player specific triggers
        {
            let mut t = self.triggers.lock().expect("triggers poisoned");
            let remove = std::mem::take(&mut t.remove_queue);
            for id in remove {
                if let Some(i) = t.list.iter().position(|x| x.id == id) {
                    t.list.remove(i);
                }
            }
        }
        // Handle Global triggers for quest starters
        let Some(world) = server.world.world(0) else {
            return;
        };
        let mut current_positions = HashMap::new();
        for starter in self.starters() {
            if starter.sphere.world_id != world.template.name {
                continue;
            }
            let (sx, sy, _) = starter.sphere.xyz;
            let Some(region) = world.region_by_pos(sx, sy) else {
                continue;
            };
            let has_players = world
                .objects_in_region(region)
                .iter()
                .any(|o| o.as_player().is_some());
            if !has_players {
                continue;
            }
            let mut players: Vec<PlayerRef> = Vec::new();
            for n in world.neighbors(region) {
                for o in world.objects_in_region(n) {
                    if let Some(p) = o.as_player() {
                        if !players.iter().any(|x| x.character_id == p.character_id) {
                            players.push(p.clone());
                        }
                    }
                }
            }
            for p in players {
                let pos = super::events::world_position(server, &WorldObject::from(p.clone()));
                let last = self
                    .last_positions
                    .lock()
                    .expect("positions poisoned")
                    .get(&p.character_id)
                    .copied()
                    .unwrap_or((0.0, 0.0, 0.0));
                let old_inside = starter.sphere.contains(last);
                let new_inside = starter.sphere.contains(pos);
                if !old_inside && new_inside {
                    let data = &server.game_data.systems.quests;
                    let db_sphere = data
                        .spheres
                        .get(&data.sphere_id_from_quest(starter.sphere.quest_id));
                    let can = match db_sphere {
                        None => true,
                        Some(db) => {
                            let (id, or) = (db.id, db.or_unit_reqs);
                            super::events::with_quests(server, &p, None, move |ctx| {
                                ctx.can_trigger_sphere(id, or)
                            })
                            .unwrap_or(false)
                        }
                    };
                    if can {
                        super::events::on_enter_quest_starter_sphere(
                            server,
                            &p,
                            starter.quest_template_id,
                            starter.sphere_id,
                        );
                    }
                }
                current_positions.insert(p.character_id, pos);
            }
        }
        let mut last = self.last_positions.lock().expect("positions poisoned");
        last.extend(current_positions);
        // A character that left the world between `forget_character` and its removal from the
        // regions may have been recorded again by this very tick.
        last.retain(|id, _| server.world.character_by_id(*id).is_some());
    }
}

/// `SphereDoodadInteract`: on entering the sphere, use the authored skill on the doodads inside
/// the sphere's `sphere_area_fill.json` area.
///
/// Evidence: the 10.x `DoodadAreaTriggerRuntime` documents zone area edges of kind 21
/// (`SphereDoodadInteract`) whose `value1` is the `spheres.id`, resolved through
/// `sphere_doodad_interacts` (`id, skill_id, doodad_family_id`). `doodad_families` is empty in
/// the 1.2 compact, so the family filter cannot be resolved; the skill's own `max_range`
/// (4 m for skill 11284) and the sphere area bound the candidates. Sharpwind's sphere 1552
/// ("bridge collapse area", 5 m at the Derelict Bridge) contains only the bridge doodad
/// (5058, phase func `DoodadFuncAreaTrigger`), which advances to the broken model.
fn dispatch_doodad_interact(
    server: &GameServer,
    player: &PlayerRef,
    area: &SphereArea,
    sphere_detail_id: u32,
) {
    let Some(detail) = server
        .game_data
        .systems
        .npcs
        .sphere
        .sphere_doodad_interacts
        .get(sphere_detail_id)
    else {
        return;
    };
    if detail.skill_id == 0 {
        return;
    }
    let max_range = server
        .game_data
        .systems
        .skills
        .skill(detail.skill_id)
        .map_or(0.0, |t| t.max_range as f32)
        .max(1.0);
    let Some(world) = server.world.world_of(&WorldObject::from(player)) else {
        return;
    };
    let me = world.world_pos_rot(&WorldObject::from(player));
    let caster = WorldObject::from(player);
    for doodad in world.all_doodads() {
        let pos = world.world_pos_rot(&WorldObject::from(&doodad));
        if !area.contains((pos.x, pos.y, pos.z)) {
            continue;
        }
        let (dx, dy, dz) = (pos.x - me.x, pos.y - me.y, pos.z - me.z);
        if dx * dx + dy * dy + dz * dz > max_range * max_range {
            continue;
        }
        // The 10.x `DoodadAreaTriggerRuntime.Dispatch` runs the owning `DoodadFuncAreaTrigger`
        // row directly (`DoFunc`) and changes the phase when it asked for the next one; the
        // normal `Doodad.Use` funnel only matches rows by skill id and the shipped rows have
        // none.
        let group = doodad.data().func_group_id;
        let Some(func) = server
            .game_data
            .systems
            .npcs
            .doodad
            .funcs_for_group(group)
            .iter()
            .find(|f| f.func_type == "DoodadFuncAreaTrigger")
            .cloned()
        else {
            continue;
        };
        tracing::debug!(
            "SphereDoodadInteract {}: using doodad {} ({}) for {}",
            sphere_detail_id,
            doodad.obj_id,
            doodad.data().template_id,
            player.data().unit.name
        );
        let ctx = crate::systems::npcs::doodad::runtime::DoodadCtx::new(
            server,
            world.clone(),
            doodad.clone(),
        );
        doodad.data().phase.to_next_phase = true;
        crate::systems::npcs::doodad::phase::do_func(&ctx, Some(&caster), 0, Some(&func));
        if doodad.data().phase.to_next_phase {
            let next = doodad.data().func_group_id as i32;
            crate::systems::npcs::doodad::phase::do_change_phase(&ctx, Some(&caster), next);
        }
    }
}

/// `SphereQuestTrigger.Tick`: returns the new `LastCheckLocation` when the trigger ran.
fn tick_trigger(
    server: &GameServer,
    player: &PlayerRef,
    trigger: &SphereQuestTrigger,
    now: i64,
) -> Result<Option<(f32, f32, f32)>, String> {
    // Imported spheres carry the world they were authored in; a main_world area must not fire
    // inside an instance (and vice versa).
    let Some(world) = server.world.world_of(&WorldObject::from(player.clone())) else {
        return Ok(None);
    };
    if trigger.sphere.world_id != world.template.name {
        return Ok(None);
    }
    let elapsed = if trigger.last_tick_ms == i64::MIN {
        f64::INFINITY
    } else {
        (now - trigger.last_tick_ms) as f64
    };
    if !(trigger.tick_rate > 0 && elapsed > f64::from(trigger.tick_rate)) {
        return Ok(None);
    }
    let data = &server.game_data.systems.quests;
    let db_sphere = data
        .spheres
        .get(&data.sphere_id_from_quest(trigger.sphere.quest_id))
        .map(|d| (d.id, d.or_unit_reqs));
    let mut env = super::events::capture_env(server, player);
    if trigger.npc_template > 0 {
        env.nearby_npcs = super::events::nearby_npcs(server, player);
    }
    let sphere = trigger.sphere.clone();
    let last = trigger.last_check_location;
    let npc_template = trigger.npc_template;
    let r = super::events::with_quests(server, player, Some(env), move |ctx| {
        let active = match db_sphere {
            None => true,
            Some((id, or)) => ctx.can_trigger_sphere(id, or),
        };
        let pos = ctx.env.position;
        if active {
            let (old_inside, new_inside) = if npc_template == 0 {
                // Normal distance check
                (sphere.contains(last), sphere.contains(pos))
            } else {
                // Using NPC Template, find nearby NPCs with it first (GetAround radius 1.5x)
                let r = sphere.radius * 1.5;
                let mut old_inside = false;
                let mut new_inside = false;
                for &(tid, x, y, z) in &ctx.env.nearby_npcs {
                    if tid != npc_template {
                        continue;
                    }
                    let (dx, dy) = (x - pos.0, y - pos.1);
                    if dx * dx + dy * dy >= r * r {
                        continue;
                    }
                    if distance3((x, y, z), last) <= sphere.radius {
                        old_inside = true;
                    }
                    if distance3((x, y, z), pos) <= sphere.radius {
                        new_inside = true;
                    }
                }
                (old_inside, new_inside)
            };
            let ev = if !old_inside && new_inside {
                Some(super::events::QuestEvent::EnterSphere { sphere })
            } else if old_inside && !new_inside {
                Some(super::events::QuestEvent::ExitSphere { sphere })
            } else {
                None
            };
            if let Some(ev) = ev {
                ctx.dispatch(&ev).map_err(|e| e.0)?;
            }
        }
        Ok(pos)
    });
    match r {
        Some(Ok(pos)) => Ok(Some(pos)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    }
}

impl super::engine::Ctx<'_> {
    /// `UnitRequirementsGameData.CanTriggerSphere(sphere, Owner)`.
    pub fn can_trigger_sphere(&self, sphere_id: u32, or_unit_reqs: bool) -> bool {
        let owner = super::engine::OwnerView {
            c: self.c,
            st: self.st,
            server: self.server,
        };
        let env = super::engine::EnvView {
            server: self.server,
            env: &self.env,
        };
        match &self.env.target {
            Some(t) => self.data.unit_reqs.can_trigger_sphere(
                sphere_id,
                or_unit_reqs,
                &owner,
                &super::engine::TargetView(t, self.server),
                &env,
            ),
            None => self.data.unit_reqs.can_trigger_sphere(
                sphere_id,
                or_unit_reqs,
                &owner,
                &owner,
                &env,
            ),
        }
    }
}
