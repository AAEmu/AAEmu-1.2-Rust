//! `Interaction` configuration (`Configurations/Interaction.json`): the server-side reach check
//! of `CSStartInteraction`.
//!
//! The 1.2 client opens and keeps the NPC interaction window only within 3.0 m of the target's
//! shape (`FUN_393bef10` / `FUN_3932a3b0`, both comparing against `sqrt(9.0)`, helpers
//! `FUN_39093860` / `FUN_39093800`), but it does not gate the `CSStartInteraction` send, so only
//! the server can refuse a forged far interaction. The tolerance below is **INFERRED** (movement
//! and latency slack); no capture shows what the original zone server answered.

use serde::Deserialize;

use crate::config::lenient;

/// Reach the 1.2 client uses for the NPC interaction window (3.0 m; the decompiled constant is
/// squared, 9.0, `DAT_39ac79fc` / `DAT_39ac73b0`).
pub const NPC_INTERACTION_REACH_METERS: f32 = 3.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InteractionConfig {
    /// Refuse `CSStartInteraction` for an NPC farther than the reach; the client gets no reply
    /// (no error message is evidenced). `false` restores the old behaviour (no check).
    #[serde(rename = "npcreachcheck", deserialize_with = "lenient")]
    pub npc_reach_check: bool,
    /// Extra metres on top of [`NPC_INTERACTION_REACH_METERS`] for movement and latency.
    #[serde(rename = "npcreachtolerancemeters", deserialize_with = "lenient")]
    pub npc_reach_tolerance_meters: f32,
    /// Enforce `ai_command_sets.can_interact = 'f'`: while such a set runs the NPC's marker is
    /// off, open windows close and `CSStartInteraction` is refused. INFERRED rule (302 of the
    /// 325 sets are 'f' and the 1.2 client only loads the column), so `false` (default) leaves
    /// every NPC interactable.
    #[serde(rename = "commandsetlock", deserialize_with = "lenient")]
    pub command_set_lock: bool,
}

impl Default for InteractionConfig {
    fn default() -> Self {
        Self {
            npc_reach_check: true,
            npc_reach_tolerance_meters: 2.0,
            command_set_lock: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::AppConfig;

    #[test]
    fn interaction_section_binds_and_defaults() {
        let default = AppConfig::default();
        assert!(default.interaction.npc_reach_check);
        assert_eq!(default.interaction.npc_reach_tolerance_meters, 2.0);
        assert!(!default.interaction.command_set_lock);

        let config = AppConfig::from_value(serde_json::json!({
            "Interaction": {
                "NpcReachCheck": "false",
                "NpcReachToleranceMeters": "0.5"
            }
        }))
        .unwrap();
        assert!(!config.interaction.npc_reach_check);
        assert_eq!(config.interaction.npc_reach_tolerance_meters, 0.5);
    }
}
