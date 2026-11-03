//! `Premium` GM command (Rust only, WP-B): grant / remove premium with an end time.
//!
//! `/premium <account id|character name|self> <on|off|days> [days]`
//!
//! * `on` / a positive day count grants premium from now until `now + days` (default 30 days).
//! * `off` removes it (`PaymentMethodType.None`, premium point `deactivate_point`).
//!
//! The grant / removal writes `aaemu_game.accounts` under the account lock, updates the runtime
//! state (`AccountManager::state`) and pushes `SCAccountInfo` + `SCUpdatePremiumPoint` to the
//! account's connection (when online). A grade change also swaps the `pcbang_buffs` buffs on the
//! online characters.

use chrono::Utc;
use futures::future::BoxFuture;

use super::prelude::*;
use crate::systems::services::premium;

/// Days a bare `on` grants.
pub const DEFAULT_DAYS: u32 = 30;
/// Upper bound for the day count (100 years): keeps `now + days * 86_400` inside `i64` and
/// stops a typo from granting forever.
pub const MAX_DAYS: u32 = 36_500;

/// `Premium` (`/premium`).
pub struct Premium;

impl Premium {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for Premium {
    fn command_names(&self) -> Vec<String> {
        names(&["premium", "setpremium"])
    }

    fn execute(
        &self,
        _ctx: &CommandContext<'_>,
        _args: &[String],
        _out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        anyhow::bail!("Premium runs through execute_async")
    }

    fn execute_async<'a>(
        &'a self,
        ctx: &'a CommandContext<'a>,
        args: &'a [String],
        out: &'a mut dyn MessageOutput,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let server = ctx.server()?;
            let character = ctx.player()?;
            let Some(target_arg) = args.first() else {
                CommandManager::send_default_help_text(self, out);
                return Ok(());
            };
            let account_id = match target_arg.as_str() {
                "self" => character.data().account_id,
                text => match try_parse::<u32>(text) {
                    Some(id) => id,
                    None => match server.world.character_by_name(text) {
                        Some(target) => target.data().account_id,
                        None => {
                            CommandManager::send_error_text(
                                self,
                                out,
                                &format!("Account or character '{text}' not found"),
                            );
                            return Ok(());
                        }
                    },
                },
            };
            let Some(action) = args.get(1).map(|a| a.to_ascii_lowercase()) else {
                CommandManager::send_default_help_text(self, out);
                return Ok(());
            };
            let now = Utc::now().timestamp();
            match action.as_str() {
                "off" | "none" => match premium::remove(server, account_id).await {
                    Ok(_) => {
                        tracing::info!("premium: account {account_id} removed by GM command");
                        CommandManager::send_normal_text(
                            self,
                            out,
                            &format!("Premium of account {account_id} removed"),
                        );
                    }
                    Err(e) => CommandManager::send_error_text(
                        self,
                        out,
                        &format!("Could not remove premium of account {account_id}: {e}"),
                    ),
                },
                "on" | "premium" | "grant" => {
                    let days = match args.get(2) {
                        Some(text) => match try_parse::<u32>(text) {
                            Some(days) => days,
                            None => {
                                CommandManager::send_error_text(
                                    self,
                                    out,
                                    &format!("'{text}' is not a day count"),
                                );
                                return Ok(());
                            }
                        },
                        None => DEFAULT_DAYS,
                    };
                    self.grant(server, account_id, now, days, out).await?;
                }
                text => match try_parse::<u32>(text) {
                    Some(days) => self.grant(server, account_id, now, days, out).await?,
                    None => CommandManager::send_default_help_text(self, out),
                },
            }
            Ok(())
        })
    }

    fn get_command_line_help(&self) -> String {
        "<account id|character name|self> <on|off|days> [days]".into()
    }

    fn get_command_help_text(&self) -> String {
        format!(
            "Grants premium until now + <days> (bare on = {DEFAULT_DAYS} days), or removes it with off. \
             Pushes SCAccountInfo and SCUpdatePremiumPoint and swaps the premium grade buffs."
        )
    }
}

impl Premium {
    async fn grant(
        &self,
        server: &GameServer,
        account_id: u32,
        now: i64,
        days: u32,
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if days == 0 || days > MAX_DAYS {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Days must be between 1 and {MAX_DAYS}"),
            );
            return Ok(());
        }
        let end = now.saturating_add(i64::from(days) * 86_400);
        if let Err(e) = premium::grant(server, account_id, now, end).await {
            CommandManager::send_error_text(
                self,
                out,
                &format!("Could not grant premium to account {account_id}: {e}"),
            );
            return Ok(());
        }
        let until = chrono::DateTime::from_timestamp(end, 0)
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| end.to_string());
        tracing::info!("premium: account {account_id} granted until {until} by GM command");
        CommandManager::send_normal_text(
            self,
            out,
            &format!("Premium of account {account_id} granted until {until}"),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use futures::future::BoxFuture;

    use crate::data::skills::{BuffKind, BuffTemplate};
    use crate::data::GameData;
    use crate::server::GameServer;
    use crate::systems::services::commands::scripts::harness;
    use crate::systems::services::premium::{
        PcBangBuff, PremiumBenefit, PremiumGrade, PremiumStore,
    };

    /// An in-memory store for the command flow (the harness DB is closed); `fail` simulates a
    /// persistence error.
    #[derive(Default)]
    struct MemoryStore {
        fail: AtomicBool,
    }

    impl PremiumStore for MemoryStore {
        fn update_payment<'a>(
            &'a self,
            _server: &'a GameServer,
            _account_id: u32,
            _payment: crate::net::connection::AccountPayment,
            _premium_point: i32,
        ) -> BoxFuture<'a, crate::Result<()>> {
            let fail = self.fail.load(Ordering::Relaxed);
            Box::pin(async move {
                if fail {
                    Err(crate::GameError::Protocol("database down".into()))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn install_store(server: &GameServer) -> Arc<MemoryStore> {
        let store = Arc::new(MemoryStore::default());
        server.systems.services.premium_store.install(store.clone());
        store
    }

    /// `premium_grades` (1 -> 0, 2 -> 1), the benefit rows and the active VIP buff row.
    fn premium_data() -> GameData {
        let mut data = GameData::empty();
        data.systems.services.premium.grades = vec![
            PremiumGrade {
                id: 1,
                grade_id: 1,
                point: 0,
            },
            PremiumGrade {
                id: 2,
                grade_id: 2,
                point: 1,
            },
        ];
        data.systems.services.premium.benefits = vec![
            PremiumBenefit {
                id: 1,
                online_labor: 5,
                offline_labor: 0,
                max_labor: 2000,
                icon_id: 0,
                grade_id: 1,
            },
            PremiumBenefit {
                id: 2,
                online_labor: 10,
                offline_labor: 5,
                max_labor: 5000,
                icon_id: 0,
                grade_id: 2,
            },
        ];
        data.systems.services.premium.pcbang_buffs = vec![PcBangBuff {
            id: 8_000_001,
            buff_id: 8_000_011,
            active: true,
            kind_id: 3,
            premium_grade_id: 2,
        }];
        data.skills
            .buffs
            .insert(8_000_011, BuffTemplate::simple(8_000_011, BuffKind::Good));
        // The behaviour path (`add_buff_by_id`) reads the subsystem's full template.
        let full = crate::systems::skills::data::templates::BuffTemplate {
            id: 8_000_011,
            kind: 1,
            ..Default::default()
        };
        data.systems
            .skills
            .buffs
            .insert(8_000_011, std::sync::Arc::new(full));
        data
    }

    fn frames(pc: &mut harness::Pc) -> Vec<(u16, Vec<u8>)> {
        pc.packets()
    }

    #[tokio::test]
    async fn premium_off_removes_it_and_pushes_the_evidenced_packets() {
        use crate::opcodes::sc;

        let server = harness::server_with(premium_data());
        install_store(&server);
        let mut gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        // The configured default (premium) is grade 2; apply its buff as the login path would.
        crate::systems::services::premium::apply_grade_buffs(&server, &gm.p, 2);
        assert!(gm.p.data().unit.buffs.check_buff(8_000_011));
        gm.client.drain();

        let (handled, out) = harness::run(&server, &gm.p, "premium self off").await;
        assert!(handled);
        assert!(out[0].contains("removed"), "{out:?}");
        let state = crate::systems::services::accounts::state(&server, 1001);
        assert_eq!(
            state.payment.method,
            crate::net::connection::PaymentMethodType::None
        );
        assert_eq!(state.premium_point, 0);
        assert!(!gm.p.data().unit.buffs.check_buff(8_000_011));

        let frames = frames(&mut gm);
        let account_info = frames
            .iter()
            .find(|(op, _)| *op == sc::SCAccountInfoPacket)
            .expect("SCAccountInfo");
        // [payMethod i32 = 5][payLocation i32 = 1][payStart i64 = 0][payEnd i64 = 0][realPayTime = 0]
        assert_eq!(
            account_info.1,
            [
                5, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0
            ]
        );
        let update = frames
            .iter()
            .find(|(op, _)| *op == sc::SCUpdatePremiumPointPacket)
            .expect("SCUpdatePremiumPoint");
        // [point i32 = 0][oldPg u8 = 2][pg u8 = 1]
        assert_eq!(update.1, [0, 0, 0, 0, 2, 1]);
    }

    #[tokio::test]
    async fn premium_grant_failure_is_reported_and_not_applied() {
        // The harness database points at a closed port: the upsert fails, so the command must
        // not answer "granted" and must leave the runtime state untouched.
        let server = harness::server_with(premium_data());
        let store = install_store(&server);
        store.fail.store(true, Ordering::Relaxed);
        let mut gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        gm.client.drain();
        let (handled, out) = harness::run(&server, &gm.p, "premium self 2").await;
        assert!(handled);
        assert!(out[0].contains("Could not grant"), "{out:?}");
        let state = crate::systems::services::accounts::state(&server, 1001);
        assert_eq!(
            state.payment.method,
            crate::net::connection::PaymentMethodType::Premium
        );
        assert_eq!(state.payment.end_time, 0, "the default stays permanent");
        assert_eq!(state.premium_point, 1);
        assert!(
            gm.packets().is_empty(),
            "a failed grant must not push account packets"
        );
    }

    #[tokio::test]
    async fn premium_on_grants_days_and_grade_two() {
        use crate::opcodes::sc;

        let server = harness::server_with(premium_data());
        install_store(&server);
        let mut gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let _ = harness::run(&server, &gm.p, "premium self off").await;
        gm.client.drain();

        let (handled, out) = harness::run(&server, &gm.p, "premium self 7").await;
        assert!(handled);
        assert!(out[0].contains("granted until"), "{out:?}");
        let state = crate::systems::services::accounts::state(&server, 1001);
        assert_eq!(
            state.payment.method,
            crate::net::connection::PaymentMethodType::Premium
        );
        assert_eq!(state.premium_point, 1);
        let days = (state.payment.end_time - state.payment.start_time) / 86_400;
        assert_eq!(days, 7);
        assert!(gm.p.data().unit.buffs.check_buff(8_000_011));

        let frames = frames(&mut gm);
        let update = frames
            .iter()
            .find(|(op, _)| *op == sc::SCUpdatePremiumPointPacket)
            .expect("SCUpdatePremiumPoint");
        // [point i32 = 1][oldPg u8 = 1][pg u8 = 2]
        assert_eq!(update.1, [1, 0, 0, 0, 1, 2]);
    }
}
