use super::*;

/// Precision Strike 12026: `onehand_co_sk_weapon_blunt_2` (132), two-handed `twohand_co_sk_weapon_blunt_2` (136); a skill
/// without a variant keeps its own clip for every weapon.
#[test]
fn the_fire_clip_follows_the_weapons_held() {
    let t = SkillTemplate {
        fire_anim_id: 132,
        twohand_fire_anim_id: 136,
        dual_wield_fire_anim_id: 504,
        ..Default::default()
    };
    assert_eq!(fire_anim_for(&t, 0), 132, "no weapon");
    assert_eq!(fire_anim_for(&t, 1), 132, "one weapon");
    assert_eq!(fire_anim_for(&t, 2), 136, "a two-handed weapon");
    assert_eq!(fire_anim_for(&t, 3), 504, "two weapons");
    let plain = SkillTemplate {
        fire_anim_id: 18,
        ..Default::default()
    };
    assert_eq!(fire_anim_for(&plain, 2), 18);
    assert_eq!(fire_anim_for(&plain, 3), 18);
}
