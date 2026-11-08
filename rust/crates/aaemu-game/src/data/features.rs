//! Port of `AAEmu.Game.Models.Game.Features.FeatureSet` / `Feature` and
//! `FeaturesManager.Initialize`.

use std::collections::BTreeMap;
use std::fmt;

use aaemu_commons::{Marshal, PacketStream};
use serde::Deserialize;

use crate::config::{lenient, lenient_map, lenient_option};

/// `Feature` bit indexes (byte = value / 8, bit = value % 8). Only named members are listed;
/// `flag_x_y` placeholders are omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Feature {
    Siege = 0,
    AllowFamilyChanges = 1,
    HouseSale = 3,
    Premium = 4,
    Nations = 6,
    Ranking = 36,
    Ingamecashshop = 38,
    CustomUiButton = 39,
    Customsaveload = 40,
    BmMileage = 43,
    AaPoint = 44,
    ItemSecure = 45,
    Secondpass = 46,
    SlaveCustomize = 48,
    BeautyshopBypass = 52,
    FreeLpRaise = 53,
    IngameshopSecondpass = 55,
    BackpackProfitShare = 56,
    SensitiveOperation = 58,
    TaxItem = 59,
    Achievement = 63,
    DwarfWarborn = 74,
    MailCoolTime = 75,
    HudAuctionButton = 81,
    AuctionPostBuff = 83,
    HouseTaxPrepay = 84,
}

const FSET_BYTES: usize = 11;

/// Default fset bytes (`FeatureSet._fset`).
pub const DEFAULT_FSET: [u8; FSET_BYTES] = [
    0x11, 0x37, 0x0F, 0x0F, 0x79, 0x69, 0xb3, 0x8d, 0x32, 0x0c, 0x1a,
];
const PLAYER_LEVEL_LIMIT_INDEX: usize = 1;
const MATE_LEVEL_LIMIT_INDEX: usize = 8;

/// `FeatureSet` configuration (`Configurations/FeatureSet.json`).
///
/// The named switches are the keys `FUN_39496b20` (`x2game.dll.c:956456`) puts into the Lua
/// table behind `X2Player:GetFeatureSet()`; each doc comment gives the bit that function tests
/// and its line. `AaPoint` (44) is not in that table; the client reads it in C++ (see its
/// comment). `ExtraBits` overrides any bit by number, for the unnamed `flag_x_y` members.
///
/// Every named default is `true`/`false` except `siege` and `nations`: those are `None` and
/// follow the era profile (`World.Profile`). With the `Kr12` profile the defaults reproduce
/// the bytes [`FeatureSet::initialized`] produces with the stock level caps.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FeatureSetConfig {
    /// `siege` (bit 0, `x2game.dll.c:956485`). `None` = the era profile decides
    /// (`World.Profile`: on for `EuAuroria` and `Kr12`, off for `EuLaunch`); an explicit
    /// value wins.
    #[serde(rename = "siege", default, deserialize_with = "lenient_option")]
    pub siege: Option<bool>,
    /// `nations` (bit 6, C# `Feature.Nations`). `None` = the era profile decides (KR 1.2 only);
    /// an explicit value wins. Not exposed through `GetFeatureSet()`, so the client reads the
    /// bit itself.
    #[serde(rename = "nations", default, deserialize_with = "lenient_option")]
    pub nations: Option<bool>,
    /// `premium` (bit 4, `x2game.dll.c:956509`).
    #[serde(rename = "premium", deserialize_with = "lenient")]
    pub premium: bool,
    /// `itemSecure` (bit 45, `x2game.dll.c:956493`).
    #[serde(rename = "itemsecure", deserialize_with = "lenient")]
    pub item_secure: bool,
    /// `secondpass` (bit 46, `x2game.dll.c:956501`).
    #[serde(rename = "secondpass", deserialize_with = "lenient")]
    pub secondpass: bool,
    /// `ranking` (bit 36, `x2game.dll.c:956517`).
    #[serde(rename = "ranking", deserialize_with = "lenient")]
    pub ranking: bool,
    /// `slave_customize` (bit 48, `x2game.dll.c:956525`).
    #[serde(rename = "slave_customize", deserialize_with = "lenient")]
    pub slave_customize: bool,
    /// `bm_mileage` (bit 43, `x2game.dll.c:956533`).
    #[serde(rename = "bm_mileage", deserialize_with = "lenient")]
    pub bm_mileage: bool,
    /// `ingamecashshop` (bit 38, `x2game.dll.c:956541`).
    #[serde(rename = "ingamecashshop", deserialize_with = "lenient")]
    pub ingamecashshop: bool,
    /// `customsaveload` (bit 40, `x2game.dll.c:956549`).
    #[serde(rename = "customsaveload", deserialize_with = "lenient")]
    pub customsaveload: bool,
    /// `beautyshopBypass` (bit 52, `x2game.dll.c:956557`).
    #[serde(rename = "beautyshopbypass", deserialize_with = "lenient")]
    pub beautyshop_bypass: bool,
    /// `ingameshopSecondpass` (bit 55, `x2game.dll.c:956565`).
    #[serde(rename = "ingameshopsecondpass", deserialize_with = "lenient")]
    pub ingameshop_secondpass: bool,
    /// `backpackProfitShare` (bit 56, `x2game.dll.c:956573`).
    #[serde(rename = "backpackprofitshare", deserialize_with = "lenient")]
    pub backpack_profit_share: bool,
    /// `sensitiveOpeartion` (bit 58, `x2game.dll.c:956581`; the key's spelling is the client's).
    #[serde(rename = "sensitiveopeartion", deserialize_with = "lenient")]
    pub sensitive_operation: bool,
    /// `taxItem` (bit 59, `x2game.dll.c:956589`).
    #[serde(rename = "taxitem", deserialize_with = "lenient")]
    pub tax_item: bool,
    /// `achievement` (bit 63, `x2game.dll.c:956597`).
    #[serde(rename = "achievement", deserialize_with = "lenient")]
    pub achievement: bool,
    /// `dwarfWarborn` (bit 74, `x2game.dll.c:956605`). The Dwarf/Warborn races are
    /// `creatable = 'f'` in the shipped `characters` data, so enabling the bit does not make
    /// them creatable.
    #[serde(rename = "dwarfwarborn", deserialize_with = "lenient")]
    pub dwarf_warborn: bool,
    /// `mailCoolTime` (bit 75, `x2game.dll.c:956613`).
    #[serde(rename = "mailcooltime", deserialize_with = "lenient")]
    pub mail_cool_time: bool,
    /// `hudAuctionButton` (bit 81, `x2game.dll.c:956639`).
    #[serde(rename = "hudauctionbutton", deserialize_with = "lenient")]
    pub hud_auction_button: bool,
    /// `auctionPostBuff` (bit 83, `x2game.dll.c:956647`).
    #[serde(rename = "auctionpostbuff", deserialize_with = "lenient")]
    pub auction_post_buff: bool,
    /// AA-point bit 44. The client reads it for AA-point amounts in price/currency displays
    /// (`x2game.dll.c:66363` `grade_enchant_cost` / `grade_enchant_currency`, `:633707`,
    /// `:659611`, `:733342`); it is not one of `GetFeatureSet()`'s Lua keys. AA points are not
    /// supported, so the key must stay `false`.
    #[serde(rename = "aapoint", deserialize_with = "lenient")]
    pub aa_point: bool,
    /// Explicit overrides for bits without a named switch (the unnamed `flag_x_y` members).
    /// Keys are bit numbers `0..=87` (byte = bit / 8); a `false` value turns a default-on bit
    /// off.
    #[serde(rename = "extrabits", deserialize_with = "lenient_map")]
    pub extra_bits: BTreeMap<u8, bool>,
}

impl Default for FeatureSetConfig {
    fn default() -> Self {
        Self {
            siege: None,
            nations: None,
            premium: true,
            item_secure: true,
            secondpass: false,
            ranking: true,
            slave_customize: true,
            bm_mileage: true,
            ingamecashshop: true,
            customsaveload: true,
            beautyshop_bypass: true,
            ingameshop_secondpass: false,
            backpack_profit_share: true,
            sensitive_operation: false,
            tax_item: true,
            achievement: true,
            dwarf_warborn: false,
            mail_cool_time: true,
            hud_auction_button: true,
            auction_post_buff: true,
            aa_point: false,
            extra_bits: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureSet {
    fset: [u8; FSET_BYTES],
}

impl Default for FeatureSet {
    fn default() -> Self {
        Self { fset: DEFAULT_FSET }
    }
}

impl FeatureSet {
    /// `FeaturesManager.Initialize()`.
    pub fn initialized(max_player_level: u8, max_mate_level: u8) -> Self {
        let mut f = Self::default();
        f.set_player_level_limit(max_player_level);
        f.set_mate_level_limit(max_mate_level);
        f.set(Feature::HouseSale, true);
        f.set(Feature::Nations, true);
        f.set(Feature::AllowFamilyChanges, true);
        f.set(Feature::DwarfWarborn, false);
        f.set(Feature::SensitiveOperation, false);
        f.set(Feature::Secondpass, false);
        f.set(Feature::IngameshopSecondpass, false);
        f.set(Feature::ItemSecure, true);
        f.set(Feature::CustomUiButton, true);
        f
    }

    /// Applies [`FeatureSetConfig`] on top of [`FeatureSet::initialized`].
    ///
    /// Every named switch sets exactly its bit; an absent `siege` / `nations` key follows the
    /// resolved server flags (`AppConfig::auroria_claims_enabled` /
    /// `AppConfig::player_nations_enabled`, which default from the era profile). `extra_bits`
    /// is applied last, so it can also override a named bit or turn a default-on unnamed bit
    /// off.
    pub fn apply_config(
        &mut self,
        config: &FeatureSetConfig,
        siege_default: bool,
        nations_default: bool,
    ) {
        self.set(Feature::Siege, config.siege.unwrap_or(siege_default));
        self.set(Feature::Nations, config.nations.unwrap_or(nations_default));
        self.set(Feature::Premium, config.premium);
        self.set(Feature::ItemSecure, config.item_secure);
        self.set(Feature::Secondpass, config.secondpass);
        self.set(Feature::Ranking, config.ranking);
        self.set(Feature::SlaveCustomize, config.slave_customize);
        self.set(Feature::BmMileage, config.bm_mileage);
        self.set(Feature::Ingamecashshop, config.ingamecashshop);
        self.set(Feature::Customsaveload, config.customsaveload);
        self.set(Feature::BeautyshopBypass, config.beautyshop_bypass);
        self.set(Feature::IngameshopSecondpass, config.ingameshop_secondpass);
        self.set(Feature::BackpackProfitShare, config.backpack_profit_share);
        self.set(Feature::SensitiveOperation, config.sensitive_operation);
        self.set(Feature::TaxItem, config.tax_item);
        self.set(Feature::Achievement, config.achievement);
        self.set(Feature::DwarfWarborn, config.dwarf_warborn);
        self.set(Feature::MailCoolTime, config.mail_cool_time);
        self.set(Feature::HudAuctionButton, config.hud_auction_button);
        self.set(Feature::AuctionPostBuff, config.auction_post_buff);
        self.set(Feature::AaPoint, config.aa_point);
        let max_bit = (self.fset.len() * 8 - 1) as u8;
        for (&bit, &enabled) in &config.extra_bits {
            if !self.set_bit(bit, enabled) {
                tracing::warn!("FeatureSet: ExtraBits bit {bit} is out of range (0-{max_bit})");
            }
        }
    }

    fn index(feature: Feature) -> (usize, u8) {
        let v = feature as u8;
        (usize::from(v / 8), v % 8)
    }

    pub fn check(&self, feature: Feature) -> bool {
        let (byte, bit) = Self::index(feature);
        self.fset[byte] & (1 << bit) != 0
    }

    pub fn set(&mut self, feature: Feature, enabled: bool) -> bool {
        let (byte, bit) = Self::index(feature);
        let Some(b) = self.fset.get_mut(byte) else {
            return false;
        };
        if enabled {
            *b |= 1 << bit;
        } else {
            *b &= !(1 << bit);
        }
        true
    }

    /// `Check((Feature)bit)` for any bit index (also the unnamed `flag_x_y` members).
    pub fn check_bit(&self, bit: u8) -> bool {
        self.fset
            .get(usize::from(bit / 8))
            .is_some_and(|b| b & (1 << (bit % 8)) != 0)
    }

    /// `Set((Feature)bit, enabled)` for any bit index; `false` when out of range.
    pub fn set_bit(&mut self, bit: u8, enabled: bool) -> bool {
        let Some(b) = self.fset.get_mut(usize::from(bit / 8)) else {
            return false;
        };
        if enabled {
            *b |= 1 << (bit % 8);
        } else {
            *b &= !(1 << (bit % 8));
        }
        true
    }

    pub fn player_level_limit(&self) -> u8 {
        self.fset[PLAYER_LEVEL_LIMIT_INDEX]
    }
    pub fn set_player_level_limit(&mut self, v: u8) {
        self.fset[PLAYER_LEVEL_LIMIT_INDEX] = v;
    }
    pub fn mate_level_limit(&self) -> u8 {
        self.fset[MATE_LEVEL_LIMIT_INDEX]
    }
    pub fn set_mate_level_limit(&mut self, v: u8) {
        self.fset[MATE_LEVEL_LIMIT_INDEX] = v;
    }

    pub fn bytes(&self) -> &[u8; FSET_BYTES] {
        &self.fset
    }
}

impl Marshal for FeatureSet {
    /// `FeatureSet.Write`: `u16`-length-prefixed byte array.
    fn write(&self, s: &mut PacketStream) {
        s.write_bytes_sized(&self.fset);
    }
}

impl fmt::Display for FeatureSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.fset {
            write!(f, "{b:02x} ")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(config: &FeatureSetConfig, profile: crate::config::ServerProfile) -> FeatureSet {
        let mut f = FeatureSet::initialized(55, 50);
        f.apply_config(config, profile.sieges(), profile.player_nations());
        f
    }

    #[test]
    fn initialized_bytes() {
        let f = FeatureSet::initialized(55, 50);
        // byte0 0x11 | houseSale(bit3) | nations(bit6) | allowFamilyChanges(bit1) = 0x5B
        // byte4 0x79 | customUiButton(bit7) = 0xF9
        // byte5 0x69 & ~secondpass(bit6) = 0x29 (itemSecure(bit5) is on)
        // byte6 0xb3 & ~ingameshopSecondpass(bit7) = 0x33
        // byte7 0x8d & ~sensitiveOperation(bit2) = 0x89
        // byte9 0x0c & ~dwarfWarborn(bit2) = 0x08
        assert_eq!(
            f.bytes(),
            &[0x5B, 55, 0x0F, 0x0F, 0xF9, 0x29, 0x33, 0x89, 50, 0x08, 0x1a]
        );
    }

    #[test]
    fn default_config_reproduces_the_current_bytes() {
        let f = configured(
            &FeatureSetConfig::default(),
            crate::config::ServerProfile::Kr12,
        );
        assert_eq!(
            f.bytes(),
            &[0x5B, 55, 0x0F, 0x0F, 0xF9, 0x29, 0x33, 0x89, 50, 0x08, 0x1a]
        );
        assert_eq!(f, FeatureSet::initialized(55, 50));
    }

    #[test]
    fn profile_drives_siege_and_nations_unless_explicit() {
        use crate::config::ServerProfile;
        let eu = configured(&FeatureSetConfig::default(), ServerProfile::EuAuroria);
        assert!(eu.check(Feature::Siege));
        assert!(!eu.check(Feature::Nations));
        let launch = configured(&FeatureSetConfig::default(), ServerProfile::EuLaunch);
        assert!(!launch.check(Feature::Siege));
        assert!(!launch.check(Feature::Nations));
        let kr = configured(&FeatureSetConfig::default(), ServerProfile::Kr12);
        assert!(kr.check(Feature::Siege) && kr.check(Feature::Nations));
        // An explicit key wins over the profile.
        let explicit = FeatureSetConfig {
            siege: Some(false),
            nations: Some(true),
            ..Default::default()
        };
        let f = configured(&explicit, ServerProfile::EuAuroria);
        assert!(!f.check(Feature::Siege));
        assert!(f.check(Feature::Nations));
    }

    #[test]
    fn aa_point_flag_sets_bit_44() {
        let mut config = FeatureSetConfig::default();
        assert!(!configured(&config, crate::config::ServerProfile::Kr12).check(Feature::AaPoint));
        config.aa_point = true;
        let f = configured(&config, crate::config::ServerProfile::Kr12);
        assert!(f.check(Feature::AaPoint));
        assert!(f.check_bit(44));
        assert_eq!(f.bytes()[5], 0x39);
    }

    /// Flips one configured switch and asserts that exactly its bit changed; flipping it back
    /// restores every byte.
    fn assert_only_bit_flips(name: &str, bit: u8, apply: impl Fn(&mut FeatureSetConfig)) {
        let profile = crate::config::ServerProfile::Kr12;
        let base_config = FeatureSetConfig::default();
        let base = configured(&base_config, profile);
        let default_on = base.check_bit(bit);

        let mut flipped_config = base_config.clone();
        apply(&mut flipped_config);
        let flipped = configured(&flipped_config, profile);
        for b in 0..(FSET_BYTES * 8) as u8 {
            let expected = if b == bit {
                !default_on
            } else {
                base.check_bit(b)
            };
            assert_eq!(flipped.check_bit(b), expected, "{name}: bit {b}");
        }

        let mut restored_config = flipped_config;
        apply(&mut restored_config);
        assert_eq!(
            configured(&restored_config, profile),
            base,
            "{name}: flipped twice"
        );
    }

    #[test]
    fn named_flags_flip_exactly_their_bit() {
        assert_only_bit_flips("siege", 0, |c| c.siege = Some(!c.siege.unwrap_or(true)));
        assert_only_bit_flips("nations", 6, |c| {
            c.nations = Some(!c.nations.unwrap_or(true))
        });
        assert_only_bit_flips("premium", 4, |c| c.premium = !c.premium);
        assert_only_bit_flips("itemSecure", 45, |c| c.item_secure = !c.item_secure);
        assert_only_bit_flips("secondpass", 46, |c| c.secondpass = !c.secondpass);
        assert_only_bit_flips("ranking", 36, |c| c.ranking = !c.ranking);
        assert_only_bit_flips("slave_customize", 48, |c| {
            c.slave_customize = !c.slave_customize
        });
        assert_only_bit_flips("bm_mileage", 43, |c| c.bm_mileage = !c.bm_mileage);
        assert_only_bit_flips("ingamecashshop", 38, |c| {
            c.ingamecashshop = !c.ingamecashshop
        });
        assert_only_bit_flips("customsaveload", 40, |c| {
            c.customsaveload = !c.customsaveload
        });
        assert_only_bit_flips("beautyshopBypass", 52, |c| {
            c.beautyshop_bypass = !c.beautyshop_bypass
        });
        assert_only_bit_flips("ingameshopSecondpass", 55, |c| {
            c.ingameshop_secondpass = !c.ingameshop_secondpass
        });
        assert_only_bit_flips("backpackProfitShare", 56, |c| {
            c.backpack_profit_share = !c.backpack_profit_share
        });
        assert_only_bit_flips("sensitiveOpeartion", 58, |c| {
            c.sensitive_operation = !c.sensitive_operation
        });
        assert_only_bit_flips("taxItem", 59, |c| c.tax_item = !c.tax_item);
        assert_only_bit_flips("achievement", 63, |c| c.achievement = !c.achievement);
        assert_only_bit_flips("dwarfWarborn", 74, |c| c.dwarf_warborn = !c.dwarf_warborn);
        assert_only_bit_flips("mailCoolTime", 75, |c| c.mail_cool_time = !c.mail_cool_time);
        assert_only_bit_flips("hudAuctionButton", 81, |c| {
            c.hud_auction_button = !c.hud_auction_button
        });
        assert_only_bit_flips("auctionPostBuff", 83, |c| {
            c.auction_post_buff = !c.auction_post_buff
        });
        assert_only_bit_flips("aaPoint", 44, |c| c.aa_point = !c.aa_point);
    }

    #[test]
    fn extra_bits_only_change_the_listed_bits() {
        let base = configured(
            &FeatureSetConfig::default(),
            crate::config::ServerProfile::Kr12,
        );
        let mut config = FeatureSetConfig::default();
        config.extra_bits.insert(78, true);
        config.extra_bits.insert(34, true);
        config.extra_bits.insert(32, false);
        config.extra_bits.insert(87, true);
        let f = configured(&config, crate::config::ServerProfile::Kr12);
        for b in 0..(FSET_BYTES * 8) as u8 {
            let expected = match b {
                78 | 34 | 87 => true,
                32 => false,
                _ => base.check_bit(b),
            };
            assert_eq!(f.check_bit(b), expected, "bit {b}");
        }
    }

    #[test]
    fn extra_bits_out_of_range_are_ignored() {
        let mut config = FeatureSetConfig::default();
        config.extra_bits.insert(200, true);
        assert_eq!(
            configured(&config, crate::config::ServerProfile::Kr12),
            configured(
                &FeatureSetConfig::default(),
                crate::config::ServerProfile::Kr12
            )
        );
    }

    #[test]
    fn config_binds_switches_and_extra_bits() {
        let config: FeatureSetConfig = serde_json::from_value(serde_json::json!({
            "siege": false,
            "dwarfwarborn": "true",
            "aapoint": true,
            "extrabits": { "78": true, "32": "false" }
        }))
        .unwrap();
        assert_eq!(config.siege, Some(false));
        assert!(config.dwarf_warborn);
        assert!(config.aa_point);
        assert_eq!(config.extra_bits.get(&78), Some(&true));
        assert_eq!(config.extra_bits.get(&32), Some(&false));
    }
}
