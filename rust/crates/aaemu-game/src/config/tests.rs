use super::*;
use serde_json::json;

#[test]
fn merges_case_insensitively_and_binds_strings() {
    let mut base = json!({
        "Id": 1, "SecretKey": "test", "AdditionalesId": [],
        "Network": { "Host": "*", "Port": 1239, "NumConnections": 10 },
        "LoginNetwork": { "Host": "%login_host%", "Port": "%login_port%" }
    });
    merge(
        &mut base,
        json!({ "loginnetwork": { "host": "127.0.0.1", "PORT": "1234" }, "Account": { "DeleteReleaseName": "True" } }),
    );
    let cfg = AppConfig::from_value(base).unwrap();
    assert_eq!(cfg.id, 1);
    assert_eq!(cfg.network.port, 1239);
    assert_eq!(cfg.login_network.host, "127.0.0.1");
    assert_eq!(cfg.login_network.port, 1234);
    assert!(cfg.account.delete_release_name);
    assert_eq!(cfg.account.access_level_first_account, 100);
}

/// Test accounts receive their starting Credits/Loyalty when the game creates their account
/// row on first login. The defaults stay 0, so a normal server is unaffected.
#[test]
fn test_server_starting_credits_and_loyalty_bind() {
    assert_eq!(AppConfig::default().credits.default, 0);
    assert_eq!(AppConfig::default().loyalty.default, 0);
    let cfg = AppConfig::from_value(json!({
        "Credits": { "Default": 1_000_000 },
        "Loyalty": { "Default": "5000" }
    }))
    .unwrap();
    assert_eq!(cfg.credits.default, 1_000_000);
    assert_eq!(cfg.loyalty.default, 5_000);
}

/// The shipped example file binds to the values the wiki page documents (a typo in the file
/// would otherwise only show up on a test server).
#[test]
fn the_shipped_test_server_example_binds_to_its_documented_values() {
    let value: serde_json::Value =
        serde_json::from_str(include_str!("../../app/ExampleConfig.TestServer.json"))
            .expect("the example is valid JSON");
    let cfg = AppConfig::from_value(value).unwrap();
    assert_eq!(cfg.credits.default, 1_000_000);
    assert_eq!(cfg.loyalty.default, 5_000);
}

#[test]
fn characters_config_binds_the_base_slot_count() {
    let cfg = AppConfig::from_value(json!({
        "Characters": { "MaxPerAccount": "8" }
    }))
    .unwrap();
    assert_eq!(cfg.characters.max_per_account, 8);
    assert_eq!(AppConfig::default().characters.max_per_account, 6);
}

#[test]
fn characters_config_binds_the_pre_select_period() {
    let cfg = AppConfig::from_value(json!({
        "Characters": { "PreSelectPeriod": "True" }
    }))
    .unwrap();
    assert!(cfg.characters.pre_select_period);
    assert!(!AppConfig::default().characters.pre_select_period);
    let cfg = AppConfig::from_value(json!({
        "Characters": { "PreSelectPeriod": false }
    }))
    .unwrap();
    assert!(!cfg.characters.pre_select_period);
}

#[test]
fn account_restrictions_and_idle_kick_bind_from_config() {
    use crate::net::connection::PaymentMethodType;

    let default = AppConfig::default();
    assert_eq!(default.account.idle_kick_time, 0);
    for method in [
        PaymentMethodType::Premium,
        PaymentMethodType::Demo,
        PaymentMethodType::None,
    ] {
        let flags = default.account.restrictions.for_payment(method);
        assert!(flags.can_place_house);
        assert!(flags.can_pay_tax);
        assert!(flags.can_use_auction);
        assert!(flags.can_trade);
        assert!(flags.can_send_mail);
        assert!(flags.can_use_bank);
        assert!(flags.can_use_copper);
    }

    let cfg = AppConfig::from_value(json!({
        "Account": {
            "IdleKickTime": "1800",
            "Restrictions": {
                "Demo": { "CanTrade": false, "CanSendMail": false }
            }
        }
    }))
    .unwrap();
    assert_eq!(cfg.account.idle_kick_time, 1800);
    let demo = cfg
        .account
        .restrictions
        .for_payment(PaymentMethodType::Demo);
    assert!(!demo.can_trade);
    assert!(!demo.can_send_mail);
    // Missing flags keep the default `true` (only the two listed are off).
    assert!(demo.can_place_house);
    assert!(demo.can_pay_tax);
    assert!(demo.can_use_auction);
    assert!(demo.can_use_bank);
    assert!(demo.can_use_copper);
    // The other methods are unaffected.
    assert!(
        cfg.account
            .restrictions
            .for_payment(PaymentMethodType::Premium)
            .can_trade
    );
    assert!(
        cfg.account
            .restrictions
            .for_payment(PaymentMethodType::None)
            .can_trade
    );
}

#[test]
fn account_trion_binds_from_config() {
    let default = AppConfig::default();
    assert!(default.account.trion.activate);
    assert_eq!(
        default.account.trion.auth_url,
        "http://localhost/aaemu/login"
    );
    assert_eq!(
        default.account.trion.platform_url,
        "http://localhost/aaemu/platform"
    );
    assert_eq!(
        default.account.trion.commerce_url,
        "http://localhost/aaemu/shop"
    );

    let cfg = AppConfig::from_value(json!({
        "Account": {
            "Trion": {
                "Activate": "False",
                "Auth_Url": "https://auth.example.test/l",
                "Platform_Url": "https://platform.example.test",
                "Commerce_Url": "https://shop.example.test/c"
            }
        }
    }))
    .unwrap();
    assert!(!cfg.account.trion.activate);
    assert_eq!(cfg.account.trion.auth_url, "https://auth.example.test/l");
    assert_eq!(
        cfg.account.trion.platform_url,
        "https://platform.example.test"
    );
    assert_eq!(
        cfg.account.trion.commerce_url,
        "https://shop.example.test/c"
    );
}

#[test]
fn binds_debug_reply_delay_from_json() {
    let cfg = AppConfig::from_value(json!({
        "Debug": {
            "ReplyDelay": {
                "DelayMs": "600",
                "Packets": ["SCSkillStarted", "SCSkillFired"],
                "RepliesTo": ["CSStartSkill"],
                "CharacterName": "Capture",
                "DelayEverythingAfter": true
            }
        }
    }))
    .unwrap();
    let delay = &cfg.debug.reply_delay;
    assert_eq!(delay.delay_ms, 600);
    assert_eq!(delay.packets, ["SCSkillStarted", "SCSkillFired"]);
    assert_eq!(delay.replies_to, ["CSStartSkill"]);
    assert_eq!(delay.character_name.as_deref(), Some("Capture"));
    assert!(delay.delay_everything_after);
}

#[test]
fn relaxed_json() {
    let src = "{\n // c\n \"a\": \"x//y\\\"\", /* b */ \"b\": [1, 2, // t\n],\n}";
    let v: Value = serde_json::from_str(&relax_json(src)).unwrap();
    assert_eq!(v, json!({ "a": "x//y\"", "b": [1, 2] }));
}

#[test]
fn arrays_merge_by_index() {
    let mut a = json!({ "A": [1, 2, 3] });
    merge(&mut a, json!({ "a": [9] }));
    assert_eq!(a, json!({ "A": [9, 2, 3] }));
}

/// The shipped application directory (`aaemu-game/app/`).
fn shipped_app_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("app")
}

#[test]
fn shipped_configuration_binds_every_section() {
    let app = shipped_app_path();
    if !app.join("Config.json").exists() {
        return;
    }
    // Config.json keeps `%login_port%` / `%db_port%` placeholders (filled by the installer):
    // override them like the C# command line would.
    let args: Vec<String> = [
        "--LoginNetwork:Port=1234",
        "/Connections:MySQLProvider:Port",
        "3306",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let cfg = AppConfig::load_from(&app, None, Vec::new(), &args).unwrap();
    assert_eq!(cfg.login_network.port, 1234);
    assert_eq!(cfg.connections.mysql_provider.port, 3306);
    let web = cfg.web_api_network.as_ref().expect("WebApiNetwork");
    assert_eq!((web.host.as_str(), web.port), ("127.0.0.1", 1280));
    assert!(web.enabled);
    assert!(web.api_key.is_none());
    assert_eq!(cfg.max_concurency_thread_pool, 8);
    assert!(cfg.discord_token.is_none());
    // World.json
    assert_eq!(cfg.world.motd, "Welcome to AAEmu!");
    assert_eq!(cfg.world.days_for_tax_payment, 7);
    assert!(cfg.world.geo_data_mode);
    assert!(!cfg.world.prewarm_navmesh);
    assert_eq!(cfg.world.max_instances, 32);
    assert_eq!(cfg.world.wind_model, WindModelType::Official);
    assert_eq!(cfg.world.exp_loss_rate_at_death, 0.05);
    assert_eq!(cfg.world.minimum_exp_loss_level, 10);
    assert_eq!(cfg.world.portal_use_range, 5.0);
    // Dungeons.json
    assert_eq!(cfg.dungeons.auto_team_disband_kick_time, 15);
    assert_eq!(
        cfg.dungeons.auto_create,
        vec![crate::systems::instances::config::DungeonLoadConfig {
            name: "arche_mall_world".into(),
            channel: 0,
            id: 1
        }]
    );
    // Mail.json: the 1.2 client's `mail_guide` windows.
    assert_eq!(cfg.mail.read_retention_days, 5);
    assert_eq!(cfg.mail.unread_retention_days, 30);
    assert_eq!(cfg.mail.system_read_retention_days, 5);
    assert_eq!(cfg.mail.system_unread_retention_days, 90);
    assert!(!cfg.mail.delete_system_mail_with_attachments);
    assert_eq!(cfg.mail.retention_grace_start_unix, 0);
    // Specialty.json, AccessLevels.json, CharacterSettings.json
    assert_eq!(cfg.specialty.trade_pack_mail_delay_in_minutes, 22.0 * 60.0);
    assert_eq!(cfg.access_level_of("help"), Some(0));
    assert_eq!(cfg.access_level_of("fly"), Some(50));
    assert_eq!(cfg.access_level_of("teleport"), Some(50));
    assert_eq!(cfg.access_level_of("announce"), Some(50));
    assert_eq!(cfg.access_level_of("Announce"), None);
    assert!(cfg.account.delete_release_name);
    assert_eq!(cfg.account.name_regex, "^[a-zA-Z0-9]{1,18}$");
    assert_eq!(cfg.scripts.load_strategy, LoadStrategyType::Reflection);
    assert_eq!(cfg.client_data.sources.len(), 4);
    // Events.json / Config.json: the Halloween window defaults to the yearly roll.
    assert!(cfg.events.halloween.enabled);
    assert_eq!(cfg.events.halloween.start, "");
    assert_eq!(cfg.events.halloween.end, "");
    // FeatureSet.json: the shipped switches leave the profile-driven bits (`siege`,
    // `nations`) unset, the rest match the current bytes, and the unnamed-bit override list
    // starts empty.
    assert_eq!(cfg.feature_set.siege, None);
    assert_eq!(cfg.feature_set.nations, None);
    assert!(!cfg.feature_set.aa_point);
    assert!(!cfg.feature_set.dwarf_warborn);
    assert!(cfg.feature_set.extra_bits.is_empty());
    // The shipped file applied to the default profile and stock level caps.
    assert_eq!(cfg.world.profile, ServerProfile::EuAuroria);
    let mut fset = crate::data::FeatureSet::initialized(
        cfg.world.profile.player_level_cap(),
        crate::data::experience::MATE_LEVEL_CAP,
    );
    fset.apply_config(
        &cfg.feature_set,
        cfg.auroria_claims_enabled(),
        cfg.player_nations_enabled(),
    );
    assert_eq!(
        fset.bytes(),
        &[0x1B, 50, 0x0F, 0x0F, 0xF9, 0x29, 0x33, 0x89, 50, 0x08, 0x1a]
    );
}

#[test]
fn feature_set_section_binds_switches_and_extra_bits() {
    let cfg = AppConfig::from_value(json!({
        "FeatureSet": {
            "Siege": false,
            "DwarfWarborn": "true",
            "aaPoint": true,
            "ExtraBits": { "78": true, "32": "false" }
        }
    }))
    .unwrap();
    assert_eq!(cfg.feature_set.siege, Some(false));
    assert!(cfg.feature_set.dwarf_warborn);
    assert!(cfg.feature_set.aa_point);
    assert_eq!(cfg.feature_set.extra_bits.get(&78), Some(&true));
    assert_eq!(cfg.feature_set.extra_bits.get(&32), Some(&false));

    // The section is a top-level key, so the `FeatureSet__Key` environment form binds too.
    let mut value = json!({});
    apply_env(
        &mut value,
        [("FeatureSet__Siege".to_string(), "true".to_string())],
    );
    let cfg = AppConfig::from_value(value).unwrap();
    assert_eq!(cfg.feature_set.siege, Some(true));
}

#[test]
fn web_api_security_settings_bind_from_environment() {
    let mut value = json!({"WebApiNetwork": {"Host": "127.0.0.1"}});
    apply_env(
        &mut value,
        [
            (
                "WebApiNetwork__ApiKey".into(),
                "synthetic-environment-key".into(),
            ),
            ("WebApiNetwork__Enabled".into(), "false".into()),
        ],
    );
    let config = AppConfig::from_value(value).unwrap();
    let web = config.web_api_network.unwrap();
    assert_eq!(web.api_key.as_deref(), Some("synthetic-environment-key"));
    assert!(!web.enabled);
    assert!(AppConfig::from_value(json!({"WebApiNetwork": null}))
        .unwrap()
        .web_api_network
        .is_none());
}

#[test]
fn overrides_apply_in_builder_order() {
    let dir = std::env::temp_dir().join(format!("aaemu-cfg-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("Configurations")).unwrap();
    std::fs::write(
        dir.join("Config.json"),
        r#"{ "Id": 1, "World": { "ExpRate": 1.0 }, "WebApiNetwork": { "Host": "*", "Port": 1 } }"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("Configurations/World.json"),
        r#"{ "World": { "ExpRate": 2.0, "WindModel": "realistic", "RadarGmAccessLevel": 90 } }"#,
    )
    .unwrap();
    let secrets = dir.join("secrets.json");
    std::fs::write(&secrets, r#"{ "World:ExpRate": 3, "DiscordToken": "tok" }"#).unwrap();
    // Secrets beat the files.
    let cfg = AppConfig::load_from(&dir, Some(&secrets), Vec::new(), &[]).unwrap();
    assert_eq!(cfg.world.exp_rate, 3.0);
    assert_eq!(cfg.world.wind_model, WindModelType::Realistic);
    assert_eq!(cfg.world.radar_gm_access_level, 90);
    assert_eq!(cfg.discord_token.as_deref(), Some("tok"));
    // Environment beats secrets; unknown top-level variables are ignored.
    let env = vec![
        ("WORLD__EXPRATE".to_string(), "4".to_string()),
        ("Path".to_string(), "C:/x".to_string()),
        ("WebApiNetwork__Port".to_string(), "1280".to_string()),
    ];
    let cfg = AppConfig::load_from(&dir, Some(&secrets), env.clone(), &[]).unwrap();
    assert_eq!(cfg.world.exp_rate, 4.0);
    assert_eq!(cfg.web_api_network.unwrap().port, 1280);
    // The command line beats everything.
    let args = vec!["World:ExpRate=5".to_string(), "-x".to_string()];
    let cfg = AppConfig::load_from(&dir, Some(&secrets), env, &args).unwrap();
    assert_eq!(cfg.world.exp_rate, 5.0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_sections_keep_the_csharp_defaults() {
    let cfg = AppConfig::from_value(json!({})).unwrap();
    assert!(cfg.web_api_network.is_none());
    assert_eq!(cfg.labor_offline.tick_minutes, 5);
    assert_eq!(cfg.world.target_physics_tps, 25.0);
    assert_eq!(cfg.world.radar_gm_access_level, 100);
    assert!(cfg.dungeons.auto_cleanup_after_kick);
    assert_eq!(cfg.specialty.ratio_decrease_per_pack, f64::from(0.5f32));
    assert_eq!(cfg.credits.get_tick_amount(true), 0);
    assert!(cfg.events.halloween.enabled);
    assert_eq!(cfg.events.halloween.start, "");
    assert_eq!(cfg.events.halloween.end, "");
    let cfg = AppConfig::from_value(json!({
        "Credits": { "TickAmount": 1, "TickAmountPremium": "2" },
        "Scripts": { "LoadStrategy": 0 },
        "AccessLevel": { "Zeta": "5", "alpha": 1 }
    }))
    .unwrap();
    assert_eq!(cfg.credits.get_tick_amount(false), 1);
    assert_eq!(cfg.credits.get_tick_amount(true), 2);
    assert_eq!(cfg.scripts.load_strategy, LoadStrategyType::Compilation);
    assert_eq!(
        cfg.access_level,
        vec![("alpha".to_string(), 1), ("Zeta".to_string(), 5)]
    );
    assert!(AppConfig::from_value(json!({ "World": { "WindModel": "Nope" } })).is_err());
}

#[test]
fn era_profile_defaults_and_explicit_flags_win() {
    let cfg = AppConfig::from_value(json!({})).unwrap();
    assert_eq!(cfg.world.profile, ServerProfile::EuAuroria);
    assert_eq!(cfg.world.profile.player_level_cap(), 50);
    assert!(!cfg.player_nations_enabled());
    assert!(cfg.siege_calendar_enabled());
    assert!(cfg.auroria_claims_enabled());
    assert_eq!(format!("{}", cfg.world.profile), "EuAuroria");
    assert_eq!(cfg.world.profile.lodestone_max_hp(), Some(4_100_000));
    assert_eq!(cfg.world.profile.defense_tower_max_hp(), Some(1_500_000));

    let cfg = AppConfig::from_value(json!({ "World": { "Profile": "EuLaunch" } })).unwrap();
    assert_eq!(cfg.world.profile, ServerProfile::EuLaunch);
    assert_eq!(cfg.world.profile.player_level_cap(), 50);
    assert!(!cfg.player_nations_enabled());
    assert!(!cfg.siege_calendar_enabled());
    assert!(!cfg.auroria_claims_enabled());
    assert_eq!(cfg.world.profile.lodestone_max_hp(), None);
    assert_eq!(cfg.world.profile.defense_tower_max_hp(), None);

    let cfg = AppConfig::from_value(json!({ "World": { "Profile": "Kr12" } })).unwrap();
    assert_eq!(cfg.world.profile, ServerProfile::Kr12);
    assert_eq!(cfg.world.profile.player_level_cap(), 55);
    assert!(cfg.player_nations_enabled());
    assert!(cfg.siege_calendar_enabled());
    assert!(cfg.auroria_claims_enabled());
    assert_eq!(cfg.world.profile.lodestone_max_hp(), None);
    assert_eq!(cfg.world.profile.defense_tower_max_hp(), None);

    // An explicit flag wins over the profile in both directions.
    let cfg = AppConfig::from_value(json!({
        "World": { "Profile": "EuLaunch" },
        "Social": { "PlayerNations": true },
        "Housing": { "SiegeCalendar": "true" }
    }))
    .unwrap();
    assert!(cfg.player_nations_enabled());
    assert!(cfg.siege_calendar_enabled());
    // Claims follow the launch profile; the explicit calendar does not enable them.
    assert!(!cfg.auroria_claims_enabled());
    let cfg = AppConfig::from_value(json!({
        "World": { "Profile": "Kr12" },
        "Social": { "PlayerNations": "false" },
        "Housing": { "SiegeCalendar": false }
    }))
    .unwrap();
    assert!(!cfg.player_nations_enabled());
    assert!(!cfg.siege_calendar_enabled());
    // Claims follow the KR profile; the explicit calendar does not disable them.
    assert!(cfg.auroria_claims_enabled());
    assert_eq!(cfg.world.profile.player_level_cap(), 55);

    // The key binds through the environment (`World__Profile`) and the command line.
    let mut value = json!({});
    apply_env(
        &mut value,
        [("World__Profile".to_string(), "Kr12".to_string())],
    );
    let cfg = AppConfig::from_value(value).unwrap();
    assert_eq!(cfg.world.profile, ServerProfile::Kr12);
    let mut value = json!({});
    apply_args(&mut value, &["--World:Profile=EuLaunch".to_string()]);
    let cfg = AppConfig::from_value(value).unwrap();
    assert_eq!(cfg.world.profile, ServerProfile::EuLaunch);
    assert!(AppConfig::from_value(json!({ "World": { "Profile": "Nope" } })).is_err());
}

#[test]
fn siege_calendar_is_independent_of_claims_and_the_client_bit() {
    use crate::data::experience::MATE_LEVEL_CAP;
    use crate::data::{Feature, FeatureSet};

    let client_siege = |cfg: &AppConfig| {
        let mut f = FeatureSet::initialized(cfg.world.profile.player_level_cap(), MATE_LEVEL_CAP);
        f.apply_config(
            &cfg.feature_set,
            cfg.auroria_claims_enabled(),
            cfg.player_nations_enabled(),
        );
        f.check(Feature::Siege)
    };

    // Auroria + calendar off (GM schedules only): claims and the client bit stay on.
    let cfg = AppConfig::from_value(json!({
        "World": { "Profile": "EuAuroria" },
        "Housing": { "SiegeCalendar": false }
    }))
    .unwrap();
    assert!(!cfg.siege_calendar_enabled());
    assert!(cfg.auroria_claims_enabled());
    assert!(client_siege(&cfg));

    // Launch: claims and the client bit are off even with the calendar forced on.
    let cfg = AppConfig::from_value(json!({
        "World": { "Profile": "EuLaunch" },
        "Housing": { "SiegeCalendar": true }
    }))
    .unwrap();
    assert!(cfg.siege_calendar_enabled());
    assert!(!cfg.auroria_claims_enabled());
    assert!(!client_siege(&cfg));

    // Kr12: claims stay on with the calendar off.
    let cfg = AppConfig::from_value(json!({
        "World": { "Profile": "Kr12" },
        "Housing": { "SiegeCalendar": "false" }
    }))
    .unwrap();
    assert!(!cfg.siege_calendar_enabled());
    assert!(cfg.auroria_claims_enabled());
    assert!(client_siege(&cfg));

    // An explicit FeatureSet.siege value wins over the profile under Auroria.
    let cfg = AppConfig::from_value(json!({
        "World": { "Profile": "EuAuroria" },
        "FeatureSet": { "Siege": false }
    }))
    .unwrap();
    assert!(cfg.auroria_claims_enabled());
    assert!(!client_siege(&cfg));
}

#[test]
fn world_npc_settings_default_and_bind_leniently() {
    // Missing settings keep their documented defaults.
    let cfg = AppConfig::from_value(json!({ "World": {} })).unwrap();
    assert_eq!(cfg.world.npc_prewarm_region_margin, 1);
    assert_eq!(cfg.world.npc_spawn_tick_ms, 250);
    assert_eq!(cfg.world.npc_spawns_per_tick, 64);
    assert_eq!(cfg.world.npc_warm_retention_seconds, 60);
    assert!(cfg.world.npc_resident_service_npcs);
    assert_eq!(cfg.world.ai_worker_threads, 1);
    assert_eq!(cfg.world.npc_spawn_scatter_trigger, 1.0);
    assert_eq!(cfg.world.npc_spawn_scatter_min, 1.5);
    assert_eq!(cfg.world.npc_spawn_scatter_max, 3.0);
    assert_eq!(cfg.world.stacked_npc_check_seconds, 60);
    assert_eq!(cfg.world.stacked_npc_radius, 0.5);
    assert_eq!(cfg.world.stacked_npc_min_seconds, 120);
    // Binding is case-insensitive and accepts string values.
    let cfg = AppConfig::from_value(json!({
        "world": {
            "npcprewarmregionmargin": "2",
            "NPCSpawnTickMs": "500",
            "NpcSpawnsPerTick": 8,
            "npcwarmretentionseconds": "300",
            "NpcResidentServiceNpcs": "False",
            "AIWorkerThreads": "4",
            "NpcSpawnScatterTrigger": "2",
            "npcspawnscattermin": 2.5,
            "NpcSpawnScatterMax": "0",
            "StackedNpcCheckSeconds": "30",
            "StackedNpcRadius": 0.25,
            "StackedNpcMinSeconds": "10"
        }
    }))
    .unwrap();
    assert_eq!(cfg.world.npc_prewarm_region_margin, 2);
    assert_eq!(cfg.world.npc_spawn_tick_ms, 500);
    assert_eq!(cfg.world.npc_spawns_per_tick, 8);
    assert_eq!(cfg.world.npc_warm_retention_seconds, 300);
    assert!(!cfg.world.npc_resident_service_npcs);
    assert_eq!(cfg.world.ai_worker_threads, 4);
    assert_eq!(cfg.world.npc_spawn_scatter_trigger, 2.0);
    assert_eq!(cfg.world.npc_spawn_scatter_min, 2.5);
    assert_eq!(cfg.world.npc_spawn_scatter_max, 0.0);
    assert_eq!(cfg.world.stacked_npc_check_seconds, 30);
    assert_eq!(cfg.world.stacked_npc_radius, 0.25);
    assert_eq!(cfg.world.stacked_npc_min_seconds, 10);
}

#[test]
fn loot_announce_to_party_defaults_false_and_binds_leniently() {
    let cfg = AppConfig::from_value(json!({})).unwrap();
    assert!(!cfg.loot.announce_to_party);
    let cfg = AppConfig::from_value(json!({
        "Loot": { "AnnounceToParty": "True" }
    }))
    .unwrap();
    assert!(cfg.loot.announce_to_party);
}

#[test]
fn events_halloween_binds_leniently() {
    let cfg = AppConfig::from_value(json!({
        "Events": { "Halloween": {
            "Enabled": "False",
            "Start": "2026-10-14T00:00:00Z",
            "End": "2026-11-01T00:00:00Z"
        } }
    }))
    .unwrap();
    assert!(!cfg.events.halloween.enabled);
    assert_eq!(cfg.events.halloween.start, "2026-10-14T00:00:00Z");
    assert_eq!(cfg.events.halloween.end, "2026-11-01T00:00:00Z");
}

#[test]
fn scheduled_events_default_off_and_bind_leniently() {
    let cfg = AppConfig::from_value(json!({})).unwrap();
    assert!(!cfg.events.scheduled_events.enabled);
    assert!(cfg.events.scheduled_events.events.is_empty());

    let cfg = AppConfig::from_value(json!({
        "Events": { "ScheduledEvents": {
            "Enabled": "True",
            "Events": [
                { "ScheduleId": "14", "OpenMsg": "Weekend dungeon open",
                  "CloseMsg": "Weekend dungeon closed" }
            ]
        } }
    }))
    .unwrap();
    assert!(cfg.events.scheduled_events.enabled);
    assert_eq!(
        cfg.events.scheduled_events.events,
        vec![ScheduledEventEntry {
            schedule_id: 14,
            open_msg: "Weekend dungeon open".into(),
            close_msg: "Weekend dungeon closed".into(),
        }]
    );
}

#[test]
fn config_cell_replaces_values_at_runtime() {
    let cell = ConfigCell::new(AppConfig::default());
    let before = cell.get();
    cell.update(|c| c.world.exp_rate = 7.0);
    assert_eq!(cell.world.exp_rate, 7.0);
    // Earlier snapshots stay valid and unchanged.
    assert_eq!(before.world.exp_rate, 1.0);
    cell.replace(AppConfig::default());
    assert_eq!(cell.world.exp_rate, 1.0);
}
