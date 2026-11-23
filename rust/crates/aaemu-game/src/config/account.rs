use super::{lenient, lenient_enum, ConfigEnum};
use serde::Deserialize;

/// `AccountConfig`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AccountConfig {
    /// `NameRegex` (allowed account names).
    #[serde(rename = "nameregex")]
    pub name_regex: String,
    #[serde(rename = "accessleveldefault", deserialize_with = "lenient")]
    pub access_level_default: i32,
    #[serde(rename = "accesslevelfirstaccount", deserialize_with = "lenient")]
    pub access_level_first_account: i32,
    #[serde(rename = "accesslevelfirstcharacter", deserialize_with = "lenient")]
    pub access_level_first_character: i32,
    #[serde(rename = "deletereleasename", deserialize_with = "lenient")]
    pub delete_release_name: bool,
    #[serde(rename = "deletetimings")]
    pub delete_timings: Vec<AccountDeleteDelayTiming>,
    /// Payment state of accounts whose `accounts.pay_method` is 0 (never written by a GM or the
    /// (not yet ported) premium shop). Default `premium`: the owner decision of 2026-11-03 keeps
    /// the C# behaviour for existing databases.
    #[serde(rename = "defaultpayment", deserialize_with = "lenient_enum")]
    pub default_payment: DefaultPayment,
    /// The seven `canXxx` flags of `SCInitialConfig`, per effective payment method (Rust only,
    /// WP-J). The 1.2 client uses them to gate its own UI; the server has no restriction data,
    /// so every default is `true` (today's bytes). The per-method source is INFERRED
    /// (`ACCOUNT-RESTRICT-1`, capture-pending), not client-evidenced.
    #[serde(rename = "restrictions")]
    pub restrictions: AccountRestrictionsConfig,
    /// `SCInitialConfig.idleKickTime` (raw `u16`, `0` = off; Rust only, WP-J). The unit is
    /// **UNKNOWN** (the 1.2 `CGame::Update` consumer was not located): do not document it as
    /// minutes or seconds. The client performs the idle kick itself; the server never kicks.
    /// **Needs one real-client run** with `Account:IdleKickTime=1`: about 1 s means seconds,
    /// about 1 min means minutes. `aaemu-testclient` does not emulate the kick.
    #[serde(rename = "idlekicktime", deserialize_with = "lenient")]
    pub idle_kick_time: u16,
    /// The lobby `SCTrionConfig` values (Rust only, WP-UI4). The defaults are the formerly
    /// hard-coded test URLs. What the client does with `activate: false` is UNKNOWN (CAP-UI-5;
    /// the 1.2 consumer of the struct was not traced), so the default keeps today's `true`.
    #[serde(rename = "trion")]
    pub trion: TrionConfig,
}

/// `Account:Trion` (Rust only, WP-UI4): `SCTrionConfigPacket` sent in the lobby. The embedded
/// browser panels (`webbrowser/web_help`, `web_market`, `web_play_diary`, promotion event,
/// in-game notification) load these URLs; the defaults are the C# test values ("TODO - config
/// files"). `activate: false` is the way to hide the panels once verified, but the client's
/// reaction is UNKNOWN (CAP-UI-5).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TrionConfig {
    #[serde(rename = "activate", deserialize_with = "lenient")]
    pub activate: bool,
    #[serde(rename = "auth_url")]
    pub auth_url: String,
    #[serde(rename = "platform_url")]
    pub platform_url: String,
    #[serde(rename = "commerce_url")]
    pub commerce_url: String,
}

impl Default for TrionConfig {
    fn default() -> Self {
        Self {
            activate: true,
            auth_url: "http://localhost/aaemu/login".into(),
            platform_url: "http://localhost/aaemu/platform".into(),
            commerce_url: "http://localhost/aaemu/shop".into(),
        }
    }
}

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            name_regex: "^[a-zA-Z0-9]{1,18}$".into(),
            access_level_default: 0,
            access_level_first_account: 100,
            access_level_first_character: 100,
            delete_release_name: false,
            delete_timings: Vec::new(),
            default_payment: DefaultPayment::Premium,
            restrictions: AccountRestrictionsConfig::default(),
            idle_kick_time: 0,
            trion: TrionConfig::default(),
        }
    }
}

/// `Account:Restrictions` (Rust only, WP-J): the seven `SCInitialConfig` `canXxx` booleans, keyed
/// by the account's effective payment method. The 1.2 client only uses them to gate its UI; the
/// server does not enforce them. INFERRED source (`ACCOUNT-RESTRICT-1`): the 1.2 client reader
/// (`FUN_397b2a00` -> `FUN_397b1fb0`) names the flags but shows no server-side source.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AccountRestrictionsConfig {
    #[serde(rename = "premium")]
    pub premium: RestrictionFlags,
    #[serde(rename = "demo")]
    pub demo: RestrictionFlags,
    #[serde(rename = "none")]
    pub none: RestrictionFlags,
}

impl AccountRestrictionsConfig {
    /// The flags sent for an effective payment method.
    pub fn for_payment(
        &self,
        method: crate::net::connection::PaymentMethodType,
    ) -> &RestrictionFlags {
        use crate::net::connection::PaymentMethodType;
        match method {
            PaymentMethodType::Premium => &self.premium,
            PaymentMethodType::Demo => &self.demo,
            PaymentMethodType::None => &self.none,
        }
    }
}

/// The seven `SCInitialConfig` `canXxx` booleans in wire order. Every default is `true`, which
/// keeps the bytes the server sent before WP-J.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RestrictionFlags {
    #[serde(rename = "canplacehouse")]
    pub can_place_house: bool,
    #[serde(rename = "canpaytax")]
    pub can_pay_tax: bool,
    #[serde(rename = "canuseauction")]
    pub can_use_auction: bool,
    #[serde(rename = "cantrade")]
    pub can_trade: bool,
    #[serde(rename = "cansendmail")]
    pub can_send_mail: bool,
    #[serde(rename = "canusebank")]
    pub can_use_bank: bool,
    #[serde(rename = "canusecopper")]
    pub can_use_copper: bool,
}

impl Default for RestrictionFlags {
    fn default() -> Self {
        Self {
            can_place_house: true,
            can_pay_tax: true,
            can_use_auction: true,
            can_trade: true,
            can_send_mail: true,
            can_use_bank: true,
            can_use_copper: true,
        }
    }
}

/// `Account:DefaultPayment` (Rust only, WP-B): the effective payment of an account whose
/// `accounts.pay_method` is still 0. The numeric values are the 1.2 `PaymentMethodType`
/// (`Premium = 1`, `Demo = 3`, `None = 5`) the client receives in `SCAccountInfo.payMethod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultPayment {
    #[default]
    Premium,
    Demo,
    None,
}

impl_config_enum!(DefaultPayment { Premium = 1, Demo = 3, None = 5 });

/// `Characters` (Rust only): the lobby character-slot limit and the pre-select period.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CharactersConfig {
    /// Base character slots of every account, before the purchased
    /// `accounts.extra_character_slots` are added. Must equal the login server's
    /// `Login:MaxCharactersPerAccount`; the login server advertises it, this server enforces it.
    /// There is no cross-process check of the two values.
    #[serde(rename = "maxperaccount", deserialize_with = "lenient")]
    pub max_per_account: u8,
    /// Whether the lobby accepts `CSEditCharacter` (character edit at character select). Must
    /// equal the login server's `Login:PreSelectCharacterPeriod`: the login server advertises
    /// the period in `ACJoinResponse.afs` and the client only sends the edit packet while it is
    /// set. There is no cross-process check of the two values.
    #[serde(rename = "preselectperiod", deserialize_with = "lenient")]
    pub pre_select_period: bool,
}

impl Default for CharactersConfig {
    fn default() -> Self {
        Self {
            max_per_account: aaemu_commons::character_slots::DEFAULT_MAX_CHARACTERS_PER_ACCOUNT,
            pre_select_period: false,
        }
    }
}

/// `AccountDeleteDelayTiming`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AccountDeleteDelayTiming {
    #[serde(rename = "level", deserialize_with = "lenient")]
    pub level: u8,
    /// Minutes.
    #[serde(rename = "delay", deserialize_with = "lenient")]
    pub delay: i32,
}
