//! Port of `Core/Managers/Stream/UccManager` (user created content: guild crests / emblems) and
//! `Core/Managers/Id/UccIdManager`.
//!
//! Persistence matches the C# default build (`STORE_UCC_AS_FILE` off): every UCC is a row of the
//! `uccs` table, custom image data in the `data` MEDIUMBLOB.
//!
//! The C# uses plain `Dictionary` instances keyed by stream connection id (and `Add`, which
//! throws on duplicates and so closes the offending connection); the same failure modes are
//! returned as errors here. Queue entries are never cleaned up on disconnect, as in C#.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{NaiveDateTime, Utc};
use sqlx::mysql::{MySqlPool, MySqlRow};
use sqlx::Row;

use crate::error::{GameError, Result};
use crate::models::container::slot_type;
use crate::models::item::Item;
use crate::models::item_task::{task_type, ItemFx};
use crate::server::GameServer;
use crate::stream::connection::StreamConnection;
use crate::stream::models::{DotNetDateTime, Ucc, UccPart, UccType, UccUploadHandle};
use crate::stream::s2c::{
    EmblemStreamStatus, TCEmblemStreamDownloadPacket, TCEmblemStreamRecvStatusPacket,
    TCEmblemStreamSendStatusPacket, TCUccComplexCheckValidPacket, TCUccComplexPacket,
};
use crate::stream::world::{ActiveCharacter, ITEM_CREST_INK};
use crate::systems::economy::inventory::{self, Inv, ItemTxn};
use crate::systems::npcs::doodad::data::DoodadData;
use crate::systems::npcs::doodad::templates::FuncTemplate;
use crate::world::PlayerRef;

/// Skill "Craft Crest Stamp" (`DoodadFuncStampMaker` rows 2 / 3 / 4 carry it).
pub const CREST_STAMP_SKILL: u32 = 14_435;

/// The `doodad_func_stamp_makers` row of the crest printer's current func group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrestPrinterCost {
    /// Copper charged for a default pattern (`ConfirmDefaultUcc`); a custom image charges only
    /// [`Self::consume_item_id`].
    pub consume_money: i32,
    /// Item the client charges for a custom image (0 = none).
    pub consume_item_id: u32,
    pub consume_count: i32,
}

/// Resolve the cost of a crest printer from its **current** func group: the group must contain a
/// `StampMaker` func whose row creates Crest Ink (`item_id` 17663; doodads 3038 and 1470). A
/// group without one is not a crest printer, whatever the template id (the client resolves the
/// cost by doodad, section 1.1 of the housing audit). Negative values are clamped to 0.
pub fn crest_printer_cost(data: &DoodadData, func_group_id: u32) -> Option<CrestPrinterCost> {
    for func in data.funcs_for_group(func_group_id) {
        let Some(FuncTemplate::StampMaker(row)) = data.func_template(func.func_id, &func.func_type)
        else {
            continue;
        };
        if row.item_id == ITEM_CREST_INK {
            return Some(CrestPrinterCost {
                consume_money: row.consume_money.max(0),
                consume_item_id: row.consume_item_id,
                consume_count: row.consume_count.max(0),
            });
        }
    }
    None
}

/// Whether the character can pay the printer row now. The client picks the cost by upload kind
/// (`X2Ucc.GetMakeUccConsumeInfo`, `FUN_394b5e80`, x2game.dll.c:981806): a user image
/// (`useUserImage` = `data_size > 0`) needs the row's item + count ("You need 1 Crest Brainstorm
/// to create a custom crest.", `ui_texts` 4206), otherwise the row's `consume_money` ("Crest Ink
/// costs the following amount to craft", `ui_texts` 4207). A custom image is therefore never
/// refused for a short money balance.
pub fn can_pay(player: &PlayerRef, cost: &CrestPrinterCost, custom: bool) -> bool {
    if custom {
        return cost.consume_item_id == 0
            || cost.consume_count <= 0
            || Inv::of(player).check_items(
                slot_type::INVENTORY,
                cost.consume_item_id,
                cost.consume_count,
            );
    }
    player.data().money >= i64::from(cost.consume_money)
}

/// `UccIdManager`: `IdManager("UccIdManager", 0x00000001, 0x00FFFFFF, {{"uccs","id"}})`.
#[derive(Debug)]
pub struct UccIdManager {
    used: HashSet<u32>,
    next_free: u32,
}

impl UccIdManager {
    pub const FIRST_ID: u32 = 0x0000_0001;
    pub const LAST_ID: u32 = 0x00FF_FFFF;

    /// `Initialize` from the ids already present in the table. Ids below `FIRST_ID` are skipped
    /// with a warning, as in C#.
    pub fn new(used_ids: impl IntoIterator<Item = u32>) -> Self {
        let mut used = HashSet::new();
        for id in used_ids {
            if id < Self::FIRST_ID {
                tracing::warn!(
                    "UccIdManager: Object ID {id} in DB is less than {}",
                    Self::FIRST_ID
                );
                continue;
            }
            used.insert(id);
        }
        let mut m = Self {
            used,
            next_free: Self::FIRST_ID,
        };
        m.next_free = m.next_clear(Self::FIRST_ID).unwrap_or(Self::LAST_ID);
        m
    }

    fn next_clear(&self, from: u32) -> Option<u32> {
        (from..Self::LAST_ID).find(|id| !self.used.contains(id))
    }

    /// `GetNextId`: the lowest free id at or after the cursor, wrapping to the start.
    pub fn next_id(&mut self) -> Result<u32> {
        let id = if self.next_free < Self::LAST_ID && !self.used.contains(&self.next_free) {
            self.next_free
        } else {
            self.next_clear(self.next_free)
                .or_else(|| self.next_clear(Self::FIRST_ID))
                .ok_or_else(|| GameError::Protocol("Ran out of valid Id's.".into()))?
        };
        self.used.insert(id);
        self.next_free = self
            .next_clear(id)
            .or_else(|| self.next_clear(Self::FIRST_ID))
            .unwrap_or(Self::LAST_ID);
        Ok(id)
    }
}

impl Default for UccIdManager {
    fn default() -> Self {
        Self::new([])
    }
}

/// One `_uploadQueue` entry: the UCC plus the printer row resolved when the upload started, so
/// `ConfirmDefaultUcc` can re-check and charge the same row.
#[derive(Debug, Clone)]
struct QueuedUcc {
    ucc: Ucc,
    cost: CrestPrinterCost,
}

#[derive(Debug, Default)]
struct UccState {
    /// `_uploadQueue`: stream connection id -> UCC being created.
    upload_queue: HashMap<u32, QueuedUcc>,
    /// `_complexUploadParts`: stream connection id -> image upload in progress.
    complex_upload_parts: HashMap<u32, UccUploadHandle>,
    /// `_uccs`.
    uccs: HashMap<u64, Arc<Ucc>>,
    /// `_downloadQueue`: stream connection id -> UCC id being downloaded.
    download_queue: HashMap<u32, u64>,
}

/// `UccManager`.
#[derive(Debug, Default)]
pub struct UccManager {
    state: Mutex<UccState>,
    ids: Mutex<UccIdManager>,
}

fn duplicate_key(what: &str, key: u32) -> GameError {
    GameError::Protocol(format!(
        "{what}: an item with the same key ({key}) has already been added"
    ))
}

/// `connection.GameConnection.ActiveChar` (a null dereference in C# when missing).
pub fn active_character(server: &GameServer, conn: &StreamConnection) -> Result<ActiveCharacter> {
    let gid = conn.game_connection_id.ok_or_else(|| {
        GameError::Protocol(format!(
            "stream connection {}: GameConnection is null (not joined)",
            conn.id
        ))
    })?;
    server.streams.world().active_character(gid).ok_or_else(|| {
        GameError::Protocol(format!(
            "stream connection {}: game connection {gid} has no ActiveChar",
            conn.id
        ))
    })
}

impl UccManager {
    fn state(&self) -> MutexGuard<'_, UccState> {
        self.state.lock().expect("ucc state lock")
    }

    /// `Load()`: every row of `uccs`, plus `UccIdManager.Initialize` (which reads the same ids).
    pub async fn load(&self, db: &MySqlPool) -> Result<()> {
        let rows = sqlx::query(
            "SELECT CAST(id AS SIGNED) AS id, CAST(uploader_id AS SIGNED) AS uploader_id, \
             CAST(type AS SIGNED) AS type, \
             CAST(pattern1 AS SIGNED) AS pattern1, CAST(pattern2 AS SIGNED) AS pattern2, \
             CAST(color1R AS SIGNED) AS color1R, CAST(color1G AS SIGNED) AS color1G, \
             CAST(color1B AS SIGNED) AS color1B, CAST(color2R AS SIGNED) AS color2R, \
             CAST(color2G AS SIGNED) AS color2G, CAST(color2B AS SIGNED) AS color2B, \
             CAST(color3R AS SIGNED) AS color3R, CAST(color3G AS SIGNED) AS color3G, \
             CAST(color3B AS SIGNED) AS color3B, modified, data FROM uccs",
        )
        .fetch_all(db)
        .await?;

        let mut ids = Vec::with_capacity(rows.len());
        let mut uccs = HashMap::new();
        for row in &rows {
            let id = row.try_get::<i64, _>("id")? as u32;
            ids.push(id);
            if let Some(ucc) = ucc_from_row(row, id)? {
                uccs.insert(u64::from(id), Arc::new(ucc));
            }
        }
        let count = uccs.len();
        {
            let mut st = self.state();
            *st = UccState {
                uccs,
                ..UccState::default()
            };
        }
        *self.ids.lock().expect("ucc id lock") = UccIdManager::new(ids);
        tracing::info!("Loaded {count} UCCs");
        Ok(())
    }

    /// `GetUccFromItem(item)` by the item's `UccId`.
    pub fn get_ucc(&self, ucc_id: u64) -> Option<Arc<Ucc>> {
        self.state().uccs.get(&ucc_id).cloned()
    }

    pub fn len(&self) -> usize {
        self.state().uccs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state().uccs.is_empty()
    }

    /// `_uploadQueue.Count` (tests: a refused start leaves no entry).
    #[cfg(test)]
    pub fn queue_len(&self) -> usize {
        self.state().upload_queue.len()
    }

    /// `_complexUploadParts.Count` (tests: a refused start leaves no entry).
    #[cfg(test)]
    pub fn parts_len(&self) -> usize {
        self.state().complex_upload_parts.len()
    }

    /// Adds an already persisted UCC to the in-memory table (tests / tools).
    pub fn insert(&self, ucc: Ucc) {
        self.state().uccs.insert(u64::from(ucc.id), Arc::new(ucc));
    }

    /// `StartUpload(connection, expectedDataSize, customUcc)`.
    pub fn start_upload(
        &self,
        conn: &mut StreamConnection,
        expected_size: i32,
        mut ucc: Ucc,
        cost: CrestPrinterCost,
    ) -> Result<()> {
        ucc.save_data_in_db = true;
        {
            let mut st = self.state();
            // One *in-progress* upload per stream connection: the parts entry is removed when
            // the upload completes (`upload_part`) or is confirmed/aborted, so a second custom
            // upload later on the same connection is accepted (the C# never removed it and
            // therefore closed the connection on the second one). A refused start leaves no
            // queue entry (the C# inserted into `_uploadQueue` first and left it behind).
            if st.upload_queue.contains_key(&conn.id)
                || st.complex_upload_parts.contains_key(&conn.id)
            {
                return Err(duplicate_key("UccManager.StartUpload", conn.id));
            }
            st.upload_queue.insert(conn.id, QueuedUcc { ucc, cost });
            st.complex_upload_parts
                .insert(conn.id, UccUploadHandle::new(expected_size));
        }
        conn.send(&TCEmblemStreamRecvStatusPacket {
            status: EmblemStreamStatus::Continue,
        });
        Ok(())
    }

    /// `UploadPart(connection, part)`.
    pub async fn upload_part(
        &self,
        server: &GameServer,
        conn: &mut StreamConnection,
        part: UccPart,
    ) -> Result<()> {
        let complete = {
            let mut st = self.state();
            let Some(handle) = st.complex_upload_parts.get_mut(&conn.id) else {
                return Ok(());
            };
            if !handle.add_part(part) {
                // More data than declared (or an endless stream of parts): drop the upload and
                // the connection.
                st.complex_upload_parts.remove(&conn.id);
                st.upload_queue.remove(&conn.id);
                return Err(GameError::Protocol(format!(
                    "UccManager.UploadPart: stream connection {} sent more than its declared upload",
                    conn.id
                )));
            }
            if handle.upload_complete() {
                // FinalizeUpload writes into the UCC that StartUpload queued, then the
                // per-connection upload entry is done. The C# left it in
                // `_complexUploadParts`, which made a second custom upload on the same stream
                // connection throw; clearing it here keeps only a *concurrent* upload refused
                // (`start_upload` still rejects a start while parts are arriving).
                let data = handle.finalize();
                st.complex_upload_parts.remove(&conn.id);
                if let Some(queued) = st.upload_queue.get_mut(&conn.id) {
                    queued.ucc.data = data;
                }
                true
            } else {
                false
            }
        };
        if complete {
            self.confirm_default_ucc(server, conn).await
        } else {
            conn.send(&TCEmblemStreamRecvStatusPacket {
                status: EmblemStreamStatus::Continue,
            });
            Ok(())
        }
    }

    /// Drops the pending upload / download state of a closed stream connection (Rust addition:
    /// the C# left these entries behind for ever).
    pub fn drop_connection(&self, connection_id: u32) {
        let mut st = self.state();
        st.upload_queue.remove(&connection_id);
        st.complex_upload_parts.remove(&connection_id);
        st.download_queue.remove(&connection_id);
    }

    /// Entries left in the download queue (tests).
    #[cfg(test)]
    pub fn download_queue_len(&self) -> usize {
        self.state().download_queue.len()
    }

    /// `AddDefaultUcc(defaultUcc, connection)`.
    pub fn add_default_ucc(
        &self,
        conn: &mut StreamConnection,
        ucc: Ucc,
        cost: CrestPrinterCost,
    ) -> Result<()> {
        {
            let mut st = self.state();
            if st.upload_queue.contains_key(&conn.id) {
                return Err(duplicate_key("UccManager.AddDefaultUcc", conn.id));
            }
            st.upload_queue.insert(conn.id, QueuedUcc { ucc, cost });
        }
        conn.send(&TCEmblemStreamRecvStatusPacket {
            status: EmblemStreamStatus::Start,
        });
        Ok(())
    }

    /// `CheckUccIsValid(connection, id)`: answers `isValid = false` for a *known* UCC and nothing
    /// for an unknown one (as the C# does).
    pub fn check_ucc_is_valid(&self, conn: &mut StreamConnection, id: u64) {
        if self.state().uccs.contains_key(&id) {
            conn.send(&TCUccComplexCheckValidPacket {
                id,
                is_valid: false,
            });
        }
    }

    /// `UccComplex(connection, id)`.
    pub fn ucc_complex(&self, conn: &mut StreamConnection, id: u64) {
        let ucc = self.get_ucc(id);
        if let Some(ucc) = ucc {
            conn.send(&TCUccComplexPacket::new(&ucc));
        }
    }

    /// `RequestUcc(connection, id)`.
    pub fn request_ucc(
        &self,
        server: &GameServer,
        conn: &mut StreamConnection,
        id: u64,
    ) -> Result<()> {
        let character = active_character(server, conn)?;
        tracing::warn!("User {} requesting UCC {id}", character.name);
        let Some(ucc) = self.get_ucc(id) else {
            return Ok(());
        };

        let already = {
            let mut st = self.state();
            if st.download_queue.get(&conn.id) == Some(&id) {
                true
            } else {
                // Update currently downloading UCC Id
                st.download_queue.insert(conn.id, id);
                false
            }
        };
        if already {
            tracing::warn!(
                "User {} is already requesting UCC {id}, sending first block !",
                character.name
            );
            conn.send(&TCEmblemStreamDownloadPacket::new(&ucc, 0)?);
            return Ok(());
        }

        let status = if ucc.is_custom() && !ucc.data.is_empty() {
            EmblemStreamStatus::Start
        } else {
            EmblemStreamStatus::Continue
        };
        conn.send(&TCEmblemStreamSendStatusPacket::new(&ucc, status));
        Ok(())
    }

    /// `RequestUccPart(connection, previousIndex, previousSize)`.
    pub fn request_ucc_part(
        &self,
        server: &GameServer,
        conn: &mut StreamConnection,
        previous_index: i32,
        previous_size: i32,
    ) -> Result<()> {
        let character = active_character(server, conn)?;
        tracing::warn!(
            "User {} validated UCC part {previous_index} ({previous_size} bytes), sending next part",
            character.name
        );
        let ucc_id = self.state().download_queue.get(&conn.id).copied();
        let Some(ucc_id) = ucc_id else {
            tracing::warn!(
                "User {} UCC {previous_index} was not in the request queue",
                character.name
            );
            return Ok(());
        };
        let Some(ucc) = self.get_ucc(ucc_id) else {
            return Ok(());
        };
        if !ucc.is_custom() {
            return Ok(());
        }

        let index = previous_index.wrapping_add(1);
        let start = index.wrapping_mul(TCEmblemStreamDownloadPacket::BUFFER_SIZE);
        let end = start.wrapping_add(TCEmblemStreamDownloadPacket::BUFFER_SIZE);
        let count = ucc.data.len() as i32;
        if start >= count {
            self.state().download_queue.remove(&conn.id);
            tracing::warn!(
                "User {} UCC {}, sending end part TCEmblemStreamSendStatusPacket",
                character.name,
                ucc.id
            );
            conn.send(&TCEmblemStreamSendStatusPacket::new(
                &ucc,
                EmblemStreamStatus::Continue,
            ));
            return Ok(());
        }
        tracing::warn!(
            "User {} UCC {}, sending part {index} ({start} => {end} / {count})",
            character.name,
            ucc.id
        );
        conn.send(&TCEmblemStreamDownloadPacket::new(&ucc, index)?);
        Ok(())
    }

    /// `DownloadStatus(connection, id, status, count)`.
    pub fn download_status(
        &self,
        server: &GameServer,
        conn: &mut StreamConnection,
        id: u64,
        status: u8,
        count: i32,
    ) -> Result<()> {
        tracing::warn!("DownloadStatus Id:{id}, Status: {status}, Count:{count}");
        let Some(ucc) = self.get_ucc(id) else {
            return Ok(());
        };
        if status == 4 && ucc.is_custom() && !ucc.data.is_empty() {
            // status 4 == I'm ready to begin download of the image ?
            let max_parts = ucc
                .data
                .len()
                .div_ceil(TCEmblemStreamDownloadPacket::BUFFER_SIZE as usize)
                as i32;
            let send_part = max_parts.wrapping_sub(count);
            if send_part > 0 {
                self.request_ucc_part(server, conn, send_part, 0)
            } else {
                self.request_ucc(server, conn, id)
            }
        } else if status == 3 {
            // status 3 == I'm done downloading the image ?
            conn.send(&TCEmblemStreamSendStatusPacket::new(
                &ucc,
                EmblemStreamStatus::End,
            ));
            Ok(())
        } else {
            conn.send(&TCEmblemStreamSendStatusPacket::new(
                &ucc,
                EmblemStreamStatus::Start,
            ));
            Ok(())
        }
    }

    /// `ConfirmDefaultUcc(connection)`: assigns an id, registers the UCC, sends
    /// `TCEmblemStreamRecvStatus(End)`, then charges the re-checked cost (a default pattern: the
    /// row's money; a custom image: the row's reagent only, see [`can_pay`]), creates the Crest
    /// Ink with the new `UccId`, saves the row and puts the ink in the bag (`GainItemWithUcc`).
    /// A failed save (C# MySqlException) or a missing Crest Ink template (C# NRE) ends the
    /// operation with an error after the cost was taken.
    pub async fn confirm_default_ucc(
        &self,
        server: &GameServer,
        conn: &mut StreamConnection,
    ) -> Result<()> {
        let (mut ucc, cost) = {
            let mut st = self.state();
            let Some(queued) = st.upload_queue.remove(&conn.id) else {
                return Err(GameError::Protocol(format!(
                    "UccManager.ConfirmDefaultUcc: no queued UCC for stream connection {}",
                    conn.id
                )));
            };
            // The upload is over (saved or aborted): release the per-connection parts entry,
            // so the next custom upload on this stream connection can start.
            st.complex_upload_parts.remove(&conn.id);
            (queued.ucc, queued.cost)
        };

        // connection.GameConnection.ActiveChar (NullReferenceException when missing)
        let player = active_player(server, conn)?;
        // The money / reagent may have changed between the start packet and this one. There is
        // no known failure status in 1.2 (`TCEmblemStreamRecvStatus` only carries
        // Continue/Start/End), so a refusal drops the queued upload with a warning (UNKNOWN,
        // CAP-UCC-1); the queue entry above is already gone.
        let custom = ucc.is_custom();
        if !can_pay(&player, &cost, custom) {
            tracing::warn!(
                "UccManager.ConfirmDefaultUcc: stream connection {} can no longer pay the crest printer (money for a default pattern, reagent for a custom image, changed); upload dropped",
                conn.id
            );
            return Ok(());
        }

        let ucc = {
            let mut st = self.state();
            let id = self.ids.lock().expect("ucc id lock").next_id()?;
            ucc.id = id;
            let ucc = Arc::new(ucc);
            st.uccs.insert(u64::from(id), ucc.clone());
            ucc
        };

        conn.send(&TCEmblemStreamRecvStatusPacket {
            status: EmblemStreamStatus::End,
        });

        let Some(item) = charge_and_create_crest_ink(server, &player, ucc.id, &cost, custom) else {
            return Err(GameError::Protocol(format!(
                "UCC {}: could not create the Crest Ink item (NullReferenceException)",
                ucc.id
            )));
        };
        save(&server.db, &ucc).await?;
        add_crest_ink(server, &player, item);
        Ok(())
    }
}

/// The character of the stream connection's game connection (`GameConnection.ActiveChar`).
pub(crate) fn active_player(server: &GameServer, conn: &StreamConnection) -> Result<PlayerRef> {
    let gid = conn.game_connection_id.ok_or_else(|| {
        GameError::Protocol(format!(
            "stream connection {}: GameConnection is null (not joined)",
            conn.id
        ))
    })?;
    server.world.character_by_connection(gid).ok_or_else(|| {
        GameError::Protocol(format!(
            "stream connection {}: game connection {gid} has no ActiveChar",
            conn.id
        ))
    })
}

/// The part of `ConfirmDefaultUcc` before `Save(ucc)`, in **one** item transaction: for a default
/// pattern the checked money debit (`ChangeMoney(Inventory, -consume_money)` ->
/// `SCItemTaskSuccess(DepositMoney, [MoneyChange(-consume_money)])`, sent now); for a custom
/// image (`consume_item`) the reagent (`consume_item_id` x `consume_count`) and no money, as the
/// client's `GetMakeUccConsumeInfo` shows item cost only; then
/// `(UccItem)ItemManager.Create(Item.CrestInk, 1, 0, true)` with `UccId = ucc_id`. `None` when a
/// check or the item creation fails, before anything is charged.
pub fn charge_and_create_crest_ink(
    server: &GameServer,
    player: &PlayerRef,
    ucc_id: u32,
    cost: &CrestPrinterCost,
    consume_item: bool,
) -> Option<Item> {
    let mut txn = ItemTxn::begin(server);
    let inv = Inv::of(player);
    let take_reagent = consume_item && cost.consume_item_id != 0 && cost.consume_count > 0;
    // Checks first, while the transaction's operation lock is held: a refusal changes nothing.
    if take_reagent
        && !inv.check_items(
            slot_type::INVENTORY,
            cost.consume_item_id,
            cost.consume_count,
        )
    {
        return None;
    }
    if !consume_item && player.data().money < i64::from(cost.consume_money) {
        return None;
    }
    // The debit re-checks the balance under the character lock (`change_money`).
    if !consume_item
        && cost.consume_money > 0
        && !inventory::change_money(
            &mut txn.out(),
            player,
            slot_type::INVENTORY,
            slot_type::NONE,
            cost.consume_money,
            task_type::DEPOSIT_MONEY,
        )
    {
        return None;
    }
    if take_reagent {
        let used = inventory::consume_item(
            &mut txn,
            &inv,
            Some(&[slot_type::INVENTORY]),
            task_type::IMPRINT_UCC,
            cost.consume_item_id,
            cost.consume_count,
            None,
        );
        if used != cost.consume_count {
            // Unreachable after the check under the same operation lock; never charge without
            // the reagent.
            return None;
        }
    }
    let mut item = txn.env.create(ITEM_CREST_INK, 1, 0)?;
    item.set_ucc(u64::from(ucc_id));
    Some(item)
}

/// The part of `ConfirmDefaultUcc` after `Save(ucc)`:
/// `character.Inventory.Bag.AddOrMoveExistingItem(ItemTaskType.GainItemWithUcc, newItem)`.
pub fn add_crest_ink(server: &GameServer, player: &PlayerRef, item: Item) {
    let mut txn = ItemTxn::begin(server);
    let bag = Inv::of(player).bag();
    let data = txn.data();
    let res = bag.lock().expect("bag poisoned").add_item(
        task_type::GAIN_ITEM_WITH_UCC,
        item,
        -1,
        data,
        &mut txn.env.fx,
    );
    if let Err(item) = res {
        // Bag full: the new item stays in the C# `_allItems`.
        txn.env.fx.push(ItemFx::Orphaned(Box::new(item)));
    }
}

fn ucc_from_row(row: &MySqlRow, id: u32) -> Result<Option<Ucc>> {
    let int = |col: &str| -> Result<i64> { Ok(row.try_get::<i64, _>(col)?) };
    let Some(ty) = UccType::from_u8(int("type")? as u8) else {
        return Ok(None);
    };
    let mut ucc = Ucc::new(ty, int("uploader_id")? as u32);
    ucc.id = id;
    ucc.pattern1 = int("pattern1")? as u32;
    ucc.pattern2 = int("pattern2")? as u32;
    ucc.color1 = [
        int("color1R")? as u32,
        int("color1G")? as u32,
        int("color1B")? as u32,
    ];
    ucc.color2 = [
        int("color2R")? as u32,
        int("color2G")? as u32,
        int("color2B")? as u32,
    ];
    ucc.color3 = [
        int("color3R")? as u32,
        int("color3G")? as u32,
        int("color3B")? as u32,
    ];
    ucc.modified = row
        .try_get::<Option<NaiveDateTime>, _>("modified")
        .ok()
        .flatten()
        .map_or(DotNetDateTime::MIN, DotNetDateTime::from_naive);
    if ty == UccType::Complex {
        ucc.save_data_in_db = true;
        let data = row
            .try_get::<Option<Vec<u8>>, _>("data")?
            .unwrap_or_default();
        if data.is_empty() {
            tracing::warn!("CustomUcc has no data for UccId {id}");
        }
        ucc.data = data;
    }
    Ok(Some(ucc))
}

/// `UccManager.Save(ucc)` (`DefaultUcc.Save` / `CustomUcc.Save`): `REPLACE INTO uccs`, with
/// `modified = UtcNow`.
pub async fn save(db: &MySqlPool, ucc: &Ucc) -> Result<()> {
    let with_data = ucc.is_custom() && ucc.save_data_in_db;
    let sql = if with_data {
        "REPLACE INTO `uccs` \
         (`id`,`uploader_id`,`type`,`pattern1`,`pattern2`,`color1R`,`color1G`,`color1B`,`color2R`,`color2G`,`color2B`,`color3R`,`color3G`,`color3B`,`modified`,`data`) \
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    } else {
        "REPLACE INTO `uccs` \
         (`id`,`uploader_id`,`type`,`pattern1`,`pattern2`,`color1R`,`color1G`,`color1B`,`color2R`,`color2G`,`color2B`,`color3R`,`color3G`,`color3B`,`modified`) \
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    };
    let mut q = sqlx::query(sql)
        .bind(ucc.id)
        .bind(ucc.uploader_id)
        .bind(ucc.ty as u8)
        .bind(ucc.pattern1)
        .bind(ucc.pattern2);
    for c in [ucc.color1, ucc.color2, ucc.color3] {
        q = q.bind(c[0]).bind(c[1]).bind(c[2]);
    }
    q = q.bind(Utc::now().naive_utc());
    if with_data {
        q = q.bind(ucc.data.as_slice());
    }
    q.execute(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_manager_allocates_lowest_free_after_cursor() {
        let mut m = UccIdManager::new([0, 1, 2, 4]);
        assert_eq!(m.next_id().unwrap(), 3);
        assert_eq!(m.next_id().unwrap(), 5);
        assert_eq!(m.next_id().unwrap(), 6);
        let mut empty = UccIdManager::default();
        assert_eq!(empty.next_id().unwrap(), 1);
    }
}
