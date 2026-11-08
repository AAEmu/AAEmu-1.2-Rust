//! Automatic posting of the declaration-auction Siege Camp lots (WP-SD1c).
//!
//! Evidence: the 1.2 client carries the GM console command `auction_post_siege_items`
//! (`strings.txt`, EVIDENCED) next to the other auction commands, and the six Siege Camps are
//! the only `auction_a_category_id = 11` (`declare_siege`) items. The Western launch-era
//! schedule (Total ArcheAge, 2014-11-20/25, grade C) says the declaration scrolls are posted
//! Wednesday 22:30 and stay up for 48 h, which is exactly the shipped `siege_zones` auction
//! (Wed 22:30) -> declare (Fri 22:30) gap (`round6-siege.md` 1, 2).
//!
//! INFERRED policy (recorded in `PORTING.md`, capture-pending):
//! * one system lot per zone that runs a cycle, for that zone's authored
//!   `siege_zones.declare_item_id`; the shipped rotation therefore posts the two zones of the
//!   week's pair (six distinct camp items exist in the table);
//! * the lot uses the auction's 48 h duration class (`declare_start - auction_start` is 48 h
//!   in the shipped rows, and the auction only has the 6/12/24/48 h classes);
//! * the start bid is the item template's `price` (5,000,000 copper in the shipped data), with
//!   no buyout and a system seller (`client_id = 0`, empty name), as with the GM command;
//! * every other auction rule (bid increment, tie-break, outbid refund, who may bid) is the
//!   normal auction engine's; there is no evidence for a special siege rule;
//! * the won item expires when the declare period ends (`Cycle::warmup_start`) through the
//!   item's own `expiration_time` timer.
//!
//! Posting is idempotent per cycle through the persisted `dominions.auction_posted_cycle`
//! marker, so a restart inside the auction period posts nothing twice.

use chrono::{DateTime, Utc};

use crate::server::GameServer;
use crate::systems::economy::auction::{auction_duration, AuctionManager};

use super::calendar::{self, SiegeCalendar};

/// Every lot ends at the authored declare start, 48 h after the auction start in the shipped
/// rows; the auction engine only knows fixed duration classes.
pub const DECLARE_AUCTION_DURATION: u8 = auction_duration::HOURS_48;

fn utc_at(seconds: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(seconds, 0)
}

/// Map a calendar-clock boundary to the real clock, preserving the offset between the two
/// clocks (`real = boundary + (real_now - calendar_now)`). The auction engine and the item
/// timers always run on the real clock, so a cycle walked by the GM `/dominion clock` must
/// still produce lots dated around the real now; with no test clock the offset is zero.
fn real_at(calendar_boundary: i64, calendar_now: i64, real_now: i64) -> Option<i64> {
    calendar_boundary.checked_add(real_now.checked_sub(calendar_now)?)
}

/// Post the running cycle's camp lots. Runs inside the housing operation lock (called from
/// `runtime::tick_once`); it takes the auction operation lock for the item/lot work.
pub fn tick(server: &GameServer, now: i64) {
    if now < 0 {
        return;
    }
    let settings = server
        .systems
        .housing
        .calendar
        .settings(&server.config.housing);
    if !settings.enabled {
        return;
    }
    let castles = &server.game_data.systems.housing.castles;
    let calendar = SiegeCalendar::from_castles(castles);
    if calendar.is_empty() {
        return;
    }
    let claim_delay = calendar.siege_delay_after_claim_days();
    let real_now = chrono::Utc::now().timestamp();
    let auction = &server.systems.economy.auction;
    let _op = auction.operation_lock();
    for state in server.systems.housing.dominions.all() {
        if state.owner_expedition_id == 0 {
            continue;
        }
        // A GM-scheduled explicit siege owns that territory's timers (as in `runtime::tick`).
        if state.siege.as_ref().is_some_and(|siege| {
            siege.status != super::state::SiegeStatus::Finished
                && real_now < siege.ends_at
                && !siege.materialised_from_declaration
        }) {
            continue;
        }
        let zone = state.zone_group_id;
        let Some(schedule) = castles.siege_zones.get(&zone) else {
            continue;
        };
        let item_template_id = schedule.declare_item_id;
        if item_template_id == 0 {
            continue;
        }
        let siege_not_before = if state.claimed_at > 0 && claim_delay > 0 {
            state
                .claimed_at
                .saturating_add(claim_delay.saturating_mul(86_400))
        } else {
            0
        };
        let Some(view) = calendar.view_for_declaration(
            zone,
            now,
            &settings.timezone,
            state.declared_cycle(),
            siege_not_before,
        ) else {
            continue;
        };
        if view.period != calendar::PERIOD_AUCTION {
            continue;
        }
        let Some(cycle) = view.cycle else {
            continue;
        };
        if state.auction_posted_cycle == cycle.auction_start {
            continue;
        }
        let Some(template) = server.game_data.items.template(item_template_id) else {
            // Skip a bad data row instead of failing the whole cycle.
            tracing::warn!(
                "Dominion {zone}: declare item {item_template_id} has no item template; its camp lot is not posted"
            );
            continue;
        };
        if template.price < 0 {
            tracing::warn!(
                "Dominion {zone}: declare item {item_template_id} has a negative price; its camp lot is not posted"
            );
            continue;
        }
        // All three lot timestamps use the same calendar->real mapping.
        let Some(post_at) = real_at(cycle.auction_start, now, real_now).and_then(utc_at) else {
            continue;
        };
        let Some(end_at) = real_at(cycle.declare_start, now, real_now).and_then(utc_at) else {
            continue;
        };
        let Some(expires_at) = real_at(cycle.warmup_start, now, real_now) else {
            continue;
        };
        match server
            .systems
            .housing
            .dominions
            .claim_auction_post(zone, cycle.auction_start)
        {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                tracing::warn!("Dominion {zone}: could not claim the camp-lot post: {error}");
                continue;
            }
        }
        // Anchor the lot at the cycle's auction start, not this tick's instant: the auction
        // period is the authored window and the lot ends exactly at the declare start. The
        // engine's duration class is still the authored 48 h; the mapped end time is set
        // explicitly so all three values use the same clock mapping.
        let Some(mut lot) = AuctionManager::create_system_lot(
            server,
            item_template_id,
            template.price,
            0,
            DECLARE_AUCTION_DURATION,
            post_at,
        ) else {
            server
                .systems
                .housing
                .dominions
                .release_auction_post(zone, cycle.auction_start);
            tracing::warn!(
                "Dominion {zone}: could not create the camp lot for declare item {item_template_id}"
            );
            continue;
        };
        lot.end_time = end_at;
        // The camp is only usable in the declare period, so the won item expires when the
        // period ends (warmup start) through the client's own item expiration timer.
        let item_id = lot.item_id();
        server
            .items
            .update_item(item_id, |item| item.expiration_time = expires_at);
        tracing::info!(
            "Dominion {zone}: posted the declare-auction camp lot {} for item {item_template_id} (start bid {}, expires {expires_at})",
            lot.id,
            template.price
        );
        auction.add_auction_lot(lot);
    }
}

#[cfg(test)]
#[path = "auction_post_tests.rs"]
mod tests;
