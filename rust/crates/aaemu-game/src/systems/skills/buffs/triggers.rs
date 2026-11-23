use crate::models::buff::Buff;
use crate::server::GameServer;
use crate::systems::instances::encounters::registry as encounters;
use crate::systems::instances::encounters::registry::TriggerCaster;
use crate::world::WorldObject;

use super::super::data::BuffTriggerTemplate;
use super::super::effects;
use super::super::effects::{EffectArgs, EffectSource};
use super::super::geometry::Target;
use super::super::types::{
    BuffEventTriggerKind, CastAction, SkillCastTarget, SkillCaster, SkillObject,
};
use super::{buff_skill, check_buff_tag, find_obj, full_template, get_buff};

// ------------------------------------------------------------------ triggers

/// One subscribed `BuffTrigger` (`BuffTriggersHandler._triggers` entry).
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerSub {
    /// The buff owner whose unit events are listened to, 0 for buff events.
    pub source_obj: u32,
    pub owner_obj: u32,
    pub buff_index: u32,
    /// Internal buff lifetime, since an explicit index can be reused by a callback.
    pub buff_instance_id: u64,
    pub template: BuffTriggerTemplate,
    /// `DamagedBuffTrigger` (else the generic `BuffTrigger`).
    pub damaged: bool,
    /// `DamageBuffTrigger` (event 3, damage dealt by the owner): it carries the event amount
    /// like `damaged`, but targets the owner as the C# trigger does.
    pub damage: bool,
}

/// `BuffTriggersHandler.SubscribeEvents`.
pub(super) fn subscribe_triggers(server: &GameServer, owner: &WorldObject, b: &Buff) {
    let data = &server.game_data.systems.skills;
    let templates = data.buff_triggers_for(b.template.id);
    if templates.is_empty() {
        return;
    }
    // `BuffTrigger.Execute` only warns and returns when the owner is not a Unit (a doodad hit by
    // an area skill), so such a trigger can never run: do not subscribe it.
    if !owner.is_unit() {
        tracing::trace!(
            "Buff {} on non-unit owner {}: triggers not subscribed",
            b.template.id,
            owner.obj_id()
        );
        return;
    }
    let mut subs = server
        .systems
        .skills
        .triggers
        .lock()
        .expect("triggers poisoned");
    use BuffEventTriggerKind as K;
    for t in templates {
        let kind = K::from_raw(i64::from(t.kind));
        let (unit_event, damaged, damage) = match kind {
            Some(K::Damage) => (true, false, true),
            Some(
                K::Attack
                | K::Attacked
                | K::Death
                | K::Landing
                | K::ChannelingCancel
                | K::Unmount
                | K::Kill
                | K::KillAny
                | K::DamagedCollision
                | K::Immotality
                | K::Time,
            ) => (true, false, false),
            Some(
                K::Damaged | K::DamagedMelee | K::DamagedRanged | K::DamagedSpell | K::DamagedSiege,
            ) => (true, true, false),
            Some(K::Dispelled | K::Timeout | K::Started | K::RemoveOnMove | K::RemoveOnDamage) => {
                (false, false, false)
            }
            _ => {
                tracing::trace!("Unimplemented BuffTrigger[\"{:?}\"]", kind);
                continue;
            }
        };
        subs.push(TriggerSub {
            source_obj: if unit_event { owner.obj_id() } else { 0 },
            owner_obj: owner.obj_id(),
            buff_index: b.index,
            buff_instance_id: b.instance_id,
            template: t.clone(),
            damaged,
            damage,
        });
    }
}

/// `BuffTriggersHandler.UnsubscribeEvents` (fires `OnDispelled` first).
pub(super) fn unsubscribe_triggers(server: &GameServer, owner: &WorldObject, b: &Buff) {
    fire_buff_trigger_with(server, owner, b, BuffEventTriggerKind::Dispelled);
    let owner_id = owner.obj_id();
    server
        .systems
        .skills
        .triggers
        .lock()
        .expect("triggers poisoned")
        .retain(|s| {
            !(s.owner_obj == owner_id
                && s.buff_index == b.index
                && s.buff_instance_id == b.instance_id)
        });
}

/// Fires a buff-level event (`Started`, `Timeout`, `Dispelled`) of the buff at `index`.
pub(super) fn fire_buff_trigger(
    server: &GameServer,
    owner: &WorldObject,
    index: u32,
    kind: BuffEventTriggerKind,
) {
    if let Some(b) = get_buff(owner, index) {
        fire_buff_trigger_with(server, owner, &b, kind);
    }
}

fn fire_buff_trigger_with(
    server: &GameServer,
    owner: &WorldObject,
    b: &Buff,
    kind: BuffEventTriggerKind,
) {
    let owner_id = owner.obj_id();
    let subs: Vec<TriggerSub> = server
        .systems
        .skills
        .triggers
        .lock()
        .expect("triggers poisoned")
        .iter()
        .filter(|s| {
            s.owner_obj == owner_id
                && s.buff_index == b.index
                && s.buff_instance_id == b.instance_id
                && s.source_obj == 0
                && s.template.kind == kind as u16
        })
        .cloned()
        .collect();
    for s in subs {
        execute_trigger(server, owner, b, &s, 0);
    }
}

/// Fires a unit event (`OnAttack`, `OnDamage`, `OnDamaged*`, `OnDeath`) of `unit` for every
/// trigger subscribed to it.
pub fn fire_unit_event(server: &GameServer, unit: u32, kind: BuffEventTriggerKind, amount: i32) {
    let subs: Vec<TriggerSub> = server
        .systems
        .skills
        .triggers
        .lock()
        .expect("triggers poisoned")
        .iter()
        .filter(|s| s.source_obj == unit && s.template.kind == kind as u16)
        .cloned()
        .collect();
    for s in subs {
        let Some(owner) = find_obj(server, s.owner_obj) else {
            continue;
        };
        let Some(b) = get_buff(&owner, s.buff_index) else {
            continue;
        };
        if b.instance_id != s.buff_instance_id {
            continue;
        }
        execute_trigger(server, &owner, &b, &s, amount);
    }
}

/// `BuffTrigger.Execute` / `DamagedBuffTrigger.Execute`.
fn execute_trigger(
    server: &GameServer,
    owner: &WorldObject,
    b: &Buff,
    s: &TriggerSub,
    amount: i32,
) {
    if !owner.is_unit() {
        tracing::warn!(
            "Owner is not a Unit (buff {}, owner obj {})",
            b.template.id,
            owner.obj_id()
        );
        return;
    }
    let data = &server.game_data.systems.skills;
    let Some(effect) = data.effect(s.template.effect.0, s.template.effect.1) else {
        return;
    };
    let t = &s.template;
    let caster = find_obj(server, b.caster_obj_id);
    let caster_source = encounters::trigger_source_is_caster(
        b.template.id,
        || {
            caster
                .as_ref()
                .and_then(WorldObject::as_doodad)
                .map(|doodad| doodad.data().template_id)
        },
        || {
            caster
                .as_ref()
                .and_then(WorldObject::as_npc)
                .map(|npc| npc.data().unit.template_id)
        },
    );
    // The floor spear's Injured trigger is authored with the victim as its
    // source. Use the trap that applied it so self-attack rules do not stop
    // the environmental damage.
    let floor_spear = caster_source == Some(TriggerCaster::FloorSpear);
    // The tombstone marks its victim, but Lady Velarre casts the follow-up
    // skill that owns the hostile souls.
    let velarre_tombstone = caster_source == Some(TriggerCaster::VelarreTombstone);
    let mut target = Some(owner.clone());
    let mut source = Some(owner.clone());
    if t.use_original_source || floor_spear || velarre_tombstone {
        source = caster.clone();
    }
    if t.effect_on_source {
        target = source.clone();
    }
    let Some(target) = target else {
        return;
    };
    if t.target_buff_tag_id != 0 && !check_buff_tag(server, &target, t.target_buff_tag_id) {
        return;
    }
    if t.target_no_buff_tag_id != 0 && check_buff_tag(server, &target, t.target_no_buff_tag_id) {
        return;
    }
    let template = full_template(server, &b.template);
    let skill_object = SkillObject::None;
    // `DamagedBuffTrigger` and `DamageBuffTrigger` both apply the effect from the owner with
    // the buff template as the source and the event's damage amount. The C# `DamageBuffTrigger`
    // uses `args.Attacker`, which `OnDamageArgs` fills with the damage caster itself, so the
    // target stays the owner here too.
    let (effect_caster, source_info) = if s.damaged || s.damage {
        (
            Some(owner.clone()),
            EffectSource {
                skill: None,
                buff: Some(template.clone()),
                cast_target: None,
                range_multiplier: None,
                height_multiplier: None,
                amount,
                is_trigger: true,
                buff_permanent: false,
                use_damage_amount: t.use_damage_amount,
                always_hit: false,
                source_item: None,
                source_item_debited: false,
            },
        )
    } else {
        (
            source,
            EffectSource {
                skill: buff_skill(server, b),
                buff: None,
                cast_target: None,
                range_multiplier: None,
                height_multiplier: None,
                amount: 0,
                is_trigger: false,
                buff_permanent: false,
                use_damage_amount: t.use_damage_amount,
                always_hit: false,
                source_item: None,
                source_item_debited: false,
            },
        )
    };
    let args = EffectArgs {
        caster: effect_caster,
        caster_caster: if floor_spear {
            SkillCaster::Doodad(b.caster_obj_id)
        } else if velarre_tombstone {
            SkillCaster::Unit(b.caster_obj_id)
        } else {
            SkillCaster::Unit(owner.obj_id())
        },
        target: Target::Obj(target.clone()),
        target_caster: SkillCastTarget::Unit(target.obj_id()),
        cast: CastAction::Buff {
            buff_id: template.id,
            owner_obj_id: owner.obj_id(),
            index: b.index,
        },
        source: source_info,
        skill_object: &skill_object,
    };
    effects::apply(server, s.template.effect.0, effect, &args, None);
}
