use anyhow::bail;
use serde::{Deserialize, Serialize};

/// Which scenario to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Login server only: auth, world list, world cookie.
    Login,
    /// Login + game lobby: enter world, FinishState burst, character list, stream join,
    /// character creation, then quit.
    Lobby,
    /// Lobby character edit (`CSEditCharacter`, pre-select period): create, edit the appearance,
    /// check `SCEditCharacterResponse`, relist and check the changed entry, then quit.
    Edit,
    /// Everything: lobby, select + spawn, move, chat, skill on an NPC, inventory move, quest
    /// accept, leave world.
    Full,
    /// The load-test loop: login, (create), select, move, chat, leave.
    Load,
    /// `full` plus a fight with a hostile NPC (random outcome: diff with `parity-combat.json`).
    Combat,
    /// NPC residency: accept the configured quest at its resident giver, leave beyond the warm
    /// horizon for longer than the warm-retention window, then return and require the same
    /// giver instance and an active quest.
    #[serde(rename = "npc-return")]
    NpcReturn,
    /// Stablehand dead-mount flow: summon a mount, kill it with a GM command, dismiss the
    /// corpse, ask a stablehand NPC to revive it, re-summon and require a live mate. Needs an
    /// account with GM access (`/item add`, `/kill`).
    #[serde(rename = "mate-revive")]
    MateRevive,
    /// Quest-item use: GM setup (quest, orbs, teleport), then the quest item's use skill on
    /// nearby NPCs until the quest objective is counted (see `scenario/questitem.rs`).
    #[serde(rename = "questitem")]
    QuestItem,
    /// Doodad interaction: GM setup (quest 3447, teleport next to the Amber Boulder), then the
    /// doodad's fake use skill on it, logging the packets (see `scenario/doodad.rs`).
    #[serde(rename = "doodad")]
    Doodad,
    /// Plant crops / trees and place animals with `CSCreateDoodadPacket` after GM item setup
    /// (see `scenario/plant.rs`). GM account.
    #[serde(rename = "plant")]
    Plant,
    /// Auramancy "Comet's Boon" (skill 18222, plot 363): summon doodad 5017 and apply buff 3771
    /// to the caster (see `scenario/cometsboon.rs`). GM account.
    #[serde(rename = "cometsboon")]
    CometsBoon,
    /// Stone throwing (skill 11268) from a stackable item: the `ItemUnlock` is sent while the
    /// stack survives and skipped for the last stone (see `scenario/fixes.rs`). GM account.
    #[serde(rename = "stonethrow")]
    StoneThrow,
    /// Kill the ghost of spawner 3380, delete its corpse with a GM command, watch the respawn
    /// (see `scenario/fixes.rs`). GM account.
    #[serde(rename = "ghost")]
    Ghost,
    /// Lunafrost enchant (skill 11285, `MAGICAL_ENCHANT`) of a bag and of a worn head armor (see
    /// `scenario/lunafrost.rs`). GM account.
    #[serde(rename = "lunafrost")]
    Lunafrost,
    /// Portal book delete (`SCPortalDeleted`) and the Memory Tome recall-point binding (skill
    /// 14602 cast at doodad 841, see `scenario/portal.rs`). GM account.
    #[serde(rename = "portal")]
    Portal,
    /// Lunagem socketing (skill 23728): a wrong slot group is rejected with error 18 and the gem
    /// survives, a fitting gem is installed and consumed (see `scenario/socket.rs`). GM account.
    #[serde(rename = "socket")]
    Socket,
    /// A craft batch that runs out of labor ends with the `SCErrorMsg` 148 the client needs to
    /// unlock its craft window (Gilda Star disintegration at an Archeum Workbench, see
    /// `scenario/craft.rs`). GM account.
    #[serde(rename = "craft")]
    Craft,
    /// Ad-hoc live checks driven by the command file `$AAEMU_SCRIPT` (see `scenario/script.rs`).
    /// Server-select reconnect round trip (game `SCReconnectAuth` -> login `CARequestReconnect`).
    #[serde(rename = "reconnect")]
    Reconnect,
    /// GM account.
    #[serde(rename = "script")]
    Script,
    /// Capacity test: a fully active player (teleport, mate, boat, spawns, skill casts, moves,
    /// chat) for `busy_secs` (see `scenario/busy.rs`). GM account.
    #[serde(rename = "busy")]
    Busy,
}

impl std::str::FromStr for Kind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "login" => Self::Login,
            "lobby" => Self::Lobby,
            "edit" => Self::Edit,
            "full" => Self::Full,
            "load" => Self::Load,
            "combat" => Self::Combat,
            "npc-return" => Self::NpcReturn,
            "mate-revive" => Self::MateRevive,
            "questitem" => Self::QuestItem,
            "doodad" => Self::Doodad,
            "plant" => Self::Plant,
            "cometsboon" => Self::CometsBoon,
            "stonethrow" => Self::StoneThrow,
            "ghost" => Self::Ghost,
            "lunafrost" => Self::Lunafrost,
            "portal" => Self::Portal,
            "socket" => Self::Socket,
            "craft" => Self::Craft,
            "reconnect" => Self::Reconnect,
            "script" => Self::Script,
            "busy" => Self::Busy,
            _ => bail!(
                "unknown scenario '{s}' (login, lobby, edit, full, combat, npc-return, mate-revive, questitem, doodad, \n                 plant, cometsboon, stonethrow, ghost, lunafrost, portal, socket, craft, script, busy, load)"
            ),
        })
    }
}

/// Bot settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BotConfig {
    /// Login server `host:port`.
    pub login: String,
    pub user: String,
    pub password: String,
    /// OTP for an account with two-factor authentication (`ACEnterOtpPacket`).
    #[serde(default)]
    pub otp: Option<String>,
    /// `reconnect` scenario: present this cookie instead of the real token and expect a denial.
    #[serde(default)]
    pub reconnect_cookie: Option<u32>,
    /// Character name used when the account has none.
    pub character: String,
    /// Require creation of `character`, even if the account already has other characters.
    #[serde(default)]
    pub create_new_character: bool,
    #[serde(default = "default_gs")]
    pub game_server_id: u8,
    /// Skill cast on the nearest NPC (2 = melee attack).
    #[serde(default = "default_skill")]
    pub skill_id: u32,
    /// Quest accepted from an NPC (`CSStartQuestContextPacket`).
    #[serde(default = "default_quest")]
    pub quest_id: u32,
    /// Template of the NPC that gives `quest_id` (must be near the start position).
    #[serde(default = "default_quest_npc")]
    pub quest_npc_template: u32,
    /// Hostile NPC attacked in the combat step (3492: Solzreed fox near the Nuian spawn); 0
    /// skips the step.
    #[serde(default = "default_combat_npc")]
    pub combat_npc_template: u32,
    /// Wait for `SCLeaveWorldGrantedPacket` (10 s logout timer) before disconnecting.
    #[serde(default = "default_true")]
    pub wait_leave_granted: bool,
    /// `npc-return`: distance (m) walked away from the spawn before returning. Must clear the
    /// NPC warm/trigger horizon (default 2048 m).
    #[serde(default = "default_npc_return_distance")]
    pub npc_return_distance: f32,
    /// `npc-return`: seconds to stay away; must exceed `World.NpcWarmRetentionSeconds`
    /// (server default 60 s).
    #[serde(default = "default_npc_return_away_secs")]
    pub npc_return_away_secs: u64,
    /// `mate-revive`: template of the `SummonMate` item added with `/item add` (8159: brown
    /// Lilliot horse, npc 5431).
    #[serde(default = "default_mate_item")]
    pub mate_item_template: u32,
    /// `mate-revive`: use skill of `mate_item_template` (`items.use_skill_id`, 10602 for the
    /// horse whistles).
    #[serde(default = "default_mate_item_skill")]
    pub mate_item_skill_id: u32,
    /// `mate-revive`: stablehand NPC template (1890: Herman, `stabler`, near 12590,15407,156).
    #[serde(default = "default_stablehand_npc")]
    pub stablehand_npc_template: u32,
    /// `mate-revive`: where the stablehand stands (the bot walks there with `CSMoveUnit`
    /// jumps, then waits for the NPC to be announced).
    #[serde(default = "default_stablehand_pos")]
    pub stablehand_pos: (f32, f32, f32),
    /// `mate-revive`: the interaction skill the stablehand must offer (12195: heal pet's
    /// wounds).
    #[serde(default = "default_revive_skill")]
    pub revive_skill_id: u32,
    /// `mate-revive`: how the revive request is sent, see [`ReviveMethod`].
    #[serde(default)]
    pub revive_method: ReviveMethod,
    /// `questitem`: quest started by GM command (2394 "Searching for Answers").
    #[serde(default = "default_quest_item_quest")]
    pub quest_item_quest: u32,
    /// `questitem`: the quest's giver NPC template (511), recorded as the acceptor so the
    /// `QuestActConAcceptNpc` start condition passes and the quest reaches its Progress step.
    #[serde(default = "default_quest_item_giver")]
    pub quest_item_giver: u32,
    /// `questitem`: quest item template (17594 "Detection Orb").
    #[serde(default = "default_quest_item_template")]
    pub quest_item_template: u32,
    /// `questitem`: the item's use skill (16881).
    #[serde(default = "default_quest_item_skill")]
    pub quest_item_skill: u32,
    /// `questitem`: NPC templates the item is used on (7824 / 7823 Field of Honor
    /// participants at Soulreath).
    #[serde(default = "default_quest_item_npcs")]
    pub quest_item_npcs: Vec<u32>,
    /// `questitem`: successful uses required (objective count of the quest act).
    #[serde(default = "default_quest_item_uses")]
    pub quest_item_uses: u32,
    /// `questitem`: GM teleport target next to those NPCs (`/move x y z`).
    #[serde(default = "default_quest_item_pos")]
    pub quest_item_pos: [f32; 3],
    /// `questitem`: casts tried (rejected ones included) before the step fails.
    #[serde(default = "default_quest_item_max_attempts")]
    pub quest_item_max_attempts: u32,
    /// `busy`: seconds of the activity loop.
    #[serde(default = "default_busy_secs")]
    pub busy_secs: u64,
    /// Optional fixed scene center for a local GM load test. Each bot uses a
    /// deterministic offset within 25 m. The default retains the eight-point route.
    #[serde(default)]
    pub busy_center: Option<(f32, f32, f32)>,
    /// Skip rowboat setup for the land-only crowd workload.
    #[serde(default)]
    pub busy_skip_boat: bool,
    /// Character race override for newly created load-test characters.
    #[serde(default)]
    pub character_race: Option<u8>,
    /// `busy`: boat summon item (15817: Rowboat scroll, slave 15) and its use skill (15802).
    #[serde(default = "default_busy_boat_item")]
    pub busy_boat_item: u32,
    #[serde(default = "default_busy_boat_skill")]
    pub busy_boat_skill: u32,
}

/// The client packet that asks a stablehand to revive the dead mate. The real protocol is still
/// being reverse-engineered: the request is isolated in `mate_revive::send_revive_request` so another
/// variant can be added without touching the scenario.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviveMethod {
    /// `CSRepairPetItemsPacket(stablehand obj id)` after `CSInteractNPCPacket`, what the 1.2
    /// client sends.
    #[default]
    Repair,
    /// `CSStartSkillPacket(revive_skill_id)` on the stablehand (unit caster, unit target).
    Skill,
    /// Send nothing (negative control: the mate must stay dead).
    None,
}

impl std::str::FromStr for ReviveMethod {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "repair" => Self::Repair,
            "skill" => Self::Skill,
            "none" => Self::None,
            _ => bail!("unknown revive method '{s}' (repair, skill, none)"),
        })
    }
}

fn default_gs() -> u8 {
    1
}
fn default_skill() -> u32 {
    2
}
fn default_quest() -> u32 {
    330 // Nuian starter quest given by NPC 3597, next to the Nuian spawn point
}
fn default_quest_npc() -> u32 {
    3597
}
fn default_combat_npc() -> u32 {
    3492
}
fn default_true() -> bool {
    true
}
fn default_npc_return_distance() -> f32 {
    2048.0
}
fn default_npc_return_away_secs() -> u64 {
    70
}
fn default_mate_item() -> u32 {
    8159
}
fn default_mate_item_skill() -> u32 {
    10602
}
fn default_stablehand_npc() -> u32 {
    1890
}
fn default_stablehand_pos() -> (f32, f32, f32) {
    (12590.85, 15406.811, 156.48676)
}
fn default_revive_skill() -> u32 {
    12195
}
fn default_quest_item_quest() -> u32 {
    2394
}
fn default_quest_item_giver() -> u32 {
    511
}
fn default_quest_item_template() -> u32 {
    17594
}
fn default_quest_item_skill() -> u32 {
    16881
}
fn default_quest_item_npcs() -> Vec<u32> {
    vec![7824, 7823]
}
fn default_quest_item_uses() -> u32 {
    3
}
fn default_quest_item_pos() -> [f32; 3] {
    [11041.0, 15622.0, 246.0]
}
fn default_busy_secs() -> u64 {
    300
}
fn default_busy_boat_item() -> u32 {
    15817
}
fn default_busy_boat_skill() -> u32 {
    15802
}
fn default_quest_item_max_attempts() -> u32 {
    12
}
