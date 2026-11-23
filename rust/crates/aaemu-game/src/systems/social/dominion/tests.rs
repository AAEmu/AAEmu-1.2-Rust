use super::*;
use chrono::TimeZone;

fn t(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).unwrap()
}

fn long(v: f32) -> i64 {
    ((v * 4096.0) as i64) << 32
}

pub(crate) fn sample() -> DominionData {
    DominionData {
        zone_id: 0x0102,
        expedition_id: 3,
        house: 4,
        tax_rate: 5,
        x: 100.5,
        y: 200.25,
        z: 30.0,
        cur_house_tax_money: 6,
        cur_hunt_tax_money: 7,
        peace_tax_money: 8,
        cur_house_tax_aa_point: 9,
        peace_tax_aa_point: 10,
        last_paid_time: t(11),
        last_siege_end_time: t(12),
        reign_start_time: t(13),
        last_tax_rate_changed_time: t(14),
        last_national_tax_rate_changed_time: t(15),
        national_tax_rate: 16,
        national_monument_db_id: 17,
        national_monument_x: 18.0,
        national_monument_y: 19.0,
        national_monument_z: 20.0,
        obj_id: 0x0A0B0C,
        territory_data: DominionTerritoryData {
            id: 21,
            id2: 22,
            max_gates: 23,
            max_walls: 24,
            radius_declare: -25,
            radius_dominion: 26,
            radius_offense_hq: 27,
            radius_siege: 28,
        },
        siege_timers: DominionSiegeTimers {
            durations: [1, 2, 3, 4, 5],
            started: t(29),
            fixed: t(30),
            bdm: 31,
            siege_period: 32,
            unk_data: DominionUnkData {
                id: 33,
                obj_id: 34,
                x: 1.0,
                y: 2.0,
                z: 3.0,
                ni: 4,
                nr: 5,
                limit: 6,
                unk_ids: vec![7, 8],
            },
            unk2_data: DominionUnkData::default(),
        },
        non_pvp_start: t(35),
        non_pvp_duration: 36,
    }
}

/// The expected `Write` of [`sample`], spelled out with the C# primitives.
pub(crate) fn sample_bytes() -> Vec<u8> {
    let mut e = PacketStream::new();
    e.write_u16(0x0102)
        .write_u32(3)
        .write_u32(4)
        .write_i32(5)
        .write_i64(long(100.5))
        .write_i64(long(200.25))
        .write_f32(30.0);
    for v in 6..=10 {
        e.write_i32(v);
    }
    for v in 11..=15 {
        e.write_i64(v);
    }
    e.write_u16(16)
        .write_i64(17)
        .write_i64(long(18.0))
        .write_i64(long(19.0))
        .write_f32(20.0)
        .write_bc(0x0A0B0C)
        // territory
        .write_u32(21)
        .write_u32(22)
        .write_u8(23)
        .write_u8(24)
        .write_i16(-25)
        .write_u16(26)
        .write_i16(27)
        .write_i16(28);
    // siege timers
    for v in 1..=5 {
        e.write_i32(v);
    }
    e.write_i64(29).write_i64(30).write_i32(31).write_u8(32);
    e.write_u32(33)
        .write_bc(34)
        .write_i64(long(1.0))
        .write_i64(long(2.0))
        .write_f32(3.0)
        .write_u8(4)
        .write_u8(5)
        .write_u8(6)
        .write_u8(2)
        .write_u32(7)
        .write_u32(8);
    e.write_u32(0)
        .write_bc(0)
        .write_i64(0)
        .write_i64(0)
        .write_f32(0.0)
        .write_raw(&[0, 0, 0, 0]);
    e.write_i64(35).write_u16(36);
    e.into_bytes()
}

#[test]
fn dominion_data_write_is_byte_exact() {
    let mut s = PacketStream::new();
    sample().write(&mut s);
    assert_eq!(s.into_bytes(), sample_bytes());
}
