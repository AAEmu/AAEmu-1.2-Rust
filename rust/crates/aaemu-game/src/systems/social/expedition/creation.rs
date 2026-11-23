use super::{
    broadcast_self, default_policies, error, expedition_of, guild_chat, item_task_money_packet,
    member_from_character, on_character_login, rename, save, send_error, send_expedition_info,
    util, validate_name, Expedition, GameServer, PlayerRef, SCFactionCreatedPacket,
    SCFactionListPacket, SCUnitExpeditionChangedPacket, SystemFaction,
    ITEM_TASK_EXPEDITION_CREATION,
};

/// The sponsor factions a guild can be founded under, by the owner's alliance (`MotherId` of
/// its faction; the outlaw faction is its own alliance): Nuia 148, Haranya 149, outlaws 114.
/// Taken from the 1.2 client's expedition creation dialog.
pub fn valid_sponsors(alliance: u32) -> &'static [u32] {
    match alliance {
        148 => &[101, 102, 103, 105, 106, 107],
        149 => &[108, 109, 110, 111, 113],
        114 => &[161],
        _ => &[],
    }
}

/// `ExpeditionManager.CreateExpedition(name, connection)`.
///
/// `sponsor` is the faction id the client's dialog sends (`CSCreateExpedition.id`): one of
/// [`valid_sponsors`] of the owner's alliance, and it becomes the guild's `mother_id`. Deviation
/// from the C#, which dropped it and used the owner's own faction (0 = a client that sends none:
/// the owner's faction as before). Also unlike the C#: a party member of another alliance is
/// `EXPEDITION_CREATE_MEMBER_FACTION` (320, not 321), the name block list applies and names are
/// unique case-insensitively, and only members that are online and in the world join.
pub async fn create_expedition(server: &GameServer, owner: &PlayerRef, name: &str, sponsor: u32) {
    let cfg = &server.config.expedition;
    if expedition_of(server, owner).is_some() {
        send_error(owner, error::EXPEDITION_ALREADY_MEMBER);
        return;
    }
    if let Err(code) = validate_name(server, name, 0) {
        send_error(owner, code);
        return;
    }
    if rename::faction_name_in_use(server, name, None) {
        send_error(owner, error::EXPEDITION_NAME_EXIST);
        return;
    }
    // ----------------- Conditions, can change this...
    let Some(team_members) = server
        .systems
        .social
        .teams
        .team_characters_of(owner.character_id)
    else {
        // We send the same error on number of party members when we don't have a party
        send_error(owner, error::EXPEDITION_CREATE_MEMBER);
        return;
    };
    // The saved faction, not a temporary duel / battlefield team faction (NATION-14). The
    // sponsor list is the owner's alliance; the members must share its top-level faction, with
    // the runtime nations overlaid: a nation's top-level faction is itself, so two citizens of
    // different nations are not grouped like two factions of one alliance (PORTING.md,
    // NATION-13).
    let owner_faction = crate::db_character::persisted_faction(&owner.data());
    let owner_alliance = util::alliance_id(server, owner_faction);
    let owner_top =
        super::nation::top_level_faction(server, owner_faction).unwrap_or(owner_alliance);
    let mother_id = if sponsor == 0 {
        owner_faction
    } else if valid_sponsors(owner_alliance).contains(&sponsor) {
        sponsor
    } else {
        send_error(owner, error::EXPEDITION_CREATE_FACTION);
        return;
    };
    let mut valid: Vec<PlayerRef> = Vec::new();
    for m in team_members {
        let (level, exp, faction, online) = {
            let c = m.data();
            (
                c.unit.level,
                c.unit.expedition_id,
                crate::db_character::persisted_faction(&c),
                c.is_online,
            )
        };
        // A team member that is offline (or not in the world) does not join.
        if !online && m.character_id != owner.character_id {
            continue;
        }
        if level < cfg.create.level {
            send_error(owner, error::EXPEDITION_CREATE_LEVEL);
            return;
        }
        if exp != 0 && server.expeditions.exists(exp) {
            send_error(owner, error::EXPEDITION_CREATE_MEMBER_EXPEDITION);
            return;
        }
        let member_top = super::nation::top_level_faction(server, faction)
            .unwrap_or_else(|| util::alliance_id(server, faction));
        if member_top != owner_top {
            send_error(owner, error::EXPEDITION_CREATE_MEMBER_FACTION);
            return;
        }
        valid.push(m);
    }
    if valid.len() < usize::from(cfg.create.party_member_count) {
        send_error(owner, error::EXPEDITION_CREATE_MEMBER);
        return;
    }
    let cost = i64::from(cfg.create.cost);
    {
        let mut c = owner.data();
        if c.money < cost {
            drop(c);
            send_error(owner, error::EXPEDITION_CREATE_MONEY);
            return;
        }
        c.money -= cost;
    }
    owner.send(item_task_money_packet(
        ITEM_TASK_EXPEDITION_CREATION,
        -cfg.create.cost,
    ));
    // -----------------
    let id = match server.ids.expedition.get_next_id() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("{e}");
            return;
        }
    };
    let (owner_name, owner_obj) = {
        let c = owner.data();
        (c.unit.name.clone(), owner.obj_id)
    };
    let expedition = Expedition {
        faction: SystemFaction {
            id,
            mother_id,
            name: name.to_string(),
            owner_id: owner.character_id,
            owner_name: owner_name.clone(),
            unit_owner_type: 0,
            political_system: 1,
            created: util::now_secs(),
            aggro_link: false,
            diplomacy_target: false,
            diplomacy_link_id: 0,
            guard_help: false,
            allow_change_name: 0,
        },
        members: vec![member_from_character(server, id, owner, true)],
        policies: default_policies(server, id),
        removed_members: Vec::new(),
        is_disbanded: false,
    };
    {
        // The early check above can be overtaken by a concurrent rename; the name lock makes
        // the final check and the insert one step (RENAME-6).
        let _names = rename::lock_names(server);
        if rename::faction_name_in_use(server, name, None) {
            owner.data().money += cost;
            owner.send(item_task_money_packet(
                ITEM_TASK_EXPEDITION_CREATION,
                cfg.create.cost,
            ));
            send_error(owner, error::EXPEDITION_NAME_EXIST);
            return;
        }
        server.expeditions.add(expedition.clone());
    }
    owner.data().unit.expedition_id = id;
    // ENROLL_GUILD (queued: runs once the packet is handled).
    crate::systems::refresh_achievements_later(server, owner);

    owner.send(SCFactionCreatedPacket {
        faction: expedition.faction.clone(),
        owner_obj_id: owner_obj,
        members: vec![(owner_obj, owner.character_id, owner_name.clone())],
    });
    server.world.broadcast_to_server(&SCFactionListPacket {
        factions: vec![expedition.faction.clone()],
    });
    broadcast_self(
        server,
        owner,
        &SCUnitExpeditionChangedPacket {
            obj_id: owner_obj,
            character_id: owner.character_id,
            kicker: String::new(),
            unit_name: owner_name,
            id: 0,
            expedition_id: id,
            expel: false,
        },
    );
    guild_chat(server, id, name).join(owner);
    send_expedition_info(owner, &expedition);

    for m in valid {
        if m.character_id == owner.character_id {
            continue;
        }
        let member = member_from_character(server, id, &m, false);
        m.data().unit.expedition_id = id;
        crate::systems::refresh_achievements_later(server, &m);
        let snapshot = server
            .expeditions
            .with_mut(id, |e| {
                e.members.push(member);
                e.clone()
            })
            .expect("just added");
        let unit_name = m.data().unit.name.clone();
        broadcast_self(
            server,
            &m,
            &SCUnitExpeditionChangedPacket {
                obj_id: m.obj_id,
                character_id: m.character_id,
                kicker: String::new(),
                unit_name,
                id: 0,
                expedition_id: id,
                expel: false,
            },
        );
        send_expedition_info(&m, &snapshot);
        on_character_login(server, &m, id);
    }
    save(server, id).await;
}
