//! `/rebuild`: the vanilla-client fallback of the housing rebuild (the 1.2 client has no rebuild
//! window). `/rebuild` lists the rebuild options of the nearest own house, `/rebuild <id>` starts
//! one. Players with the client extension use the `house.rebuild.*` channels instead.

use super::prelude::*;
use crate::systems::housing::rebuild::ops;

/// `Rebuild` (AAEmu-only, no C# counterpart).
pub struct RebuildHouse;

impl RebuildHouse {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for RebuildHouse {
    fn command_names(&self) -> Vec<String> {
        names(&["rebuild"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        let server = ctx.server()?;
        let player = ctx.player()?;
        let Some(house) = ops::resolve_house(server, &player) else {
            CommandManager::send_error_text(
                self,
                out,
                "You are not near a house of yours (within 30 m).",
            );
            return Ok(());
        };
        let view = match ops::view(server, &player, &house) {
            Ok(view) => view,
            Err(e) => {
                CommandManager::send_error_text(self, out, e.message());
                return Ok(());
            }
        };
        match args {
            [] => {
                if view.options.is_empty() {
                    CommandManager::send_normal_text(
                        self,
                        out,
                        &format!("{} cannot be rebuilt.", view.name),
                    );
                    return Ok(());
                }
                CommandManager::send_normal_text(
                    self,
                    out,
                    &format!("Rebuild options for {} (use /rebuild <id>):", view.name),
                );
                if let Some(blocked) = view.blocked {
                    CommandManager::send_error_text(self, out, blocked.message());
                }
                for o in &view.options {
                    let materials = o
                        .materials
                        .iter()
                        .map(|m| {
                            let name = server
                                .game_data
                                .items
                                .template(m.item)
                                .map(|t| t.name.clone())
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| format!("item {}", m.item));
                            format!("{name} {}/{}", m.have, m.count)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    CommandManager::send_normal_text(
                        self,
                        out,
                        &format!(
                            "|cFFFFFFFF{}|r: {} (labor {}, tax {}) {}{}",
                            o.rebuilding,
                            o.name,
                            o.labor,
                            o.tax,
                            if materials.is_empty() {
                                "no materials".to_string()
                            } else {
                                materials
                            },
                            o.reason
                                .map(|r| format!(" - {}", r.message()))
                                .unwrap_or_default()
                        ),
                    );
                }
            }
            [id] => {
                let Some(rebuilding) = try_parse::<u32>(id) else {
                    CommandManager::send_default_help_text(self, out);
                    return Ok(());
                };
                match ops::rebuild(server, &player, &house, rebuilding) {
                    Ok(target) => CommandManager::send_normal_text(
                        self,
                        out,
                        &format!("{} is being rebuilt (building {target}).", view.name),
                    ),
                    Err(e) => CommandManager::send_error_text(self, out, e.message()),
                }
            }
            _ => CommandManager::send_default_help_text(self, out),
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "[rebuilding id]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Lists the rebuild options of the nearest house you own, or starts the rebuild with the given id. Costs materials and labor.".into()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::harness;
    use super::super::test_house::fixture;
    use crate::data::GameData;
    use crate::systems::housing::data::{HousingBuildStep, HousingTemplate, Taxation};
    use crate::systems::housing::rebuild::{RebuildMaterial, Rebuilding};

    const TIER2: u32 = 302;

    fn data() -> GameData {
        let mut data = fixture::data();
        let h = &mut data.systems.housing;
        let mut build_steps = BTreeMap::new();
        build_steps.insert(
            0,
            HousingBuildStep {
                id: 900,
                housing_id: TIER2,
                step: 0,
                model_id: 1085,
                skill_id: 29291,
                num_actions: 1,
            },
        );
        h.add_template(HousingTemplate {
            id: TIER2,
            name: "Tier Two".into(),
            main_model_id: 900,
            hp: 1500,
            taxation: Some(Taxation {
                id: 2,
                tax: 100_000,
                show: true,
            }),
            taxation_id: 2,
            build_steps,
            ..Default::default()
        });
        h.rebuild.add(
            fixture::DESIGN,
            7,
            vec![
                Rebuilding {
                    id: 70,
                    name: "Upgrade".into(),
                    housing_id: TIER2,
                    materials: vec![RebuildMaterial {
                        item_id: 8101,
                        count: 2,
                    }],
                    ..Default::default()
                },
                Rebuilding {
                    id: 71,
                    name: "Free upgrade".into(),
                    housing_id: TIER2,
                    ..Default::default()
                },
            ],
        );
        data
    }

    #[tokio::test]
    async fn rebuild_lists_options_and_starts_one() {
        let server = harness::server_with(data());
        server
            .systems
            .services
            .commands
            .access_levels
            .load(&[("rebuild".into(), 0)]);
        let gm = harness::pc(&server, 1, "Alpha", 100.0, 100.0);
        gm.p.data().access_level = 0;
        let (_, out) = harness::run_at(&server, &gm.p, "rebuild", 0).await;
        assert_eq!(
            out,
            [harness::error(
                "rebuild",
                "You are not near a house of yours (within 30 m)."
            )]
        );

        let house = fixture::place_house(&server, &gm.p, 110.0, 100.0, -1);
        let (_, out) = harness::run_at(&server, &gm.p, "rebuild", 0).await;
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(out[0].contains("Rebuild options for"), "{out:?}");
        assert!(out[1].contains("70") && out[1].contains("Upgrade") && out[1].contains("0/2"));
        assert!(out[2].contains("71") && out[2].contains("Free upgrade"));
        assert!(out[2].contains("no materials"));

        // Missing materials: refused, nothing changes.
        let (_, out) = harness::run_at(&server, &gm.p, "rebuild 70", 0).await;
        assert_eq!(out, [harness::error("rebuild", "Not enough materials.")]);
        assert_eq!(house.data().unit.template_id, fixture::DESIGN);
        let (_, out) = harness::run_at(&server, &gm.p, "rebuild 9", 0).await;
        assert_eq!(
            out,
            [harness::error(
                "rebuild",
                "This rebuilding is not available for this house."
            )]
        );
        let (_, out) = harness::run_at(&server, &gm.p, "rebuild x", 0).await;
        assert!(out[0].contains("Help for"), "{out:?}");

        // The free option goes through.
        let (_, out) = harness::run_at(&server, &gm.p, "rebuild 71", 0).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("is being rebuilt"), "{out:?}");
        assert_eq!(house.data().unit.template_id, TIER2);
        assert_eq!(house.data().current_step, 0);

        // Another player cannot rebuild it (and does not even find "their" house).
        let other = harness::pc(&server, 2, "Beta", 110.0, 100.0);
        other.p.data().access_level = 0;
        let (_, out) = harness::run_at(&server, &other.p, "rebuild 71", 0).await;
        assert_eq!(
            out,
            [harness::error(
                "rebuild",
                "You are not near a house of yours (within 30 m)."
            )]
        );
    }

    #[tokio::test]
    async fn gm_house_rebuild_swaps_any_template() {
        let server = harness::server_with(data());
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let house = fixture::place_house(&server, &gm.p, 110.0, 100.0, -1);
        fixture::target(&gm.p, &house);
        let (_, out) = harness::run(&server, &gm.p, "house rebuild").await;
        assert_eq!(
            out,
            [harness::error("house", "Usage: rebuild <housing id>")]
        );
        let (_, out) = harness::run(&server, &gm.p, "house rebuild 12345").await;
        assert!(out[0].contains("Failed to rebuild"), "{out:?}");
        let (_, out) = harness::run(&server, &gm.p, "house rebuild 302").await;
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("into housing template 302"), "{out:?}");
        assert_eq!(house.data().unit.template_id, TIER2);
        assert_eq!(house.data().unit.model_id, 1085);
    }
}
