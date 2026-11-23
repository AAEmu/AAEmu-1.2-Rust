use super::super::{ld, lm};
use crate::collision::contact::{GeomContact, IntersectionParams};
use crate::geometry::{GeomWorldData, Geometry};
use crate::ids::GeomType;
use crate::math::{Mat33, Quat, Vec3};
use crate::world::{EntIdx, World};

/// `geometry->Intersect(other, gwd1, gwd2, params, &contacts)` with the primitive driver's
/// write-back into `gwd1` (the `-0.0 → +0.0` / sweep round trip of 0x35151F20).
pub fn intersect_wb(
    a: &dyn Geometry,
    b: &dyn Geometry,
    g1: &mut GeomWorldData,
    g2: &GeomWorldData,
    ip: &mut IntersectionParams,
    out: &mut Vec<GeomContact>,
) -> usize {
    if a.is_a_primitive() && a.geom_type() != GeomType::Ray {
        let env = crate::collision::prim_intersect::env_with_generic();
        crate::collision::prim_intersect::with_thread_scratch(|s| {
            let o = crate::collision::prim_intersect::intersect(&env, s, a, b, g1, g2, ip);
            out.clear();
            if let (Some(f), true) = (o.first, o.ret > 0) {
                out.extend(s.contacts(f, o.ret));
            }
            o.ret.max(0) as usize
        })
    } else {
        // the mesh glue appends to `out`: start from an empty list like the primitive path
        out.clear();
        a.intersect(b, g1, g2, ip, out)
    }
}

/// `sub_35022100`: reset intersection params (`+98`/`+100` are not modelled; the primitive
/// driver does not read them).
pub(super) fn reset_params(ip: &mut IntersectionParams) {
    *ip = IntersectionParams::default();
}

/// `R·v + p` with the binary's association (`((R0·v.x + R1·v.y) + R2·v.z) + p`).
pub(super) fn place(r: &[[f32; 3]; 3], v: Vec3, p: Vec3) -> Vec3 {
    Vec3::new(
        ((r[0][0] * v.x + v.y * r[0][1]) + v.z * r[0][2]) + p.x,
        ((v.x * r[1][0] + v.y * r[1][1]) + v.z * r[1][2]) + p.y,
        p.z + ((v.x * r[2][0] + v.y * r[2][1]) + v.z * r[2][2]),
    )
}

/// The head-sphere ceiling check of Step: the head sphere swept upward over
/// `heightHead − heightCollider − min(eyeOffset, 0)` against the parts (flag 2) of the static
/// neighbours; the eye offset is limited to stay under the nearest hit.
pub(super) fn head_check(
    w: &mut World,
    e: EntIdx,
    kept: &[EntIdx],
    r: &[[f32; 3]; 3],
    p: Vec3,
    up: Vec3,
    _tid: usize,
) {
    let mut ip = IntersectionParams {
        sweep_test: true,
        ..IntersectionParams::default()
    };
    let pp0 = w.ent(e).parts[0].pos;
    let mut g_own = GeomWorldData {
        offset: place(r, pp0, p),
        r: Mat33 { m: *r },
        scale: 1.0,
        v: up,
        ..GeomWorldData::default()
    };
    let l = ld(w, e);
    let eo = l.eye_offset;
    let v721 = (l.height_head - l.height_collider) - (eo - eo.abs()) * 0.5;
    ip.time_interval = v721;
    let mut v722 = v721;
    let head = l.head.clone();
    let mut contacts = Vec::new();
    for &o in kept {
        if w.ent(o).id_bits & 0xFF00_0000 != 0 {
            continue;
        }
        let np = w.ent(o).parts.len();
        for j in 0..np {
            let (g1, geom) = {
                let oe = w.ent(o);
                let pt = &oe.parts[j];
                if pt.flags & 2 == 0 {
                    continue;
                }
                let qp = oe.q.product(pt.q);
                let m = Mat33::from_quat(qp);
                let rr = &m.m;
                let pp = pt.pos;
                let off = Vec3::new(
                    ((rr[0][0] * pp.x + rr[0][1] * pp.y) + rr[0][2] * pp.z) + oe.pos.x,
                    ((rr[1][0] * pp.x + rr[1][1] * pp.y) + rr[1][2] * pp.z) + oe.pos.y,
                    ((rr[2][0] * pp.x + rr[2][1] * pp.y) + rr[2][2] * pp.z) + oe.pos.z,
                );
                (
                    GeomWorldData {
                        offset: off,
                        r: m,
                        scale: pt.scale,
                        ..GeomWorldData::default()
                    },
                    pt.pgeom_proxy.geom.clone(),
                )
            };
            if intersect_wb(
                head.as_ref(),
                geom.as_ref(),
                &mut g_own,
                &g1,
                &mut ip,
                &mut contacts,
            ) != 0
            {
                let t = contacts[0].t as f32;
                v722 = ((t + v722) - (v722 - t).abs()) * 0.5;
            }
        }
    }
    let l = lm(w, e);
    let v744 = l.eye_offset;
    let lim = ((v744 - v744.abs()) * 0.5 + v721) - v722;
    if v744 < lim || (l.eye_acc.abs() + l.eye_vel.abs()) == 0.0 {
        l.eye_offset = lim;
    }
}

/// The flagged-static pre-check of the neighbour gathering (entity flags `0x60000000`): parts
/// with a bbox diagonal over 1 are tested at the start and swept over the move; an entity is
/// dropped when the start overlaps or the sweep hits a part whose centre the move approaches
/// (flag `0x20000000`: recedes from).
pub(super) fn static_moving_away(
    w: &mut World,
    e: EntIdx,
    o: EntIdx,
    p: Vec3,
    mv: Vec3,
    mlen: f32,
    dt: f32,
    pos0: Vec3,
    q: Quat,
) -> bool {
    let collider = ld(w, e).collider.geom.clone();
    let r = if q.y * q.y + q.x * q.x >= 0.000_001_000_000_1 {
        Mat33::from_quat(q)
    } else {
        Mat33::IDENTITY
    };
    let pp0 = w.ent(e).parts[0].pos;
    let np = w.ent(o).parts.len();
    let mut contacts = Vec::new();
    for j in 0..np {
        let (pb, g1, geom) = {
            let oe = w.ent(o);
            let pt = &oe.parts[j];
            let qp = oe.q.product(pt.q);
            let rr = oe.q.rotate(pt.pos);
            (
                pt.bbox,
                GeomWorldData {
                    offset: Vec3::new(oe.pos.x + rr.x, oe.pos.y + rr.y, oe.pos.z + rr.z),
                    r: Mat33::from_quat(qp),
                    scale: pt.scale,
                    ..GeomWorldData::default()
                },
                pt.pgeom_proxy.geom.clone(),
            )
        };
        let d = Vec3::new(pb[1].x - pb[0].x, pb[1].y - pb[0].y, pb[1].z - pb[0].z);
        if !(((d.x * d.x) + d.y * d.y) + d.z * d.z > 1.0) {
            continue;
        }
        let mut ip = IntersectionParams {
            time_interval: dt * 200.0,
            ..IntersectionParams::default()
        };
        let mut g = GeomWorldData {
            offset: place(&r.m, pp0, p),
            r,
            scale: 1.0,
            ..GeomWorldData::default()
        };
        if intersect_wb(
            collider.as_ref(),
            geom.as_ref(),
            &mut g,
            &g1,
            &mut ip,
            &mut contacts,
        ) != 0
        {
            return true;
        }
        let inv = 1.0 / mlen;
        g.v = Vec3::new(mv.x * inv, inv * mv.y, inv * mv.z);
        g.offset = place(&r.m, pp0, Vec3::new(p.x + mv.x, p.y + mv.y, p.z + mv.z));
        let n = intersect_wb(
            collider.as_ref(),
            geom.as_ref(),
            &mut g,
            &g1,
            &mut ip,
            &mut contacts,
        );
        if n != 0 {
            let c = &contacts[n - 1];
            let cx = (pb[1].x - pb[0].x) * 0.5 + pb[0].x;
            let cy = (pb[1].y - pb[0].y) * 0.5 + pb[0].y;
            let v164 = (c.pt.y - cy) * (c.pt.y - cy) + (c.pt.x - cx) * (c.pt.x - cx);
            let v165 = (pos0.y - cy) * (pos0.y - cy) + (pos0.x - cx) * (pos0.x - cx);
            if w.ent(o).flags & 0x2000_0000 != 0 {
                if v164 <= v165 {
                    return true;
                }
            } else if v164 >= v165 {
                return true;
            }
        }
    }
    false
}
