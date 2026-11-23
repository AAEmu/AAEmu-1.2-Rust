use crate::models::container::slot_type;
use crate::models::item_task::{task_type, ItemFx, ItemFxList, ItemTask};
use crate::server::GameServer;
use crate::world::PlayerRef;

use super::super::inventory;
use super::super::inventory::{Inv, ItemTxn};
use super::{lock, task_packet};

// ---------------------------------------------------------------------------------------------
// quests: QuestHooks (item / money / mail part)
// ---------------------------------------------------------------------------------------------

/// The item / money / mail part of the quest `QuestHooks` (installed into
/// `server.systems.quests.economy_hooks`). These run while the quest code holds the character
/// lock: the item operations use an unlocked [`ItemTxn`] on the character's containers, and
/// their effects (item task packets, quest item events) are flushed after the lock is released
/// (`quests::events::after_unlock`).
pub struct EconomyForQuests;

impl EconomyForQuests {
    fn inv_of(c: &crate::models::character::Character) -> Inv {
        Inv {
            character_id: c.id,
            containers: c.inventory.containers.clone(),
        }
    }

    /// Flushes `fx` once the character lock is released.
    fn defer(server: &GameServer, character_id: u32, fx: ItemFxList) {
        if fx.is_empty() {
            return;
        }
        crate::systems::quests::events::after_unlock(server, character_id, move |s| {
            super::fx::flush(s, fx);
        });
    }
}

impl crate::systems::quests::hooks::QuestHooks for EconomyForQuests {
    fn change_money(
        &self,
        server: &GameServer,
        _player: Option<&PlayerRef>,
        c: &mut crate::models::character::Character,
        amount: i32,
    ) {
        // ChangeMoney(SlotType.None, SlotType.Inventory, amount) (DepositMoney)
        c.money += i64::from(amount);
        let money = c.money;
        let mut fx = ItemFxList::default();
        fx.send(
            c.id,
            task_packet(
                task_type::DEPOSIT_MONEY,
                vec![ItemTask::MoneyChange(amount)],
            ),
        );
        // A quest reward deposit is an upward money change like any other: `fx::flush` reports
        // the absolute `MY_GOLD` value after the character lock is released.
        if amount > 0 {
            fx.push(ItemFx::Money { owner: c.id, money });
        }
        Self::defer(server, c.id, fx);
    }

    fn acquire_default_item(
        &self,
        server: &GameServer,
        _player: Option<&PlayerRef>,
        c: &mut crate::models::character::Character,
        task: u8,
        template_id: u32,
        count: i32,
        grade_id: u8,
    ) -> bool {
        if server.game_data.items.template(template_id).is_none() {
            // A reward item without a template (282 rows of the 1.2 `quest_act_supply_items`)
            // is skipped like the C# 10.x loot fix (#1488): nothing can be created for it, and
            // one bad static row must not fail the rest of the reward.
            tracing::debug!("Quest reward: skipping unknown item template {template_id}");
            return true;
        }
        let mut txn = ItemTxn::unlocked(server);
        let inv = Self::inv_of(c);
        let ok = lock(&inv.bag()).acquire_default_item(
            task,
            template_id,
            count,
            i32::from(grade_id),
            0,
            &mut txn.env,
        );
        Self::defer(server, c.id, txn.into_fx());
        ok
    }

    fn try_equip_new_backpack(
        &self,
        server: &GameServer,
        _player: Option<&PlayerRef>,
        c: &mut crate::models::character::Character,
        task: u8,
        template_id: u32,
        count: i32,
        grade_id: u8,
    ) -> bool {
        let mut txn = ItemTxn::unlocked(server);
        let inv = Self::inv_of(c);
        let ok = inventory::try_equip_new_backpack(
            &mut txn,
            &inv,
            task,
            template_id,
            count,
            i32::from(grade_id),
            0,
        );
        Self::defer(server, c.id, txn.into_fx());
        ok
    }

    fn consume_item(
        &self,
        server: &GameServer,
        _player: Option<&PlayerRef>,
        c: &mut crate::models::character::Character,
        task: u8,
        template_id: u32,
        count: i32,
        preferred_item_id: Option<u64>,
    ) -> i32 {
        let mut txn = ItemTxn::unlocked(server);
        let inv = Self::inv_of(c);
        let n = inventory::consume_item(
            &mut txn,
            &inv,
            None,
            task,
            template_id,
            count,
            preferred_item_id,
        );
        Self::defer(server, c.id, txn.into_fx());
        n
    }

    fn send_quest_reward_mails(
        &self,
        server: &GameServer,
        player: Option<&PlayerRef>,
        c: &mut crate::models::character::Character,
        quest_template_id: u32,
        items: &[crate::systems::quests::model::ItemCreationDefinition],
        copper: i32,
    ) -> usize {
        let Some(container) = c
            .inventory
            .containers
            .iter()
            .find(|(kind, _)| *kind == slot_type::MAIL)
            .map(|(_, v)| v.clone())
        else {
            return 1;
        };
        if player.is_none() {
            return 1;
        }
        let defs: Vec<_> = items
            .iter()
            .filter(|i| {
                let known = server.game_data.items.template(i.template_id).is_some();
                if !known {
                    // Same skip as the bag path: a missing reward template must not fail the
                    // whole mail-out. `create_quest_reward_mail_bodies` would reject every
                    // definition on one unknown id.
                    tracing::debug!(
                        "Quest {quest_template_id}: skipping unknown reward item template {}",
                        i.template_id
                    );
                }
                known
            })
            .map(|i| {
                super::mail_kinds::ItemCreationDefinition::new(
                    i.template_id,
                    i.count,
                    i32::from(i.grade_id),
                )
            })
            .collect();
        let mut txn = ItemTxn::unlocked(server);
        let Some(mails) = super::mail_kinds::create_quest_reward_mail_bodies(
            &mut txn,
            c.id,
            &c.unit.name,
            &container,
            quest_template_id,
            &defs,
            copper,
        ) else {
            Self::defer(server, c.id, txn.into_fx());
            return 1;
        };
        let ok = super::mail::store_quest_rewards_locked(&mut txn, c, mails);
        Self::defer(server, c.id, txn.into_fx());
        usize::from(!ok)
    }

    fn is_auto_equip_trade_pack(&self, server: &GameServer, template_id: u32) -> bool {
        inventory::is_auto_equip_trade_pack(&server.game_data.items, template_id)
    }

    fn can_replace_glider_in_backpack_slot(
        &self,
        server: &GameServer,
        c: &crate::models::character::Character,
    ) -> bool {
        inventory::can_replace_glider_in_backpack_slot(&server.game_data.items, &Self::inv_of(c))
    }
}
