//! Route smoothing over the navmesh triangles: the portal edges of a triangle corridor and the
//! 2D "funnel" (string-pulling) algorithm, plus a direct-line corridor walk.
//!
//! The A* corridor ([`super::astar::PathNode::find_route`]) is a chain of linked triangles.
//! Walking centroid to centroid detours through huge triangles, so the route handed to the AI
//! is the shortest 2D path *inside* the corridor: the funnel pulls the string tight around the
//! shared edge corners (the native engine's direct-path attempt, `RequestPathToPipeUser`
//! 0x31062728, likewise steers by actor positions, not graph centroids).
//!
//! This is an engine-style addition, not part of the C# (`PORTING.md`, "geodata deviations").

use crate::systems::skills::controllers::Vec3;

use super::graph::NavGraph;

type P2 = (f64, f64);

fn p2(v: Vec3) -> P2 {
    (f64::from(v.x), f64::from(v.y))
}

/// Z component of (a - o) x (b - o): positive when `b` is counter-clockwise (left) of `o -> a`.
fn cross(o: P2, a: P2, b: P2) -> f64 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

/// An edge the route crosses, seen in the direction of travel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Portal {
    pub left: Vec3,
    pub right: Vec3,
}

impl Portal {
    /// The portal with both corners moved `radius` towards each other (to the midpoint when the
    /// edge is shorter than the agent): the string keeps that distance from the edge's corners.
    pub fn shrunk(self, radius: f32) -> Self {
        let (l, r) = (p2(self.left), p2(self.right));
        let len = ((l.0 - r.0).powi(2) + (l.1 - r.1).powi(2)).sqrt();
        let lerp = |a: Vec3, b: Vec3, t: f64| {
            Vec3::new(
                (f64::from(a.x) + (f64::from(b.x) - f64::from(a.x)) * t) as f32,
                (f64::from(a.y) + (f64::from(b.y) - f64::from(a.y)) * t) as f32,
                (f64::from(a.z) + (f64::from(b.z) - f64::from(a.z)) * t) as f32,
            )
        };
        let r = f64::from(radius.max(0.0));
        if len <= 2.0 * r + 1e-6 {
            let m = lerp(self.left, self.right, 0.5);
            return Self { left: m, right: m };
        }
        let t = r / len;
        Self {
            left: lerp(self.left, self.right, t),
            right: lerp(self.right, self.left, t),
        }
    }
}

/// The portals along a corridor of linked triangle nodes (`None` when an edge cannot be
/// determined, e.g. a non-triangle node).
pub fn corridor_portals(graph: &NavGraph, corridor: &[u32]) -> Option<Vec<Portal>> {
    corridor
        .windows(2)
        .map(|w| {
            let (v1, v2) = graph.portal(w[0], w[1])?;
            let tri = graph.nodes[w[0] as usize].triangle?;
            let apex = tri.iter().copied().find(|v| *v != v1 && *v != v2)?;
            let (a1, a2, o) = (
                graph.vertices[v1 as usize],
                graph.vertices[v2 as usize],
                p2(graph.vertices[apex as usize]),
            );
            let mid = ((p2(a1).0 + p2(a2).0) / 2.0, (p2(a1).1 + p2(a2).1) / 2.0);
            // left of the travel direction: from the triangle's far corner across the edge
            Some(if cross(o, mid, p2(a1)) >= 0.0 {
                Portal {
                    left: a1,
                    right: a2,
                }
            } else {
                Portal {
                    left: a2,
                    right: a1,
                }
            })
        })
        .collect()
}

/// Twice the signed area of (a, b, c), negative when `c` is counter-clockwise of `a -> b`
/// (the "simple stupid funnel" convention).
fn tri2(a: P2, b: P2, c: P2) -> f64 {
    (c.0 - a.0) * (b.1 - a.1) - (b.0 - a.0) * (c.1 - a.1)
}

fn same(a: P2, b: P2) -> bool {
    (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) < 1e-10
}

/// The shortest 2D path from `start` to `goal` through the portals ("simple stupid funnel
/// algorithm"): `start`, the corners the string bends around, `goal`. Corner z values are the
/// vertices' (unreliable on large triangles); `None` only when the funnel does not terminate.
pub fn string_pull(start: Vec3, goal: Vec3, portals: &[Portal]) -> Option<Vec<Vec3>> {
    let mut all = Vec::with_capacity(portals.len() + 2);
    all.push(Portal {
        left: start,
        right: start,
    });
    all.extend_from_slice(portals);
    all.push(Portal {
        left: goal,
        right: goal,
    });
    let mut path = vec![start];
    let (mut apex, mut left, mut right) = (start, start, start);
    let (mut li, mut ri) = (0usize, 0usize);
    let mut i = 1usize;
    let mut budget = all.len() * all.len() + 16;
    while i < all.len() {
        budget = budget.checked_sub(1)?;
        let (l, r) = (all[i].left, all[i].right);
        // tighten the right side
        if tri2(p2(apex), p2(right), p2(r)) <= 0.0 {
            if same(p2(apex), p2(right)) || tri2(p2(apex), p2(left), p2(r)) > 0.0 {
                right = r;
                ri = i;
            } else {
                // the right side crossed over the left: the left corner is a path corner
                path.push(left);
                apex = left;
                right = apex;
                ri = li;
                i = li + 1;
                continue;
            }
        }
        // tighten the left side
        if tri2(p2(apex), p2(left), p2(l)) >= 0.0 {
            if same(p2(apex), p2(left)) || tri2(p2(apex), p2(right), p2(l)) < 0.0 {
                left = l;
                li = i;
            } else {
                path.push(right);
                apex = right;
                left = apex;
                li = ri;
                i = ri + 1;
                continue;
            }
        }
        i += 1;
    }
    if path.last().is_none_or(|p| !same(p2(*p), p2(goal))) {
        path.push(goal);
    }
    Some(path)
}

/// The corridor of triangles the straight segment `a -> b` crosses, walking from `start`
/// through passable links (each crossing must hit the shared edge); `None` when the segment
/// leaves the linked mesh (no link, blocked link, a non-triangle node) before reaching the
/// triangle containing `b`.
pub fn straight_corridor(
    graph: &NavGraph,
    a: Vec3,
    b: Vec3,
    start: u32,
    agent_radius: f32,
) -> Option<Vec<u32>> {
    const EPS: f64 = 1e-6;
    let (pa, pb) = (p2(a), p2(b));
    let r = (pb.0 - pa.0, pb.1 - pa.1);
    let mut corridor = vec![start];
    let mut cur = start;
    let mut t_cur = 0.0f64;
    for _ in 0..256 {
        graph.nodes.get(cur as usize)?.triangle?;
        if graph
            .triangle_height(cur, f64::from(b.x), f64::from(b.y))
            .is_some()
        {
            return Some(corridor);
        }
        let mut best: Option<(f64, u32)> = None;
        for l in &graph.nodes[cur as usize].links {
            if !l.passable(agent_radius) || corridor.contains(&l.target) {
                continue;
            }
            let Some((v1, v2)) = graph.portal(cur, l.target) else {
                continue;
            };
            let (c, d) = (
                p2(graph.vertices[v1 as usize]),
                p2(graph.vertices[v2 as usize]),
            );
            let s = (d.0 - c.0, d.1 - c.1);
            let denom = r.0 * s.1 - r.1 * s.0;
            if denom.abs() < 1e-12 {
                continue;
            }
            let ca = (c.0 - pa.0, c.1 - pa.1);
            let t = (ca.0 * s.1 - ca.1 * s.0) / denom;
            let u = (ca.0 * r.1 - ca.1 * r.0) / denom;
            if !(-EPS..=1.0 + EPS).contains(&u) || t < t_cur - EPS || t > 1.0 + EPS {
                continue;
            }
            if best.is_none_or(|(bt, _)| t < bt) {
                best = Some((t, l.target));
            }
        }
        let (t, next) = best?;
        t_cur = t_cur.max(t);
        corridor.push(next);
        cur = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f32, y: f32) -> Vec3 {
        Vec3::new(x, y, 0.0)
    }

    fn xy(p: &[Vec3]) -> Vec<(f32, f32)> {
        p.iter().map(|p| (p.x, p.y)).collect()
    }

    #[test]
    fn collinear_portals_give_the_straight_line() {
        // a 10 m wide corridor along +x
        let portals: Vec<Portal> = (1..4)
            .map(|k| Portal {
                left: v(10.0 * k as f32, 5.0),
                right: v(10.0 * k as f32, -5.0),
            })
            .collect();
        let p = string_pull(v(0.0, 0.0), v(40.0, 0.0), &portals).unwrap();
        assert_eq!(xy(&p), [(0.0, 0.0), (40.0, 0.0)]);
    }

    #[test]
    fn single_portal_and_empty_corridor() {
        let portal = Portal {
            left: v(5.0, 5.0),
            right: v(5.0, -5.0),
        };
        let p = string_pull(v(0.0, 0.0), v(10.0, 3.0), &[portal]).unwrap();
        assert_eq!(xy(&p), [(0.0, 0.0), (10.0, 3.0)]);
        let p = string_pull(v(0.0, 0.0), v(10.0, 3.0), &[]).unwrap();
        assert_eq!(xy(&p), [(0.0, 0.0), (10.0, 3.0)]);
    }

    #[test]
    fn corridor_corner_is_pulled_tight_around_the_corner_vertex() {
        // an L-shaped corridor: east along y in [0, 10], then north through x in [20, 30]
        let portals = [
            Portal {
                left: v(10.0, 10.0),
                right: v(10.0, 0.0),
            },
            Portal {
                // travelling east then turning north: the corner is the inner (left) vertex
                left: v(20.0, 10.0),
                right: v(30.0, 10.0),
            },
        ];
        // the string bends around the left vertex (20, 10)
        let p = string_pull(v(0.0, 0.0), v(25.0, 40.0), &portals).unwrap();
        assert_eq!(p.len(), 3, "{p:?}");
        assert_eq!((p[1].x, p[1].y), (20.0, 10.0));
        assert_eq!((p[2].x, p[2].y), (25.0, 40.0));
    }

    #[test]
    fn shrunk_portals_keep_the_radius_from_the_corners() {
        let s = Portal {
            left: v(0.0, 10.0),
            right: v(0.0, 0.0),
        }
        .shrunk(0.25);
        assert_eq!((s.left.y, s.right.y), (9.75, 0.25));
        // narrower than the agent: both ends at the midpoint
        let s = Portal {
            left: v(0.0, 0.4),
            right: v(0.0, 0.0),
        }
        .shrunk(0.25);
        assert_eq!((s.left.y, s.right.y), (0.2, 0.2));
    }
}
