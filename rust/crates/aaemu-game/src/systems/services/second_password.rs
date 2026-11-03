//! 1.2 second password (virtual keyboard) and its account lock.
//!
//! The 1.2 client never sends the second password itself: `SCSecondPassKeyTables` (0x1e4)
//! carries four shuffled permutations of `0-9a-zA-Z` plus a server key id, and the client sends
//! `table[tableIndex][alphabet.index(char)]` for every typed character
//! (`FUN_3907a430` / `FUN_3907a370`). The server stores the tables per connection, requires the
//! key id and table type back, decodes through the inverse table and hashes the plaintext with
//! the shared [`aaemu_commons::password::PasswordService`] on a blocking thread.
//!
//! Packet layouts (1.2 client serializers):
//! * C->S 0x125 `CSRequestSecondPasswordKeyTablesPacket`: `u8 type` (FUN_397bc850)
//! * C->S 0x126 create: `u32 keyId, u8 tableIndex, string pass` (FUN_397b14d0)
//! * C->S 0x127 change: `u32 keyId, u8 oldPassTableIndex, u8 newPassTableIndex, string oldPass,
//!   string newPass` (FUN_397b1560)
//! * C->S 0x128 clear / 0x129 check: `u32 keyId, u8 tableIndex, string pass` (FUN_397b1650 /
//!   FUN_397b16e0)
//! * S->C 0x1e3..0x1ea as implemented below (FUN_397b0060, FUN_397b8640, FUN_397b8710,
//!   FUN_397b00d0, FUN_397b0110, FUN_397b0150, FUN_397b0170, FUN_397b00b0).
//!
//! Evidence status: the alphabet, 4..=31 length bound, substitution direction, packet layouts,
//! table reset after every send (`DAT_39addd68` / `DAT_39addd6c`) and the client's type-4 push
//! (`FUN_3907a500` case 4) are read from the 1.2 client. The `SCSecondPassChanged.result` values
//! 1..=5 come from the compiled `chat_msg_event.alb` constants (`R_SUCCESSED` .. ,
//! `R_PASSWORD_SAME_FAILED`). The unix-seconds unlock time is inferred from the client's
//! server-time comparison and remains **capture-pending** (`GetSecondPasswordUnlockRemainTime`).
//! The lobby enforcement points are INFERRED from the client's "locked" branch.

use std::sync::Arc;

use aaemu_commons::password::{Password, PasswordService, PasswordVerificationResult};
use aaemu_commons::{Marshal, PacketStream};
use futures::future::BoxFuture;
use rand::seq::SliceRandom;
use rand::Rng;
use sqlx::Row;

use crate::data::Feature;
use crate::error::Result;
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::{ClientPacket, PacketRegistry};
use crate::opcodes::{cs, sc};
use crate::packets::packet_def;
use crate::server::GameServer;
use crate::systems::HookSlot;

/// The exact 1.2 alphabet (`FUN_3907a370`).
pub const ALPHABET: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
/// Tables per `SCSecondPassKeyTables`.
pub const TABLE_COUNT: usize = 4;
/// `FUN_3907a430`: `0x1b < len - 4` rejects, i.e. 4..=31.
pub const PASSWORD_MIN_LEN: usize = 4;
pub const PASSWORD_MAX_LEN: usize = 31;

/// `SCSecondPassChanged.result` (`chat_msg_event.alb` constants 1318-1334). The numeric values
/// 1..=5 are read from the compiled Lua constant table; the 1.2 server values are therefore
/// evidenced, not inferred.
pub mod changed_result {
    pub const SUCCESSED: u8 = 1;
    pub const FAILED: u8 = 2;
    pub const OLD_PASSWORD_FAILED: u8 = 3;
    pub const NEW_PASSWORD_FAILED: u8 = 4;
    pub const PASSWORD_SAME_FAILED: u8 = 5;
}

/// `CSRequestSecondPasswordKeyTablesPacket.type` (1 = create, 2 = change, 3 = clear, 4 = check;
/// 1..=3 are sent by the client, 4 is pushed by the server as `SCSecondPassKeyTables`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondPasswordAction {
    Create = 1,
    Change = 2,
    Clear = 3,
    Check = 4,
}

impl SecondPasswordAction {
    pub fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Create),
            2 => Some(Self::Change),
            3 => Some(Self::Clear),
            4 => Some(Self::Check),
            _ => None,
        }
    }
}

/// The four virtual-keyboard permutations and the key id they were sent with.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyTables {
    pub key_id: u32,
    pub tables: [String; TABLE_COUNT],
}

impl std::fmt::Debug for KeyTables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyTables")
            .field("key_id", &self.key_id)
            .field("tables", &"[redacted]")
            .finish()
    }
}

impl KeyTables {
    /// Four freshly shuffled permutations of [`ALPHABET`].
    pub fn generate(rng: &mut impl Rng) -> Self {
        let mut tables = std::array::from_fn(|_| String::new());
        for table in &mut tables {
            let mut chars: Vec<char> = ALPHABET.chars().collect();
            chars.shuffle(rng);
            *table = chars.into_iter().collect();
        }
        Self {
            key_id: rng.gen(),
            tables,
        }
    }

    /// A table must be a permutation of the alphabet; duplicates make the inverse ambiguous.
    pub fn is_valid(&self) -> bool {
        let mut expected: Vec<char> = ALPHABET.chars().collect();
        expected.sort_unstable();
        self.tables.iter().all(|table| {
            let mut chars: Vec<char> = table.chars().collect();
            chars.sort_unstable();
            chars == expected
        })
    }

    /// Inverts the client substitution: `plain = alphabet[table.position(sent char)]`.
    /// `None` for an unknown table index, a non-permutation table, or a character that is not
    /// in the table.
    pub fn decode(&self, table_index: u8, sent: &str) -> Option<String> {
        if !self.is_valid() {
            return None;
        }
        let table: Vec<char> = self.tables.get(usize::from(table_index))?.chars().collect();
        let alphabet: Vec<char> = ALPHABET.chars().collect();
        let mut plain = String::with_capacity(sent.len());
        for c in sent.chars() {
            let position = table.iter().position(|t| *t == c)?;
            plain.push(*alphabet.get(position)?);
        }
        Some(plain)
    }
}

/// `true` when `plain` is 4..=31 characters of [`ALPHABET`] (server-side re-validation).
pub fn valid_password(plain: &str) -> bool {
    let len = plain.chars().count();
    (PASSWORD_MIN_LEN..=PASSWORD_MAX_LEN).contains(&len)
        && plain.chars().all(|c| ALPHABET.contains(c))
}

/// The pending tables of a connection: only the last pushed set is valid and only once.
#[derive(Clone)]
struct PendingKeyTables {
    action: SecondPasswordAction,
    key_tables: KeyTables,
}

impl std::fmt::Debug for PendingKeyTables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingKeyTables")
            .field("action", &self.action)
            .field("key_tables", &self.key_tables)
            .finish()
    }
}

/// Per-connection second password state (`GameConnection::second_password`).
#[derive(Clone, Default)]
pub struct ConnectionState {
    pending: Option<PendingKeyTables>,
    /// The 1.2 `passed` flag: per connection, reset on reconnect.
    pub passed: bool,
    /// Cached `accounts.second_password_hash IS NOT NULL` (loaded on `CSListCharacter` /
    /// `CSSetupSecondPassword` or on the first gated action).
    pub has_password: bool,
    /// `has_password` / `passed` are known from the database.
    pub loaded: bool,
    /// A store read/write failed: this connection refuses further second-password attempts
    /// instead of running without a working failure counter or lock.
    pub fail_closed: bool,
}

impl std::fmt::Debug for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionState")
            .field("passed", &self.passed)
            .field("has_password", &self.has_password)
            .field("loaded", &self.loaded)
            .field("fail_closed", &self.fail_closed)
            .field("pending", &self.pending)
            .finish()
    }
}

impl std::fmt::Debug for AccountSecondPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSecondPassword")
            .field("hash", &self.hash.as_ref().map(|_| "[redacted]"))
            .field("failed", &self.failed)
            .field("locked_until", &self.locked_until)
            .finish()
    }
}

/// The persistent per-account values (`accounts.second_password_*`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AccountSecondPassword {
    pub hash: Option<String>,
    pub failed: u8,
    /// Unix seconds; 0 = not locked.
    pub locked_until: i64,
}

/// MySQL access for the second password (a trait so flow tests run without MySQL).
pub trait SecondPasswordStore: Send + Sync {
    /// `None` = the database read failed (callers fail closed).
    fn load<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
    ) -> BoxFuture<'a, Option<AccountSecondPassword>>;
    fn set_hash<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        hash: String,
    ) -> BoxFuture<'a, bool>;
    fn set_failed<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        failed: u8,
        locked_until: i64,
    ) -> BoxFuture<'a, bool>;
    fn clear<'a>(&'a self, server: &'a GameServer, account_id: u32) -> BoxFuture<'a, bool>;
}

/// The C#-side implementation: the account row of the game database.
#[derive(Debug, Default)]
pub struct MySqlSecondPasswordStore;

impl SecondPasswordStore for MySqlSecondPasswordStore {
    fn load<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
    ) -> BoxFuture<'a, Option<AccountSecondPassword>> {
        Box::pin(async move {
            let row = sqlx::query(
                "SELECT second_password_hash, CAST(second_password_failed AS SIGNED) AS failed, \
                 CAST(second_password_locked_until AS SIGNED) AS locked_until \
                 FROM accounts WHERE account_id = ?",
            )
            .bind(account_id)
            .fetch_optional(&server.db)
            .await;
            match row {
                Ok(Some(row)) => {
                    let decoded = (|| {
                        let hash = row
                            .try_get::<Option<String>, _>("second_password_hash")?
                            .filter(|hash| !hash.is_empty());
                        let failed = row.try_get::<i64, _>("failed")?;
                        let locked_until = row.try_get::<i64, _>("locked_until")?;
                        Ok::<_, sqlx::Error>(AccountSecondPassword {
                            hash,
                            failed: failed.clamp(0, i64::from(u8::MAX)) as u8,
                            locked_until,
                        })
                    })();
                    match decoded {
                        Ok(row) => Some(row),
                        Err(error) => {
                            tracing::error!(
                                "second password: decode account {account_id}: {error}"
                            );
                            None
                        }
                    }
                }
                // The accounts row is created on login; a missing one has no second password.
                Ok(None) => Some(AccountSecondPassword::default()),
                Err(error) => {
                    tracing::error!("second password: load account {account_id}: {error}");
                    None
                }
            }
        })
    }

    fn set_hash<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        hash: String,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let result = sqlx::query(
                "INSERT INTO accounts (account_id, second_password_hash, second_password_failed, \
                 second_password_locked_until) VALUES (?, ?, 0, 0) \
                 ON DUPLICATE KEY UPDATE second_password_hash = ?, second_password_failed = 0, \
                 second_password_locked_until = 0",
            )
            .bind(account_id)
            .bind(&hash)
            .bind(&hash)
            .execute(&server.db)
            .await;
            match result {
                Ok(_) => true,
                Err(error) => {
                    tracing::error!("second password: store account {account_id}: {error}");
                    false
                }
            }
        })
    }

    fn set_failed<'a>(
        &'a self,
        server: &'a GameServer,
        account_id: u32,
        failed: u8,
        locked_until: i64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let result = sqlx::query(
                "INSERT INTO accounts (account_id, second_password_failed, \
                 second_password_locked_until) VALUES (?, ?, ?) \
                 ON DUPLICATE KEY UPDATE second_password_failed = ?, \
                 second_password_locked_until = ?",
            )
            .bind(account_id)
            .bind(i32::from(failed))
            .bind(locked_until)
            .bind(i32::from(failed))
            .bind(locked_until)
            .execute(&server.db)
            .await;
            match result {
                Ok(_) => true,
                Err(error) => {
                    tracing::error!("second password: update account {account_id}: {error}");
                    false
                }
            }
        })
    }

    fn clear<'a>(&'a self, server: &'a GameServer, account_id: u32) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let result = sqlx::query(
                "UPDATE accounts SET second_password_hash = NULL, second_password_failed = 0, \
                 second_password_locked_until = 0 WHERE account_id = ?",
            )
            .bind(account_id)
            .execute(&server.db)
            .await;
            match result {
                Ok(_) => true,
                Err(error) => {
                    tracing::error!("second password: clear account {account_id}: {error}");
                    false
                }
            }
        })
    }
}

/// Hashing seam: production uses the shared [`PasswordService`] with its 600 000 iterations;
/// tests count the calls and use a cheap iteration count.
pub trait SecondPasswordHasher: Send + Sync {
    /// `None` = hashing failed (never logged with the plaintext).
    fn hash(&self, plaintext: &str) -> Option<String>;
    fn verify(&self, stored: &str, plaintext: &str) -> bool;
}

/// The production hasher: the login server's `PasswordService` (ASP.NET Identity V3).
#[derive(Debug, Clone, Default)]
pub struct SharedPasswordHasher {
    service: PasswordService,
}

impl SecondPasswordHasher for SharedPasswordHasher {
    fn hash(&self, plaintext: &str) -> Option<String> {
        match self
            .service
            .hash_for_storage(&Password::from_plaintext(plaintext))
        {
            Ok(hash) => Some(hash),
            Err(error) => {
                tracing::error!("second password: hashing failed: {error}");
                None
            }
        }
    }

    fn verify(&self, stored: &str, plaintext: &str) -> bool {
        self.service
            .verify_password(stored, &Password::from_plaintext(plaintext))
            != PasswordVerificationResult::Failed
    }
}

/// `SecondPassword` manager (`server.systems.services.second_password`).
#[derive(Debug, Default)]
pub struct SecondPasswordManager {
    /// Account store; MySQL when nothing is installed.
    pub store: HookSlot<dyn SecondPasswordStore>,
    /// Hasher; [`SharedPasswordHasher`] when nothing is installed.
    pub hasher: HookSlot<dyn SecondPasswordHasher>,
}

impl SecondPasswordManager {
    fn store(&self) -> Arc<dyn SecondPasswordStore> {
        self.store
            .get()
            .unwrap_or_else(|| Arc::new(MySqlSecondPasswordStore))
    }

    fn hasher(&self) -> Arc<dyn SecondPasswordHasher> {
        self.hasher
            .get()
            .unwrap_or_else(|| Arc::new(SharedPasswordHasher::default()))
    }
}

fn manager(server: &GameServer) -> &SecondPasswordManager {
    &server.systems.services.second_password
}

fn enabled(server: &GameServer) -> bool {
    server.config.second_password.enabled
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `FeaturesManager.Initialize` extension: the two second-password bits follow the config.
pub fn initialize(server: &GameServer) {
    if enabled(server) {
        server
            .systems
            .services
            .set_feature(server, Feature::Secondpass as u8, true);
        server
            .systems
            .services
            .set_feature(server, Feature::IngameshopSecondpass as u8, true);
    }
}

fn push_key_tables(
    conn: &mut GameConnection,
    action: SecondPasswordAction,
    wire_type: u8,
) -> KeyTables {
    let key_tables = KeyTables::generate(&mut rand::thread_rng());
    conn.second_password.pending = Some(PendingKeyTables {
        action,
        key_tables: key_tables.clone(),
    });
    conn.send(&SCSecondPassKeyTablesPacket {
        key_type: wire_type,
        key_id: key_tables.key_id,
        tables: key_tables.tables.clone(),
    });
    key_tables
}

/// `CSRequestSecondPasswordKeyTablesPacket`: generate and push the four tables.
pub fn handle_key_tables_request(
    server: &GameServer,
    conn: &mut GameConnection,
    wire_type: u8,
) -> Result<()> {
    if !enabled(server) {
        tracing::debug!("second password: key tables requested while the feature is disabled");
        return Ok(());
    }
    if conn.second_password.fail_closed {
        tracing::warn!(
            "second password: session {} refuses key tables after a store failure",
            conn.id
        );
        return Ok(());
    }
    let Some(action) = SecondPasswordAction::from_wire(wire_type) else {
        tracing::warn!("second password: key tables requested with unknown type {wire_type}");
        return Ok(());
    };
    if conn.account_id() == 0 {
        return Ok(());
    }
    push_key_tables(conn, action, wire_type);
    Ok(())
}

fn take_pending(
    conn: &mut GameConnection,
    action: SecondPasswordAction,
    key_id: u32,
) -> Option<KeyTables> {
    // One-shot: any create/change/clear/check consumes the last pushed tables, even on a
    // mismatch, so a replayed key id can never be used twice.
    let pending = conn.second_password.pending.take()?;
    if pending.action != action || pending.key_tables.key_id != key_id {
        tracing::debug!(
            "second password: key id {} does not match the pending tables of session {}",
            key_id,
            conn.id
        );
        return None;
    }
    Some(pending.key_tables)
}

fn apply_account_state(conn: &mut GameConnection, row: &AccountSecondPassword) {
    let state = &mut conn.second_password;
    state.loaded = true;
    state.has_password = row.hash.is_some();
    if !state.has_password {
        // No password: the account is treated as passed.
        state.passed = true;
    }
}

/// Loads the account state and pushes `SCSecondPassSetted` when a password exists.
async fn push_state(server: &GameServer, conn: &mut GameConnection) -> Result<()> {
    if !enabled(server) || conn.account_id() == 0 {
        return Ok(());
    }
    if conn.second_password.fail_closed {
        tracing::warn!(
            "second password: session {} refuses a state push after a store failure",
            conn.id
        );
        return Ok(());
    }
    let Some(row) = manager(server)
        .store()
        .load(server, conn.account_id())
        .await
    else {
        return Ok(());
    };
    apply_account_state(conn, &row);
    if row.hash.is_none() {
        // The client default is created = 0; nothing to push.
        return Ok(());
    }
    let now = now_unix();
    let unlock = if conn.second_password.passed {
        0
    } else if row.locked_until > now {
        row.locked_until
    } else {
        0
    };
    conn.send(&SCSecondPassSettedPacket {
        passed: conn.second_password.passed,
        account_unlock_time: unlock,
        failed_count: row.failed,
    });
    Ok(())
}

/// `CSListCharacter`: push the per-account state after the lobby load.
pub async fn send_state(server: &GameServer, conn: &mut GameConnection) -> Result<()> {
    push_state(server, conn).await
}

/// `CSSetupSecondPassword`: the client asks for the state at world entry.
pub async fn handle_setup(server: &GameServer, conn: &mut GameConnection) -> Result<()> {
    push_state(server, conn).await
}

/// Admin recovery (GM command): clears the hash, the failure counter and the lock under the
/// account lock, so an operator can unlock an account whose password was lost.
pub async fn clear_account(server: &GameServer, account_id: u32) -> bool {
    let account_lock = server.accounts.lock_of(account_id);
    let _lock = account_lock.lock().await;
    manager(server).store().clear(server, account_id).await
}

/// Gate for the lobby actions that must wait until `passed` (or no password exists). On refusal
/// the server pushes `SCSecondPassKeyTables(type = 4)`; the client never requests type 4 itself.
pub async fn allow_lobby_action(server: &GameServer, conn: &mut GameConnection) -> Result<bool> {
    if !enabled(server) || conn.account_id() == 0 {
        return Ok(true);
    }
    if conn.second_password.fail_closed {
        tracing::warn!(
            "second password: session {} refuses a lobby action after a store failure",
            conn.id
        );
        return Ok(false);
    }
    if !conn.second_password.loaded {
        let Some(row) = manager(server)
            .store()
            .load(server, conn.account_id())
            .await
        else {
            tracing::error!(
                "second password: state load failed for account {}; refusing the lobby action",
                conn.account_id()
            );
            return Ok(false);
        };
        apply_account_state(conn, &row);
    }
    if !conn.second_password.has_password || conn.second_password.passed {
        return Ok(true);
    }
    push_key_tables(conn, SecondPasswordAction::Check, 4);
    Ok(false)
}

async fn hash_on_blocking(hasher: Arc<dyn SecondPasswordHasher>, plain: String) -> Option<String> {
    match tokio::task::spawn_blocking(move || hasher.hash(&plain)).await {
        Ok(hash) => hash,
        Err(error) => {
            tracing::error!("second password: hashing task failed: {error}");
            None
        }
    }
}

async fn verify_on_blocking(
    hasher: Arc<dyn SecondPasswordHasher>,
    stored: String,
    plain: String,
) -> bool {
    match tokio::task::spawn_blocking(move || hasher.verify(&stored, &plain)).await {
        Ok(valid) => valid,
        Err(error) => {
            tracing::error!("second password: verification task failed: {error}");
            false
        }
    }
}

#[derive(Clone, Copy)]
enum FailureReply {
    Changed,
    Cleared,
    Checked,
}

fn send_failure(conn: &mut GameConnection, reply: FailureReply, failed_count: u8) {
    match reply {
        FailureReply::Changed => conn.send(&SCSecondPassChangedPacket {
            result: changed_result::OLD_PASSWORD_FAILED,
            failed_count,
        }),
        FailureReply::Cleared => conn.send(&SCSecondPassClearedPacket {
            success: false,
            failed_count,
        }),
        FailureReply::Checked => conn.send(&SCSecondPassCheckedPacket {
            success: false,
            failed_count,
        }),
    }
}

/// Reads the stored failed count for a protocol-error reply. A failed read closes the
/// connection's second-password handling (fail closed), like every other store error.
async fn stored_failed_count(server: &GameServer, conn: &mut GameConnection) -> u8 {
    let account_id = conn.account_id();
    if account_id == 0 {
        return 0;
    }
    let store = manager(server).store();
    let account_lock = server.accounts.lock_of(account_id);
    let _lock = account_lock.lock().await;
    match store.load(server, account_id).await {
        Some(row) => row.failed,
        None => {
            conn.second_password.passed = false;
            conn.second_password.fail_closed = true;
            tracing::error!(
                "second password: account {account_id}: stored failure count unavailable; refusing further attempts on session {}",
                conn.id
            );
            0
        }
    }
}

/// Counts one failed verification and answers under the account lock. Reaching
/// `SecondPassword:MaxFailCount` locks the account for `LockMinutes` and sends
/// `SCSecondPassCheckOverFailed`; otherwise the action's own `failedCount` reply is sent.
async fn record_failure(
    server: &GameServer,
    conn: &mut GameConnection,
    store: Arc<dyn SecondPasswordStore>,
    account_id: u32,
    failed: u8,
    now: i64,
    reply: FailureReply,
) {
    let config = &server.config.second_password;
    let next = failed.saturating_add(1);
    conn.second_password.passed = false;
    let locked = next >= config.max_fail_count;
    let unlock = if locked {
        let lock_secs = i64::from(config.lock_minutes) * 60;
        now.saturating_add(lock_secs)
    } else {
        0
    };
    if !store.set_failed(server, account_id, next, unlock).await {
        // The failure counter is the real control behind the weak 4-character passwords: if
        // it cannot be recorded, this connection must not keep checking (and must not be
        // allowed any lobby action that needs `passed`).
        conn.second_password.fail_closed = true;
        tracing::error!(
            "second password: account {account_id}: failure could not be recorded; refusing further attempts on session {}",
            conn.id
        );
        send_failure(conn, reply, next);
        return;
    }
    if locked {
        tracing::warn!(
            "second password: account {account_id} locked until {unlock} after {next} failed checks"
        );
        conn.send(&SCSecondPassCheckOverFailedPacket {
            account_unlock_time: unlock,
        });
    } else {
        tracing::info!(
            "second password: account {account_id} failed check ({next}/{})",
            config.max_fail_count
        );
        send_failure(conn, reply, next);
    }
}

/// `CSCreateSecondPasswordPacket`: no existing password, hash the decoded plaintext.
pub async fn handle_create(
    server: &GameServer,
    conn: &mut GameConnection,
    packet: &CSCreateSecondPasswordPacket,
) -> Result<()> {
    if !enabled(server) {
        return Ok(());
    }
    if conn.second_password.fail_closed {
        conn.send(&SCSecondPassCreatedPacket { success: false });
        return Ok(());
    }
    let Some(key_tables) = take_pending(conn, SecondPasswordAction::Create, packet.key_id) else {
        conn.send(&SCSecondPassCreatedPacket { success: false });
        return Ok(());
    };
    let plain = match key_tables.decode(packet.table_index, &packet.pass) {
        Some(plain) if valid_password(&plain) => plain,
        _ => {
            conn.send(&SCSecondPassCreatedPacket { success: false });
            return Ok(());
        }
    };
    let account_id = conn.account_id();
    if account_id == 0 {
        conn.send(&SCSecondPassCreatedPacket { success: false });
        return Ok(());
    }
    let store = manager(server).store();
    let account_lock = server.accounts.lock_of(account_id);
    let _lock = account_lock.lock().await;
    let Some(row) = store.load(server, account_id).await else {
        conn.send(&SCSecondPassCreatedPacket { success: false });
        return Ok(());
    };
    if row.hash.is_some() {
        conn.send(&SCSecondPassCreatedPacket { success: false });
        return Ok(());
    }
    let Some(hash) = hash_on_blocking(manager(server).hasher(), plain).await else {
        conn.send(&SCSecondPassCreatedPacket { success: false });
        return Ok(());
    };
    if !store.set_hash(server, account_id, hash).await {
        conn.send(&SCSecondPassCreatedPacket { success: false });
        return Ok(());
    }
    {
        let state = &mut conn.second_password;
        state.loaded = true;
        state.has_password = true;
        state.passed = true;
    }
    conn.send(&SCSecondPassCreatedPacket { success: true });
    conn.send(&SCSecondPassSettedPacket {
        passed: true,
        account_unlock_time: 0,
        failed_count: 0,
    });
    tracing::info!("second password: account {account_id} created a second password");
    Ok(())
}

/// `CSChangeSecondPasswordPacket`: verify the old password, reject a same password, store the new.
pub async fn handle_change(
    server: &GameServer,
    conn: &mut GameConnection,
    packet: &CSChangeSecondPasswordPacket,
) -> Result<()> {
    if !enabled(server) {
        return Ok(());
    }
    if conn.second_password.fail_closed {
        conn.send(&SCSecondPassChangedPacket {
            result: changed_result::FAILED,
            failed_count: 0,
        });
        return Ok(());
    }
    let Some(key_tables) = take_pending(conn, SecondPasswordAction::Change, packet.key_id) else {
        let failed = stored_failed_count(server, conn).await;
        conn.send(&SCSecondPassChangedPacket {
            result: changed_result::FAILED,
            failed_count: failed,
        });
        return Ok(());
    };
    let Some(old_plain) = key_tables.decode(packet.old_pass_table_index, &packet.old_pass) else {
        let failed = stored_failed_count(server, conn).await;
        conn.send(&SCSecondPassChangedPacket {
            result: changed_result::OLD_PASSWORD_FAILED,
            failed_count: failed,
        });
        return Ok(());
    };
    let new_plain = match key_tables.decode(packet.new_pass_table_index, &packet.new_pass) {
        Some(plain) if valid_password(&plain) => plain,
        _ => {
            let failed = stored_failed_count(server, conn).await;
            conn.send(&SCSecondPassChangedPacket {
                result: changed_result::NEW_PASSWORD_FAILED,
                failed_count: failed,
            });
            return Ok(());
        }
    };
    let account_id = conn.account_id();
    if account_id == 0 {
        conn.send(&SCSecondPassChangedPacket {
            result: changed_result::FAILED,
            failed_count: 0,
        });
        return Ok(());
    }
    let store = manager(server).store();
    let hasher = manager(server).hasher();
    let account_lock = server.accounts.lock_of(account_id);
    let _lock = account_lock.lock().await;
    let Some(row) = store.load(server, account_id).await else {
        conn.send(&SCSecondPassChangedPacket {
            result: changed_result::FAILED,
            failed_count: 0,
        });
        return Ok(());
    };
    let now = now_unix();
    if row.locked_until > now {
        conn.send(&SCSecondPassAccountLockedPacket {
            account_unlock_time: row.locked_until,
        });
        return Ok(());
    }
    if row.locked_until != 0 && row.locked_until <= now {
        store.set_failed(server, account_id, 0, 0).await;
        conn.send(&SCSecondPassAccountLockedPacket {
            account_unlock_time: 0,
        });
        return Ok(());
    }
    let Some(stored) = row.hash else {
        conn.send(&SCSecondPassChangedPacket {
            result: changed_result::OLD_PASSWORD_FAILED,
            failed_count: row.failed,
        });
        return Ok(());
    };
    if !verify_on_blocking(hasher.clone(), stored, old_plain.clone()).await {
        record_failure(
            server,
            conn,
            store,
            account_id,
            row.failed,
            now,
            FailureReply::Changed,
        )
        .await;
        return Ok(());
    }
    if new_plain == old_plain {
        if row.failed != 0 {
            store.set_failed(server, account_id, 0, 0).await;
        }
        conn.second_password.passed = true;
        conn.send(&SCSecondPassChangedPacket {
            result: changed_result::PASSWORD_SAME_FAILED,
            failed_count: 0,
        });
        return Ok(());
    }
    let Some(hash) = hash_on_blocking(hasher, new_plain).await else {
        conn.send(&SCSecondPassChangedPacket {
            result: changed_result::FAILED,
            failed_count: row.failed,
        });
        return Ok(());
    };
    if !store.set_hash(server, account_id, hash).await {
        conn.send(&SCSecondPassChangedPacket {
            result: changed_result::FAILED,
            failed_count: row.failed,
        });
        return Ok(());
    }
    {
        let state = &mut conn.second_password;
        state.loaded = true;
        state.has_password = true;
        state.passed = true;
    }
    conn.send(&SCSecondPassChangedPacket {
        result: changed_result::SUCCESSED,
        failed_count: 0,
    });
    tracing::info!("second password: account {account_id} changed its second password");
    Ok(())
}

/// `CSClearSecondPasswordPacket`: verify and remove the stored hash.
pub async fn handle_clear(
    server: &GameServer,
    conn: &mut GameConnection,
    packet: &CSClearSecondPasswordPacket,
) -> Result<()> {
    if !enabled(server) {
        return Ok(());
    }
    if conn.second_password.fail_closed {
        conn.send(&SCSecondPassClearedPacket {
            success: false,
            failed_count: 0,
        });
        return Ok(());
    }
    let Some(key_tables) = take_pending(conn, SecondPasswordAction::Clear, packet.key_id) else {
        let failed = stored_failed_count(server, conn).await;
        conn.send(&SCSecondPassClearedPacket {
            success: false,
            failed_count: failed,
        });
        return Ok(());
    };
    let Some(plain) = key_tables.decode(packet.table_index, &packet.pass) else {
        let failed = stored_failed_count(server, conn).await;
        conn.send(&SCSecondPassClearedPacket {
            success: false,
            failed_count: failed,
        });
        return Ok(());
    };
    let account_id = conn.account_id();
    if account_id == 0 {
        conn.send(&SCSecondPassClearedPacket {
            success: false,
            failed_count: 0,
        });
        return Ok(());
    }
    let store = manager(server).store();
    let hasher = manager(server).hasher();
    let account_lock = server.accounts.lock_of(account_id);
    let _lock = account_lock.lock().await;
    let Some(row) = store.load(server, account_id).await else {
        conn.send(&SCSecondPassClearedPacket {
            success: false,
            failed_count: 0,
        });
        return Ok(());
    };
    let now = now_unix();
    if row.locked_until > now {
        conn.send(&SCSecondPassAccountLockedPacket {
            account_unlock_time: row.locked_until,
        });
        return Ok(());
    }
    if row.locked_until != 0 && row.locked_until <= now {
        store.set_failed(server, account_id, 0, 0).await;
        conn.send(&SCSecondPassAccountLockedPacket {
            account_unlock_time: 0,
        });
        return Ok(());
    }
    let Some(stored) = row.hash else {
        conn.send(&SCSecondPassClearedPacket {
            success: false,
            failed_count: row.failed,
        });
        return Ok(());
    };
    if !verify_on_blocking(hasher, stored, plain).await {
        record_failure(
            server,
            conn,
            store,
            account_id,
            row.failed,
            now,
            FailureReply::Cleared,
        )
        .await;
        return Ok(());
    }
    if !store.clear(server, account_id).await {
        conn.send(&SCSecondPassClearedPacket {
            success: false,
            failed_count: row.failed,
        });
        return Ok(());
    }
    {
        let state = &mut conn.second_password;
        state.loaded = true;
        state.has_password = false;
        state.passed = true;
    }
    conn.send(&SCSecondPassClearedPacket {
        success: true,
        failed_count: 0,
    });
    tracing::info!("second password: account {account_id} cleared its second password");
    Ok(())
}

/// `CSCheckSecondPasswordPacket`: mark the connection passed or count the failure.
pub async fn handle_check(
    server: &GameServer,
    conn: &mut GameConnection,
    packet: &CSCheckSecondPasswordPacket,
) -> Result<()> {
    if !enabled(server) {
        return Ok(());
    }
    if conn.second_password.fail_closed {
        conn.send(&SCSecondPassCheckedPacket {
            success: false,
            failed_count: 0,
        });
        return Ok(());
    }
    let Some(key_tables) = take_pending(conn, SecondPasswordAction::Check, packet.key_id) else {
        let failed = stored_failed_count(server, conn).await;
        conn.send(&SCSecondPassCheckedPacket {
            success: false,
            failed_count: failed,
        });
        return Ok(());
    };
    let Some(plain) = key_tables.decode(packet.table_index, &packet.pass) else {
        let failed = stored_failed_count(server, conn).await;
        conn.send(&SCSecondPassCheckedPacket {
            success: false,
            failed_count: failed,
        });
        return Ok(());
    };
    let account_id = conn.account_id();
    if account_id == 0 {
        conn.send(&SCSecondPassCheckedPacket {
            success: false,
            failed_count: 0,
        });
        return Ok(());
    }
    let store = manager(server).store();
    let hasher = manager(server).hasher();
    let account_lock = server.accounts.lock_of(account_id);
    let _lock = account_lock.lock().await;
    let Some(row) = store.load(server, account_id).await else {
        conn.send(&SCSecondPassCheckedPacket {
            success: false,
            failed_count: 0,
        });
        return Ok(());
    };
    let now = now_unix();
    if row.locked_until > now {
        // No hashing while locked.
        conn.second_password.passed = false;
        conn.send(&SCSecondPassAccountLockedPacket {
            account_unlock_time: row.locked_until,
        });
        return Ok(());
    }
    if row.locked_until != 0 && row.locked_until <= now {
        // The lock expired: reset the counter and let the client retry.
        store.set_failed(server, account_id, 0, 0).await;
        conn.send(&SCSecondPassAccountLockedPacket {
            account_unlock_time: 0,
        });
        return Ok(());
    }
    let Some(stored) = row.hash else {
        conn.second_password.passed = true;
        conn.send(&SCSecondPassCheckedPacket {
            success: true,
            failed_count: 0,
        });
        return Ok(());
    };
    if !verify_on_blocking(hasher, stored, plain).await {
        record_failure(
            server,
            conn,
            store,
            account_id,
            row.failed,
            now,
            FailureReply::Checked,
        )
        .await;
        return Ok(());
    }
    if row.failed != 0 {
        store.set_failed(server, account_id, 0, 0).await;
    }
    conn.second_password.passed = true;
    conn.send(&SCSecondPassCheckedPacket {
        success: true,
        failed_count: 0,
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Packets
// ---------------------------------------------------------------------------------------------

/// `SCSecondPassSettedPacket(bool passed, long accountUnlockTime, byte failedCount)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SCSecondPassSettedPacket {
    pub passed: bool,
    pub account_unlock_time: i64,
    pub failed_count: u8,
}
packet_def!(
    SCSecondPassSettedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSecondPassSettedPacket
);

impl Marshal for SCSecondPassSettedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.passed)
            .write_i64(self.account_unlock_time)
            .write_u8(self.failed_count);
    }
}

/// `SCSecondPassKeyTablesPacket(byte type, uint keyId, string[4] tables)`.
#[derive(Clone, PartialEq, Eq)]
pub struct SCSecondPassKeyTablesPacket {
    pub key_type: u8,
    pub key_id: u32,
    pub tables: [String; TABLE_COUNT],
}
packet_def!(
    SCSecondPassKeyTablesPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSecondPassKeyTablesPacket
);

impl std::fmt::Debug for SCSecondPassKeyTablesPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SCSecondPassKeyTablesPacket")
            .field("key_type", &self.key_type)
            .field("key_id", &self.key_id)
            .field("tables", &"[redacted]")
            .finish()
    }
}

impl Marshal for SCSecondPassKeyTablesPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.key_type).write_u32(self.key_id);
        for table in &self.tables {
            s.write_str(table);
        }
    }
}

/// `SCSecondPassChangedPacket(byte result, byte failedCount)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCSecondPassChangedPacket {
    pub result: u8,
    pub failed_count: u8,
}
packet_def!(
    SCSecondPassChangedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSecondPassChangedPacket
);

impl Marshal for SCSecondPassChangedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u8(self.result).write_u8(self.failed_count);
    }
}

/// `SCSecondPassCheckedPacket(bool success, byte failedCount)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCSecondPassCheckedPacket {
    pub success: bool,
    pub failed_count: u8,
}
packet_def!(
    SCSecondPassCheckedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSecondPassCheckedPacket
);

impl Marshal for SCSecondPassCheckedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.success).write_u8(self.failed_count);
    }
}

/// `SCSecondPassClearedPacket(bool success, byte failedCount)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCSecondPassClearedPacket {
    pub success: bool,
    pub failed_count: u8,
}
packet_def!(
    SCSecondPassClearedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSecondPassClearedPacket
);

impl Marshal for SCSecondPassClearedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.success).write_u8(self.failed_count);
    }
}

/// `SCSecondPassCheckOverFailedPacket(long accountUnlockTime)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCSecondPassCheckOverFailedPacket {
    pub account_unlock_time: i64,
}
packet_def!(
    SCSecondPassCheckOverFailedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSecondPassCheckOverFailedPacket
);

impl Marshal for SCSecondPassCheckOverFailedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i64(self.account_unlock_time);
    }
}

/// `SCSecondPassAccountLockedPacket(long accountUnlockTime)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCSecondPassAccountLockedPacket {
    pub account_unlock_time: i64,
}
packet_def!(
    SCSecondPassAccountLockedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSecondPassAccountLockedPacket
);

impl Marshal for SCSecondPassAccountLockedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_i64(self.account_unlock_time);
    }
}

/// `SCSecondPassCreatedPacket(bool success)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SCSecondPassCreatedPacket {
    pub success: bool,
}
packet_def!(
    SCSecondPassCreatedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCSecondPassCreatedPacket
);

impl Marshal for SCSecondPassCreatedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_bool(self.success);
    }
}

/// `CSRequestSecondPasswordKeyTablesPacket(byte type)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSRequestSecondPasswordKeyTablesPacket {
    pub key_type: u8,
}

impl ClientPacket for CSRequestSecondPasswordKeyTablesPacket {
    const NAME: &'static str = "CSRequestSecondPasswordKeyTablesPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            key_type: s.read_u8()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handle_key_tables_request(server, conn, self.key_type) })
    }
}

/// `CSSetupSecondPassword` (empty body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSSetupSecondPassword;

impl ClientPacket for CSSetupSecondPassword {
    const NAME: &'static str = "CSSetupSecondPassword";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handle_setup(server, conn).await })
    }
}

/// `CSCreateSecondPasswordPacket(uint keyId, byte tableIndex, string pass)`.
#[derive(Clone, PartialEq, Eq)]
pub struct CSCreateSecondPasswordPacket {
    pub key_id: u32,
    pub table_index: u8,
    pub pass: String,
}

impl std::fmt::Debug for CSCreateSecondPasswordPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CSCreateSecondPasswordPacket")
            .field("key_id", &self.key_id)
            .field("table_index", &self.table_index)
            .field("pass", &"[redacted]")
            .finish()
    }
}

impl ClientPacket for CSCreateSecondPasswordPacket {
    const NAME: &'static str = "CSCreateSecondPasswordPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            key_id: s.read_u32()?,
            table_index: s.read_u8()?,
            pass: s.read_string()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handle_create(server, conn, &self).await })
    }
}

/// `CSChangeSecondPasswordPacket(uint keyId, byte oldPassTableIndex, byte newPassTableIndex,
/// string oldPass, string newPass)`.
#[derive(Clone, PartialEq, Eq)]
pub struct CSChangeSecondPasswordPacket {
    pub key_id: u32,
    pub old_pass_table_index: u8,
    pub new_pass_table_index: u8,
    pub old_pass: String,
    pub new_pass: String,
}

impl std::fmt::Debug for CSChangeSecondPasswordPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CSChangeSecondPasswordPacket")
            .field("key_id", &self.key_id)
            .field("old_pass_table_index", &self.old_pass_table_index)
            .field("new_pass_table_index", &self.new_pass_table_index)
            .field("old_pass", &"[redacted]")
            .field("new_pass", &"[redacted]")
            .finish()
    }
}

impl ClientPacket for CSChangeSecondPasswordPacket {
    const NAME: &'static str = "CSChangeSecondPasswordPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            key_id: s.read_u32()?,
            old_pass_table_index: s.read_u8()?,
            new_pass_table_index: s.read_u8()?,
            old_pass: s.read_string()?,
            new_pass: s.read_string()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handle_change(server, conn, &self).await })
    }
}

/// `CSClearSecondPasswordPacket(uint keyId, byte tableIndex, string pass)`.
#[derive(Clone, PartialEq, Eq)]
pub struct CSClearSecondPasswordPacket {
    pub key_id: u32,
    pub table_index: u8,
    pub pass: String,
}

impl std::fmt::Debug for CSClearSecondPasswordPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CSClearSecondPasswordPacket")
            .field("key_id", &self.key_id)
            .field("table_index", &self.table_index)
            .field("pass", &"[redacted]")
            .finish()
    }
}

impl ClientPacket for CSClearSecondPasswordPacket {
    const NAME: &'static str = "CSClearSecondPasswordPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            key_id: s.read_u32()?,
            table_index: s.read_u8()?,
            pass: s.read_string()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handle_clear(server, conn, &self).await })
    }
}

/// `CSCheckSecondPasswordPacket(uint keyId, byte tableIndex, string pass)`.
#[derive(Clone, PartialEq, Eq)]
pub struct CSCheckSecondPasswordPacket {
    pub key_id: u32,
    pub table_index: u8,
    pub pass: String,
}

impl std::fmt::Debug for CSCheckSecondPasswordPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CSCheckSecondPasswordPacket")
            .field("key_id", &self.key_id)
            .field("table_index", &self.table_index)
            .field("pass", &"[redacted]")
            .finish()
    }
}

impl ClientPacket for CSCheckSecondPasswordPacket {
    const NAME: &'static str = "CSCheckSecondPasswordPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        Ok(Self {
            key_id: s.read_u32()?,
            table_index: s.read_u8()?,
            pass: s.read_string()?,
        })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { handle_check(server, conn, &self).await })
    }
}

#[cfg(test)]
#[path = "second_password_tests.rs"]
mod flow_tests;

/// Registers the second password packets.
pub fn register(reg: &mut PacketRegistry) {
    reg.register::<CSRequestSecondPasswordKeyTablesPacket>(
        LEVEL_PLAIN,
        cs::CSRequestSecondPasswordKeyTablesPacket,
    );
    reg.register::<CSSetupSecondPassword>(LEVEL_PLAIN, cs::CSSetupSecondPassword);
    reg.register::<CSCreateSecondPasswordPacket>(LEVEL_PLAIN, cs::CSCreateSecondPasswordPacket);
    reg.register::<CSChangeSecondPasswordPacket>(LEVEL_PLAIN, cs::CSChangeSecondPasswordPacket);
    reg.register::<CSClearSecondPasswordPacket>(LEVEL_PLAIN, cs::CSClearSecondPasswordPacket);
    reg.register::<CSCheckSecondPasswordPacket>(LEVEL_PLAIN, cs::CSCheckSecondPasswordPacket);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::packet::encode;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    #[test]
    fn sc_packet_bytes_are_exact() {
        assert_eq!(
            encode(&SCSecondPassSettedPacket {
                passed: true,
                account_unlock_time: 0x0102_0304_0506_0708,
                failed_count: 3,
            }),
            hex("DD 01 00 00 E3 01  01  08 07 06 05 04 03 02 01  03")
        );
        assert_eq!(
            encode(&SCSecondPassKeyTablesPacket {
                key_type: 2,
                key_id: 0x1122_3344,
                tables: ["0".into(), "1".into(), "2".into(), "3".into()],
            }),
            hex("DD 01 00 00 E4 01  02  44 33 22 11  01 00 30  01 00 31  01 00 32  01 00 33")
        );
        assert_eq!(
            encode(&SCSecondPassChangedPacket {
                result: changed_result::OLD_PASSWORD_FAILED,
                failed_count: 4,
            }),
            hex("DD 01 00 00 E5 01  03  04")
        );
        assert_eq!(
            encode(&SCSecondPassCheckedPacket {
                success: true,
                failed_count: 0,
            }),
            hex("DD 01 00 00 E6 01  01  00")
        );
        assert_eq!(
            encode(&SCSecondPassClearedPacket {
                success: false,
                failed_count: 2,
            }),
            hex("DD 01 00 00 E7 01  00  02")
        );
        assert_eq!(
            encode(&SCSecondPassCheckOverFailedPacket {
                account_unlock_time: 0x0102_0304_0506_0708,
            }),
            hex("DD 01 00 00 E8 01  08 07 06 05 04 03 02 01")
        );
        assert_eq!(
            encode(&SCSecondPassAccountLockedPacket {
                account_unlock_time: 0,
            }),
            hex("DD 01 00 00 E9 01  00 00 00 00 00 00 00 00")
        );
        assert_eq!(
            encode(&SCSecondPassCreatedPacket { success: true }),
            hex("DD 01 00 00 EA 01  01")
        );
    }

    #[test]
    fn cs_packet_bodies_decode() {
        let mut s = PacketStream::from_bytes(hex("01"));
        let request = CSRequestSecondPasswordKeyTablesPacket::read(&mut s).unwrap();
        assert_eq!(request.key_type, 1);

        let mut s = PacketStream::from_bytes(hex("44 33 22 11  02  03 00 61 62 63"));
        let create = CSCreateSecondPasswordPacket::read(&mut s).unwrap();
        assert_eq!(create.key_id, 0x1122_3344);
        assert_eq!(create.table_index, 2);
        assert_eq!(create.pass, "abc");

        let mut s =
            PacketStream::from_bytes(hex("44 33 22 11  01  03  03 00 6F 6C 64  03 00 6E 65 77"));
        let change = CSChangeSecondPasswordPacket::read(&mut s).unwrap();
        assert_eq!(change.old_pass_table_index, 1);
        assert_eq!(change.new_pass_table_index, 3);
        assert_eq!(change.old_pass, "old");
        assert_eq!(change.new_pass, "new");

        let body = hex("44 33 22 11  00  01 00 78");
        let mut s = PacketStream::from_bytes(body.clone());
        let clear = CSClearSecondPasswordPacket::read(&mut s).unwrap();
        assert_eq!(clear.key_id, 0x1122_3344);
        assert_eq!(clear.table_index, 0);
        assert_eq!(clear.pass, "x");
        let mut s = PacketStream::from_bytes(body);
        let check = CSCheckSecondPasswordPacket::read(&mut s).unwrap();
        assert_eq!(check.key_id, 0x1122_3344);
        assert_eq!(check.table_index, 0);
        assert_eq!(check.pass, "x");
    }

    #[test]
    fn key_tables_invert_the_client_substitution() {
        let mut rng = rand::thread_rng();
        let tables = KeyTables::generate(&mut rng);
        assert!(tables.is_valid());
        let plain = "aZ09Bc";
        for index in 0..TABLE_COUNT as u8 {
            let table: Vec<char> = tables.tables[usize::from(index)].chars().collect();
            let sent: String = plain
                .chars()
                .map(|c| {
                    let position = ALPHABET.find(c).expect("alphabet member");
                    table[position]
                })
                .collect();
            assert_eq!(tables.decode(index, &sent).as_deref(), Some(plain));
        }
        assert_eq!(tables.decode(TABLE_COUNT as u8, "abc"), None);
        assert_eq!(tables.decode(0, "!!"), None);
    }

    #[test]
    fn non_permutation_tables_are_rejected() {
        let mut tables = KeyTables::generate(&mut rand::thread_rng());
        tables.tables[0] = "0".repeat(ALPHABET.len());
        assert!(!tables.is_valid());
        let sent = "0000";
        assert_eq!(tables.decode(0, sent), None);
    }
}
