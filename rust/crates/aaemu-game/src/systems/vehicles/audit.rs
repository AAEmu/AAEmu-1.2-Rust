//! Read-only ship diagnostics. This is observation, never a second ownership index.
//!
//! The item operation scope must precede the publication gate. These protect summon item /
//! hull publication together, but do NOT protect the spawn manager's direct world removals,
//! every phase timer, or world-instance creation. Consequently v1 always says consistent:false.
//! HP writers can also change the live-root lookup predicate before vehicle death protection.
//! Queue, HP and world removal observations do not constitute an atomic authority snapshot.
//! A complete authority cut needs a spawn-manager reference snapshot under a boundary shared
//! with its queue drain and world removal. Its current public accessor exposes IDs only.
//! Transform, visibility, phase and queue deadlines are explicitly temporal samples. A settled
//! snapshot cannot prove an interval between calls. Release manifests must supply binary/HEAD
//! provenance; this module does not guess the source commit from a developer checkout.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

use serde::Serialize;
use serde_json::{json, Value};

use crate::models::doodad::Doodad;
use crate::models::item::Item;
use crate::models::slave::Slave;
use crate::models::unit::BaseUnitType;
use crate::server::GameServer;
use crate::systems::npcs::spawn_manager;
use crate::world::{DoodadRef, Entity, SlaveRef};

use super::slaves;

pub const PROTOCOL: &str = "aaemu-slave-audit/1";
pub const PREFIX: &str = "AAEMU_SLAVE_AUDIT/1";
const CAPABILITY: &str = "ship-audit/1;prepoll-leases;weak-retirements;sampled-queues";
const MAX_BYTES: usize = 65_536;
const CHUNK_BYTES: usize = 160;

#[derive(Debug, Clone)]
pub struct Request {
    pub character_id: u32,
    pub item_id: u64,
    pub nonce: String,
}

impl Request {
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            args.len() == 3,
            "usage: /slave audit <character-id> <item-id> <nonce>"
        );
        anyhow::ensure!(
            args[..2]
                .iter()
                .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())),
            "IDs must be unsigned decimal integers"
        );
        let character_id = args[0].parse::<u32>()?;
        let item_id = args[1].parse::<u64>()?;
        anyhow::ensure!(character_id != 0 && item_id != 0, "IDs must be nonzero");
        let nonce = &args[2];
        anyhow::ensure!(
            (1..=32).contains(&nonce.len())
                && nonce
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "nonce must be 1..32 ASCII letters, digits, '_' or '-'"
        );
        Ok(Self {
            character_id,
            item_id,
            nonce: nonce.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct Job {
    id: u64,
    kind: &'static str,
    owner: u32,
    item: u64,
    pub(super) phase: &'static str,
    root: Option<u32>,
    generation: Option<u64>,
}

#[derive(Debug)]
struct Retirement {
    world: u32,
    slave: Weak<Entity<Slave>>,
    doodads: Vec<Weak<Entity<Doodad>>>,
}

#[derive(Debug, Default)]
struct State {
    sequence: u64,
    jobs: BTreeMap<u64, Job>,
    retirements: Vec<Retirement>,
}

/// Leases describe actual futures and never decide whether those futures may publish.
#[derive(Debug, Clone, Default)]
pub(super) struct Diagnostics(Arc<Mutex<State>>);

#[derive(Debug)]
pub(super) struct Lease {
    state: Arc<Mutex<State>>,
    id: u64,
}

impl Diagnostics {
    pub(super) fn summon(&self, owner: u32, item: u64) -> Lease {
        self.job("summon", owner, item, "queued", None, None)
    }

    pub(super) fn retirement(&self, slave: &SlaveRef, generation: u64) -> Lease {
        let (owner, item, instance) = {
            let d = slave.data();
            (
                d.summoner_id,
                d.summoning_item_id,
                d.unit.transform.instance_id,
            )
        };
        self.retiring(instance, slave, &[]);
        self.job(
            "retirement",
            owner,
            item,
            "waiting_timer",
            Some(slave.obj_id),
            Some(generation),
        )
    }

    fn job(
        &self,
        kind: &'static str,
        owner: u32,
        item: u64,
        phase: &'static str,
        root: Option<u32>,
        generation: Option<u64>,
    ) -> Lease {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.sequence += 1;
        let id = state.sequence;
        state.jobs.insert(
            id,
            Job {
                id,
                kind,
                owner,
                item,
                phase,
                root,
                generation,
            },
        );
        Lease {
            state: self.0.clone(),
            id,
        }
    }

    /// Weak handles retain neither the hull nor its children beyond existing queue/task owners.
    pub(super) fn retiring(&self, world: u32, slave: &SlaveRef, doodads: &[DoodadRef]) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.retirements.retain(|r| r.slave.strong_count() != 0);
        if let Some(existing) = state
            .retirements
            .iter_mut()
            .find(|r| r.slave.ptr_eq(&Arc::downgrade(slave)))
        {
            for d in doodads {
                let weak = Arc::downgrade(d);
                if !existing.doodads.iter().any(|w| w.ptr_eq(&weak)) {
                    existing.doodads.push(weak);
                }
            }
        } else {
            state.retirements.push(Retirement {
                world,
                slave: Arc::downgrade(slave),
                doodads: doodads.iter().map(Arc::downgrade).collect(),
            });
        }
    }

    pub(super) fn jobs(&self, request: &Request) -> Vec<Job> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .values()
            .filter(|j| j.owner == request.character_id || j.item == request.item_id)
            .cloned()
            .collect()
    }
}

impl Lease {
    pub(super) fn phase(&self, phase: &'static str) {
        if let Some(job) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .get_mut(&self.id)
        {
            job.phase = phase;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Also runs for an unpolled future, cancellation, and unwind. Recovering poison here
        // avoids a second panic during unwind; this lock never guards gameplay authority.
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .jobs
            .remove(&self.id);
    }
}

fn item_snapshot(server: &GameServer, id: u64) -> Value {
    let Some(item) = server.items.item_by_id(id) else {
        return Value::Null;
    };
    let containers: Vec<_> = server
        .items
        .all_containers()
        .into_iter()
        .filter_map(|c| {
            let c = c.lock().expect("container poisoned");
            let matches: Vec<_> = c
                .items
                .iter()
                .filter(|i| i.id == id)
                .map(item_fields)
                .collect();
            (!matches.is_empty()).then(|| {
                json!({"id": c.container_id, "kind": c.kind.type_name(),
            "owner_id": c.owner_id, "slot_type": c.slot_type, "mate_id": c.mate_id,
            "size": c.container_size(), "items": matches})
            })
        })
        .collect();
    json!({"lookup": item_fields(&item), "containers": containers})
}

fn item_fields(i: &Item) -> Value {
    json!({"id": i.id, "template_id": i.template_id, "class": i.class.type_name(),
        "owner_id": i.owner_id, "slot_type": i.slot_type, "slot": i.slot, "count": i.count,
        "grade": i.grade, "flags": i.flags, "dirty": i.dirty,
        "saved_slave_db_id": i.details.slave_db_id,
        "detail_type_and_body_hex": hex(&i.detail_type_and_details())})
}

/// Cached attached object IDs and Doodad.parent_obj_id can outlive a parent relation. The
/// registered hull allocation is checked separately before using this live transform relation.
pub(super) fn rack_parent_matches(rack: &Doodad, instance: u32, hull: u32) -> bool {
    rack.transform.instance_id == instance && rack.transform.parent_obj_id == hull
}

/// No grants, find-or-create container calls, saves, SQL, or repairs. All guards are synchronous.
pub fn snapshot(server: &GameServer, request: &Request) -> Value {
    let _items = server.systems.economy.operation_scope();
    let summons = &server.systems.vehicles.slave_summons;
    let _publication = summons.publication_scope();
    let (sample, retiring) = {
        let mut state = summons
            .diagnostics
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.sequence += 1;
        (
            state.sequence,
            state
                .retirements
                .iter()
                .filter_map(|r| {
                    Some((
                        r.world,
                        r.slave.upgrade()?,
                        r.doodads
                            .iter()
                            .filter_map(Weak::upgrade)
                            .collect::<Vec<_>>(),
                    ))
                })
                .collect::<Vec<_>>(),
        )
    };
    let jobs = summons.diagnostics.jobs(request);
    let retirement_references: Vec<_> = retiring.iter().map(|(_, s, _)| s.clone()).collect();
    let mut worlds = server.world.worlds();
    worlds.sort_by_key(|w| w.id);
    let mut references = Vec::new();
    for world in &worlds {
        for slave in slaves::all_slaves(world) {
            references.push((world.id, slave, Vec::new()));
        }
    }
    for (world, slave, doodads) in retiring {
        if let Some((_, _, existing)) = references
            .iter_mut()
            .find(|(_, s, _)| Arc::ptr_eq(s, &slave))
        {
            existing.extend(doodads);
        } else {
            references.push((world, slave, doodads));
        }
    }
    references.sort_by_key(|(w, s, _)| (*w, s.obj_id));
    let owner_roots = slaves::owned_roots(server, request.character_id);
    let owner_object_lookups: Vec<_> = server.world.all_characters().into_iter()
        .filter(|p| p.character_id == request.character_id)
        .map(|p| json!({"character_id": p.character_id, "object_id": p.obj_id, "connection_id": p.conn.id,
            "worlds": worlds.iter().map(|w| json!({"world_instance": w.id,
                "get_active_slave_by_owner_obj_id": slaves::get_active_slave_by_owner_obj_id(w, p.obj_id).map(|s| s.obj_id)})).collect::<Vec<_>>()})).collect();
    let mut candidates = Vec::new();
    for (instance, slave, mut doodads) in references {
        // Copy one entity, then release its data guard before any world or item lookup.
        let d = slave.data().clone();
        if d.summoner_id != request.character_id && d.summoning_item_id != request.item_id {
            continue;
        }
        let world = worlds.iter().find(|w| w.id == instance);
        let registered = world.is_some_and(|w| {
            w.slave(slave.obj_id)
                .is_some_and(|s| Arc::ptr_eq(&s, &slave))
        });
        let retained_doodads = doodads.clone();
        // An old departure future can retain a hull after its object ID was recycled. Only
        // the registered allocation may discover racks through numeric world parent fields.
        // For an unregistered hull keep only weak references captured from its own lifetime.
        if let Some(world) = world.filter(|_| registered) {
            for rack in world.all_doodads() {
                let matches = {
                    let r = rack.data();
                    rack_parent_matches(&r, instance, slave.obj_id)
                };
                if matches && !doodads.iter().any(|r| Arc::ptr_eq(r, &rack)) {
                    doodads.push(rack);
                }
            }
        }
        doodads.sort_by_key(|r| r.obj_id);
        let racks: Vec<_> = doodads.iter().map(|rack| {
            let r = rack.data().clone();
            let rack_registered = world.is_some_and(|w| w.doodad(rack.obj_id).is_some_and(|live| Arc::ptr_eq(&live, rack)));
            let current = registered && rack_registered && rack_parent_matches(&r, instance, slave.obj_id);
            json!({"object_id": rack.obj_id, "db_id": r.db_id, "template_id": r.template_id,
                "parent": r.parent_obj_id, "transform_parent": r.transform.parent_obj_id,
                "attach": r.attach_point, "owner_id": r.owner_id, "owner_db_id": r.owner_db_id,
                "owner_type": r.owner_type, "registered": rack_registered, "persistent": r.is_persistent,
                "captured_retirement_reference": retained_doodads.iter().any(|old| Arc::ptr_eq(old, rack)),
                "current_hull_association": current,
                "association": if current { "registered_hull_parent" } else { "retained_hull_lifetime" },
                "phase": r.func_group_id, "phase_time_ms": r.phase_time_ms,
                "interaction_counts": r.phase.interaction_counts, "deleted": r.phase.deleted,
                "func_task": r.phase.func_task, "item_id": r.item_id, "item_template_id": r.item_template_id,
                "item": if r.item_id == 0 { Value::Null } else { item_snapshot(server, r.item_id) }})
        }).collect();
        // Historical references remain visible above but never count as current racks/cargo.
        let rack_attach: Vec<_> = racks
            .iter()
            .filter(|r| r["current_hull_association"] == true)
            .filter_map(|r| r["attach"].as_u64().map(|a| a as u8))
            .collect();
        let missing: Vec<_> = (60..=79u8).filter(|p| !rack_attach.contains(p)).collect();
        let duplicate: Vec<_> = (60..=79u8)
            .filter(|p| rack_attach.iter().filter(|a| *a == p).count() > 1)
            .collect();
        candidates.push(json!({"world_instance": instance, "object_id": slave.obj_id,
            "transport_id": d.unit.tl_id, "db_id": d.id, "template_id": d.unit.template_id,
            "model_id": d.unit.model_id, "parent": d.unit.transform.parent_obj_id,
            "attach": d.attach_point_id, "summoner_id": d.summoner_id, "summoner_object_id": d.summoner_obj_id,
            "summoning_item_id": d.summoning_item_id, "owner_type": d.owner_type as u8,
            "owner_id": d.unit.owner_id, "owner_object_id": d.owner_obj_id,
            "visible": slave.is_visible(), "dead": d.unit.is_dead(), "leave_generation": d.leave_task,
            "registered": registered, "owner_root_lookup_member": owner_roots.iter().any(|(_, s)| Arc::ptr_eq(s, &slave)),
            "observed_retirement_reference": retirement_references.iter().any(|s| Arc::ptr_eq(s, &slave)),
            "item_live_root_lookup_member": registered && d.owner_type == BaseUnitType::Character && !d.unit.is_dead() && d.summoning_item_id == request.item_id,
            "local_transform_sample": [d.unit.transform.x, d.unit.transform.y, d.unit.transform.z, d.unit.transform.rot_x, d.unit.transform.rot_y, d.unit.transform.rot_z],
            "attached_slaves": d.attached_slaves, "attached_doodads": d.attached_doodads,
            "rack_summary_scope": "current_registered_hull_parent",
            "missing_rack_attach": missing, "duplicate_rack_attach": duplicate, "doodads": racks}));
    }
    let queues: Vec<_> = worlds.iter().map(|world| {
        let manager = server.systems.npcs.spawns.get(world.id);
        let rows: Vec<_> = manager.as_ref().map(|m| m.pending_despawns()).unwrap_or_default().into_iter()
            .filter(|(id, _)| candidates.iter().any(|c| c["world_instance"] == world.id && (c["object_id"] == *id || c["attached_slaves"].as_array().is_some_and(|a| a.iter().any(|v| v.as_u64() == Some(u64::from(*id)))) || c["attached_doodads"].as_array().is_some_and(|a| a.iter().any(|v| v.as_u64() == Some(u64::from(*id)))))))
            .map(|(id, at)| json!({"object_id": id, "due_in_ms_sample": at.map(|t| t.saturating_duration_since(spawn_manager::now()).as_millis())})).collect();
        json!({"world_instance": world.id, "matching_despawns_sample": rows})
    }).collect();
    json!({"protocol": PROTOCOL, "nonce": request.nonce, "character_id": request.character_id,
        "item_id": request.item_id, "source_capability": CAPABILITY, "package_version": env!("CARGO_PKG_VERSION"),
        "source_head": Value::Null, "provenance": "exact binary/source identity requires release manifest",
        "sample_sequence": sample, "consistent": false, "authority_consistent": false,
        "pending_supported": true, "retirement_consistent": false,
        "unsupported": ["spawn queue retained references/direct world removals have no shared audit boundary", "HP writers affect live-root membership outside publication protection; world enumeration/removal has no shared snapshot boundary", "phase/transform/visibility and queue deadlines are temporal samples; no interval proof"],
        "publication_scope_held": true, "item_operation_scope_held": true,
        "world_instances": worlds.iter().map(|w| w.id).collect::<Vec<_>>(),
        "summon_item": item_snapshot(server, request.item_id),
        "jobs": jobs, "pending_summons": jobs.iter().filter(|j| j.kind == "summon").count(),
        "pending_retirement_tasks": jobs.iter().filter(|j| j.kind == "retirement").count(),
        "identity_state": summons.audit_identity(request.character_id, request.item_id),
        "owner_object_lookups": owner_object_lookups,
        "item_has_other_live_root": slaves::item_has_other_live_root(server, request.item_id, request.character_id),
        "item_root_lookup": candidates.iter().filter(|c| c["item_live_root_lookup_member"] == true).map(|c| json!({"world_instance": c["world_instance"], "object_id": c["object_id"]})).collect::<Vec<_>>(),
        "owner_root_lookup": owner_roots.iter().map(|(w, s)| json!({"world_instance": w.id, "object_id": s.obj_id})).collect::<Vec<_>>(),
        "candidates": candidates, "queues": queues})
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| {
            [
                DIGITS[(b >> 4) as usize] as char,
                DIGITS[(b & 15) as usize] as char,
            ]
        })
        .collect()
}

/// FNV-1a/64 is a completeness checksum, not an authenticity or cryptographic claim.
fn digest(bytes: &[u8]) -> String {
    format!(
        "{:016x}",
        bytes
            .iter()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b))
                .wrapping_mul(0x100000001b3))
    )
}

/// ASCII hex permits splitting UTF-8 JSON at byte boundaries. Every line is < 450 bytes;
/// max 410 data records + END. Consumers require all unique indices, exact length, digest,
/// matching nonce/sequence and END. END means transport completeness, never an audit pass.
pub fn chunks(request: &Request, snapshot: &Value) -> anyhow::Result<Vec<String>> {
    let mut bytes = serde_json::to_vec(snapshot)?;
    if bytes.len() > MAX_BYTES {
        bytes = serde_json::to_vec(&json!({"protocol": PROTOCOL, "nonce": request.nonce,
            "character_id": request.character_id, "item_id": request.item_id,
            "sample_sequence": snapshot["sample_sequence"], "consistent": false, "authority_consistent": false,
            "pending_supported": false, "unsupported": ["snapshot exceeds 65536 byte transport limit"], "source_capability": CAPABILITY}))?;
    }
    let sequence = &snapshot["sample_sequence"];
    let sum = digest(&bytes);
    let count = bytes.len().div_ceil(CHUNK_BYTES);
    let mut lines: Vec<_> = bytes
        .chunks(CHUNK_BYTES)
        .enumerate()
        .map(|(i, b)| {
            format!(
                "{PREFIX} {} {sequence} DATA {}/{count} {} fnv1a64:{sum} hex:{}",
                request.nonce,
                i + 1,
                bytes.len(),
                hex(b)
            )
        })
        .collect();
    lines.push(format!(
        "{PREFIX} {} {sequence} END {count} {} fnv1a64:{sum}",
        request.nonce,
        bytes.len()
    ));
    Ok(lines)
}
