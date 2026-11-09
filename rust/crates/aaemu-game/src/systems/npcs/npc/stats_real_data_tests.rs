//! Real `compact.sqlite3` NPC max HP/MP with the `owner_type='Npc'` `unit_modifiers`.
//!
//! Evidence: video-evidence/dungeons-2014-15-ayin.md (AyinMaiden, NA Sep 2014 - Mar 2015,
//! grade C) section 3. Formula 31 + the per-NPC MaxHealth modifier reproduces the values on
//! screen to the digit; formula 32 (mana) matched wherever a full bar was visible.

use rusqlite::{Connection, OpenFlags};

use crate::data::attribute_limits::UnitAttributeLimits;
use crate::data::formulas::FormulaManager;
use crate::data::GameData;
use crate::test_support::test_server_with;

use crate::systems::npcs::npc::data::NpcData;
use crate::systems::npcs::npc::manager::create;

/// `(npc id, max HP, max MP, name)`. HP and MP are the values in `compact.sqlite3` after
/// `NpcManager.Create` applies the template's `unit_modifiers` (attribute 6 MaxHealth /
/// attribute 7 MaxMana, value kind).
const BOSSES: &[(u32, i32, i32, &str)] = &[
    // Palace Cellar
    (9797, 21_747, 7_450, "Salphira's Avatar Akmit"),
    (9796, 19_930, 1_490, "Combat Drillmaster Tushan"),
    (9809, 19_655, 1_490, "Dreamer of Purity Melkina"),
    (9845, 5_953, 1_430, "Salphira Pyromancer"),
    // Hadir Farm
    (9798, 133_361, 22_700, "Marmas the Reaper"),
    (9793, 133_361, 22_700, "Rotten Flesh Golem"),
    (9792, 113_372, 22_700, "Lady Velarre Blueblood"),
    (9478, 21_542, 11_050, "Faceless Shaman"),
    (9468, 30_751, 11_350, "Ravenous Specimen"),
    (10063, 24_004, 11_650, "Farm Supervisor Sagrath"),
    // Burnt Castle Armory
    (8960, 133_361, 22_700, "Quartermaster Sorvell"),
    (8838, 29_109, 11_050, "Bloodhand Fighter"),
    (8846, 22_756, 11_350, "Bloodwind Warlock"),
    // Greater Howling Abyss
    (12761, 280_000, 32_300, "Travis Godfrey"),
    (12759, 270_000, 32_300, "Calleil the Cursed"),
    (12762, 280_000, 32_300, "Viboros the Golden"),
    (12786, 316_787, 32_300, "Musperosa's Spirit"),
    (12784, 41_702, 15_250, "Medusa"),
    (12765, 49_043, 15_250, "Blackscale Corps Knight"),
    // Serpentis (Nachashgar)
    (14670, 656_787, 32_300, "Serpentis Keeper"),
    (12605, 1_175_480, 32_300, "Distorted Duaxini"),
    (12962, 3_135_230, 32_300, "Sirothe the Infernal"),
    (12971, 1_635_230, 32_300, "Lerman"),
    (12974, 1_635_230, 32_300, "Aunorin Fangbow"),
    (12976, 1_655_500, 32_300, "Carmila"),
    (13162, 113_439, 16_150, "Carmila's Guard"),
    (13282, 46_916, 16_150, "Balius"),
    (12700, 62_939, 16_150, "Bloodhand Captain"),
    (13097, 15_706, 3_230, "Dominated Creation of Screams"),
    // Lesser Sea of Drowned Love (weekend)
    (13712, 1_140_000, 32_300, "Dahuta"),
    (13713, 36_819, 3_230, "Bloated Corpse"),
    (13716, 5_609, 969, "Tidal Nymph"),
];

fn real_data() -> GameData {
    let path = std::env::var("AAEMU_COMPACT_DB").expect("AAEMU_COMPACT_DB");
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut data = GameData::empty();
    data.formulas = FormulaManager::load(&conn).unwrap();
    data.characters.unit_attribute_limits = UnitAttributeLimits::load(&conn).unwrap().into();
    data.systems.npcs.npc = NpcData::load_conn(&conn, "en_us").unwrap();
    data
}

#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_npc_max_hp_and_mp_include_unit_modifiers() {
    let server = test_server_with(real_data());
    for &(id, hp, mp, name) in BOSSES {
        let npc = create(&server, 0x1000 + id, id).unwrap_or_else(|| panic!("npc {id} {name}"));
        let n = npc.data();
        assert_eq!((n.unit.max_hp, n.unit.max_mp), (hp, mp), "{id} {name}");
    }
}

/// The values above are not formula 31/32 alone: Lerman's template carries the +1,359,750
/// MaxHealth modifier that turns 275,480 into the 1,635,230 on screen, and Carmila's Guard its
/// +50,000. A loader or `NpcManager.Create` change that drops the template bonuses fails the
/// test above; this one names the data rows.
#[tokio::test]
#[ignore = "needs AAEMU_COMPACT_DB"]
async fn real_compact_unit_modifier_rows_behind_the_pinned_values() {
    let data = real_data();
    let lerman = data.systems.npcs.npc.template(12971).unwrap();
    let max_hp = lerman
        .bonuses
        .iter()
        .find(|b| b.attribute == crate::models::unit::UnitAttribute::MaxHealth)
        .expect("Lerman MaxHealth modifier");
    assert_eq!((max_hp.value, max_hp.linear_level_bonus), (1_359_750, 0));
    let guard = data.systems.npcs.npc.template(13162).unwrap();
    assert_eq!(
        guard
            .bonuses
            .iter()
            .filter(|b| b.attribute == crate::models::unit::UnitAttribute::MaxHealth)
            .map(|b| b.value)
            .sum::<i32>(),
        50_000
    );
    let marmas = data.systems.npcs.npc.template(9798).unwrap();
    assert!(marmas
        .bonuses
        .iter()
        .all(|b| b.attribute != crate::models::unit::UnitAttribute::MaxHealth));
}
