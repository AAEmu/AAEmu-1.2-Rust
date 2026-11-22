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

// ---- gimmicks

#[test]
fn gimmick_ids() {
    assert_eq!(r::FALLING_BARREL, 39);
    assert_eq!(r::ROLLING_BOULDER, 42);
}

#[test]
fn is_thrown_trap() {
    table(
        r::is_thrown_trap,
        &[49, 64, 66],
        &[0, 39, 42, 45, 46, 48, 50, 56, 63, 65, 67],
    );
}

// ---- skills, plots, buffs and controllers

#[test]
fn applies_to_source() {
    let pairs = [
        (17635, 19275),
        (19372, 23742),
        (21422, 29282),
        (21423, 29284),
        (21426, 29291),
        (19442, 23953),
        (19535, 24195),
        (19019, 24232),
        (19022, 24231),
        (21477, 29402),
    ];
    table(
        |(s, e)| r::applies_to_source(s, e),
        &pairs,
        &[
            (0, 0),
            (17635, 19276),
            (17635, 23742),
            (19372, 19275),
            (21422, 29284),
            (19019, 24231),
            (19022, 24232),
            (19535, 24194),
            (21477, 29403),
            (17636, 19275),
        ],
    );
}

#[test]
fn doodad_hit_skill() {
    assert_eq!(r::doodad_hit_skill(20615, || 5947), Some(17422));
    assert_eq!(r::doodad_hit_skill(20615, || 5946), None);
    assert_eq!(r::doodad_hit_skill(20615, || 4577), None);
    for skill in [0, 17422, 20614, 20616] {
        // The doodad is not asked for another skill.
        assert_eq!(
            r::doodad_hit_skill(skill, || panic!("doodad read for {skill}")),
            None
        );
    }
}

#[test]
fn hostile_area_skill() {
    table(
        r::hostile_area_skill,
        &[15592, 15668],
        &[0, 15591, 15593, 15667, 15669, 15629],
    );
}

#[test]
fn hits_only_accepting_doodads() {
    table(r::hits_only_accepting_doodads, &[20212], &[0, 20211, 20213]);
}

#[test]
fn plot_targets_caster() {
    table(
        |(s, e)| r::plot_targets_caster(s, e),
        &[(20615, 5347)],
        &[
            (20615, 5346),
            (20615, 5348),
            (20614, 5347),
            (20616, 5347),
            (0, 0),
        ],
    );
}

#[test]
fn keeps_first_random_target() {
    table(
        r::keeps_first_random_target,
        &[1452],
        &[0, 1451, 1453, 1508],
    );
}

#[test]
fn refreshes_stack_window() {
    table(r::refreshes_stack_window, &[2407], &[0, 2406, 2408, 2409]);
}

#[test]
fn is_nerta_bomb_drop() {
    for buff in [4099, 4631] {
        assert!(r::is_nerta_bomb_drop(buff, || Some(11362)), "buff {buff}");
        assert!(!r::is_nerta_bomb_drop(buff, || Some(11361)), "buff {buff}");
        assert!(!r::is_nerta_bomb_drop(buff, || Some(11363)), "buff {buff}");
        assert!(!r::is_nerta_bomb_drop(buff, || None), "buff {buff}");
    }
    for buff in [0, 4098, 4100, 4630, 4632] {
        // The owner is not asked for another buff.
        assert!(!r::is_nerta_bomb_drop(buff, || panic!(
            "owner read for {buff}"
        )));
    }
    assert_eq!((r::NERTA_BOMB_BUFF, r::NERTA_BOMB_BUFF_2), (4099, 4631));
}

#[test]
fn server_runs_buff_controller() {
    table(r::server_runs_buff_controller, &[2659], &[0, 2658, 2660]);
}

#[test]
fn trigger_source_is_caster() {
    use r::TriggerCaster::*;
    let no_npc = || -> Option<u32> { panic!("caster NPC read for a spear") };
    let no_doodad = || -> Option<u32> { None };
    for doodad in [5516, 5706, 5708] {
        assert_eq!(
            r::trigger_source_is_caster(3999, || Some(doodad), no_npc),
            Some(FloorSpear),
            "doodad {doodad}"
        );
    }
    for doodad in [5515, 5517, 5705, 5707, 5709] {
        assert_eq!(
            r::trigger_source_is_caster(3999, || Some(doodad), || None),
            None,
            "doodad {doodad}"
        );
    }
    assert_eq!(
        r::trigger_source_is_caster(3999, || None, || Some(9792)),
        None
    );
    assert_eq!(
        r::trigger_source_is_caster(2586, no_doodad, || Some(9792)),
        Some(VelarreTombstone)
    );
    for npc in [9791, 9793] {
        assert_eq!(
            r::trigger_source_is_caster(2586, no_doodad, || Some(npc)),
            None
        );
    }
    assert_eq!(
        r::trigger_source_is_caster(2586, || Some(5516), || None),
        None
    );
    for buff in [0, 2585, 2587, 3998, 4000] {
        // Neither caster is asked for another buff.
        assert_eq!(
            r::trigger_source_is_caster(
                buff,
                || panic!("caster doodad read for {buff}"),
                || panic!("caster NPC read for {buff}")
            ),
            None
        );
    }
}

#[test]
fn dashes_to_victim() {
    table(
        r::dashes_to_victim,
        &[6640, 6683, 7130, 7149],
        &[0, 6639, 6641, 6682, 6684, 7129, 7131, 7148, 7150],
    );
}

#[test]
fn probes_raised_floors() {
    table(
        r::probes_raised_floors,
        &[265, 280],
        &[0, 240, 262, 264, 266, 279, 281],
    );
}

#[test]
fn skill_ids() {
    assert_eq!(r::POWDER_KEG_SKILL, 18715);
    assert_eq!(r::ARMORY_EXIT_SKILL, 15667);
    assert_eq!(crate::systems::instances::indun::ARMORY_EXIT_SKILL, 15667);
    assert_eq!(r::CRADLE_CHEST_SKILL, 19691);
}

#[test]
fn is_armory_exit_doodad() {
    table(
        |(t, g)| r::is_armory_exit_doodad(t, g),
        &[(3673, 8794), (4923, 12257)],
        &[
            (3673, 12257),
            (4923, 8794),
            (3672, 8794),
            (3673, 8793),
            (4923, 12258),
            (0, 0),
        ],
    );
}

// ---- plots

#[test]
fn plot_ground() {
    use r::PlotGround::*;
    let no_zone = || -> u32 { panic!("zone read for a skill that is not the Colossus's") };
    let cases = [
        ((15629, 2668), Some(MeteorLaunch)),
        ((15629, 1508), Some(MeteorImpact)),
        ((15629, 2667), None),
        ((15629, 1509), None),
        ((15628, 2668), None),
        ((15630, 1508), None),
        ((18976, 4023), Some(CalleilFireball)),
        ((18976, 5919), Some(CalleilFireball)),
        ((21131, 4023), Some(CalleilFireball)),
        ((21131, 5919), Some(CalleilFireball)),
        ((18976, 4024), Some(CalleilImpact)),
        ((21131, 5920), Some(CalleilImpact)),
        ((18976, 5920), None),
        ((21131, 4024), None),
        ((18975, 4023), None),
        ((21132, 5919), None),
        ((18976, 4022), None),
        ((0, 2668), None),
        ((0, 4023), None),
    ];
    for ((skill, event), ground) in cases {
        assert_eq!(
            r::plot_ground(skill, event, no_zone),
            ground,
            "skill {skill}, event {event}"
        );
    }
    for skill in [18374, 18401, 18406, 19248] {
        for event in [3199, 3201] {
            assert_eq!(
                r::plot_ground(skill, event, || 262),
                Some(ColossusFist { near: true })
            );
        }
        for event in [0, 3198, 3200, 3202] {
            assert_eq!(
                r::plot_ground(skill, event, || 262),
                Some(ColossusFist { near: false })
            );
        }
        for zone in [0, 261, 263, 265] {
            assert_eq!(r::plot_ground(skill, 3199, || zone), None, "zone {zone}");
        }
    }
    for skill in [18373, 18375, 18400, 18402, 18405, 18407, 19247, 19249] {
        assert_eq!(r::plot_ground(skill, 3199, no_zone), None, "skill {skill}");
    }
}

// ---- spawn effects

#[test]
fn summon_boss() {
    let cases = [
        (11273, Some(11265)),
        (12959, Some(12762)),
        (12384, Some(11201)),
        (12912, Some(12759)),
        (0, None),
        (11265, None),
        (11272, None),
        (11274, None),
        (12383, None),
        (12385, None),
        (12911, None),
        (12913, None),
        (12958, None),
        (12960, None),
    ];
    for (sub_type, boss) in cases {
        assert_eq!(r::summon_boss(sub_type), boss, "sub type {sub_type}");
    }
}

#[test]
fn is_wave_spawn() {
    // Hieronimus: his portal spawners, only with the summoner's aggro target.
    for spawner in [46629, 46640, 46656] {
        assert!(r::is_wave_spawn(8962, spawner, true), "spawner {spawner}");
        assert!(!r::is_wave_spawn(8962, spawner, false), "spawner {spawner}");
    }
    for spawner in [0, 46628, 46657] {
        assert!(!r::is_wave_spawn(8962, spawner, true), "spawner {spawner}");
    }
    // Viboros: any spawner with the summoner's aggro target.
    for caster in [11265, 12762] {
        assert!(r::is_wave_spawn(caster, 0, true));
        assert!(r::is_wave_spawn(caster, 46629, true));
        assert!(!r::is_wave_spawn(caster, 0, false));
    }
    // Calleil: listed spawners, whatever the flag.
    for spawner in [102334, 102337, 102340, 102341, 102342] {
        assert!(r::is_wave_spawn(11201, spawner, false), "spawner {spawner}");
        assert!(r::is_wave_spawn(11201, spawner, true), "spawner {spawner}");
    }
    for spawner in [0, 102333, 102335, 102336, 102338, 102339, 102343, 110540] {
        assert!(!r::is_wave_spawn(11201, spawner, true), "spawner {spawner}");
    }
    for spawner in [110540, 110541, 110544, 110545, 110548] {
        assert!(r::is_wave_spawn(12759, spawner, false), "spawner {spawner}");
    }
    for spawner in [0, 110539, 110542, 110543, 110546, 110547, 110549, 102334] {
        assert!(!r::is_wave_spawn(12759, spawner, true), "spawner {spawner}");
    }
    // Other casters.
    for caster in [
        0, 8961, 8963, 11264, 11266, 11200, 11202, 12758, 12760, 12761, 12763,
    ] {
        assert!(!r::is_wave_spawn(caster, 46629, true), "caster {caster}");
        assert!(!r::is_wave_spawn(caster, 102334, true), "caster {caster}");
        assert!(!r::is_wave_spawn(caster, 110540, true), "caster {caster}");
    }
}
