//! Account payment, premium state, premium grade benefits and the PC-bang buff rows (WP-B).
//!
//! # Evidence
//!
//! * `SCAccountInfo` (0x1c2) is serialized as `i32 payMethod, i32 payLocation, i64 payStart,
//!   i64 payEnd, i64 realPayTime` by the 1.2 client (`FUN_397aeb90`); the handler `FUN_391d4050`
//!   stores method/location/start/end at `DAT_39b02294+0x2c..0x3c` and logs the premium point /
//!   grade. The C# sends it at lobby `FinishState(0)` and on `CSListCharacter`; Rust keeps both
//!   moments.
//! * `SCUpdatePremiumPoint` (0x201) is `i32 point, u8 oldPg, u8 pg` (`FUN_397b8b00`); the handler
//!   `FUN_39186340` stores the point at `player+0x68c`, raises `PREMIUM_POINT_CHANGE` and (only
//!   when `oldPg != pg`) `PREMIUM_GRADE_CHANGE`. The C# sends `(1,1,1)` at `FinishState(7)`; the
//!   Rust sends the real point and grade there.
//! * The grade is derived client side from the point with `premium_grades`: `FUN_397a7d10` starts
//!   at grade 1 and takes the highest `grade_id` whose `point <= p`. The real rows are
//!   `(grade_id 1, point 0)` and `(grade_id 2, point 1)`, so a premium account (point 1) is
//!   grade 2 / VIP.
//! * `premium_configs` (one row): `max_grade 1, connect_point 1, disconnect_point 0,
//!   deactivate_point 0, max_point 1`. Meaning INFERRED: a premium account gains `connect_point`
//!   on connect (capped at `max_point`), `disconnect_point` on disconnect and
//!   `deactivate_point` when premium ends. `max_grade` has no 1.2 consumer and its relation to
//!   `premium_grades` is UNKNOWN; it is loaded but not applied.
//! * `premium_benefits` (grade 1: 5/0/2000, grade 2: 10/5/5000 = online labor / offline labor /
//!   max labor). The online/offline tick amounts stay in `CharacterSettings.json` (already
//!   equal); a startup mismatch logs a warning. `max_labor` is data-driven here.
//! * `pcbang_buffs`: the only `active='t'` row is 8000001 -> buff 8000011 "VIP service" for
//!   `premium_grade_id 2`; the Korean PC-bang rows (5222, 6442, 6918, 6577) are `active='f'`.
//!   The 1.2 client has no PC-bang membership flag, so the inactive rows stay off and there is
//!   no PC-bang source; schedule-item rows that need one remain ineligible (`pc_bang = false`).
//!   Buff 8000011 has no `buff_modifiers` (it is display-only) and `duration = 0`, so it is
//!   applied on login / grade change and never persisted.
//!
//! The C# never loaded these tables (premium state was the permanent `AccountPayment` default);
//! this is a Rust addition, recorded in `PORTING.md`.

use std::sync::Arc;

use chrono::Utc;
use futures::future::BoxFuture;
use rusqlite::Connection;

use crate::config::DefaultPayment;
use crate::data::sqlite::for_each_row;
use crate::data::GameDataError;
use crate::db::AccountDetails;
use crate::managers::account::AccountState;
use crate::models::buff::{Buff, SkillCaster};
use crate::models::character::Character;
use crate::net::connection::{AccountPayment, ConnectionHandle, PaymentMethodType};
use crate::packets::g2c::{SCAccountInfoPacket, SCUpdatePremiumPointPacket};
use crate::server::GameServer;
use crate::world::{now_ms, PlayerRef, WorldObject};

/// Fallback `TimedRewardsManager.MaxLabor` when `premium_benefits` is empty.
pub const FALLBACK_MAX_LABOR: i16 = 2000;
/// Fallback `TimedRewardsManager.MaxLaborPremium` when `premium_benefits` is empty.
pub const FALLBACK_MAX_LABOR_PREMIUM: i16 = 5000;

/// One `premium_grades` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PremiumGrade {
    pub id: i32,
    pub grade_id: u8,
    pub point: i32,
}

/// One `premium_benefits` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PremiumBenefit {
    pub id: i32,
    pub online_labor: i32,
    pub offline_labor: i32,
    pub max_labor: i16,
    pub icon_id: i32,
    pub grade_id: u8,
}

/// The single `premium_configs` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PremiumConfig {
    pub id: i32,
    pub max_grade: u8,
    pub connect_point: i32,
    pub disconnect_point: i32,
    pub deactivate_point: i32,
    pub max_point: i32,
}

impl Default for PremiumConfig {
    fn default() -> Self {
        // The shipped 1.2 row, so a server / test without the table still gets grade 2 on
        // connect and caps the point at 1.
        Self {
            id: 0,
            max_grade: 1,
            connect_point: 1,
            disconnect_point: 0,
            deactivate_point: 0,
            max_point: 1,
        }
    }
}

/// One `pcbang_buffs` row (the `active` flag is the SQLite `'t'` / `'f'` text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcBangBuff {
    pub id: i32,
    pub buff_id: u32,
    pub active: bool,
    pub kind_id: i32,
    pub premium_grade_id: u8,
}

/// `premium_grades` / `premium_benefits` / `premium_configs` / `pcbang_buffs`
/// (`server.game_data.systems.services.premium`).
#[derive(Debug, Default)]
pub struct PremiumData {
    pub grades: Vec<PremiumGrade>,
    pub benefits: Vec<PremiumBenefit>,
    pub config: PremiumConfig,
    pub pcbang_buffs: Vec<PcBangBuff>,
}

impl PremiumData {
    pub fn load(conn: &Connection) -> Result<Self, GameDataError> {
        let mut data = PremiumData::default();
        for_each_row(conn, "SELECT * FROM premium_grades", [], |r| {
            data.grades.push(PremiumGrade {
                id: r.i32("id")?,
                grade_id: r.u8("grade_id")?,
                point: r.i32("point")?,
            });
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM premium_benefits", [], |r| {
            data.benefits.push(PremiumBenefit {
                id: r.i32("id")?,
                online_labor: r.i32("online_labor")?,
                offline_labor: r.i32("offline_labor")?,
                max_labor: r.i16("max_labor")?,
                icon_id: r.i32("icon_id")?,
                grade_id: r.u8("grade_id")?,
            });
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM premium_configs", [], |r| {
            data.config = PremiumConfig {
                id: r.i32("id")?,
                max_grade: r.u8("max_grade")?,
                connect_point: r.i32("connect_point")?,
                disconnect_point: r.i32("disconnect_point")?,
                deactivate_point: r.i32("deactivate_point")?,
                max_point: r.i32("max_point")?,
            };
            Ok(())
        })?;
        for_each_row(conn, "SELECT * FROM pcbang_buffs", [], |r| {
            data.pcbang_buffs.push(PcBangBuff {
                id: r.i32("id")?,
                buff_id: r.u32("buff_id")?,
                active: r.bool_str("active")?,
                kind_id: r.i32("kind_id")?,
                premium_grade_id: r.u8("premium_grade_id")?,
            });
            Ok(())
        })?;
        tracing::info!(
            "Loaded {} premium grades, {} benefits, {} PC-bang buffs ({} active)",
            data.grades.len(),
            data.benefits.len(),
            data.pcbang_buffs.len(),
            data.pcbang_buffs.iter().filter(|b| b.active).count()
        );
        Ok(data)
    }

    /// `FUN_397a7d10`: the highest `grade_id` whose `point <= point`, starting at 1.
    pub fn grade_for_point(&self, point: i32) -> u8 {
        let mut grade = 1u8;
        for row in &self.grades {
            if row.point <= point && grade < row.grade_id {
                grade = row.grade_id;
            }
        }
        grade
    }

    pub fn benefit_for_grade(&self, grade: u8) -> Option<&PremiumBenefit> {
        self.benefits.iter().find(|b| b.grade_id == grade)
    }

    /// `premium_benefits.max_labor` of the grade; the C# constants when the table is empty.
    pub fn max_labor_for_grade(&self, grade: u8, premium: bool) -> i16 {
        self.benefit_for_grade(grade).map_or_else(
            || {
                if premium {
                    FALLBACK_MAX_LABOR_PREMIUM
                } else {
                    FALLBACK_MAX_LABOR
                }
            },
            |b| b.max_labor,
        )
    }

    /// The `active='t'` buff ids of a grade.
    pub fn active_buff_ids(&self, grade: u8) -> Vec<u32> {
        self.pcbang_buffs
            .iter()
            .filter(|b| b.active && b.premium_grade_id == grade)
            .map(|b| b.buff_id)
            .collect()
    }

    /// Every buff id a grade row can name (used to clear stale buffs on logout / grade change).
    pub fn all_buff_ids(&self) -> Vec<u32> {
        self.pcbang_buffs.iter().map(|b| b.buff_id).collect()
    }
}

/// The account data of this server (`server.game_data.systems.services.premium`).
pub fn data(server: &GameServer) -> &PremiumData {
    &server.game_data.systems.services.premium
}

/// Persistence of the account payment columns; the MySQL implementation unless a test installs
/// an in-memory one (`systems::services::System::premium_store`).
pub trait PremiumStore: Send + Sync {
    fn update_payment<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        payment: AccountPayment,
        premium_point: i32,
    ) -> BoxFuture<'a, crate::Result<()>>;
}

/// The production store (`db::update_account_payment`, an upsert).
pub struct MySqlPremiumStore;

impl PremiumStore for MySqlPremiumStore {
    fn update_payment<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        payment: AccountPayment,
        premium_point: i32,
    ) -> BoxFuture<'a, crate::Result<()>> {
        Box::pin(crate::db::update_account_payment(
            &server.db,
            account_id,
            payment.method as i32,
            payment.location,
            payment.start_time,
            payment.end_time,
            premium_point,
        ))
    }
}

fn store(server: &GameServer) -> Arc<dyn PremiumStore> {
    server
        .systems
        .services
        .premium_store
        .get()
        .unwrap_or_else(|| Arc::new(MySqlPremiumStore))
}

/// `Account:DefaultPayment` as the C# default `AccountPayment`.
pub fn default_payment(default: DefaultPayment) -> AccountPayment {
    match default {
        DefaultPayment::Premium => AccountPayment {
            method: PaymentMethodType::Premium,
            location: 1,
            start_time: 0,
            // Owner decision 2026-11-03: the configured default premium is permanent; the C#
            // placeholder end (2030-01-01) is not inherited. GM grants keep their own end.
            end_time: 0,
        },
        DefaultPayment::Demo => AccountPayment {
            method: PaymentMethodType::Demo,
            location: 1,
            start_time: 0,
            end_time: 0,
        },
        DefaultPayment::None => AccountPayment {
            method: PaymentMethodType::None,
            location: 1,
            start_time: 0,
            end_time: 0,
        },
    }
}

/// The effective payment of an account row: `pay_method = 0` means the configured default,
/// unless a premium purchase wrote an explicit window on the row (the shop keeps `pay_method`
/// at 0 on a default row, so a stored `pay_start` / `pay_end` is a purchased premium window).
pub fn resolve_payment(default: DefaultPayment, details: &AccountDetails) -> AccountPayment {
    if details.pay_method == 0 {
        if details.pay_start != 0 || details.pay_end != 0 {
            return AccountPayment {
                method: PaymentMethodType::Premium,
                location: details.pay_location,
                start_time: details.pay_start,
                end_time: details.pay_end,
            };
        }
        return default_payment(default);
    }
    AccountPayment {
        method: PaymentMethodType::from_wire(details.pay_method).unwrap_or(PaymentMethodType::None),
        location: details.pay_location,
        start_time: details.pay_start,
        end_time: details.pay_end,
    }
}

/// The state of an account that was never loaded (tests / a DB failure): the configured default
/// payment with the connect point already applied.
pub fn default_state(server: &GameServer) -> AccountState {
    let now = Utc::now().timestamp();
    let payment = default_payment(server.config.account.default_payment);
    let cfg = &data(server).config;
    let point = if payment.premium_state(now) {
        cfg.connect_point.clamp(0, cfg.max_point.max(0))
    } else {
        0
    };
    AccountState {
        payment,
        premium_point: point,
    }
}

/// The result of resolving one `accounts` row.
#[derive(Debug, Clone)]
pub struct LoadedState {
    pub state: AccountState,
    /// The point as stored, before clamping / the connect / deactivate transition.
    pub stored_point: i32,
    /// The stored point must be written back (connect grant, clamp or deactivation).
    pub changed: bool,
    /// Premium ended and the row still had points: the stored point before deactivation.
    pub deactivated_from: Option<i32>,
}

/// The effective point shown to the client: a premium account is at least
/// `premium_configs.connect_point`, a non-premium one at `deactivate_point` when it still has
/// points, and everything is capped at `max_point`.
pub fn effective_point(server: &GameServer, state: &AccountState, now: i64) -> i32 {
    let cfg = &data(server).config;
    let max = cfg.max_point.max(0);
    if state.payment.premium_state(now) {
        state.premium_point.max(cfg.connect_point).clamp(0, max)
    } else if state.premium_point > cfg.deactivate_point {
        cfg.deactivate_point.clamp(0, max)
    } else {
        state.premium_point.clamp(0, max)
    }
}

/// The grade of an account state (`premium_grades` lookup of [`effective_point`]).
pub fn grade_of_state(server: &GameServer, state: &AccountState, now: i64) -> u8 {
    data(server).grade_for_point(effective_point(server, state, now))
}

/// The grade of a point.
pub fn grade_for_point(server: &GameServer, point: i32) -> u8 {
    data(server).grade_for_point(point)
}

/// Resolves an `accounts` row into the runtime state and the transition to persist.
pub fn state_from_details(server: &GameServer, details: &AccountDetails, now: i64) -> LoadedState {
    let payment = resolve_payment(server.config.account.default_payment, details);
    let cfg = &data(server).config;
    let max = cfg.max_point.max(0);
    let premium = payment.premium_state(now);
    let deactivated_from =
        (!premium && details.premium_point > cfg.deactivate_point).then_some(details.premium_point);
    let point = if premium {
        details.premium_point.max(cfg.connect_point).clamp(0, max)
    } else if deactivated_from.is_some() {
        cfg.deactivate_point.clamp(0, max)
    } else {
        details.premium_point.clamp(0, max)
    };
    LoadedState {
        state: AccountState {
            payment,
            premium_point: point,
        },
        stored_point: details.premium_point,
        changed: point != details.premium_point,
        deactivated_from,
    }
}

/// `TimedRewardsManager.GetMaxLabor(PremiumState)` for an account (data-driven).
pub fn max_labor(server: &GameServer, account_id: u32) -> i16 {
    let state = crate::systems::services::accounts::state(server, account_id);
    max_labor_for_state(server, &state)
}

/// [`max_labor`] for an already loaded state.
pub fn max_labor_for_state(server: &GameServer, state: &AccountState) -> i16 {
    let now = Utc::now().timestamp();
    let premium = state.payment.premium_state(now);
    let grade = grade_of_state(server, state, now);
    data(server).max_labor_for_grade(grade, premium)
}

/// The PCs / characters online for an account.
fn online_players(server: &GameServer, account_id: u32) -> Vec<PlayerRef> {
    server
        .world
        .all_characters()
        .into_iter()
        .filter(|p| p.data().account_id == account_id)
        .collect()
}

/// The connections that must receive account packets: the registered connection of the account,
/// or (tests / a connection registered without `AccountManager`) the online characters' handles.
fn account_handles(server: &GameServer, account_id: u32) -> Vec<ConnectionHandle> {
    if let Some(connection_id) = server.accounts.connection_of(account_id) {
        if let Some(handle) = server.connections.get(connection_id) {
            return vec![handle];
        }
    }
    online_players(server, account_id)
        .into_iter()
        .map(|p| p.conn.clone())
        .collect()
}

/// Sends `SCAccountInfo` and `SCUpdatePremiumPoint(point, oldGrade, newGrade)` to the account's
/// connection (if it is connected).
pub fn notify_account(
    server: &GameServer,
    account_id: u32,
    state: &AccountState,
    old_grade: u8,
    new_grade: u8,
) {
    let point = effective_point(server, state, Utc::now().timestamp());
    let account_info = SCAccountInfoPacket {
        pay_method: state.payment.method as i32,
        pay_location: state.payment.location,
        pay_start: state.payment.start_time,
        pay_end: state.payment.end_time,
    };
    let premium_point = SCUpdatePremiumPointPacket {
        point,
        old_pg: old_grade,
        pg: new_grade,
    };
    for handle in account_handles(server, account_id) {
        handle.send(account_info.clone());
        handle.send(premium_point.clone());
    }
}

/// Removes every `pcbang_buffs` buff from an online character (logout / grade change).
pub fn remove_grade_buffs(server: &GameServer, player: &PlayerRef) {
    let ids = data(server).all_buff_ids();
    let obj = WorldObject::from(player);
    for id in ids {
        if player.data().unit.buffs.check_buff(id) {
            crate::systems::skills::buffs::remove_buff(server, &obj, id);
        }
    }
}

/// Applies the `active='t'` `pcbang_buffs` rows of `grade` to a character (grade change).
pub fn apply_grade_buffs(server: &GameServer, player: &PlayerRef, grade: u8) {
    let obj = WorldObject::from(player);
    for buff_id in data(server).active_buff_ids(grade) {
        if player.data().unit.buffs.check_buff(buff_id) {
            continue;
        }
        if server.game_data.skills.buff(buff_id).is_none() {
            tracing::error!("Premium grade {grade}: buff template {buff_id} not loaded");
            continue;
        }
        crate::systems::skills::buffs::add_buff_by_id(server, &obj, buff_id, &obj, 0);
    }
    crate::systems::dispatch_buff_events(server, &obj);
}

/// Applies the grade buffs on character selection, before the unit state is written: like the
/// saved-buff load, this adds to the bookkeeping directly (the buff is part of `SCUnitState`
/// instead of a separate `SCBuffCreated`). The account state was loaded by `AccountManager.Add`.
pub fn apply_login_buffs(server: &GameServer, c: &mut Character) {
    let state = crate::systems::services::accounts::state(server, c.account_id);
    let now = Utc::now().timestamp();
    let premium = state.payment.premium_state(now);
    let grade = grade_of_state(server, &state, now);
    c.premium = premium;
    if !premium {
        return;
    }
    let now_ms = now_ms();
    for buff_id in data(server).active_buff_ids(grade) {
        if c.unit.buffs.check_buff(buff_id) {
            continue;
        }
        // The login path mirrors the saved-buff load: the unit-state template (`GameData.skills`)
        // is added directly, so the buff is part of `SCUnitState` instead of a separate packet.
        let Some(template) = server.game_data.skills.buff(buff_id) else {
            tracing::error!("Premium grade {grade}: buff template {buff_id} not loaded");
            continue;
        };
        let caster = SkillCaster::default();
        let buff = Buff::new(template.clone(), caster, Some((c.id, c.unit.level)), now_ms);
        c.unit.buffs.add_buff(buff, 0, 0, now_ms);
    }
}

/// Caches the account's premium flag on every online character of the account (the
/// `SCUnitStatePacket` premium field of later observers and instance reloads).
pub fn set_online_premium(server: &GameServer, account_id: u32, premium: bool) {
    for player in online_players(server, account_id) {
        player.data().premium = premium;
    }
}

/// Removes the old grade buffs and applies the new grade's (grade change / expiry), and updates
/// the cached premium flag.
pub fn sync_grade(server: &GameServer, account_id: u32, new_grade: u8) {
    let state = crate::systems::services::accounts::state(server, account_id);
    let premium = state.payment.premium_state(Utc::now().timestamp());
    for player in online_players(server, account_id) {
        player.data().premium = premium;
        remove_grade_buffs(server, &player);
        apply_grade_buffs(server, &player, new_grade);
    }
}

/// Persists a new payment state (GM grant / removal and the expiry transition) and pushes it to
/// the account's connection and online characters. Caller must not hold an object lock.
pub async fn set_payment(
    server: &GameServer,
    account_id: u32,
    payment: AccountPayment,
    premium_point: i32,
) -> crate::Result<AccountState> {
    let now = Utc::now().timestamp();
    let old = crate::systems::services::accounts::state(server, account_id);
    let old_grade = grade_of_state(server, &old, now);
    let cfg = &data(server).config;
    let premium_point = premium_point.clamp(0, cfg.max_point.max(0));
    let lock = server.accounts.lock_of(account_id);
    let guard = lock.lock().await;
    store(server)
        .update_payment(server, account_id, payment.clone(), premium_point)
        .await?;
    let state = AccountState {
        payment,
        premium_point,
    };
    server.accounts.set_state(account_id, state.clone());
    drop(guard);
    publish_state(server, account_id, &state, old_grade);
    Ok(state)
}

/// Applies an already-persisted payment state (the caller wrote the row under the account lock,
/// set it in the registry and dropped the lock): pushes `SCAccountInfo` /
/// `SCUpdatePremiumPoint` and syncs the grade buffs. `old_grade` is the grade before the change.
/// This is the WP-B grant path shared by `set_payment` (GM) and the premium service shop.
pub fn publish_state(server: &GameServer, account_id: u32, state: &AccountState, old_grade: u8) {
    let now = Utc::now().timestamp();
    let new_grade = grade_of_state(server, state, now);
    notify_account(server, account_id, state, old_grade, new_grade);
    if old_grade != new_grade {
        sync_grade(server, account_id, new_grade);
    } else {
        set_online_premium(server, account_id, state.payment.premium_state(now));
    }
}

/// Grants premium until `end` (unix seconds) with the configured connect point.
pub async fn grant(
    server: &GameServer,
    account_id: u32,
    start: i64,
    end: i64,
) -> crate::Result<AccountState> {
    let point = data(server).config.connect_point;
    set_payment(
        server,
        account_id,
        AccountPayment {
            method: PaymentMethodType::Premium,
            location: 1,
            start_time: start,
            end_time: end,
        },
        point,
    )
    .await
}

/// Ends premium explicitly (GM `/premium off`): method `None`, deactivate point, no end time.
pub async fn remove(server: &GameServer, account_id: u32) -> crate::Result<AccountState> {
    let point = data(server).config.deactivate_point;
    set_payment(
        server,
        account_id,
        AccountPayment {
            method: PaymentMethodType::None,
            location: 1,
            start_time: 0,
            end_time: 0,
        },
        point,
    )
    .await
}

/// Startup check of the loaded data against the labour configuration (the tick amounts stay in
/// configuration; the tables must agree).
pub fn initialize(server: &GameServer) {
    let premium = data(server);
    for benefit in &premium.benefits {
        let is_premium = benefit.grade_id > premium.grade_for_point(0);
        let online = server.config.labor.get_tick_amount(is_premium);
        if benefit.online_labor != online {
            tracing::warn!(
                "premium_benefits grade {}: online_labor {} != Labor.TickAmount (premium: {}) {}",
                benefit.grade_id,
                benefit.online_labor,
                is_premium,
                online
            );
        }
        let offline = server.config.labor_offline.get_tick_amount(is_premium);
        if benefit.offline_labor != offline {
            tracing::warn!(
                "premium_benefits grade {}: offline_labor {} != LaborOffline.TickAmount (premium: {}) {}",
                benefit.grade_id,
                benefit.offline_labor,
                is_premium,
                offline
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn details(pay_method: i32, point: i32) -> AccountDetails {
        AccountDetails {
            pay_method,
            premium_point: point,
            pay_location: 1,
            ..AccountDetails::default()
        }
    }

    #[test]
    fn grade_lookup_takes_the_highest_reached_grade() {
        let data = PremiumData {
            grades: vec![
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
            ],
            ..PremiumData::default()
        };
        assert_eq!(data.grade_for_point(-1), 1);
        assert_eq!(data.grade_for_point(0), 1);
        assert_eq!(data.grade_for_point(1), 2);
        assert_eq!(data.grade_for_point(99), 2);
    }

    #[test]
    fn empty_benefits_fall_back_to_the_c_sharp_constants() {
        let data = PremiumData::default();
        assert_eq!(data.max_labor_for_grade(1, false), FALLBACK_MAX_LABOR);
        assert_eq!(
            data.max_labor_for_grade(1, true),
            FALLBACK_MAX_LABOR_PREMIUM
        );
    }

    #[test]
    fn default_payment_premium_and_none() {
        let premium = default_payment(DefaultPayment::Premium);
        assert_eq!(premium.method, PaymentMethodType::Premium);
        assert_eq!(premium.end_time, 0, "the default premium has no end date");
        assert!(premium.premium_state(1_800_000_000));
        assert!(premium.premium_state(4_000_000_000), "permanent");
        let none = default_payment(DefaultPayment::None);
        assert_eq!(none.method, PaymentMethodType::None);
        assert!(!none.premium_state(1_800_000_000));
    }

    #[test]
    fn row_without_payment_uses_the_configured_default() {
        let resolved = resolve_payment(DefaultPayment::None, &details(0, 0));
        assert_eq!(resolved.method, PaymentMethodType::None);
        let resolved = resolve_payment(
            DefaultPayment::Premium,
            &AccountDetails {
                pay_method: 3,
                pay_location: 2,
                pay_start: 10,
                pay_end: 20,
                ..AccountDetails::default()
            },
        );
        assert_eq!(resolved.method, PaymentMethodType::Demo);
        assert_eq!(resolved.location, 2);
        assert_eq!((resolved.start_time, resolved.end_time), (10, 20));
    }

    #[test]
    fn default_row_with_a_purchased_window_resolves_to_premium() {
        let premium = resolve_payment(
            DefaultPayment::None,
            &AccountDetails {
                pay_method: 0,
                pay_location: 1,
                pay_start: 100,
                pay_end: 200,
                ..AccountDetails::default()
            },
        );
        assert_eq!(premium.method, PaymentMethodType::Premium);
        assert_eq!((premium.start_time, premium.end_time), (100, 200));
        // No stored window: the configured default still applies.
        let none = resolve_payment(DefaultPayment::None, &details(0, 0));
        assert_eq!(none.method, PaymentMethodType::None);
    }
}
