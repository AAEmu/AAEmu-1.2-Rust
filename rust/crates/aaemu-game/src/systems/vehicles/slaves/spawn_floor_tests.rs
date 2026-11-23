use super::summon_floor;

#[test]
fn a_ground_probe_miss_uses_the_terrain_fallback() {
    assert_eq!(summon_floor(Some(Some(7.0)), || 3.0), 7.0);
    // Some(None): no static/terrain collider on the ray, fall back to the terrain height
    // (the heightmap) instead of treating the spot as bottomless water.
    assert_eq!(summon_floor(Some(None), || 3.0), 3.0);
    // None: the collision service is off, use the fallback as well.
    assert_eq!(summon_floor(None, || 3.0), 3.0);
}
