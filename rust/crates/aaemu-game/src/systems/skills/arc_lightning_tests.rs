//! Spreading DoT buffs (`tick_area_relation_id` 6: Arc Lightning / Electric Shock 250, 5847,
//! 7465). The DoT jumps from its holder to the holder's allies only: never to the caster, its
//! mount or its party, and hitting them never raises Retribution. Friendly tick areas
//! (relation 1) still reach the caster's allies.

use std::sync::Arc;

use crate::data::world_data::{FactionRelation, SystemFaction};
use crate::data::GameData;
use crate::models::buff::{Buff, SkillCaster};
use crate::models::mate::Mate;
use crate::systems::skills::data::effects::{BuffEffect, EffectTemplate};
use crate::systems::skills::data::{BuffTemplate, EffectKind, TickEffect};
use crate::systems::skills::types::{buff_constants, RelationState};
use crate::systems::skills::{bind, buffs};
use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
use crate::world::{now_ms, Entity, MateRef, PlayerRef, WorldObject};
use crate::GameServer;

const DOT: u32 = 9301;
const FRIENDLY_TICK: u32 = 9302;
const MARK: u32 = 9303;
const SPREAD_EFFECT: u32 = 9311;
const MARK_EFFECT: u32 = 9312;
const DAMAGE_EFFECT: u32 = 9313;
const PLAYERS: u32 = 101;
const MOBS: u32 = 102;

fn faction(id: u32) -> SystemFaction {
    SystemFaction {
        id,
        mother_id: 0,
        ..SystemFaction::default()
    }
}

fn tick(effect_id: u32) -> TickEffect {
    TickEffect {
        effect_id,
        target_buff_tag_id: 0,
        target_no_buff_tag_id: 0,
    }
}

fn buff_effect(id: u32, buff: u32) -> EffectTemplate {
    EffectTemplate::Buff(BuffEffect {
        id,
        buff_id: Some(buff),
        chance: 100,
        stack: 1,
        ab_level: 0,
    })
}

fn server() -> Arc<GameServer> {
    let mut data = GameData::empty();
    for id in [PLAYERS, MOBS] {
        data.factions.add(faction(id));
    }
    data.factions.relations.push(FactionRelation {
        id: PLAYERS,
        id2: MOBS,
        state: RelationState::Hostile as u8,
        exp_time: 0,
    });
    let s = &mut data.systems.skills;
    s.add_effect(EffectKind::BuffEffect, buff_effect(1, DOT), SPREAD_EFFECT);
    s.add_effect(EffectKind::BuffEffect, buff_effect(2, MARK), MARK_EFFECT);
    // The fixed value is the total over the 8 s duration: 320 / (8000/1000) = 40 per tick,
    // the damage the spread / Retribution assertions below expect (1.2 fixed-DoT rule).
    let mut damage = crate::systems::skills::tests::damage_effect(3, 320);
    damage.check_crime = true;
    s.add_effect(
        EffectKind::DamageEffect,
        EffectTemplate::Damage(Box::new(damage)),
        DAMAGE_EFFECT,
    );
    // The real Arc Lightning DoT (250): radius 2, relation 6, a spread and a damage tick.
    s.add_buff(BuffTemplate {
        id: DOT,
        duration: 8000,
        tick: 1000.0,
        tick_area_radius: 2.0,
        tick_area_relation_id: 6,
        tick_effects: vec![tick(SPREAD_EFFECT), tick(DAMAGE_EFFECT)],
        ..BuffTemplate::default()
    });
    // A friendly tick area (relation 1, evaluated from the caster) marking the allies.
    s.add_buff(BuffTemplate {
        id: FRIENDLY_TICK,
        duration: 8000,
        tick: 1000.0,
        tick_area_radius: 5.0,
        tick_area_relation_id: 1,
        tick_effects: vec![tick(MARK_EFFECT)],
        ..BuffTemplate::default()
    });
    s.add_buff(BuffTemplate {
        id: buff_constants::RETRIBUTION,
        duration: 60000,
        ..BuffTemplate::default()
    });
    s.add_buff(BuffTemplate {
        id: MARK,
        duration: 8000,
        ..BuffTemplate::default()
    });
    let server = Arc::new(test_server_with(data));
    server.set_self();
    bind(&server);
    server
}

struct Pc {
    _client: TestClient,
    p: PlayerRef,
}

impl Pc {
    fn new(server: &GameServer, id: u32, x: f32, faction_id: u32) -> Self {
        let mut client = TestClient::new(server, id, 1000 + id);
        let mut c = test_character(id, &format!("P{id}"), x, 100.0);
        c.unit.faction_id = faction_id;
        c.unit.hp = 1000;
        c.unit.max_hp = 1000;
        c.is_online = true;
        let p = spawn_player(
            server,
            &client,
            c,
            server.ids.object.get_next_id().expect("obj id"),
        );
        client.conn.active_char = Some(p.clone());
        client.drain();
        Self { _client: client, p }
    }

    fn obj(&self) -> WorldObject {
        WorldObject::from(&self.p)
    }

    fn has(&self, id: u32) -> bool {
        self.p.data().unit.buffs.check_buff(id)
    }

    fn hp(&self) -> i32 {
        self.p.data().unit.hp
    }
}

fn mount(server: &GameServer, owner: &Pc, x: f32) -> MateRef {
    let mut m = Mate::new(1, 1);
    m.owner_obj_id = owner.p.obj_id;
    m.unit.faction_id = PLAYERS;
    m.unit.hp = 1000;
    m.unit.max_hp = 1000;
    m.unit.transform.set_xyz(x, 100.0, 100.0);
    let mate = Entity::new(server.ids.object.get_next_id().expect("obj id"), m);
    server
        .world
        .world(0)
        .unwrap()
        .spawn(&mate, &server.view_ctx());
    mate
}

fn tick_buff(server: &GameServer, id: u32, caster: &Pc, holder: &WorldObject) {
    let template = server
        .game_data
        .systems
        .skills
        .buff(id)
        .expect("buff template")
        .clone();
    let b = Buff::new(
        crate::data::skills::BuffTemplate::simple(id, crate::data::skills::BuffKind::Bad),
        SkillCaster {
            kind: 1,
            obj_id: caster.p.obj_id,
        },
        None,
        now_ms(),
    );
    buffs::do_area_tick(server, &template, Some(&caster.obj()), holder, &b);
}

#[tokio::test]
async fn spread_dot_never_reaches_the_caster_its_mount_or_its_party() {
    let server = server();
    let caster = Pc::new(&server, 1, 100.0, PLAYERS);
    let party = Pc::new(&server, 2, 101.5, PLAYERS);
    let enemy = Pc::new(&server, 3, 101.0, MOBS);
    let second = Pc::new(&server, 4, 102.0, MOBS);
    let far = Pc::new(&server, 5, 110.0, MOBS);
    let mate = mount(&server, &caster, 100.5);
    let mate_obj = WorldObject::from(&mate);

    tick_buff(&server, DOT, &caster, &enemy.obj());

    // The holder takes the damage; the mechanic still spreads to the next enemy in range.
    assert_eq!(enemy.hp(), 960);
    assert!(
        second.has(DOT),
        "the DoT must jump to an ally of its holder"
    );
    assert_eq!(second.hp(), 960);
    assert!(!far.has(DOT));
    assert_eq!(far.hp(), 1000);
    // Neither the caster, its mount nor its party member is hit.
    assert!(!caster.has(DOT));
    assert_eq!(caster.hp(), 1000);
    assert!(!party.has(DOT));
    assert_eq!(party.hp(), 1000);
    assert!(!mate_obj.with_base_unit(|b| b.buffs().check_buff(DOT)));
    assert_eq!(mate.data().unit.hp, 1000);
    for pc in [&caster, &party] {
        assert!(!pc.has(buff_constants::RETRIBUTION), "no Retribution");
    }
}

#[tokio::test]
async fn own_mount_and_self_damage_never_raise_retribution() {
    let server = server();
    let caster = Pc::new(&server, 1, 100.0, PLAYERS);
    let mate = mount(&server, &caster, 100.5);
    let mate_obj = WorldObject::from(&mate);
    // Ctrl+F force attack makes friendly units attackable; a `check_crime` damage tick straight
    // onto the caster and onto its own mount must still do nothing (no damage, no Retribution).
    caster.p.data().unit.force_attack = true;
    for target in [caster.obj(), mate_obj] {
        let template = server.game_data.systems.skills.buff(DOT).unwrap().clone();
        let b = Buff::new(
            crate::data::skills::BuffTemplate::simple(DOT, crate::data::skills::BuffKind::Bad),
            SkillCaster {
                kind: 1,
                obj_id: caster.p.obj_id,
            },
            None,
            now_ms(),
        );
        let only_damage = BuffTemplate {
            tick_area_radius: 0.0,
            tick_effects: vec![tick(DAMAGE_EFFECT)],
            ..(*template).clone()
        };
        buffs::do_area_tick(
            &server,
            &Arc::new(BuffTemplate {
                tick_area_radius: 5.0,
                tick_area_relation_id: 0,
                ..only_damage
            }),
            Some(&caster.obj()),
            &target,
            &b,
        );
    }
    assert_eq!(caster.hp(), 1000);
    assert_eq!(mate.data().unit.hp, 1000);
    assert!(!caster.has(buff_constants::RETRIBUTION));
}

#[tokio::test]
async fn friendly_tick_area_still_reaches_allies() {
    let server = server();
    let caster = Pc::new(&server, 1, 100.0, PLAYERS);
    let ally = Pc::new(&server, 2, 101.5, PLAYERS);
    let enemy = Pc::new(&server, 3, 101.0, MOBS);
    tick_buff(&server, FRIENDLY_TICK, &caster, &caster.obj());
    assert!(caster.has(MARK));
    assert!(ally.has(MARK));
    assert!(!enemy.has(MARK));
}
