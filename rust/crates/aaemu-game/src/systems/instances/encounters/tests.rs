//! Truth tables for the registry facade: the ids of the engine lines each function replaced,
//! and neighbouring ids that must not match.

use super::registry as r;
use super::{armory, cradle, hadir, howling_abyss, palace_cellar, sharpwind};

/// Asserts `f(id)` for every id in `yes` and `!f(id)` for every id in `no`.
fn table<T: Copy + std::fmt::Debug>(f: impl Fn(T) -> bool, yes: &[T], no: &[T]) {
    for &id in yes {
        assert!(f(id), "{id:?} must match");
    }
    for &id in no {
        assert!(!f(id), "{id:?} must not match");
    }
}

#[test]
fn keys() {
    assert_eq!(
        (armory::WORLD, armory::ZONE),
        ("instance_burntcastle_armory", 236)
    );
    assert_eq!(
        (palace_cellar::WORLD, palace_cellar::ZONE),
        ("instance_sal_temple", 240)
    );
    assert_eq!((hadir::WORLD, hadir::ZONE), ("instance_hadir_farm", 241));
    assert_eq!(
        (sharpwind::WORLD, sharpwind::ZONE),
        ("instance_cuttingwind_deadmine", 262)
    );
    assert_eq!(
        (cradle::WORLD, cradle::ZONE),
        ("instance_cradle_of_destruction", 264)
    );
    assert_eq!(
        (howling_abyss::WORLD, howling_abyss::ZONE),
        ("instance_howling_abyss", 265)
    );
    assert_eq!(
        (howling_abyss::GREATER_WORLD, howling_abyss::GREATER_ZONE),
        ("instance_howling_abyss_2", 280)
    );
}

// ---- AI and command sets

#[test]
fn walks_scene_routes() {
    table(
        r::walks_scene_routes,
        &[222, 223, 224, 228, 229, 230, 234, 235, 236, 237],
        &[0, 207, 221, 225, 227, 231, 232, 233, 238, 240],
    );
}

#[test]
fn is_encounter_intro() {
    table(
        r::is_encounter_intro,
        &[65, 79, 80, 81, 103, 106, 107, 126],
        &[0, 64, 66, 78, 82, 102, 104, 105, 108, 125, 127],
    );
}

#[test]
fn route_runs_with_flags() {
    assert_eq!(r::route_runs_with_flags("aipath_alistair"), Some(4));
    assert_eq!(r::route_runs_with_flags("aipath_alistair_02"), Some(4));
    assert_eq!(r::route_runs_with_flags("aipath_alistai"), None);
    assert_eq!(r::route_runs_with_flags("x_aipath_alistair"), None);
    assert_eq!(r::route_runs_with_flags(""), None);
}

#[test]
fn appended_intro_skill() {
    assert_eq!(r::appended_intro_skill(103, || 10518), Some(17196));
    assert_eq!(r::appended_intro_skill(103, || 10517), None);
    assert_eq!(r::appended_intro_skill(103, || 8962), None);
    for set in [0, 102, 104, 106, 107] {
        // The template is not asked for another command set.
        assert_eq!(
            r::appended_intro_skill(set, || panic!("template read for set {set}")),
            None
        );
    }
}

#[test]
fn runs_spawn_events_on_spawn() {
    table(
        r::runs_spawn_events_on_spawn,
        &[12014, 12188],
        &[0, 12013, 12015, 12187, 12189, 12190],
    );
}

#[test]
fn waits_for_visible_caster() {
    table(
        r::waits_for_visible_caster,
        &[18983, 19535],
        &[0, 18982, 18984, 19534, 19536],
    );
}

#[test]
fn is_directional_self_skill() {
    table(
        r::is_directional_self_skill,
        &[15935, 19980, 18374, 18401, 18406, 19248],
        &[
            0, 15934, 15936, 19979, 19981, 18373, 18375, 18400, 18402, 18405, 18407, 19247, 19249,
        ],
    );
}

#[test]
fn is_priority_combat_skill() {
    table(r::is_priority_combat_skill, &[18485], &[0, 18484, 18486]);
}

#[test]
fn floor_bound() {
    let templates = [11258, 11264, 11757, 11759, 12765, 12784, 12785];
    for zone in [265, 280] {
        table(
            |t| r::floor_bound(zone, t),
            &templates,
            &[
                0, 11257, 11259, 11263, 11265, 11756, 11758, 11760, 12764, 12766, 12783, 12786,
            ],
        );
    }
    for zone in [0, 240, 264, 266, 279, 281] {
        table(|t| r::floor_bound(zone, t), &[], &templates);
    }
    assert_eq!(r::FLOOR_GAP, 3.0);
}

#[test]
fn helps_only_near_the_pull() {
    table(
        r::helps_only_near_the_pull,
        &["instance_cuttingwind_deadmine", "instance_hadir_farm"],
        &[
            "",
            "main_world",
            "instance_cuttingwind_deadmine_2",
            "instance_hadir",
            "instance_howling_abyss",
            "instance_sal_temple",
        ],
    );
}

#[test]
fn is_wave_add() {
    table(
        |t| r::is_wave_add(236, t),
        &[8964, 8965, 8966, 8967, 8968, 8969],
        &[0, 8962, 8963, 8970, 10518],
    );
    for zone in [0, 235, 237, 262] {
        table(|t| r::is_wave_add(zone, t), &[], &[8964, 8966, 8969]);
    }
}

#[test]
fn raised_floor_drop() {
    for zone in [240, 265, 280] {
        assert_eq!(r::raised_floor_drop(zone), Some(2.5), "zone {zone}");
    }
    for zone in [0, 236, 239, 241, 262, 264, 266, 279, 281] {
        assert_eq!(r::raised_floor_drop(zone), None, "zone {zone}");
    }
}

// ---- cinemas

#[test]
fn broadcasts_room_cinema() {
    table(
        r::broadcasts_room_cinema,
        &[51, 53, 133],
        &[0, 8, 9, 50, 52, 54, 132, 134],
    );
}

#[test]
fn plays_cinema_directly() {
    table(
        r::plays_cinema_directly,
        &[8, 9, 52],
        &[0, 7, 10, 51, 53, 133],
    );
}

// ---- doodads and clouts

#[test]
fn clout_class() {
    use r::CloutClass::*;
    let cases = [
        ((4626, 0, 3615), Some(HowlWard)),
        ((4626, 0, 3614), None),
        ((4625, 0, 3615), None),
        ((4627, 0, 3615), None),
        ((5170, 0, 0), Some(HowlingHazard)),
        ((5383, 0, 0), Some(HowlingHazard)),
        ((5706, 0, 0), Some(HowlingHazard)),
        ((5708, 7, 9), Some(HowlingHazard)),
        ((6261, 0, 0), Some(HowlingHazard)),
        ((6273, 0, 0), Some(HowlingHazard)),
        ((5169, 0, 0), None),
        ((5707, 0, 0), None),
        ((6262, 0, 0), None),
        ((6274, 0, 0), None),
        ((3874, 0, 0), Some(HadirHazard)),
        ((3956, 1, 2), Some(HadirHazard)),
        ((3873, 0, 0), None),
        ((3955, 0, 0), None),
        ((5150, 1152, 0), Some(FrozenZone)),
        ((5150, 1151, 0), None),
        ((5150, 1153, 0), None),
        ((5149, 1152, 0), None),
        ((4109, 0, 2783), Some(PalaceFlame)),
        ((4741, 5, 2783), Some(PalaceFlame)),
        ((4109, 0, 2782), None),
        ((4741, 0, 2784), None),
        ((4110, 0, 2783), None),
        ((4740, 0, 2783), None),
        ((0, 0, 0), None),
        ((5516, 1295, 3999), None),
    ];
    for ((template, clout, buff), class) in cases {
        assert_eq!(
            r::clout_class(template, clout, buff),
            class,
            "doodad {template}, clout {clout}, buff {buff}"
        );
    }
}

#[test]
fn clout_reapplies_contact() {
    table(
        r::clout_reapplies_contact,
        &[1139, 1266, 1267, 1295, 1334, 1336],
        &[
            0, 1138, 1140, 1265, 1268, 1294, 1296, 1333, 1335, 1337, 1182, 1415,
        ],
    );
}

#[test]
fn is_contact_buff() {
    table(r::is_contact_buff, &[3996], &[0, 3995, 3997, 3999]);
}

#[test]
fn clout_requires_submersion() {
    let buffs = [3813, 4362, 4483, 4683];
    table(
        |b| r::clout_requires_submersion("instance_cradle_of_destruction", b),
        &buffs,
        &[0, 3812, 3814, 4361, 4363, 4482, 4484, 4682, 4684, 3996],
    );
    for world in [
        "",
        "main_world",
        "instance_howling_abyss",
        "instance_cradle",
    ] {
        table(|b| r::clout_requires_submersion(world, b), &[], &buffs);
    }
}

#[test]
fn clout_payload_without_inside_buff() {
    table(
        r::clout_payload_without_inside_buff,
        &[1415, 1542],
        &[0, 1414, 1416, 1541, 1543, 1152],
    );
}

#[test]
fn powder_keg_ids() {
    assert_eq!(r::POWDER_KEG_DOODAD, 5951);
    assert_eq!(r::POWDER_KEG_ITEM, 25002);
}
