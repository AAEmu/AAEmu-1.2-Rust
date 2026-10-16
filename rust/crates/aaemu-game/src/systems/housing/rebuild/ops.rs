//! Rebuild validation, the option listing shown to the player and the in-place template swap.

use crate::server::GameServer;
use crate::systems::social::achievement::char_record_kind;
use crate::systems::social::achievement_hooks::{AchievementInput, RecordMatch};
use crate::world::{HouseRef, PlayerRef};

use super::super::data::{HousingData, HousingTemplate};
use super::super::dominion::fort;
use super::super::manager::{self, now, set_permission, tax_due_date, template_of, world_of};
use super::super::ops::my_house_packet;
use super::{PREVIEW_MAX_DISTANCE, RESOLVE_DISTANCE};

/// Why a rebuild (or one option of it) is not possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildError {
    /// No such house.
    NoHouse,
    /// The player does not own the house.
    NotOwner,
    /// The house is not finished (still under construction).
    NotFinished,
    /// The house is for sale.
    ForSale,
    /// The weekly tax is due or overdue.
    TaxDue,
    /// The house stands in a territory with a siege in progress.
    InSiege,
    /// Forts, lodestones and other templates that cannot be rebuilt.
    NotRebuildable,
    /// The rebuilding is not offered for this house.
    UnknownRebuilding,
    /// The target template does not exist.
    NoTarget,
    /// The player is too far from the house.
    TooFar,
    /// Not enough labor power.
    Labor,
    /// Not enough materials.
    Materials,
}

impl RebuildError {
    /// The machine-readable code sent in `error`.
    pub fn code(self) -> &'static str {
        match self {
            Self::NoHouse => "no_house",
            Self::NotOwner => "not_owner",
            Self::NotFinished => "not_finished",
            Self::ForSale => "for_sale",
            Self::TaxDue => "tax_due",
            Self::InSiege => "in_siege",
            Self::NotRebuildable => "not_rebuildable",
            Self::UnknownRebuilding => "unknown_rebuilding",
            Self::NoTarget => "no_target",
            Self::TooFar => "too_far",
            Self::Labor => "labor",
            Self::Materials => "materials",
        }
    }

    /// A short English message (also the per-option `reason_message` shown by the client UI).
    pub fn message(self) -> &'static str {
        match self {
            Self::NoHouse => "No house found.",
            Self::NotOwner => "Only the owner can rebuild this house.",
            Self::NotFinished => "The house must be finished first.",
            Self::ForSale => "A house that is for sale cannot be rebuilt.",
            Self::TaxDue => "Pay the house tax first.",
            Self::InSiege => "The house cannot be rebuilt during a siege.",
            Self::NotRebuildable => "This house cannot be rebuilt.",
            Self::UnknownRebuilding => "This rebuilding is not available for this house.",
            Self::NoTarget => "The target building is not available.",
            Self::TooFar => "You are too far from the house.",
            Self::Labor => "Not enough labor power.",
            Self::Materials => "Not enough materials.",
        }
    }
}

/// One material of an option with the owner's current count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialView {
    pub item: u32,
    /// The item's name from the server's item data (empty when unknown).
    pub name: String,
    pub count: i32,
    pub have: i32,
}

/// One rebuilding offered for a house.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionView {
    pub rebuilding: u32,
    pub target: u32,
    pub name: String,
    pub skill: u32,
    /// The localized skill name (empty when the data has none: the client does not know the
    /// skill, so the UI shows nothing instead of a bare id).
    pub skill_name: String,
    pub labor: i32,
    pub materials: Vec<MaterialView>,
    /// The target template's weekly tax.
    pub tax: i32,
    pub can: bool,
    pub reason: Option<RebuildError>,
}

/// A house with its rebuild options for one player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HouseView {
    /// `housings.id`.
    pub house_id: u32,
    pub template: u32,
    pub name: String,
    /// The current weekly tax.
    pub tax: i32,
    /// A house-level reason that blocks every option.
    pub blocked: Option<RebuildError>,
    pub options: Vec<OptionView>,
}

/// Horizontal distance between the player and the house (`None` = different instance).
fn distance(player: &PlayerRef, house: &HouseRef) -> Option<f32> {
    let p = player.data().unit.transform;
    let h = house.data().unit.transform;
    (p.instance_id == h.instance_id).then(|| ((p.x - h.x).powi(2) + (p.y - h.y).powi(2)).sqrt())
}

/// The house of `player` nearest to them within `max` metres (the 1.2 client UI cannot read a
/// house id, so the extension asks the server to find "the" house).
pub fn nearest_owned_house(server: &GameServer, player: &PlayerRef, max: f32) -> Option<HouseRef> {
    server
        .systems
        .housing
        .manager
        .by_character(player.character_id)
        .into_iter()
        .filter_map(|h| distance(player, &h).filter(|d| *d <= max).map(|d| (d, h)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, h)| h)
}

/// [`nearest_owned_house`] with the default [`RESOLVE_DISTANCE`].
pub fn resolve_house(server: &GameServer, player: &PlayerRef) -> Option<HouseRef> {
    nearest_owned_house(server, player, RESOLVE_DISTANCE)
}

/// Templates that can never be rebuilt (or be a rebuild target): forts, walls, lodestones.
fn fixed_structure(template: &HousingTemplate) -> bool {
    template.guard_tower_setting_id != 0
        || fort::design(template).is_some()
        || fort::castle_category(template.category_id)
}

/// A siege is in progress in the territory the house stands in.
fn in_siege(server: &GameServer, house: &HouseRef) -> bool {
    let zone = house.data().unit.transform.zone_id;
    let Some(group) = server.game_data.zones.zone_by_key(zone).map(|z| z.group_id) else {
        return false;
    };
    server
        .systems
        .housing
        .dominions
        .get(group)
        .and_then(|state| state.siege)
        .is_some_and(|siege| siege.active_at(now()))
}

/// The house-level conditions (the same for every option). Ownership is checked separately.
pub fn blocked_reason(server: &GameServer, house: &HouseRef) -> Option<RebuildError> {
    let Some(template) = template_of(server, house) else {
        return Some(RebuildError::NotRebuildable);
    };
    if fixed_structure(template) {
        return Some(RebuildError::NotRebuildable);
    }
    let (finished, for_sale, tax_due) = {
        let h = house.data();
        let now = now();
        (
            h.current_step == -1,
            h.sell_price > 0,
            h.protection_end_date <= now || tax_due_date(&h) <= now,
        )
    };
    if !finished {
        Some(RebuildError::NotFinished)
    } else if for_sale {
        Some(RebuildError::ForSale)
    } else if tax_due {
        Some(RebuildError::TaxDue)
    } else if in_siege(server, house) {
        Some(RebuildError::InSiege)
    } else {
        None
    }
}

fn is_owner(player: &PlayerRef, house: &HouseRef) -> bool {
    house.data().unit.owner_id == player.character_id
}

/// The rebuilding name shown to the player: the localized rebuilding name, else the target's.
fn option_name(rebuilding_name: &str, target: &HousingTemplate) -> String {
    if rebuilding_name.is_empty() {
        target.name.clone()
    } else {
        rebuilding_name.to_string()
    }
}

/// The name a house gets when its template changes from `old_template` to `target`: the target's
/// localized default name when the house still carries a default (or no) name, else `None`.
pub fn retargeted_name(
    data: &HousingData,
    old_template: u32,
    current: &str,
    target: u32,
) -> Option<String> {
    if !current.is_empty() && current != data.localized_house_name(old_template) {
        return None;
    }
    Some(data.localized_house_name(target)).filter(|n| !n.is_empty())
}

/// The rebuild options of `house` for its owner. `Err` when `player` does not own the house.
pub fn view(
    server: &GameServer,
    player: &PlayerRef,
    house: &HouseRef,
) -> Result<HouseView, RebuildError> {
    if !is_owner(player, house) {
        return Err(RebuildError::NotOwner);
    }
    let data = &server.game_data.systems.housing;
    let hooks = server.systems.housing.hooks.get();
    let blocked = blocked_reason(server, house);
    let labor = player.data().labor_power;
    let (house_id, template_id, name, tax) = {
        let h = house.data();
        (h.id, h.unit.template_id, h.unit.name.clone(), h.tax)
    };
    let options = data
        .rebuild
        .options_for(template_id)
        .into_iter()
        .map(|b| {
            let target = data.template(b.housing_id).filter(|t| !fixed_structure(t));
            let materials: Vec<MaterialView> = b
                .materials
                .iter()
                .map(|m| MaterialView {
                    item: m.item_id,
                    name: server
                        .game_data
                        .items
                        .template(m.item_id)
                        .map(|t| t.name.clone())
                        .unwrap_or_default(),
                    count: m.count,
                    have: hooks.inventory_item_count(server, player, m.item_id),
                })
                .collect();
            let reason = blocked
                .or_else(|| target.is_none().then_some(RebuildError::NoTarget))
                .or_else(|| (i32::from(labor) < b.labor_power).then_some(RebuildError::Labor))
                .or_else(|| {
                    materials
                        .iter()
                        .any(|m| m.have < m.count)
                        .then_some(RebuildError::Materials)
                });
            OptionView {
                rebuilding: b.id,
                target: b.housing_id,
                name: target.map_or_else(|| b.name.clone(), |t| option_name(&b.name, t)),
                skill: b.skill_id,
                skill_name: data.rebuild.skill_name(b.skill_id).to_string(),
                labor: b.labor_power,
                materials,
                tax: target.map_or(0, |t| t.tax_or_zero() as i32),
                can: reason.is_none(),
                reason,
            }
        })
        .collect();
    Ok(HouseView {
        house_id,
        template: template_id,
        name,
        tax,
        blocked,
        options,
    })
}

/// Player rebuild: `rebuilding_id` must be offered for the house's template. Validates, pays
/// materials and labor, then swaps the template in place. Returns the new template id.
pub fn rebuild(
    server: &GameServer,
    player: &PlayerRef,
    house: &HouseRef,
    rebuilding_id: u32,
) -> Result<u32, RebuildError> {
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !is_owner(player, house) {
        return Err(RebuildError::NotOwner);
    }
    if let Some(reason) = blocked_reason(server, house) {
        return Err(reason);
    }
    let data = &server.game_data.systems.housing;
    let template_id = house.data().unit.template_id;
    let rebuilding = data
        .rebuild
        .option_for(template_id, rebuilding_id)
        .ok_or(RebuildError::UnknownRebuilding)?;
    let target = data
        .template(rebuilding.housing_id)
        .filter(|t| !fixed_structure(t))
        .ok_or(RebuildError::NoTarget)?;
    let reach = PREVIEW_MAX_DISTANCE + template_of(server, house).map_or(0.0, |t| t.garden_radius);
    if distance(player, house).is_none_or(|d| d > reach) {
        return Err(RebuildError::TooFar);
    }
    if player.data().labor_power < rebuilding.labor_power as i16 {
        return Err(RebuildError::Labor);
    }
    let materials: Vec<(u32, i32)> = rebuilding
        .materials
        .iter()
        .map(|m| (m.item_id, m.count))
        .collect();
    if !server
        .systems
        .housing
        .hooks
        .get()
        .pay_house_rebuild(server, player, &materials)
    {
        return Err(RebuildError::Materials);
    }
    if rebuilding.labor_power > 0 {
        crate::systems::skills::progress::change_labor(
            server,
            player,
            -(rebuilding.labor_power as i16),
            0,
        );
    }
    swap_template(server, house, target);
    crate::systems::report_achievement(
        server,
        player,
        AchievementInput::Increment {
            kind_id: char_record_kind::REBUILD_HOUSING,
            amount: 1,
            matcher: RecordMatch::Value1(target.id),
        },
    );
    tracing::info!(
        "{} rebuilt house {} from template {template_id} to {} (rebuilding {rebuilding_id})",
        player.data().unit.name,
        house.data().id,
        target.id
    );
    Ok(target.id)
}

/// GM rebuild: swaps the house to any housing template without ownership, state, material or
/// labor checks (live testing). Forts and lodestones are still refused.
pub fn force_rebuild(
    server: &GameServer,
    house: &HouseRef,
    target_template_id: u32,
) -> Result<(), RebuildError> {
    let _operation = server
        .systems
        .housing
        .operation_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let data = &server.game_data.systems.housing;
    if template_of(server, house).is_none_or(fixed_structure) {
        return Err(RebuildError::NotRebuildable);
    }
    let target = data
        .template(target_template_id)
        .filter(|t| !fixed_structure(t))
        .ok_or(RebuildError::NoTarget)?;
    swap_template(server, house, target);
    tracing::info!(
        "GM rebuilt house {} to template {}",
        house.data().id,
        target.id
    );
    Ok(())
}

/// The swap itself (the caller holds the housing operation lock and has validated/paid):
/// recompute the template-derived fields on the same `House`, replace the bound doodads, set the
/// first build step (or finish a step-less target), re-run the collision/fort/tax hooks and
/// redraw the house for everyone around.
fn swap_template(server: &GameServer, house: &HouseRef, target: &HousingTemplate) {
    let data = &server.game_data.systems.housing;
    let world = world_of(server, house);
    let house_id = house.data().id;
    // Previews of this house (any player) are over: their phantom shows the old target.
    super::preview::end_for_house(server, house_id);
    // A template change has no in-place packet: viewers drop the house (and its doodads, with
    // the old attached list) now and learn the new one from the show below.
    if let Some(w) = &world {
        w.hide(house);
    }
    {
        let mut h = house.data();
        let name = retargeted_name(data, h.unit.template_id, &h.unit.name, target.id);
        h.unit.template_id = target.id;
        // Not -1 and not a step: the CurrentStep setter below must see a change.
        h.current_step = -2;
        h.num_action = 0;
        h.base_action = 0;
        h.all_action = target.all_actions();
        h.build_step_count = target.build_steps.len() as i32;
        h.tax = target.tax_or_zero() as i32;
        h.unit.max_hp = target.hp;
        h.unit.hp = target.hp;
        h.unit.model_id = target.main_model_id;
        if let Some(name) = name {
            h.unit.name = name;
        }
        let permission = h.permission;
        set_permission(&mut h, Some(target), permission);
        h.is_dirty = true;
    }
    // The old template's bindings go away explicitly; the setter below creates the new
    // template's bindings itself when the target needs no construction step.
    manager::remove_bound_doodads(server, house);
    let first_step = if target.build_steps.is_empty() { -1 } else { 0 };
    manager::set_current_step(server, house, first_step, false);
    // `spawn_house`: registers the attached doodads, shows the house to everyone around and
    // re-runs the collision sync and the fort refresh.
    if world.is_some() {
        manager::spawn_house(server, house);
    }
    manager::update_tax_info(server, house);
    notify_owner(server, house);
}

/// `SCMyHouse` to the owner when online.
fn notify_owner(server: &GameServer, house: &HouseRef) {
    let owner = house.data().unit.owner_id;
    if let Some(owner) = server.world.character_by_id(owner) {
        owner.send(my_house_packet(server, house));
    }
}
