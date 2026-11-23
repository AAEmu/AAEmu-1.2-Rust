//! Scripted bot scenarios with assertions on the server responses.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use aaemu_game::opcodes::{pp, sc};
use aaemu_game::systems::skills::types::{SkillCastTarget, SkillCaster, SkillObject};
use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};

use crate::game::{read_character, Appearance, GameClient, Packet, UnitInfo, LEVEL_PROXY};
use crate::login::LoginClient;
use crate::record::Recorder;
use crate::stream::StreamClient;

mod busy;
mod cometsboon;
mod craft;
mod doodad;
mod fixes;
mod lunafrost;
mod music;
mod plant;
mod portal;
mod questitem;
mod script;
mod socket;

mod config;
mod mate_revive;
mod npc_return;

pub use self::config::{BotConfig, Kind, ReviveMethod};

use self::mate_revive::{interaction_skill_list, mate_revive, summon_mate};
use self::npc_return::{npc_return, quest_template_id};

#[cfg(test)]
use aaemu_commons::PacketStream;

/// Result of one scenario step.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepResult {
    pub name: String,
    pub ok: bool,
    pub ms: u64,
    pub note: String,
}

/// Result of a scenario run.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Outcome {
    pub steps: Vec<StepResult>,
    /// Values that differ between runs by construction (ids, ports, ...), for the diff
    /// normaliser.
    pub vars: BTreeMap<String, u64>,
    pub error: Option<String>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.error.is_none() && self.steps.iter().all(|s| s.ok)
    }
    pub fn step_ms(&self, name: &str) -> Option<u64> {
        self.steps.iter().find(|s| s.name == name).map(|s| s.ms)
    }
}

struct Runner<'a> {
    rec: &'a Recorder,
    out: Outcome,
}

impl Runner<'_> {
    fn begin(&self, name: &str) -> (String, Instant) {
        self.rec.step(name);
        (name.to_string(), Instant::now())
    }

    fn end(&mut self, (name, t): (String, Instant), note: impl Into<String>) {
        self.out.steps.push(StepResult {
            name,
            ok: true,
            ms: t.elapsed().as_millis() as u64,
            note: note.into(),
        });
    }

    fn var(&mut self, name: &str, v: impl Into<u64>) {
        self.out.vars.insert(name.to_string(), v.into());
    }

    /// Ends a step as failed without aborting the scenario (the caller still cleans up); the
    /// first failure also becomes [`Outcome::error`].
    fn fail(&mut self, (name, t): (String, Instant), note: impl Into<String>) {
        let note = note.into();
        if self.out.error.is_none() {
            self.out.error = Some(format!("[{name}] {note}"));
        }
        self.out.steps.push(StepResult {
            name,
            ok: false,
            ms: t.elapsed().as_millis() as u64,
            note,
        });
    }
}

/// One bot's script, supplied by the `verify` suite runner instead of `$AAEMU_SCRIPT`.
#[derive(Debug, Clone, Copy)]
pub struct ScriptInput<'a> {
    /// The script text (one command per line).
    pub text: &'a str,
    /// Where the text came from, for log / error messages.
    pub label: &'a str,
    /// This bot's suite id (used by `sync`).
    pub bot: &'a str,
    /// Number of bots in the suite (default `sync` participant count).
    pub bots: usize,
    /// Directory shared by the suite's bots for `sync`.
    pub sync_dir: Option<&'a Path>,
}

/// Runs `kind` for one bot. Failures end the run; the error is in [`Outcome::error`].
pub async fn run(kind: Kind, cfg: &BotConfig, rec: &Recorder) -> Outcome {
    finish(kind, cfg, rec, None).await
}

/// Runs one bot of a `verify` suite: the usual login / select / spawn, then `script`.
pub async fn run_script(cfg: &BotConfig, rec: &Recorder, script: &ScriptInput<'_>) -> Outcome {
    finish(Kind::Script, cfg, rec, Some(script)).await
}

async fn finish(
    kind: Kind,
    cfg: &BotConfig,
    rec: &Recorder,
    script: Option<&ScriptInput<'_>>,
) -> Outcome {
    let mut r = Runner {
        rec,
        out: Outcome::default(),
    };
    if let Err(e) = run_inner(kind, cfg, &mut r, script).await {
        let step = rec.current_step();
        r.out.steps.push(StepResult {
            name: step.clone(),
            ok: false,
            ms: 0,
            note: format!("{e:#}"),
        });
        r.out.error = Some(format!("[{step}] {e:#}"));
    }
    r.out
}

const QUIET: Duration = Duration::from_millis(1500);

fn character_to_select<'a>(
    characters: &'a [crate::game::CharInfo],
    requested: &str,
    created: bool,
) -> anyhow::Result<&'a crate::game::CharInfo> {
    if created {
        characters
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(requested))
    } else {
        characters.first()
    }
    .with_context(|| format!("no character returned for {requested} (created={created})"))
}

/// Existing login/lobby path shared by scenarios and the stdio controller.
async fn open_lobby(
    kind: Kind,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
) -> anyhow::Result<Option<(GameClient, StreamClient, LoginClient)>> {
    // ---- login server ----
    let s = r.begin("login.auth");
    let mut login = LoginClient::connect(&cfg.login, r.rec.clone()).await?;
    let account_id = login
        .auth_trion_with_otp(&cfg.user, &cfg.password, cfg.otp.as_deref())
        .await?;
    ensure!(account_id != 0, "account id 0");
    r.var("account_id", account_id);
    r.end(s, format!("account {account_id}"));

    let s = r.begin("login.worlds");
    let worlds = login.list_worlds().await?;
    let world = worlds
        .worlds
        .iter()
        .find(|w| w.id == cfg.game_server_id)
        .with_context(|| format!("world {} not in ACWorldList", cfg.game_server_id))?;
    ensure!(world.active, "world {} is not active", world.name);
    r.end(
        s,
        format!(
            "{} world(s), {} character(s)",
            worlds.worlds.len(),
            worlds.characters.len()
        ),
    );

    let s = r.begin("login.enter");
    let cookie = login.enter_world(cfg.game_server_id).await?;
    r.var("cookie", cookie.cookie);
    r.var("game_port", cookie.port);
    let queue_note = login.queue_updates.last().map_or_else(String::new, |q| {
        format!(
            ", {} queue update(s), last position {}/{}",
            login.queue_updates.len(),
            q.my_turn,
            q.normal_length
        )
    });
    r.end(
        s,
        format!(
            "{}:{} cookie {}{queue_note}",
            cookie.ip, cookie.port, cookie.cookie
        ),
    );
    if kind == Kind::Login {
        return Ok(None);
    }

    // ---- game server: lobby ----
    let s = r.begin("game.enter");
    let addr = format!("{}:{}", cookie.ip, cookie.port);
    let mut game = GameClient::connect(&addr, r.rec.clone()).await?;
    game.enter_world(account_id, cookie.cookie).await?;
    game.wait_sc(sc::X2EnterWorldResponsePacket).await?;
    game.wait_for("ChangeStatePacket(0)", |p| {
        p.is(LEVEL_PROXY, pp::ChangeStatePacket)
    })
    .await?;
    ensure!(
        game.state.last_change_state == Some(0),
        "ChangeState {:?}",
        game.state.last_change_state
    );
    r.var("token", game.state.token);
    r.var("stream_port", game.state.stream_port);
    r.end(s, format!("token {}", game.state.token));

    let s = r.begin("game.finish_state");
    for state in 0..=7 {
        game.finish_state(state).await?;
        if state < 7 {
            game.wait_for("ChangeStatePacket", |p| {
                p.is(LEVEL_PROXY, pp::ChangeStatePacket)
            })
            .await?;
            ensure!(
                game.state.last_change_state == Some(state + 1),
                "FinishState({state}) answered with ChangeState {:?}",
                game.state.last_change_state
            );
        } else {
            game.wait_sc(sc::SCUpdatePremiumPointPacket).await?;
        }
    }
    r.end(s, "states 0..7");

    let s = r.begin("stream.join");
    let stream_addr = format!("{}:{}", cookie.ip, game.state.stream_port);
    let mut stream = StreamClient::connect(&stream_addr, r.rec.clone()).await?;
    let resp = stream.join(account_id, game.state.token).await?;
    ensure!(resp == 0, "TCJoinResponse {resp}");
    r.end(s, "joined");

    let s = r.begin("game.list");
    game.list_characters().await?;
    game.wait_for("last SCCharacterListPacket", |p| {
        p.is_sc(sc::SCCharacterListPacket) && p.body.first() == Some(&1)
    })
    .await?;
    ensure!(
        game.state.char_list_done,
        "incomplete character-list metadata"
    );
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await?;
    r.end(s, format!("{} character(s)", game.state.characters.len()));

    Ok(Some((game, stream, login)))
}

async fn select_and_spawn(
    game: &mut GameClient,
    character_id: u32,
    r: &mut Runner<'_>,
) -> anyhow::Result<()> {
    // ---- world ----
    let s = r.begin("game.select");
    game.select_character(character_id).await?;
    game.wait_sc(sc::SCCharacterStatePacket).await?;
    game.drain(QUIET, Duration::from_secs(10)).await?;
    r.end(s, format!("{} bag items seen", game.state.bag_items.len()));

    let s = r.begin("game.spawn");
    game.spawn_character().await?;
    game.wait_for("own SCUnitStatePacket", |p| p.is_sc(sc::SCUnitStatePacket))
        .await?;
    game.notify_in_game().await?;
    game.notify_in_game_completed().await?;
    game.drain(QUIET, Duration::from_secs(15)).await?;
    ensure!(game.state.self_obj != 0, "own unit state not received");
    r.var("self_obj", game.state.self_obj);
    let npcs = game
        .state
        .units
        .values()
        .filter(|u| u.unit_type == 1)
        .count();
    r.end(
        s,
        format!(
            "obj {} at {:?}, {npcs} npcs visible",
            game.state.self_obj, game.state.self_pos
        ),
    );

    Ok(())
}

/// A control session owns its connections and reuses the scenario login handoff.
pub struct ControlSession {
    pub game: GameClient,
    pub stream: StreamClient,
    // Preserve the ordinary scenario's login-connection lifetime through the handoff.
    _login: LoginClient,
}

impl ControlSession {
    /// Own the three connections of an already established session.
    pub fn new(game: GameClient, stream: StreamClient, login: LoginClient) -> Self {
        Self {
            game,
            stream,
            _login: login,
        }
    }

    /// Release this session's stream, login, and both game halves without application logout.
    pub async fn disconnect_hard(self) {
        let Self {
            game,
            stream,
            _login: login,
        } = self;
        drop(stream);
        drop(login);
        game.disconnect_hard().await;
    }
}

fn control_character(
    characters: &[crate::game::CharInfo],
    requested: &str,
    character_id: Option<u32>,
) -> anyhow::Result<u32> {
    let mut matches = characters.iter().filter(|c| match character_id {
        Some(id) => id != 0 && c.id == id,
        None => c.name.eq_ignore_ascii_case(requested),
    });
    let ch = matches
        .next()
        .context("requested existing character not listed")?;
    ensure!(matches.next().is_none(), "ambiguous existing character");
    Ok(ch.id)
}

/// Connect once, selecting an existing character only. No creation or retries.
pub async fn connect_control(
    cfg: &BotConfig,
    rec: &Recorder,
    character_id: Option<u32>,
) -> anyhow::Result<ControlSession> {
    ensure!(
        !cfg.create_new_character,
        "control cannot create characters"
    );
    rec.control_game_ready(false);
    let mut r = Runner {
        rec,
        out: Outcome::default(),
    };
    let (mut game, stream, login) = open_lobby(Kind::Full, cfg, &mut r)
        .await?
        .context("game lobby was not opened")?;
    let id = control_character(&game.state.characters, &cfg.character, character_id)?;
    rec.control_game_ready(true);
    select_and_spawn(&mut game, id, &mut r).await?;
    Ok(ControlSession::new(game, stream, login))
}

/// Use the scenario's GM chat path without printing command text or replies.
pub async fn control_gm(game: &mut GameClient, command: &str) -> anyhow::Result<()> {
    questitem::gm_command(game, command).await?;
    Ok(())
}

/// The script `tp` sequence, using the position returned by the server.
pub async fn control_tp(
    game: &mut GameClient,
    x: f32,
    y: f32,
    z: f32,
    clock: u32,
    until: tokio::time::Instant,
) -> anyhow::Result<()> {
    tokio::time::timeout_at(until, async {
        game.state.last_teleport = None;
        game.chat(0, &format!("/move {x} {y} {z}")).await?;
        game.wait_sc(sc::SCTeleportUnitPacket).await?;
        let at = game
            .state
            .last_teleport
            .context("teleport position missing")?;
        game.teleport_ended(at).await?;
        game.move_to(at, clock).await?;
        // The completion boundary is the received teleport and the two sent acknowledgments.
        // Ambient traffic is not an action acknowledgment and need never become quiet.
        // Pump any immediate replies for a small slice of the *remaining* action budget.
        let remaining = until.saturating_duration_since(tokio::time::Instant::now());
        let max = remaining
            .saturating_sub(Duration::from_millis(20))
            .min(Duration::from_millis(100));
        if !max.is_zero() {
            game.drain(Duration::from_millis(10), max).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("teleport command deadline")?
}

async fn run_inner(
    kind: Kind,
    cfg: &BotConfig,
    r: &mut Runner<'_>,
    script_input: Option<&ScriptInput<'_>>,
) -> anyhow::Result<()> {
    let Some((mut game, stream, login)) = open_lobby(kind, cfg, r).await? else {
        return Ok(());
    };

    if kind == Kind::Reconnect {
        let s = r.begin("login.reconnect");
        let account_id = login.account_id;
        // At the server list, the client asks the game server for a reconnect token.
        game.leave_world(2).await?;
        let packet = game.wait_sc(sc::SCReconnectAuthPacket).await?;
        let token = packet.stream().read_u32()?;
        ensure!(token != 0, "empty reconnect token");
        // The client then presents it to the login server on a fresh connection.
        let mut reconnect_login = LoginClient::connect(&cfg.login, r.rec.clone()).await?;
        if let Some(forged) = cfg.reconnect_cookie {
            match reconnect_login
                .auth_reconnect(account_id, cfg.game_server_id, forged)
                .await
            {
                Ok(_) => bail!("forged reconnect cookie {forged} was accepted"),
                Err(_) => {
                    r.end(s, format!("forged cookie {forged} refused"));
                    return Ok(());
                }
            }
        }
        let reconnected = reconnect_login
            .auth_reconnect(account_id, cfg.game_server_id, token)
            .await?;
        ensure!(
            reconnected == account_id,
            "reconnect authenticated account {reconnected}, expected {account_id}"
        );
        r.var("reconnect_token", u64::from(token));
        r.end(s, format!("token {token:#010x}, account {reconnected}"));
        return Ok(());
    }

    let s = r.begin("game.create");
    let created = if cfg.create_new_character || game.state.characters.is_empty() {
        game.create_character(&cfg.character).await?;
        let p = game
            .wait_for("character creation result", |p| {
                p.is_sc(sc::SCCreateCharacterResponsePacket)
                    || p.is_sc(sc::SCCharacterCreationFailedPacket)
            })
            .await?;
        if p.is_sc(sc::SCCharacterCreationFailedPacket) {
            bail!("SCCharacterCreationFailed reason {:?}", p.body.first());
        }
        true
    } else {
        false
    };
    let ch = character_to_select(&game.state.characters, &cfg.character, created)?.clone();
    r.var("char_id", ch.id);
    r.end(
        s,
        if created {
            format!("created {} ({})", ch.name, ch.id)
        } else {
            format!("existing {} ({})", ch.name, ch.id)
        },
    );

    if kind == Kind::Edit {
        let s = r.begin("game.edit");
        let before = game
            .state
            .characters
            .iter()
            .find(|c| c.id == ch.id)
            .map(|c| c.appearance)
            .context("created character missing from the lobby list")?;
        ensure!(
            before.hair_color_id == 0,
            "unexpected starting appearance {before:?}"
        );
        let wanted = Appearance {
            kind: before.kind,
            hair_color_id: 7,
            skin_color_id: 3,
            model_id: before.model_id,
        };
        game.edit_character(ch.id, &ch.name, wanted).await?;
        let packet = game
            .wait_for("SCEditCharacterResponsePacket", |p| {
                p.is_sc(sc::SCEditCharacterResponsePacket)
            })
            .await?;
        let mut body = packet.stream();
        let edited = read_character(&mut body)?;
        ensure!(body.remaining() == 0, "trailing edit response bytes");
        ensure!(edited.id == ch.id, "edited id {} != {}", edited.id, ch.id);
        ensure!(
            edited.name == ch.name,
            "edit changed the name to {}",
            edited.name
        );
        ensure!(
            edited.appearance == wanted,
            "edit response appearance {:?} != {wanted:?}",
            edited.appearance
        );
        r.var("hair", edited.appearance.hair_color_id);
        r.end(
            s,
            format!(
                "hair {} skin {}",
                edited.appearance.hair_color_id, edited.appearance.skin_color_id
            ),
        );

        // A relist must carry the edit (the lobby copy the server lists was updated).
        let s = r.begin("game.relist");
        game.state.characters.clear();
        game.state.char_list_done = false;
        game.list_characters().await?;
        game.wait_for("last SCCharacterListPacket", |p| {
            p.is_sc(sc::SCCharacterListPacket) && p.body.first() == Some(&1)
        })
        .await?;
        let relisted = game
            .state
            .characters
            .iter()
            .find(|c| c.id == ch.id)
            .context("edited character missing from the relisted lobby")?;
        ensure!(
            relisted.appearance == wanted,
            "relisted appearance {:?} != {wanted:?}",
            relisted.appearance
        );
        r.end(s, format!("hair {}", relisted.appearance.hair_color_id));
        drop(stream);

        let s = r.begin("game.quit");
        game.leave_world(0).await?;
        game.drain(Duration::from_millis(500), Duration::from_secs(2))
            .await?;
        r.end(s, "");
        return Ok(());
    }

    if kind == Kind::Lobby {
        let s = r.begin("game.quit");
        game.leave_world(0).await?;
        game.drain(Duration::from_millis(500), Duration::from_secs(2))
            .await?;
        r.end(s, "");
        return Ok(());
    }

    select_and_spawn(&mut game, ch.id, r).await?;

    if kind == Kind::NpcReturn {
        npc_return(&mut game, cfg, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::MateRevive {
        mate_revive(&mut game, cfg, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::QuestItem {
        questitem::run(&mut game, cfg, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::Doodad {
        doodad::run(&mut game, cfg, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::Plant {
        plant::run(&mut game, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::CometsBoon {
        cometsboon::run(&mut game, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::StoneThrow {
        fixes::stonethrow(&mut game, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::Ghost {
        fixes::ghost(&mut game, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::Lunafrost {
        lunafrost::run(&mut game, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::Portal {
        portal::run(&mut game, cfg, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::Socket {
        socket::run(&mut game, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::Busy {
        busy::run(&mut game, cfg, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::Craft {
        craft::run(&mut game, r).await?;
        drop(stream);
        return Ok(());
    }
    if kind == Kind::Script {
        match script_input {
            Some(input) => script::run(&mut game, cfg, r, input).await?,
            None => {
                let path =
                    std::env::var("AAEMU_SCRIPT").context("set AAEMU_SCRIPT to the script file")?;
                let text =
                    std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
                let input = ScriptInput {
                    text: &text,
                    label: &path,
                    bot: &cfg.user,
                    bots: 1,
                    sync_dir: None,
                };
                script::run(&mut game, cfg, r, &input).await?;
            }
        }
        drop(stream);
        return Ok(());
    }

    let s = r.begin("game.move");
    let p0 = game.state.self_pos;
    for i in 1..=3u32 {
        let pos = (p0.0 + i as f32 * 0.5, p0.1 + i as f32 * 0.5, p0.2);
        game.move_to(pos, i * 100).await?;
    }
    game.drain(Duration::from_millis(500), Duration::from_secs(3))
        .await?;
    ensure!(!game.state.disconnected, "disconnected after moving");
    r.end(s, format!("moved to {:?}", game.state.self_pos));

    let s = r.begin("game.chat");
    game.chat(0, "hello from aaemu-testclient").await?;
    game.wait_sc(sc::SCChatMessagePacket).await?;
    r.end(s, "say echoed");

    if matches!(kind, Kind::Full | Kind::Combat) {
        let s = r.begin("game.skill");
        // NPCs of newly active regions appear within a few seconds.
        if game.stable_unit(1, 40.0).is_none() {
            let _ = game
                .wait_for("an NPC in range", |p| p.is_sc(sc::SCUnitStatePacket))
                .await;
            game.drain(Duration::from_millis(500), Duration::from_secs(3))
                .await?;
        }
        let npc = game.stable_unit(1, 40.0);
        let note = match npc {
            Some(npc) => {
                r.var("npc_obj", npc.obj_id);
                // Walk up to the target so range checks pass.
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), 2000)
                    .await?;
                game.drain(Duration::from_millis(300), Duration::from_secs(1))
                    .await?;
                game.start_skill(cfg.skill_id, npc.obj_id).await?;
                let caster = game.state.self_obj;
                game.wait_for("skill response", |p| {
                    own_skill_response(p, cfg.skill_id, caster) || p.is_sc(sc::SCErrorMsgPacket)
                })
                .await?;
                game.drain(QUIET, Duration::from_secs(5)).await?;
                format!("skill {} on npc {} ({})", cfg.skill_id, npc.obj_id, npc.id)
            }
            None => "no npc in range".into(),
        };
        r.end(s, note);

        let s = r.begin("game.inventory");
        let note = match game.state.bag_items.first().copied() {
            Some((item, slot, tpl)) => {
                r.var("item_id", item);
                game.swap_items(item, slot, 0, 49).await?;
                game.wait_for("item task result", |p| {
                    p.is_sc(sc::SCItemTaskSuccessPacket) || p.is_sc(sc::SCErrorMsgPacket)
                })
                .await?;
                game.drain(Duration::from_millis(500), Duration::from_secs(3))
                    .await?;
                format!("moved item {item} (template {tpl}) from slot {slot} to 49")
            }
            None => "no bag item".into(),
        };
        r.end(s, note);

        let s = r.begin("game.quest");
        let giver = game
            .state
            .units
            .values()
            .filter(|u| u.unit_type == 1 && u.id == cfg.quest_npc_template)
            .min_by(|a, b| a.obj_id.cmp(&b.obj_id))
            .cloned();
        let note = match giver {
            Some(npc) => {
                game.move_to((npc.pos.0 + 1.0, npc.pos.1, npc.pos.2), 3000)
                    .await?;
                game.start_quest(cfg.quest_id, npc.obj_id).await?;
                // No answer at all when the quest cannot be started (e.g. already taken).
                let got = game
                    .drain(Duration::from_millis(800), Duration::from_secs(4))
                    .await?;
                let answer = got
                    .iter()
                    .find(|p| {
                        p.is_sc(sc::SCQuestContextStartedPacket) || p.is_sc(sc::SCErrorMsgPacket)
                    })
                    .map_or("no answer", |p| p.name());
                format!("quest {} from npc {} -> {answer}", cfg.quest_id, npc.id)
            }
            None => {
                game.start_quest(cfg.quest_id, 0).await?;
                game.drain(Duration::from_millis(800), Duration::from_secs(4))
                    .await?;
                format!("quest {} (no giver in sight)", cfg.quest_id)
            }
        };
        r.end(s, note);
    }

    if kind == Kind::Combat && cfg.combat_npc_template != 0 {
        let s = r.begin("game.combat");
        let note = combat(&mut game, cfg).await?;
        r.end(s, note);
    }

    // Back to where we spawned: the position saved on logout (and seen by the next session)
    // then does not depend on where the NPCs wandered.
    game.move_to(p0, 19000).await?;
    let s = r.begin("game.leave");
    game.leave_world(0).await?;
    let prep = game.wait_sc(sc::SCPrepareLeaveWorldPacket).await?;
    let logout_ms = prep.stream().read_i32().unwrap_or(0);
    // In combat the logout timer is 5 minutes: just drop the connection then.
    if cfg.wait_leave_granted && logout_ms <= 10_000 {
        game.wait_sc(sc::SCLeaveWorldGrantedPacket).await?;
    }
    game.drain(Duration::from_millis(300), Duration::from_secs(2))
        .await
        .ok();
    r.end(s, "left");
    drop(stream);
    Ok(())
}

/// `name xN` summary of received packets, for step notes.
fn packet_summary(packets: &[Packet]) -> String {
    if packets.is_empty() {
        return "nothing".into();
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in packets {
        *counts.entry(p.name()).or_default() += 1;
    }
    counts
        .iter()
        .map(|(n, c)| format!("{n} x{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The configured quest giver, once it is visible at the spawn point.
async fn wait_for_giver(
    game: &mut GameClient,
    template_id: u32,
    timeout: Duration,
) -> anyhow::Result<UnitInfo> {
    let deadline = Instant::now() + timeout;
    loop {
        let giver = game
            .state
            .units
            .values()
            .filter(|u| u.unit_type == 1 && u.id == template_id)
            .min_by(|a, b| a.obj_id.cmp(&b.obj_id))
            .cloned();
        if let Some(giver) = giver {
            return Ok(giver);
        }
        if Instant::now() >= deadline {
            bail!("quest giver NPC {template_id} is not visible at the spawn point ({timeout:?})");
        }
        game.drain(Duration::from_millis(500), Duration::from_secs(1))
            .await?;
    }
}

/// Reads packets for `secs`, collecting whatever arrives. `GameClient::recv_within` keeps the
/// connection alive (keep-alive pings) while waiting.
async fn pump(game: &mut GameClient, secs: u64) -> anyhow::Result<Vec<Packet>> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        if let Some(p) = game
            .recv_within(left.min(Duration::from_millis(500)))
            .await?
        {
            seen.push(p);
        }
    }
    Ok(seen)
}

/// First `Bc` of a unit packet body: the object id of `SCUnitStatePacket` /
/// `SCOneUnitMovementPacket`.
fn packet_obj_id(p: &Packet) -> anyhow::Result<u32> {
    Ok(p.stream().read_bc()?)
}

/// Walks up to the closest `combat_npc_template` NPC (following its movement) and hits it with
/// `skill_id` up to three times.
async fn combat(game: &mut GameClient, cfg: &BotConfig) -> anyhow::Result<String> {
    let pick = |game: &GameClient| {
        let me = game.state.self_pos;
        game.state
            .units
            .values()
            .filter(|u| u.unit_type == 1 && u.id == cfg.combat_npc_template)
            .min_by(|a, b| {
                let d =
                    |u: &crate::game::UnitInfo| (u.pos.0 - me.0).powi(2) + (u.pos.1 - me.1).powi(2);
                d(a).total_cmp(&d(b))
            })
            .cloned()
    };
    let target = pick(game)
        .with_context(|| format!("no combat NPC {} in sight", cfg.combat_npc_template))?;
    game.change_target(target.obj_id).await?;
    let mut started = 0;
    let mut results = Vec::new();
    for attempt in 0..3u32 {
        let pos = game
            .state
            .units
            .get(&target.obj_id)
            .map_or(target.pos, |u| u.pos);
        game.move_to_facing((pos.0 + 0.8, pos.1, pos.2), pos, 10000 + attempt * 1000)
            .await?;
        game.drain(Duration::from_millis(300), Duration::from_secs(1))
            .await?;
        game.start_skill(cfg.skill_id, target.obj_id).await?;
        // NPC attacks also broadcast these opcodes. Wait for our own cast.
        let caster = game.state.self_obj;
        let p = game
            .wait_for("skill response", |p| {
                own_skill_response(p, cfg.skill_id, caster) || p.is_sc(sc::SCErrorMsgPacket)
            })
            .await?;
        let result = skill_response_result(&p)?;
        ensure!(
            result == 0,
            "combat skill {} rejected with SkillResult {result}",
            cfg.skill_id
        );
        results.push(result);
        started += 1;
        game.drain(Duration::from_millis(800), Duration::from_secs(3))
            .await?;
    }
    Ok(format!(
        "{started} player casts of skill {} on npc {} ({}), SkillResults {results:?}",
        cfg.skill_id, target.obj_id, target.id
    ))
}

fn own_skill_response(packet: &Packet, skill_id: u32, caster: u32) -> bool {
    if !packet.is_sc(sc::SCSkillStartedPacket) && !packet.is_sc(sc::SCSkillFiredPacket) {
        return false;
    }
    let mut stream = packet.stream();
    stream.read_u32().ok() == Some(skill_id)
        && stream.read_u16().is_ok()
        && stream.read_u8().is_ok()
        && stream.read_bc().ok() == Some(caster)
}

fn skill_response_result(packet: &Packet) -> anyhow::Result<u8> {
    if packet.is_sc(sc::SCSkillFiredPacket) {
        return Ok(0);
    }
    ensure!(
        packet.is_sc(sc::SCSkillStartedPacket),
        "server reported an error instead of a skill response"
    );
    let mut stream = packet.stream();
    stream.read_u32()?;
    stream.read_u16()?;
    let caster_kind = stream.read_u8()?;
    SkillCaster::read(caster_kind, &mut stream)?;
    let target_kind = stream.read_u8()?;
    SkillCastTarget::read(target_kind, &mut stream)?;
    let object_kind = stream.read_u8()?;
    SkillObject::read(object_kind, &mut stream)?;
    stream.read_u16()?;
    stream.read_u16()?;
    stream.read_u8()?;
    let flags = stream.read_u8()?;
    Ok(if flags & 1 != 0 { stream.read_u8()? } else { 0 })
}

#[cfg(test)]
mod control_character_tests;
#[cfg(test)]
mod control_session_tests;
#[cfg(test)]
mod creation_tests;
#[cfg(test)]
mod mate_revive_tests;
#[cfg(test)]
mod npc_return_tests;
#[cfg(test)]
mod skill_response_tests;
