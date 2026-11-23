use super::{
    add_own_buff, broadcast, buff, calculate_jail_time, char_record_kind, court_room_region,
    deg_to_rad, fmt_vec3, remove_from_queues, room_by_id, room_by_id_mut, set_jury_point, step,
    teleport_reason, util, world_pos_rot, AchievementInput, BTreeMap, CourtRoom, CrimeEvent,
    Effect, Effects, GameServer, JuryBox, PlayerRef, RecordMatch, SCAskImprisonOrTrialPacket,
    SCCharacterResurrectedPacket, SCCrimeDataPacket, SCCrimeRecordsPacket, SCForceAttackSetPacket,
    SCInviteJuryPacket, SCJuryBeSeatedPacket, SCJuryWaitStatusPacket, SCLoadInstancePacket,
    SCSummonDefendantPacket, SCSummonJuryPacket, SCTeleportUnitPacket, SCTrialInfoPacket,
    SCTrialWaitStatusPacket, SCUnitPointsPacket, SpawnPosition, TrialData, WorldObject,
    AWAITING_JURY_SUMMONS_MINUTES, MINUTE_MS, SECOND_MS, TIME_MAX, TRIAL_INIT_SECONDS,
};

/// `GenerateCrimeRecordsPacketList(trialId, unk1, events)`: one packet per 5 events.
pub fn generate_crime_records_packet_list(
    server: &GameServer,
    trial_id: u32,
    unk1: u32,
    events: &[CrimeEvent],
) -> Vec<SCCrimeRecordsPacket> {
    events
        .chunks(5)
        .map(|block| SCCrimeRecordsPacket {
            trial_id,
            unknown1: unk1,
            total_count: events.len() as i32,
            this_count: block.len() as i32,
            entries: block
                .iter()
                .map(|e| server.systems.social.crimes.record_entry(server, e))
                .collect(),
        })
        .collect()
}

/// `TrialData.SendAllTrialRecords(player)`.
fn send_all_trial_records(server: &GameServer, rooms: &[CourtRoom], t: &TrialData, p: &PlayerRef) {
    let info = {
        let c = t.defendant.data();
        SCTrialInfoPacket {
            player_id: t.defendant_id,
            crime_point: i32::from(c.crime_point),
            arrest: c.arrest_count,
            accept_guilty: c.accept_guilty_count,
            accept_trial: c.accept_trial_count,
            not_guilty: c.not_guilty_count,
            guilty: c.guilty_count,
            total: 0,
            cur: 0,
            crime_record_count: c.evidence_reported_count,
            bot_report: c.bot_reported_count,
        }
    };
    let (race, faction) = {
        let c = t.defendant.data();
        (c.race, c.unit.faction_id)
    };
    p.send(info);
    let evidence = server
        .systems
        .social
        .crimes
        .crimes_of_player(t.defendant_id, false);
    for pkt in generate_crime_records_packet_list(server, t.id, 0, &evidence) {
        p.send(pkt);
    }
    // `CourtRoom` is never null here (the C# would throw inside its try/catch).
    let Some(room) = room_by_id(rooms, t.court_room) else {
        return;
    };
    let judge_id = if room.judge_spawner.is_some() {
        server
            .systems
            .social
            .hooks
            .get()
            .judge_obj_id(server, room.id)
    } else {
        0
    };
    p.send(SCCrimeDataPacket {
        defendant_id: t.defendant_id,
        defendant_name: t.defendant_name.clone(),
        defendant_race: race,
        defendant_faction: faction,
        trial_id: t.id,
        sentence_time_ms: t.jail_time.wrapping_mul(60_000),
        judge_id,
    });
}

/// `GenerateJuryList(trial, maxCount)`.
pub(super) fn generate_jury_list(
    server: &GameServer,
    rooms: &[CourtRoom],
    queues: &[(u32, Vec<u32>)],
    t: &TrialData,
    max_count: usize,
) -> Vec<PlayerRef> {
    let mut res = Vec::new();
    if max_count == 0 {
        return res;
    }
    let Some(faction) = room_by_id(rooms, t.court_room).map(|r| r.faction) else {
        return res;
    };
    let Some((_, queue)) = queues.iter().find(|q| q.0 == faction) else {
        return res;
    };
    for &pid in queue {
        let Some(p) = server.world.character_by_id(pid) else {
            continue;
        };
        if !p.data().is_online {
            continue;
        }
        if pid == t.defendant_id {
            continue;
        }
        res.push(p);
        if res.len() >= max_count {
            break;
        }
    }
    res
}

pub(super) fn teleport(
    server: &GameServer,
    p: &PlayerRef,
    pos: SpawnPosition,
    reason: u8,
    fx: &mut Effects,
) {
    let zone_id = server
        .world
        .world(pos.world_id)
        .map_or(pos.zone_id, |w| w.zone_id_at(pos.x, pos.y));
    let destination = crate::models::transform::Transform {
        instance_id: pos.world_id,
        world_id: pos.world_id,
        zone_id,
        x: pos.x,
        y: pos.y,
        z: pos.z,
        rot_z: deg_to_rad(pos.yaw),
        ..Default::default()
    };
    if p.data().unit.transform.instance_id != destination.instance_id {
        p.send(SCLoadInstancePacket {
            instance_id: destination.world_id,
            zone_id: destination.zone_id,
            x: destination.x,
            y: destination.y,
            z: destination.z,
            ang_x: destination.rot_x,
            ang_y: destination.rot_y,
            ang_z: destination.rot_z,
        });
    }
    crate::world::session::send_teleport_unit_ending_trade(
        server,
        p,
        SCTeleportUnitPacket {
            reason,
            error_message: 0,
            x: pos.x,
            y: pos.y,
            z: pos.z,
            z2: destination.rot_z,
        },
    );
    fx.push(Effect::Teleport(p.clone(), destination));
}

/// `TrialData.EnterCourtJail(temporaryCourtRoom, defendant)`.
pub(super) fn enter_court_jail(
    server: &GameServer,
    t: &mut TrialData,
    room: &CourtRoom,
    fx: &mut Effects,
) {
    t.court_region = room.region;
    t.court_room = Some(room.id);
    let d = t.defendant.clone();
    let (hp, mp, inst) = {
        let mut c = d.data();
        t.defendant_id = c.id;
        t.defendant_name = c.unit.name.clone();
        // Make sure the criminal is alive.
        c.unit.hp = c.unit.hp.max(1);
        c.offline_guilty_region = i32::from(room.region);
        add_own_buff(
            server,
            &mut c,
            buff::FORCIBLY_AWAITING_TRIAL,
            0,
            util::now_ms(),
        );
        (c.unit.hp, c.unit.mp, c.unit.transform.instance_id)
    };
    let pr = world_pos_rot(server, &WorldObject::from(&d));
    if let Some(w) = server.world.world(inst) {
        broadcast(
            &w,
            &d,
            &SCCharacterResurrectedPacket {
                unit_id: d.obj_id,
                x: pr.x,
                y: pr.y,
                z: pr.z,
                z_rot: pr.rot_z,
            },
            true,
        );
        broadcast(
            &w,
            &d,
            &SCUnitPointsPacket {
                obj_id: d.obj_id,
                hp,
                mp,
            },
            true,
        );
    }
    fx.push(Effect::PostUpdateHp(d.clone(), 0, hp));
    // Teleport the criminal to the local jail.
    fx.push(Effect::ForceDismount(d.clone()));
    teleport(server, &d, room.jail, teleport_reason::LOCKUP, fx);
    let crime_point = d.data().crime_point;
    d.send(SCAskImprisonOrTrialPacket {
        crime_value: crime_point as u32,
        jail_minutes: t.jail_time,
    });
    d.send(SCTrialWaitStatusPacket {
        order: 0,
        sentence_time_ms: t.jail_time.wrapping_mul(60_000),
    });
    fx.push(Effect::DispatchBuffs(d));
}

/// `TrialData.EnterCourtRoom(courtRoom, defendant)` (+ `SendJuryInvites`).
pub(super) fn enter_court_room(
    server: &GameServer,
    rooms: &mut [CourtRoom],
    queues: &[(u32, Vec<u32>)],
    t: &mut TrialData,
    room_index: usize,
    now_ms: i64,
    fx: &mut Effects,
) {
    let d = t.defendant.clone();
    {
        let room = &mut rooms[room_index];
        room.current_trial = Some(t.id);
        t.court_room = Some(room.id);
        // Make jury seats.
        t.jury.clear();
        for (&i, seat) in &room.jury_seats {
            t.jury.insert(
                i,
                JuryBox {
                    seat: seat.clone(),
                    seat_id: i,
                    jury_member: None,
                    confirm_testimony: false,
                    selected_sentence: -1,
                },
            );
        }
    }
    let room = rooms[room_index].clone();
    {
        let mut c = d.data();
        t.defendant_id = c.id;
        t.defendant_name = c.unit.name.clone();
        // On Trial buff.
        c.unit.buffs.remove_buff(buff::FORCIBLY_AWAITING_TRIAL);
        add_own_buff(server, &mut c, buff::TRIAL_DEFENDANT, 0, now_ms);
    }
    teleport(server, &d, room.defendant, teleport_reason::DEFENDANT, fx);
    d.send(SCSummonDefendantPacket { trial: t.id });
    room.join_chat(&d);
    d.send(SCJuryWaitStatusPacket {
        count: 0,
        total: t.jury.len() as i32,
        sentence_time_ms: t.jail_time.wrapping_mul(60_000),
    });
    t.step = step::VERIFY_CRIMINAL_RECORD;
    t.current_step_end_ms = now_ms + TRIAL_INIT_SECONDS * SECOND_MS;
    d.send(t.state_packet(0, now_ms));
    send_all_trial_records(server, rooms, t, &d);
    t.step = step::AWAITING_JURY_SUMMONS;
    t.current_step_end_ms = now_ms + AWAITING_JURY_SUMMONS_MINUTES * MINUTE_MS;
    d.send(t.state_packet(0, now_ms));
    {
        let mut c = d.data();
        add_own_buff(server, &mut c, buff::CANNOT_ESCAPE, 0, now_ms);
    }
    d.send(SCForceAttackSetPacket {
        obj_id: d.obj_id,
        on: false,
    });
    fx.push(Effect::DispatchBuffs(d));
    // SendJuryInvites
    for p in generate_jury_list(server, rooms, queues, t, 10) {
        t.invited_jury.push(p.character_id);
        p.send(SCInviteJuryPacket {
            defendant_name: t.defendant_name.clone(),
            trial: t.id,
        });
    }
}

/// `TrialData.SummonJuryMember(player)`.
pub(super) fn summon_jury_member(
    server: &GameServer,
    rooms: &[CourtRoom],
    queues: &mut [(u32, Vec<u32>)],
    t: &mut TrialData,
    player: &PlayerRef,
    now_ms: i64,
    fx: &mut Effects,
) -> bool {
    let seat_ids: Vec<i32> = t.jury.keys().copied().collect();
    for seat_id in seat_ids {
        let seat = {
            let Some(jb) = t.jury.get_mut(&seat_id) else {
                continue;
            };
            if jb.jury_member.is_some() {
                continue;
            }
            jb.jury_member = Some(player.clone());
            jb.seat.clone()
        };
        remove_from_queues(queues, player.character_id);
        let Some(room) = room_by_id(rooms, t.court_room) else {
            tracing::error!("SummonJuryMember: trial {} has no court room", t.id);
            return false;
        };
        // Save the current position to return to after the trial (or on disconnect).
        let pr = world_pos_rot(server, &WorldObject::from(player));
        {
            let mut c = player.data();
            let t = c.unit.transform;
            c.main_world_position = Some(crate::models::transform::Transform {
                instance_id: t.instance_id,
                world_id: t.world_id,
                zone_id: t.zone_id,
                x: pr.x,
                y: pr.y,
                z: pr.z,
                rot_x: pr.rot_x,
                rot_y: pr.rot_y,
                rot_z: pr.rot_z,
                parent_obj_id: 0,
            });
        }
        player.send(SCSummonJuryPacket {
            trial: t.id,
            court: room.id,
            jury_number: seat_id,
        });
        // Teleport the jury.
        let seat_pr = world_pos_rot(server, &WorldObject::from(&seat));
        let seat_world_id = seat.data().transform.world_id;
        fx.push(Effect::ForceDismount(player.clone()));
        teleport(
            server,
            player,
            SpawnPosition {
                world_id: seat_world_id,
                x: seat_pr.x,
                y: seat_pr.y,
                z: seat_pr.z,
                yaw: seat_pr.rot_z.to_degrees(),
                ..Default::default()
            },
            teleport_reason::JURY,
            fx,
        );
        // Seat them down.
        player.send(SCJuryBeSeatedPacket {
            is_west: t.court_region == court_room_region::HARANYAN,
            trial: t.id,
            court: room.id as i32,
            jury_number: seat_id,
        });
        room.join_chat(player);
        {
            let mut c = player.data();
            add_own_buff(server, &mut c, buff::COURT_HOUSE, 0, now_ms);
            add_own_buff(server, &mut c, buff::JURY, 0, now_ms);
            add_own_buff(server, &mut c, buff::CANNOT_ESCAPE, 0, now_ms);
        }
        fx.push(Effect::DispatchBuffs(player.clone()));
        // Send them the trial info.
        send_all_trial_records(server, rooms, t, player);
        let count = t.active_jury_count();
        let total = t.jury.len() as i32;
        t.send_packets(&SCJuryWaitStatusPacket {
            count,
            total,
            sentence_time_ms: t.jail_time.wrapping_mul(60_000),
        });
        // End this step early if all seats are taken.
        if count >= total && t.step == step::AWAITING_JURY_SUMMONS && t.current_step_end_ms > now_ms
        {
            t.current_step_end_ms = now_ms;
        }
        return true;
    }
    false
}

/// `CreateTrialCase(defendant)`.
pub(super) fn create_trial_case(
    server: &GameServer,
    trials: &mut BTreeMap<u32, TrialData>,
    defendant: &PlayerRef,
    court_region: u8,
) -> Option<u32> {
    let id = match server.ids.trial.get_next_id() {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("CreateTrialCase: {e:?}");
            return None;
        }
    };
    let (did, name) = {
        let c = defendant.data();
        (c.id, c.unit.name.clone())
    };
    let mut t = TrialData {
        id,
        court_region,
        court_room: None,
        step: step::DEFENDANT_AWAITING_TRIAL,
        defendant: defendant.clone(),
        defendant_id: did,
        defendant_name: name,
        current_step_end_ms: TIME_MAX,
        jury: BTreeMap::new(),
        invited_jury: Vec::new(),
        jail_time: 15,
        evidence: Vec::new(),
    };
    tracing::debug!("TrialStep.DefendantAwaitingTrial - {id}");
    t.evidence = server.systems.social.crimes.crimes_of_player(did, false);
    calculate_jail_time(&mut t);
    if trials.contains_key(&id) {
        return None;
    }
    trials.insert(id, t);
    Some(id)
}

/// The jury part of the `EndTrial` cleanup for one member.
pub(super) fn cleanup_trial(
    server: &GameServer,
    rooms: &mut [CourtRoom],
    t: &TrialData,
    reward: bool,
    fx: &mut Effects,
) {
    for j in t.jury.values() {
        if let Some(m) = &j.jury_member {
            cleanup_jury_member(server, rooms, t, m, reward && j.selected_sentence >= 0, fx);
        }
    }
    if let Some(room) = room_by_id_mut(rooms, t.court_room) {
        room.leave_chat(&t.defendant);
        if room.current_trial == Some(t.id) {
            room.current_trial = None;
        }
    }
    {
        let mut c = t.defendant.data();
        c.unit.buffs.remove_buff(buff::FORCIBLY_AWAITING_TRIAL);
        c.unit.buffs.remove_buff(buff::TRIAL_DEFENDANT);
        c.unit.buffs.remove_buff(buff::CANNOT_ESCAPE);
    }
    fx.push(Effect::DispatchBuffs(t.defendant.clone()));
}

pub(super) fn cleanup_jury_member(
    server: &GameServer,
    rooms: &[CourtRoom],
    t: &TrialData,
    m: &PlayerRef,
    reward: bool,
    fx: &mut Effects,
) {
    let escaped = server.config.justice.allow_jury_escape
        && !m.data().unit.buffs.check_buff(buff::COURT_HOUSE);
    if escaped {
        // The jury is allowed to escape: only cancel the MainWorldPosition.
        m.data().main_world_position = None;
    } else {
        let cur = world_pos_rot(server, &WorldObject::from(m));
        let (name, main) = {
            let c = m.data();
            (c.unit.name.clone(), c.main_world_position)
        };
        let pos = main.map_or((cur.x, cur.y, cur.z), |m| (m.x, m.y, m.z));
        let cur3 = (cur.x, cur.y, cur.z);
        tracing::debug!(
            "Returning {name} to {} <- {}",
            fmt_vec3(pos),
            fmt_vec3(cur3)
        );
        fx.push(Effect::ForceDismount(m.clone()));
        m.data().set_disabled_set_position(true);
        tracing::debug!(
            "Returning jury member {name} to their old position {}",
            fmt_vec3(pos)
        );
        let world_id = main.map_or_else(|| m.data().unit.transform.world_id, |p| p.world_id);
        teleport(
            server,
            m,
            SpawnPosition {
                world_id,
                x: pos.0,
                y: pos.1,
                z: pos.2,
                yaw: main.map_or(cur.rot_z, |p| p.rot_z).to_degrees(),
                ..Default::default()
            },
            teleport_reason::JURY,
            fx,
        );
        let pkt = {
            let mut c = m.data();
            c.main_world_position = None;
            // Update the trials attended count.
            if reward {
                let v = c.jury_point.saturating_add(1);
                set_jury_point(&mut c, v)
            } else {
                None
            }
        };
        tracing::debug!("{name} back at {}", fmt_vec3(cur3));
        if let Some(pkt) = pkt {
            m.send(pkt);
            // GET_JURY_POINT: the point awarded for the finished trial.
            fx.push(Effect::Achievement(
                m.clone(),
                AchievementInput::Increment {
                    kind_id: char_record_kind::GET_JURY_POINT,
                    amount: 1,
                    matcher: RecordMatch::Any,
                },
            ));
        }
    }
    {
        let mut c = m.data();
        c.unit.buffs.remove_buff(buff::COURT_HOUSE);
        c.unit.buffs.remove_buff(buff::JURY);
        c.unit.buffs.remove_buff(buff::CANNOT_ESCAPE);
    }
    fx.push(Effect::DispatchBuffs(m.clone()));
    if let Some(room) = room_by_id(rooms, t.court_room) {
        room.leave_chat(m);
    }
}
