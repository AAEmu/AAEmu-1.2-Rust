use futures::future::BoxFuture;
use sqlx::mysql::MySqlPool;
use sqlx::Row;

use crate::db;
use crate::db::AccountDetails;
use crate::error::Result;
use crate::server::GameServer;

use super::{currency, AuditIcsSale, CharacterRow};

/// Account values and the sales audit (`AccountManager` credits / loyalty,
/// `CashShopManager.GetSalesForShopItem` / `LogSale` / `UpdateRemainingShopItemStock`).
pub trait CashShopDb: Send + Sync {
    /// `AccountManager.GetAccountDetails(accountId)`.
    fn account_details<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
    ) -> BoxFuture<'a, AccountDetails>;
    /// `AccountManager.AddCredits(accountId, amount)`.
    fn add_credits<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        amount: i32,
    ) -> BoxFuture<'a, bool>;
    /// `AccountManager.AddLoyalty(accountId, amount)`.
    fn add_loyalty<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        amount: i32,
    ) -> BoxFuture<'a, bool>;
    /// `CashShopManager.GetSalesForShopItem(accountId, characterId, shopItemId)`.
    fn sales_for_shop_item<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        character_id: u32,
        shop_item_id: u32,
    ) -> BoxFuture<'a, Vec<AuditIcsSale>>;
    /// `CashShopManager.LogSale(...)`.
    fn log_sale<'a>(&'a self, server: &'a GameServer, sale: AuditIcsSale) -> BoxFuture<'a, bool>;
    /// `CashShopManager.UpdateRemainingShopItemStock(shopItemId, newRemaining)`.
    fn update_remaining_stock<'a>(
        &'a self,
        server: &'a GameServer,
        shop_item_id: u32,
        remaining: i32,
    ) -> BoxFuture<'a, bool>;
    /// `SELECT ... FROM characters WHERE name = ?`: the row of a gift receiver that is not
    /// online (`WorldManager::GetCharacter` only sees online characters; the C# therefore
    /// refused every offline gift).
    fn find_character_by_name<'a>(
        &'a self,
        server: &'a GameServer,
        name: &'a str,
    ) -> BoxFuture<'a, Option<CharacterRow>>;
}

/// The C# MySQL implementation of [`CashShopDb`].
#[derive(Debug, Default)]
pub struct MySqlCashShopDb;

impl CashShopDb for MySqlCashShopDb {
    fn account_details<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
    ) -> BoxFuture<'a, AccountDetails> {
        Box::pin(db::get_account_details(
            &server.db,
            &server.config,
            account_id,
        ))
    }

    fn add_credits<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        amount: i32,
    ) -> BoxFuture<'a, bool> {
        Box::pin(add_credits_sql(&server.db, account_id, amount))
    }

    fn add_loyalty<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        amount: i32,
    ) -> BoxFuture<'a, bool> {
        Box::pin(add_loyalty_sql(&server.db, account_id, amount))
    }

    fn sales_for_shop_item<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        character_id: u32,
        shop_item_id: u32,
    ) -> BoxFuture<'a, Vec<AuditIcsSale>> {
        Box::pin(async move {
            match sales_for_shop_item_sql(&server.db, account_id, character_id, shop_item_id).await
            {
                Ok(v) => v,
                Err(e) => {
                    // The C# lets the exception escape (the buy task dies).
                    tracing::error!("{e}");
                    Vec::new()
                }
            }
        })
    }

    fn log_sale<'a>(&'a self, server: &'a GameServer, sale: AuditIcsSale) -> BoxFuture<'a, bool> {
        Box::pin(log_sale_sql(&server.db, sale))
    }

    fn update_remaining_stock<'a>(
        &'a self,
        server: &'a GameServer,
        shop_item_id: u32,
        remaining: i32,
    ) -> BoxFuture<'a, bool> {
        Box::pin(update_remaining_stock_sql(
            &server.db,
            shop_item_id,
            remaining,
        ))
    }

    fn find_character_by_name<'a>(
        &'a self,
        server: &'a GameServer,
        name: &'a str,
    ) -> BoxFuture<'a, Option<CharacterRow>> {
        Box::pin(async move {
            match find_character_by_name_sql(&server.db, name).await {
                Ok(row) => row,
                Err(e) => {
                    // The C# lookup cannot fail here (online only); refusing the gift is safer
                    // than delivering it to an unresolved character.
                    tracing::error!("Cash shop receiver lookup for {name:?} failed: {e}");
                    None
                }
            }
        })
    }
}

/// `AccountManager.AddCredits` (`RemoveCredits` = negative amount).
pub async fn add_credits_sql(pool: &MySqlPool, account_id: u32, amount: i32) -> bool {
    if amount < 0 {
        return spend_account_points_sql(pool, account_id, amount, currency::CREDITS).await;
    }
    match sqlx::query(
        "INSERT INTO accounts (account_id, credits) VALUES(?, ?) ON DUPLICATE KEY UPDATE credits = credits + ?",
    )
    .bind(account_id)
    .bind(amount)
    .bind(amount)
    .execute(pool)
    .await
    {
        Ok(r) => r.rows_affected() > 0,
        Err(e) => {
            tracing::error!("{e}");
            false
        }
    }
}

/// `AccountManager.AddLoyalty`.
pub async fn add_loyalty_sql(pool: &MySqlPool, account_id: u32, amount: i32) -> bool {
    if amount < 0 {
        return spend_account_points_sql(pool, account_id, amount, currency::LOYALTY).await;
    }
    match sqlx::query(
        "INSERT INTO accounts (account_id, loyalty) VALUES(?, ?) ON DUPLICATE KEY UPDATE loyalty = loyalty + ?",
    )
    .bind(account_id)
    .bind(amount)
    .bind(amount)
    .execute(pool)
    .await
    {
        Ok(r) => r.rows_affected() > 0,
        Err(e) => {
            tracing::error!("{e}");
            false
        }
    }
}

/// Account debits must pass a balance check in the same SQL statement as the debit.
async fn spend_account_points_sql(
    pool: &MySqlPool,
    account_id: u32,
    amount: i32,
    currency: u8,
) -> bool {
    let sql = match currency {
        currency::CREDITS => {
            "UPDATE accounts SET credits = credits + ? WHERE account_id = ? AND credits >= ?"
        }
        currency::LOYALTY => {
            "UPDATE accounts SET loyalty = loyalty + ? WHERE account_id = ? AND loyalty >= ?"
        }
        _ => return false,
    };
    match sqlx::query(sql)
        .bind(amount)
        .bind(account_id)
        .bind(-i64::from(amount))
        .execute(pool)
        .await
    {
        Ok(result) => result.rows_affected() > 0,
        Err(error) => {
            tracing::error!("Cash shop debit failed: {error}");
            false
        }
    }
}

const SALES_COLUMNS: &str = "CAST(id AS SIGNED) AS id, CAST(buyer_account AS SIGNED) AS buyer_account, \
     CAST(buyer_char AS SIGNED) AS buyer_char, CAST(target_account AS SIGNED) AS target_account, \
     CAST(target_char AS SIGNED) AS target_char, sale_date, CAST(shop_item_id AS SIGNED) AS shop_item_id, \
     CAST(sku AS SIGNED) AS sku, CAST(sale_cost AS SIGNED) AS sale_cost, \
     CAST(sale_currency AS SIGNED) AS sale_currency, description";

/// `CashShopManager.GetSalesForShopItem`.
pub async fn sales_for_shop_item_sql(
    pool: &MySqlPool,
    account_id: u32,
    character_id: u32,
    shop_item_id: u32,
) -> Result<Vec<AuditIcsSale>> {
    let mut res = Vec::new();
    if (account_id == 0 && character_id == 0) || shop_item_id == 0 {
        return Ok(res);
    }
    let rows = if character_id > 0 {
        sqlx::query(&format!(
            "SELECT {SALES_COLUMNS} FROM audit_ics_sales WHERE (buyer_char = ?) AND (shop_item_id = ?)"
        ))
        .bind(character_id)
        .bind(shop_item_id)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(&format!(
            "SELECT {SALES_COLUMNS} FROM audit_ics_sales WHERE (buyer_account = ?) AND (shop_item_id = ?)"
        ))
        .bind(account_id)
        .bind(shop_item_id)
        .fetch_all(pool)
        .await?
    };
    for row in rows {
        res.push(AuditIcsSale {
            id: db::int(&row, "id")?,
            buyer_account: db::int(&row, "buyer_account")? as u32,
            buyer_char: db::int(&row, "buyer_char")? as u32,
            target_account: db::int(&row, "target_account")? as u32,
            target_char: db::int(&row, "target_char")? as u32,
            sale_date: db::dt(&row, "sale_date"),
            shop_item_id: db::int(&row, "shop_item_id")? as u32,
            sku: db::int(&row, "sku")? as u32,
            sale_cost: db::int(&row, "sale_cost")? as i32,
            sale_currency: db::int(&row, "sale_currency")? as u8,
            description: row.try_get::<String, _>("description")?,
        });
    }
    Ok(res)
}

/// `CashShopManager.LogSale`.
pub async fn log_sale_sql(pool: &MySqlPool, sale: AuditIcsSale) -> bool {
    match sqlx::query(
        "INSERT INTO audit_ics_sales (buyer_account, buyer_char, target_account, target_char, sale_date, shop_item_id, sku, sale_cost, sale_currency, description) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sale.buyer_account)
    .bind(sale.buyer_char)
    .bind(sale.target_account)
    .bind(sale.target_char)
    .bind(sale.sale_date.unwrap_or_else(db::min_value))
    .bind(sale.shop_item_id)
    .bind(sale.sku)
    .bind(sale.sale_cost as u32) // the C# passes the uint saleCost
    .bind(sale.sale_currency)
    .bind(sale.description)
    .execute(pool)
    .await
    {
        Ok(r) if r.rows_affected() > 0 => true,
        Ok(_) => {
            tracing::error!("Saving sale failed");
            false
        }
        Err(e) => {
            tracing::error!("Saving sale failed Exception: {e}");
            false
        }
    }
}

/// `CashShopManager.UpdateRemainingShopItemStock`.
pub async fn update_remaining_stock_sql(
    pool: &MySqlPool,
    shop_item_id: u32,
    remaining: i32,
) -> bool {
    match sqlx::query("UPDATE ics_shop_items SET `remaining` = ? WHERE `shop_id` = ?")
        .bind(remaining)
        .bind(shop_item_id)
        .execute(pool)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => true,
        Ok(_) => {
            tracing::error!("Updating stock failed! ShopItem: {shop_item_id} -> {remaining}");
            false
        }
        Err(e) => {
            tracing::error!("Stock updating failed Exception: {e}");
            false
        }
    }
}

/// `characters.name` uses a case- and accent-insensitive collation (`SQL/aaemu_game.sql`), so the
/// SQL `name = ?` can return a different spelling ("Elan" for "Élan"). The online lookup
/// (`WorldManager::character_by_name`, `world/mod.rs`) compares lowercased names; apply the same
/// rule to the offline row so a gift cannot go to a different character.
pub(super) fn same_character_name(typed: &str, stored: &str) -> bool {
    typed.to_lowercase() == stored.to_lowercase()
}

/// The gift receiver's `characters` row (offline gifts). `deleted` is a signed tinyint, so it is
/// decoded as `i64`; a pending deletion is either `deleted != 0` or `delete_request_time` set
/// (`CharacterManager.IsCharacterPendingDeletion`).
pub async fn find_character_by_name_sql(
    pool: &MySqlPool,
    name: &str,
) -> Result<Option<CharacterRow>> {
    let row = sqlx::query(
        "SELECT CAST(id AS SIGNED) AS id, name, CAST(account_id AS SIGNED) AS account_id, \
         CAST(deleted AS SIGNED) AS deleted, delete_request_time, transfer_request_time \
         FROM characters WHERE name = ? ORDER BY deleted ASC, id ASC LIMIT 1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let stored_name: String = row.try_get("name")?;
    // The collation matched a different spelling: treat it as an unknown name (623).
    if !same_character_name(name, &stored_name) {
        return Ok(None);
    }
    let deleted = db::int(&row, "deleted")? != 0;
    Ok(Some(CharacterRow {
        id: db::int(&row, "id")? as u32,
        name: stored_name,
        account_id: db::int(&row, "account_id")? as u32,
        deleted,
        delete_requested: deleted || db::is_set(db::dt(&row, "delete_request_time")),
        transfer_requested: db::is_set(db::dt(&row, "transfer_request_time")),
    }))
}
