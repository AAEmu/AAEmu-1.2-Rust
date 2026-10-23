//! Special effect actions on progression, points, portals, doodads, mates / slaves, crime and
//! account UI: `AddExp`, `AddLaborPower`, `GiveLivingPoint`, `GiveHonorPoint`,
//! `GiveCrimePoint`, `EnterBeautyshop`, `ExitArchemall`, `GiveAppellation`, `SavePortal`,
//! `SpawnDoodad`, `RemoveDoodad`, `SpawnPet`, `SpawnSlave`, `AttachTo`, `EscapeMySlave`,
//! `DeclareDominion`, `ReportBot`, `ReportBotExpired`. The managers they reach into (doodads,
//! mates, slaves, portals, instances, crime, dominions) belong to other subsystems and are
//! called through `ext` hooks.

use std::collections::HashMap;

use crate::data::formulas::FormulaKind;
use crate::managers::chat::chat_type;
use crate::models::container::equip_slot;
use crate::models::transform::PositionAndRotation;
use crate::packets::world::SCErrorMsgPacket;
use crate::server::GameServer;
use crate::systems::economy::error_message;
use crate::systems::economy::inventory::{send_error_message_to, Out};
use crate::systems::services::beauty_salon::SCCharacterGenderAndModelModifiedPacket;
use crate::world::WorldObject;

use super::super::super::broadcast;
use super::super::super::geometry::{
    around_radius, instance_and_zone, world_of, world_pos, Target,
};
use super::super::super::packets_special::{
    SCBotSuspectReportedPacket, SCToggleBeautyshopResponsePacket,
};
use super::super::super::progress;
use super::super::super::types::{SkillCastTarget, SkillObject, SkillTargetSelection};
use super::super::EffectArgs;
use super::movement::{add_distance_to_front_deg, rad_to_deg};
use super::{caster_is_player, log_values};

/// `ErrorMessageType.InvalidTarget`.
const ERROR_INVALID_TARGET: i16 = 18;

/// `AddExp`: `value1` exp (or, at level 50+ with `value1 == 0`, the `ExpBySkillEffect` formula
/// times `value3 / 10`) to a character, or to a mate unless it is at its max level (then the
/// skill is cancelled and the caster gets `InvalidTarget`).
pub fn add_exp(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "AddExp", v);
    let Some(unit) = args.target.as_obj().filter(|o| o.is_unit()) else {
        return;
    };
    let level = unit.with_unit(|u| u.unit().level).unwrap_or(0);
    let mut exp_to_add = v[0];
    if exp_to_add == 0 && level >= 50 {
        // Experia
        let Some(formula) = server
            .game_data
            .formulas
            .get_formula_by_kind(FormulaKind::ExpBySkillEffect)
        else {
            tracing::error!(
                "Special effects: AddExp has no ExpBySkillEffect formula (NullReferenceException)"
            );
            return;
        };
        let params = HashMap::from([("pc_level", f64::from(level))]);
        let res = formula.evaluate(&params);
        exp_to_add = (res * f64::from(v[2] as f32 / 10.0)) as i32;
    }
    match unit {
        WorldObject::Mate(mate) => {
            let npcs = server.systems.skills.ext.npcs();
            if npcs.mate_is_max_level(server, mate) {
                // Cancel the skill and don't consume the item/reagent (e.g. Companion Crust)
                match args.source.skill() {
                    Some(s) => s.state().cancelled = true,
                    None => {
                        tracing::error!(
                            "Special effects: AddExp without a skill (NullReferenceException)"
                        );
                        return;
                    }
                }
                if let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) {
                    p.send(SCErrorMsgPacket::new(ERROR_INVALID_TARGET, 0, true));
                }
            } else {
                npcs.mate_add_exp(server, mate, exp_to_add);
            }
        }
        WorldObject::Player(p) => progress::add_exp(server, p, exp_to_add, true),
        _ => {}
    }
}

/// `AddLaborPower`: `ChangeLabor((short)value1, 0)`.
pub fn add_labor_power(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    if let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) {
        progress::change_labor(server, p, v[0] as i16, 0);
        log_values(args, "AddLaborPower", v);
    }
}

/// `GiveLivingPoint`: `round(VocationRate * value1)` vocation points.
pub fn give_living_point(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "GiveLivingPoint", v);
    let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
        return;
    };
    let rate = server.systems.skills.config().vocation_rate;
    let points = (rate * f64::from(v[0])).round_ties_even() as i32;
    progress::change_game_points(server, p, progress::GAME_POINT_VOCATION, points);
}

/// `GiveHonorPoint`: `round(HonorRate * value1)` honor points.
pub fn give_honor_point(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    if caster_is_player(args) {
        tracing::debug!(
            "Special effects: GiveHonorPoint amount {}, value2 {}, value3 {}, value4 {}",
            v[0],
            v[1],
            v[2],
            v[3]
        );
    }
    let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
        return;
    };
    let rate = server.systems.skills.config().honor_rate;
    let points = (rate * f64::from(v[0])).round_ties_even() as i32;
    progress::change_game_points(server, p, progress::GAME_POINT_HONOR, points);
}

/// `GiveCrimePoint` (only logs in the C#).
pub fn give_crime_point(_server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "GiveCrimePoint", v);
}

/// `EnterBeautyshop`: `SCToggleBeautyshopResponsePacket(1)`.
pub fn enter_beautyshop(_server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    if let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) {
        log_values(args, "EnterBeautyshop", v);
        p.send(SCToggleBeautyshopResponsePacket { state: 1 });
    }
}

/// `MateMakeGetUp` (special 51): revives a downed summoned mate.
///
/// Data evidence: skill 13719 `일으켜세우기` "revives an injured summoned mate", self-target,
/// friendly; the former C# body only logged. The mate is the target if it is one, else the
/// caster, else the caster player's active mate.
pub fn mate_make_get_up(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "MateMakeGetUp", v);
    let mate = args
        .target
        .as_obj()
        .and_then(WorldObject::as_mate)
        .or_else(|| args.caster.as_ref().and_then(WorldObject::as_mate))
        .cloned()
        .or_else(|| {
            args.caster
                .as_ref()
                .and_then(WorldObject::as_player)
                .and_then(|p| crate::systems::vehicles::mates::first_active_mate(server, p))
        });
    let Some(mate) = mate else {
        return;
    };
    if mate.data().unit.hp > 0 {
        return;
    }
    crate::systems::vehicles::mates::revive_mate(server, &mate);
}

/// `ExitArchemall`: `IndunManager.RequestLeaveInstance(character)`.
pub fn exit_archemall(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "ExitArchemall", v);
    if let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) {
        server.systems.skills.ext.npcs().leave_instance(server, p);
    }
}

/// `GiveAppellation`: `character.Appellations.Add(value1)`.
pub fn give_appellation(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    if let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) {
        tracing::debug!("Special effects: GiveAppellation value1 {}", v[0]);
        server
            .systems
            .skills
            .ext
            .social()
            .give_appellation(server, p, v[0] as u32);
    }
}

/// `GenderTransfer` (special 107).
///
/// Data evidence: skill 23469 `성별 전환하기` is cast by the beauty-salon NPC interaction
/// (`target_type` any unit, `target_selection` Target); the former C# class only logged.
pub fn gender_transfer(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "GenderTransfer", v);
    let target = args
        .target
        .as_obj()
        .and_then(WorldObject::as_player)
        .or_else(|| args.caster.as_ref().and_then(WorldObject::as_player));
    let Some(player) = target else {
        return;
    };
    transfer_gender(server, player);
}

/// Flips a character's gender (`3 - gender`: 1 male <-> 2 female), refuses gender-locked
/// equipment of the target gender and announces the new model.
///
/// `Character.Save` already persists `gender` and the model id is rebuilt from the race/gender
/// template on load, so the change reaches the database with the next character save.
pub(crate) fn transfer_gender(server: &GameServer, player: &crate::world::PlayerRef) -> bool {
    let (race, old_gender, equipment, char_id) = {
        let c = player.data();
        (c.race, c.gender, c.inventory.equipment().clone(), c.id)
    };
    if old_gender != 1 && old_gender != 2 {
        tracing::warn!(
            "Special effects: GenderTransfer character {char_id} has invalid gender {old_gender}"
        );
        return false;
    }
    let new_gender = 3 - old_gender;
    let mut hair_template_id = 0;
    {
        // Copy what the check needs, then release the container lock before touching the
        // character (locking rule: at most one object data lock at a time).
        let eq = equipment.lock().expect("container poisoned");
        if let Some(item) = eq.get_item_by_slot(equip_slot::HAIR) {
            hair_template_id = item.template_id;
        }
        let blocked = eq.items.iter().any(|item| {
            server
                .game_data
                .items
                .template(item.template_id)
                .is_some_and(|t| t.char_gender != 0 && t.char_gender != new_gender)
        });
        if blocked {
            let mut out = Out::Direct;
            send_error_message_to(
                &mut out,
                player,
                error_message::GENDER_TRANSFER_AS_GENDER_EQUIPMENT,
            );
            return false;
        }
    }
    let Some(template) = server
        .game_data
        .characters
        .template(race, new_gender)
        .cloned()
    else {
        tracing::warn!(
            "Special effects: GenderTransfer has no character template for race {race} gender {new_gender}"
        );
        return false;
    };
    let packet = {
        let mut c = player.data();
        c.gender = new_gender;
        c.unit.model_id = template.model_id;
        SCCharacterGenderAndModelModifiedPacket {
            character_id: c.id,
            hair_template_id,
            model_params: c.unit.model_params.clone(),
        }
    };
    let me = WorldObject::from(player);
    broadcast(server, &me, &packet, true);
    true
}

/// `SavePortal`: a new private portal at the character's position (`SkillObjectSavePortalInfo`
/// id 0) or a rename of portal `id`.
pub fn save_portal(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let Some(caster) = &args.caster else {
        return;
    };
    let Some(p) = caster.as_player() else {
        return;
    };
    log_values(args, "SavePortal", v);
    let SkillObject::SavePortalInfo { id, name } = args.skill_object else {
        return;
    };
    let npcs = server.systems.skills.ext.npcs();
    if *id == 0 {
        let pos = world_pos(server, &Target::Obj(caster.clone()));
        let zone_id = p.data().unit.transform.zone_id;
        npcs.add_private_portal(server, p, pos.x, pos.y, pos.z, pos.rot_z, zone_id, name);
    } else {
        npcs.rename_private_portal(server, p, *id as u32, name);
    }
}

/// `SpawnDoodad`: doodad `value1` one metre in front of the caster (or the target for
/// `TargetSelection.Target` skills), facing like the target, spawned after `value2` ms.
///
/// Position and orientation use the selected source/target frame, in radians.
/// `value3` (create trade pack) is unused, as in the C#. The
/// doodad itself (`DoodadManager.Create`, `GeoData.GetHeight`, `InitDoodad`, `Spawn`) is the
/// npc subsystem's.
pub fn spawn_doodad(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let [doodad_id, delay, create_trade_pack, value4] = v;
    let Some(caster) = &args.caster else {
        tracing::warn!(
            "Special effects: SpawnDoodad has no caster defined, doodadId {doodad_id}, delay {delay}, createTradePack {create_trade_pack}, value4 {value4}"
        );
        return;
    };
    if caster.as_player().is_some() {
        tracing::debug!(
            "Special effects: SpawnDoodad doodadId {doodad_id}, delay {delay}, createTradePack {create_trade_pack}, value4 {value4}"
        );
    }
    let caster_t = Target::Obj(caster.clone());
    let target_pos = world_pos(server, &args.target);
    let selection = args
        .source
        .skill()
        .and_then(|s| SkillTargetSelection::from_raw(i64::from(s.template.target_selection)));
    let (base, zone_id) = match selection {
        Some(SkillTargetSelection::Target) => (target_pos, instance_and_zone(&args.target).1),
        _ => (world_pos(server, &caster_t), instance_and_zone(&caster_t).1),
    };
    let (xx, yy) = add_distance_to_front_deg(1.0, base.x, base.y, rad_to_deg(base.rot_z) + 90.0);
    let pos = PositionAndRotation::new(xx, yy, base.z, base.rot_x, base.rot_y, base.rot_z);
    server.systems.skills.ext.npcs().spawn_doodad(
        server,
        caster,
        doodad_id as u32,
        zone_id,
        pos,
        delay,
    );
}

/// Delete matching doodads through their manager; world visibility sends the removal once.
pub fn remove_doodad(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "RemoveDoodad", v);
    let Some(caster) = &args.caster else {
        tracing::error!("Special effects: RemoveDoodad without a caster (NullReferenceException)");
        return;
    };
    let npcs = server.systems.skills.ext.npcs();
    for o in around_radius(server, &Target::Obj(caster.clone()), 10.0, false) {
        let Some(d) = o.as_doodad() else {
            continue;
        };
        let template_id = d.data().template_id;
        if i64::from(template_id) == i64::from(v[0]) {
            npcs.delete_doodad(server, o.obj_id());
        }
    }
}

/// `SpawnPet`: `owner.Mates.SpawnMount(skillItem)` (the `value1` switch is empty in the C#).
pub fn spawn_pet(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "SpawnPet", v);
    let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
        tracing::error!(
            "Special effects: SpawnPet caster is not a Character (InvalidCastException)"
        );
        return;
    };
    if !args.caster_caster.is_item() {
        tracing::error!(
            "Special effects: SpawnPet casterObj is not a SkillItem (InvalidCastException)"
        );
        return;
    }
    server
        .systems
        .skills
        .ext
        .npcs()
        .spawn_mount(server, p, &args.caster_caster);
}

/// `SpawnSlave`: `SlaveManager.Create(owner, skillItem)`. `SummonPos` casts carry the spot the
/// player picked; a self-targeted summon item cast has no placement and keeps the C# offset.
pub fn spawn_slave(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "SpawnSlave", v);
    let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
        tracing::error!(
            "Special effects: SpawnSlave caster is not a Character (InvalidCastException)"
        );
        return;
    };
    if !args.caster_caster.is_item() {
        tracing::error!(
            "Special effects: SpawnSlave casterObj is not a SkillItem (InvalidCastException)"
        );
        return;
    }
    let placement = match (&args.target, &args.target_caster) {
        (Target::Pos(pos), SkillCastTarget::Position { rot, .. }) => {
            Some(crate::systems::skills::ext::SummonPlacement {
                x: pos.pos.x,
                y: pos.pos.y,
                z: pos.pos.z,
                yaw: *rot,
            })
        }
        (
            Target::Pos(pos),
            SkillCastTarget::Position2 { .. } | SkillCastTarget::Position3 { .. },
        ) => Some(crate::systems::skills::ext::SummonPlacement {
            x: pos.pos.x,
            y: pos.pos.y,
            z: pos.pos.z,
            yaw: 0.0,
        }),
        _ => None,
    };
    let max_range = args
        .source
        .skill()
        .map_or(0.0, |s| s.template.max_range as f32);
    server.systems.skills.ext.npcs().spawn_slave(
        server,
        p,
        &args.caster_caster,
        placement,
        max_range,
    );
}

/// `AttachTo`: a character binds the target slave at attach point `value1`.
pub fn attach_to(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "AttachTo", v);
    let Some(slave) = args.target.as_obj().filter(|t| t.as_slave().is_some()) else {
        return;
    };
    let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
        return;
    };
    server
        .systems
        .skills
        .ext
        .npcs()
        .bind_slave(server, p, slave.obj_id(), v[0]);
}

/// `EscapeMySlave`: `SlaveManager.RidersEscape(player, positionTarget)`.
pub fn escape_my_slave(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
        return;
    };
    if !matches!(args.target_caster, SkillCastTarget::Position { .. }) {
        return;
    }
    tracing::debug!(
        "Special effects: EscapeMySlave value1 {}, value2 {}, value3 {}, value4 {}",
        v[0],
        v[1],
        v[2],
        v[3]
    );
    server
        .systems
        .skills
        .ext
        .npcs()
        .riders_escape(server, p, &args.target_caster);
}

/// `DeclareDominion`: an expedition member claims the lodestone (house) target; the dominion
/// data / packet / backpack consumption are the social subsystem's.
pub fn declare_dominion(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "DeclareDominion", v);
    let Some(caster) = &args.caster else {
        tracing::error!(
            "Special effects: DeclareDominion without a caster (NullReferenceException)"
        );
        return;
    };
    let Some(expedition_id) = caster.with_unit(|u| u.unit().expedition_id) else {
        tracing::error!(
            "Special effects: DeclareDominion caster is not a Unit (InvalidCastException)"
        );
        return;
    };
    if expedition_id == 0 {
        return;
    }
    // Check target is not already claimed
    let Some(lodestone) = args.target.as_obj().filter(|t| t.as_house().is_some()) else {
        return;
    };
    server
        .systems
        .skills
        .ext
        .social()
        .declare_dominion(server, caster, expedition_id, lodestone);
}

/// `ReportBot`: a character reports the target character as a bot (`CrimeManager.ReportBot`);
/// on success `SCBotSuspectReportedPacket` is broadcast around the reporter.
pub fn report_bot(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let Some(caster) = &args.caster else {
        return;
    };
    let Some(reporter) = caster.as_player() else {
        // Only players can report other players
        return;
    };
    tracing::debug!(
        "Special effects: ReportBot value1 {}, value2 {}, value3 {}, value4 {}",
        v[0],
        v[1],
        v[2],
        v[3]
    );
    let SkillCastTarget::Unit(target_obj_id) = args.target_caster else {
        tracing::debug!(
            "Special effects: ReportBot target is not a Unit, ObjId: {}",
            args.target_caster.obj_id()
        );
        return;
    };
    let Some(suspect) = world_of(server, caster).and_then(|w| w.character_by_obj_id(target_obj_id))
    else {
        tracing::debug!(
            "Special effects: ReportBot target is not a player, ObjId: {target_obj_id}"
        );
        return;
    };
    let msg = match args.skill_object {
        SkillObject::Text(t) => t.as_str(),
        _ => "",
    };
    let reporter_name = reporter.data().unit.name.clone();
    let suspect_name = suspect.data().unit.name.clone();
    if !server
        .systems
        .skills
        .ext
        .social()
        .report_bot(server, &suspect, reporter, msg)
    {
        tracing::warn!(
            "Special effects: ReportBot target {suspect_name} failed to get reported by {reporter_name} (possible multiple reports)"
        );
        return;
    }
    broadcast(
        server,
        caster,
        &SCBotSuspectReportedPacket {
            reporter: reporter_name,
            suspect: suspect_name,
        },
        true,
    );
}

/// `ReportBotExpired`: `CrimeManager.ReportBotExpired(player)` (suspect buffs removed).
pub fn report_bot_expired(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
        // Only applies to players
        return;
    };
    tracing::debug!(
        "Special effects: ReportBotExpired value1 {}, value2 {}, value3 {}, value4 {}",
        v[0],
        v[1],
        v[2],
        v[3]
    );
    if server
        .systems
        .skills
        .ext
        .social()
        .report_bot_expired(server, p)
    {
        let (name, id) = {
            let c = p.data();
            (c.unit.name.clone(), c.id)
        };
        tracing::info!(
            "Special effects: ReportBotExpired, {name} ({id}) removed their suspect buffs"
        );
    }
}

/// `SextantPos` (special effect 69; row 7181 is the only row, on skill 18416 "measure my
/// current position", used by item 24795 Sextant). The 1.2 data has no authored system-message
/// coordinate format (only the treasure-map item descriptions), so this is a placeholder
/// format to verify in game.
pub fn sextant_pos(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "SextantPos", v);
    let Some(caster) = &args.caster else {
        return;
    };
    let Some(player) = caster.as_player() else {
        return;
    };
    let pos = world_pos(server, &Target::Obj(caster.clone()));
    send_system_message(player, &format!("X: {:.0}, Y: {:.0}", pos.x, pos.y));
}

/// `Character.SendMessage(message)` (`ChatType.System`).
pub(crate) fn send_system_message(p: &crate::world::PlayerRef, message: &str) {
    crate::world::session::send_message(p, chat_type::SYSTEM, message);
}

#[cfg(test)]
mod tests {
    use super::transfer_gender;
    use super::*;
    use crate::data::character_templates::CharacterTemplate;
    use crate::data::items::{EquipTemplate, GradeTemplate, ItemTemplate, TemplateKind};
    use crate::data::GameData;
    use crate::opcodes::sc;
    use crate::systems::skills::effects::EffectSource;
    use crate::systems::skills::types::{CastAction, SkillCaster};
    use crate::test_support::{spawn_player, test_character, test_server_with, TestClient};
    use crate::world::PlayerRef;

    const LOCKED_CHEST: u32 = 70;

    fn key(race: u8, gender: u8) -> u8 {
        16 * gender + race
    }

    fn data() -> GameData {
        let mut d = GameData::empty();
        d.characters.templates.insert(
            key(1, 1),
            CharacterTemplate {
                race: 1,
                gender: 1,
                model_id: 100,
                ..CharacterTemplate::default()
            },
        );
        d.characters.templates.insert(
            key(1, 2),
            CharacterTemplate {
                race: 1,
                gender: 2,
                model_id: 200,
                ..CharacterTemplate::default()
            },
        );
        d.items.grades.insert(
            0,
            GradeTemplate {
                grade: 0,
                grade_order: 0,
                durability: 1.0,
            },
        );
        d.items.config.durability_const = 1.0;
        d.items.templates.insert(
            LOCKED_CHEST,
            ItemTemplate {
                char_gender: 1,
                kind: TemplateKind::Armor(EquipTemplate {
                    slot_type_id: 3,
                    coverage: 10,
                    durability_ratio: 1.0,
                    durability_multiplier: 100,
                    ..EquipTemplate::default()
                }),
                ..ItemTemplate::plain(LOCKED_CHEST, 1)
            },
        );
        d
    }

    fn equip(server: &crate::server::GameServer, player: &PlayerRef, template: u32, slot: i32) {
        let item = server
            .items
            .create(
                template,
                1,
                0,
                1,
                &server.game_data.items,
                &server.ids.item,
                0,
            )
            .expect("item");
        let eq = player.data().inventory.equipment().clone();
        assert!(eq
            .lock()
            .unwrap()
            .add_new_item(item, slot, &server.game_data.items)
            .is_ok());
    }

    #[tokio::test]
    async fn gender_transfer_flips_gender_model_and_broadcasts() {
        let server = test_server_with(data());
        let mut a = TestClient::new(&server, 1, 1001);
        let mut b = TestClient::new(&server, 2, 1002);
        let pa = spawn_player(&server, &a, test_character(1, "Actor", 100.0, 100.0), 0x100);
        let _pb = spawn_player(&server, &b, test_character(2, "Seer", 101.0, 100.0), 0x101);
        a.drain();
        b.drain();

        assert!(transfer_gender(&server, &pa));
        assert_eq!(pa.data().gender, 2);
        assert_eq!(pa.data().unit.model_id, 200);
        for frames in [a.drain(), b.drain()] {
            assert!(
                frames
                    .iter()
                    .any(|(op, _)| *op == sc::SCCharacterGenderAndModelModifiedPacket),
                "gender packet not received: {frames:x?}"
            );
        }
    }

    #[tokio::test]
    async fn gender_transfer_is_blocked_by_gender_locked_equipment() {
        let server = test_server_with(data());
        let mut a = TestClient::new(&server, 1, 1001);
        let pa = spawn_player(&server, &a, test_character(1, "Actor", 100.0, 100.0), 0x100);
        equip(&server, &pa, LOCKED_CHEST, 2);
        a.drain();

        assert!(!transfer_gender(&server, &pa));
        assert_eq!(pa.data().gender, 1);
        let frames = a.drain();
        assert_eq!(frames.len(), 1, "{frames:x?}");
        assert_eq!(frames[0].0, sc::SCErrorMsgPacket);
        assert_eq!(
            i16::from_le_bytes([frames[0].1[0], frames[0].1[1]]),
            crate::systems::economy::error_message::GENDER_TRANSFER_AS_GENDER_EQUIPMENT
        );
    }

    #[tokio::test]
    async fn sextant_pos_sends_the_caster_position_as_one_system_message() {
        let server = test_server_with(GameData::empty());
        let mut client = TestClient::new(&server, 1, 1);
        let p = spawn_player(
            &server,
            &client,
            test_character(1, "Caster", 123.0, 456.0),
            0x100,
        );
        client.drain();
        let args = EffectArgs {
            caster: Some(WorldObject::from(&p)),
            caster_caster: SkillCaster::Unit(p.obj_id),
            target: Target::Obj(WorldObject::from(&p)),
            target_caster: SkillCastTarget::Unit(p.obj_id),
            cast: CastAction::Skill {
                skill_id: 18_416,
                tl_id: 1,
            },
            source: EffectSource::default(),
            skill_object: &SkillObject::None,
        };
        super::super::execute(&server, 69, &args, [0; 4]);
        let frames = client.drain();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, crate::opcodes::sc::SCChatMessagePacket);
        let text = String::from_utf8_lossy(&frames[0].1);
        assert!(text.contains("X: 123"), "{text}");
        assert!(text.contains("Y: 456"), "{text}");
    }
}
