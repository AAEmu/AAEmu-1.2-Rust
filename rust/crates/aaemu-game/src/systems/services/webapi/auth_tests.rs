//! Security regressions through the production route dispatcher and HTTP adapter. No live
//! stack or database is needed; privileged handlers leave observable in-memory effects.

use std::sync::{Arc, Mutex};

use axum::http::{header::AUTHORIZATION, HeaderValue, Request};
use serde_json::json;

use super::hooks::{WebApiEconomyHooks, WebApiMail};
use super::*;
use crate::config::{AppConfig, WebApiConfig};
use crate::data::{items::ItemTemplate, GameData};
use crate::systems::services::commands::{output::MessageOutput, Command, CommandContext};
use crate::test_support::{
    spawn_player, test_character, test_server, test_server_with, TestClient,
};

pub(super) const TEST_KEY: &str = "synthetic-webapi-test-key";

pub(super) fn authenticated(mut request: HttpRequest) -> HttpRequest {
    request.headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer synthetic-webapi-test-key"),
    );
    request
}

fn api(server: &Arc<GameServer>, key: Option<&str>) -> WebApiServer {
    server.config.update(|c| {
        c.web_api_network = Some(WebApiConfig {
            api_key: key.map(str::to_owned),
            ..Default::default()
        });
    });
    WebApiServer::new(Arc::downgrade(server)).unwrap()
}

fn post(url: &str, body: &str) -> HttpRequest {
    HttpRequest::new("POST", url, "HTTP/1.1").with_body(body)
}

fn invalid(mut request: HttpRequest) -> HttpRequest {
    request.headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer synthetic-webapi-bad-key"),
    );
    request
}

fn assert_unauthorized(response: &HttpResponse) {
    assert_eq!(response.status, 401);
    assert_eq!(response.status_phrase, "Unauthorized");
    assert!(response.headers.iter().any(|(name, value)| {
        name == "WWW-Authenticate" && value == "Bearer realm=\"AAEmu Web API\""
    }));
    assert!(!response.body.contains(TEST_KEY));
}

struct SetHp;

impl Command for SetHp {
    fn command_names(&self) -> Vec<String> {
        vec!["auth-test".into()]
    }

    fn execute(
        &self,
        ctx: &CommandContext<'_>,
        _args: &[String],
        _out: &mut dyn MessageOutput,
    ) -> anyhow::Result<()> {
        ctx.player()?.data().unit.hp = 42;
        Ok(())
    }

    fn get_command_line_help(&self) -> String {
        String::new()
    }

    fn get_command_help_text(&self) -> String {
        String::new()
    }
}

#[tokio::test]
async fn command_requires_credentials_before_executing_as_online_gm() {
    let server = Arc::new(test_server());
    let client = TestClient::new(&server, 1, 1);
    let mut character = test_character(1, "SyntheticGM", 10.0, 10.0);
    character.access_level = 100;
    character.unit.hp = 100;
    let player = spawn_player(&server, &client, character, 0x100);
    server
        .systems
        .services
        .commands
        .register("auth-test", Arc::new(SetHp));
    let api = api(&server, Some(TEST_KEY));
    let request = post(
        "/api/commands/auth-test",
        r#"{"character":"SyntheticGM","arguments":"fixture"}"#,
    );
    for denied in [request.clone(), invalid(request.clone())] {
        assert_unauthorized(&on_received_request(&api, denied).await);
        assert_eq!(player.data().unit.hp, 100);
    }
    let response = on_received_request(&api, authenticated(request)).await;
    assert_eq!(response.status, 200);
    assert_eq!(player.data().unit.hp, 42);
}

#[derive(Default)]
struct Sent(Mutex<Vec<WebApiMail>>);

impl WebApiEconomyHooks for Sent {
    fn send_mail(&self, _server: &GameServer, mail: WebApiMail) -> bool {
        self.0.lock().unwrap().push(mail);
        true
    }
}

#[tokio::test]
async fn mail_with_items_requires_credentials_in_http_adapter() {
    let mut data = GameData::empty();
    data.items
        .templates
        .insert(12345, ItemTemplate::plain(12345, 20));
    let server = Arc::new(test_server_with(data));
    let sent = Arc::new(Sent::default());
    server
        .systems
        .services
        .webapi_economy_hooks
        .install(sent.clone());
    let client = TestClient::new(&server, 1, 1);
    spawn_player(
        &server,
        &client,
        test_character(1, "Bob", 10.0, 10.0),
        0x100,
    );
    let api = Arc::new(api(&server, Some(TEST_KEY)));
    for credential in [
        None,
        Some("Bearer synthetic-webapi-bad-key"),
        Some("Bearer synthetic-webapi-test-key"),
    ] {
        let mut builder = Request::builder().method("POST").uri("/api/mail/send");
        if let Some(credential) = credential {
            // HTTP header names and the Bearer scheme are case-insensitive.
            builder = builder.header("aUtHoRiZaTiOn", credential);
        }
        let request = builder.body(Body::from(
            r#"{"Title":"fixture","Body":"fixture","RecipientType":3,"Money":5,"AttachmentItems":[{"Id":12345,"Count":2}]}"#,
        )).unwrap();
        let response = dispatch(State(api.clone()), request).await;
        if credential == Some("Bearer synthetic-webapi-test-key") {
            assert_eq!(response.status(), 200);
            let mails = sent.0.lock().unwrap();
            assert_eq!(mails.len(), 1);
            assert_eq!(mails[0].receiver_name, "Bob");
            assert_eq!(mails[0].copper_coins, 5);
            assert_eq!(mails[0].attachments.len(), 1);
            assert_eq!(mails[0].attachments[0].template_id, 12345);
            assert_eq!(mails[0].attachments[0].count, 2);
        } else {
            assert_eq!(response.status(), 401);
            assert!(sent.0.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn all_api_routes_require_auth_even_with_legacy_regex_urls() {
    let server = Arc::new(test_server());
    let api = api(&server, Some(TEST_KEY));
    for (method, url) in [
        ("POST", "/api/commands/kick"),
        ("POST", "/api/mail/send"),
        ("POST", "/api/mail/delete"),
        ("POST", "/api/mail/list"),
        ("POST", "/api/auction/add"),
        ("POST", "/api/auction/generate"),
        ("GET", "/api/character/list"),
        ("GET", "/api/world/logged-characters"),
        ("GET", "/api/expedition/list"),
        ("GET", "/api/auction/list"),
        ("GET", "/api/auction/search"),
        ("POST", "/status?next=/api/mail/send"),
        ("POST", "/prefix/API/COMMANDS/kick?status=1"),
        ("POST", "/?next=/api/mail/send"),
    ] {
        // Invalid JSON must not be parsed before authentication.
        let request = HttpRequest::new(method, url, "HTTP/1.1").with_body("invalid JSON");
        for denied in [request.clone(), invalid(request)] {
            assert_unauthorized(&on_received_request(&api, denied).await);
        }
    }
}

#[tokio::test]
async fn status_remains_public_and_missing_key_never_grants_api_access() {
    let server = Arc::new(test_server());
    for key in [None, Some(""), Some(TEST_KEY)] {
        let api = api(&server, key);
        let response =
            on_received_request(&api, HttpRequest::new("GET", "/status", "HTTP/1.1")).await;
        assert_eq!(response.status, 200);
        assert!(response.body.contains("Players online: 0"));
        if key != Some(TEST_KEY) {
            for url in ["/api/commands/kick", "/api/mail/send"] {
                assert_unauthorized(
                    &on_received_request(&api, authenticated(post(url, "{}"))).await,
                );
            }
        }
    }
}

#[test]
fn bearer_parser_rejects_malformed_and_duplicate_credentials() {
    let request = post("/api/mail/send", "{}");
    for value in [
        "",
        "Bearer",
        "Bearer ",
        "Basic synthetic-webapi-test-key",
        "Bearer  synthetic-webapi-test-key",
        "Bearer synthetic-webapi-test-key, synthetic-webapi-bad-key",
    ] {
        let mut bad = request.clone();
        bad.headers
            .insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        assert!(!auth::authorized(Some(TEST_KEY), &bad.headers));
    }
    let mut good = request;
    good.headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("bEaReR synthetic-webapi-test-key"),
    );
    assert!(auth::authorized(Some(TEST_KEY), &good.headers));
    good.headers.append(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer synthetic-webapi-test-key"),
    );
    assert!(!auth::authorized(Some(TEST_KEY), &good.headers));
}

#[test]
fn binding_requires_key_for_every_nonloopback_address() {
    for host in [
        "*",
        "0.0.0.0",
        "::",
        "192.0.2.1",
        "2001:db8::1",
        "::ffff:127.0.0.1",
    ] {
        for key in [None, Some("")] {
            let config = WebApiConfig {
                host: host.into(),
                api_key: key.map(str::to_owned),
                ..Default::default()
            };
            assert!(bind_address(&config).is_err(), "{host}");
        }
        let config = WebApiConfig {
            host: host.into(),
            api_key: Some(TEST_KEY.into()),
            ..Default::default()
        };
        assert!(bind_address(&config).is_ok(), "{host}");
    }
    for host in ["127.0.0.1", "127.0.0.2", "::1"] {
        assert!(bind_address(&WebApiConfig {
            host: host.into(),
            ..Default::default()
        })
        .is_ok());
    }
    for key in [" ", "key with spaces", "key\n", "key,other"] {
        let config = WebApiConfig {
            api_key: Some(key.into()),
            ..Default::default()
        };
        let error = bind_address(&config).unwrap_err().to_string();
        assert_eq!(
            error,
            "WebApiNetwork.ApiKey must contain only bearer-token characters"
        );
    }
}

#[tokio::test]
async fn unsafe_legacy_configuration_fails_before_listener_start() {
    let server = Arc::new(test_server());
    for host in ["*", "0.0.0.0", "::", "192.0.2.1"] {
        let config =
            AppConfig::from_value(json!({"WebApiNetwork": {"Host": host, "Port": 0}})).unwrap();
        server
            .config
            .update(|c| c.web_api_network = config.web_api_network);
        let service = WebApiService::default();
        let error = service.start(&server).await.unwrap_err();
        assert!(error.to_string().contains("ApiKey is required"));
        assert!(!service.is_started());
    }
}

#[tokio::test]
async fn disabled_or_absent_api_never_starts_a_listener() {
    let server = Arc::new(test_server());
    for config in [
        None,
        Some(WebApiConfig {
            enabled: false,
            host: "invalid-address".into(),
            ..Default::default()
        }),
    ] {
        server.config.update(|c| c.web_api_network = config);
        let service = WebApiService::default();
        service.start(&server).await.unwrap();
        assert!(!service.is_started());
    }
}

#[test]
fn configuration_defaults_and_diagnostics_do_not_expose_the_key() {
    let config = AppConfig::from_value(json!({"WebApiNetwork": {}})).unwrap();
    let web = config.web_api_network.unwrap();
    assert_eq!((web.host.as_str(), web.port), ("127.0.0.1", 1280));
    assert!(web.enabled);
    assert!(web.api_key.is_none());
    let config =
        AppConfig::from_value(json!({"WebApiNetwork": {"ApiKey": TEST_KEY, "Enabled": "false"}}))
            .unwrap();
    assert!(!config.web_api_network.as_ref().unwrap().enabled);
    assert!(!format!("{config:?}").contains(TEST_KEY));
    assert!(!format!("{:?}", authenticated(post("/api/mail/send", "{}"))).contains(TEST_KEY));
}
