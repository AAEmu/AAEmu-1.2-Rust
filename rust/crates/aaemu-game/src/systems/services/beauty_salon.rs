//! The beauty salon: `CSEnterBeautySalonPacket`, `CSExitBeautySalonPacket`,
//! `CSBeautyshopDataPacket` -> `CharacterManager.ApplyBeautySalon` and
//! `SCCharacterGenderAndModelModifiedPacket`.
//!
//! The item work (counting / consuming the Salon Certificate, swapping the hair item in the
//! equipment) goes through `item_ops::CHARACTER_ITEM_OPS` (economy subsystem).

use aaemu_commons::{Marshal, PacketStream};
use futures::future::BoxFuture;

use super::item_ops::{item_task, CHARACTER_ITEM_OPS};
use crate::error::Result;
use crate::models::container::equip_slot;
use crate::models::unit_model::UnitCustomModelParams;
use crate::net::connection::GameConnection;
use crate::net::packet::LEVEL_PLAIN;
use crate::net::registry::ClientPacket;
use crate::opcodes::sc;
use crate::packets::packet_def;
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::skills::packets_special::SCToggleBeautyshopResponsePacket;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{broadcast, PlayerRef, WorldObject};

/// `BuffConstants.InBeautySalon`.
pub const BUFF_IN_BEAUTY_SALON: u32 = 6117;
/// `Item.SalonCertificate`.
pub const SALON_CERTIFICATE: u32 = 30811;
/// `EquipmentItemSlotType.Hair`.
const SLOT_TYPE_HAIR: u32 = 24;

/// A refused salon request. The salon-specific `ErrorMessageType` is UNKNOWN (no 1.2
/// evidence); `FAILED_TO_CHANGE_ITEM_LOOK` is the generic look-change error.
fn refuse(player: &PlayerRef) {
    player.send(SCErrorMsgPacket::new(
        error_message::FAILED_TO_CHANGE_ITEM_LOOK,
        0,
        false,
    ));
}

/// Whether `hair_model` may be worn as hair by this character. The real data decides:
/// an `item_body_parts` row in the hair slot, not `npc_only`, whose `model_id` is the
/// character's own model from the `characters` template. `beautyshop_only` rows are
/// allowed. A character without a template (or with model 0) does not restrict the model.
fn is_valid_hair(server: &GameServer, player: &PlayerRef, hair_model: u32) -> bool {
    let Some(part) = server.game_data.items.body_part(hair_model) else {
        return false;
    };
    if part.slot_type_id != SLOT_TYPE_HAIR || part.npc_only {
        return false;
    }
    let (race, gender) = {
        let c = player.data();
        (c.race, c.gender)
    };
    let model = server
        .game_data
        .characters
        .template(race, gender)
        .map_or(0, |t| t.model_id);
    model == 0 || part.model_id == 0 || part.model_id == model
}

/// `SCCharacterGenderAndModelModifiedPacket(character)`.
#[derive(Debug, Clone, PartialEq)]
pub struct SCCharacterGenderAndModelModifiedPacket {
    pub character_id: u32,
    /// `Equipment.GetItemBySlot(Hair)?.TemplateId ?? 0`.
    pub hair_template_id: u32,
    pub model_params: UnitCustomModelParams,
}
packet_def!(
    SCCharacterGenderAndModelModifiedPacket,
    level = LEVEL_PLAIN,
    opcode = sc::SCCharacterGenderAndModelModifiedPacket
);
impl Marshal for SCCharacterGenderAndModelModifiedPacket {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.character_id)
            .write_u32(self.hair_template_id);
        self.model_params.write(s);
        s.write_u32(0);
    }
}

/// `CharacterManager.ApplyBeautySalon(character, hairModel, modelParams)`.
pub fn apply_beauty_salon(
    server: &GameServer,
    player: &PlayerRef,
    hair_model: u32,
    model_params: UnitCustomModelParams,
) {
    let Some(items) = CHARACTER_ITEM_OPS.get() else {
        tracing::warn!("ApplyBeautySalon: item operations are not available (economy)");
        return;
    };
    // A forged request must not put an arbitrary template (stats included) into the hair
    // slot; only a body part the character's race/gender can wear is accepted.
    if !is_valid_hair(server, player, hair_model) {
        refuse(player);
        return;
    }
    // TODO (C#): Add support for future X-day Salon Certificate items
    if items.items_count(server, player, SALON_CERTIFICATE) <= 0 {
        refuse(player);
        return;
    }
    let old_hair = hair_template(player).unwrap_or(0);
    if old_hair != hair_model && !items.replace_hair(server, player, hair_model) {
        let (name, id) = {
            let c = player.data();
            (c.unit.name.clone(), c.id)
        };
        tracing::error!("Failed to add new hairstyle for player {name} ({id})!");
    }
    player.data().unit.model_params = model_params;
    let packet = {
        let c = player.data();
        SCCharacterGenderAndModelModifiedPacket {
            character_id: c.id,
            hair_template_id: 0,
            model_params: c.unit.model_params.clone(),
        }
    };
    let packet = SCCharacterGenderAndModelModifiedPacket {
        hair_template_id: hair_template(player).unwrap_or(0),
        ..packet
    };
    let me = WorldObject::from(player);
    match server.world.world_of(&me) {
        Some(world) => broadcast(&world, &me, &packet, true),
        None => player.send(packet),
    }
    if items.consume_item(
        server,
        player,
        item_task::EDIT_COSMETIC,
        SALON_CERTIFICATE,
        1,
    ) <= 0
    {
        let (name, id) = {
            let c = player.data();
            (c.unit.name.clone(), c.id)
        };
        tracing::error!("Could not consume salon certificate for player {name} ({id})!");
    } else {
        // CHANGE_LOOK: one completed appearance change (certificate consumed).
        crate::systems::report_achievement(
            server,
            player,
            AchievementInput::Increment {
                kind_id: char_record_kind::CHANGE_LOOK,
                amount: 1,
                matcher: RecordMatch::Any,
            },
        );
    }
    // The client sends the salon leave request after SCCharacterGenderAndModelModifiedPacket.
}

/// The hair item template of the equipment.
fn hair_template(player: &PlayerRef) -> Option<u32> {
    let c = player.data();
    let eq = c.inventory.equipment().clone();
    drop(c);
    let eq = eq.lock().expect("container poisoned");
    eq.get_item_by_slot(equip_slot::HAIR).map(|i| i.template_id)
}

/// `CSEnterBeautySalonPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSEnterBeautySalonPacket;

impl ClientPacket for CSEnterBeautySalonPacket {
    const NAME: &'static str = "CSEnterBeautySalonPacket";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(p) = conn.active_char.clone() else {
                return Ok(());
            };
            p.send(SCToggleBeautyshopResponsePacket { state: 1 });
            let me = WorldObject::from(&p);
            crate::systems::skills::buffs::add_buff_by_id(
                server,
                &me,
                BUFF_IN_BEAUTY_SALON,
                &me,
                0,
            );
            Ok(())
        })
    }
}

/// `CSExitBeautySalonPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CSExitBeautySalonPacket;

impl ClientPacket for CSExitBeautySalonPacket {
    const NAME: &'static str = "CSExitBeautySalonPacket";

    fn read(_s: &mut PacketStream) -> Result<Self> {
        Ok(Self)
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(p) = conn.active_char.clone() else {
                return Ok(());
            };
            p.send(SCToggleBeautyshopResponsePacket { state: 0 });
            crate::systems::skills::buffs::remove_buff(
                server,
                &WorldObject::from(&p),
                BUFF_IN_BEAUTY_SALON,
            );
            Ok(())
        })
    }
}

/// `CSBeautyshopDataPacket`.
#[derive(Debug, Clone, PartialEq)]
pub struct CSBeautyshopDataPacket {
    pub hair: u32,
    pub model: UnitCustomModelParams,
}

impl ClientPacket for CSBeautyshopDataPacket {
    const NAME: &'static str = "CSBeautyshopDataPacket";

    fn read(s: &mut PacketStream) -> Result<Self> {
        let hair = s.read_u32()?;
        let model = s.read()?;
        Ok(Self { hair, model })
    }

    fn handle<'a>(
        self,
        server: &'a GameServer,
        conn: &'a mut GameConnection,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if let Some(p) = conn.active_char.clone() {
                apply_beauty_salon(server, &p, self.hair, self.model);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "beauty_salon_tests.rs"]
mod flow_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_modified_packet_layout() {
        let model = UnitCustomModelParams::default();
        let mut e = PacketStream::new();
        e.write_u32(5).write_u32(77);
        model.write(&mut e);
        e.write_u32(0);
        let p = SCCharacterGenderAndModelModifiedPacket {
            character_id: 5,
            hair_template_id: 77,
            model_params: model,
        };
        assert_eq!(crate::net::packet::encode(&p)[6..].to_vec(), e.into_bytes());
    }
}
