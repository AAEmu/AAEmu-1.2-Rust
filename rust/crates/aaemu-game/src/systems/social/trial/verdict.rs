use super::{
    add_own_buff, buff, char_record_kind, court_room_region, crime_state, factions, room_by_id,
    sentence_result, set_crime_point, set_infamy_point, step, teleport, teleport_reason,
    AchievementInput, BuffKind, CourtRoom, Effect, Effects, GameServer, PlayerRef, RecordMatch,
    SCCrimeChangedPacket, SCRulingStatusPacket, TrialData, END_TRIAL_SECONDS, MAX_JAIL_MINUTES,
    SECOND_MS,
};

/// `CalculateJailTime()`.
///
/// The retail rule (2014 community documentation) is `(crime_points / 5) * (1 + infamy / 1000)`
/// in integer arithmetic. The 2014-10-31 trial video confirms the `/ 5` part: the defendant's
/// record is 76 crime points / 26 trial records, the trial dialogs' text shows 15 minutes (assumed to be this defendant's)
/// (= 76 / 5), and the verdict's maximum option (the C# 1.2x factor) is the 18 minutes the judge
/// announces. The C# replaced this with a per-evidence score (murder 20 / theft 8, x10 for
/// victims under level 30) because it took the jury's scaled sentence for the base; that score
/// gives 164 minutes for the same record. The dialog's amount and the packet's `jail_minutes`
/// are this base, not an extra penalty (ui_text 2666: "a $2 minute sentence will be automatically
/// enforced"). The video's infamy was below 1000, so the `/ 1000` term and the non-maximum jury
/// factors keep the 2014 formula / C# guesses; they are not pinned by this video.
pub(super) fn calculate_jail_time(t: &mut TrialData) {
    let (crime_point, infamy, faction) = {
        let c = t.defendant.data();
        (c.crime_point, c.infamy_point, c.unit.faction_id)
    };
    let minutes = i32::from(crime_point) / 5;
    t.jail_time = minutes
        .saturating_mul(1 + infamy / 1000)
        .clamp(0, MAX_JAIL_MINUTES);
    if faction == factions::PIRATE {
        t.jail_time = 40;
    }
    let mut c = t.defendant.data();
    // Pending sentences are stored in whole minutes. Even a zero-minute case must not
    // disappear by disconnecting before the minimum prison buff is applied.
    c.offline_guilty_time = t.jail_time.max(1);
    c.offline_guilty_region = i32::from(t.court_region);
}

/// `TrialData.ArchiveEvidence()`.
fn archive_trial_evidence(server: &GameServer, t: &TrialData, now_ms: i64) {
    let judge_time = now_ms.div_euclid(1000);
    let crimes = &server.systems.social.crimes;
    for e in &t.evidence {
        if server.config.justice.keep_history {
            crimes.archive_evidence(e.id, judge_time);
        } else {
            crimes.remove_evidence(e.id);
        }
    }
}

/// `ResultIsGuilty(defendant, trial, pleadGuilty)`.
pub(super) fn result_is_guilty(
    server: &GameServer,
    rooms: &[CourtRoom],
    defendant: &PlayerRef,
    t: &mut TrialData,
    plead_guilty: bool,
    now_ms: i64,
    fx: &mut Effects,
) {
    if t.step >= step::PLEAD_GUILTY {
        return;
    }
    tracing::debug!("TrialStep.EndTrial - {}", t.id);
    let Some(room) = room_by_id(rooms, t.court_room) else {
        tracing::error!("ResultIsGuilty: trial {} has no court room", t.id);
        return;
    };
    // Find the jail to go to.
    let Some(jail) = server
        .config
        .justice
        .jails
        .iter()
        .find(|j| j.faction == room.faction)
    else {
        let name = defendant.data().unit.name.clone();
        tracing::error!(
            "Could not find a jail location for Court: {}, Defendant: {name}",
            room.name
        );
        return;
    };
    t.step = if plead_guilty {
        step::PLEAD_GUILTY
    } else {
        step::END_TRIAL
    };
    t.current_step_end_ms = now_ms + END_TRIAL_SECONDS * SECOND_MS;
    t.send_state(now_ms);
    room.leave_chat(defendant);
    archive_trial_evidence(server, t, now_ms);
    let (pkt, change) = {
        let mut c = defendant.data();
        let crime_count = c.crime_point;
        c.unit.buffs.remove_buff(buff::FORCIBLY_AWAITING_TRIAL);
        c.unit.buffs.remove_buff(buff::CANNOT_ESCAPE);
        c.offline_guilty_time = 0;
        c.offline_guilty_region = i32::from(court_room_region::INVALID);
        let v = c.crime_point.wrapping_sub(crime_count);
        let change = set_crime_point(server, &mut c, v, now_ms);
        if plead_guilty {
            c.accept_guilty_count += 1;
        } else {
            c.guilty_count += 1;
        }
        (
            SCCrimeChangedPacket {
                points: i32::from(crime_count),
                crime_points: c.crime_point,
                infamy_points: c.infamy_point,
                crime_state: crime_state(&c),
            },
            change,
        )
    };
    defendant.send(pkt);
    if !change.is_empty() {
        // Deferred: the trial mutex is still held (module rule, lines 5-13), and the faction
        // change re-enters the world / guild / team subsystems.
        fx.push(Effect::CrimeFaction(defendant.clone(), change));
    }
    // Prisoner buff (minimum 10 s: 0 would use the default 30 minutes).
    let jail_time_ms = t.jail_time.saturating_mul(60_000).max(10_000);
    {
        let mut c = defendant.data();
        match t.court_region {
            court_room_region::NUIAN => {
                add_own_buff(server, &mut c, buff::PRISONER_NUIAN, jail_time_ms, now_ms)
            }
            court_room_region::HARANYAN => add_own_buff(
                server,
                &mut c,
                buff::PRISONER_HARANYAN,
                jail_time_ms,
                now_ms,
            ),
            r => tracing::error!(
                "ResultIsGuilty - Invalid court region? {r}, defendant: {}",
                t.defendant_name
            ),
        }
        c.set_disabled_set_position(true);
    }
    fx.push(Effect::ForceDismount(defendant.clone()));
    teleport(server, defendant, jail.pos, teleport_reason::JAIL, fx);
    defendant
        .data()
        .unit
        .buffs
        .remove_buff(buff::TRIAL_DEFENDANT);
    fx.push(Effect::DispatchBuffs(defendant.clone()));
}

/// `ResultIsNotGuilty(defendant, trial)`.
fn result_is_not_guilty(
    server: &GameServer,
    defendant: &PlayerRef,
    t: &mut TrialData,
    now_ms: i64,
    fx: &mut Effects,
) {
    tracing::debug!("TrialStep.EndTrial - {}", t.id);
    t.step = step::END_TRIAL;
    t.current_step_end_ms = now_ms + END_TRIAL_SECONDS * SECOND_MS;
    t.send_state(now_ms);
    archive_trial_evidence(server, t, now_ms);
    let (pkt, change) = {
        let mut c = defendant.data();
        let crime_count = c.crime_point;
        c.unit.buffs.remove_buff(buff::FORCIBLY_AWAITING_TRIAL);
        c.unit.buffs.remove_buff(buff::CANNOT_ESCAPE);
        c.offline_guilty_time = 0;
        c.offline_guilty_region = i32::from(court_room_region::INVALID);
        c.not_guilty_count += 1;
        let v = c.crime_point.wrapping_sub(crime_count);
        let mut change = set_crime_point(server, &mut c, v, now_ms);
        let v = c.infamy_point.wrapping_sub(i32::from(crime_count));
        change = change.merge(set_infamy_point(server, &mut c, v, now_ms));
        (
            SCCrimeChangedPacket {
                points: i32::from(crime_count),
                crime_points: c.crime_point,
                infamy_points: c.infamy_point,
                crime_state: crime_state(&c),
            },
            change,
        )
    };
    defendant.send(pkt);
    if !change.is_empty() {
        // Deferred: the trial mutex is still held (module rule, lines 5-13).
        fx.push(Effect::CrimeFaction(defendant.clone(), change));
    }
    fx.push(Effect::RemoveBuffsByTag(
        defendant.clone(),
        BuffKind::Good,
        1,
        buff::TAG_PRISONER,
    ));
    defendant
        .data()
        .unit
        .buffs
        .remove_buff(buff::TRIAL_DEFENDANT);
    fx.push(Effect::DispatchBuffs(defendant.clone()));
}

/// `TrialData.FinalizeVerdict()`.
pub(super) fn finalize_verdict(
    server: &GameServer,
    rooms: &[CourtRoom],
    t: &mut TrialData,
    now_ms: i64,
    fx: &mut Effects,
) {
    let mut total = 0;
    let mut selected_count = 0;
    let mut votes: Vec<(i32, i32)> = Vec::new();
    for j in t.jury.values() {
        if let Some(m) = &j.jury_member {
            total += 1;
            if j.selected_sentence >= 0 {
                match votes.iter_mut().find(|v| v.0 == j.selected_sentence) {
                    Some(v) => v.1 += 1,
                    None => votes.push((j.selected_sentence, 1)),
                }
                selected_count += 1;
            }
            m.data().unit.buffs.remove_buff(buff::JURY);
            fx.push(Effect::DispatchBuffs(m.clone()));
        }
    }
    let mut most_voted = 3; // default guilty
    let mut most_voted_count = 0;
    for &(k, c) in &votes {
        if c > most_voted_count {
            most_voted = k;
            most_voted_count = c;
        }
    }
    let is_guilty = most_voted > 1;
    // The five guilt buttons scale the base sentence. The C# guessed these rates; only the
    // top rate (1.2x) is pinned by the 2014-10-31 video (15 base -> the announced 18), the
    // other rates stay capture-pending.
    let round = |f: f32| f64::from(t.jail_time as f32 * f).round_ties_even() as i32;
    let (sentence_type, sentence_time) = match most_voted {
        0 | 1 => (sentence_result::NOT_GUILTY, 0),
        2 => (sentence_result::GUILTY1, round(0.2)),
        3 => (sentence_result::GUILTY2, round(0.5)),
        4 => (sentence_result::GUILTY3, round(0.8)),
        5 => (sentence_result::GUILTY4, round(1.0)),
        6 => (sentence_result::GUILTY5, round(1.2)),
        _ => (sentence_result::NOT_GUILTY, 0),
    };
    let sentence_time = sentence_time.clamp(0, MAX_JAIL_MINUTES);
    t.send_packets(&SCRulingStatusPacket {
        count: selected_count,
        total,
        sentence_type,
        sentence_time: sentence_time.wrapping_mul(60_000),
    });
    t.jail_time = sentence_time.clamp(0, MAX_JAIL_MINUTES);
    let d = t.defendant.clone();
    // JUDGEMENT: the verdict for the defendant (value1 0 = not guilty, 1 = guilty).
    fx.push(Effect::Achievement(
        d.clone(),
        AchievementInput::Increment {
            kind_id: char_record_kind::JUDGEMENT,
            amount: 1,
            matcher: RecordMatch::Value1(u32::from(is_guilty)),
        },
    ));
    if is_guilty {
        result_is_guilty(server, rooms, &d, t, false, now_ms, fx);
    } else {
        result_is_not_guilty(server, &d, t, now_ms, fx);
    }
}
