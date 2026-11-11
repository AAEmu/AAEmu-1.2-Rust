//! Special effect actions on buffs, cooldowns, mana, plots and casting: `Charge`,
//! `DisturbCasting`, `Resurrection`, `SkillUse`, `ManaCost`, `Cooldown`, `GlobalCooldown`,
//! `ResetCooldown`, `StopManaRegen`, `AddBreath`, `CancelStealth`, `AutoAttack`,
//! `FinishChanneling`, `SetVariable`, `ConsumeLaborPower`.

use crate::models::unit::SkillAttribute;
use crate::server::GameServer;
use crate::world::{now_ms, WorldObject};

use super::super::super::buffs;
use super::super::super::combat as unit_combat;
use super::super::super::packets::{SCCastingDelayedPacket, SCSkillCooldownResetPacket};
use super::super::super::packets_special::SCNotifyResurrectionPacket;
use super::super::super::progress;
use super::super::super::skill::{
    apply_skill_modifiers, delay_cast, level_and_inc, mana_level_scale, skips_level_mana,
    start_auto_skill, use_skill,
};
use super::super::super::state::{SkillRun, SkillTaskKind};
use super::super::super::stats::{stat, StatKind};
use super::super::super::types::{BuffRemoveOn, SkillCastTarget};
use super::super::{rand_int, EffectArgs};
use super::{caster_is_player, log_values};

/// `(Unit)caster` (`None` + an error log where the C# throws).
fn unit_caster<'a>(args: &'a EffectArgs<'_>, action: &str) -> Option<&'a WorldObject> {
    match &args.caster {
        Some(c) if c.is_unit() => Some(c),
        Some(_) => {
            tracing::error!(
                "Special effects: {action} caster is not a Unit (InvalidCastException)"
            );
            None
        }
        None => {
            tracing::error!("Special effects: {action} without a caster (NullReferenceException)");
            None
        }
    }
}

/// `Charge`: adds charges to a buff on the caster.
///
/// Compute the accumulated charge before AddBuff so refreshes and their packets keep it.
pub fn charge(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let [buff_id, min_charge, max_charge, unused] = v;
    if caster_is_player(args) {
        tracing::debug!(
            "Special effects: Charge buffId {buff_id}, minCharge {min_charge}, maxCharge {max_charge}, unused {unused}"
        );
    }
    let Some(caster) = &args.caster else {
        tracing::error!("Special effects: Charge without a caster (NullReferenceException)");
        return;
    };
    let buff = unit_combat::buff_of(caster, buff_id as u32);
    let Some(template) = server
        .game_data
        .systems
        .skills
        .buff(buff_id as u32)
        .cloned()
    else {
        tracing::error!(
            "Special effects: Charge buff {buff_id} has no template (NullReferenceException)"
        );
        return;
    };
    let Some(charge_delta) = rand_int(min_charge, max_charge) else {
        tracing::error!(
            "Special effects: Charge minCharge {min_charge} > maxCharge {max_charge} (ArgumentOutOfRangeException)"
        );
        return;
    };
    let old_charge = buff.as_ref().map_or(0, |b| b.charge);
    let mut new_effect = buffs::new_buff(
        server,
        &template,
        Some(caster),
        &args.caster_caster,
        args.source.skill(),
        now_ms(),
    );
    new_effect.charge = old_charge
        .saturating_add(charge_delta)
        .clamp(0, template.max_charge.max(0));
    buffs::add_buff(
        server,
        caster,
        new_effect,
        buff.as_ref().map_or(0, |b| b.index),
        0,
    );
}

/// Interrupt both plot and direct casts, using the configured percentage and delay.
pub fn disturb_casting(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    if caster_is_player(args) {
        tracing::debug!(
            "Special effects: DisturbCasting chance {}, delay {}, value3 {}, value4 {}",
            v[0],
            v[1],
            v[2],
            v[3]
        );
    }
    if !super::super::chance_succeeds(v[0]) {
        return;
    }
    let Some(target) = args.target.as_obj().filter(|t| t.is_unit()).cloned() else {
        return;
    };
    if v[1] > 0 {
        // WP-1: a delayed disturb extends a running direct cast instead of interrupting it
        // (the 1.2 client adds the delay to the cast bar, `SCCastingDelayed`). Only the victim
        // is told: whether viewers also receive it is capture-pending.
        if let Some(skill_id) = delay_cast(&target, i64::from(v[1])) {
            if let Some(p) = target.as_player() {
                p.send(SCCastingDelayedPacket {
                    skill_id: skill_id as u16,
                    plot_id: 0,
                    delay: v[1] as u32,
                });
            }
            return;
        }
        let (channeling, plotting) = target
            .with_unit(|u| {
                let s = &u.unit().skill_state;
                (
                    s.skill_task
                        .as_ref()
                        .is_some_and(|t| matches!(t.kind, SkillTaskKind::EndChanneling { .. })),
                    s.active_plot.is_some(),
                )
            })
            .unwrap_or((false, false));
        if channeling || plotting {
            // Neither a channel nor a plot cast has a 1.2 delay packet, so how a delayed
            // disturb affects them is capture-pending: keep the old delayed interrupt.
            tracing::debug!(
                "Special effects: DisturbCasting {} ms on a channel or plot cast (it cannot be delayed, capture-pending); interrupting after the delay",
                v[1]
            );
            super::super::super::tasks::schedule(server, i64::from(v[1]), move |server| {
                if super::super::super::geometry::is_live_object(&server, &target) {
                    super::super::super::skill::interrupt_skills(&server, &target);
                }
            });
        }
        // No cast, channel or plot: nothing to disturb (the old code armed an interrupt that
        // could hit a cast started during the delay).
    } else {
        super::super::super::skill::interrupt_skills(server, &target);
    }
}

/// `Resurrection`: offers a resurrection to a dead character (`SCNotifyResurrectionPacket`).
pub fn resurrection(_server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let Some(p) = args.target.as_obj().and_then(WorldObject::as_player) else {
        return;
    };
    if p.data().unit.hp > 0 {
        return;
    }
    log_values(args, "Resurrection", v);
    p.send(SCNotifyResurrectionPacket {
        caster: args.caster_caster,
    });
    let mut c = p.data();
    c.skill_char.resurrect_offer = true;
    c.skill_char.resurrect_hp_percent = v[1] as u32;
    c.skill_char.resurrect_mp_percent = v[2] as u32;
}

/// `SkillUse`: rolls `chance`, then uses skill `value1` on the target after `delay` ms
/// (`UseSkillTask`, `bypassGcd`).
pub fn skill_use(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let [skill_id, delay, chance, value4] = v;
    if caster_is_player(args) {
        tracing::debug!(
            "Special effects: SkillUse skillId {skill_id}, delay {delay}, value3 {chance}, value4 {value4}"
        );
    }
    let Some(caster) = unit_caster(args, "SkillUse") else {
        return;
    };
    if chance != 0 && !super::super::chance_succeeds(chance) {
        caster.with_unit(|u| u.unit_mut().skill_state.condition_chance = false);
        return;
    }
    caster.with_unit(|u| u.unit_mut().skill_state.condition_chance = true);
    let Some(template) = server
        .game_data
        .systems
        .skills
        .skill(skill_id as u32)
        .cloned()
    else {
        tracing::error!(
            "Special effects: SkillUse skill {skill_id} has no template (NullReferenceException)"
        );
        return;
    };
    let skill = SkillRun::new(template, 1);
    let target_caster = SkillCastTarget::Unit(args.target.obj_id());
    // `skills.keep_stealth` on the skill that carries this effect: chained stealth skills
    // (13202 pickpocket, 12522 scout magic) must not drop the caster's stealth buffs.
    let keep_stealth = args
        .source
        .skill
        .as_ref()
        .is_some_and(|s| s.template.keep_stealth);
    if keep_stealth {
        buffs::trigger_remove_on_keep_stealth(server, caster, BuffRemoveOn::UseSkill, 0);
    } else {
        buffs::trigger_remove_on(server, caster, BuffRemoveOn::UseSkill, 0);
    }
    let caster = caster.clone();
    let target = args.target.as_obj().cloned();
    let originating_cast = args
        .source
        .skill
        .as_ref()
        .map(|skill| (skill.clone(), skill.state().generation));
    let caster_caster = args.caster_caster;
    let skill_object = args.skill_object.clone();
    super::super::super::tasks::schedule(server, i64::from(delay), move |srv| {
        if !super::super::super::geometry::is_live_object(&srv, &caster)
            || target
                .as_ref()
                .is_some_and(|target| !super::super::super::geometry::is_live_object(&srv, target))
        {
            return;
        }
        if originating_cast
            .as_ref()
            .is_some_and(|(skill, generation)| {
                let state = skill.state();
                state.cancelled || state.generation != *generation
            })
        {
            return;
        }
        let mut tc = target_caster;
        use_skill(
            &srv,
            &skill,
            &caster,
            caster_caster,
            &mut tc,
            Some(skill_object),
            true,
        );
    });
}

/// Mana of a plot `ManaCost` effect: `value1 + value2 / 100 * (1 + T) * scale`, `scale` being
/// [`mana_level_scale`] (the `LevelMana` data formula at the cast's skill level `L`) and `T`
/// the cast's `casting_inc` rank term. Checked against `compact.sqlite3`
/// (84 distinct (skill, effect values) rows of `SpecialEffect` type 39 reached through the
/// skills' plots): 62 have `value1 == skills.mana_cost` and `value2 / 100 == skills.mana_level_md`,
/// so the effect charges what the skill tooltip shows
/// (`mana_cost + mana_level_md * (1 + T) * scale`) instead of the C# guess `value2 / 6.35`.
/// The 22 others are plot effects whose skill row has `mana_level_md` 0 or 1 (`value2` 1,
/// 100-600): they charge an amount of their own, which this formula takes at face value
/// (`value2` percent of the scaled term). An ability-less skill cast from an item skips the
/// scaled term like [`super::super::super::skill::mana_cost`] does (the one item row here,
/// capture net 22590, then charges its `value1` 0).
fn plot_mana_cost(value1: i32, value2: i32, scale: f64, inc: f64) -> f64 {
    f64::from(value1) + f64::from(value2) / 100.0 * (1.0 + inc) * scale
}

/// `ManaCost`: [`plot_mana_cost`] (skill modifiers applied) is taken from the character.
///
/// It does not start the post-cast window: the 1.2 client's `OnPlotEvent` (`FUN_3938e6b0` @
/// 0x3938e6b0) only sets `LastCast` for the `StopManaRegen` special effect (0x34), and a plot
/// is not a `SCSkillFired` (`FUN_3939b010`). The C# also set `IsInPostCast` here; see
/// PORTING.md.
pub fn mana_cost(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "ManaCost", v);
    let Some(caster) = args.caster.as_ref() else {
        return;
    };
    if caster.as_player().is_none() {
        return;
    };
    let Some(skill) = args.source.skill() else {
        tracing::error!("Special effects: ManaCost without a skill (NullReferenceException)");
        return;
    };
    // The item-use rule of `skill::mana_cost`: an ability-less skill cast from an item keeps
    // only the fixed part (`value1`, the skill's `mana_cost`), without the level-scaled term.
    let mana = if skips_level_mana(&skill.template, &args.caster_caster) {
        f64::from(v[0])
    } else {
        let (level, inc) = level_and_inc(server, skill, caster);
        plot_mana_cost(v[0], v[1], mana_level_scale(server, level), inc)
    };
    let mana = apply_skill_modifiers(server, skill, caster, SkillAttribute::ManaCost, mana);
    unit_combat::reduce_current_mp(server, caster, mana as i32);
}

/// `Cooldown`: puts the skill on cooldown for `value1` ms (skill modifiers applied).
pub fn cooldown(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    if caster_is_player(args) {
        tracing::debug!(
            "Special effects: Cooldown cooldownTime {}, value2 {}, value3 {}, value4 {}",
            v[0],
            v[1],
            v[2],
            v[3]
        );
    }
    let Some(caster) = unit_caster(args, "Cooldown") else {
        return;
    };
    let Some(skill) = args.source.skill() else {
        tracing::error!("Special effects: Cooldown without a skill (NullReferenceException)");
        return;
    };
    let duration = apply_skill_modifiers(
        server,
        skill,
        caster,
        SkillAttribute::Cooldown,
        f64::from(v[0]),
    );
    let skill_id = skill.template.id;
    if duration <= 0.0 {
        return;
    }
    // The 1.2 client applies the `Cooldown` special effect itself (`OnPlotEvent` special 0x28
    // -> `39368440`), so the server must not also send an `SCCooldowns` update here.
    caster.with_unit(|u| {
        u.unit_mut()
            .cooldowns
            .add_cooldown(skill_id, duration as u32, now_ms())
    });
}

/// `GlobalCooldown`: `GlobalCooldown = now + value1 * (GlobalCooldownMul / 100)`.
pub fn global_cooldown(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "GlobalCooldown", v);
    let Some(caster) = unit_caster(args, "GlobalCooldown") else {
        return;
    };
    let mul = stat(server, caster, StatKind::GlobalCooldownMul) as f32;
    let ms = v[0] as f32 * (mul / 100.0);
    let until = now_ms() + f64::from(ms) as i64;
    caster.with_unit(|u| u.unit_mut().global_cooldown_ms = until);
}

/// `ResetCooldown`: `SCSkillCooldownResetPacket` for skill `value1` and / or tag `value2`
/// (characters only).
pub fn reset_cooldown(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    if caster_is_player(args) {
        tracing::debug!(
            "Special effects: ResetCooldown skillId {}, tagId {}, gcd {}, value4 {}",
            v[0],
            v[1],
            v[2],
            v[3]
        );
    }
    let gcd = v[2] == 1;
    let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
        return;
    };
    {
        let mut player = p.data();
        if gcd {
            player.unit.global_cooldown_ms = 0;
        }
        if v[0] > 0 {
            player.unit.cooldowns.remove_cooldown(v[0] as u32);
        }
        for id in server.game_data.systems.skills.skills_by_tag(v[1] as u32) {
            player.unit.cooldowns.remove_cooldown(*id);
        }
    }
    if v[0] != 0 {
        // Character.ResetSkillCooldown(skillId, gcd)
        p.send(SCSkillCooldownResetPacket {
            obj_id: p.obj_id,
            skill_id: v[0] as u32,
            tag_id: 0,
            gcd,
        });
    }
    if v[1] != 0 {
        p.send(SCSkillCooldownResetPacket {
            obj_id: p.obj_id,
            skill_id: 0,
            tag_id: v[1] as u32,
            gcd,
        });
    }
}

/// `StopManaRegen`: `IsInPostCast = true`, `LastCast = now`.
///
/// The 1.2 client does exactly this in its `OnPlotEvent` handler (`FUN_3938e6b0` @ 0x3938e6b0,
/// DEC 740255: special effect 0x34 -> `FUN_390b3ab0` @ 0x390b3ab0 sets `LastCast`;
/// `FUN_390b63f0` then uses `persistent_mana_regen` until `LastCast + 5000`).
pub fn stop_mana_regen(_server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "StopManaRegen", v);
    if let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) {
        let mut c = p.data();
        c.skill_char.is_in_post_cast = true;
        c.skill_char.last_cast_ms = now_ms();
    }
}

/// `AddBreath`: under water, `Breath = min(LungCapacity, Breath + value1)` (no packet, as the
/// C#). Negative changes clamp at zero.
pub fn add_breath(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "AddBreath", v);
    // Deviation: the C# refilled only a character caster. A buff a doodad applied (Air Bubble
    // 3036 from the Lake Bubbles clout, tick effect 17316) has the doodad as its caster, so its
    // breath tick did nothing and the player drowned inside the bubbles; the character holding
    // the buff (the effect target) breathes instead.
    let target = args.target.as_obj().filter(|o| o.as_player().is_some());
    let Some(breather) = args
        .caster
        .as_ref()
        .filter(|c| c.as_player().is_some())
        .or(target)
    else {
        return;
    };
    let Some(p) = breather.as_player() else {
        return;
    };
    if !p.data().unit.is_under_water {
        return;
    }
    let lung = stat(server, breather, StatKind::LungCapacity) as u32;
    let mut c = p.data();
    let sum = i64::from(c.breath) + i64::from(v[0]);
    c.breath = sum.clamp(0, i64::from(lung)) as u32;
}

/// `CancelStealth`: `caster.Buffs.RemoveStealth()`.
pub fn cancel_stealth(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "CancelStealth", v);
    let Some(caster) = &args.caster else {
        tracing::error!("Special effects: CancelStealth without a caster (NullReferenceException)");
        return;
    };
    buffs::remove_stealth(server, caster);
}

/// `AutoAttack`: starts the auto attack with skill `value1` (default 2 = melee).
pub fn auto_attack(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let Some(caster) = args.caster.as_ref() else {
        return;
    };
    let Some(p) = caster.as_player() else {
        return;
    };
    if p.data().unit.is_auto_attack {
        return;
    }
    let attack_skill_id = if v[0] > 0 { v[0] as u32 } else { 2 };
    let Some(template) = server
        .game_data
        .systems
        .skills
        .skill(attack_skill_id)
        .cloned()
    else {
        return;
    };
    let skill = SkillRun::new(template, 1);
    p.data().unit.is_auto_attack = true;
    start_auto_skill(server, caster, &skill);
}

/// `FinishChanneling`: `unit.ActivePlotState?.FinishChanneling()` of the target.
pub fn finish_channeling(_server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "FinishChanneling", v);
    if let Some(t) = args.target.as_obj() {
        if let Some(Some(plot)) = t.with_unit(|u| u.unit().skill_state.active_plot.clone()) {
            plot.0.finish_channeling();
        }
    }
}

/// `SetVariable`: plot variable `value1` += `value2` (operation 1) or = `value2` (operation 11)
/// on the skill's active plot state.
pub fn set_variable(_server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    let [index, value, operation, value4] = v;
    if caster_is_player(args) {
        tracing::debug!(
            "Special effects: SetVariable index {index}, value {value}, operation {operation}, value4 {value4}"
        );
    }
    let Some(skill) = args.source.skill() else {
        tracing::error!("Special effects: SetVariable without a skill (NullReferenceException)");
        return;
    };
    let plot = skill.state().active_plot.clone();
    match plot {
        Some(plot) => {
            if operation == 1 || operation == 11 {
                let mut inner = plot.inner();
                let Some(slot) = usize::try_from(index)
                    .ok()
                    .and_then(|i| inner.variables.get_mut(i))
                else {
                    tracing::error!(
                        "Special effects: SetVariable index {index} out of range (IndexOutOfRangeException)"
                    );
                    return;
                };
                if operation == 1 {
                    *slot = slot.wrapping_add(value);
                } else {
                    *slot = value;
                }
            } else {
                tracing::error!("Invalid Plot Variable Operation Kind.");
            }
        }
        None => tracing::error!("No active plot state located."),
    }
    tracing::trace!(
        "value1 {}, value2 {}, value3 {}, value4 {}",
        v[0],
        v[1],
        v[2],
        v[3]
    );
}

/// `ConsumeLaborPower`: the skill's `ConsumeLaborPower` is taken from the character.
pub fn consume_labor_power(server: &GameServer, args: &EffectArgs<'_>, v: [i32; 4]) {
    log_values(args, "ConsumeLaborPower", v);
    let Some(skill) = args.source.skill() else {
        tracing::error!(
            "Special effects: ConsumeLaborPower without a skill (NullReferenceException)"
        );
        return;
    };
    let t = &skill.template;
    if t.consume_labor_power > 0 {
        let Some(p) = args.caster.as_ref().and_then(WorldObject::as_player) else {
            tracing::error!(
                "Special effects: ConsumeLaborPower caster is not a Character (InvalidCastException)"
            );
            return;
        };
        progress::change_labor(
            server,
            p,
            (-t.consume_labor_power) as i16,
            t.actability_group_id,
        );
    }
}

#[cfg(test)]
mod mana_cost_tests {
    use std::sync::Arc;

    use super::plot_mana_cost;

    #[test]
    fn plot_effect_matches_the_skill_tooltip_formula() {
        // skill 11854 (md 1.55, effect 0/155) at ability level 55: LevelMana data formula,
        // (ab_level * 1.6 + 8) * 3 / 3.65
        let scale = (55.0 * 1.6 + 8.0) * 3.0 / 3.65;
        let tooltip = scale * 1.55;
        assert!((plot_mana_cost(0, 155, scale, 0.0) - tooltip).abs() < 1e-9);
        // The rank term scales only the level-mana part, as `mana_cost` does.
        assert!((plot_mana_cost(0, 155, scale, 0.5) - tooltip * 1.5).abs() < 1e-9);
        // skill 18132 (mana_cost 30, md 0): fixed cost only
        assert_eq!(plot_mana_cost(30, 0, scale, 0.5), 30.0);
    }

    /// The item rule reaches the plot `ManaCost` effect: capture net 22590 (ability 0, item
    /// 26836) has plot effect 0/600, so a unit cast charges 600% of `LevelMana(55)` while the
    /// item use charges only the effect's fixed `value1` (0).
    #[tokio::test]
    async fn item_cast_skips_the_scaled_plot_charge() {
        use crate::world::WorldObject;

        use super::super::super::super::cast_validation_tests::fixture;
        use super::super::super::super::data::SkillTemplate;
        use super::super::super::super::geometry::Target;
        use super::super::super::super::skill_tests::formula_data;
        use super::super::super::super::state::SkillRun;
        use super::super::super::super::types::{
            CastAction, SkillCastTarget, SkillCaster, SkillObject,
        };
        use super::super::super::{EffectArgs, EffectSource};

        let (server, _client, player, npc) = fixture(formula_data());
        player.data().unit.level = 55;
        player.data().unit.mp = 2000;
        let caster = WorldObject::from(&player);
        let template = Arc::new(SkillTemplate {
            id: 22590,
            ability_id: 0,
            ability_level: 1,
            level_step: 1,
            mana_cost: 0,
            mana_level_md: 0.0,
            ..SkillTemplate::default()
        });
        let skill = SkillRun::new(template, 1);
        let skill_object = SkillObject::None;
        let args = |caster_caster| EffectArgs {
            caster: Some(caster.clone()),
            caster_caster,
            target: Target::Obj(WorldObject::from(&npc)),
            target_caster: SkillCastTarget::Unit(npc.obj_id),
            cast: CastAction::BuffTarget,
            source: EffectSource::from_skill(skill.clone()),
            skill_object: &skill_object,
        };
        super::mana_cost(
            &server,
            &args(SkillCaster::Unit(player.obj_id)),
            [0, 600, 0, 0],
        );
        let scale = (55.0 * 1.6 + 8.0) * 3.0 / 3.65;
        assert_eq!(player.data().unit.mp, 2000 - (6.0 * scale) as i32);
        // The plot effect charges mana but does not start the client's post-cast window: its
        // `OnPlotEvent` only does that for `StopManaRegen` (effect 52).
        assert!(!player.data().skill_char.is_in_post_cast);
        player.data().unit.mp = 2000;
        super::mana_cost(
            &server,
            &args(SkillCaster::Item {
                obj_id: player.obj_id,
                item_id: 1,
                item_template_id: 26836,
                type1: 0,
                type2: 0,
            }),
            [0, 600, 0, 0],
        );
        assert_eq!(player.data().unit.mp, 2000);
        assert!(!player.data().skill_char.is_in_post_cast);
    }
}
