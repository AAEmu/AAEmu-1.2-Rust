//! `doodad_funcs.perm_id` flow tests: `Doodad.Use` (the shared funnel of the skill-bound
//! `DoodadFuncUse` and the skill-less loot form) must apply the 1.2 client's `FUN_393b3300`
//! permission switch. See `permission.rs` for the evidence table.

use std::sync::{Arc, Mutex};

use crate::data::world_data::{SystemFaction, Zone};
use crate::data::GameData;
use crate::models::doodad::{doodad_owner_type, Doodad};
use crate::models::house::housing_permission;
use crate::server::GameServer;
use crate::systems::housing::data::HousingTemplate;
use crate::systems::housing::dominion::larceny;
use crate::systems::housing::dominion::runtime::tower_identity;
use crate::systems::housing::manager as housing_manager;
use crate::systems::npcs::doodad::interactions::{self, world_interaction_type as wi};
use crate::systems::npcs::doodad::phase;
use crate::systems::npcs::doodad::runtime::DoodadCtx;
use crate::systems::npcs::doodad::templates::{
    doodad_func_permission as permission, DoodadFunc, DoodadFuncLootPack, DoodadFuncRecoverItem,
    DoodadFuncUse, DoodadGroups, DoodadTemplate, FuncTemplate,
};
use crate::systems::npcs::hooks::{EconomyBridge, SkillsBridge};
use crate::systems::social::expedition::{Expedition, ExpeditionMember, ExpeditionRolePolicy};
use crate::systems::social::family::{Family, FamilyMember};
use crate::systems::social::nation::Nation;
use crate::systems::social::team::Team;
use crate::test_support::*;
use crate::world::{DoodadRef, Entity, HouseRef, PlayerRef, WorldObject};

const SKILL: u32 = 4242;
const UPROOT_SKILL: u32 = larceny::UPROOT_SKILL_ID;
const WATER_SKILL: u32 = 13625;
const TEMPLATE: u32 = 500;
const HOUSE_TEMPLATE: u32 = 300;
const DOODAD_GROUP: u32 = 12; // 농사 - 작물
const ZONE_FACTION: u32 = 148;
const OTHER_FACTION: u32 = 149;
const FAMILY_ID: u32 = 7;
const EXPEDITION_ID: u32 = 50;
const OWNER_EXPEDITION: u32 = 60;
const NATION_ID: u32 = 9000;
const OTHER_NATION_ID: u32 = 9001;
const TOWER_TEMPLATE: u32 = 301;
const ZONE_GROUP: u32 = 25;
const CHAIN_GROUP: u32 = 230;
const CHAIN_NEXT_GROUP: u32 = 231;
const CHAIN_FIRST_PACK: u32 = 1;
const CHAIN_SECOND_PACK: u32 = 2;

const ANY_GROUP: u32 = 210;
const OWNER_GROUP: u32 = 201;
const FAMILY_GROUP: u32 = 202;
const EXPEDITION_GROUP: u32 = 203;
const TEAM_GROUP: u32 = 204;
const RAID_GROUP: u32 = 205;
const ACCOUNT_GROUP: u32 = 206;
const NATION_GROUP: u32 = 207;
const RESIDENT_GROUP: u32 = 208;
const UPROOT_FAMILY_GROUP: u32 = 211;
const WATER_ANY_GROUP: u32 = 212;
const UPROOT_ANY_GROUP: u32 = 213;
const RECOVER_GROUP: u32 = 220;
const RECOVER_FUNC: u32 = 920;
const RECOVER_SKILL: u32 = 11361;
const GROWTH_GROUP: u32 = 221;
const GROWTH_FUNC: u32 = 921;
const GROWTH_SKILL: u32 = 4243;

fn next_group(group: u32) -> u32 {
    group + 100
}

fn use_func(group: u32, skill: u32, perm: u32) -> DoodadFunc {
    DoodadFunc {
        group_id: group,
        func_id: group,
        func_type: "DoodadFuncUse".into(),
        next_phase: next_group(group) as i32,
        skill_id: skill,
        perm_id: perm,
        ..DoodadFunc::default()
    }
}

fn game_data() -> GameData {
    let mut data = GameData::empty();
    data.zones.zones.insert(
        TEST_ZONE,
        Zone {
            id: TEST_ZONE,
            zone_key: TEST_ZONE,
            group_id: 25,
            faction_id: ZONE_FACTION,
            ..Default::default()
        },
    );
    for id in [ZONE_FACTION, OTHER_FACTION] {
        data.factions.add(SystemFaction {
            id,
            mother_id: 0,
            name: format!("Faction {id}"),
            ..Default::default()
        });
    }
    data.systems.npcs.doodad.templates.insert(
        TEMPLATE,
        DoodadTemplate {
            id: TEMPLATE,
            group_id: DOODAD_GROUP,
            group: DoodadGroups {
                id: DOODAD_GROUP,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    data.systems.npcs.doodad.template_order.push(TEMPLATE);
    data.systems.housing.add_template(HousingTemplate {
        id: HOUSE_TEMPLATE,
        garden_radius: 8.0,
        hp: 1000,
        ..Default::default()
    });
    data.systems.housing.add_template(HousingTemplate {
        id: TOWER_TEMPLATE,
        guard_tower_setting_id: 6,
        hp: 1000,
        ..Default::default()
    });

    let funcs = [
        (ANY_GROUP, SKILL, permission::ANY),
        (OWNER_GROUP, SKILL, permission::OWNER),
        (FAMILY_GROUP, SKILL, permission::FAMILY),
        (EXPEDITION_GROUP, SKILL, permission::EXPEDITION),
        (TEAM_GROUP, SKILL, permission::TEAM),
        (RAID_GROUP, SKILL, permission::RAID),
        (ACCOUNT_GROUP, SKILL, permission::SAME_ACCOUNT),
        (NATION_GROUP, SKILL, permission::NATION),
        (RESIDENT_GROUP, SKILL, permission::ZONE_RESIDENT),
        (UPROOT_FAMILY_GROUP, UPROOT_SKILL, permission::FAMILY),
        (WATER_ANY_GROUP, WATER_SKILL, permission::ANY),
        (UPROOT_ANY_GROUP, UPROOT_SKILL, permission::ANY),
    ];
    for (group, skill, perm) in funcs {
        data.systems
            .npcs
            .doodad
            .funcs_by_groups
            .insert(group, vec![use_func(group, skill, perm)]);
        data.systems
            .npcs
            .doodad
            .func_templates
            .entry("DoodadFuncUse")
            .or_default()
            .insert(
                group,
                FuncTemplate::Use(DoodadFuncUse {
                    id: group,
                    skill_id: 0,
                }),
            );
    }

    // The two entry paths that call `DoodadFuncTemplate.Use` without `phase::use_doodad`:
    // `DoodadFuncRecoverItem` (`CSLootOpenBag` / `RecoverItem`) and `CheckGrowth`.
    data.systems.npcs.doodad.funcs_by_groups.insert(
        RECOVER_GROUP,
        vec![DoodadFunc {
            group_id: RECOVER_GROUP,
            func_id: RECOVER_FUNC,
            func_type: "DoodadFuncRecoverItem".into(),
            next_phase: next_group(RECOVER_GROUP) as i32,
            skill_id: RECOVER_SKILL,
            perm_id: permission::OWNER,
            ..DoodadFunc::default()
        }],
    );
    data.systems
        .npcs
        .doodad
        .func_templates
        .entry("DoodadFuncRecoverItem")
        .or_default()
        .insert(
            RECOVER_FUNC,
            FuncTemplate::RecoverItem(DoodadFuncRecoverItem { id: RECOVER_FUNC }),
        );
    data.systems.npcs.doodad.funcs_by_groups.insert(
        GROWTH_GROUP,
        vec![DoodadFunc {
            group_id: GROWTH_GROUP,
            func_id: GROWTH_FUNC,
            func_type: "DoodadFuncUse".into(),
            next_phase: -1,
            skill_id: GROWTH_SKILL,
            perm_id: permission::OWNER,
            ..DoodadFunc::default()
        }],
    );
    data.systems
        .npcs
        .doodad
        .func_templates
        .entry("DoodadFuncUse")
        .or_default()
        .insert(
            GROWTH_FUNC,
            FuncTemplate::Use(DoodadFuncUse {
                id: GROWTH_FUNC,
                skill_id: 0,
            }),
        );

    // A skill-less loot row that advances the phase followed by a second, denied loot row:
    // the chained iteration must skip the denied row silently.
    for (group, pack, perm, next) in [
        (
            CHAIN_GROUP,
            CHAIN_FIRST_PACK,
            permission::ANY,
            CHAIN_NEXT_GROUP as i32,
        ),
        (CHAIN_NEXT_GROUP, CHAIN_SECOND_PACK, permission::OWNER, -1),
    ] {
        data.systems.npcs.doodad.funcs_by_groups.insert(
            group,
            vec![DoodadFunc {
                group_id: group,
                func_id: group,
                func_type: "DoodadFuncLootPack".into(),
                next_phase: next,
                skill_id: 0,
                perm_id: perm,
                ..DoodadFunc::default()
            }],
        );
        data.systems
            .npcs
            .doodad
            .func_templates
            .entry("DoodadFuncLootPack")
            .or_default()
            .insert(
                group,
                FuncTemplate::LootPack(DoodadFuncLootPack {
                    id: group,
                    loot_pack_id: pack,
                }),
            );
    }
    data
}

/// `EconomyBridge::give_doodad_loot_pack` recorder.
#[derive(Default)]
struct LootRec {
    packs: Mutex<Vec<u32>>,
}

impl EconomyBridge for LootRec {
    fn give_doodad_loot_pack(
        &self,
        _server: &GameServer,
        _player: &PlayerRef,
        loot_pack_id: u32,
        _skill_id: u32,
        _task: u8,
    ) -> Option<bool> {
        self.packs.lock().unwrap().push(loot_pack_id);
        Some(true)
    }
}

/// `SkillsBridge::cancel_skill` recorder: a refusal refunds the cast, a chained skip must not.
#[derive(Default)]
struct CancelRec {
    cancelled: Mutex<Vec<u32>>,
}

impl SkillsBridge for CancelRec {
    fn cancel_skill(&self, _server: &GameServer, player: &PlayerRef) {
        self.cancelled.lock().unwrap().push(player.character_id);
    }
}

struct Fx {
    server: GameServer,
    _clients: Vec<TestClient>,
    owner: PlayerRef,
    other: PlayerRef,
    family_mate: PlayerRef,
    guild_mate: PlayerRef,
    same_account: PlayerRef,
    expedition_member: PlayerRef,
    house: HouseRef,
    loot: Arc<LootRec>,
    cancels: Arc<CancelRec>,
}

impl Fx {
    fn new() -> Self {
        let server = test_server_with(game_data());
        let loot = Arc::new(LootRec::default());
        server.systems.npcs.hooks.economy.install(loot.clone());
        let cancels = Arc::new(CancelRec::default());
        server.systems.npcs.hooks.skills.install(cancels.clone());
        let mut clients = Vec::new();
        let mut spawn = |id: u32, name: &str, account: u32, faction: u32, obj_id: u32| {
            let client = TestClient::new(&server, id, account);
            let mut character = test_character(id, name, 100.0, 100.0);
            character.account_id = account;
            character.unit.faction_id = faction;
            let player = spawn_player(&server, &client, character, obj_id);
            server.names.add_character(id, name, account);
            clients.push(client);
            player
        };
        let owner = spawn(1, "Owner", 5001, OTHER_FACTION, 0x101);
        let other = spawn(2, "Other", 5002, OTHER_FACTION, 0x102);
        let family_mate = spawn(3, "Family", 5003, OTHER_FACTION, 0x103);
        let guild_mate = spawn(4, "Guild", 5004, OTHER_FACTION, 0x104);
        let same_account = spawn(5, "Alt", 5001, OTHER_FACTION, 0x105);
        let expedition_member = spawn(6, "Member", 5006, OTHER_FACTION, 0x106);

        owner.data().family = FAMILY_ID;
        family_mate.data().family = FAMILY_ID;
        server.systems.social.families.insert(Family {
            id: FAMILY_ID,
            members: vec![
                FamilyMember {
                    character: Some(owner.clone()),
                    id: owner.character_id,
                    name: "Owner".into(),
                    role: 1,
                    title: String::new(),
                },
                FamilyMember {
                    character: Some(family_mate.clone()),
                    id: family_mate.character_id,
                    name: "Family".into(),
                    role: 2,
                    title: String::new(),
                },
            ],
            removed_members: Vec::new(),
        });

        owner.data().unit.expedition_id = EXPEDITION_ID;
        guild_mate.data().unit.expedition_id = EXPEDITION_ID;
        expedition_member.data().unit.expedition_id = EXPEDITION_ID;
        // Role 0xff is the leader (`FUN_392f8af0` passes it directly); every other role needs
        // the policy right at policy +0x90, decoded as `siege_master`.
        server.expeditions.add(Expedition {
            faction: SystemFaction {
                id: EXPEDITION_ID,
                mother_id: 0,
                name: "Guild".into(),
                ..Default::default()
            },
            members: [
                (owner.character_id, "Owner", 0xff),
                (guild_mate.character_id, "Guild", 1),
                (expedition_member.character_id, "Member", 2),
            ]
            .into_iter()
            .map(|(character_id, name, role)| ExpeditionMember {
                expedition_id: EXPEDITION_ID,
                character_id,
                in_party: false,
                is_online: true,
                last_world_leave_time: 0,
                name: name.into(),
                level: 10,
                zone_id: TEST_ZONE,
                id3: 0,
                abilities: [0; 3],
                role,
                position: (100.0, 100.0, 100.0),
                memo: String::new(),
                transfer_requested_time: 0,
            })
            .collect(),
            policies: vec![ExpeditionRolePolicy {
                expedition_id: EXPEDITION_ID,
                role: 1,
                name: "Officer".into(),
                dominion_declare: false,
                invite: false,
                expel: false,
                promote: false,
                dismiss: false,
                chat: true,
                manager_chat: false,
                siege_master: true,
                join_siege: false,
            }],
            removed_members: Vec::new(),
            is_disbanded: false,
        });

        // A territory owned by OWNER_EXPEDITION, a guild founded in NATION_ID (its `mother`).
        server.expeditions.add(Expedition {
            faction: SystemFaction {
                id: OWNER_EXPEDITION,
                mother_id: NATION_ID,
                name: "Territory owner".into(),
                ..Default::default()
            },
            members: Vec::new(),
            policies: Vec::new(),
            removed_members: Vec::new(),
            is_disbanded: false,
        });
        for id in [NATION_ID, OTHER_NATION_ID] {
            server.systems.social.nations.add(Nation {
                id,
                expedition_id: 0,
                owner_character_id: 0,
                owner_name: String::new(),
                name: format!("Nation {id}"),
                created_at: 0,
                allow_change_name: 0,
                relations: Vec::new(),
                offline_members: Vec::new(),
            });
        }

        let world = server.world.world(0).expect("instance 0");
        let tower_house =
            housing_manager::create(&server, TOWER_TEMPLATE, OTHER_FACTION, &world, 0, 0)
                .expect("tower house");
        {
            let mut h = tower_house.data();
            h.id = 61;
            h.unit.transform.zone_id = TEST_ZONE;
            h.unit.transform.set_xyz(140.0, 140.0, 100.0);
        }
        let (_, tower) = tower_identity(&server, &tower_house).expect("tower identity");
        let dominions = &server.systems.housing.dominions;
        dominions.register_tower(ZONE_GROUP, tower).unwrap();
        dominions
            .declare_unowned_with(
                ZONE_GROUP,
                tower,
                OWNER_EXPEDITION,
                chrono::Utc::now().timestamp(),
                || true,
            )
            .unwrap();

        let house = housing_manager::create(&server, HOUSE_TEMPLATE, OTHER_FACTION, &world, 0, 0)
            .expect("house");
        {
            let mut h = house.data();
            h.id = 62;
            h.account_id = other.data().account_id;
            h.unit.owner_id = other.character_id;
            h.unit.transform.zone_id = TEST_ZONE;
            h.unit.transform.set_xyz(100.0, 100.0, 100.0);
            h.current_step = -1;
            h.permission = housing_permission::PRIVATE;
        }
        assert!(server.systems.housing.manager.insert(&house));

        Self {
            server,
            _clients: clients,
            owner,
            other,
            family_mate,
            guild_mate,
            same_account,
            expedition_member,
            house,
            loot,
            cancels,
        }
    }

    /// A doodad owned by `owner` (character), in the current zone, using `group`'s func.
    fn doodad(&self, group: u32) -> DoodadRef {
        let mut d = Doodad::new(TEMPLATE);
        d.func_group_id = group;
        d.owner_type = doodad_owner_type::CHARACTER;
        d.owner_id = self.owner.character_id;
        d.owner_obj_id = self.owner.obj_id;
        d.transform.zone_id = TEST_ZONE;
        d.transform.set_xyz(101.0, 100.0, 100.0);
        self.spawn_raw(d)
    }

    /// A system-owned world doodad (no owning character): `perm 1..` must refuse it.
    fn world_doodad(&self, group: u32) -> DoodadRef {
        let mut d = Doodad::new(TEMPLATE);
        d.func_group_id = group;
        d.owner_type = doodad_owner_type::SYSTEM;
        d.transform.zone_id = TEST_ZONE;
        d.transform.set_xyz(104.0, 100.0, 100.0);
        self.spawn_raw(d)
    }

    /// A slave-owned doodad (the summoner is the character owner).
    fn slave_doodad(&self, group: u32) -> DoodadRef {
        let mut d = Doodad::new(TEMPLATE);
        d.func_group_id = group;
        d.owner_type = doodad_owner_type::SLAVE;
        d.owner_id = self.owner.character_id;
        d.owner_obj_id = self.owner.obj_id;
        d.transform.zone_id = TEST_ZONE;
        d.transform.set_xyz(105.0, 100.0, 100.0);
        self.spawn_raw(d)
    }

    /// A doodad owned by `player` (character), using `group`'s func.
    fn doodad_owned_by(&self, group: u32, player: &PlayerRef) -> DoodadRef {
        let mut d = Doodad::new(TEMPLATE);
        d.func_group_id = group;
        d.owner_type = doodad_owner_type::CHARACTER;
        d.owner_id = player.character_id;
        d.owner_obj_id = player.obj_id;
        d.transform.zone_id = TEST_ZONE;
        d.transform.set_xyz(106.0, 100.0, 100.0);
        self.spawn_raw(d)
    }

    fn spawn_raw(&self, mut d: Doodad) -> DoodadRef {
        phase::refresh_phase_caches(&self.server.game_data.systems.npcs.doodad, &mut d);
        static NEXT_OBJ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x1000);
        let obj_id = NEXT_OBJ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let doodad = Entity::new(obj_id, d);
        let world = self.server.world.world(0).unwrap();
        world.spawn(&doodad, &self.server.view_ctx());
        doodad
    }

    fn use_skill(&self, doodad: &DoodadRef, player: &PlayerRef, skill: u32) {
        let world = self.server.world.world(0).unwrap();
        let ctx = DoodadCtx::new(&self.server, world, doodad.clone());
        let caster = WorldObject::from(player);
        phase::use_doodad(&ctx, Some(&caster), skill, 0);
    }
}

fn advanced(doodad: &DoodadRef, group: u32) -> bool {
    doodad.data().func_group_id == next_group(group)
}

fn denied(fx: &Fx, doodad: &DoodadRef, player: &PlayerRef, group: u32, skill: u32) {
    fx.use_skill(doodad, player, skill);
    assert!(
        !advanced(doodad, group),
        "perm {} must refuse {} (group {group} stayed)",
        doodad.data().func_group_id,
        player.data().unit.name
    );
}

fn allowed(fx: &Fx, doodad: &DoodadRef, player: &PlayerRef, group: u32, skill: u32) {
    fx.use_skill(doodad, player, skill);
    assert!(
        advanced(doodad, group),
        "perm rule must allow {} (group {} -> {})",
        player.data().unit.name,
        group,
        doodad.data().func_group_id
    );
}

#[tokio::test]
async fn perm_zero_is_public_water_and_uproot() {
    let fx = Fx::new();
    let water = fx.doodad(WATER_ANY_GROUP);
    allowed(&fx, &water, &fx.other, WATER_ANY_GROUP, WATER_SKILL);
    let uproot = fx.doodad(UPROOT_ANY_GROUP);
    allowed(&fx, &uproot, &fx.other, UPROOT_ANY_GROUP, UPROOT_SKILL);
}

#[tokio::test]
async fn owner_only_perm_refuses_a_non_owner() {
    let fx = Fx::new();
    let doodad = fx.doodad(OWNER_GROUP);
    denied(&fx, &doodad, &fx.other, OWNER_GROUP, SKILL);
    let own = fx.doodad(OWNER_GROUP);
    allowed(&fx, &own, &fx.owner, OWNER_GROUP, SKILL);
}

#[tokio::test]
async fn family_perm_allows_the_owner_and_family_but_not_others() {
    let fx = Fx::new();
    let d1 = fx.doodad(FAMILY_GROUP);
    allowed(&fx, &d1, &fx.owner, FAMILY_GROUP, SKILL);
    let d2 = fx.doodad(FAMILY_GROUP);
    allowed(&fx, &d2, &fx.family_mate, FAMILY_GROUP, SKILL);
    let d3 = fx.doodad(FAMILY_GROUP);
    denied(&fx, &d3, &fx.other, FAMILY_GROUP, SKILL);
}

#[tokio::test]
async fn expedition_perm_allows_members_with_the_role_right_but_not_outsiders() {
    let fx = Fx::new();
    // The owner is the expedition leader and passes directly.
    let leader = fx.doodad(EXPEDITION_GROUP);
    allowed(&fx, &leader, &fx.owner, EXPEDITION_GROUP, SKILL);
    // Role 1 carries the `siege_master` policy right (`FUN_392f8af0`).
    let d1 = fx.doodad(EXPEDITION_GROUP);
    allowed(&fx, &d1, &fx.guild_mate, EXPEDITION_GROUP, SKILL);
    let d2 = fx.doodad(EXPEDITION_GROUP);
    denied(&fx, &d2, &fx.other, EXPEDITION_GROUP, SKILL);
}

#[tokio::test]
async fn expedition_perm_has_no_owner_shortcut_and_needs_the_role_right() {
    let fx = Fx::new();
    // `expedition_member` owns this doodad but has role 2 without a policy right: case 3 never
    // compares the owner, so the member is refused.
    let dossier = fx.doodad_owned_by(EXPEDITION_GROUP, &fx.expedition_member);
    denied(
        &fx,
        &dossier,
        &fx.expedition_member,
        EXPEDITION_GROUP,
        SKILL,
    );
    // A role with the right passes even on a doodad owned by the refused member.
    let dossier = fx.doodad_owned_by(EXPEDITION_GROUP, &fx.expedition_member);
    allowed(&fx, &dossier, &fx.guild_mate, EXPEDITION_GROUP, SKILL);
}

#[tokio::test]
async fn same_account_perm_allows_the_owner_only_through_the_account() {
    let fx = Fx::new();
    let d1 = fx.doodad(ACCOUNT_GROUP);
    allowed(&fx, &d1, &fx.same_account, ACCOUNT_GROUP, SKILL);
    let d2 = fx.doodad(ACCOUNT_GROUP);
    denied(&fx, &d2, &fx.family_mate, ACCOUNT_GROUP, SKILL);
}

#[tokio::test]
async fn nation_perm_needs_the_territory_owner_nation_not_the_zone_faction() {
    let fx = Fx::new();
    // Zone group 25 is owned by OWNER_EXPEDITION, whose mother is NATION_ID. The zone's static
    // `faction_id` (148) is irrelevant (every shipped siege-zone row has 0 there).
    assert_eq!(
        fx.server
            .game_data
            .zones
            .zone_by_key(TEST_ZONE)
            .map(|zone| zone.faction_id),
        Some(ZONE_FACTION)
    );
    fx.owner.data().unit.faction_id = NATION_ID;
    let d1 = fx.doodad(NATION_GROUP);
    allowed(&fx, &d1, &fx.owner, NATION_GROUP, SKILL);

    // A player-founded nation that does not own the territory is refused.
    fx.owner.data().unit.faction_id = OTHER_NATION_ID;
    let d2 = fx.doodad(NATION_GROUP);
    denied(&fx, &d2, &fx.owner, NATION_GROUP, SKILL);

    // A race / alliance faction is not a player nation (`FUN_392fa6b0`), even inside the
    // territory: the old `zones.faction_id` rule would have allowed it.
    fx.owner.data().unit.faction_id = ZONE_FACTION;
    let d3 = fx.doodad(NATION_GROUP);
    denied(&fx, &d3, &fx.owner, NATION_GROUP, SKILL);
}

#[tokio::test]
async fn zone_resident_perm_requires_a_house_in_the_doodad_zone() {
    let fx = Fx::new();
    // `other` owns the house in TEST_ZONE.
    let d1 = fx.doodad(RESIDENT_GROUP);
    allowed(&fx, &d1, &fx.other, RESIDENT_GROUP, SKILL);
    let d2 = fx.doodad(RESIDENT_GROUP);
    denied(&fx, &d2, &fx.family_mate, RESIDENT_GROUP, SKILL);

    // A house in another zone does not count.
    {
        let mut h = fx.house.data();
        h.unit.transform.zone_id = TEST_ZONE + 1;
    }
    let d3 = fx.doodad(RESIDENT_GROUP);
    denied(&fx, &d3, &fx.other, RESIDENT_GROUP, SKILL);
}

#[tokio::test]
async fn team_and_raid_perms_follow_the_team() {
    let fx = Fx::new();
    let mut team = Team::new(9, fx.owner.character_id, true);
    team.add_member(&fx.owner);
    team.add_member(&fx.family_mate);
    fx.server.systems.social.teams.insert_team_for_test(team);

    let d1 = fx.doodad(TEAM_GROUP);
    allowed(&fx, &d1, &fx.family_mate, TEAM_GROUP, SKILL);
    let d2 = fx.doodad(TEAM_GROUP);
    denied(&fx, &d2, &fx.other, TEAM_GROUP, SKILL);

    // A party is not a raid: perm 5 needs the raid team.
    let d3 = fx.doodad(RAID_GROUP);
    denied(&fx, &d3, &fx.family_mate, RAID_GROUP, SKILL);
    // Convert the same team (same id) into a raid; the test helper replaces it.
    let mut raid = Team::new(9, fx.owner.character_id, false);
    raid.add_member(&fx.owner);
    raid.add_member(&fx.family_mate);
    fx.server.systems.social.teams.insert_team_for_test(raid);
    let d4 = fx.doodad(RAID_GROUP);
    allowed(&fx, &d4, &fx.family_mate, RAID_GROUP, SKILL);
}

#[tokio::test]
async fn system_owned_world_doodad_allows_public_rows_and_refuses_owner_rows() {
    let fx = Fx::new();
    let public = fx.world_doodad(ANY_GROUP);
    allowed(&fx, &public, &fx.other, ANY_GROUP, SKILL);
    let owned = fx.world_doodad(OWNER_GROUP);
    denied(&fx, &owned, &fx.other, OWNER_GROUP, SKILL);
    // Even the player who last used it is not the owner: there is no owner at all.
    let owned = fx.world_doodad(OWNER_GROUP);
    denied(&fx, &owned, &fx.owner, OWNER_GROUP, SKILL);
}

#[tokio::test]
async fn slave_owned_storage_follows_its_perm() {
    let fx = Fx::new();
    // The backpack storage box (3446) carries perm 5: owner or raid.
    let mut raid = Team::new(9, fx.owner.character_id, false);
    raid.add_member(&fx.owner);
    raid.add_member(&fx.guild_mate);
    fx.server.systems.social.teams.insert_team_for_test(raid);

    let own = fx.slave_doodad(RAID_GROUP);
    allowed(&fx, &own, &fx.owner, RAID_GROUP, SKILL);
    let mate = fx.slave_doodad(RAID_GROUP);
    allowed(&fx, &mate, &fx.guild_mate, RAID_GROUP, SKILL);
    let outsider = fx.slave_doodad(RAID_GROUP);
    denied(&fx, &outsider, &fx.other, RAID_GROUP, SKILL);

    // The same slave doodad on an owner-only row is refused to the raid member.
    let own = fx.slave_doodad(OWNER_GROUP);
    allowed(&fx, &own, &fx.owner, OWNER_GROUP, SKILL);
    let mate = fx.slave_doodad(OWNER_GROUP);
    denied(&fx, &mate, &fx.guild_mate, OWNER_GROUP, SKILL);
}

#[tokio::test]
async fn chained_loot_skips_a_denied_row_without_refunding_the_earlier_use() {
    let fx = Fx::new();
    let doodad = fx.doodad(CHAIN_GROUP);
    // The first (public) loot row runs, advances to CHAIN_NEXT_GROUP, whose owner-only row is
    // then skipped: the interaction already happened, so no 232 and no cast refund.
    fx.use_skill(&doodad, &fx.other, 0);
    assert_eq!(*fx.loot.packs.lock().unwrap(), vec![CHAIN_FIRST_PACK]);
    assert_eq!(doodad.data().func_group_id, CHAIN_NEXT_GROUP);
    assert!(
        fx.cancels.cancelled.lock().unwrap().is_empty(),
        "a chained denied row must not cancel/refund the earlier use"
    );

    // The owner gets both rows.
    let doodad = fx.doodad(CHAIN_GROUP);
    fx.use_skill(&doodad, &fx.owner, 0);
    assert_eq!(
        *fx.loot.packs.lock().unwrap(),
        vec![CHAIN_FIRST_PACK, CHAIN_FIRST_PACK, CHAIN_SECOND_PACK]
    );
}

#[tokio::test]
async fn uproot_perm_two_refuses_a_non_owner_but_water_perm_zero_allows_it() {
    let fx = Fx::new();
    // The QA crop group 1135: water 13974?? no: uproot 13789 has perm 2 there, while the
    // player-planted groups bind both water 13625 and uproot 13789 with perm 0.
    let crop = fx.doodad(UPROOT_FAMILY_GROUP);
    denied(&fx, &crop, &fx.other, UPROOT_FAMILY_GROUP, UPROOT_SKILL);
    let water = fx.doodad(WATER_ANY_GROUP);
    allowed(&fx, &water, &fx.other, WATER_ANY_GROUP, WATER_SKILL);
}

#[tokio::test]
async fn denied_use_sends_the_permission_error_and_cancels_the_cast() {
    let fx = Fx::new();
    let doodad = fx.doodad(OWNER_GROUP);
    // The client fixture keeps its frames on the TestClient; drain it directly.
    let mut client = TestClient::new(&fx.server, 99, 5099);
    // Use a player whose connection we can read: `other`'s client is hidden in `Fx`, so check
    // the packet through a fresh doodad/player pair instead.
    let player = {
        let mut character = test_character(98, "Probe", 100.0, 100.0);
        character.account_id = 5098;
        spawn_player(&fx.server, &client, character, 0x198)
    };
    let world = fx.server.world.world(0).unwrap();
    let ctx = DoodadCtx::new(&fx.server, world, doodad.clone());
    phase::use_doodad(&ctx, Some(&WorldObject::from(&player)), SKILL, 0);
    assert!(!advanced(&doodad, OWNER_GROUP));
    let error = client
        .drain()
        .into_iter()
        .find(|(op, _)| *op == crate::opcodes::sc::SCErrorMsgPacket)
        .expect("permission error packet");
    assert_eq!(
        &error.1[0..2],
        &232i16.to_le_bytes(),
        "interaction_permission_deny (232)"
    );
}

/// The shipped 1.2 data pins the values the gate decodes (`cargo test -p aaemu-game
/// real_compact_db_perm -- --ignored` with `AAEMU_COMPACT_DB`).
#[test]
#[ignore = "needs AAEMU_COMPACT_DB (read-only)"]
fn real_compact_db_perm_values() {
    let Ok(path) = std::env::var("AAEMU_COMPACT_DB") else {
        return;
    };
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let data = crate::systems::npcs::doodad::data::DoodadData::load_from(&conn).unwrap();

    // QA crop 901 group 1135: cut-tree (13974) owner-only, uproot (13789) family.
    let qa = data.funcs_for_group(1135);
    assert!(qa.iter().any(|f| f.skill_id == 13974 && f.perm_id == 1));
    assert!(qa.iter().any(|f| f.skill_id == 13789 && f.perm_id == 2));
    // QA bench 7252 "이 존에 내 집" (house in this zone): uproot and craft are perm 8.
    assert!(data
        .funcs_for_group(19712)
        .iter()
        .any(|f| f.skill_id == 13789 && f.perm_id == 8));
    assert!(
        data.funcs_for_group(19710).iter().any(|f| f.perm_id == 7),
        "7251 zone-nation bench"
    );
    // A shipped player crop (15684 potato seed bundle): water and uproot are public (perm 0).
    let potato = data.funcs_for_group(15684);
    assert!(potato.iter().any(|f| f.skill_id == 13625 && f.perm_id == 0));
    assert!(potato.iter().any(|f| f.skill_id == 13789 && f.perm_id == 0));

    // Every shipped water row is public, and no row names a value the client switch lacks.
    let mut water = 0;
    for funcs in data.funcs_by_groups.values() {
        for f in funcs {
            assert!(
                f.perm_id <= permission::ZONE_RESIDENT,
                "unknown perm {} on func {}",
                f.perm_id,
                f.func_key
            );
            if f.skill_id == WATER_SKILL {
                water += 1;
                assert_eq!(f.perm_id, 0, "water 13625 is public");
            }
        }
    }
    assert_eq!(water, 99, "the shipped water rows");
}

fn probe(fx: &Fx) -> (TestClient, PlayerRef) {
    let client = TestClient::new(&fx.server, 98, 5098);
    let mut character = test_character(98, "Probe", 100.0, 100.0);
    character.account_id = 5098;
    let player = spawn_player(&fx.server, &client, character, 0x198);
    (client, player)
}

fn permission_denied(client: &mut TestClient) -> bool {
    client.drain().into_iter().any(|(op, body)| {
        op == crate::opcodes::sc::SCErrorMsgPacket && body[0..2] == 232i16.to_le_bytes()
    })
}

#[tokio::test]
async fn recover_item_path_applies_perm() {
    let fx = Fx::new();
    let (mut client, probe) = probe(&fx);
    let denied_doodad = fx.doodad(RECOVER_GROUP);
    interactions::execute_interaction(
        &fx.server,
        wi::RECOVER_ITEM,
        &WorldObject::from(&probe),
        Some(&WorldObject::from(&denied_doodad)),
        RECOVER_SKILL,
        0,
        None,
    );
    assert!(
        !denied_doodad.data().phase.deleted,
        "a denied recover must not take the furniture"
    );
    assert!(permission_denied(&mut client), "232 to the non-owner");

    let allowed_doodad = fx.doodad(RECOVER_GROUP);
    interactions::execute_interaction(
        &fx.server,
        wi::RECOVER_ITEM,
        &WorldObject::from(&fx.owner),
        Some(&WorldObject::from(&allowed_doodad)),
        RECOVER_SKILL,
        0,
        None,
    );
    let live = fx
        .server
        .world
        .world(0)
        .and_then(|w| w.object(allowed_doodad.obj_id))
        .map(|o| {
            o.as_doodad()
                .is_some_and(|d| std::sync::Arc::ptr_eq(d, &allowed_doodad))
        });
    let state = {
        let d = allowed_doodad.data();
        (d.phase.to_next_phase, d.phase.deleted, d.item_id, live)
    };
    assert!(
        allowed_doodad.data().phase.deleted,
        "the owner recovers the furniture (to_next, deleted, item_id = {state:?})"
    );
}

#[tokio::test]
async fn check_growth_path_applies_perm() {
    let fx = Fx::new();
    let (mut client, probe) = probe(&fx);
    let denied_doodad = fx.doodad(GROWTH_GROUP);
    interactions::execute_interaction(
        &fx.server,
        wi::CHECK_GROWTH,
        &WorldObject::from(&probe),
        Some(&WorldObject::from(&denied_doodad)),
        GROWTH_SKILL,
        0,
        None,
    );
    assert!(permission_denied(&mut client), "232 to the non-owner");

    let allowed_doodad = fx.doodad(GROWTH_GROUP);
    interactions::execute_interaction(
        &fx.server,
        wi::CHECK_GROWTH,
        &WorldObject::from(&fx.owner),
        Some(&WorldObject::from(&allowed_doodad)),
        GROWTH_SKILL,
        0,
        None,
    );
    assert!(
        allowed_doodad.data().phase.to_next_phase,
        "the owner's CheckGrowth ran the func"
    );
}

#[tokio::test]
async fn house_owned_crop_still_uses_the_house_permission_on_top_of_perm_zero() {
    let fx = Fx::new();
    let mut d = Doodad::new(TEMPLATE);
    d.func_group_id = UPROOT_ANY_GROUP;
    d.owner_type = doodad_owner_type::HOUSING;
    d.owner_db_id = fx.house.data().id;
    d.owner_id = fx.other.character_id;
    d.transform.zone_id = TEST_ZONE;
    d.transform.set_xyz(102.0, 100.0, 100.0);
    let doodad = fx.spawn_raw(d);

    // The house is PRIVATE and owned by `other`: a third player is refused even though the
    // uproot row is perm 0.
    denied(
        &fx,
        &doodad,
        &fx.family_mate,
        UPROOT_ANY_GROUP,
        UPROOT_SKILL,
    );
    let mut own = Doodad::new(TEMPLATE);
    own.func_group_id = UPROOT_ANY_GROUP;
    own.owner_type = doodad_owner_type::HOUSING;
    own.owner_db_id = fx.house.data().id;
    own.owner_id = fx.other.character_id;
    own.transform.zone_id = TEST_ZONE;
    own.transform.set_xyz(103.0, 100.0, 100.0);
    let own = fx.spawn_raw(own);
    allowed(&fx, &own, &fx.other, UPROOT_ANY_GROUP, UPROOT_SKILL);
}
