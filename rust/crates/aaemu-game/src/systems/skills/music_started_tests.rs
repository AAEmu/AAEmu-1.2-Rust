//! Instant player play / pause score skills (special effects 93 / 94) announce themselves with
//! the player's own `SCSkillStarted` immediately before their `SCSkillFired`; other instant
//! skills, rejected casts and non-player casters do not.

use aaemu_commons::PacketStream;

use super::super::types::skill_result;
use super::*;
use crate::systems::skills::skill::is_player_music_skill;

const PLAY: u32 = 10_093;
const PAUSE: u32 = 10_094;
const CLIENT_ONLY: u32 = 10_037;
const COSTLY_PLAY: u32 = 10_095;

fn special(id: u32, type_id: i32) -> EffectTemplate {
    EffectTemplate::Special(super::super::data::effects::SpecialEffect {
        id,
        special_effect_type_id: type_id,
        value1: 0,
        value2: 0,
        value3: 0,
        value4: 0,
    })
}

fn music_data() -> GameData {
    let mut d = data();
    let s = &mut d.systems.skills;
    s.add_effect(EffectKind::SpecialEffect, special(9_301, 93), 931);
    s.add_effect(EffectKind::SpecialEffect, special(9_302, 94), 932);
    s.add_effect(EffectKind::SpecialEffect, special(9_303, 37), 933);
    for (id, effect, actual) in [
        (PLAY, 931, 9_301),
        (PAUSE, 932, 9_302),
        (CLIENT_ONLY, 933, 9_303),
    ] {
        let mut t = skill_template(id);
        t.target_type = 0; // Self
        t.damage_type_id = 0;
        t.casting_time = 0;
        t.effect_delay = 200;
        t.effects
            .push(skill_effect(effect, EffectKind::SpecialEffect, actual));
        s.add_skill(t);
    }
    let mut costly = (**s.skill(PLAY).unwrap()).clone();
    costly.id = COSTLY_PLAY;
    costly.mana_cost = 5_000;
    s.add_skill(costly);
    d
}

fn started_and_fired(frames: &[(u16, Vec<u8>)], skill: u32) -> (Vec<usize>, Vec<usize>) {
    let id = skill.to_le_bytes();
    let pick = |op: u16| {
        frames
            .iter()
            .enumerate()
            .filter(|(_, (o, b))| *o == op && b.starts_with(&id))
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
    };
    (pick(sc::SCSkillStartedPacket), pick(sc::SCSkillFiredPacket))
}

#[tokio::test]
async fn play_and_pause_announce_started_right_before_fired() {
    for (skill, effect_op) in [
        (PLAY, sc::SCSendUserMusicPacket),
        (PAUSE, sc::SCPauseUserMusicPacket),
    ] {
        let mut f = fixture_with(music_data());
        start_skill(
            &f.server,
            &mut f.client.conn,
            cast_packet(skill, SkillCastTarget::Unit(PLAYER_OBJ)),
        );
        let got = frames(&mut f.client);
        let (started, fired) = started_and_fired(&got, skill);
        assert_eq!((started.len(), fired.len()), (1, 1), "{skill}: {got:x?}");
        assert_eq!(
            started[0] + 1,
            fired[0],
            "{skill}: Started directly before Fired"
        );
        // Timeline: the same non-zero id as the fire; zero cast-time fields, no result extras.
        let fire_body = &got[fired[0]].1;
        let tl = u16::from_le_bytes([fire_body[4], fire_body[5]]);
        assert_ne!(tl, 0);
        let mut expected = PacketStream::new();
        expected
            .write_u32(skill)
            .write_u16(tl)
            .write_u8(0)
            .write_bc(PLAYER_OBJ) // own unit caster
            .write_u8(0)
            .write_bc(PLAYER_OBJ) // unit target
            .write_u8(0) // no skill object
            .write_u16(0) // real cast time
            .write_u16(0) // base cast time
            .write_u8(0) // cast synergy
            .write_u8(0); // no extra data
        assert_eq!(got[started[0]].1, expected.as_bytes(), "{skill}");
        // The authored 200 ms effect delay is untouched: the effect arrives after the fire.
        assert!(!got.iter().any(|(op, _)| *op == effect_op));
        tokio::time::sleep(std::time::Duration::from_millis(450)).await;
        let later = frames(&mut f.client);
        assert!(
            later.iter().any(|(op, _)| *op == effect_op),
            "{skill}: {later:x?}"
        );
        assert!(!later.iter().any(|(op, _)| *op == sc::SCSkillStartedPacket));
    }
}

#[tokio::test]
async fn other_instant_skills_get_no_started() {
    let mut f = fixture_with(music_data());
    for skill in [HEAL_SKILL, CLIENT_ONLY] {
        start_skill(
            &f.server,
            &mut f.client.conn,
            cast_packet(skill, SkillCastTarget::Unit(PLAYER_OBJ)),
        );
        let got = frames(&mut f.client);
        let (started, fired) = started_and_fired(&got, skill);
        assert!(started.is_empty(), "{skill}: {got:x?}");
        assert_eq!(fired.len(), 1, "{skill}");
        age_gcd_for_next_cast(&f);
    }
}

fn age_gcd_for_next_cast(f: &Fixture) {
    f.player.data().unit.global_cooldown_ms = 0;
    f.player.data().unit.skill_last_used_ms = 0;
}

#[tokio::test]
async fn rejected_play_sends_only_the_failure_started() {
    let mut f = fixture_with(music_data());
    start_skill(
        &f.server,
        &mut f.client.conn,
        cast_packet(COSTLY_PLAY, SkillCastTarget::Unit(PLAYER_OBJ)),
    );
    let got = frames(&mut f.client);
    let (started, fired) = started_and_fired(&got, COSTLY_PLAY);
    assert!(fired.is_empty(), "{got:x?}");
    assert_eq!(started.len(), 1, "{got:x?}");
    let body = &got[started[0]].1;
    // The failure carries extra data (flag byte after the cast fields) and a zero timeline.
    assert_eq!(u16::from_le_bytes([body[4], body[5]]), 0);
    // Extra-data flag 1 (byte result) and the LACK_MANA result.
    assert_eq!(
        &body[body.len() - 2..],
        &[1, skill_result::LACK_MANA],
        "{body:x?}"
    );
}

#[tokio::test]
async fn only_player_casters_with_a_music_special_effect_qualify() {
    let f = fixture_with(music_data());
    let skills = &f.server.game_data.systems.skills;
    let me = WorldObject::from(&f.player);
    let npc = WorldObject::from(&f.npc);
    assert!(is_player_music_skill(
        &f.server,
        skills.skill(PLAY).unwrap(),
        &me
    ));
    assert!(is_player_music_skill(
        &f.server,
        skills.skill(PAUSE).unwrap(),
        &me
    ));
    assert!(!is_player_music_skill(
        &f.server,
        skills.skill(CLIENT_ONLY).unwrap(),
        &me
    ));
    assert!(!is_player_music_skill(
        &f.server,
        skills.skill(HEAL_SKILL).unwrap(),
        &me
    ));
    assert!(!is_player_music_skill(
        &f.server,
        skills.skill(PLAY).unwrap(),
        &npc
    ));
}
