//! Controller behaviour through `WebApiSession.OnReceivedRequest` with a test server (no
//! MySQL: the DB-backed paths are covered where they fail before the first query).

use std::sync::Arc;

use super::super::auth_tests::{authenticated, TEST_KEY};
use super::super::http::HttpRequest;
use super::super::{on_received_request, WebApiServer};
use super::*;
use crate::data::world_data::{SystemFaction, ZoneGroup};
use crate::data::GameData;
use crate::systems::housing::dominion::TowerIdentity;
use crate::systems::social::expedition::Expedition;
use crate::test_support::{
    spawn_player, test_character, test_server, test_server_with, TestClient,
};

fn api(server: &Arc<crate::GameServer>) -> WebApiServer {
    server.config.update(|c| {
        c.web_api_network = Some(crate::config::WebApiConfig {
            api_key: Some(TEST_KEY.into()),
            ..Default::default()
        });
    });
    WebApiServer::new(Arc::downgrade(server)).unwrap()
}

async fn get(api: &WebApiServer, url: &str) -> HttpResponse {
    on_received_request(api, authenticated(HttpRequest::new("GET", url, "HTTP/1.1"))).await
}

async fn post(api: &WebApiServer, url: &str, body: &str) -> HttpResponse {
    on_received_request(
        api,
        authenticated(HttpRequest::new("POST", url, "HTTP/1.1").with_body(body)),
    )
    .await
}

#[test]
fn timespan_format() {
    assert_eq!(format_timespan(0), "00:00:00");
    assert_eq!(format_timespan(3_725), "01:02:05");
    assert_eq!(format_timespan(90_061), "1.01:01:01");
}

#[tokio::test]
async fn status_world_and_expeditions() {
    let server = Arc::new(test_server());
    server.set_self();
    let client = TestClient::new(&server, 1, 1);
    let mut c = test_character(1, "Bob", 10.0, 10.0);
    c.created = 1_700_000_000;
    c.is_online = true;
    spawn_player(&server, &client, c, 0x100);
    let api = api(&server);

    let r = get(&api, "/status").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.content_type(), Some("text/html"));
    assert!(r.body.starts_with("Server uptime: "));
    assert!(r.body.ends_with(
        "<br/>Players online: 1<br/>Number of TaskManager Jobs: 0<br/>\
         SkillTlIds in use: 0 / 65525 (peak 0, gets 0, releases 0, unused 0, exhaustions 0, replaced 0, leaked(>10min) 0)<br/>"
    ));

    let r = get(&api, "/api/world/logged-characters").await;
    assert_eq!(r.content_type(), Some("application/json"));
    assert_eq!(
        r.body,
        r#"[{"Id":1,"Name":"Bob","Level":10,"CreatedAt":"2023-11-14T22:13:20","IsOnline":true}]"#
    );

    let r = get(&api, "/api/expedition/list").await;
    assert_eq!((r.status, r.body.as_str()), (200, "[]"));
}

fn guild(id: u32, name: &str) -> Expedition {
    Expedition {
        faction: SystemFaction {
            id,
            name: name.into(),
            ..SystemFaction::default()
        },
        members: Vec::new(),
        policies: Vec::new(),
        removed_members: Vec::new(),
        is_disbanded: false,
    }
}

#[tokio::test]
async fn online_and_dominions_require_credentials() {
    let server = Arc::new(test_server());
    server.set_self();
    let api = api(&server);
    for url in ["/online", "/dominions"] {
        let r = on_received_request(&api, HttpRequest::new("GET", url, "HTTP/1.1")).await;
        assert_eq!(r.status, 401, "{url}");
        assert_eq!(r.status_phrase, "Unauthorized");
        assert_eq!(r.content_type(), Some("application/json"));
        assert_eq!(r.body, r#"{"Message":"Unauthorized","StackTrace":null}"#);
        assert!(r.headers.iter().any(|(name, value)| {
            name == "WWW-Authenticate" && value == "Bearer realm=\"AAEmu Web API\""
        }));
    }
}

#[tokio::test]
async fn online_lists_only_online_characters() {
    let server = Arc::new(test_server());
    server.set_self();
    server.expeditions.add(guild(77, "Knights"));
    let api = api(&server);
    let c1 = TestClient::new(&server, 1, 1);
    let c2 = TestClient::new(&server, 2, 2);
    let c3 = TestClient::new(&server, 3, 3);
    let mut al = test_character(1, "Al", 10.0, 10.0);
    al.is_online = true;
    al.faction_name = "Nuian".into();
    al.unit.expedition_id = 77;
    spawn_player(&server, &c1, al, 0x100);
    let mut bo = test_character(2, "Bo", 10.0, 10.0);
    bo.is_online = true;
    spawn_player(&server, &c2, bo, 0x101);
    // Spawned but not online (not entered the world as a player yet): excluded.
    spawn_player(
        &server,
        &c3,
        test_character(3, "Offline", 10.0, 10.0),
        0x102,
    );

    let r = get(&api, "/online").await;
    assert_eq!(
        (r.status, r.content_type()),
        (200, Some("application/json"))
    );
    assert_eq!(
        r.body,
        r#"{"Count":2,"Characters":[{"Id":1,"Name":"Al","Level":10,"Race":1,"FactionId":101,"FactionName":"Nuian","ExpeditionId":77,"ExpeditionName":"Knights","ZoneId":100},{"Id":2,"Name":"Bo","Level":10,"Race":1,"FactionId":101,"FactionName":"","ExpeditionId":0,"ExpeditionName":null,"ZoneId":100}]}"#
    );
}

#[tokio::test]
async fn dominions_report_owner_period_siege_and_declaration() {
    let mut data = GameData::empty();
    for (id, name) in [(25, "Solzreed"), (26, "Lilyut"), (27, "Austera")] {
        data.zones.groups.insert(
            id,
            ZoneGroup {
                id,
                name: name.into(),
                ..ZoneGroup::default()
            },
        );
    }
    let server = Arc::new(test_server_with(data));
    server.set_self();
    for (id, name) in [(10, "Defenders"), (20, "Attackers"), (30, "Claimants")] {
        server.expeditions.add(guild(id, name));
    }
    let tower = |house_id| TowerIdentity {
        house_id,
        instance_id: 0,
        guard_tower_setting_id: 1,
    };
    let authority = &server.systems.housing.dominions;
    let now = chrono::Utc::now().timestamp();
    let (starts, ends) = (now + 3_600, now + 7_200);
    authority.register_tower(25, tower(250)).unwrap();
    authority
        .declare_unowned_with(25, tower(250), 10, now, || true)
        .unwrap();
    authority.schedule_siege(25, 20, starts, ends, now).unwrap();
    authority.register_tower(26, tower(260)).unwrap();
    authority.register_tower(27, tower(270)).unwrap();
    authority
        .declare_unowned_with(27, tower(270), 10, now, || true)
        .unwrap();
    authority
        .declare(27, 30, 1_700_000_000, now, 0, (1.0, 2.0, 3.0))
        .unwrap();
    let api = api(&server);

    let r = get(&api, "/dominions").await;
    assert_eq!(
        (r.status, r.content_type()),
        (200, Some("application/json"))
    );
    let body: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    let list = body.as_array().expect("array");
    assert_eq!(list.len(), 3);
    let date = |unix: i64| {
        serde_json::json!(crate::systems::services::webapi::json::format_date(
            crate::db::from_unix(unix),
            crate::systems::services::webapi::json::DateKind::Utc
        ))
    };

    let owned = list[0].as_object().expect("owned object");
    assert_eq!(owned.len(), 8);
    assert_eq!(owned["ZoneGroupId"], 25);
    assert_eq!(owned["ZoneGroupName"], "Solzreed");
    assert_eq!(owned["OwnerExpeditionId"], 10);
    assert_eq!(owned["OwnerExpeditionName"], "Defenders");
    assert_eq!(owned["SiegePeriod"], 3);
    assert_eq!(owned["NextSiegeStart"], date(starts));
    assert_eq!(owned["NextSiegeEnd"], date(ends));
    assert_eq!(owned["DeclarationAttacker"], serde_json::Value::Null);

    let unowned = list[1].as_object().expect("unowned object");
    assert_eq!(unowned["ZoneGroupId"], 26);
    assert_eq!(unowned["ZoneGroupName"], "Lilyut");
    assert_eq!(unowned["OwnerExpeditionId"], serde_json::Value::Null);
    assert_eq!(unowned["OwnerExpeditionName"], serde_json::Value::Null);
    assert_eq!(unowned["SiegePeriod"], 0);
    assert_eq!(unowned["NextSiegeStart"], serde_json::Value::Null);
    assert_eq!(unowned["NextSiegeEnd"], serde_json::Value::Null);
    assert_eq!(unowned["DeclarationAttacker"], serde_json::Value::Null);

    let declared = list[2].as_object().expect("declared object");
    assert_eq!(declared["ZoneGroupId"], 27);
    assert_eq!(declared["OwnerExpeditionId"], 10);
    assert_eq!(declared["OwnerExpeditionName"], "Defenders");
    assert_eq!(declared["SiegePeriod"], 5);
    assert_eq!(declared["NextSiegeStart"], serde_json::Value::Null);
    assert_eq!(declared["NextSiegeEnd"], serde_json::Value::Null);
    assert_eq!(declared["DeclarationAttacker"]["ExpeditionId"], 30);
    assert_eq!(declared["DeclarationAttacker"]["Name"], "Claimants");
}

#[tokio::test]
async fn command_controller_validation_and_dispatch() {
    let server = Arc::new(test_server());
    server.set_self();
    let api = api(&server);
    // Missing body -> System.Text.Json exception -> 500 page.
    let r = post(&api, "/api/commands/kick", "").await;
    assert_eq!(r.status, 500);
    assert!(r
        .body
        .contains("The input does not contain any JSON tokens"));
    // Missing property -> KeyNotFoundException -> 500.
    let r = post(&api, "/api/commands/kick", r#"{"character":"Bob"}"#).await;
    assert_eq!(r.status, 500);
    let r = post(
        &api,
        "/api/commands/kick",
        r#"{"character":" ","arguments":"x"}"#,
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(
        r.body,
        r#"{"Message":"Character name is required","StackTrace":null}"#
    );
    let r = post(
        &api,
        "/api/commands/kick",
        r#"{"character":"Bob","arguments":null}"#,
    )
    .await;
    assert_eq!(
        r.body,
        r#"{"Message":"Command line is required","StackTrace":null}"#
    );
    let r = post(
        &api,
        "/api/commands/kick",
        r#"{"character":"Bob","arguments":"x"}"#,
    )
    .await;
    assert_eq!(
        r.body,
        r#"{"Message":"Character \"Bob\" not found","StackTrace":null}"#
    );
    // GET is not a route.
    assert_eq!(get(&api, "/api/commands/kick").await.status, 404);
}

#[tokio::test]
async fn mail_controller_validation() {
    let server = Arc::new(test_server());
    server.set_self();
    let api = api(&server);
    let bad = |m: &str| format!(r#"{{"Message":"{m}","StackTrace":null}}"#);
    let r = post(&api, "/api/mail/send", "null").await;
    assert_eq!(
        (r.status, r.body.clone()),
        (400, bad("Invalid mail request"))
    );
    let r = post(&api, "/api/mail/send", r#"{"Body":"b"}"#).await;
    assert_eq!(r.body, bad("Email Title cannot be empty."));
    let r = post(&api, "/api/mail/send", r#"{"Title":"t"}"#).await;
    assert_eq!(r.body, bad("Email body cannot be empty."));
    let r = post(
        &api,
        "/api/mail/send",
        r#"{"Title":"t","Body":"b","Money":-1}"#,
    )
    .await;
    assert_eq!(r.body, bad("Money cannot be less than 0."));
    let r = post(
        &api,
        "/api/mail/send",
        r#"{"Title":"t","Body":"b","Billing":-1}"#,
    )
    .await;
    assert_eq!(r.body, bad("Billing cannot be less than 0."));
    let r = post(
        &api,
        "/api/mail/send",
        r#"{"Title":"t","Body":"b","RecipientType":9}"#,
    )
    .await;
    assert_eq!(r.body, bad("Invalid recipient type."));
    // Online recipients with nobody online.
    let r = post(
        &api,
        "/api/mail/send",
        r#"{"Title":"t","Body":"b","RecipientType":3}"#,
    )
    .await;
    assert_eq!(r.body, bad("Recipients is empty."));
    let r = post(
        &api,
        "/api/mail/send",
        r#"{"Title":"t","Body":"b","RecipientType":2,"Recipients":[5]}"#,
    )
    .await;
    assert_eq!(r.body, bad("Invalid FamilyId: 5 ."));
    let r = post(
        &api,
        "/api/mail/send",
        r#"{"Title":"t","Body":"b","RecipientType":1,"Recipients":[5]}"#,
    )
    .await;
    assert_eq!(r.body, bad("Invalid ExpeditionId: 5 ."));

    // List / delete: plain JSON strings.
    let r = post(&api, "/api/mail/list", "x").await;
    assert_eq!((r.status, r.body.as_str()), (400, "\"Invalid request.\""));
    let r = post(&api, "/api/mail/list", r#"{"CharacterId":0}"#).await;
    assert_eq!(r.body, "\"Unknown character.\"");
    let r = post(&api, "/api/mail/list", r#"{"CharacterId":7}"#).await;
    assert_eq!(r.body, "\"Unknown character.\"");
    let r = post(&api, "/api/mail/delete", r#"{"MailId":0}"#).await;
    assert_eq!(r.body, "\"Unknown mail.\"");
    let r = post(&api, "/api/mail/delete", r#"{"MailId":3}"#).await;
    assert_eq!(r.body, "\"Verification failed.\"");
}

#[tokio::test]
async fn mail_send_to_online_characters_goes_through_the_hook() {
    use std::sync::Mutex;

    use crate::systems::services::webapi::hooks::{WebApiEconomyHooks, WebApiMail};

    #[derive(Default)]
    struct Sent(Mutex<Vec<WebApiMail>>);
    impl WebApiEconomyHooks for Sent {
        fn send_mail(&self, _server: &crate::GameServer, mail: WebApiMail) -> bool {
            self.0.lock().unwrap().push(mail);
            true
        }
    }

    let server = Arc::new(test_server());
    server.set_self();
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
    let api = api(&server);
    let r = post(
        &api,
        "/api/mail/send",
        r#"{"Title":"t","Body":"b","RecipientType":3,"Money":5,"SenderName":" "}"#,
    )
    .await;
    assert_eq!((r.status, r.body.as_str()), (200, r#"{"SendCount":1}"#));
    let mails = sent.0.lock().unwrap();
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].receiver_name, "Bob");
    assert_eq!(mails[0].sender_name, "GM");
    assert_eq!(mails[0].copper_coins, 5);
    assert_eq!(mails[0].mail_type, 18);
}

#[tokio::test]
async fn auction_controller_without_economy() {
    let server = Arc::new(test_server());
    server.set_self();
    let api = api(&server);
    let r = post(&api, "/api/auction/add", r#"{"ItemId":1}"#).await;
    assert_eq!(
        (r.status, r.body.as_str()),
        (400, r#"{"error":"Invalid parameters"}"#)
    );
    let r = post(
        &api,
        "/api/auction/add",
        r#"{"ItemId":1,"Quantity":1,"Price":1,"Duration":0,"ClientId":1,"ClientName":"x"}"#,
    )
    .await;
    assert_eq!(
        r.body,
        r#"{"error":"Internal server error","details":"Item not found!"}"#
    );
    let r = get(&api, "/api/auction/list").await;
    assert_eq!(
        r.body,
        r#"{"error":"Internal server error","details":"AuctionManager is not available"}"#
    );
    // Wrong JSON types outside the try -> 500.
    let r = post(
        &api,
        "/api/auction/add",
        r#"{"ItemId":"1","Quantity":1,"Price":1,"Duration":0,"ClientId":1,"ClientName":"x"}"#,
    )
    .await;
    assert_eq!(r.status, 500);
}
