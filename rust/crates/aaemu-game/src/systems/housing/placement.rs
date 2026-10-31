//! Server-side placement checks using the original 1.2 housing polygons and eligibility data.

use std::collections::{HashMap, HashSet};

use super::data::{HousingData, HousingTemplate};
use crate::data::GameData;
use crate::server::GameServer;
use crate::systems::ai::collision::world::{ObjKind, Owner};
use crate::systems::economy::error_message as err;
use crate::systems::npcs::subzone::{self, Area};
use crate::world::{PlayerRef, WorldInstance};

/// `FUN_393312a0` / `FUN_398971c0`: the placement cells are 4 m squares.
const FOOTPRINT_CELL: f32 = 4.0;
/// `FUN_39897170`: 12 or more cells is logged as "house too big" and yields no footprint.
const FOOTPRINT_MAX_CELLS: i32 = 12;
/// `FUN_39335860`: the unit/doodad collision query uses entity mask 0x1f.
const UNIT_OVERLAP_MASK: u32 = 0x1f;

/// `FUN_39897070`: categories that require a matching housing-area group/category.
fn restricted_category(category: u32) -> bool {
    matches!(category, 1 | 7..=13 | 15..=18)
}

/// `FUN_39897170`: `n = int((garden_radius * 2 + 3) * 0.25)`; `n >= 12` has no footprint.
fn footprint_cells(garden_radius: f32) -> i32 {
    if !garden_radius.is_finite() {
        return 0;
    }
    let n = ((garden_radius * 2.0 + 3.0) * 0.25) as i32;
    if n >= FOOTPRINT_MAX_CELLS {
        return 0;
    }
    n.max(0)
}

/// `FUN_39326980` / `FUN_393312a0`: the grid origin is snapped to the world's 4 m grid.
fn footprint_origin(x: f32, y: f32, cells: i32) -> (f32, f32) {
    let half = cells as f32 * FOOTPRINT_CELL * 0.5;
    let snap = |v: f32| ((v - half + FOOTPRINT_CELL * 0.5) * 0.25) as i32 as f32 * FOOTPRINT_CELL;
    (snap(x), snap(y))
}

/// Every housing polygon of a world template, as the client's housing-area manager sees them.
fn housing_areas(data: &GameData, world_template_id: u32) -> Vec<&Area> {
    data.systems
        .npcs
        .subzone
        .housing_zones
        .get(&world_template_id)
        .map(|zones| zones.values().flatten().collect())
        .unwrap_or_default()
}

fn areas_at<'a>(areas: &[&'a Area], x: f32, y: f32) -> Vec<&'a Area> {
    areas
        .iter()
        .copied()
        .filter(|area| subzone::is_inside(&area.points, [x, y, 0.0]))
        .collect()
}

/// `FUN_39331ba0`: all four corners of one 4 m cell must lie in the same housing polygon.
fn cell_inside_one_area(areas: &[&Area], x: f32, y: f32) -> bool {
    let corners = [
        (x, y),
        (x + FOOTPRINT_CELL, y),
        (x, y + FOOTPRINT_CELL),
        (x + FOOTPRINT_CELL, y + FOOTPRINT_CELL),
    ];
    areas.iter().any(|area| {
        corners
            .iter()
            .all(|&(px, py)| subzone::is_inside(&area.points, [px, py, 0.0]))
    })
}

/// The account's houses counted per `(housing_group_id, category_id)` for the authored
/// `max_construct_count` limit.
fn owned_group_counts(
    server: &GameServer,
    data: &HousingData,
    account: u32,
) -> HashMap<(u32, u32), usize> {
    let mut counts = HashMap::new();
    for house in server.systems.housing.manager.by_account(account) {
        let (instance, template_id, pos) = {
            let h = house.data();
            (
                h.unit.transform.instance_id,
                h.unit.template_id,
                h.unit.transform.local(),
            )
        };
        let Some(world) = server.world.world(instance) else {
            continue;
        };
        let Some(template) = data.template(template_id) else {
            continue;
        };
        let areas = areas_at(
            &housing_areas(&server.game_data, world.template.id),
            pos.x,
            pos.y,
        );
        for group in areas
            .iter()
            .filter_map(|area| data.areas.get(&area.id))
            .filter_map(|id| data.groups.get(id))
        {
            *counts.entry((group.id, template.category_id)).or_insert(0) += 1;
        }
    }
    counts
}

/// `FUN_39897280`: the area's group must allow the category. Returns the matching group.
fn validate_groups(
    data: &HousingData,
    areas: &[u32],
    category: u32,
    owned_categories: &[u32],
    owned_counts: &HashMap<(u32, u32), usize>,
) -> Result<u32, i16> {
    let mut error = err::HOUSE_CANNOT_LOACATE_INVALID_CATEGORY_AREA;
    for group in areas
        .iter()
        .filter_map(|id| data.areas.get(id))
        .filter_map(|id| data.groups.get(id))
    {
        if !data
            .group_categories
            .get(&group.id)
            .is_some_and(|categories| categories.contains(&category))
        {
            continue;
        }
        let limit = data
            .max_construct_count
            .get(&(group.id, category))
            .copied()
            .unwrap_or(0);
        if limit > 0 {
            let used = owned_counts
                .get(&(group.id, category))
                .copied()
                .unwrap_or(0);
            if used >= limit as usize {
                error = err::HOUSE_CANNOT_CONSTRUCT_IN_AREA_BY_MAX_CONSTRUCT_COUNT;
                continue;
            }
        }
        if group.houseless && !owned_categories.is_empty() {
            error = err::HOUSE_CANNOT_OWN_MORE_HOUSELESS_CONDITION;
            continue;
        }
        if group.existing_category_id != 0 && owned_categories.contains(&group.existing_category_id)
        {
            error = err::HOUSE_CANNOT_OWN_MORE_EXISTING_CATEGORY_CONDITION;
            continue;
        }
        return Ok(group.id);
    }
    Err(error)
}

fn player_owned(server: &GameServer, player: &PlayerRef, obj_id: u32) -> bool {
    let Some(world) = server
        .world
        .world_of(&crate::world::WorldObject::from(player))
    else {
        return false;
    };
    let character_id = player.character_id;
    world.object(obj_id).is_some_and(|object| match object {
        crate::world::WorldObject::Slave(s) => s.data().unit.owner_id == character_id,
        crate::world::WorldObject::Mate(m) => m.data().unit.owner_id == character_id,
        _ => false,
    })
}

/// `FUN_39335860`: the footprint collides with a unit, doodad, gimmick or ship (not a house).
#[allow(clippy::too_many_arguments)]
fn validate_unit_overlap(
    server: &GameServer,
    player: &PlayerRef,
    world: &WorldInstance,
    x0: f32,
    y0: f32,
    side: f32,
    z: f32,
    ignored: &HashSet<u32>,
) -> Result<(), i16> {
    let half = side * 0.5;
    let center = [x0 + half, y0 + half, z];
    let Some(found) = crate::systems::ai::collision::box_obstacles(
        server,
        world.id,
        center,
        [half, half, half],
        0.0,
        UNIT_OVERLAP_MASK,
    ) else {
        return Ok(());
    };
    for (owner, _) in found {
        let Owner::Object { kind, obj_id } = owner else {
            continue;
        };
        // Houses are the 0x71 rule; the placing character and its own mount are ignored.
        if kind == ObjKind::House || obj_id == player.obj_id {
            continue;
        }
        if player_owned(server, player, obj_id) || ignored.contains(&obj_id) {
            continue;
        }
        return Err(err::HOUSE_CANNOT_LOCATE_OVERLAP_UNIT);
    }
    Ok(())
}

/// `FUN_39331f30` (mode 2/2 in `FUN_393349e0`): the existing grid house is its snapped
/// axis-aligned 4n box and touching edges do not overlap (strict inequalities).
fn snapped_squares_overlap(a: (f32, f32), a_cells: i32, b: (f32, f32), b_cells: i32) -> bool {
    let (bx, by) = footprint_origin(b.0, b.1, b_cells);
    let (a_side, b_side) = (
        a_cells as f32 * FOOTPRINT_CELL,
        b_cells as f32 * FOOTPRINT_CELL,
    );
    a.0 < bx + b_side && a.1 < by + b_side && bx < a.0 + a_side && by < a.1 + a_side
}

/// Validate before reserving a design, money, or tax certificates. Accounts without static
/// placement tables (synthetic flow fixtures) still receive world and overlap checks.
pub fn validate_house(
    server: &GameServer,
    player: &PlayerRef,
    template: &HousingTemplate,
    x: f32,
    y: f32,
    z: f32,
) -> Result<(), i16> {
    if ![x, y, z].into_iter().all(f32::is_finite) {
        return Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA);
    }
    let (instance, account) = {
        let c = player.data();
        (c.unit.transform.instance_id, c.account_id)
    };
    let world = server
        .world
        .world(instance)
        .ok_or(err::HOUSE_CANNOT_LOCATE_INVALID_AREA)?;
    if world.template.zone_id_at(x, y) == 0 {
        return Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA);
    }
    let data = &server.game_data.systems.housing;
    let fort = super::dominion::fort::placement_owner(server, player, template, x, y)?;
    if fort.is_none() {
        super::dominion::rights::placement_allowed(
            server,
            player,
            instance,
            x,
            y,
            template.garden_radius,
        )?;
    }
    let cells = footprint_cells(template.garden_radius);
    let (x0, y0) = if cells > 0 {
        footprint_origin(x, y, cells)
    } else {
        (x, y)
    };
    if fort.is_none() && !data.areas.is_empty() {
        let areas = housing_areas(&server.game_data, world.template.id);
        let centre = areas_at(&areas, x, y);
        let centre_ids = centre.iter().map(|area| area.id).collect::<Vec<_>>();
        let restricted = restricted_category(template.category_id);
        let has_limit = centre_ids
            .iter()
            .filter_map(|id| data.areas.get(id))
            .filter_map(|group_id| data.groups.get(group_id))
            .any(|group| {
                data.group_categories
                    .get(&group.id)
                    .is_some_and(|categories| categories.contains(&template.category_id))
                    && data
                        .max_construct_count
                        .get(&(group.id, template.category_id))
                        .copied()
                        .unwrap_or(0)
                        > 0
            });
        if restricted || has_limit {
            if restricted && centre_ids.is_empty() {
                // `FUN_39333e20`: a centre outside every polygon is the category-area error.
                return Err(err::HOUSE_CANNOT_LOACATE_INVALID_CATEGORY_AREA);
            }
            if !centre_ids.is_empty() {
                let owned_categories = server
                    .systems
                    .housing
                    .manager
                    .by_account(account)
                    .iter()
                    .filter_map(|house| {
                        data.template(house.data().unit.template_id)
                            .map(|t| t.category_id)
                    })
                    .collect::<Vec<_>>();
                let owned_counts = owned_group_counts(server, data, account);
                match validate_groups(
                    data,
                    &centre_ids,
                    template.category_id,
                    &owned_categories,
                    &owned_counts,
                ) {
                    Ok(_) => {}
                    Err(error) => {
                        // Outside the restricted categories the client does not check the
                        // area/category pairing; only an authored construct limit can fire.
                        if restricted
                            || error == err::HOUSE_CANNOT_CONSTRUCT_IN_AREA_BY_MAX_CONSTRUCT_COUNT
                        {
                            return Err(error);
                        }
                    }
                }
            }
        }
        if cells > 0 {
            for ix in 0..cells {
                for iy in 0..cells {
                    let cx = x0 + ix as f32 * FOOTPRINT_CELL;
                    let cy = y0 + iy as f32 * FOOTPRINT_CELL;
                    if !cell_inside_one_area(&areas, cx, cy) {
                        return Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA);
                    }
                }
            }
        }
    }
    if cells > 0 {
        let side = cells as f32 * FOOTPRINT_CELL;
        let mut ignored = HashSet::new();
        for house in server.systems.housing.manager.all() {
            let (house_instance, obj_id, id, position, scale) = {
                let h = house.data();
                (
                    h.unit.transform.instance_id,
                    house.obj_id,
                    h.unit.template_id,
                    h.unit.transform.local(),
                    h.unit.scale,
                )
            };
            if house_instance != instance {
                continue;
            }
            // `FUN_39335860` puts every attached doodad of every house in the collision
            // query's skip list.
            for doodad in server.systems.housing.manager.attached(obj_id) {
                ignored.insert(doodad.obj_id);
            }
            let Some(other) = data.template(id) else {
                continue;
            };
            let other_cells = footprint_cells(other.garden_radius * scale);
            if other_cells == 0 {
                // Mode-1 houses use the ghost model box (`FUN_390318f0` / `FUN_39332c10`);
                // the server has no render-model bounds, so this stays lenient (PORTING.md).
                continue;
            }
            if snapped_squares_overlap((x0, y0), cells, (position.x, position.y), other_cells) {
                return Err(err::HOUSE_CANNOT_LOCATE_OVERLAP_HOUSE);
            }
        }
        validate_unit_overlap(server, player, &world, x0, y0, side, z, &ignored)?;
    }
    Ok(())
}

/// Beginner plots in the original data (`housing_groups.can_extend = false`) expire rather
/// than accepting another weekly payment. Ordinary or unclassified plots retain normal taxes.
pub fn can_extend_house(server: &GameServer, house: &crate::world::HouseRef) -> bool {
    let transform = house.data().unit.transform;
    let Some(world) = server.world.world(transform.instance_id) else {
        return false;
    };
    let data = &server.game_data.systems.housing;
    let pos = transform.local();
    let Ok(areas) =
        subzone::get_housing_zone_by_position(&server.game_data, &world.template, pos.x, pos.y)
    else {
        return false;
    };
    !areas
        .iter()
        .filter_map(|id| data.areas.get(id))
        .filter_map(|id| data.groups.get(id))
        .any(|group| !group.can_extend)
}

#[cfg(test)]
mod tests {
    use super::super::data::HousingGroup;
    use super::*;

    #[tokio::test]
    async fn placement_resolves_zone_ids_and_rejects_invalid_land_and_overlap() {
        use crate::data::{world_data::Zone, GameData};
        use crate::models::house::House;
        use crate::systems::npcs::subzone::Area;
        use crate::test_support::*;
        use crate::world::Entity;
        let mut data = GameData::empty();
        data.zones.zones.insert(
            TEST_ZONE,
            Zone {
                id: 42,
                zone_key: TEST_ZONE,
                ..Default::default()
            },
        );
        data.systems
            .npcs
            .subzone
            .housing_zones
            .entry(0)
            .or_default()
            .insert(
                42,
                vec![Area {
                    id: 500,
                    name: "Housing".into(),
                    points: vec![
                        [100., 100., 0.],
                        [900., 100., 0.],
                        [900., 900., 0.],
                        [100., 900., 0.],
                    ],
                }],
            );
        let template = HousingTemplate {
            id: 1,
            category_id: 17,
            garden_radius: 8.,
            ..Default::default()
        };
        data.systems.housing.add_template(template.clone());
        data.systems.housing.areas.insert(500, 1);
        data.systems.housing.groups.insert(
            1,
            HousingGroup {
                id: 1,
                ..Default::default()
            },
        );
        data.systems.housing.group_categories.insert(1, vec![17]);
        let server = test_server_with(data);
        let client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(&server, &client, test_character(1, "Owner", 500., 500.), 10);
        assert_eq!(
            validate_house(&server, &player, &template, 500., 500., 100.),
            Ok(())
        );
        assert_eq!(
            validate_house(&server, &player, &template, 50., 50., 100.),
            Err(err::HOUSE_CANNOT_LOACATE_INVALID_CATEGORY_AREA)
        );
        assert_eq!(
            validate_house(&server, &player, &template, 2500., 500., 100.),
            Err(err::HOUSE_CANNOT_LOCATE_INVALID_AREA)
        );
        let mut h = House::new(7, 1);
        h.unit.tl_id = 7;
        h.unit.scale = 1.0;
        h.unit.transform.set_xyz(510., 510., 100.);
        let house = Entity::new(20, h);
        assert!(server.systems.housing.manager.insert(&house));
        assert_eq!(
            validate_house(&server, &player, &template, 500., 500., 100.),
            Err(err::HOUSE_CANNOT_LOCATE_OVERLAP_HOUSE)
        );
        house.data().unit.transform.instance_id = 1;
        assert_eq!(
            validate_house(&server, &player, &template, 500., 500., 100.),
            Ok(())
        );
    }

    #[test]
    fn placement_groups_enforce_categories_and_account_eligibility() {
        let mut data = HousingData::default();
        data.areas.insert(100, 9);
        data.groups.insert(
            9,
            HousingGroup {
                id: 9,
                houseless: false,
                existing_category_id: 17,
                ..Default::default()
            },
        );
        data.group_categories.insert(9, vec![17]);
        let none = HashMap::new();
        assert_eq!(validate_groups(&data, &[100], 17, &[], &none), Ok(9));
        assert_eq!(
            validate_groups(&data, &[100], 1, &[], &none),
            Err(err::HOUSE_CANNOT_LOACATE_INVALID_CATEGORY_AREA)
        );
        assert_eq!(
            validate_groups(&data, &[100], 17, &[17], &none),
            Err(err::HOUSE_CANNOT_OWN_MORE_EXISTING_CATEGORY_CONDITION)
        );
        data.groups.get_mut(&9).unwrap().houseless = true;
        assert_eq!(
            validate_groups(&data, &[100], 17, &[1], &none),
            Err(err::HOUSE_CANNOT_OWN_MORE_HOUSELESS_CONDITION)
        );
        assert!(validate_groups(&data, &[999], 17, &[], &none).is_err());
    }

    #[test]
    fn placement_groups_enforce_the_authored_max_construct_count() {
        let mut data = HousingData::default();
        data.areas.insert(100, 9);
        data.groups.insert(
            9,
            HousingGroup {
                id: 9,
                ..Default::default()
            },
        );
        data.group_categories.insert(9, vec![17]);
        data.max_construct_count.insert((9, 17), 2);
        let mut counts = HashMap::new();
        counts.insert((9, 17), 1usize);
        assert_eq!(validate_groups(&data, &[100], 17, &[], &counts), Ok(9));
        counts.insert((9, 17), 2);
        assert_eq!(
            validate_groups(&data, &[100], 17, &[], &counts),
            Err(err::HOUSE_CANNOT_CONSTRUCT_IN_AREA_BY_MAX_CONSTRUCT_COUNT)
        );
        // A zero limit means unlimited.
        data.max_construct_count.insert((9, 17), 0);
        assert_eq!(validate_groups(&data, &[100], 17, &[], &counts), Ok(9));
    }

    #[tokio::test]
    async fn placement_counts_the_accounts_houses_against_the_authored_limit() {
        use crate::data::{world_data::Zone, GameData};
        use crate::models::house::House;
        use crate::systems::npcs::subzone::Area;
        use crate::test_support::*;
        use crate::world::Entity;
        let mut data = GameData::empty();
        data.zones.zones.insert(
            TEST_ZONE,
            Zone {
                id: 42,
                zone_key: TEST_ZONE,
                ..Default::default()
            },
        );
        data.systems
            .npcs
            .subzone
            .housing_zones
            .entry(0)
            .or_default()
            .insert(
                42,
                vec![Area {
                    id: 500,
                    name: "Housing".into(),
                    points: vec![
                        [100., 100., 0.],
                        [900., 100., 0.],
                        [900., 900., 0.],
                        [100., 900., 0.],
                    ],
                }],
            );
        let template = HousingTemplate {
            id: 1,
            category_id: 17,
            garden_radius: 8.,
            ..Default::default()
        };
        data.systems.housing.add_template(template.clone());
        data.systems.housing.areas.insert(500, 1);
        data.systems.housing.groups.insert(
            1,
            HousingGroup {
                id: 1,
                ..Default::default()
            },
        );
        data.systems.housing.group_categories.insert(1, vec![17]);
        data.systems.housing.max_construct_count.insert((1, 17), 2);
        let server = test_server_with(data);
        let client = TestClient::new(&server, 1, 1001);
        let player = spawn_player(&server, &client, test_character(1, "Owner", 500., 500.), 10);
        for (i, x) in [300., 320.].into_iter().enumerate() {
            let mut h = House::new(7 + i as u32, 1);
            h.id = 7 + i as u32;
            h.account_id = 1001;
            h.unit.scale = 1.0;
            h.unit.tl_id = 7 + i as u16;
            h.unit.transform.set_xyz(x, 300., 100.);
            let house = Entity::new(20 + i as u32, h);
            assert!(server.systems.housing.manager.insert(&house));
        }
        assert_eq!(
            validate_house(&server, &player, &template, 500., 500., 100.),
            Err(err::HOUSE_CANNOT_CONSTRUCT_IN_AREA_BY_MAX_CONSTRUCT_COUNT)
        );
    }

    /// Real 1.2 data: the client's own area-338 polygon, design 171's category/group and the
    /// authored `max_construct_count` row.
    /// `AAEMU_COMPACT_DB=... AAEMU_GAME_PAK=... cargo test -p aaemu-game
    /// real_area_338_footprint -- --ignored`.
    #[test]
    #[ignore = "needs AAEMU_COMPACT_DB and AAEMU_GAME_PAK"]
    fn real_area_338_footprint_accepts_only_the_fitting_spot() {
        use crate::data::client_files::ClientFileManager;
        use crate::data::world_data::ZoneData;
        use crate::data::world_templates::WorldTemplates;
        use crate::data::GameData;
        use crate::systems::npcs::subzone::SubZoneData;
        use crate::systems::DataLoadCtx;
        use rusqlite::{Connection, OpenFlags};
        use std::path::PathBuf;
        let (Ok(path), Ok(pak)) = (
            std::env::var("AAEMU_COMPACT_DB"),
            std::env::var("AAEMU_GAME_PAK"),
        ) else {
            return;
        };
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let app = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("app");
        let files = ClientFileManager::from_sources(&[pak], &app);
        let ctx = DataLoadCtx {
            conn: &conn,
            app_path: &app,
            files: &files,
            language: "en_us",
        };
        let mut data = GameData::empty();
        data.worlds = WorldTemplates::load(&files, &app).unwrap();
        data.zones = ZoneData::load(&conn, 0).unwrap();
        data.systems.housing = HousingData::load(&ctx).unwrap();
        data.systems.npcs.subzone = SubZoneData::load(&ctx).unwrap();
        let main = data
            .worlds
            .get_world_template_by_name("main_world")
            .unwrap();
        let areas = housing_areas(&data, main.id);
        let area338 = areas
            .iter()
            .find(|area| area.id == 338)
            .expect("client area 338");
        assert_eq!(data.systems.housing.areas.get(&338), Some(&15));
        assert!(data.systems.housing.group_categories[&15].contains(&18));
        assert_eq!(
            data.systems.housing.max_construct_count.get(&(1, 8)),
            Some(&3)
        );
        let cells = footprint_cells(11.0);
        assert_eq!(cells, 6);
        let list = [*area338];
        let (x0, y0) = footprint_origin(15328.4, 14631.8, cells);
        for ix in 0..cells {
            for iy in 0..cells {
                assert!(
                    cell_inside_one_area(
                        &list,
                        x0 + ix as f32 * FOOTPRINT_CELL,
                        y0 + iy as f32 * FOOTPRINT_CELL
                    ),
                    "accepted cell {ix},{iy}"
                );
            }
        }
        let (x0, y0) = footprint_origin(15338.53, 14632.33, cells);
        let inside = (0..cells).all(|ix| {
            (0..cells).all(|iy| {
                cell_inside_one_area(
                    &list,
                    x0 + ix as f32 * FOOTPRINT_CELL,
                    y0 + iy as f32 * FOOTPRINT_CELL,
                )
            })
        });
        assert!(!inside, "the captured spot must not fit area 338");
    }
}
