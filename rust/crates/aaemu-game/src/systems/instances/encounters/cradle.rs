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

/// Metamoze (cinema 52, `id_264_01`) reaches the players of his room through an area skill
/// that needs a playback cue.
pub(super) fn plays_cinema_directly(cinema_id: u32) -> bool {
    cinema_id == 52
}

/// Miasma injuries last beyond the contact that applies them. A unit inside Miasma keeps its
/// buff. The Cradle stair barriers 5423 / 5424 (clouts 1266 / 1267, 200 ms) root with
/// Impaled 4351 for its 2 s like the spears' Injury.
pub(super) fn clout_reapplies_contact(clout_id: u32) -> bool {
    matches!(clout_id, 1139 | 1266 | 1267)
}

/// A Fire Pillar hit lasts beyond the contact that applies it. Fire Pillar (300 ms) hits once
/// per entry, as in the C#: each flame phase of a jet is a new trigger, so a player who stays
/// on the grate is hit once per burst.
pub(super) fn is_contact_buff(buff_id: u32) -> bool {
    buff_id == 3996
}

/// The Cradle pool hazards: Toxicity (green pool), Cursed Blood (red pools),
/// Poison (stepping stone pools) and Lava (Arkanis's room).
pub(super) fn clout_requires_submersion(world: &str, buff_id: u32) -> bool {
    world == WORLD && matches!(buff_id, 3813 | 4362 | 4483 | 4683)
}
