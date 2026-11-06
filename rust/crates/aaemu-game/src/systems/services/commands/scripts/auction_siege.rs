//! Retail GM commands `auction_post_siege_items` and `auction_cancel`.
//!
//! The 1.2 client's GM console knows both names (`strings.txt`; EVIDENCED). The argument shapes
//! are UNKNOWN: `auction_post_siege_items [duration] [startPrice] [buyoutPrice]` is assumed, with
//! the defaults of the audit (`siege-social-2.md` WP-SD1b): 48 h duration class, the item's own
//! `price` as the start price and no buyout. The command posts one lot per distinct
//! `siege_zones.declare_item_id` (the six Siege Camps) as a system lot (`client_id = 0`, empty
//! seller name). `auction_cancel <lotId>` removes such a lot and deletes its item; player lots
//! and lots with a bid are refused.

use super::prelude::*;
use crate::systems::economy::auction::{auction_duration, AuctionLot, AuctionManager};

/// Distinct `siege_zones.declare_item_id` values, in id order (the six camps in 1.2 data).
fn declare_item_ids(server: &GameServer) -> Vec<u32> {
    let zones = &server.game_data.systems.housing.castles.siege_zones;
    let mut ids: Vec<u32> = zones
        .values()
        .map(|z| z.declare_item_id)
        .filter(|id| *id != 0)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Deletes a lot that was created by [`AuctionPostSiegeItems`] before it is accepted: the item
/// is discarded and both ids are released, so a failed run leaves nothing behind.
fn discard_lot(server: &GameServer, lot: &AuctionLot) {
    server.items.release_id(lot.item_id(), &server.ids.item);
    server.ids.auction.release_id(lot.id as u32);
}

/// `auction_post_siege_items`.
pub struct AuctionPostSiegeItems;

impl AuctionPostSiegeItems {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for AuctionPostSiegeItems {
    fn command_names(&self) -> Vec<String> {
        names(&["auction_post_siege_items", "auction_post_siege"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        if args.len() > 3 {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        }
        let duration = match args.first() {
            Some(arg) => match try_parse::<u8>(arg) {
                Some(d) if d <= auction_duration::HOURS_48 => d,
                _ => {
                    CommandManager::send_error_text(
                        self,
                        out,
                        &format!("Invalid duration '{arg}' (0-3: 6/12/24/48 hours)"),
                    );
                    return Ok(());
                }
            },
            None => auction_duration::HOURS_48,
        };
        let start_price = match args.get(1).map(|a| try_parse::<i32>(a)) {
            Some(Some(p)) if p >= 0 => Some(p),
            Some(_) => {
                CommandManager::send_error_text(
                    self,
                    out,
                    "The start price must be a non-negative number",
                );
                return Ok(());
            }
            None => None,
        };
        let buyout_price = match args.get(2).map(|a| try_parse::<i32>(a)) {
            Some(Some(p)) if p >= 0 => p,
            Some(_) => {
                CommandManager::send_error_text(
                    self,
                    out,
                    "The buyout price must be a non-negative number",
                );
                return Ok(());
            }
            None => 0,
        };
        if buyout_price > 0 && start_price.is_some_and(|start| buyout_price < start) {
            CommandManager::send_error_text(
                self,
                out,
                "The buyout price must be zero or at least the start price",
            );
            return Ok(());
        }

        let server = ctx.server()?;
        let ids = declare_item_ids(server);
        if ids.is_empty() {
            CommandManager::send_error_text(self, out, "No siege_zones declare items are loaded");
            return Ok(());
        }
        let _items = server.systems.economy.operation_scope();
        let auction = &server.systems.economy.auction;
        let _op = auction.operation_lock();
        let now = chrono::Utc::now();
        let mut created = Vec::with_capacity(ids.len());
        for template_id in ids {
            let Some(template) = server.game_data.items.template(template_id) else {
                // Skip a bad data row instead of failing the whole post.
                tracing::warn!(
                    "auction_post_siege_items: siege_zones declare item {template_id} has no item template"
                );
                continue;
            };
            let start = start_price.unwrap_or(template.price);
            match AuctionManager::create_system_lot(
                server,
                template_id,
                start,
                buyout_price,
                duration,
                now,
            ) {
                Some(lot) => created.push(lot),
                None => {
                    for lot in &created {
                        discard_lot(server, lot);
                    }
                    CommandManager::send_error_text(
                        self,
                        out,
                        "Could not create every siege camp lot; nothing was posted",
                    );
                    return Ok(());
                }
            }
        }
        for lot in &created {
            auction.add_auction_lot(lot.clone());
        }
        CommandManager::send_normal_text(
            self,
            out,
            &format!("Posted {} siege camp lot(s)", created.len()),
        );
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "[duration] [startPrice] [buyoutPrice]".into()
    }

    fn get_command_help_text(&self) -> String {
        "Posts one system lot per siege_zones.declare_item_id (the six Siege Camps).\n\
         Arguments are optional and positional (assumed; the retail shape is unknown):\n\
         duration 0-3 = 6/12/24/48 hours (default 48), startPrice (default: the item price),\n\
         buyoutPrice (default 0 = no buyout)."
            .into()
    }
}

/// `auction_cancel`.
pub struct AuctionCancel;

impl AuctionCancel {
    pub fn new(_env: &ScriptEnv) -> Self {
        Self
    }
}

impl Command for AuctionCancel {
    fn command_names(&self) -> Vec<String> {
        names(&["auction_cancel"])
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        args: &[String],
        out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        let Some(arg) = args.first() else {
            CommandManager::send_default_help_text(self, out);
            return Ok(());
        };
        let Some(lot_id) = try_parse::<u64>(arg) else {
            CommandManager::send_error_text(self, out, &format!("Invalid auction lot id '{arg}'"));
            return Ok(());
        };
        let server = ctx.server()?;
        match server
            .systems
            .economy
            .auction
            .cancel_system_lot(server, lot_id)
        {
            Ok(lot) => CommandManager::send_normal_text(
                self,
                out,
                &format!(
                    "Cancelled system auction lot {lot_id} (item {} deleted)",
                    lot.item_template_id()
                ),
            ),
            Err(e) => CommandManager::send_error_text(self, out, e),
        }
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        "<lotId>".into()
    }

    fn get_command_help_text(&self) -> String {
        "Cancels an unbid system auction lot (client_id 0) and deletes its item.\n\
         Player lots and lots with a bid are refused; the lot id argument is assumed."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::items::ItemTemplate;
    use crate::data::GameData;
    use crate::models::container::slot_type;
    use crate::systems::housing::data::SiegeZone;
    use crate::systems::services::commands::scripts::harness;

    const CAMPS: [u32; 6] = [21130, 21134, 21135, 21136, 27744, 27745];

    fn data() -> GameData {
        let mut d = GameData::empty();
        for (i, id) in CAMPS.iter().enumerate() {
            let mut t = ItemTemplate::plain(*id, 1);
            t.price = 5_000_000;
            t.auction_category_a = 11;
            d.items.templates.insert(*id, t);
            let zone_group_id = 33 + i as u32;
            d.systems.housing.castles.siege_zones.insert(
                zone_group_id,
                SiegeZone {
                    id: zone_group_id,
                    zone_group_id,
                    declare_item_id: *id,
                    ..Default::default()
                },
            );
        }
        d
    }

    #[tokio::test]
    async fn auction_post_siege_items_posts_one_system_lot_per_declare_item() {
        let server = harness::server_with(data());
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let (handled, out) = harness::run(&server, &gm.p, "auction_post_siege_items").await;
        assert!(handled);
        assert_eq!(
            out,
            [harness::normal(
                "auction_post_siege_items",
                "Posted 6 siege camp lot(s)"
            )]
        );

        let auction = &server.systems.economy.auction;
        let mut lots = auction.all_lots();
        lots.sort_by_key(|l| l.item_template_id());
        assert_eq!(lots.len(), CAMPS.len());
        for (lot, template_id) in lots.iter().zip(CAMPS) {
            assert_eq!(lot.item_template_id(), template_id);
            assert!(lot.is_system_lot(), "a siege camp lot has no player seller");
            assert_eq!((lot.client_id, lot.client_name.as_str()), (0, ""));
            assert_eq!((lot.start_money, lot.direct_money), (5_000_000, 0));
            assert_eq!(lot.duration, auction_duration::HOURS_48);
            assert_eq!(lot.end_time - lot.post_date, chrono::Duration::hours(48));
            let item = server
                .items
                .item_by_id(lot.item_id())
                .expect("the camp item exists");
            assert_eq!(
                (item.template_id, item.count, item.owner_id, item.slot_type),
                (template_id, 1, 0, slot_type::AUCTION)
            );
        }
        // Each item was created exactly once.
        for template_id in CAMPS {
            assert_eq!(
                lots.iter()
                    .filter(|l| l.item_template_id() == template_id)
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn auction_post_siege_items_arguments_are_optional_and_validated() {
        let server = harness::server_with(data());
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let (_, out) = harness::run(&server, &gm.p, "auction_post_siege_items 1 100 500").await;
        assert_eq!(
            out,
            [harness::normal(
                "auction_post_siege_items",
                "Posted 6 siege camp lot(s)"
            )]
        );
        for lot in server.systems.economy.auction.all_lots() {
            assert_eq!(
                (lot.duration, lot.start_money, lot.direct_money),
                (auction_duration::HOURS_12, 100, 500)
            );
        }

        // Bad arguments post nothing and consume no ids.
        let server = harness::server_with(data());
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let next_item = server.ids.item.get_next_id().unwrap();
        server.ids.item.release_id(next_item);
        for (args, message) in [
            ("9", "Invalid duration '9' (0-3: 6/12/24/48 hours)"),
            ("1 -5", "The start price must be a non-negative number"),
            ("1 5 -1", "The buyout price must be a non-negative number"),
            (
                "1 100 50",
                "The buyout price must be zero or at least the start price",
            ),
        ] {
            let (handled, out) =
                harness::run(&server, &gm.p, &format!("auction_post_siege_items {args}")).await;
            assert!(handled);
            assert_eq!(
                out,
                [harness::error("auction_post_siege_items", message)],
                "{args}"
            );
            assert!(server.systems.economy.auction.all_lots().is_empty());
        }
        assert_eq!(server.ids.item.get_next_id().unwrap(), next_item);
    }

    #[tokio::test]
    async fn auction_post_siege_items_skips_a_declare_item_without_a_template() {
        let mut d = data();
        d.systems.housing.castles.siege_zones.insert(
            99,
            SiegeZone {
                id: 99,
                zone_group_id: 99,
                declare_item_id: 999_999,
                ..Default::default()
            },
        );
        let server = harness::server_with(d);
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        let (_, out) = harness::run(&server, &gm.p, "auction_post_siege_items").await;
        assert_eq!(
            out,
            [harness::normal(
                "auction_post_siege_items",
                "Posted 6 siege camp lot(s)"
            )]
        );
        assert_eq!(server.systems.economy.auction.all_lots().len(), 6);
    }

    #[tokio::test]
    async fn auction_cancel_removes_the_system_lot_and_deletes_its_item() {
        let server = harness::server_with(data());
        let gm = harness::pc(&server, 1, "Gm", 100.0, 100.0);
        harness::run(&server, &gm.p, "auction_post_siege_items").await;
        let auction = &server.systems.economy.auction;
        let lot = auction.all_lots().into_iter().next().expect("a lot");
        let item_id = lot.item_id();

        let (_, out) = harness::run(&server, &gm.p, &format!("auction_cancel {}", lot.id)).await;
        assert_eq!(
            out,
            [harness::normal(
                "auction_cancel",
                &format!(
                    "Cancelled system auction lot {} (item {} deleted)",
                    lot.id,
                    lot.item_template_id()
                )
            )]
        );
        assert!(auction.lot(lot.id).is_none());
        assert!(server.items.item_by_id(item_id).is_none());
        assert_eq!(auction.all_lots().len(), 5);

        // Unknown lot and bad id are reported, nothing changes.
        let (_, out) = harness::run(&server, &gm.p, &format!("auction_cancel {}", lot.id)).await;
        assert_eq!(
            out,
            [harness::error("auction_cancel", "no such auction lot")]
        );
        let (_, out) = harness::run(&server, &gm.p, "auction_cancel xyz").await;
        assert_eq!(
            out,
            [harness::error(
                "auction_cancel",
                "Invalid auction lot id 'xyz'"
            )]
        );
    }

    /// The system lots and their items survive a save / reload round trip: after a restart the
    /// lots are listed again and `client_id = 0` is not treated as a data error.
    #[tokio::test]
    #[ignore = "requires private MySQL3317 via AAEMU_TEST_MYSQL; connection-local temporary tables only"]
    async fn private_mysql_system_lots_survive_reload() {
        use crate::config::AppConfig;
        use crate::data::world_templates::{WorldTemplate, WorldTemplates};
        use crate::test_support::TEST_ZONE;
        use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
        use std::str::FromStr;

        let url = std::env::var("AAEMU_TEST_MYSQL").expect("AAEMU_TEST_MYSQL");
        let options = MySqlConnectOptions::from_str(&url).unwrap();
        assert_eq!(
            options.get_port(),
            3317,
            "Only private MySQL3317 is allowed"
        );
        assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        for table in ["auction_house", "items", "item_containers"] {
            let (_, ddl): (String, String) =
                sqlx::query_as(&format!("SHOW CREATE TABLE `{table}`"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            sqlx::query(&ddl.replacen("CREATE TABLE", "CREATE TEMPORARY TABLE", 1))
                .execute(&pool)
                .await
                .unwrap();
        }
        let build = |pool: sqlx::MySqlPool| {
            let mut d = data();
            d.worlds = WorldTemplates::from_templates(
                vec![WorldTemplate::synthetic(0, "main_world", 2, 2, TEST_ZONE)],
                Vec::new(),
            )
            .expect("worlds");
            let server = std::sync::Arc::new(GameServer::new(AppConfig::default(), pool, d));
            server.set_self();
            server
        };

        let server = build(pool.clone());
        {
            let auction = &server.systems.economy.auction;
            let _items = server.systems.economy.operation_scope();
            let _op = auction.operation_lock();
            for template_id in CAMPS {
                let lot = AuctionManager::create_system_lot(
                    &server,
                    template_id,
                    5_000_000,
                    0,
                    auction_duration::HOURS_48,
                    chrono::Utc::now(),
                )
                .expect("system lot");
                auction.add_auction_lot(lot);
            }
        }
        let mut tx = pool.begin().await.unwrap();
        server.items.save(&mut tx).await.unwrap();
        server
            .systems
            .economy
            .auction
            .save(&server, &mut tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let reloaded = build(pool.clone());
        reloaded
            .items
            .load_user_items(
                &pool,
                &reloaded.game_data.items,
                1,
                &reloaded.ids.container,
                &reloaded.ids.item,
            )
            .await
            .unwrap();
        assert!(reloaded.systems.economy.auction.load(&reloaded).await);
        let mut lots = reloaded.systems.economy.auction.all_lots();
        assert_eq!(lots.len(), CAMPS.len());
        lots.sort_by_key(|l| l.item_template_id());
        for (lot, template_id) in lots.iter().zip(CAMPS) {
            assert_eq!(lot.item_template_id(), template_id);
            assert!(lot.is_system_lot());
            assert_eq!((lot.client_id, lot.client_name.as_str()), (0, ""));
            let item = reloaded
                .items
                .item_by_id(lot.item_id())
                .expect("the system lot item was reloaded");
            assert_eq!(
                (item.template_id, item.slot_type, item.owner_id),
                (template_id, slot_type::AUCTION, 0)
            );
        }
        pool.close().await;
    }

    /// The six ids the command posts are the real `siege_zones.declare_item_id` set.
    /// `AAEMU_COMPACT_DB=... cargo test -p aaemu-game auction_real_compact_db -- --ignored`.
    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB"]
    fn auction_real_compact_db_declare_items() {
        let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
            return;
        };
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let castles = crate::systems::housing::data::CastleReferenceData::load(&conn).unwrap();
        let mut ids: Vec<u32> = castles
            .siege_zones
            .values()
            .map(|z| z.declare_item_id)
            .filter(|id| *id != 0)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids, CAMPS.to_vec());
    }

    #[tokio::test]
    async fn non_gm_is_refused() {
        let server = harness::server_with(data());
        let player = harness::pc(&server, 1, "Player", 100.0, 100.0);
        player.p.data().access_level = 0;
        let (handled, out) =
            harness::run_at(&server, &player.p, "auction_post_siege_items", 0).await;
        assert!(handled);
        assert_eq!(out, ["|cFFFF0000Insufficient privileges.|r"]);
        assert!(server.systems.economy.auction.all_lots().is_empty());

        harness::run(&server, &player.p, "auction_post_siege_items").await;
        let lot = server
            .systems
            .economy
            .auction
            .all_lots()
            .into_iter()
            .next()
            .expect("a lot");
        let (handled, _) =
            harness::run_at(&server, &player.p, &format!("auction_cancel {}", lot.id), 0).await;
        assert!(handled);
        assert!(server.systems.economy.auction.lot(lot.id).is_some());
    }
}
