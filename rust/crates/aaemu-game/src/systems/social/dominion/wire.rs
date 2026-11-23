use super::{convert_coord_to_long, DateTime, Marshal, PacketStream, Utc};

/// `DominionData`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DominionData {
    pub zone_id: u16,
    pub expedition_id: u32,
    /// `House` (TODO in C#: an id?).
    pub house: u32,
    pub tax_rate: i32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub cur_house_tax_money: i32,
    pub cur_hunt_tax_money: i32,
    pub peace_tax_money: i32,
    pub cur_house_tax_aa_point: i32,
    pub peace_tax_aa_point: i32,
    pub last_paid_time: DateTime<Utc>,
    pub last_siege_end_time: DateTime<Utc>,
    pub reign_start_time: DateTime<Utc>,
    pub last_tax_rate_changed_time: DateTime<Utc>,
    /// `LastNationalTaxRateChagedTime` (sic).
    pub last_national_tax_rate_changed_time: DateTime<Utc>,
    pub national_tax_rate: u16,
    pub national_monument_db_id: i64,
    pub national_monument_x: f32,
    pub national_monument_y: f32,
    pub national_monument_z: f32,
    pub obj_id: u32,
    pub territory_data: DominionTerritoryData,
    pub siege_timers: DominionSiegeTimers,
    pub non_pvp_start: DateTime<Utc>,
    pub non_pvp_duration: u16,
}

impl Marshal for DominionData {
    fn write(&self, s: &mut PacketStream) {
        s.write_u16(self.zone_id)
            .write_u32(self.expedition_id)
            .write_u32(self.house)
            .write_i32(self.tax_rate)
            .write_i64(convert_coord_to_long(self.x))
            .write_i64(convert_coord_to_long(self.y))
            .write_f32(self.z)
            .write_i32(self.cur_house_tax_money)
            .write_i32(self.cur_hunt_tax_money)
            .write_i32(self.peace_tax_money)
            .write_i32(self.cur_house_tax_aa_point)
            .write_i32(self.peace_tax_aa_point)
            .write_date_time(self.last_paid_time)
            .write_date_time(self.last_siege_end_time)
            .write_date_time(self.reign_start_time)
            .write_date_time(self.last_tax_rate_changed_time)
            .write_date_time(self.last_national_tax_rate_changed_time)
            .write_u16(self.national_tax_rate)
            .write_i64(self.national_monument_db_id)
            .write_i64(convert_coord_to_long(self.national_monument_x))
            .write_i64(convert_coord_to_long(self.national_monument_y))
            .write_f32(self.national_monument_z)
            .write_bc(self.obj_id)
            .write(&self.territory_data)
            .write(&self.siege_timers)
            .write_date_time(self.non_pvp_start)
            .write_u16(self.non_pvp_duration);
    }
}

/// `DominionTerritoryData`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DominionTerritoryData {
    pub id: u32,
    pub id2: u32,
    pub max_gates: u8,
    pub max_walls: u8,
    pub radius_declare: i16,
    pub radius_dominion: u16,
    pub radius_offense_hq: i16,
    pub radius_siege: i16,
}

impl Marshal for DominionTerritoryData {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id)
            .write_u32(self.id2)
            .write_u8(self.max_gates)
            .write_u8(self.max_walls)
            .write_i16(self.radius_declare)
            .write_u16(self.radius_dominion)
            .write_i16(self.radius_offense_hq)
            .write_i16(self.radius_siege);
    }
}

/// `DominionSiegeTimers` (TODO in C#: the names may be wrong).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DominionSiegeTimers {
    pub durations: [i32; 5],
    pub started: DateTime<Utc>,
    pub fixed: DateTime<Utc>,
    pub bdm: i32,
    pub siege_period: u8,
    pub unk_data: DominionUnkData,
    pub unk2_data: DominionUnkData,
}

impl Marshal for DominionSiegeTimers {
    fn write(&self, s: &mut PacketStream) {
        for d in self.durations {
            s.write_i32(d);
        }
        s.write_date_time(self.started)
            .write_date_time(self.fixed)
            .write_i32(self.bdm)
            .write_u8(self.siege_period)
            .write(&self.unk_data)
            .write(&self.unk2_data);
    }
}

/// `DominionUnkData`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DominionUnkData {
    /// `Id` (TODO in C#: the expedition id).
    pub id: u32,
    pub obj_id: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub ni: u8,
    pub nr: u8,
    pub limit: u8,
    /// `UnkIds` (the count is written as a byte).
    pub unk_ids: Vec<u32>,
}

impl Marshal for DominionUnkData {
    fn write(&self, s: &mut PacketStream) {
        s.write_u32(self.id)
            .write_bc(self.obj_id)
            .write_i64(convert_coord_to_long(self.x))
            .write_i64(convert_coord_to_long(self.y))
            .write_f32(self.z)
            .write_u8(self.ni)
            .write_u8(self.nr)
            .write_u8(self.limit)
            .write_u8(self.unk_ids.len() as u8);
        for id in &self.unk_ids {
            s.write_u32(*id);
        }
    }
}
