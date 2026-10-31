//! Ray and primitive queries on a locked physics world: `RayWorldIntersection`
//! (IPhysicalWorld vt+336 → cryphysics 0x351AA730) and `PrimitiveWorldIntersection`
//! (vt+512 → 0x350964F0), with the object-type masks and ray flags the zone's game code uses
//! (`ZONE-PHYSICS-USAGE.md` §2.6, `sub_x2game.md` §5).

use aaemu_physics::collision::contact::IntersectionParams;
use aaemu_physics::geometry::primitives::{BoxPrim, CapsulePrim, Primitive, SpherePrim};
use aaemu_physics::math::{Mat33, Vec3};
use aaemu_physics::world::queries::{PwiParams, QueryIntersectionParams, RayHit, RwiParams};
use aaemu_physics::world::EntIdx;

use super::world::{Owner, WorldState};

/// Object-type masks (`ent_*`) of the zone's queries.
pub mod mask {
    /// static + terrain: vehicle slope `sub_39248100`, wheels `sub_39283780`, transfer path
    /// `sub_3973EF70`.
    pub const STATIC_TERRAIN: u32 = 0x101;
    /// static + sleeping + rigid + terrain: unmount check `sub_3972C690`, building
    /// `sub_398351E0`.
    pub const SOLID_TERRAIN: u32 = 0x107;
    /// static + sleeping + rigid: the 1.2 skill obstacle ray without `check_terrain`
    /// (`FUN_390B5540`, `x2game.dll.c:149916`).
    pub const SOLID: u32 = 0x007;
    /// all entities + terrain: tornado `sub_3920FE10`, surface id `sub_394B6660`.
    pub const ALL_TERRAIN: u32 = 0x11F;
    /// all + terrain + water: `WZRayCasting` `sub_3936F340`.
    pub const ALL_TERRAIN_WATER: u32 = 0x31F;
    /// static + terrain + water: glide start `sub_392E36B0`.
    pub const STATIC_TERRAIN_WATER: u32 = 0x301;
    /// static only: the summon hull-envelope overlap. Terrain is left to the depth probes (a
    /// hull legitimately sits above the seabed, and the heightfield has no back faces to test
    /// the envelope against).
    pub const STATIC: u32 = 0x001;
    /// static + sleeping + rigid + living (`WZBlinkUnit` capsule sweep, entTypes 15).
    pub const ENTITIES_15: u32 = 0xF;
    pub const TERRAIN: u32 = 0x100;
    pub const WATER: u32 = 0x200;
}

/// Ray flags.
pub mod ray_flags {
    /// `rwi_stop_at_pierceable | rwi_ignore_back_faces` (0x8F): most x2game rays.
    pub const DEFAULT: u32 = 0x8F;
    /// Skill obstacle ray (`FUN_390B5540`, `x2game.dll.c:149930`): colltype-any with
    /// collision class 2, ignore back faces, pierceability 15.
    pub const SKILL_OBSTACLE: u32 = 0x0002_048F;
    /// `WZRayCasting` (0xFFFE048F: colltype-any with every class but 0).
    pub const WZ_RAYCASTING: u32 = 0xFFFE_048F;
    /// [`DEFAULT`] without `rwi_ignore_back_faces` (0x0F): exit faces of a solid the ray starts
    /// inside are reported, their normal pointing along the ray.
    pub const WITH_BACK_FACES: u32 = 0x00F;
}

/// A ray hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub pos: Vec3,
    pub normal: Vec3,
    /// Distance from the ray origin.
    pub dist: f32,
    /// What was hit (`None` for a water-plane hit).
    pub owner: Option<Owner>,
    /// Hit the water surface.
    pub water: bool,
    /// Mapped surface index (-1 without a material mapping).
    pub surface: i16,
    /// The physical entity.
    pub ent: EntIdx,
}

/// A sweep hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweepHit {
    /// Distance travelled along the sweep before the contact.
    pub dist: f32,
    pub point: Vec3,
    pub normal: Vec3,
}

impl WorldState {
    /// `RayWorldIntersection` from `from` to `to` (caller 4, one hit slot: the closest solid
    /// hit).
    pub fn raycast(
        &mut self,
        from: Vec3,
        to: Vec3,
        objtypes: u32,
        flags: u32,
        skip: &[EntIdx],
    ) -> Option<Hit> {
        let dir = to - from;
        if dir.len2() <= 0.0 {
            return None;
        }
        let p = RwiParams {
            org: from,
            dir,
            objtypes,
            flags,
            skip: skip.to_vec(),
            ..RwiParams::default()
        };
        let mut hits = [RayHit::default()];
        let n = self.world.rwi(&p, &mut hits, None, 4);
        let h = hits[0];
        if n <= 0 || h.dist < 0.0 || h.dist >= 1e9 {
            return None;
        }
        // water-plane hits carry the global area (or no entity)
        let water = h.collider.is_none() || self.world.is_area(h.collider);
        Some(Hit {
            pos: h.pt,
            normal: h.n,
            dist: h.dist,
            owner: if water { None } else { self.owner(h.collider) },
            water,
            surface: h.surface_idx,
            ent: h.collider,
        })
    }

    /// `PrimitiveWorldIntersection` sweep of `prim` along `dir` (the length of `dir` is the
    /// sweep length). `None` = no contact on the way.
    pub fn sweep(
        &mut self,
        prim: Primitive,
        dir: Vec3,
        ent_types: u32,
        geom_flags_any: u32,
        skip: &[EntIdx],
    ) -> Option<SweepHit> {
        let p = PwiParams {
            prim,
            sweep_dir: dir,
            ent_types,
            geom_flags_any,
            skip: skip.to_vec(),
            ..PwiParams::default()
        };
        let r = self.world.pwi(&p);
        let c = r.contacts.first()?;
        if r.dist <= 0.0 && c.t <= 0.0 {
            // contact at the start position
            return Some(SweepHit {
                dist: 0.0,
                point: c.pt,
                normal: c.n,
            });
        }
        Some(SweepHit {
            dist: r.dist,
            point: c.pt,
            normal: c.n,
        })
    }

    /// Static overlap test of `prim` (zero sweep: stops at the first intersecting part).
    pub fn overlaps(
        &mut self,
        prim: Primitive,
        ent_types: u32,
        geom_flags_any: u32,
        skip: &[EntIdx],
    ) -> bool {
        let p = PwiParams {
            prim,
            sweep_dir: Vec3::ZERO,
            ent_types,
            geom_flags_any,
            skip: skip.to_vec(),
            ..PwiParams::default()
        };
        self.world.pwi(&p).dist > 0.0
    }

    /// Static overlap contacts of `prim`: the unique entities that intersect it and their
    /// world AABBs, plus whether the contact query was saturated. All contacts are collected
    /// (no stop at the first part), so the caller can ignore vegetation and tiny statics.
    ///
    /// The per-thread contact pool holds 64 records and 2040 border points; the engine drops
    /// further parts silently once it is full, so `saturated` means the returned entities are
    /// not necessarily all that intersect. Callers must re-query with the seen entities in
    /// `skip` (and refuse the spot if it never becomes complete).
    pub fn overlap_entities(
        &mut self,
        prim: Primitive,
        ent_types: u32,
        geom_flags_any: u32,
        skip: &[EntIdx],
    ) -> (Vec<(EntIdx, [Vec3; 2])>, bool) {
        let p = PwiParams {
            prim,
            sweep_dir: Vec3::ZERO,
            ent_types,
            geom_flags_any,
            skip: skip.to_vec(),
            ip: Some(QueryIntersectionParams {
                ip: IntersectionParams::default(),
                stop_at_first: false,
            }),
            ..PwiParams::default()
        };
        let r = self.world.pwi(&p);
        let mut out: Vec<(EntIdx, [Vec3; 2])> = Vec::new();
        for c in &r.contacts {
            let id = c.i_prim[0];
            if id < 0 {
                continue;
            }
            let Some(e) = self.world.entity_by_id(id) else {
                continue;
            };
            if out.iter().any(|(x, _)| *x == e) {
                continue;
            }
            out.push((e, self.world.ent(e).bbox));
        }
        (out, r.saturated)
    }
}

/// A vertical capsule (`r`, half height of the cylindrical part `hh`) centred at `c`.
pub fn capsule(c: Vec3, r: f32, hh: f32) -> Primitive {
    Primitive::Capsule(CapsulePrim {
        center: c,
        axis: Vec3::new(0.0, 0.0, 1.0),
        r,
        hh,
    })
}

/// A sphere.
pub fn sphere(c: Vec3, r: f32) -> Primitive {
    Primitive::Sphere(SpherePrim { center: c, r })
}

/// A box centred at `c` with half extents `half`, its local X/Y axes turned by `yaw` about +Z
/// (rows of the basis are the box axes, as `CBoxGeom`/`BoxGeom::to_local` read them).
pub fn yaw_box(c: Vec3, half: Vec3, yaw: f32) -> Primitive {
    let (s, co) = yaw.sin_cos();
    Primitive::Box(BoxPrim {
        basis: Mat33::from_array([co, s, 0.0, -s, co, 0.0, 0.0, 0.0, 1.0]),
        oriented: 1,
        center: c,
        size: half,
    })
}
