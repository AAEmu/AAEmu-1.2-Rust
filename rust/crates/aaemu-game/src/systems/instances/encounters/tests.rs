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
