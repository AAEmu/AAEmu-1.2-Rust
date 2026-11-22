//! Kroloal Cradle: Kuwon's squads, Princess Surya, the pool hazards and the purifier.

// ---- keys

/// Kroloal Cradle's world template.
pub const WORLD: &str = "instance_cradle_of_destruction";
/// Kroloal Cradle's zone.
pub const ZONE: u32 = 264;

// ---- tables

/// Command sets whose `FollowPath` routes are walked at the actor's own walking pace: the
/// Kroloal Cradle scenes of Kuwon's squads (human model 18) and Princess Surya (model 19) in
/// Sprigg's, Siranus's and Metamoze's rooms. In Siranus's room the authored pauses (cast
/// times aside) bring the commander and both soldiers to their vanish (19652) together at
/// 1.8-1.9 m/s, the human walk. Siranus's shards (231-233, model 315) keep the default.
pub(super) fn walks_scene_routes(cmd_set_id: u32) -> bool {
    matches!(cmd_set_id, 222..=224 | 228..=230 | 234..=237)
}
