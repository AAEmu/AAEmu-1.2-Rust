//! Siege larceny window (`ui_texts` 3334 `in_siege_warning`) flow tests.
//!
//! Data: `D:/aaemu-audits/gap-audit-2026-11-02/research/siege-larceny-web.md` and
//! `round6-trial-larceny-2.md` 2. The rule set is INFERRED from the client string (the 1.2 data
//! has no table for it); see `larceny.rs` and the `PORTING.md` row.

use std::sync::{Arc, Mutex};

use crate::config::AppConfig;
use crate::data::world_data::{FactionRelation, SystemFaction, Zone};
use crate::data::GameData;
use crate::models::doodad::{doodad_owner_type, Doodad, DoodadCofferState};
use crate::models::house::housing_permission;
use crate::server::GameServer;
use crate::systems::housing::config::{HousingConfig, SiegeLarcenyMode};
use crate::systems::housing::dominion::runtime::tower_identity;
use crate::systems::housing::dominion::{SiegeOutcome, SiegeStatus, TowerIdentity};
use crate::systems::housing::{self, manager};
use crate::systems::npcs::doodad::interactions::{self, world_interaction_type as wi};
use crate::systems::npcs::doodad::phase;
use crate::systems::npcs::doodad::runtime::DoodadCtx;
use crate::systems::npcs::doodad::templates::{
    DoodadFunc, DoodadFuncLootPack, DoodadFuncRecoverItem, DoodadFuncUse, DoodadGroups,
    DoodadTemplate, FuncTemplate,
};
use crate::systems::npcs::hooks::{EconomyBridge, SkillsBridge};
use crate::systems::skills::data::BuffTemplate;
use crate::systems::social::faction::relation_state;
use crate::test_support::*;
use crate::world::{DoodadRef, Entity, HouseRef, PlayerRef, WorldObject};

const ZONE_GROUP: u32 = 25;
const HOUSE_TEMPLATE: u32 = 300;
const TOWER_TEMPLATE: u32 = 139;
const CROP_TEMPLATE: u32 = 500;
const HERB_TEMPLATE: u32 = 503;
const BARN_TEMPLATE: u32 = 504;
const RACK_TEMPLATE: u32 = 505;
const FOAL_TEMPLATE: u32 = 506;
const FURNITURE_TEMPLATE: u32 = 501;
const COFFER_TEMPLATE: u32 = 502;
const CROP_GROUP: u32 = 12; // 농사 - 작물
const HERB_GROUP: u32 = 3; // 채집 - 약초
const LIVESTOCK_GROUP: u32 = 5; // 축산 - 가축
const FURNITURE_GROUP: u32 = 24; // 기타 - 인터랙션
const USE_GROUP: u32 = 10;
const USE_NEXT_GROUP: u32 = 11;
const LOOT_GROUP: u32 = 20;
const LOOT_NEXT_GROUP: u32 = 21;
const RECOVER_GROUP: u32 = 30;
const RECOVER_NEXT_GROUP: u32 = 31;
const USE_FUNC: u32 = 900;
const LOOT_FUNC: u32 = 901;
const RECOVER_FUNC: u32 = 902;
const RECOVER_SKILL: u32 = 11361;
const SKILL: u32 = 4242;
const UPROOT_GROUP: u32 = 50;
const UPROOT_NEXT_GROUP: u32 = 51;
const UPROOT_FUNC: u32 = 903;
const UPROOT_SKILL: u32 = crate::systems::housing::dominion::larceny::UPROOT_SKILL_ID;

const DEFENDER_FACTION: u32 = 101;
const OPPONENT_FACTION: u32 = 102;

/// `EconomyBridge::give_doodad_loot_pack` recorder.
#[derive(Default)]
struct LootRec {
    packs: Mutex<Vec<(u32, u32)>>,
}

impl EconomyBridge for LootRec {
    fn give_doodad_loot_pack(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        id: u32,
        skill: u32,
        _task: u8,
    ) -> Option<bool> {
        self.packs.lock().unwrap().push((id, skill));
        Some(true)
    }
}

/// `SkillsBridge::generate_theft_evidence` recorder.
#[derive(Default)]
struct EvidenceRec {
    criminals: Mutex<Vec<u32>>,
}

impl SkillsBridge for EvidenceRec {
    fn generate_theft_evidence(
        &self,
        _server: &GameServer,
        criminal: &PlayerRef,
        _doodad: &DoodadRef,
    ) {
        self.criminals.lock().unwrap().push(criminal.data().id);
    }
}

fn template(id: u32, group_id: u32) -> DoodadTemplate {
    DoodadTemplate {
        id,
        group_id,
        group: DoodadGroups {
            id: group_id,
            ..DoodadGroups::default()
        },
        ..DoodadTemplate::default()
    }
}

fn use_func(group: u32) -> DoodadFunc {
    DoodadFunc {
        group_id: group,
        func_id: USE_FUNC,
        func_type: "DoodadFuncUse".into(),
        next_phase: USE_NEXT_GROUP as i32,
        skill_id: SKILL,
        ..DoodadFunc::default()
    }
}

fn uproot_func(group: u32) -> DoodadFunc {
    DoodadFunc {
        group_id: group,
        func_id: UPROOT_FUNC,
        func_type: "DoodadFuncUse".into(),
        next_phase: UPROOT_NEXT_GROUP as i32,
        skill_id: UPROOT_SKILL,
        ..DoodadFunc::default()
    }
}

fn loot_func(group: u32) -> DoodadFunc {
    DoodadFunc {
        group_id: group,
        func_id: LOOT_FUNC,
        func_type: "DoodadFuncLootPack".into(),
        next_phase: LOOT_NEXT_GROUP as i32,
        ..DoodadFunc::default()
    }
}

fn game_data() -> GameData {
    let mut data = GameData::empty();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            zone_key: TEST_ZONE,
            group_id: ZONE_GROUP,
            ..Default::default()
        },
    );
    data.factions.add(SystemFaction {
        id: DEFENDER_FACTION,
        name: "Defender".into(),
        ..Default::default()
    });
    data.factions.add(SystemFaction {
        id: OPPONENT_FACTION,
        name: "Opponent".into(),
        ..Default::default()
    });
    data.factions.relations.push(FactionRelation {
        id: DEFENDER_FACTION,
        id2: OPPONENT_FACTION,
        state: relation_state::HOSTILE,
        ..Default::default()
    });
    for id in [
        manager::BUFF_UNTOUCHABLE,
        manager::BUFF_REMOVAL_DEBUFF,
        manager::BUFF_BURNING_HOUSE,
    ] {
        data.systems.skills.add_buff(BuffTemplate {
            id,
            max_stack: 1,
            system: true,
            ..Default::default()
        });
    }
    for group_id in [
        CROP_GROUP,
        HERB_GROUP,
        LIVESTOCK_GROUP,
        2,  // 벌채 - 나무
        40, // 농사 - 해양 작물
        FURNITURE_GROUP,
    ] {
        data.systems.npcs.doodad.doodad_groups.insert(
            group_id,
            DoodadGroups {
                id: group_id,
                ..Default::default()
            },
        );
    }
    for (id, group_id) in [
        (CROP_TEMPLATE, CROP_GROUP),
        (HERB_TEMPLATE, HERB_GROUP),
        (BARN_TEMPLATE, LIVESTOCK_GROUP),
        (RACK_TEMPLATE, LIVESTOCK_GROUP),
        (FOAL_TEMPLATE, LIVESTOCK_GROUP),
        (FURNITURE_TEMPLATE, FURNITURE_GROUP),
        (COFFER_TEMPLATE, FURNITURE_GROUP),
    ] {
        data.systems
            .npcs
            .doodad
            .templates
            .insert(id, template(id, group_id));
        data.systems.npcs.doodad.template_order.push(id);
    }
    let doodad = &mut data.systems.npcs.doodad;
    doodad
        .funcs_by_groups
        .insert(USE_GROUP, vec![use_func(USE_GROUP)]);
    doodad
        .funcs_by_groups
        .insert(UPROOT_GROUP, vec![uproot_func(UPROOT_GROUP)]);
    doodad
        .func_templates
        .entry("DoodadFuncUse")
        .or_default()
        .insert(
            USE_FUNC,
            FuncTemplate::Use(DoodadFuncUse {
                id: USE_FUNC,
                skill_id: 0,
            }),
        );
    doodad
        .func_templates
        .entry("DoodadFuncUse")
        .or_default()
        .insert(
            UPROOT_FUNC,
            FuncTemplate::Use(DoodadFuncUse {
                id: UPROOT_FUNC,
                skill_id: 0,
            }),
        );
    doodad.funcs_by_groups.insert(
        RECOVER_GROUP,
        vec![DoodadFunc {
            group_id: RECOVER_GROUP,
            func_id: RECOVER_FUNC,
            func_type: "DoodadFuncRecoverItem".into(),
            next_phase: RECOVER_NEXT_GROUP as i32,
            ..DoodadFunc::default()
        }],
    );
    doodad
        .func_templates
        .entry("DoodadFuncRecoverItem")
        .or_default()
        .insert(
            RECOVER_FUNC,
            FuncTemplate::RecoverItem(DoodadFuncRecoverItem { id: RECOVER_FUNC }),
        );
    doodad
        .funcs_by_groups
        .insert(LOOT_GROUP, vec![loot_func(LOOT_GROUP)]);
    doodad
        .func_templates
        .entry("DoodadFuncLootPack")
        .or_default()
        .insert(
            LOOT_FUNC,
            FuncTemplate::LootPack(DoodadFuncLootPack {
                id: LOOT_FUNC,
                loot_pack_id: 0,
            }),
        );
    data.systems
        .housing
        .add_template(crate::systems::housing::data::HousingTemplate {
            id: HOUSE_TEMPLATE,
            garden_radius: 8.0,
            hp: 1000,
            ..Default::default()
        });
    data.systems
        .housing
        .add_template(crate::systems::housing::data::HousingTemplate {
            id: TOWER_TEMPLATE,
            guard_tower_setting_id: 6,
            hp: 1000,
            ..Default::default()
        });
    data
}

struct Harness {
    server: GameServer,
    _clients: Vec<TestClient>,
    defender: PlayerRef,
    opponent: PlayerRef,
    friend: PlayerRef,
    house: HouseRef,
    tower: TowerIdentity,
    loot: Arc<LootRec>,
    evidence: Arc<EvidenceRec>,
}

impl Harness {
    fn new(mode: SiegeLarcenyMode, crime: bool) -> Self {
        let config = AppConfig {
            housing: HousingConfig {
                siege_larceny: mode,
                siege_larceny_crime: crime,
                ..HousingConfig::default()
            },
            ..AppConfig::default()
        };
        let server = test_server_with_config(game_data(), config);
        let loot = Arc::new(LootRec::default());
        server.systems.npcs.hooks.economy.install(loot.clone());
        let evidence = Arc::new(EvidenceRec::default());
        server.systems.npcs.hooks.skills.install(evidence.clone());

        let mut clients = Vec::new();
        let mut spawn = |id: u32, name: &str, faction: u32, obj_id: u32| {
            let mut client = TestClient::new(&server, id, 1000 + id);
            let mut character = test_character(id, name, 100.0, 100.0);
            character.unit.faction_id = faction;
            let player = spawn_player(&server, &client, character, obj_id);
            client.drain();
            clients.push(client);
            player
        };
        let defender = spawn(1, "Defender", DEFENDER_FACTION, 0x101);
        let opponent = spawn(2, "Opponent", OPPONENT_FACTION, 0x102);
        let friend = spawn(3, "Friend", DEFENDER_FACTION, 0x103);

        let world = server.world.world(0).unwrap();
        // The lodestone house only supplies the tower identity; it is not the looted land.
        let tower_house = manager::create(&server, TOWER_TEMPLATE, DEFENDER_FACTION, &world, 0, 0)
            .expect("tower house");
        {
            let mut h = tower_house.data();
            h.id = 61;
            h.unit.transform.zone_id = TEST_ZONE;
            h.unit.transform.set_xyz(140.0, 140.0, 100.0);
        }
        let (_, tower) = tower_identity(&server, &tower_house).expect("tower identity");

        let house = manager::create(&server, HOUSE_TEMPLATE, DEFENDER_FACTION, &world, 0, 0)
            .expect("garden house");
        {
            let mut h = house.data();
            h.id = 62;
            h.unit.owner_id = defender.character_id;
            h.unit.transform.zone_id = TEST_ZONE;
            h.unit.transform.set_xyz(100.0, 100.0, 100.0);
            h.current_step = -1;
            h.permission = housing_permission::PRIVATE;
        }
        assert!(server.systems.housing.manager.insert(&house));

        let authority = &server.systems.housing.dominions;
        let now = chrono::Utc::now().timestamp();
        authority.register_tower(ZONE_GROUP, tower).unwrap();
        authority
            .declare_unowned_with(ZONE_GROUP, tower, 10, now - 20, || true)
            .unwrap();
        authority
            .schedule_siege(ZONE_GROUP, 20, now - 10, now + 1000, now - 20)
            .unwrap();

        Self {
            server,
            _clients: clients,
            defender,
            opponent,
            friend,
            house,
            tower,
            loot,
            evidence,
        }
    }

    fn mark_tower_destroyed(&self) {
        self.server
            .systems
            .housing
            .dominions
            .mark_tower_destroyed(ZONE_GROUP, self.tower, chrono::Utc::now().timestamp())
            .unwrap();
    }

    fn reset_tower_destroyed(&self) {
        self.server
            .systems
            .housing
            .dominions
            .reset_tower_destroyed(ZONE_GROUP, self.tower)
            .unwrap();
    }

    fn end_siege(&self) {
        self.server
            .systems
            .housing
            .dominions
            .tick(chrono::Utc::now().timestamp() + 2000)
            .unwrap();
        let state = self
            .server
            .systems
            .housing
            .dominions
            .get(ZONE_GROUP)
            .unwrap();
        assert_eq!(
            state.siege.as_ref().map(|s| (s.status, s.outcome)),
            Some((SiegeStatus::Finished, SiegeOutcome::DefenderHeld))
        );
    }

    fn doodad(&self, template_id: u32, group: u32) -> DoodadRef {
        let mut d = Doodad::new(template_id);
        d.func_group_id = group;
        d.owner_type = doodad_owner_type::HOUSING;
        d.owner_db_id = self.house.data().id;
        d.plant_time = chrono::Utc::now().timestamp();
        d.transform.zone_id = TEST_ZONE;
        d.transform.set_xyz(102.0, 100.0, 100.0);
        phase::refresh_phase_caches(&self.server.game_data.systems.npcs.doodad, &mut d);
        let doodad = Entity::new(self.server.ids.object.get_next_id().unwrap(), d);
        let world = self.server.world.world(0).unwrap();
        world.spawn(&doodad, &self.server.view_ctx());
        doodad
    }

    fn crop(&self) -> DoodadRef {
        self.doodad(CROP_TEMPLATE, USE_GROUP)
    }

    fn uproot_crop(&self) -> DoodadRef {
        self.doodad(CROP_TEMPLATE, UPROOT_GROUP)
    }

    fn herb(&self) -> DoodadRef {
        self.doodad(HERB_TEMPLATE, USE_GROUP)
    }

    fn installation(&self, template_id: u32) -> DoodadRef {
        self.doodad(template_id, USE_GROUP)
    }

    fn recoverable(&self, template_id: u32) -> DoodadRef {
        self.doodad(template_id, RECOVER_GROUP)
    }

    /// The `CSLootOpenBag` recover branch (`RecoverItem.Execute`) for a house doodad.
    fn recover_path(&self, doodad: &DoodadRef, player: &PlayerRef) -> bool {
        let caster = WorldObject::from(player);
        interactions::execute_interaction(
            &self.server,
            wi::RECOVER_ITEM,
            &caster,
            Some(&WorldObject::from(doodad)),
            RECOVER_SKILL,
            0,
            None,
        );
        doodad.data().phase.to_next_phase
    }

    fn mature_crop(&self) -> DoodadRef {
        self.doodad(CROP_TEMPLATE, LOOT_GROUP)
    }

    fn furniture(&self) -> DoodadRef {
        self.doodad(FURNITURE_TEMPLATE, USE_GROUP)
    }

    fn coffer(&self) -> DoodadRef {
        let doodad = self.doodad(COFFER_TEMPLATE, USE_GROUP);
        doodad.data().coffer = Some(DoodadCofferState {
            capacity: 10,
            ..DoodadCofferState::default()
        });
        doodad
    }

    /// `Doodad.Use` through the real phase machine with the harvest skill.
    fn use_path(&self, doodad: &DoodadRef, player: &PlayerRef) {
        self.use_path_skill(doodad, player, SKILL);
    }

    /// `Doodad.Use` with an explicit started skill (uproot / water).
    fn use_path_skill(&self, doodad: &DoodadRef, player: &PlayerRef, skill: u32) {
        let world = self.server.world.world(0).unwrap();
        let ctx = DoodadCtx::new(&self.server, world, doodad.clone());
        let caster = WorldObject::from(player);
        phase::use_doodad(&ctx, Some(&caster), skill, 0);
    }

    /// The skill-less `CSLootOpenBag` / `LootPack` path (`Doodad.Use(..., 0)`).
    fn loot_skill_less(&self, doodad: &DoodadRef, player: &PlayerRef) {
        let world = self.server.world.world(0).unwrap();
        let ctx = DoodadCtx::new(&self.server, world, doodad.clone());
        let caster = WorldObject::from(player);
        phase::use_doodad(&ctx, Some(&caster), 0, 0);
    }
}

fn advanced(doodad: &DoodadRef) -> bool {
    doodad.data().func_group_id == USE_NEXT_GROUP
}

fn uprooted(doodad: &DoodadRef) -> bool {
    doodad.data().func_group_id == UPROOT_NEXT_GROUP
}

#[tokio::test]
async fn opponent_harvests_a_garden_crop_only_while_the_lodestone_is_held() {
    let h = Harness::new(SiegeLarcenyMode::Opponents, false);
    let before = h.crop();
    h.use_path(&before, &h.opponent);
    assert!(
        !advanced(&before),
        "no window before the lodestone is destroyed"
    );

    h.mark_tower_destroyed();
    let during = h.crop();
    h.use_path(&during, &h.opponent);
    assert!(
        advanced(&during),
        "the opponent may harvest inside the window"
    );

    h.reset_tower_destroyed();
    let after = h.crop();
    h.use_path(&after, &h.opponent);
    assert!(
        !advanced(&after),
        "the window closes when the lodestone is retaken"
    );
}

#[tokio::test]
async fn the_window_grants_produce_but_not_the_uproot() {
    let h = Harness::new(SiegeLarcenyMode::Opponents, false);
    h.mark_tower_destroyed();

    // The uproot row is perm 0, i.e. public by data; outside a house it would be usable by
    // anyone. On this house garden the window is the only thing that could grant it, and it
    // must not ("loot any crops", not pull them out).
    let crop = h.uproot_crop();
    h.use_path_skill(&crop, &h.opponent, UPROOT_SKILL);
    assert!(
        !uprooted(&crop),
        "the window must not grant the uproot to an opponent"
    );

    // The owner's own house permission still allows it.
    let own = h.uproot_crop();
    h.use_path_skill(&own, &h.defender, UPROOT_SKILL);
    assert!(uprooted(&own), "the owner may uproot their own crop");

    // The same opponent still harvests the produce of a normal crop in the window.
    let produce = h.crop();
    h.use_path(&produce, &h.opponent);
    assert!(
        advanced(&produce),
        "the window still grants the produce harvest"
    );
}

#[tokio::test]
async fn herbs_are_lootable_in_the_window() {
    let h = Harness::new(SiegeLarcenyMode::Opponents, false);
    let before = h.herb();
    h.use_path(&before, &h.opponent);
    assert!(
        !advanced(&before),
        "no window before the lodestone is destroyed"
    );

    h.mark_tower_destroyed();
    let during = h.herb();
    h.use_path(&during, &h.opponent);
    assert!(
        advanced(&during),
        "group 3 herbs and flowers are garden crops"
    );
}

#[tokio::test]
async fn the_window_closes_when_the_siege_ends() {
    let h = Harness::new(SiegeLarcenyMode::Opponents, false);
    h.mark_tower_destroyed();
    h.end_siege();
    let crop = h.crop();
    h.use_path(&crop, &h.opponent);
    assert!(!advanced(&crop));
}

#[tokio::test]
async fn same_faction_is_refused_in_opponents_mode_and_allowed_in_anyone_mode() {
    let h = Harness::new(SiegeLarcenyMode::Opponents, false);
    h.mark_tower_destroyed();
    let crop = h.crop();
    h.use_path(&crop, &h.friend);
    assert!(
        !advanced(&crop),
        "a non-opponent is refused in opponents mode"
    );
    // The owner keeps the normal permission.
    let own = h.crop();
    h.use_path(&own, &h.defender);
    assert!(advanced(&own));

    let h = Harness::new(SiegeLarcenyMode::Anyone, false);
    h.mark_tower_destroyed();
    let crop = h.crop();
    h.use_path(&crop, &h.friend);
    assert!(advanced(&crop), "any non-owner is allowed in anyone mode");
}

#[tokio::test]
async fn installations_cannot_be_recovered_in_the_window() {
    let h = Harness::new(SiegeLarcenyMode::Anyone, false);
    h.mark_tower_destroyed();
    for template in [BARN_TEMPLATE, RACK_TEMPLATE, FOAL_TEMPLATE] {
        let doodad = h.recoverable(template);
        assert!(
            !h.recover_path(&doodad, &h.opponent),
            "template {template} must not be recovered as an item"
        );
        assert!(!doodad.data().phase.deleted, "template {template} stays");
    }
    // The owner keeps the normal recover right (the window does not remove it).
    let own = h.recoverable(BARN_TEMPLATE);
    assert!(h.recover_path(&own, &h.defender));
}

#[tokio::test]
async fn installation_produce_is_harvestable_but_the_installation_is_not_removed() {
    let h = Harness::new(SiegeLarcenyMode::Anyone, false);
    h.mark_tower_destroyed();
    let barn = h.installation(BARN_TEMPLATE);
    h.use_path(&barn, &h.opponent);
    assert!(
        advanced(&barn),
        "the barn's produce can be harvested in the window"
    );
    let rack = h.recoverable(RACK_TEMPLATE);
    assert!(!h.recover_path(&rack, &h.opponent));
    assert!(
        !rack.data().phase.deleted,
        "the aging rack is not picked up as an item"
    );
}

#[tokio::test]
async fn the_window_closes_when_the_territory_is_released() {
    let h = Harness::new(SiegeLarcenyMode::Opponents, false);
    h.mark_tower_destroyed();
    h.server
        .systems
        .housing
        .dominions
        .release_expedition(10, chrono::Utc::now().timestamp());
    let crop = h.crop();
    h.use_path(&crop, &h.opponent);
    assert!(!advanced(&crop), "a released territory closes the window");
}

#[tokio::test]
async fn houses_furniture_and_coffers_stay_refused_inside_the_window() {
    let h = Harness::new(SiegeLarcenyMode::Anyone, false);
    h.mark_tower_destroyed();
    assert!(
        !housing::allowed_to_interact(&h.server, &h.house, &h.opponent),
        "the house itself keeps its permission"
    );
    let furniture = h.furniture();
    h.use_path(&furniture, &h.opponent);
    assert!(!advanced(&furniture), "house furniture is not lootable");
    let coffer = h.coffer();
    h.use_path(&coffer, &h.opponent);
    assert!(!advanced(&coffer), "coffers keep their own permission");
}

#[tokio::test]
async fn the_skill_less_loot_path_also_passes_the_window() {
    let h = Harness::new(SiegeLarcenyMode::Opponents, false);
    let before = h.mature_crop();
    h.loot_skill_less(&before, &h.opponent);
    assert!(h.loot.packs.lock().unwrap().is_empty());

    h.mark_tower_destroyed();
    let during = h.mature_crop();
    h.loot_skill_less(&during, &h.opponent);
    assert_eq!(h.loot.packs.lock().unwrap().len(), 1);
}

#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
fn real_compact_larceny_group_ids_are_the_shipped_farm_categories() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let group_name = |id: u32| -> String {
        conn.query_row(
            "SELECT name FROM doodad_groups WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(group_name(2), "벌채 - 나무");
    assert_eq!(group_name(3), "채집 - 약초");
    assert_eq!(group_name(5), "축산 - 가축");
    assert_eq!(group_name(12), "농사 - 작물");
    assert_eq!(group_name(40), "농사 - 해양 작물");
    let template_group = |id: u32| -> u32 {
        conn.query_row(
            "SELECT group_id FROM doodad_almighties WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    };
    // The shipped potato crop, apple tree, chick, herb seeds and the group-5 installations.
    assert_eq!(template_group(2259), 12);
    assert_eq!(template_group(1017), 2);
    assert_eq!(template_group(2673), 5);
    assert_eq!(template_group(2269), 3);
    assert_eq!(template_group(6035), 3);
    assert_eq!(template_group(7172), 3);
    assert_eq!(template_group(8035), 5); // barn
    assert_eq!(template_group(6997), 5); // aging rack
    assert_eq!(template_group(6180), 5); // coop
    assert_eq!(template_group(8238), 5); // merchant's pond
    let has_recover = |id: u32| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM doodad_func_groups g JOIN doodad_funcs f              ON f.doodad_func_group_id = g.id              WHERE g.doodad_almighty_id = ?1 AND f.actual_func_type = 'DoodadFuncRecoverItem'",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(has_recover(8035), 0); // barns have no item pickup
    assert_eq!(has_recover(6997), 0); // racks have no item pickup
    assert!(has_recover(5775) > 0); // the young-animal case the recover gate keeps
}

#[tokio::test]
async fn a_public_house_loot_inside_the_window_is_not_theft() {
    let h = Harness::new(SiegeLarcenyMode::Opponents, true);
    h.house.data().permission = housing_permission::PUBLIC;
    h.mark_tower_destroyed();
    let crop = h.mature_crop();
    h.loot_skill_less(&crop, &h.opponent);
    assert_eq!(h.loot.packs.lock().unwrap().len(), 1);
    assert!(
        h.evidence.criminals.lock().unwrap().is_empty(),
        "normal house permission is not theft even with the crime switch on"
    );
}

#[tokio::test]
async fn theft_evidence_is_suppressed_by_default_and_restored_by_the_switch() {
    let h = Harness::new(SiegeLarcenyMode::Opponents, false);
    h.mark_tower_destroyed();
    let crop = h.mature_crop();
    h.loot_skill_less(&crop, &h.opponent);
    assert_eq!(h.loot.packs.lock().unwrap().len(), 1);
    assert!(
        h.evidence.criminals.lock().unwrap().is_empty(),
        "no theft evidence by default"
    );

    let h = Harness::new(SiegeLarcenyMode::Opponents, true);
    h.mark_tower_destroyed();
    let crop = h.mature_crop();
    h.loot_skill_less(&crop, &h.opponent);
    assert_eq!(h.loot.packs.lock().unwrap().len(), 1);
    assert_eq!(
        *h.evidence.criminals.lock().unwrap(),
        vec![h.opponent.character_id],
        "the crime switch restores the normal evidence"
    );
}
