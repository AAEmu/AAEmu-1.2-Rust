use super::{
    has_quest_completed, items_by_template, Character, GameServer, QuestEnv, QuestState, ReqEnv,
    ReqSubject, UnitSnapshot,
};

// ----- UnitReqs views -----

/// The owner (`BaseUnit` = the locked character).
pub struct OwnerView<'a> {
    pub c: &'a Character,
    pub st: &'a QuestState,
    pub server: &'a GameServer,
}

impl ReqSubject for OwnerView<'_> {
    fn is_player(&self) -> bool {
        true
    }
    fn level(&self) -> u8 {
        self.c.unit.level
    }
    fn ability_level(&self, ability: u8) -> Option<u8> {
        let exp = self
            .c
            .abilities
            .abilities
            .iter()
            .find(|a| a.id == ability)
            .map(|a| a.exp);
        Some(exp.map_or(0, |e| {
            self.server
                .game_data
                .experience
                .level_from_exp_any(e, false)
                .map_or(0, |(l, _)| l)
        }))
    }
    fn race(&self) -> Option<u8> {
        Some(self.c.race)
    }
    fn gender(&self) -> Option<u8> {
        Some(self.c.gender)
    }
    fn equip_slot_used(&self, slot: i32) -> bool {
        self.c
            .inventory
            .equipment()
            .lock()
            .expect("equipment poisoned")
            .get_item_by_slot(slot)
            .is_some()
    }
    fn has_equipped(&self, template_id: u32) -> bool {
        !self
            .c
            .inventory
            .equipment()
            .lock()
            .expect("equipment poisoned")
            .items_by_template(template_id, -1)
            .0
            .is_empty()
    }
    fn owns_item(&self, template_id: u32) -> bool {
        items_by_template(self.c, &[], template_id).0
    }
    fn has_skill(&self, skill_id: u32) -> bool {
        self.c.skills.skills.iter().any(|s| s.id == skill_id)
    }
    fn is_in_battle(&self) -> bool {
        self.c.unit.is_in_battle
    }
    fn has_buff(&self, buff_id: u32) -> bool {
        self.c.unit.buffs.check_buff(buff_id)
    }
    fn has_buff_tag(&self, tag: u32) -> bool {
        // Buffs.CheckBuffTag(tag)
        has_buff_tag_in(
            self.server,
            self.c.unit.buffs.effects.iter().map(|b| b.template.id),
            tag,
        )
    }
    fn hpp(&self) -> i32 {
        self.c.unit.hpp()
    }
    fn base_unit_type(&self) -> u8 {
        0
    }
    fn npc_template_id(&self) -> Option<u32> {
        None
    }
    fn is_under_water(&self) -> bool {
        self.c.unit.is_under_water
    }
    fn faction_id(&self) -> u32 {
        self.c.unit.faction_id
    }
    fn mother_faction_id(&self) -> u32 {
        self.server
            .game_data
            .factions
            .get(self.c.unit.faction_id)
            .map_or(0, |f| f.mother_id)
    }
    fn quest_completed(&self, quest_id: u32) -> Option<bool> {
        Some(has_quest_completed(&self.c.quests.completed, quest_id))
    }
    fn quest_step(&self, quest_id: u32) -> Option<Option<u8>> {
        Some(self.st.active_quest(quest_id).map(|q| q.step))
    }
    fn actability_point(&self, id: u32) -> Option<i32> {
        self.c
            .actability
            .actabilities
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.point)
    }
    fn honor_point(&self) -> Option<i32> {
        Some(self.c.honor_point)
    }
    fn jury_point(&self) -> Option<i32> {
        Some(self.c.jury_point)
    }
    fn vocation_point(&self) -> Option<i32> {
        Some(self.c.vocation_point)
    }
    fn has_appellation(&self, id: u32) -> Option<bool> {
        Some(self.c.appellations.appellations.contains(&id))
    }
    fn zone_key(&self) -> u32 {
        self.c.unit.transform.zone_id
    }
    fn position(&self) -> (f32, f32, f32) {
        let t = &self.c.unit.transform;
        (t.x, t.y, t.z)
    }
    fn labor_margin(&self) -> Option<i32> {
        // TimedRewardsManager.GetMaxLabor(Connection.Payment.PremiumState) - LaborPower
        Some(
            crate::systems::skills::unit_reqs::max_labor(self.server, self.c.account_id)
                - i32::from(self.c.labor_power),
        )
    }
    fn expedition(&self) -> Option<(u32, u32)> {
        (self.c.unit.expedition_id > 0).then_some((self.c.unit.expedition_id, 0))
    }
    fn character_id(&self) -> Option<u32> {
        Some(self.c.id)
    }
}

/// `Buffs.CheckBuffTag(tag)` over the template ids of a unit's buffs.
fn has_buff_tag_in(server: &GameServer, mut buff_ids: impl Iterator<Item = u32>, tag: u32) -> bool {
    match server.game_data.systems.skills.buffs_by_tag(tag) {
        Some(ids) => buff_ids.any(|id| ids.contains(&id)),
        None => false,
    }
}

/// The current target, from its snapshot.
pub struct TargetView<'a>(pub &'a UnitSnapshot, pub &'a GameServer);

impl ReqSubject for TargetView<'_> {
    fn is_unit(&self) -> bool {
        self.0.is_unit
    }
    fn is_player(&self) -> bool {
        false
    }
    fn level(&self) -> u8 {
        self.0.level
    }
    fn ability_level(&self, _ability: u8) -> Option<u8> {
        None
    }
    fn race(&self) -> Option<u8> {
        None
    }
    fn gender(&self) -> Option<u8> {
        None
    }
    fn equip_slot_used(&self, _slot: i32) -> bool {
        false
    }
    fn has_equipped(&self, _template_id: u32) -> bool {
        false
    }
    fn owns_item(&self, _template_id: u32) -> bool {
        false
    }
    fn has_skill(&self, _skill_id: u32) -> bool {
        false
    }
    fn is_in_battle(&self) -> bool {
        self.0.in_battle
    }
    fn has_buff(&self, buff_id: u32) -> bool {
        self.0.buff_ids.contains(&buff_id)
    }
    fn has_buff_tag(&self, tag: u32) -> bool {
        has_buff_tag_in(self.1, self.0.buff_ids.iter().copied(), tag)
    }
    fn hpp(&self) -> i32 {
        self.0.hpp
    }
    fn base_unit_type(&self) -> u8 {
        self.0.base_unit_type
    }
    fn npc_template_id(&self) -> Option<u32> {
        self.0.npc_template_id
    }
    fn doodad_template_id(&self) -> Option<u32> {
        self.0.doodad_template_id
    }
    fn is_under_water(&self) -> bool {
        self.0.under_water
    }
    fn faction_id(&self) -> u32 {
        self.0.faction_id
    }
    fn mother_faction_id(&self) -> u32 {
        self.0.mother_faction_id
    }
    fn quest_completed(&self, _quest_id: u32) -> Option<bool> {
        None
    }
    fn quest_step(&self, _quest_id: u32) -> Option<Option<u8>> {
        None
    }
    fn actability_point(&self, _id: u32) -> Option<i32> {
        None
    }
    fn honor_point(&self) -> Option<i32> {
        None
    }
    fn jury_point(&self) -> Option<i32> {
        None
    }
    fn vocation_point(&self) -> Option<i32> {
        None
    }
    fn has_appellation(&self, _id: u32) -> Option<bool> {
        None
    }
    fn zone_key(&self) -> u32 {
        self.0.zone_key
    }
    fn position(&self) -> (f32, f32, f32) {
        self.0.position
    }
    fn labor_margin(&self) -> Option<i32> {
        None
    }
    fn expedition(&self) -> Option<(u32, u32)> {
        None
    }
    fn character_id(&self) -> Option<u32> {
        None
    }
}

/// World / data facts for `UnitReqs.Validate`.
pub struct EnvView<'a> {
    pub server: &'a GameServer,
    pub env: &'a QuestEnv,
}

impl ReqEnv for EnvView<'_> {
    fn zone_id_of_key(&self, zone_key: u32) -> Option<u32> {
        self.server
            .game_data
            .zones
            .zone_by_key(zone_key)
            .map(|z| z.id)
    }
    fn time_of_day(&self) -> f32 {
        self.env.time_of_day
    }
    fn inside_area_sphere(&self, sphere_id: u32, value2: u32, pos: (f32, f32, f32)) -> bool {
        self.server
            .systems
            .quests
            .spheres
            .inside_requirement_sphere(
                &self.server.game_data.systems.quests,
                sphere_id,
                value2,
                pos,
            )
    }
}
