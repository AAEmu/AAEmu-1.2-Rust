use super::{AchievementInput, BuffKind, CrimeFactionChange, GameServer, PlayerRef, WorldObject};

/// Work deferred until the trial mutex is released (see the module docs).
#[derive(Debug)]
pub enum Effect {
    /// `systems::dispatch_buff_events` after buffs changed.
    DispatchBuffs(PlayerRef),
    /// `Character.PostUpdateCurrentHp(self, oldHp, newHp, KillReason.PvpEnemy)`.
    PostUpdateHp(PlayerRef, i32, i32),
    /// `Character.ForceDismount()`.
    ForceDismount(PlayerRef),
    /// `Buffs.RemoveBuffs(kind, count, tag)`.
    RemoveBuffsByTag(PlayerRef, BuffKind, i32, u32),
    /// The faction consequences of a crime-point change (`SetFaction(Pirate)` / the infamy-0
    /// pardon); run with [`crate::systems::social::faction::apply_crime_faction_change`].
    CrimeFaction(PlayerRef, CrimeFactionChange),
    /// Update world membership and visibility after a justice teleport, outside the trial lock.
    Teleport(PlayerRef, crate::models::transform::Transform),
    /// An achievement event for a trial participant (JUDGEMENT, GET_JURY_POINT).
    Achievement(PlayerRef, AchievementInput),
}

pub(super) type Effects = Vec<Effect>;

/// Runs deferred effects (the trial mutex is already released).
pub(super) fn run_effects(server: &GameServer, fx: Effects) {
    for e in fx {
        run_effect(server, e);
    }
}

fn run_effect(server: &GameServer, e: Effect) {
    let hooks = server.systems.social.hooks.get();
    match e {
        Effect::DispatchBuffs(p) => {
            crate::systems::dispatch_buff_events(server, &WorldObject::from(&p));
        }
        Effect::PostUpdateHp(p, old, new) => hooks.post_update_current_hp(server, &p, old, new),
        Effect::ForceDismount(p) => hooks.force_dismount(server, &p),
        Effect::RemoveBuffsByTag(p, kind, count, tag) => {
            hooks.remove_buffs_by_tag(server, &p, kind, count, tag);
            crate::systems::dispatch_buff_events(server, &WorldObject::from(&p));
        }
        Effect::Achievement(p, input) => crate::systems::report_achievement(server, &p, input),
        Effect::CrimeFaction(p, change) => {
            crate::systems::social::faction::apply_crime_faction_change(server, &p, change);
        }
        Effect::Teleport(p, destination) => {
            let old_instance = p.data().unit.transform.instance_id;
            let Some(world) = server.world.world(destination.instance_id) else {
                tracing::error!(
                    "Justice teleport: missing instance {}",
                    destination.instance_id
                );
                return;
            };
            if old_instance != destination.instance_id {
                if let Some(old) = server.world.world(old_instance) {
                    old.delete(&p);
                }
            }
            crate::world::session::teleport_character(server, &p, destination);
            let changes = if old_instance == destination.instance_id {
                world.add_visible_object(&p, &server.view_ctx())
            } else {
                world.spawn(&p, &server.view_ctx())
            };
            crate::world::session::handle_zone_changes(server, changes);
            crate::systems::social::session::on_moved(server, &p);
        }
    }
}
