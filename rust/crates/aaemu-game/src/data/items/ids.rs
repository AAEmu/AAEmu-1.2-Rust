/// `ItemBindType`.
pub mod bind_type {
    pub const NORMAL: u32 = 1;
    pub const BIND_ON_PICKUP: u32 = 2;
    pub const BIND_ON_EQUIP: u32 = 3;
    pub const BIND_ON_UNPACK: u32 = 4;
    pub const BIND_ON_PICKUP_PACK: u32 = 5;
    pub const BIND_ON_AUCTION_WIN: u32 = 6;
}

/// `Item.CrestInk` etc. (the hard-coded templates of `ItemManager.Load`).
pub const ITEM_CREST_INK: u32 = 17663;
pub const ITEM_CREST_STAMP: u32 = 17662;
pub const ITEM_SHEET_MUSIC: u32 = 28051;
pub const ITEM_TREASURE_MAP_WITH_COORDINATES: u32 = 24581;
/// `Item.Coins`.
pub const ITEM_COINS: u32 = 500;
/// `Item.DawnStone`.
pub const ITEM_DAWN_STONE: u32 = 327;
/// `Item.TaxCertificate`.
pub const ITEM_TAX_CERTIFICATE: u32 = 31891;
/// `Item.BoundTaxCertificate`.
pub const ITEM_BOUND_TAX_CERTIFICATE: u32 = 31892;
/// `Item.AppraisalCertificate`.
pub const ITEM_APPRAISAL_CERTIFICATE: u32 = 28085;
/// `Item.SalonCertificate`.
pub const ITEM_SALON_CERTIFICATE: u32 = 30811;

/// `BackpackType` values `ItemContainer.CanAccept` allows in player containers.
pub mod backpack_type {
    /// The castle-claim packs skill 18540 (declare independence) requires
    /// (`skills.link_backpack_type_id = 1`; `item_backpacks.backpack_type_id` 1).
    pub const CASTLE_CLAIM: u32 = 1;
    pub const GLIDER: u32 = 2;
    pub const TOY_FLAG: u32 = 7;
}

/// `ItemCategory` (`items.category_id`).
pub mod item_category {
    pub const NONE: i32 = 0;
    pub const OTHER: i32 = 1;
    pub const UNSORTED: i32 = 2;
    pub const SHIP: i32 = 4;
    pub const BAGS: i32 = 5;
    pub const BUILDINGS: i32 = 6;
    pub const FURNITURE: i32 = 7;
    pub const SAPLINGS: i32 = 8;
    pub const COSTUME: i32 = 10;
    pub const POTION: i32 = 12;
    pub const FOOD: i32 = 13;
    pub const CONTRAPTION: i32 = 14;
    pub const SIEGE_GEAR: i32 = 15;
    pub const PAPER: i32 = 17;
    pub const BOOK: i32 = 18;
    pub const LUNASTONE: i32 = 20;
    pub const LIVESTOCK: i32 = 21;
    pub const METAL: i32 = 23;
    pub const LUMBER: i32 = 24;
    pub const STONE_BRICK: i32 = 25;
    pub const HIDE: i32 = 26;
    pub const FABRIC: i32 = 27;
    pub const MACHINING: i32 = 28;
    pub const GLASS: i32 = 30;
    pub const RUBBER: i32 = 31;
    pub const ARCHEUM: i32 = 32;
    pub const DYE: i32 = 33;
    pub const CRAFTING_MATERIAL: i32 = 34;
    pub const TRASH_MISCELLANEOUS: i32 = 35;
    pub const COOKING_OIL: i32 = 36;
    pub const SPICES: i32 = 37;
    pub const ORE: i32 = 38;
    pub const HARDWOOD: i32 = 39;
    pub const RAW_STONE: i32 = 40;
    pub const PELT: i32 = 41;
    pub const TEXTILE: i32 = 42;
    pub const COMPONENT: i32 = 43;
    pub const EVIDENCE: i32 = 44;
    pub const MEAT: i32 = 45;
    pub const SEAFOOD: i32 = 46;
    pub const GRAIN: i32 = 47;
    pub const VEGETABLE: i32 = 48;
    pub const FRUIT: i32 = 49;
    pub const SEED: i32 = 51;
    pub const SPICE: i32 = 52;
    pub const HERB: i32 = 53;
    pub const FLOWER: i32 = 55;
    pub const SOIL: i32 = 56;
    pub const PRECIOUS_METAL: i32 = 58;
    pub const GEM: i32 = 59;
    pub const ALCHEMY: i32 = 62;
    pub const QUEST_ITEM: i32 = 64;
    pub const DESIGN: i32 = 65;
    pub const DAGGER: i32 = 69;
    pub const SWORD: i32 = 70;
    pub const QUEST_EQUIP_WEAPON: i32 = 71;
    pub const KATANA: i32 = 72;
    pub const AXE: i32 = 73;
    pub const CLUB: i32 = 74;
    pub const SCEPTER: i32 = 75;
    pub const SHORTSPEAR: i32 = 76;
    pub const BOW: i32 = 77;
    pub const THROWING_WEAPON: i32 = 78;
    pub const SHIELD: i32 = 79;
    pub const LUTE: i32 = 80;
    pub const FLUTE: i32 = 81;
    pub const DRUM: i32 = 82;
    pub const CLOTH_ARMOR: i32 = 83;
    pub const LEATHER_ARMOR: i32 = 84;
    pub const PLATE_ARMOR: i32 = 85;
    pub const NECKLACE: i32 = 86;
    pub const RING: i32 = 87;
    pub const ACCESSORY: i32 = 88;
    pub const OUTDOOR_DECOR: i32 = 89;
    pub const INDOOR_DECOR: i32 = 90;
    pub const HORSE: i32 = 92;
    pub const VEHICLE: i32 = 93;
    pub const CRATE: i32 = 94;
    pub const SNOWLION: i32 = 95;
    pub const NPC_PACKS: i32 = 96;
    pub const DRINK: i32 = 97;
    pub const TRASH_CRAFT: i32 = 98;
    pub const SUNDRIES: i32 = 99;
    pub const TEST_ITEM: i32 = 100;
    pub const TRASH_WEAPON: i32 = 101;
    pub const TRASH_ARMOR: i32 = 102;
    pub const TRASH_MATERIAL: i32 = 103;
    pub const TRASH_DOODAD: i32 = 104;
    pub const TRASH_CONSUME: i32 = 105;
    pub const KEY: i32 = 106;
    pub const ADVENTURE: i32 = 107;
    pub const PET_GEAR: i32 = 108;
    pub const BLACK_TIGER: i32 = 109;
    pub const UEST_EQUIP_ARMOR: i32 = 110;
    pub const QUEST_EQUIP_ACCESSORY: i32 = 111;
    pub const YATA: i32 = 112;
    pub const MANA_POTION: i32 = 113;
    pub const DEFENSE_POTION: i32 = 114;
    pub const REGEN_POTION: i32 = 115;
    pub const HEALING_POTION: i32 = 116;
    pub const TOY: i32 = 117;
    pub const GLIDER: i32 = 118;
    pub const MATERIAL: i32 = 119;
    pub const TALISMAN: i32 = 120;
    pub const CLOAK: i32 = 121;
    pub const TRADE_PACK: i32 = 122;
    pub const BODY_PACK: i32 = 123;
    pub const LEOMORPH: i32 = 124;
    pub const EARRING: i32 = 125;
    pub const RELIC: i32 = 126;
    pub const GREATSWORD: i32 = 127;
    pub const NODACHI: i32 = 128;
    pub const GREATAXE: i32 = 129;
    pub const GREATCLUB: i32 = 130;
    pub const STAFF: i32 = 131;
    pub const LONGSPEAR: i32 = 132;
    pub const SPECIALTY: i32 = 133;
    pub const DROP_EQUIP_WEAPON: i32 = 134;
    pub const DROP_EQUIP_ARMOR: i32 = 135;
    pub const DROP_EQUIP_ACCESSORY: i32 = 136;
    pub const ELK: i32 = 137;
    pub const COIN: i32 = 138;
    pub const WOLF: i32 = 139;
    pub const SIEGE_STRONGHOLD: i32 = 140;
    pub const TOKEN: i32 = 141;
    pub const SABREFANG: i32 = 142;
    pub const EXPLOSIVE: i32 = 143;
    pub const PET: i32 = 144;
    pub const FISHING_ROD: i32 = 145;
    pub const TAXIDERMY: i32 = 146;
    pub const FISH_PRINT: i32 = 147;
    pub const SHEET_MUSIC: i32 = 148;
    pub const BEAR: i32 = 149;
    pub const TRASH_DYEING: i32 = 150;
    pub const BOAR: i32 = 151;
    pub const LUNAGEM: i32 = 152;
    pub const UNIDENTIFIED: i32 = 153;
}

/// `BackpackType` (every value).
pub mod backpack_kind {
    pub const CASTLE_CLAIM: u32 = 1;
    pub const GLIDER: u32 = 2;
    pub const TRADE_PACK: u32 = 3;
    pub const SIEGE_DECLARE: u32 = 4;
    pub const NATION_FLAG: u32 = 5;
    pub const FISH: u32 = 6;
    pub const TOY_FLAG: u32 = 7;
}

/// `ShopCurrencyType`.
pub mod shop_currency_type {
    pub const MONEY: u8 = 0;
    pub const HONOR: u8 = 1;
    pub const VOCATION_BADGES: u8 = 2;
    pub const SIEGE_SHOP: u8 = 3;
}

/// `ItemGrade`.
pub mod item_grade {
    pub const CRUDE: u8 = 0;
    pub const BASIC: u8 = 1;
    pub const GRAND: u8 = 2;
    pub const RARE: u8 = 3;
    pub const ARCANE: u8 = 4;
    pub const HEROIC: u8 = 5;
    pub const UNIQUE: u8 = 6;
    pub const CELESTIAL: u8 = 7;
    pub const DIVINE: u8 = 8;
    pub const EPIC: u8 = 9;
    pub const LEGENDARY: u8 = 10;
    pub const MYTHIC: u8 = 11;
}

/// `ArmorType`.
pub mod armor_type {
    pub const CLOTH: u32 = 1;
    pub const LEATHER: u32 = 2;
    pub const METAL: u32 = 3;
    pub const PET_ARMOR: u32 = 4;
    pub const ETC: u32 = 5;
}

/// `EquipmentItemSlotType`.
pub mod equipment_item_slot_type {
    pub const HEAD: u32 = 1;
    pub const NECK: u32 = 2;
    pub const CHEST: u32 = 3;
    pub const WAIST: u32 = 4;
    pub const LEGS: u32 = 5;
    pub const HANDS: u32 = 6;
    pub const FEET: u32 = 7;
    pub const ARMS: u32 = 8;
    pub const BACK: u32 = 9;
    pub const EAR: u32 = 10;
    pub const FINGER: u32 = 11;
    pub const UNDERSHIRT: u32 = 12;
    pub const UNDERPANTS: u32 = 13;
    pub const MAINHAND: u32 = 14;
    pub const OFFHAND: u32 = 15;
    pub const TWO_HANDED: u32 = 16;
    pub const ONE_HANDED: u32 = 17;
    pub const RANGED: u32 = 18;
    pub const AMMUNITION: u32 = 19;
    pub const SHIELD: u32 = 20;
    pub const INSTRUMENT: u32 = 21;
    pub const BAG: u32 = 22;
    pub const FACE: u32 = 23;
    pub const HAIR: u32 = 24;
    pub const GLASSES: u32 = 25;
    pub const RESERVED: u32 = 26;
    pub const TAIL: u32 = 27;
    pub const BODY: u32 = 28;
    pub const BEARD: u32 = 29;
    pub const BACKPACK: u32 = 30;
    pub const COSPLAY: u32 = 31;
}

/// `ItemImplEnum`.
pub mod item_impl {
    pub const MISC: i32 = 0;
    pub const WEAPON: i32 = 1;
    pub const ARMOR: i32 = 2;
    pub const BODY: i32 = 3;
    pub const BAG: i32 = 4;
    pub const HOUSING: i32 = 5;
    pub const HOUSING_DECORATION: i32 = 6;
    pub const TOOL: i32 = 7;
    pub const SUMMON_SLAVE: i32 = 8;
    pub const SPAWN_DOODAD: i32 = 9;
    pub const ACCEPT_QUEST: i32 = 10;
    pub const SUMMON_MATE: i32 = 11;
    pub const RECIPE: i32 = 12;
    pub const CRAFTING: i32 = 13;
    pub const PORTAL: i32 = 14;
    pub const ENCHANTING_GEM: i32 = 15;
    pub const REPORT_CRIME: i32 = 16;
    pub const LOGIC_DOODAD: i32 = 17;
    pub const HAS_UCC: i32 = 18;
    pub const OPEN_EMBLEM_UI: i32 = 19;
    pub const SHIPYARD: i32 = 20;
    pub const SOCKET: i32 = 21;
    pub const BACKPACK: i32 = 22;
    pub const OPEN_PAPER: i32 = 23;
    pub const ACCESSORY: i32 = 24;
    pub const TREASURE: i32 = 25;
    pub const MUSIC_SHEET: i32 = 26;
    pub const DYEING: i32 = 27;
    pub const SLAVE_EQUIPMENT: i32 = 28;
    pub const GRADE_ENCHANTING_SUPPORT: i32 = 29;
    pub const MATE_ARMOR: i32 = 30;
    pub const LOCATION: i32 = 31;
}

/// `ProcChanceKind`.
pub mod proc_chance_kind {
    pub const HIT_ANY: u32 = 1;
    pub const HIT_MELEE: u32 = 2;
    pub const HIT_MELEE_CRIT: u32 = 3;
    pub const HIT_SPELL: u32 = 4;
    pub const HIT_SPELL_CRIT: u32 = 5;
    pub const HIT_RANGED: u32 = 6;
    pub const HIT_RANGED_CRIT: u32 = 7;
    pub const HIT_SIEGE: u32 = 8;
    pub const TAKE_DAMAGE_ANY: u32 = 9;
    pub const TAKE_DAMAGE_MELEE: u32 = 10;
    pub const TAKE_DAMAGE_MELEE_CRIT: u32 = 11;
    pub const TAKE_DAMAGE_SPELL: u32 = 12;
    pub const TAKE_DAMAGE_SPELL_CRIT: u32 = 13;
    pub const TAKE_DAMAGE_RANGED: u32 = 14;
    pub const TAKE_DAMAGE_RANGED_CRIT: u32 = 15;
    pub const TAKE_DAMAGE_SIEGE: u32 = 16;
    pub const HIT_HEAL: u32 = 17;
    pub const HIT_HEAL_CRIT: u32 = 18;
    pub const FIRE_SKILL: u32 = 19;
    pub const HIT_SKILL: u32 = 20;
}
