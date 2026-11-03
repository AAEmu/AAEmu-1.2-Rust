//! World policy for territory property tax rules absent from the available 1.2 server source.

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DominionPropertyTaxBase {
    /// Charge the territory percentage on the weekly amount after multiple-property tax.
    #[default]
    AfterHeavy,
    /// Charge the territory percentage only on the template's ordinary base tax.
    Base,
}

impl crate::config::ConfigEnum for DominionPropertyTaxBase {
    fn parse_config(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("AfterHeavy") || value == "0" {
            Some(Self::AfterHeavy)
        } else if value.eq_ignore_ascii_case("Base") || value == "1" {
            Some(Self::Base)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HousingConfig {
    #[serde(
        rename = "dominionpropertytaxbase",
        deserialize_with = "crate::config::lenient_enum"
    )]
    pub dominion_property_tax_base: DominionPropertyTaxBase,
    /// Drives the weekly siege periods from `siege_zones`/`siege_plans` instead of the
    /// explicit GM schedule alone. Default off: the calendar is a server policy, not a
    /// recovered 1.2 world-server setting. `Housing.SiegeCalendar`.
    #[serde(rename = "siegecalendar", deserialize_with = "crate::config::lenient")]
    pub siege_calendar: bool,
    /// Offset of the `siege_zones` weekday/hour/minute columns, e.g. `+09:00`. Empty or
    /// `local` uses the server's current local offset; the retail timezone is UNKNOWN.
    /// `Housing.SiegeCalendarTimezone`.
    #[serde(rename = "siegecalendartimezone")]
    pub siege_calendar_timezone: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_explicit_property_tax_policy_follow_the_configuration_binder() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        assert_eq!(
            default.housing.dominion_property_tax_base,
            DominionPropertyTaxBase::AfterHeavy
        );
        for value in [serde_json::json!("base"), serde_json::json!(1)] {
            let config = crate::config::AppConfig::from_value(serde_json::json!({
                "Housing": { "DominionPropertyTaxBase": value }
            }))
            .unwrap();
            assert_eq!(
                config.housing.dominion_property_tax_base,
                DominionPropertyTaxBase::Base
            );
        }
        assert!(crate::config::AppConfig::from_value(serde_json::json!({
            "Housing": { "DominionPropertyTaxBase": "unknown" }
        }))
        .is_err());
    }

    #[test]
    fn siege_calendar_binds_off_by_default_and_accepts_a_timezone() {
        let default = crate::config::AppConfig::from_value(serde_json::json!({})).unwrap();
        assert!(!default.housing.siege_calendar);
        assert!(default.housing.siege_calendar_timezone.is_empty());
        for value in [serde_json::json!(true), serde_json::json!("true")] {
            let config = crate::config::AppConfig::from_value(serde_json::json!({
                "Housing": {
                    "SiegeCalendar": value,
                    "SiegeCalendarTimezone": "+09:00"
                }
            }))
            .unwrap();
            assert!(config.housing.siege_calendar);
            assert_eq!(config.housing.siege_calendar_timezone, "+09:00");
        }
    }
}
