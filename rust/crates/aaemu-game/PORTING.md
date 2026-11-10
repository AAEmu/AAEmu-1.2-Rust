# aaemu-game porting status

Reference: `AAEmu.Game` at upstream `develop` 6e2f40734 (1.2 client, `client_12_r208022`).
Status: **done** = ported for its current C# scope, **partial** = only what the lobby / world
entry / movement path needs, **todo** = not started.

Layout rules, the object model API and testing conventions: `CONTRIBUTING-PORT.md`.

## 1.2 lobby flow (phase 1)

```
login  --LGPlayerEnter(acc, connId)-->  game: EnterWorldManager.AddAccount -> GLPlayerEnter(connId, gsId, 0|1)
client --X2EnterWorld(lvl1, cookie)-->  game: Login(token check) -> AccountManager.Add, StreamManager.AddToken
                                         <-- X2EnterWorldResponse(lvl1) + ChangeState(0) (lvl2)
client --FinishState(0)(lvl2)------->   <-- ChangeState(1), SetGameType("w_hanuimaru_1"), SCInitialConfig,
                                             SCTrionConfig, SCAccountInfo, SCChatSpamDelay,
                                             SCAccountAttributeConfig, SCLevelRestrictionConfig
client --FinishState(1..6)---------->   <-- ChangeState(state+1);  FinishState(7) -> SCUpdatePremiumPoint
client --CSListCharacter(lvl1)------>   <-- SCGetSlotCount, SCAccountInfo, [LoadAccount: Character.Load +
                                             CheckForDeletedCharactersDeletion -> SCCharacterDeleted +
                                             SCDeleteCharacterResponse(1)], SCRaceCongestion,
                                             SCCharacterList x ceil(n/2), SCLoginCharInfoHouse per house
client --CSLeaveWorld(ServerSelect)->   game -> GLPlayerReconnect;  LGPlayerReconnect -> SCReconnectAuth
```

## Character lifecycle and world (phase 2)

```
lobby:
client --CSCreateCharacter------->  NameManager.Validate -> SCCharacterCreationFailed(3|4|5)
                                    | CharacterManager.Create: template, starting kit (equip pack + body
                                    |   parts -> Equipment, supplies -> Bag), action slots, abilities,
                                    |   actabilities, default/start skills, MaxHp/MaxMp,
                                    |   SaveDirectlyToDatabase -> SCCreateCharacterResponse | SCCharacterCreationFailed(3)
client --CSDeleteCharacter------->  SetDeleteCharacter -> SCDeleteCharacterResponse(2|0) | SCErrorMsg; CharacterDeleteTask
client --CSCancelCharacterDelete->  SetRestoreCharacter -> SCCancelCharacterDeleteResponse(3|4)
client --CSEditCharacter--------->  (Characters:PreSelectPeriod on) CharacterManager.Edit: own, undeleted,
                                    never-played character -> create rules re-run, rebuilt starting kit,
                                    SCEditCharacterResponse | ignored (failure reply capture-pending)

enter world:
client --CSSelectCharacter------->  Character.Load() (abilities, actabilities, skills, appellations, portals,
                                    friends, blocked, quests, slots), ObjId (UsedCharacterObjIds), WorldManager.TryAddCharacter
                                    <-- SCCharacterState, SCCharacterGamePoints, SCCharacterInvenInit,
                                        SCCharacterInvenContents x (bag/10 + bank/10), SCActionSlots, SCQuests,
                                        SCCompletedQuests, SCActability, SCCountUnreadMail, [SCAppellations],
                                        [SCCharacterPortals], [SCCharacterReturnDistricts], [SCFriends], [SCBlockedUsers],
                                        SCConflictZoneState per conflict, SCFactionList x n (or the 0x007 empty one),
                                        SCFactionRelationList x n (or the 0x008 empty one), SCFactionList (guilds),
                                        SCExpeditionRolePolicyList([]), [guild policies + members],
                                        SCResponseUIData(1/2/5)
                                    + LoggedOn / template / saved buffs, CheckWantedThreshold, RestoreSavedHpMp,
                                      OnZoneChange(0, zone) (-> SCExpeditionMemberStatusChanged to the guild)
client --CSSpawnCharacter-------->  State = World, visual options <-- SCUnitState(self), SCDetailedTimeOfDay
                                    (TimeOfDayObserver: SCTimeOfDay every 10 s)
client --CSNotifyInGame---------->  IsOnline (SCFriendStatusChanged to friends), Character.Spawn:
                                    region add -> others: SCUnitState + SCUnitPoints both ways (2-region neighbourhood)
                                    <-- SCJoinedChatChannel (zone group, nation, faction; guild)
client --CSNotifyInGameCompleted->  <-- SCOnOffSnow

in world:
client --CSMoveUnit(UnitMoveType)-> position update -> SCOneUnitMovement to neighbours + self (first move: MOTD),
                                    region change -> SCUnitsRemoved / SCUnitState+SCUnitPoints for the diff regions,
                                    zone change -> zone chat leave/join
client --CSSendChatMessage-------> say: SCChatMessage to neighbours + self; shout/trade/lfg: zone channel;
                                    whisper / clan / family / nation / faction channels; party/raid -> SCErrorMsg
client --CSLeaveWorld(0|1)-------> <-- [system logout message], SCPrepareLeaveWorld(10000);  CSCancelLeaveWorld ->
                                    SCLeaveWorldCanceled;  after 10 s LeaveWorldTask: chat leave, guild status,
                                    Delete (SCUnitsRemoved to neighbours), save <-- SCLeaveWorldGranted, ChangeState(0)
disconnect                    ---> OnDisconnect: offline status, SaveAndRemoveFromWorld

server: SaveManager every World.AutoSaveInterval minutes (items + online characters), final save on shutdown.
```

### Lobby character edit (`CSEditCharacter` 0x023 / `SCEditCharacterResponse` 0x03a)

The pre-select period is a login-advertised flag (`ACJoinResponse.afs` bit 16,
`Login:PreSelectCharacterPeriod`); the client's Start button then calls
`X2LoginCharacter.EditCharacter` instead of entering the world (`character_select.alb`), and its
edit window reuses the character-create UI. The game server takes the same flag from
`Characters:PreSelectPeriod` and, at registration, from the Rust-only tail of
`LGRegisterGameServer` (`[u8 result][u8 preSelectCharacterPeriod]`): the login value is applied
(the local value is only used when the login does not send the byte, e.g. an older login), so
the two servers cannot disagree. While the period is on, `CSSelectCharacter` is refused as well:
the client does not offer world entry and a forged select must not bypass the period. Request
layout from the 1.2 serializer `FUN_397c4da0` (writer `FUN_397ae360`): `u32 characterId`, then
the `CSCreateCharacterPacket` body (name, race, gender, 7 body items, the
`UnitCustomModelParams` blob, 3 abilities, level) **without** the create's trailing
`i32 introZoneId`. Response: the character-info block shared with the create response and the
character list (serializer `FUN_397ab170` -> `FUN_39620030`) under opcode 0x03a.

Rules (from the 1.2 UI and the create path; the 1.2 server behaviour itself is not observable):

* Only the account's own, undeleted, not-pending-deletion character in the connection's lobby
  list, and only while it never entered the world: the edit re-runs creation, so an already
  played character would lose its kit, skills and slots. The decision uses the persisted row
  (`online_time`, `leave_time`, `delete_request_time`, `delete_time`, `deleted`) re-read under
  the save transaction lock, not the lobby copy; the edit is also refused when the session is
  not in the lobby (`state`, `active_char`), while a select/enter holds the character
  (`EnterWorldManager`) and while a departure save is pending (`SaveManager`).
* Race and gender are fixed in the edit UI (`GetEditUnitRace/Gender` seed the read-only step-1
  buttons and only the character's own race stays enabled, `character_create_step1_view.alb`),
  so a different pair is refused. The stored level is kept (the client sends 1 for both create
  and edit; its edit UI has no level control).
* Name changes are allowed: the edit UI pre-fills the existing name in the editable field and the
  1.2 edit builder re-validates it (`XlIsValidName`, `FUN_391a4640`). An unchanged name is left
  alone; a changed one re-runs create's rules (`NameManager.ValidateCharacterName` + blocked
  names) under the create save lock, and the name manager swap happens after the commit.
* The rebuild reuses every create step (race/gender template, ability starting kit, body items,
  supplies, actabilities, starting skills, gear buffs). Only the containers creation fills are
  rebuilt (equipment, bag, bank; `mate_id = 0`): their old item/container rows and the
  create-time ability/actability/appellation/skill rows are deleted and the new ones written in
  one transaction, and the replaced item ids are freed after the commit. A failed save leaves
  the character untouched. Mail/auction/trade/system containers and mate/slave equipment keep
  their rows and items: a never-played character can already own mail attachments delivered
  while it was offline (`systems/economy/mail.rs` creates the MAIL container by character id).
* **Failure reply: UNKNOWN / capture-pending** (`EDITCHAR-FAIL-1`). The 1.2 handler
  `FUN_391eb910` only has a success path (a zero character id is its only no-op), so a refused
  edit sends nothing and logs the reason; the client leaves its create window open.
* Not ported / not validated (**known, not fixed**): create itself accepts any item template as
  a body item (`SetEquipItemTemplate`) without checking `item_body_parts`; the edit inherits
  that rule rather than tightening it (`EDITCHAR-BODYITEM-1`, capture-pending). A forged packet
  can also persist arbitrary `ability2`/`ability3` bytes (create only logs the anomaly; the
  client sends 2 and 3, and only `ability1` selects the starting tree).

Framing: level 1 = `[dd 01][hash][count][u16 op][body]` (server writes 0/0 and never verifies the
client's bytes, as in C#), level 2 = `[dd 02][u16 op][body]`. Any other level has no packet table:
the C# lookup throws and the connection is shut down. There is no stream encryption in 1.2.

Sending: `ConnectionHandle::send` encodes and queues on the connection's writer immediately, so
packets sent to a player from another connection's handler keep the C# call order relative to the
packets its own handler sends.

## Object model (phase 3a)

All world objects share one model (details in `CONTRIBUTING-PORT.md` §2):

```
WorldObject = Player(Arc<Player>) | Npc | Doodad | Slave | Mate | House | Transfer | Shipyard | Gimmick
              (Entity<D> = obj_id + ObjectCore{region, IsVisible, children} + Mutex<D>)
D: Character | Npc | Doodad | Slave | Mate | House | Transfer | Shipyard | Gimmick
   every unit kind embeds UnitBase (GameObject + BaseUnit + Unit members) and implements UnitData
WorldInstance: objects (by obj id) + characters + region grid; spawn / show / hide / delete /
   add_visible_object / remove_visible_object for every kind; around / objects_in_cell;
   attach_child / world_pos_rot (parent transforms)
systems::{skills, npcs, economy, social, quests}: Data (tables), System (managers), register
   (packets), start, save stages; systems::Hooks (UnitHooks: buff events, unit zone change)
```

## Subsystems

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `Core/Network/Game` (GameNetwork, GameProtocolHandler, GamePacket) | `net/` | done | Levels 1/2 framing, registry keyed by (level, opcode), `0xFFF` packets dropped on send. |
| `Core/Network/Connections` (GameConnection, GameConnectionTable, GameState) | `net/connection.rs`, `world/session.rs` | partial | ActiveChar, LeaveTask (+ cancel), OnDisconnect / SaveAndRemoveFromWorld. Houses, session attributes, connection subscribers todo. |
| `Core/Network/Login` + `Packets/G2L`, `Packets/L2G` | `login/` | done | All 5 G2L and 4 L2G packets (byte-identical), LoadTask, reconnect loop. |
| `Core/Network/Stream` + `Packets/C2S`, `Packets/S2C` (stream server) | `stream/` | done | StreamNetwork listener on `StreamNetwork.Host:Port`, `[u16 len][u16 op][body]` framing, connection table, all 17 CT and 15 TC packets (byte-exact). World lookups go through `stream::world::StreamWorld`, implemented by `stream_world::GameStreamWorld` (names, items/UCC ids, ActiveChar, doodads of a cell via `WorldInstance::objects_in_cell`, and a live doodad by object id via `WorldInstance::doodad` for the crest printer check). Truncated packet bodies close the connection (C# reads zeros). |
| `Core/Packets/C2G` | `packets/c2g.rs`, `packets/c2g_world.rs` | partial | X2EnterWorld, CSLeaveWorld, CSCancelLeaveWorld, CSListCharacter, CSCreateCharacter, CSDeleteCharacter, CSCancelCharacterDelete, CSSelectCharacter, CSSpawnCharacter, CSNotifyInGame, CSNotifyInGameCompleted, CSMoveUnit, CSSendChatMessage. Others todo; full opcode table in `opcodes.rs`. |
| `Core/Packets/G2C` | `packets/g2c.rs`, `packets/world.rs` | partial | Lobby packets + every packet sent by the phase-2 paths (≈45, byte-exact tests). `SCLoginCharInfoHouse`, `SCMyHouse`, item tasks, buff packets todo. |
| `Core/Packets/Proxy` | `packets/proxy.rs` | done | All registered proxy packets (most are inert in C# too). |
| `Core/Packets/CompressedGamePackets`, `Debug` | – | todo | |
| `Core/Managers/World/EnterWorldManager` | `managers/enter_world.rs`, `world/session.rs` | done | AddAccount, Login, Leave (all targets), LeaveWorldTask (mates/slaves/coffers/quest timers are no-ops: not ported). |
| `Core/Managers/AccountManager` | `managers/account.rs`, `db.rs`, `systems/services/accounts.rs` | done | Add (login time + timed rewards), Remove/Contains/Count, GetAccountDetails, UpdateLoginTime, AddCredits/RemoveCredits/AddLoyalty/UpdateLabor/UpdateTickTimes/UpdateDivineClock/GetDivineClock under the per-account lock, RemoveDeadConnections (30 s tick). See `systems::services`. |
| `Core/Managers/FeaturesManager`, `Models/Game/Features` | `data/features.rs` | done | |
| `content_configs` (client `LoadContentConfigValueArray`) | `data/content_configs.rs` | done | All 53 rows by **id** (the client's key) into `GameData.content_configs`; `content_config(id) -> Option<i64>`, the point lookup `load_content_config` stays for the loaders that read one id (re-exported from `systems::skills::data::misc`). Ids 37 (1,000,000 copper per tax certificate) / 57 (28,085 appraisal certificate) now drive the housing sale and tax helpers, with the old constants as fallbacks; ids 38 (1,000) and 39 (200) are loaded but stay UNKNOWN (RE-2, no behaviour). |
| `Core/Managers/ExperienceManager` (+ `SqliteExperienceLevelTemplateLoader`) | `data/experience.rs` | partial | Loader validation, level caps, GetExpForLevel, GetLevelFromExp (both overloads), GetSkillPointsForLevel. Mate/exp-loss helpers todo. |
| `Core/Managers/Id/*` (`IdManager`, `BitSet`, `PrimeFinder`) | `managers/id.rs` | done | Every C# id manager (incl. `distinct` for FamilyIdManager) with the C# allocation algorithm and DB seeding; `UccIdManager` in `stream/ucc.rs`. The static `SkillTlIdManager` is the skills subsystem's. |
| `Core/Managers/NameManager` | `managers/name.rs` | done | Load, ValidateCharacterName (`CharacterNameRegex` plus the 1.2 `allowed_name_chars` whitelist and `blocked_texts` `check_name` rules), Add/Remove, NormalizeName. See "Character names" below. |
| `Core/Managers/ItemManager` (templates + user items) | `data/items.rs`, `managers/item.rs`, `models/item.rs`, `models/container.rs` | done | See `systems::economy`. |
| `Models/Game/Items/Containers` (ItemContainer, EquipmentContainer, MateEquipmentContainer, CofferContainer, LootingContainer) | `models/container.rs`, `systems/economy/looting.rs` | done | See `systems::economy`. |
| `Core/Managers/UnitManagers/CharacterManager` | `data/character_templates.rs`, `managers/character.rs`, `db_character.rs` | partial | Load (templates, body parts, buffs, supplies, `default_action_bar_actions`, equip packs, appellations, actabilities, expert limits, CharTemplates.json), Create, SetDeleteCharacter, SetRestoreCharacter, CheckForDeletedCharacters(+Deletion) with the delete task, IsCharacterPendingDeletion, LoadCharacters. `default_action_bar_actions` is Rust-only (the C# did not read it): the starter supplies use the authored slots (see the Create deviation). Bag expands, beauty salon, rolls, online tracking todo. |
| `Models/Game/Char` (Character, Inventory, Abilities, Actability, Skills, Appellations, Portals, Friends, Blocked, Quests (rows), VisualOptions, ActionSlot) | `models/character.rs`, `models/char_data.rs`, `db_character.rs` | partial | Character.Load (both variants' columns), Character.Load() sub-loads, Save / SaveDirectlyToDatabase (characters, options, abilities, actabilities, appellations, active buffs, portals, friends, blocked, skills, quests), Write, CheckWantedThreshold (buffs + pirate faction). Mates, craft, combat, events todo. |
| `Models/Game/Units/Buffs`, `Models/Game/Skills/Buff` | `models/buff.rs`, `systems/mod.rs` | partial | Indices, stack rules, GetAllBuffs, CheckBuff(s), GetEffectByIndex / FromBuffId, GetBuffCountById, RemoveBuff / RemoveEffect (index, template+skill) / RemoveAllEffects, persistence (SaveActiveBuffs / LoadActiveBuffs), the NetBuff wire format and `BuffEvent`s (started / refreshed / removed) dispatched to `systems::UnitHooks`. Effects (`Start` / `Dispel`), tolerance, triggers, `TriggerRemoveOn`, tag checks, timers, buff packets: skills subsystem. |
| `Models/Game/World/GameObject`, `Models/Game/Units/{BaseUnit,Unit,IBaseUnit,IUnit,BaseUnitType,UnitTypeFlag,ModelPostureType}` | `models/unit.rs`, `world/object.rs` | partial | All data members (`UnitBase`), `Hpp`, `IsDead`, `NeedsRegen`, trigger counts, `CalculateWithBonuses`, `UnitData` / `BaseUnitData` traits, handles (`Entity<D>`, `WorldObject`). Behaviour (`ReduceCurrentHp`, `DoDie`, `CanAttack`, regen, aggro, `UseSkill`, `SetFaction`, `UpdateGearBonuses`, the `SetPosition` underwater state) lives in the phase-3 subsystems (`systems::skills`). |
| `Models/Game/Units/{UnitCooldowns,CombatBuffs,UnitAttribute,UnitModifierType}`, `Models/Game/Skills/{Bonus,DynamicBonus,SkillModifier(s),BuffModifier(s)}`, `Skills/Static/{SkillAttribute,BuffAttribute}`, `Templates/{BonusTemplate,DynamicBonusTemplate,LinearFuncTemplate}` | `models/unit.rs` | done / partial | Data model + pure logic (`ApplyModifiers`, `Evaluate`, cooldown checks). The DB loaders for the rows and `TriggerCombatBuffs` are the skills subsystem's. |
| `Models/Game/World/Transform` (`Transform`, `PositionAndRotation`) | `models/transform.rs`, `world/mod.rs` | partial | Local position/rotation, `ToRollPitchYawSBytes/Shorts`, quaternions, `GetWorldPosition` parent composition (`WorldInstance::world_pos_rot`), `GetRootObj`, attach / detach children, sticky parents (`ObjectCore::sticky_parent`, `WorldInstance::set_sticky_parent`: the C# never fills `StickyChildren`, so a sticky parent is a marker plus a real parent). `FinalizeTransform`'s 5 s Sus analysis runs from the move handler (`SusManager::finalize_transform`). Debug trackers are not ported (debug only). |
| `Models/Game/NPChar/Npc`, `Units/Portal`, `DoodadObj/Doodad`, `Units/{Slave,Mate,Transfer}`, `Housing/House`, `Shipyard/{Shipyard,ShipyardData}`, `Gimmicks/Gimmick` | `models/{npc,doodad,slave,mate,transfer,house,shipyard,gimmick}.rs` | partial | Identity / state fields the visibility packets read, `Doodad.Write`, `House.Write`, `ShipyardData.Write`, `Gimmick.Write`, `TimeLeft`. Templates, funcs, spawners, AI, physics, persistence: phase-3 subsystems. |
| `Core/Packets/G2C` object visibility (`SCUnitStatePacket` all kinds, `SCDoodadCreated/sCreated/Removed/sRemoved`, `SCGimmicksCreated/Removed`, `SCGimmickJointsBroken`, `SCHouseState`, `SCShipyardState`, `SCMateState`, `SCSlaveState`, `SCTargetChanged`, `SCUnitAttached`) | `packets/world_objects.rs`, `world/unit_state.rs`, `world/visibility.rs` | done | Byte-exact tests (`world/unit_state_tests.rs`, `packets/world_objects.rs`, `object_flow_tests.rs`). The NPC reference-height adjust in `SCUnitStatePacket` needs heightmaps (todo). |
| Phase-3 subsystem framework | `systems/` | done | Per-subsystem data / managers / packets / start / save stages / hooks; packet registration per module with duplicate detection. |
| `Core/Managers/SkillManager` | `data/skills.rs` | partial | skills (learning fields), default_skills, passive_buffs, buffs (subset), start-ability / common skill lists. Effects, plots, tags, modifiers todo. |
| `Core/Managers/FormulaManager` + Jace engine | `data/formulas.rs`, `models/stats.rs` | done / partial | Formula tables + a Jace-compatible evaluator (differentially tested against Jace 1.0.0). Character Str..Fai / MaxHp / MaxMp from the unit formulas; bonuses and gear stats are approximated (see deviations). |
| `Core/Managers/World/WorldManager` (+ `WorldInstance`, `Region`) | `world/mod.rs`, `world/visibility.rs`, `data/world_templates.rs` | partial | World templates from `game/worlds/*/world.xml` (directory or game_pak) + world_spawns.json, GetZoneId, main_world instance, region grid, AddVisibleObject / RemoveVisibleObject / AddToCharacters / RemoveFromCharacters (all object kinds, region batches, children, the non-player removal filter), Show / Hide / Spawn / Delete, AddObject / RemoveObject / Get* lookups, GetAround, GetInCell, GetRegion / GetRootObj, server/instance character lists, UsedCharacterObjIds, OnPlayerJoin. Heightmaps, water, physics, `ResendVisibleObjectsToCharacter` todo. |
| `IO` (ClientFileManager, AAPak) | `data/client_files.rs`, `data/pak.rs` | done | Read-only pak (types A/B/F) and directory sources, file listing semantics of both. |
| `Core/Managers/World/ZoneManager`, `ZoneConflict`, `ZoneStateChangeTask` | `data/world_data.rs`, `systems/social/zone_conflict.rs` | done | zones, zone_groups, conflict_zones (+ return points / tower ids); the conflict runtime (`AddZoneKill`, `SetState`, `ForceNextState`, timers, `SCConflictZoneStatePacket` broadcasts) is `server.systems.social.conflicts`. |
| `Core/Managers/World/FactionManager`, `LocalizationManager` | `data/world_data.rs`, `systems/services/localization.rs` | partial / done | system factions (+ localized names), relations, SendFactions / SendRelations (relation checks: `systems::social`). The complete `LocalizationManager` (all translations, item-name cache, `MatchItemName`) is `systems::services::localization`. |
| `Core/Managers/ExpeditionManager`, `Models/Game/Expeditions` | `systems/social/expedition.rs` (`managers::expedition` re-exports) | done | Load, GetExpedition, SendExpeditions, SendExpeditionInfo, member refresh / login / logout status packets, and guild management: create, invite / reply, leave, kick, member role, owner, role policy, disband, rename, per-change save and offline leave. See the `systems::social` table. |
| `Core/Managers/FriendMananger` | `systems/social/friend.rs` (`managers::friend` re-exports), `db_character.rs` | done | Load, SendStatusChange, GetFriendInfo, FormatFriend, add / remove friends and block / unblock (online or by name), the client's failure feedback for every refusal (WP-SL2) and block-list enforcement on direct deliveries. See the `systems::social` table. |
| `Core/Managers/PortalManager` | `systems/npcs/portal/`, `data/world_data.rs` | done | recalls.json + district_return_points, recalls / world gates by sub-zone and id, respawn points and `closest_return_portal`, the private portal book (add / delete / rename), `CSUsePortal` with the range and court checks, the NPC teleporter and the navi teleport / open-portal packets. See `systems::npcs` and the portal notes below. |
| `Core/Managers/ChatManager`, `Models/Game/Chat` | `systems/social/chat.rs` (`managers::chat` re-exports), `world/session.rs`, `systems/social/session.rs` | done | Faction / nation / zone / guild / family / party / raid / court channels, join / leave / send, party / raid / raid-leader / judge routing (court / trial channels exist), per-channel spam delays with server enforcement, `LeaveAllChannels` and slash commands through `CommandManager` in `send_chat_message`. User channels are log-only as in C#. See the `systems::social` table. |
| `Core/Managers/TimeManager` (+ `TimeOfDayObserver`) | `managers/time.rs` | done | `Get` / `Set` / `Stop` added (phase 4). Storm-ship time override (`ResolveClientTimeOfDayHours`) per observer (`subscribe_with`). |
| `Core/Managers/SaveManager` | `managers/save.rs` | partial | Periodic + final save of items and online characters; the subsystems' rows through `systems::save_stage` (`BeforeItems`: houses, mails; `AfterItems`: auction, crimes; `AfterCharacters`: slaves). |
| `Core/Managers/World/StreamManager` | `managers/stream.rs`, `stream/cells.rs` | done | AddToken/RemoveToken/Login (wrong account removes the token, as C#), RequestCell/ContinueCell incl. the C# `next - 1` copy off-by-one. |
| `Core/Managers/Stream/UccManager`, `Models/Stream`, `Core/Managers/Id/UccIdManager` | `stream/ucc.rs`, `stream/models.rs` | done | Load/Save (`uccs` table, data in the MEDIUMBLOB as the default C# build), upload (simple + custom), download, checks, id allocation, `CreateStamp` / `ApplyStamp` (`economy/item_effects.rs`, the `GainItemWithEmblemImprint` effect and `economy/bridges.rs`). **Crest printer (WP-U1, housing-farm-ucc#1):** `CTStartUploadEmblemStream.bc` must be a live doodad of the player's instance whose **current func group** has a `DoodadFuncStampMaker` row creating Crest Ink (`item_id` 17663: doodads 3038 / 1470, funcs 6314 / 11693 / 19145), the player must be in the func skill 14435 reach (the doodad `in_func_reach`: skill `max_range` + the 15 m allowance) and must hold the row's cost for the upload kind the client selected — a default pattern's `consume_money` (50000 for 3038, 10000 for 1470) or a custom image's `consume_item_id` x `consume_count` (1x 11127 Crest Brainstorm) and **no money** — all checked **before** `add_default_ucc` / `start_upload`, so a refusal leaves no `upload_queue` / `complex_upload_parts` entry. The split is the client's own: `X2Ucc.GetMakeUccConsumeInfo` (`FUN_394b5e80`, x2game.dll.c:981806) returns item + count for a user image and otherwise formats the row's money, matching `ui_texts` 4206 "You need 1 Crest Brainstorm to create a custom crest." and 4207 "Crest Ink costs the following amount to craft". `confirm_default_ucc` re-checks the same cost (the queue entry keeps the resolved row) and `charge_and_create_crest_ink` does the checked default-pattern money debit, or the custom-image reagent consume (`ItemTaskType.IMPRINT_UCC`) with no money, plus the Crest Ink creation in one `ItemTxn` before the save. A refusal is a `warn!` and a drop (the 1.2 refusal reply is UNKNOWN, **CAP-UCC-1**) and the stream connection stays open. **CAP-UCC-1 resolved (2026-11-06 Part B reassessment, B9):** the client decompile shows a custom upload costs the item only; the C# money charge on that path is dropped. Live: no `aaemu-testclient` scenario speaks the stream protocol (join + `CTStartUploadEmblemStream`), so this is covered by flow tests only. **Imprint validation (WP-U2, housing-farm-ucc#3 / tables-unused-2#10):** `ImprintUccEffect` refuses (`Outcome::Cancelled`, the existing missing-operand path, so the stamp and the target keep their UCCs and the skill keeps its reagent) when the target template is not a `ucc_applicables` kind-2 `actual_id` (80 shipped item rows) or when the stamp's UCC (`UccManager.get_ucc`) carries a non-zero `pattern1` / `pattern2` that is not an `emblem_patterns.id` (57 shipped rows). Kinds 1 (18 slave template ids) and 3 (30 flag doodad ids) are INFERRED and not enforced; the pattern id space is INFERRED (CAP-UCC-1). A table with no matching rows stays permissive (the pre-WP-U2 behaviour), so an older compact still loads. The other `tables-unused-2#10` tables (`world_groups`, `world_spec_configs`) stay documented-only in the no-package list. |
| `Core/Managers/TimedRewardsManager` | `systems/services/timed_rewards.rs` | done | `TimedRewardsTask` every minute, daily login reward, offline labor. |
| `Core/Managers/HousingManager`, `Models/Game/Housing` | `systems/housing/` | done | See "Phase 3 subsystems". |
| `Core/Managers/CrimeManager` | `db_character.rs` | partial | CheckWantedThreshold thresholds only. |
| Other `Core/Managers/*` | – | see the subsystems | AccessLevel, Command, ExpressText, Model, Music, Radar, Sus, Task/Tick, TimedRewards: `systems::services`; Animation, EffectTask, ManaRegen, Plot: `systems::skills`; FishSchool, Gimmick, PublicFarm, SubZone, Spawn, AreaTrigger, Specialty, SphereQuest, Portal: `systems::npcs`; Family, Duel, Team, Trial, Crime: `systems::social`; Quest: `systems::quests`; Auction, CashShop, Craft, Mail, Trade, Taxations: `systems::economy`; Housing: `systems::housing`; Mate, Slave, Shipyard, Transfer: `systems::vehicles`; Indun, InstantGame: `systems::instances`; AI / AiPaths / AiGeodata, World/Physics: `systems::ai`. |
| `Models/Game/Units/UnitCustomModelParams` | `models/unit_model.rs` | done | |
| `Models/Game/Units/Movements` (MoveType hierarchy) + `MathUtil` direction helpers | `models/movement.rs` | done | Default / Unit / Vehicle / Ship / ShipRequest / Transfer, byte-exact read/write (all actor-flag combinations tested). |
| `Models/AccountPayment`, `Models/Account` | `net/connection.rs`, `db.rs` | done | |
| `Models/AppConfiguration`, `Models/Game/Configurations` + `Program` config loading | `config.rs` | done | Every member (incl. WebApiNetwork, DiscordToken, Dungeons, AccessLevel, LaborOffline, Specialty, Scripts, full World), Config.json + Configurations/** + Config.Local.json + user secrets + environment (`Section__Key`) + command line, enums by name / value, `ConfigCell` (= `AppConfiguration.Instance`, replaceable at runtime). |
| Other `Models/Game/*` (AI, Auction, Crafts, Crime, DoodadObj, Mails, Mate, NPChar, Quests logic, Slaves, Team, ...) | – | see the subsystems | Ported with their managers (phase 3 / 4 subsystems). `Achievement` has no 1.2 logic. |
| `Scripts/Commands/**`, `Scripts/SubCommands/**`, `Utils/Scripts`, `Utils/{Quest,Sphere}CommandUtil` | `systems/services/commands/` (+ `scripts/`) | done | Infrastructure (`CommandManager` with async command execution, `AccessLevelManager`, `ICommand`, `IMessageOutput`, `SubCommandBase` + parameters) and every GM command: one module per `Scripts/Commands/*.cs` file in `scripts/`, the `SubCommandBase` roots in `scripts/roots.rs` / `scripts/sub/*` (Crimes, Doodads, Gimmick, Gold, Items, Npcs, Slaves sub commands). `scripts::load` registers them in the C# type order (`ScriptReflector`), so `GoldCmd` owns `gold` (and `AddGold`'s aliases) and `CrimeCmd` registers `crime` with `/doodad` as call prefix. Tests: a harness test per command (`scripts/harness.rs`), the registered set vs the C# sources (`scripts/tests.rs`). |
| Retail GM command names, tax / crime / social (WP-SL6b) | `systems/services/commands/scripts/{delay_tax_due_date,resend_house_tax_mail,set_bounty_money,clear_crime_status,set_crime_value,set_jury,imprison}.rs` | added | The C# server had no equivalents; the names are the 1.2 client's command table (`siege-social-2.md` 1.5). `delay_tax_due_date <character> [days]`, `resend_house_tax_mail <character>` (console shapes `DelayTaxDueDate(%s)` / `ResendHouseTaxMail(%s)`; the day count is an assumed default of 7), `set_crime_value <character> <value>` (console shape `SetCrimeValue(%s, %d)`), and the assumed positional forms `set_bounty_money <character> <amount>`, `clear_crime_status <character>`, `set_jury <character> <trialId>`, `imprison <character>`. Effects reuse the gameplay paths: `HousingManager` tax dates / `new_tax_mail` (online or offline owners, dominion infrastructure skipped), `CrimePoint` / `InfamyPoint` setters (online only), `BountyManager.place` with the GM as payer (online wanted target), `TrialManager.force_summon_jury` -> `SummonJuryMember` (invitation/queue checks skipped, step/main-world/one-trial checks kept), and the `crime court jail` holding-cell effect (force dismount, `FORCIBLY_AWAITING_TRIAL`, teleport). No new opcodes; admin access 100 in `AccessLevels.json`. |
| Retail GM command names, siege / dominion (WP-SL6a) | `systems/services/commands/scripts/siege_alias.rs` | added | The names are the 1.2 client's command table (`siege-social-2.md` 1.5); the C# server had no equivalents. `schedule_siege <zoneGroupId> <year> <month> <day> <hour> <minute>` uses the EVIDENCED console shape `ScheduleSiege(%d,%d,%d,%d,%d,%d)` but reads the six integers as zone + local wall-clock date/time (INFERRED: the console labels beside the shape are `declare_min`/`warmup_min`/`siege_min`/`peace_min`/`pay_min`, so the retail minute-vs-date reading is UNKNOWN). The attacker is the territory's accepted declaration (the six arguments carry no expedition, and the declaration chain is the only other authority that names one); the record uses the zone's authored `siege_zones` duration. `stop_siege` / `off_siege <zoneGroupId>` call the existing `cancel_siege` path. `delete_dominion <character> <zoneGroupId>` uses the EVIDENCED `DeleteDominion(%s, %d)` shape and only deletes a record owned by that character's expedition (assumed meaning of the name). `change_dominion_owner` and `bestow_dominion <zoneGroupId> <expeditionId>` use the new single-lock `DominionManager::transfer_owner`: the target's owned-territory limit is checked under the claim lock, an unfinished siege is refused, and a transfer is not a fresh claim (`claimed_at = 0`); the bestow form sets the client's `bestowed` announcement bit. `set_dominion_non_pvp <zoneGroupId> [minutes]` reuses `start_non_pvp` (default 120, the authored row). `set_dominion_limit <limit>` updates `Housing.DominionClaimLimit` at runtime. `declare_dominion_always` is **not registered**: skipping the claim gates is INFERRED and no existing switch already carries that meaning (the claim window, pirate refusal and claim limit are separate checks). No new opcodes; admin access 100 in `AccessLevels.json`. Tests: effect, argument-error and access-guard cases per command in `siege_alias.rs`. |
| `Services` (WebApi, DiscordBot) | `systems/services/webapi/`, `systems/services/discord.rs` | done | axum transport with the C# regex routing; mail / auction controllers call the economy through `WebApiEconomyHooks`. |
| `GameService` startup / `ManagerOrchestrator`, `Program` | `main.rs`, `systems/services/game_service.rs` | done | `Program.Main` (initialization log, `compiler-check`, config command line, MySQL / SQLite checks) and `GameService.StartAsync` stages in order (DB updater, client files, holdables check, game data, user items, DB loads, time / task managers, subsystems, save manager, deleted characters, online tracking, networks, "Server started! Took ..."), then WebApi / Discord; shutdown in reverse (Discord, WebApi, final save, task / tick / time managers). `GameService.StartTime` / `TimeSinceStart`, `Program.UpTime`, `ManagerOrchestrator.BuildBatches` ported with their tests. |

## Phase 3 subsystems

Each subsystem records its status here, in its own section only (see `CONTRIBUTING-PORT.md` §3).

### systems::skills

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `SkillManager` (+ `Sqlite*` effect / buff / plot loaders), `PlotManager`, `FormulaManager` remainder, `AnimationManager`, `AreaTriggers` shapes, `ModelManager` radii, `UnitRequirementsManager`, `TagsManager` (skills / buffs) | `systems/skills/data/` | done | Every skill / buff / effect / plot / tag / modifier / trigger / reagent / product / linear func / tolerance / anim / aoe shape table, incl. all 30+ effect types. Gated real-DB test `skills_real_compact_db` checks row counts. |
| `Models/Game/Skills/{Skill,SkillCaster,SkillCastTarget,SkillObject,CastAction,SkillController*}`, `Core/Packets/{C2G,G2C}` skill / buff packets | `systems/skills/{types,packets,skill,handlers}.rs` | done | Byte-exact wire types (tested), `Skill.Use` / `Cast` / `Channeling` / `Apply` / `EndSkill` / `Stop`, GCD + cooldowns, mana, labor / vocation, reagents via `ItemSkillHooks`, auto attack loop, `CompressedGamePackets`. Data gap: 6 `quest_components.skill_id` values (20315, 11212, 17708, 13737, 11981, 13590) have no `skills` row. The C# `Unit.UseSkill` threw a NullReferenceException there; Rust skips the cast (the quest continues) and `unit_use_skill` warns once per skill id (`missing_skill_warned`), repeats go to debug. |
| `Models/Game/Skills/Plots/*` | `systems/skills/plots.rs` | done | Plot tree execution (async), conditions 1..17, target info updates (area / random unit / random area), channeling finish, `SCPlotEvent` / `SCPlotEnded`. Deviation: the timeline-id release in plot end, `end_skill` and `stop_skill` is take-and-clear under the skill state lock (`SkillRun::take_tl_id`); C# did an unsynchronised `ReleaseId(TlId); TlId = 0`, so an interrupted plot skill could release the same id twice. The `Tried to release unused Id` warning names the last owner (caster obj id, skill id), like C# `AssignedUnit`. |
| Plot-driven cast bars (`SCPlotEventPacket.castingTime`) | `systems/skills/plots.rs` (`node_execute_packet`), `packets.rs` | verified | The 1.2 client learns a plot cast's duration from `SCPlotEvent`'s `castingTime` (reader `FUN_397b2160`: wire u16 x10 = ms; `FUN_3938f100` starts the cast state when it is non-zero). `node_execute_packet` already sends `nextEvent.Delay / 10` for every casting / channeling next event, like C# `PlotNode.Execute`. Verified with skill 17563 (Akmit's Terrifying Roar: `casting_time` 0, plot 338, cast event 2599 has a 5000 ms `casting` next-event) against the 5.5-6 s target cast bar in the dungeon video (grade C): the real-data test asserts the plot sends `castingTime` 500 = 5000 ms. No code change. |
| `Models/Game/Units/Buffs` behaviour, `Skills/Buff`, `Skills/Effects/{Buff,Damage,Heal,Dispel,RestoreMana,ManaBurn,Aggro,FlyingStateChange,SkillController,...}`, `CombatBuffs`, `BuffTriggers` | `systems/skills/{buffs,effects/*}.rs` | done | Start / dispel / tick tasks, tolerance, stack rules, charges, `TriggerRemoveOn`, tag removal, absorption, triggers, area ticks. Explicit dispel emits Dispelled without Timeout; natural expiry retains Timeout. This corrects the mirrored C# removal path, which invoked the FISH escape trigger when catch plot 792 dispelled FISH before lethal damage. Queued CREATED handling and existing removal packet order are retained. Refreshing permanent non-ticking buffs invalidates old timers without treating GetTimeLeft=-1 as expiry; finite expiration and permanent tick cadence remain scheduled. This corrects the mirrored C# refresh timer path. Special effects routed through `ExtHooks::route_special` where they belong to another subsystem. |
| `DamageEffect` fixed-damage DoT tick order (`use_fixed_damage` with `source.Buff.TickEffects`) | `systems/skills/effects/combat.rs` (`damage_with_aoe`), `systems/skills/{buffs,effects/mod}.rs` (`EffectSource::buff_permanent`), `systems/housing/manager.rs` (burning-house deviation) | fixed | The C# tick scale `fixed * tick / duration` existed until bd90e0b73 moved the standalone fixed override after it, so a fixed-damage DoT dealt the whole fixed range on every tick. 1.2: the fixed range is the **total over the buff duration**. The client macro returns the fixed term undivided for `total_min_damage`/`total_max_damage` (10.x `sub_398CFD40`/`sub_398CFE10`) and divides it by `duration/tick` for `min_damage`/`max_damage` (`sub_398CFFC0`/`sub_398D00C0`); the localized descriptions state a total (buff 3910 "Metal Poisoning": `Deals #{total_min_damage}~#{total_max_damage} damage over #{duration}`; buff 250 "Electric Shock"; buff 146 "Poisoned"; skill 10670 "Wrath Lightning" `#{buff_total_avg_damage}`). `damage_with_aoe` now applies the tick modifier after the fixed override, exactly like `HealEffect` (fixed -> `* tick_modifier`); a buff trigger is a one-shot event, so its standalone fixed value stays whole (the heal path special-cases `IsTrigger` too). Effect: buff 3910 (DamageEffect 3427, fixed 1800-2200 over 11 s) ticks 164-200 instead of 1800-2200 (the 2014 dungeon video shows 241-329 pre-mitigation, about 1.5x the new per-tick value; that audit row is low confidence and may include caster/target modifiers). The 2026-11-08 bot run's buff 2783 (fixed 2800-3000 over 5 s, 500 ms ticks) dealt 2800-3000 per tick, above the target max HP. A live endless buff (`duration` 0, `count` -1) has no total to spread (`EffectSource::buff_permanent`), which keeps the unused buff 39 "Burning House" deviation at 30 HP/s instead of 30/100,000 s. Tests: `damage_pipeline_fixed_dot_spreads_its_total_over_the_ticks` (fails with the old order: 1100 per tick), `damage_pipeline_percent_dot_tick_keeps_the_existing_scale`, `damage_pipeline_fixed_dot_tick_matches_the_fixed_heal_tick`, `damage_pipeline_fixed_trigger_keeps_its_full_value`, `arc_lightning_tests` (fixture 320 total = 40 per tick), `abandoned_house_burning_ticks_30_per_second_and_removes_the_house_at_zero`. |
| `formulas` rows 20 / 21 (`damage_bonus_multiplier_by_level`, `damage_penalty_multiplier_by_level`) | `systems/skills/effects/combat.rs` (`level_damage_multiplier`, `damage_with_aoe`) | added | The 1.2 client never evaluates these rows (no `FormulaManager` accessor or variable setter for kinds 2, 3, 11, 12, 20, 21, 24-26; `D:/aaemu-audits/gap-audit-2026-11-10/research-data-rules.md` §1), so the rule comes from the data rows alone. Compact `formulas`: row 20 `if_negative(pc_level - npc_level, 1, 1 + (pc_level - npc_level) * 0.04)` (+4% per level at or above the NPC, x1 below it) and row 21 the constant `1` (no penalty). A player character's damage to an NPC is multiplied by row 20 evaluated with the character's and NPC's levels, read from the row at runtime (tests use a 0.10 variant and a missing row), never below x1 (a failed evaluation returns 0 and the caller clamps it; the first failure of a formula logs at `warn`, later ones at `debug`). The factor sits with the other flat multipliers next to the PvP `battle_resist` factor (row 23) before critical / armor mitigation; melee / magic / ranged / siege `DamageEffect` damage and buff-sourced DoT ticks from a player to an NPC get it, but only for weapon / level damage rows: authored fixed values and percent-of-HP rows (e.g. effect 4754 "99% of current HP") keep their exact value. Heals (`combat::heal`) never reach this path. Pets / summons / slaves are excluded: the variables name the player character (`pc_level`) and the target NPC (`npc_level`), so only the direct player caster qualifies. Structure targets are excluded too: `NpcKindType.SiegeWeapon` NPCs (kind 9: battlefield turrets / cannons and practice copies, 22 compact templates at levels 20-50) and the training-dummy faction 170 "허수아비" (8 templates at levels 1-50). Castle towers / lodestones are houses and siege gates / walls are doodads, so they never reach this NPC-only path. Tests: `damage_pipeline_level_bonus_*` (level 50 vs 40 = x1.40, equal level x1, lower-level player x1, PvP unchanged, player -> player / NPC -> player x1, fixed and percent rows unchanged, DoT tick scaled, heal not scaled, siege-weapon kind and dummy faction x1, rate from the row), `evaluate_logs_a_failure_once_then_only_at_debug`. |
| `buffs.transform_buff_id` (stack replacement) | `systems/skills/{buffs,data}.rs` (`transform_at_max_stack`); stack bookkeeping in `models/buff.rs` unchanged | added | The C# loaded the column and never read it. The 1.2 rows plus their tooltips establish the rule: the application that reaches `max_stack` replaces the stacks with `transform_buff_id`, not the one after it (1757 `max_stack` 2 "Two stacks turn it into Nymph's Flame"; 2214 `max_stack` 4 "Stacks up to 3 times. A 4th stack Stuns" -> 2541; 2407 `max_stack` 5 "Petrifies upon reaching 5 stacks"; 2724 "at the 4th stack"; 5793 "At 20 or higher tension"). All 66 rows are `stack_rule_id` 4 with `max_stack >= 2`; the authored chains 4152 -> 4752 -> 4101 and 5378 -> 5379 -> 5380 run through the normal add path (a transform that reaches its own cap transforms again), bounded by a chain depth / visited-id guard. Every stack is removed through the normal removal path (Dispel hooks, `SCBuffRemovedPacket`) after the max-th stack's Started hooks, and the replacement keeps the triggering application's caster, skill, skill level and ability level. The live GM run (`/buff 2656` x3, `max_stack` 3) sent three `SCBuffCreatedPacket`, three `SCBuffRemovedPacket` and one `SCBuffCreatedPacket` for 2657. **Inferred (no 1.2 server evidence):** the create-the-max-stack-then-replace ordering, the caster/skill carry-over, and a missing / cyclic transform id keeping today's refresh-at-cap behaviour (a missing target is reported once at load and the column is cleared). Real-DB test `skills_real_compact_db_transform_buffs` (66 rows, target exists, stack rule 4, `max_stack >= 2`). |
| `buffs` damage reflection (`reflection_chance`, `reflection_type_id`, `reflection_ratio`, `reflection_target_ratio`) | `systems/skills/{buffs,effects/combat}.rs` (`reflection_effects`, `apply_reflection`) | added | The C# loaded the columns and never read them (`BuffAttribute.ReflectionTargetRatio` is unused and no `buff_modifiers` row uses attribute 1). The 36 1.2 rows (`reflection_chance > 0`) and their localized tooltips establish the core: `reflection_type_id` is the incoming hit's `DamageType` (1 Melee, 2 Magic, 3 Siege, 4 Ranged; 4407/5646 "Melee", 177/3563 "Magic", 4408/5404 "Ranged", Fire Armor 5531-5534 one buff per type), `reflection_chance` is a percent chance (177 47, 1035 50, the rest 100), and `reflection_target_ratio` is the percent of the received damage returned (386 50 "Reflects 50%", 5646 6, 7132 50, 100 "all damage"). A landed hit's victim rolls its matching live buffs in buff order and the first success returns `floor(value * target_ratio / 100)` to the attacker through `reduce_current_hp` (normal HP/death path, kill credit, durability); the attacker must still be a living unit in the world (the incoming hit already passed `can_attack`), plus an ordinary `SCUnitDamagedPacket` cast from the buff. **REFLECT-1 / REFLECT-2 / REFLECT-3 (capture-pending):** `reflection_ratio` is ignored (the reflector takes the full hit: 100 for 28 rows, 10 for 4366/4407/4408, 50 for 7132; the best guess "share still taken" is not proven); several matching buffs are not added up (the first successful roll wins, so two "reflects all damage" buffs cannot return more than the hit); the reflected hit is an ordinary damage packet whose `SkillHitType` is the hit category's normal value (1.2 has no reflect hit type, so the client cannot select the `REFLECT` combat-text key). Reflected damage rolls no item procs, no combat buffs and no `Attacked`/`Damaged`/`Damage` events of either side, and it never re-enters `damage_with_aoe`, so two reflectors cannot ping-pong. All skill / plot / DoT-tick / NPC / PvP / buff-trigger damage goes through `damage_with_aoe`; fall damage, regen, physics collision, GM commands, item capture and shipyard damage do not, and therefore never reflect. Real-DB test `real_compact_reflection_and_use_damage_amount_catalogs` (36 rows, per-type columns). |
| `buff_triggers.use_damage_amount`, `DamageBuffTrigger` (event 3) / `DamagedBuffTrigger` (event 4) amounts | `systems/skills/{buffs,effects/combat}.rs` (`execute_trigger`, `restore_mana`, `heal`), `effects/mod.rs` (`EffectSource`) | fixed | The C# ignored `use_damage_amount` and the `Damage` event never set `IsTrigger`, so the fixed values were never scaled. `RestoreManaEffect` now scales a fixed roll by the event amount when the trigger row sets `use_damage_amount` (`value = value / 1000 * amount`, the same unit `HealEffect` already used) and skips the `percent` maximum-mana scaling (753 "Converts 3% of received damage to mana": fixed 30, 200 damage -> 6; before it restored 3% of max mana). The `Damage` event (kind 3, damage dealt by the owner) now carries the real amount into `EffectSource` for 806 Baleful Recharge ("of the damage inflicted"), 4257 Executor's Cruelty and the other event-3 trigger rows, so their heal/mana effects scale too; its target stays the owner as in the C# `DamageBuffTrigger` (`OnDamageArgs.Attacker` is the damage caster itself). Only `HealEffect`/`RestoreManaEffect` read the amount; the 23 `SpecialEffect` (Charge) rows carrying the flag have no formula and are left unchanged (rating 3 in the research). The data has 95 restore-mana triggers with the flag (94 Damaged, 1 Damage), all `use_fixed_value` and `percent` 't'. Real-DB assertions in `real_compact_reflection_and_use_damage_amount_catalogs`. |
| `Units/Unit` combat (`ReduceCurrentHp`, `DoDie`, aggro / tagging, `SetInBattle`, criminal / force attack, invisibility, `DoFallDamage`), `BaseUnit.CanAttack` / relations, regen, `UpdateGearBonuses`, all `Character` stat getters | `systems/skills/{combat,relation,stats,regen}.rs` | done | MaxHp / MaxMp etc. with buffs, passives and gear (`GearStats` hook). NPC death side effects through `NpcSkillHooks`. |
| `Character.ComputeDeathWaitTime` (C# hard-coded `DeathWaitTimesSeconds` / 5-minute reset) and `npcs.no_penalty` | `systems/skills/{combat,data}.rs`, `systems/npcs/npc/data.rs` | added | The 1.2 `resurrection_waiting_times` rows drive the escalating respawn wait and the active-siege variant; a killer NPC with `npcs.no_penalty` (88 rows) skips the exp / durability death penalty. The C# read neither table; see the deviations bullet for DEATH-1..3 and the fallback. |
| Taunt buffs (`buffs.taunt` / `buffs.taunt_with_top_aggro`) | `systems/skills/{combat,buffs,state}.rs`, `systems/ai/{behaviors,combat}.rs`, `systems/quests/bridges.rs`, `systems/skills/effects/special/unit_actions.rs` | added (10x-only) | The 1.2 client and the C# have no taunt logic; the mechanic is derived from the 10.x zone server `NpcAggroTable::TauntedBy` / `CancelTauntedBy` / `CopyTopAggro`, and the 1.2 data agrees (134 `taunt='t'` buffs, 97 also `taunt_with_top_aggro`; buff 171, buff 502 "highest threat", skill 17300). On the generic buff-start path (`on_started`, so trigger- and script-applied buffs are covered too) an NPC holder gets `taunted_by(caster, taunt_with_top_aggro)`: refused when the caster is the holder or explicitly friendly to it (10x refuses a `CanAttack`-false caster; the 1.2 PvE monsters are faction-neutral to players and attack through AI aggression, so `CanAttack` would refuse every taunt on them; this is recorded as a deviation from the 10x check); the aggro entry is created through `AddUnitAggro` (zero threat), stamped with the server clock (non-zero 64-bit unix ms, never wrapping), and with `taunt_with_top_aggro` its total is raised to the current highest total of the table (one-off copy, never above). `sorted_abusers` sorts any stamped entry above every unstamped one and the latest stamp first; `aggro_rating_in_percent` and `aggro_rating_percent` mirror that order. On buff end (expiry, dispel, death, explicit removal) `cancel_taunted_by` clears the stamp only while no other taunt buff of that caster remains; a refresh by a different caster (the `REFRESH` stack rule overwrites the effect's caster) releases the previous caster's stamp first, so a replaced taunter cannot stay pinned above the table. Threat is not reduced, so a top-aggro copy persists. `aggro_copy` drops the stamp and `aggro_reset` clears it. **Capture-pending:** re-stamping on refresh (TAUNT-1) and the copy/reset stamp behaviour (TAUNT-2) are chosen, the 10x decompile does not show them. 10x also refuses members of the NPC's own group; `npc_groups` is loaded by `npc/group_data.rs` (spawner expansion), but no runtime membership links a spawned NPC back to its group, so none is checked. Tests: `taunt_*`, `refreshing_the_taunt_buff_restamps_the_table`, `taunt_refresh_by_a_second_caster_releases_the_first_stamp_on_expiry`, `aggro_copy_drops_and_reset_clears_the_taunt_stamp`, gated `real_compact_taunt_buffs_and_the_skills_that_apply_them`. |
| Pull aggro (`Ai.PullAggro*`) | `systems/ai/{behaviors,config}.rs`, `app/Configurations/Ai.json` | added (Rust only, configurable) | An NPC with a valid current aggro target (in the table, visible, alive) keeps it until a challenger's total exceeds the target's total times a ratio: `PullAggroMeleeRatio` 1.10 when the challenger is within `PullAggroMeleeRange` (4.0 m, the melee reach the skill code already uses) of the NPC, else `PullAggroRangedRatio` 1.30; `PullAggroEnabled` defaults to true. Taunt is unaffected: a stamped challenger beats an unstamped target at once, the latest stamp still wins among stamped entries, and a stamped target cannot be pulled by threat. A target that is gone, dead or invisible falls back to the first valid entry in sorted order. The 10.x `NpcAggroTable` re-rank (`sub_3931EB70`, `x2game-dev_dedicate`) switches on a strict `>` with no ratio, so `PullAggroEnabled: false` restores the original behaviour exactly. `aggro_rating_in_percent` and the quest `aggro_rating_percent` still rank the table, not the target. Tests: `pull_aggro_*`, `ai_section_binds_and_defaults`. |
| Spreading DoT tick area (`tick_area_relation_id` 6: Arc Lightning / Electric Shock buffs 250, 5847, 7465) | `systems/skills/relation.rs` (`filter_holder_allies`), `systems/skills/buffs.rs` (`do_area_tick`), `effects/combat.rs` | deviation (C# bug fixed) | 6 is not in the C# `SkillTargetRelation` enum, so `FilterWithRelation` fell through `default => units` and the DoT and its damage hit every unit within 2 m of the holder, the caster, its mount and its party included; hitting them (`check_crime`) then raised Retribution 2167. The buff text says "on you and allies within 2m", so the Rust reads 6 from the holder: the holder plus units friendly to the holder, never the caster, its mates / slaves or anything owned by the caster's character, and only units the caster may attack. `can_attack` also returns false when attacker and target belong to the same character (yourself, your own mount / slave), even with Ctrl+F force attack or a Retribution flag (C# made such a pair attackable and raised Retribution), and `DamageEffect` never flags a crime for such a pair. Tests: `arc_lightning_tests.rs`. |
| Aura buffs (`aura_radius`, `aura_relation_id`, `aura_slave_buff_id`, `aura_child_only`) and crowd buffs (`crowd_buff_id`, `crowd_radius`, `crowd_number`, `crowd_friendly`, `crowd_hostile`) | `systems/skills/aura.rs` | data-inferred / capture-pending | The 1.2 client only reads the columns and has no runtime; no 1.2 zone binary exists, so the rules come from the localized descriptions. A 1 s enter/leave driver (the area-trigger update pattern) grants the slave buff to units in `aura_radius` passing `aura_relation_id` (the owner too for Any/Friendly/Party/Raid), removes only the copy with caster = aura owner, and grants `crowd_buff_id` while `crowd_number` *other* carriers of the source buff are in `crowd_radius` and match `crowd_friendly` / `crowd_hostile`. The dangling slave 6668 -> 6669, the self-reference 1224 and the 15 aura rows without a slave buff are skipped with one load warning. Case ids AURA-1..8 / CROWD-1..4; update period, slave caster identity, owner counting, child attach scope and dead-owner behaviour are UNKNOWN. |
| Learning / progression (`LearnSkill`, `LearnBuff`, `ResetSkills`, `SwapAbility`, `AddExp`, labor, game points) | `systems/skills/progress.rs` | done | Labor accrual uses zero-based actability steps into ordered `expert_limits`, matching inventory / regrade; step 4 caps at 50,000 (row id 5), avoiding the former row-id lookup that reduced it to 40,000. Capped gains and packet deltas have regression coverage. `SwapAbility` follows the 1.2 client: activating an empty slot (`old = None`, the level-10 `LearnAbility` path) is free; changing an active ability costs `content_configs[39] * level` copper, requires exactly three active abilities and a non-active target, and the debit and swap share one character lock. An ability level-up sends `SCSkillUpgraded` for the learned skills that crossed a level step (INFERRED, CAP-UI-2). |
| `Skills/Effects/SpecialEffects/*` (141 named `SpecialType` actions), `BubbleEffect`, `ImpulseEffect`, `PhysicalExplosionEffect` | `systems/skills/effects/{special/*,unit_fx}.rs`, `packets_special.rs` | partial | Ported actions and subsystem hooks are present, but historical logging placeholders do not establish gameplay completeness. Buff stealing, component-selective aggro copy/reset, target loss, NPC despawn, ongoing-buff cancellation and pet recovery now execute. ExplodeBuff, MoveToGround and RedeemBuff now execute (see below). Client visuals use the existing skill/plot notifications. 13 extra S->C packets, byte-exact tests. |
| `Skills/SkillControllers/{Floating,Wandering,Leap,Dash,Rotate}` | `systems/skills/controllers.rs` | partial | Rotate uses native constructor/tick timing (`0x3924E1D0` / `0x392315C0`) with 1.2 controller7678 regression coverage. WP-9 verdict (2026-11-05): kinds 5 Rope, 6 Anchor and 8 Flowgraph get no generic controller. Rope (8 rows; the users are hook throw 12093, harpoon launch 13478 / 13749 / 19166 and cut rope 13596 / 13750 / 19168) is handled by `systems/vehicles/harpoon.rs` + `CSSkillControllerState`; Flowgraph (row 7907) is the test skill 23969 only; Anchor (rows 7736 / 7739, monster skills 22268 / 23103 "Land on the ground") is score 2/10 and its parameters are unknown. Native 10.x Anchor parameter kinds 1001..1039 differ from the 1.2 data kinds 1..3. The expected log for these kinds is `debug`; see the WP-9 subsection. |
| Movement hooks (`CSMoveUnit.RemoveEffects`, `Character.SetPosition` Move removal, fall damage) | `systems/skills/mod.rs` (`on_move_request`, `on_moved`) | done | Called from `world::session::move_unit`. |
| Cannon fire aim: `BallisticPos` / `ArtilleryPos` targets, `vehicle_models` turret limits | `systems/skills/{skill,validation}.rs`, `systems/vehicles/data.rs` (`VehicleModel::clamp_turret_angles`) | deviation | **Deviation:** the C# `Skill.GetInitialTarget` mapped every `Pos` / `BallisticPos` / `ArtilleryPos` cast target through `SetInitialTarget`, whose non-position arm kept the caster's own position, and it never checked the target cone. The Rust refuses a Unit / Doodad target on `BallisticPos` / `ArtilleryPos` with `NO_TARGET` 0x9 (the 1.2 client's result for a missing target; the Lua bar, path B, sends a Unit target for cannons); a Position target is then held only to the skill's own `target_angle` cone, measured from the cannon's composed world facing (parent ship yaw + attach point). It does **not** refuse on `skills.pitch_angle` or the `vehicle_models` pitch / yaw limits: the client evaluates `CanUse` on the aim before it clamps the turret to those model limits (`FUN_394322f0` steps 7 then 8 -> `FUN_39425490`), and the spliced `CanUse` body shows no `pitch_angle` reader, so the model limits cannot block a shot (the aim preview's ballistic solve also starts at the `item_hand_r` muzzle, which the server does not reproduce). The model limits clamp the stored / relayed `CSTurretState` posture instead (see the `CSTurretStatePacket` row). A refused mount cast now keeps its own result even when a rider / operator skill is linked (the rider only runs after a successful mount skill). |
| `Skill.EndChanneling` target buff (`ChannelingTargetBuffId`) | `systems/skills/{skill,buffs}.rs` (`end_channeling`, `finish_effect_by_skill`) | deviation | **Deviation:** the C# `Buffs.RemoveEffect(templateId, skillId)` raised the buff's `OnTimeout` triggers on every removal; the Rust removal skips them for explicit removals (dispels, a stopped channel; see the FISH escape in `buff_removal_tests.rs`). Quest item grants hang on that trigger (GainItem on buff 3896 of skill 18449 'Extract Earth Crystal', 4 s channel under a 6 s buff, and the other channeling-target-buff quest skills of the item triage), so a channel that **runs to its end** now ends the target buff the way its own timer would (ACTING -> FINISHED, Timeout triggers, removal). The data pairs a channel with a target buff of the very same length in 18 skills with a Timeout trigger (e.g. 14054 'Insert Anima Into Stopped Machine', buff 1728, 5 s each), so the buff's own timer and the end of the channel reach the Timeout together; taking the buff from ACTING is an atomic claim (`finish_effect_by_skill`, `schedule_effect`), the loser leaves the buff alone, and the Timeout runs once (before, a second `GainItem` or `NpcControlEffect` followed in about one channel in ten: `integration_tests::channel_and_target_buff_ending_together_fire_the_timeout_trigger_once`). A stopped / cancelled channel fires nothing (the C# granted the item then too). The caster-side `ChannelingBuffId` and buffs applied by skill effects (buff 1963 of 'Capture Crayfish') are unchanged: the latter expire on their own timer and fire Timeout already. |
| `Skill.ApplyEffects` area selection for `target_selection` 2 (Target) | `systems/skills/skill.rs` (`apply_effects`) | deviation (C# bug fixed) | The C# built an area skill's targets from `GetUnitsAroundRadius`, which excludes its origin, and re-added the primary target only for selection 1 (Source), so Target-selected area skills with `application_method` 1 effects (Archery 'Concussive Arrow' 11933: "damage to all enemies within 6 m", selection 2, relation Hostile, radius 6) hit nothing without a bystander. The Rust adds the primary target to the area units when it is a unit, before the relation filter; Source and position targets are unchanged. Test: `target_selected_area_skill_hits_the_primary_target` (modifier_flow_tests.rs). |

#### Raw attribute bounds and permanent pStats (2026-10-25)

`systems::skills`: all 21 `unit_attribute_limits` overrides are loaded with native
defaults of -2,000,000,000..2,000,000,000 (1.2 `39577A20`,
`D:/aa-re/client12/decomp/x2game12.c:1178985`, `:1179175-1179181`). The consumer
`398A0BC0` calls `395729F0` after formula, equipment and bonus arithmetic
(`:1991333-1991338`, clamp at `:1169345-1169352`), before raw-unit conversions.
The server preserves its existing bonus ordering and units and applies this final
raw bound to character, NPC and vehicle calculations and direct UnitBase readers
(XP and haste included). MoveSpeedMul starts at 1000, so its 3000 maximum means
3x speed; ExpMul starts at 100 percentage points and has range 0..500. Limits are
per-GameServer data shared by Arc with each unit, including detached refresh paths.
As a numeric-safety policy, infinities saturate, NaN uses zero within the range,
and NPC/vehicle integer bonus intermediates are widened before the final cap.
This does not claim native parity for extreme overflow or non-finite inputs.

The five `character_p_stat_limits` rows (0..10000) are loaded separately. Native
`39752FC0` loads max/min (`:1647826-1647832`), and `3939DD10` checks proposed
permanent pStat + delta (`:765713-765775`); they are **not final primary stat caps**.
Character still lacks permanent pStat contribution state/persistence, and
`world/unit_state.rs` emits six zero pStats. That feature remains open. Applying
the table to final character attributes or to NPC/mate/slave primaries would
invent a different rule. No storage, wire format or client/compact assets change.

#### AoE diminishing compatibility policy (2026-10-25)

`systems::skills`: `aoe_diminishings` loads authored float percentages by id (native 1.2
`sub_39620E90`, `D:/aa-re/client12/decomp/x2game12.c:1321108`). Searing Rain 11939 / plot 1
resets through plot-effect row 5680 (descriptor 2, event 246); next-event 288 repeats every
500 ms. Next-event 3195 fans event 3 out per target at speed 20 to flagged event 2723 / Damage
3172. Thus resetting on each child invocation would give every projectile the first rate.

The chosen **server compatibility policy is provisional, not proven retail parity**: each plot
instance keeps first-seen distinct-target ranks in existing execution order, shared across
sibling damage events. Misses and immunity occupy ranks; repeat hits reuse their wave rank.
Only flagged plot Damage effects participate. Reset clears only the calling plot's map.
Targets beyond the last authored id use its final rate; storage is limited to N identities,
where N is the last authored id (10 in 1.2). Rates apply after existing damage/modifier reductions to the
integer result, then truncate with minimum 1 for landed damage, as the native 1.2 diagnostic
`sub_39898330` describes (`x2game12.c:1984147-1984167`). Other damage, heals, buffs and CC retain
their existing paths, and wire formats are unchanged.

The native damage diagnostic (10.x `sub_39CECDB0`, parameter `+444`) is serialized debug data
supplied by the world server; the available client/zone plot dispatcher executes visual
effects, not the authoritative counter. Its producer is unavailable in these binaries.
`plots::aoe_tests::aoe_policy_*` covers the compatibility policy through plot/combat flows;
an authoritative native lifecycle experiment and live verification remain unperformed.

#### Aura and crowd buffs (data-inferred, capture-pending)

`systems::skills::aura`: the 1.2 `buffs` catalog has 85 `aura_radius > 0` rows (relations
Any 22, Friendly 18, Party 1, Raid 13, Hostile 28, Others 3) and 37 `crowd_buff_id > 0` rows,
but neither the 1.2 client (`x2game.dll.c`) nor the C# history contains an aura/crowd runtime;
the 10.x-only columns (`aura_creator_only`, `aura_max_count`, `crowd_check_buff_id`, ...) are
deliberately not implemented. The rules are a strong reading of the localized descriptions
(research `D:/aaemu-audits/gap-audit-2026-11-02/research/aura-crowd.md`), marked capture-pending
until a retail 1.2 zone server can be recorded:

* **AURA-1/AURA-2:** the slave buff (`aura_slave_buff_id`) is applied with the aura owner as
  caster to every unit within `aura_radius` that passes
  `relation::filter_with_relation(aura_relation_id, owner, ..)`; the owner is included for
  Any/Friendly/Party/Raid ("caster and allies").
* **AURA-3:** leaving the radius / failing the relation removes only the copy whose caster is
  the aura owner, so a separately cast copy survives. Every tick renews a current member that
  holds no live copy from any caster: natural expiry, a dispel, death cleanup, a caster that
  left, or a REFRESH stack-rule overwrite that moved the caster to another owner. It does not
  add while another caster's copy exists, so two owners do not overwrite each other every
  second; the remaining owner re-adds within one tick after the other leaves. Copies whose
  caster is not the aura owner are never removed by it.
* **AURA-4:** the driver is a 1000 ms enter/leave diff (no `on_moved` hook); the 1.2 update
  period is UNKNOWN.
* **AURA-5:** `aura_child_only` reaches the owner's attached characters (slave
  `attached_characters`, mate `passengers`) plus the owner's mount/vehicle (transform parent)
  inside the radius. The exact 1.2 attach scope is UNKNOWN.
* **AURA-6:** the owner losing the aura effect (timeout, dispel, death with `remove_on_death`,
  despawn) strips every recipient at once; a logout or a teleport to another instance is
  detected by the next 1 s tick, which strips the stored recipients without a radius query (a
  teleport then re-evaluates in the new position). A dead owner whose aura lacks
  `remove_on_death` keeps its aura (UNKNOWN).
* **AURA-7:** the loader warns once and skips the dangling slave 6668 -> 6669, the
  self-reference 1224 and the 15 aura rows with no slave buff.
* **AURA-8:** a registry entry stores the owner's `ObjectCore` identity and is dropped unless
  the same object still holds a live source effect. A logged-out owner whose object id is
  reused immediately, or a Started event that races a Removed, therefore cannot keep a ghost
  aura. A dead recipient (hp <= 0) does not get the slave buff re-added unless the slave
  template is `dead_applicable`; a resurrected unit in range gets it on the next tick.
* **CROWD-1..4:** every 1000 ms a source-buff carrier counts the OTHER units within
  `crowd_radius` that also carry the source buff and are friendly (`GetRelationStateTo` =
  Friendly and not attackable) and/or hostile (`CanAttack`), per `crowd_friendly` /
  `crowd_hostile`; at `crowd_number` the owner gets `crowd_buff_id` self-cast, below it the
  buff is removed. Whether 1.2 counts the owner, and which units count, is UNKNOWN (the 10.x
  check columns are that refinement).

Known limitations (evidence is insufficient for a code rule, left as-is):

* `aura_child_only` with relation 0 also reaches the owner itself (the aura target list adds
  the owner for Any); the data descriptions say "caster and passenger", so this is plausible
  but unproven.
* `Party` filters like `Any` in `relation::filter_with_relation` (mirrored C# quirk), so the
  relation-2 aura and the relation-0 auras also reach hostile units.
* One crowd buff shared by two source buffs on the same holder can flap if the sources
  disagree on the threshold (crowd 1857 "With Travel Companion" is reachable from many
  "Overburdened" ids); the last evaluated source wins for one tick.
* Long slave buffs 7115 / 1068 can outlive the owner across a relog because buff persistence
  keeps them and no aura owner is registered after the restore; the remaining effect lives at
  most its own duration (<= 60 s).
* `Entry.crowd_active` is bookkeeping only: removing and re-adding a timed crowd buff leaves a
  gap of up to one tick. The same applies to a timed aura slave that expires in range; the
  renewal happens on the next tick.
* The test client's `GameState.buffs` index map is not cleared on `SCUnitsRemoved`, so a
  reused obj id can inherit stale indices in a bot run (verification tooling only).

Tests: `systems::skills::aura::aura_tests` (relation matrix, enter/leave, every owner-loss
path, child-only, self-reference/dangling, crowd threshold with friendly-only counting,
separately cast copy) and the gated real-data test `real_compact_aura_and_crowd_catalogs`.
Live verification: two headless clients in a party, GM aura on A, B in radius receives
`SCBuffCreated`, B leaves -> `SCBuffRemoved`, A loses the aura -> B's copy removed.

#### Special effects added from data (2026-10-20)

These `SpecialType` bodies were TODO stubs that only logged in the C# (`git show a2ab80abd^:AAEmu.Game/Models/Game/Skills/Effects/SpecialEffects/...`); the behaviour below is derived from the 1.2 data because value1..4 are 0.

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `SpecialEffects/GenderTransfer` (special 107) | `systems/skills/effects/special/misc.rs` (`gender_transfer`) | added | Data: skill 23469 `성별 전환하기` (beauty-salon NPC interaction, `target_type` any unit, selection target). Flips the affected player (`target` if a player, else the caster) with `new = 3 - old` (1 male <-> 2 female), refuses when an equipped item template has `char_gender` set to a third value with `GENDER_TRANSFER_AS_GENDER_EQUIPMENT` (744), rebuilds `unit.model_id` from the race/gender `characters` template and broadcasts `SCCharacterGenderAndModelModifiedPacket` (existing beauty-salon packet) to the player and observers. `Character.Save` persists the new `gender` with the next save. |
| `SpecialEffects/MateMakeGetUp` (special 51) | `systems/vehicles/mates.rs` (`revive_mate`), `systems/skills/effects/special/misc.rs` | added | Data: skill 13719 `일으켜세우기` (revive an injured summoned mate, self-target, friendly); the former C# body only logged. Resolves the target/caster mate or, for a player caster, the active mate; only acts while `hp <= 0`. Reuses the stablehand revive values: `mates.hp/mp = MATE_FULL_POOL`, clears the summon item's `mate_dead` flag with a `RepairPets` item task and broadcasts `SCUnitPointsPacket`; the live mate is restored to `max_hp` / `max_mp`. |
| `AccountAttributeEffect` + `SpecialEffects/AuctionPostAuthority` (special 117) | `systems/economy/account_attributes.rs`, `bridges.rs`, `world/session.rs` | added | Data: `account_attribute_effects` has 3 rows, all `kind_id = 1` (auction posting authority): item 8000027 `경매장 등록권` adds it account-wide (`is_add`, `count` 0, `time` 0, `bind_world` false) and two `is_add = false` rows remove it (VIP buff trigger, removal ticket item 8000058). Special 117 on buffs 7149 / 11034 / 8000012 grants kind 1 permanently. New `account_attributes` table (PK account/kind/world, nullable `end_time`, `world_id` 0 = not world bound), loaded on character select into `server.systems.economy.account_attributes` and persisted on every change. `is_add` upserts, otherwise every grant of the kind is deleted; `time == 0` is permanent, else `now + time` **seconds (unit inferred: all 1.2 values are 0)**. `has_account_attribute(account_id, kind)` is ready for gating; **the auction is deliberately not gated yet** (every character currently gets buff 8000012 from `UNIT_STATE_FORCED_BUFFS`, so a gate would need that decision first). |
| `SpecialEffects/AddCharacterSlot` (special 91) | `systems/economy/character_slots.rs`, `db.rs`, `packets/c2g.rs`, `managers/character.rs` | added | Data: item 8000002 `캐릭터 슬롯확장 주문서` -> skill 8000002. Adds one `accounts.extra_character_slots` (`TINYINT UNSIGNED`, saturating at 255) for the caster's account, persists it and sends `SCGetSlotCountPacket` with the new value; `CSListCharacterPacket` sends that count instead of the old hard-coded 0. The character creation limit is the login-advertised base plus those purchased slots: aaemu-login sends `Login:MaxCharactersPerAccount` (default 6) in `ACJoinResponse`/`ACAuthResponse`, and `CharacterManager.Create` re-reads the account's slots inside the save lock and refuses with `CharacterCreateError::WorldLimit` (9) at `aaemu_commons::character_slots::character_slot_limit(base, extra)`; a MySQL error on that read falls back to the base alone. **Deviation (C#):** the C# `CharacterManager.Create` had no per-account character-count check and aaemu-login hard-coded 6 in both packets, so purchased slots could never be used. `Login:AdditionalCharactersPerServer` (the expandable limit) and `Login:PreSelectCharacterPeriod` (advertised in `ACJoinResponse.afs`) are Rust-only; the pre-select flag is accepted now and enables the lobby edit, and the game's `Characters:MaxPerAccount` / `Characters:PreSelectPeriod` must equal `Login:MaxCharactersPerAccount` / `Login:PreSelectCharacterPeriod` (documented pairs, no cross-process check; each server logs its effective values at startup). |

#### Honor gain / loss attributes and NPC-kill honor (WP-3, 2026-11-05)

`UnitAttribute` 126-135 (and `HonorPointLoseMul` 125) existed but were never read. Evidence:
`unit_modifiers` has 37 rows on those attributes (all type 0 `Value`), all carried by buffs; the
test buffs 6266/6267 "Honor test buff 1/2" put +100 on 128/130/132/134 and +100 on their `Mul`
twins and -100 on 125; buff 6303 (rice ball) has only `HonorPointLoseMul = -100` with the
description "honor points are protected ... effect is not lost on death"; buff 6442 "Akalife"
(+1 on 128 and 132) says "+1 honor per monster hunted; +10 at the end of a battlefield; +1 during
conflict/siege; +4 during war". The add-then-multiply order mirrors `LivingPointGain`
(`round(change + add)`, then `round(change * (mul + 100) / 100)`), clamped at 0 for gains.

| Area | Rust | Status | Notes |
|---|---|---|---|
| Honor source attribute pairs | `systems/skills/progress.rs` (`HonorSource`, `honor_gain`, `grant_honor_locked`, `grant_honor`) | added | NpcKill 128/129, Trial 130/131, War 132/133, Quest 134/135, BattleField 126/127. **Sources mapped by name and buff 6442's text (INFERRED):** 128 = monster hunt, 132 = conflict/siege/war PvP kill; 134 follows the Quest name. |
| Quest honor (`QuestActSupplyHonorPoint`, 73 real rows) | `systems/quests/hooks.rs` (`grant_quest_honor`), `bridges.rs`, `acts.rs` | added | `round(World.HonorRate * point)` then the Quest pair. `World.HonorRate` is the same rate `GiveHonorPoint` (special 100) already applies; it is read live from `server.config`, so a GM `honorrate` change applies on the next reward. Special 100 itself has no evidenced source and stays on the generic `change_game_points` path (UNCHANGED). |
| NPC-kill honor (`npcs.honor_point`) | `systems/npcs/npc/death.rs` (`before_death`) | added | 54 boss templates have `honor_point > 0` (2 x level; max 100); the value was loaded and never read (the C# also never used it). Each player with kill-exp eligibility (the `kill_exp_for` window, or the killer when nobody holds tagging rights) gets `round(World.HonorRate * honor_point)` through the NpcKill pair. **INFERRED** that the column is the killer's honor (name + values); **UNKNOWN** whether retail splits it in a party: no split here, conservative (the same eligibility as kill exp). TagShare contributors get no honor (they get no exp either). |
| PvP kill honor | `systems/skills/combat.rs` (`award_pvp_honor`) | added | Routed through the War pair. The per-state amounts (Conflict 10 solo / 6 + 4 assists, War 20 / 16 + 4) and `World.PvPHonorRate` are unchanged. |
| Battlefield end honor | `systems/instances/instant_game.rs` (`pay_awards`) | added | Routed through the BattleField pair. |
| War-zone death loss | `systems/skills/progress.rs` (`lose_honor_on_death`), `systems/skills/combat.rs` | added | The existing flat 10 is scaled by `(HonorPointLoseMul + 100) / 100`, clamped at 0 and at the current honor; the read and the debit happen under one character lock. Buff 6303 (`-100`) therefore removes the loss. **UNKNOWN** the base loss outside War; no new loss is added there (the C# had none; the conflict test asserts no victim packet). |
| Trial honor | `HonorSource::Trial` wired | unused | **UNKNOWN** which 1.2 flow grants Trial honor (jury pay or similar); no grant site exists in the current server, so the enum variant and its attribute pair stay unused (documented, not invented). |

Tests: `systems::integration_tests::{quest_honor_without_modifiers_is_unchanged,
quest_honor_applies_world_rate_and_the_quest_attributes,
boss_kill_grants_honor_point_times_honor_rate_to_every_eligible_killer,
npc_kill_honor_applies_the_npc_kill_attribute_pair,
honor_point_lose_mul_minus_100_removes_the_war_death_loss}`; the existing
`solo_pvp_kill_in_a_conflict_zone_awards_honor`, `war_zone_kill_splits_honor_with_assists_and_costs_the_victim_honor`
and `battlefield_end_awards_bonus_honor` are the no-modifier regressions.

#### Delayed `DisturbCasting` / `SCCastingDelayed` (2026-11-05)

`systems::skills`: 1.2 packet `SCCastingDelayed` 0x0a6 (S->C, reader `FUN_397ca570`,
`D:/aa-re/client12/out-x2game/x2game.dll.c`): `u16 skillId (0x40) ; u16 plotId (0x40) ; u32
delay (0x3c)`. The client handler `FUN_391d5d70` calls `FUN_39395760(skillId, delay)` (adds the
delay to the running cast of that skill id and rescales its animation) or
`FUN_3938c520(plotId, delay)` (the same for a plot cast). Special effect 5 (`DisturbCasting`)
has `value1` chance % and `value2` delay ms: 22 of its 24 rows have delay 0; the delayed rows
are 997 (10 %, 1000), 11220 (100 %, 100) and 14687 (100 %, 10). Joining `skill_effects ->
effects.actual_id -> special_effects`, only skill 12390 Low Kick reaches a delayed row (skill
effect 5134 -> effects 5134 -> special 997).

Rule now implemented: `delay > 0` and the victim runs a direct `Cast` task -> the task's
pending wait (`SkillTaskSlot.extend_ms`) grows by the delay; the armed cast timer takes it
under the slot lock and re-arms instead of completing, so the cast ends `delay` ms later.
`SCCastingDelayed { skillId, 0, delay }` goes to the victim. `delay == 0` keeps the immediate
interrupt. **Deviation from C#:** the C# `DisturbCasting` only cancelled the plot and never
sent `SCCastingDelayed`; the delayed interrupt this replaces was Rust-only
(`git show a2ab80abd^:AAEmu.Game/Models/Game/Skills/Effects/SpecialEffects/DisturbCasting.cs`).

Capture-pending (WP-1): who besides the victim receives `SCCastingDelayed` (the 1.2 handler
reads only the local cast, so viewers are not sent it; a player-controlled mount / pet cast has
no connection of its own and is not told); how the real server delays a plot cast (the client
uses plot id 0 for a direct cast, INFERRED from the two handler branches); and how a delayed
disturb affects a channel or plot cast. Neither has a 1.2 delay packet, so both keep today's
behaviour (the old delayed interrupt, with the liveness check), not an immediate interrupt and
not an extension. A delayed disturb on a unit with no cast, channel or plot now does nothing
instead of arming an interrupt that could hit a cast started during the delay.

Tests: `systems::skills::tests::cast_delay_tests` (extension, `SCCastingDelayed` and completion
at the new time, no-delay interrupt, cancelled-after-extension, delayed channel interrupt) and
`systems::skills::packets::tests::casting_delayed_layout` (independent literal bytes).

#### WP-9 decisions and housekeeping (2026-11-05)

Decisions from the 2026-11-02 combat-ai analysis (`WP-9`), no behaviour change:

* **Skill controller kinds 5 Rope, 6 Anchor, 8 Flowgraph.** The 1.2 `skill_controllers` table has
  8 Rope rows, 2 Anchor rows and 1 Flowgraph row (kind 7 Rotate, 8 rows, is implemented). The
  Rope rows are used by skills 12093 (hook throw), 13478 / 13749 / 19166 (harpoon launch) and
  13596 / 13750 / 19168 (cut rope); that family is handled by `systems/vehicles/harpoon.rs` +
  `CSSkillControllerState` (`skills/handlers.rs`), so the generic Rope controller adds nothing.
  The Anchor rows 7736 / 7739 belong to monster skills 22268 / 23103 "Land on the ground"
  (flying monsters, end skill 23437; the `value1..3` parameters are unknown). The Flowgraph row
  7907 belongs to skill 23969 `test_flowgraph_skillcontroller` (a test). Verdict: Rope done
  elsewhere, Flowgraph test-only, Anchor two monster skills (score 2/10); the expected
  "controller class not implemented" log is `debug` for these kinds in `controllers.rs`,
  `buffs.rs` and `effects/combat.rs`. `CSCreateSkillController` (0x08b) stays client-built and
  logged only, as in the C#. The controller ids the packets name do not exist in the 1.2 data:
  `skill_controllers` has rows only for kinds 1-8 (116/592/94/105/8/2/8/1), while the 31
  glider skills reference id 2687 and arrest 20323 / objection 21379 reference 6821, neither
  of which has a row; the 82 literal `'--- :null'` values are a data artefact
  (`round6-trial-effects-controllers.md` 3). There is no server-side controller to load, so
  log-only is correct.
* **Auto-attack flags.** `skills.start_autoattack` (313 rows), `stop_autoattack` (146) and
  `auto_fire` (26) are not read by the server. The 1.2 client loads `skills` itself and drives
  auto-attack by re-sending skill ids below 5000, which the server already repeats
  (`systems/skills/skill.rs`); whether the server must start / stop auto-attack from these
  flags is UNKNOWN, so this is **capture-pending (WP-9 AUTOATTACK-1)**: it needs a capture of
  the 1.2 client casting a `start_autoattack` skill (e.g. 10752 "Flower Cluster", `auto_fire`
  't'). No server rule is added before that.
* **Log-only special effects.** 63 `CombatText` (20 rows, all `value1..4` 0), 42
  `OpacityControl` (26 rows) and 76-78 (projectile, 41 rows) are client visuals with no server
  state; they stay log-only. The bot trio 84/88/89, 19 and 57/20/21 belong to their own work
  items.
* **skills-combat#15 leftovers.** `buff_unit_modifiers` (17 rows) is NOT just tooltip
  linkage: each row owns one `unit_modifiers` row with `owner_type='BuffUnitModifier'` and
  `owner_id = buff_unit_modifiers.id` (ids 9-35): conditional bonuses keyed by `tag_id` /
  `buff_id` (SpellDamageMul +10/+20 for tagged skills, MoveSpeedMul -800 for slave buff 6280
  with 6281, Melee/Ranged/SpellDamageMul +20 for buff 7647). The server does not load them
  (every loader filters `owner_type` exactly): a gameplay gap. A future loader must key them
  through `buff_unit_modifiers.owner_id` and never treat `owner_id` 9-35 as buff ids (buffs
  11, 15, 26, 31, 34, 35 exist). `skill_synergy_icons` and `tooltip_skill_effects` are client
  tooltip data, and `pcbang_buffs` is already handled by `systems/services/premium.rs`.

#### Resurrection buffs (WP-2b, 2026-11-05)

`buffs.resurrection_health` / `resurrection_mana` / `resurrection_percent` were loaded but
never read (the C# also only loaded them). The 1.2 `compact.sqlite3` has exactly one row with
either column set: buff 239 "Nui's Favor" (`resurrection_health 10`, `resurrection_mana 10`,
`resurrection_percent 't'`, 30 min, `remove_on_death 't'`), whose description says it
resurrects the character in place once with 10 % life / vitality for 30 minutes and recovers
the experience reduced on death (EVIDENCED: `buffs` row + description).

Rule now implemented (`systems::skills::combat::apply_resurrection_buff`,
`systems::skills::buffs::take_resurrection_buff`): on a PLAYER death after the death losses
and `DoDie`, an active buff with a non-zero `resurrection_*` is consumed (one critical
section under the owner's data lock; the effect then exits through the normal buff removal),
hp / mp are set to the columns (`resurrection_percent 't'`: percent of `max_hp` / `max_mp`;
`'f'`: absolute values — **INFERRED**, no 1.2 row uses it; both are clamped to the pools),
this death's experience loss is refunded through the `RecoverExpEffect` packet / `AddExp`
path with no priest and no labor, and `SCCharacterResurrectedPacket` is broadcast at the
corpse position followed by `SCUnitPointsPacket`. The death's `recoverable_exp` entry is
cleared so a priest cannot refund the same loss twice. The one-death-per-life claim is
unchanged: after the resurrection a second lethal hit without the buff dies normally.

**UNKNOWN / capture-pending:** the timing and animation of the self-resurrection (no 1.2
capture; no extra delay is added), and whether retail refunds the whole loss or only the
80 % `RecoverableExp` portion (the description names the decreased experience, so the whole
this-death loss is refunded; the audit's alternative reading is `RecoverableExp`). The
`remove_on_death 't'` flag on the buff is handled by consuming it explicitly before
`RemoveEffectsOnDeath`; a death that takes the C# killer-less path never reaches `DoDie` and
does not consume it.

Tests: `systems::skills::resurrection_buff_tests` (buff 239 in-place resurrection with hp /
mp / refund / packet order and buff removal, second death kills normally, death without the
buff unchanged, absolute-value inference, real-compact loader for the three columns).

### systems::npcs

**Deviation: heightmap u16 scale (2026-11-08).** C# `HeightMaxCoefficient = ushort.MaxValue / (MaxHeight / 4.0)` (64 units/m in main_world) saturated every terrain height above 1023.98 m, flattening mountain tops (88 of 25 299 main_world NPC spawns sat on the clamp; real terrain peaks at about 1250 m). Rust uses `ushort.MaxValue / MaxHeight` (16 units/m, ceiling 4096 m, `world_templates.rs`) and rounds instead of truncating when quantising. The source `heightmap.dat` heights are multiples of 0.05 m x `iStep` (`NodeCell::raw_data_to_height`), so 1/16 m loses at most about 3 cm. Gated test: `main_world_spawn_terrain_clamp`.

**Deviation: low-resolution heightmap nodes (2026-11-08).** The C# `NodeCell.UpScale` was replaced. Its `FindNearestSignificantPoints` divided by `nSize` and used `nSize / 33` as the source scale, so every 33 x 33 target interpolated only the node's raw points (0,0)..(1,1); `GetHeight` then indexed the upscaled 33-wide array with the wrong stride (the C# reused `nSize`). The original 1.2 client keeps the `size x size` samples (spacing `64 / (size - 1)` m) and bilinearly interpolates them at the 2 m grid positions with the upper index clamped to `size - 2` (`AABevy1.2/crates/aa-world/src/heightmap.rs:240-255`, `sample_node`). The Rust now does the same inside `GetHeight`, interpolating the node's own samples in f32 with no re-quantisation (the C# `UpScale` re-encoded the interpolated heights into the u16 h-dec grid). A 1 x 1 node, which the client treats as invalid and the 1.2 data does not contain, is our choice to cover the sector with its one height. The original-client recheck settled the disagreement: at the 12 audit points whose quad touches an n_size 2/5/9/17 sector node the original client matches Bevy within 0.066 m, while the C# rule put the server a median 10.6 m (max 25.6 m) off (`D:/aaemu-audits/terrain-3way-2026-11-08/REPORT.md` finding 2 and the recheck section, `recheck-results.csv`). After the change all 12 are within 0.0985 m of the original client; that remainder is the existing `rescale_to_int` truncation of each source sample (up to `i_step * 5 cm`, not a fixed 5 cm), and the f32 resampling adds no step of its own. Tests: `low_resolution_nodes_resample_like_the_client` (sizes 2/5/9/17 against the rule) and the gated `real_low_resolution_node_heights` (0.15 m bound for that truncation floor).

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `NpcManager.Load` (customs, body parts, templates, equip packs, postures, unit modifiers, initial buffs, merchants / goods) | `npc/data.rs` | done | 13 382 templates on the real DB. Every `merchants` row of an NPC is kept (`merchant_pack_ids`), as the 1.2 client unions the packs; the C# kept only the last one. Creatures.xml names, `LoadAiParams` not ported. Also loads `aggro_links` / `npc_aggro_links` (130 groups, 643 rows, 572 NPC templates, rows with a missing NPC / group skipped) into `NpcData::aggro_links`; the rule that reads them is `systems::ai` (see below). |
| `NpcManager.Create`, `LoadCustom` (.NET `Random(123456789)` parity), `Npc` stats (Str..KillExp from the unit formulas + template bonuses), `InitializeSpawnBuffs`, `AnimActionId` | `npc/manager.rs`, `npc/stats.rs` | done | Every real template creates without error. The AI is created through `server.systems.hooks.ai` (`systems::ai`). |
| NPC max HP/MP with `unit_modifiers` (`owner_type='Npc'`) | `npc/manager.rs` (`initialize_spawn_buffs`), `npc/stats.rs` (`apply_int`, `refresh_cached_stats`) | verified | `NpcManager.Create` adds every template `unit_modifiers` row as a static bonus (index 0, authored value) before `refresh_cached_stats` evaluates formula 31 (MaxHealth) / 32 (MaxMana) and applies the list; attributes 6 / 7, value and percent kinds both go through `apply_int`. The dungeon video (grade C, section 3) reproduces 32 boss/mob HP values to the digit only with these rows: Lerman 12971 275,480 -> 1,635,230 (+1,359,750), Sirothe 3,135,230, Duaxini 1,175,480, Carmila 1,655,500, Keeper 656,787, Tushan 19,930, Marmas / Sorvell 133,361, Carmila's Guard 113,439, and the rest of the table; mana (formula 32) matches wherever a full bar was visible. `linear_level_bonus` is a C# TODO and inert for the 1.2 NPC rows (all 0). Gated real-DB tests `real_compact_npc_max_hp_and_mp_include_unit_modifiers` (32 rows) and `real_compact_unit_modifier_rows_behind_the_pinned_values`. No code change. |
| `Npc` `LevelDps` `ab_level` binding | `npc/stats.rs` (`NpcStatCalc::eval`) | deviation | **Deviation from the C#:** `Npc.cs` passed `ab_level = 0` to every NPC unit formula and the port mirrored it, so `unit_formulas` id 29 `(floor((ab_level+2)/3)*24-8) * (npc_template*npc_kind*npc_grade)` (kind 12, owner 1) gave -8 for every NPC whatever its level (kind 13, id 30 `LevelMana` reads `ab_level` too, but it is never evaluated for NPCs). The same `LevelDps` feeds the `use_level_damage` damage term (`level_range`), `use_level_heal` heals and `use_level_value` mana effects: 518 of the 3676 `damage_effects` rows are `use_level_damage` and all carry `level_md > 0` (the research counts 3674 rows with `level_md > 0`), so NPC skill damage, heals and mana were near zero or negative. The Rust binds `ab_level` to the NPC level, like the character `LevelDps` (`skills::stats::CharStats`: `ab_level = level`), `skills::skill::ab_level` for non-player units and the mate getters. Evidence: the 1.2 formula is a level step (the same `floor((L+2)/3)` shape as the `dps_inc` rows), the other owner types' kind-12 formulas are level steps too, and a level-50 grade-1 NPC then gets 400 (character level 50: 264) instead of -8. The 10.x per-unit binder was not located (research `npc-ab-level.md` under `D:/aaemu-audits/gap-audit-2026-11-02/research/`), so the binding is inferred from the data shape and the character path, not captured. The slave `LevelDps` getter (`vehicles/stats.rs`) binds `slaves.level` for the same reason (owner 2, id 46 `(ab_level + 1) ^ 1.2 + 5`); `slaves.level` is a real 1..50 template level; no slave mount skill uses level damage today (all 26 slave damage effects are fixed), so this is currently inert for vehicle weapons. |
| `Npc.DoDie` | `npc/death.rs` | done | Kill exp (team eligibility, `PlayerExpModifier`, mate exp, TagShare contributors), quest kill events, loot (`LootManager.GenerateLoot` through the economy), aggro / tagger clean-up, spawner despawn, `Ai.GoToDead()` (`systems::ai`). |
| `KillNpcWithoutCorpseEffect` scripted removal | `npcs/bridges.rs` (`kill_npc_without_corpse`, `remove_effects_and_delete`), `npc/death.rs` (`claim_scripted_removal`, `credit_scripted_kill`), `skills/skill.rs` (`nearest_kill_npc`) | added (C# PR #1792) | The removal without a corpse (832 `kill_npc_without_corpse_effects` rows) now raises the same monster-hunt quest events a normal death raises (MonsterHunt, MonsterGroupHunt, ZoneKill) for the players a normal death credits (tag / loot-rights holders and their party, Tag Share contributors; the skill caster when nobody holds tagging rights), with no exp, loot or honor. C# #1792 calls only `DoOnMonsterHuntEvents`, so the KILL_NPC achievement, the attacker's `OnKill`, the instance `OnUnitKilled` and `GuardNpcDied` are not raised; `Vanish` gives no credit. The removal is real: a spawner-owned NPC goes through `DoDespawn` / `DespawnWithRespawn` (C# parity) and a spawner-less NPC is deleted from the world (deviation: the C#/PR only removed spawner-owned NPCs, leaving a credited summon alive). `UnitBase::scripted_removed` claims the life before the removal under the NPC data lock; damage does not clear it (unlike `death_processed`), so a damage hit between the claim and the removal, or on a stale handle after it, cannot run the death path or credit again; a direct `DoDie` on it is a quiet no-op. A respawned NPC is a new object with a fresh life. Player-cast retreats (Rust-only: the C# effect is a no-op for a character caster) remove the matching-template NPC nearest the caster, restricted to the effect radius when it has one, and none when none qualifies. **Open issue:** the Nymph Queen encounter reset script is 10.x content (1.2 has no Nepheroda NPC and no skill uses effect 33617); the ordinary world spawner already respawns Lympheroda 9385 after its 15 s `RespawnTime` (`despawn_with_respawn` + `reconcile_deleted_npc`), so it is not ported. |
| NPC interaction packets (CSStartInteraction, CSInteractNPC(End), CSSelectInteractionEx; SCNpcInteractionSkillList, SCNpcInteractionStatusChanged, SCNpcInteractionEndedByZone, SCAiAggro, SCNpcChatMessage, SCUnitModelPostureChanged, SCAggroTargetChanged) | `npc/packets.rs` | done | Byte-exact tests (the two interaction-state layouts from the 1.2 readers `FUN_397b4b00` / `FUN_397b4aa0`). `CSStartInteractionPacket` is refused silently for an NPC whose `interactable` flag is off (see the WP-5b note under "systems::ai") and for one farther than the 1.2 client's 3.0 m interaction-window reach plus `Interaction.NpcReachToleranceMeters` (default 2.0, INFERRED slack; `Interaction.NpcReachCheck`, default true); the C# had neither guard. Merchant / bank / auction UIs are economy's. **Deviation:** `CSStartInteractionPacket` also answers a slave with one seat skill, because the 1.2 client acts on the first list entry (`FUN_393be850` / `FUN_393be280`): 12076 (occupy) while the driver seat is free, else 13863 (passenger seat, `special_effects` 2378 value1 2) while point 2 is free, the slave's model declares point 2 in `model_bindings` and the slave is not a cannon; a slave with both seats taken keeps 12076; the bind then ignores the taken seat silently (C# behaviour). The C# answered only NPCs and mates (12077). `CSSelectInteractionExPacket` (0x06b) is the client world-interaction-window selection (`FUN_393b3120` logs `Interaction Ex Selected`); the doodad interaction effects run through the skill `Interaction` effect, so it stays a debug log. Whether the server must also act on the selected id / `var1` is capture-pending (`INTERACTION-EX-1`). |
| `NpcGameData` (npc_spawners, npc_spawner_npcs, member index, generated spawners) | `npc/spawner_data.rs` | done | |
| `NpcGroupGameData` (`npc_groups`, `npc_group_members`) | `npc/group_data.rs` | done | 158 groups / 571 member rows on the real DB; every one of the 147 `NpcGroup` spawner rows resolves to a group whose 531 member NPC templates exist. A `"NpcGroup"` spawner member expands into all group members at the spawner position plus `formation_offset_*` (see deviations). |
| `NpcSpawner`, `NpcSpawnerNpc`, `DoodadSpawner`, `Spawner<T>` | `npc/spawner.rs`, `doodad/spawner.rs` | done | Schedules / time windows / population caches, respawn + despawn timing, exact-identity reconciliation of direct deletes (see deviations). Ordinary populations keep the authored `Count` / `min_population` baseline (default 1) and per-slot respawn deadlines (see deviations). AI home position / follow path / simulation through `server.systems.hooks.ai` (`systems::ai`). |
| `SpawnManager` (one per instance) + `WorldManager.ActiveRegionTick` spawner part | `spawn_manager.rs` | done | main_world: 25 118 NPC entries -> 24 047 ordinary placements / 38 174 spawner clones (1 011 duplicates and 60 event entries filtered, 0 missing templates), 42 649 doodads, 69 transfers, 14 gimmicks, 5 slaves, 0 parse errors. Ordinary spawning is a bounded per-instance 64 m region-index activation pass (`World.NpcPrewarmRegionMargin` / `World.NpcSpawnTickMs` / `World.NpcSpawnsPerTick`) with warm retention and resident-service retention (`World.NpcWarmRetentionSeconds` / `World.NpcResidentServiceNpcs`; see deviations). Persistent doodads from MySQL. Transfers / slaves: `systems::vehicles`; houses and their doodads: `systems::housing`; trial court doodads / NPCs linked after the spawns (`TrialManager.Initialize*`). |
| `DoodadManager.Load` (templates, func groups, funcs, phase funcs, all 121 func template tables) | `doodad/data.rs`, `doodad/templates.rs` | done | 7 124 templates, 13 974 funcs, 24 465 phase funcs (counts match the DB). |
| `Doodad` phase machine (`Use`, `DoFunc`, `DoPhaseFuncs`, `DoChangePhase`, `InitDoodad`, `OnSkillHit`, `FuncGroupId` setter, ToD triggers), 42 phase funcs, func tasks, `DoodadManager` runtime (Create, CreatePlayerDoodad, ChangeDoodadData, coffers), persistence (`doodads` table) | `doodad/{phase,phase_funcs,tasks,manager,db,logic}.rs` | partial | Serialized interactions, per-object counters, generation and handle guards for timers, ToD transitions, ordered MySQL persistence. Authored immediate logic/family pulses, spawn management groups and moving gimmicks are implemented. FlowGraph runs in the client. Logic delay descriptors 2/3 lack timing semantics; spawn groups 73–76 lack authored membership in shipped data. |
| `blocked_child_doodads` (buff id -> child doodad template id) | `systems/skills/data/mod.rs` (loader), `doodad/{phase,interactions,packets}.rs`, `npcs/{bridges,portal/mod}.rs`, `economy/store.rs` (use entry guards) | added | The C# did not load this table; the 1.2 client does (`dll.c` `39a0c158` / `39a0c460`: `SELECT owner_type, owner_id, doodad_id FROM blocked_child_doodads`). The two rows are buff 6064 -> doodad 6560 (fishing boat crane) and buff 6245 -> doodad 3446 (backpack storage box). While such a buff is active on the child doodad's parent unit, a player's use is refused with the existing `NO_INTERACTION_AVAILABLE` error (no new packet) and `Character.SkillCancelled`, so a running cast refunds its item / labor cost. Only `owner_type = 'Buff'` is handled (the only evidenced value; other / NULL values are skipped at debug, NULL ids too). Covered player entries: `Doodad.Use` (skills `InteractionEffect` / `ScopedFEffect`, `CSHang`, harpoon, loot bag; `funcs::use_func` is reached through it), world interactions (`execute_interaction_with`, incl. the `CSLootOpenBag` recover path and `CheckGrowth`), `CSChangeDoodadPhase`, the `CSBuyCoinItem` and `CSBuyItems` purchase paths, and navi open / teleport. Doodads without a parent are unchanged and internal phase changes (timers, ToD, func chaining, instance events, GM phase commands, crime-evidence loot) are not gated. Who casts 6064 / 6245 in the live flow is UNKNOWN. Tests: `blocked_child_tests` (refused with the buff, allowed without / unrelated buff, parentless doodad) and gated `blocked_child_doodads_real_compact_db`. |
| 79 non-phase funcs, 41 world interactions (`execute_interaction` for the skills InteractionEffect), coffers, bond / seats | `doodad/funcs/`, `doodad/interactions.rs`, `doodad/coffer.rs` | done | Items, loot, coffers through the economy `EconomyBridge`; skills, quests, housing through the bridges; instance funcs (`systems::instances`), `BindSlave` (`systems::vehicles`). Sticky parents are not modelled. The funcs that stay trace-only (`DoodadFuncBankUi` / `_AuctionUi` / `_StoreUi` / `_CofferPerm` / `_RenewItem`, `Climb`, `CraftStart/Act/GetItem/Cancel/Info/Pack`, `InsertCounter`, `StampMaker`, `UccImprint`) are **client-driven**: the 1.2 client loads `doodad_func_bank_uis`, `_auction_uis`, `_store_uis`, `_coffer_perms` and `_renew_items` itself (`strings.txt:13498-13557`), so there is no server answer to add (do not reopen). |
| `doodad_funcs.perm_id` (`DoodadFuncPermission`) | `doodad/permission.rs`, `doodad/phase.rs`, `doodad/interactions.rs`, `doodad/funcs/skills.rs`, `housing/dominion/larceny.rs`, `permission_tests.rs` | added | The C# read `Doodad.FuncPermission` only in `CharacterCraft` (its enum was a guess) and never enforced it in `Doodad.Use`; the 1.2 client filters every interaction entry by `FUN_393b3300(func->perm, doodad, ...)`, so a direct `CSStartSkill` / `CSLootOpenBag` bypassed the filter. Decoded and enforced: 0 any (13 228 rows), 1 owner, 2 owner or family, 3 expedition membership with the role right (`FUN_392f8af0`: leader `0xff` or the role policy's `siege_master` byte) and the doodad's expedition (`+0x350`) is mine or 0, with **no owner shortcut**, 4 owner or party/raid, 5 owner or raid, 6 same account, 7 the doodad's zone **group** is owned by my player-founded nation (dominion owner expedition through `FUN_392f8180`; `zones.faction_id` is not read), 8 a house of mine is in the doodad's zone **group**, other values deny (client default). Evidence: the data patterns (territory statues 7229-7234 and QA doodads 7251 "이 존이 내 국가" (perm 7) / 7252 "이 존에 내 집" (perm 8), beds/fences/livestock perm 1, the single perm-2 QA uproot, the perm-3 siege-declaration backpack, perm-5 storage box / fish crane, perm-6 farm bench / cushions), the decompile (`FUN_393b3300`; family map `FUN_39302aa0`/`FUN_39300340`, expedition role policy `FUN_392f8af0`/`FUN_398a40c0`, raid `FUN_39308dd0`, account `FUN_393b32c0`, zone nation `FUN_39053360`/`FUN_392fa6b0`/`FUN_392f3510`/`FUN_3983a730`/`FUN_392f8180`, zone house `FUN_3932aaf0`/`FUN_397a6c60`) and the C# history (`git log -S Perm`: only `CharacterCraft`). Deviation: the C# enum's names are wrong (3 is expedition, not owner-only; 1 is owner-only) and value 7 was missing. The owner is the owning character (CHARACTER owner, house owner, slave owner). `perm 0` is not an owner rule: the shipped player crop groups carry perm 0 on water 13625 and uproot 13789 (98 groups bind both), so a house garden stays behind the house permission (unchanged) and a public farm behind the guard time (unchanged). Gate: `phase::use_doodad` (the skill-bound func before `do_func`, the skill-less loot rows one by one) plus the direct `CheckGrowth` and `RecoverItem` branches; a refusal sends `interaction_permission_deny` 232 and cancels the cast so its cost is refunded. Siege larceny: the window never grants the uproot skill 13789 (produce only), and the `funcs::skills::use_` window fallback has the same exclusion; most group-5 (baby livestock) and 4594 rows are perm 1, so the window does not grant them either, matching the client filter. **perm 7 / EuAuroria:** with player nations off no player passes `FUN_392fa6b0`, so the territory-statue buffs (statues 7229-7234, buffs 23681 and 23684-23688) are unusable under that profile; that is the client rule, not a regression. **SYSTEM-owned rows (public-farm release):** `PublicFarmTick` turns an expired public-farm crop into owner type SYSTEM; its perm-1 rows (livestock group 5 care/loot among them) then refuse everyone, and the client hides the same rows because its owner lookup returns null. **perm 3 inference:** the client's doodad `+0x350` is the field `Doodad.Write` sends as 0 (C# `stream.Write(0); // family TODO`), so the server resolves it from the dominion declaration's attacker expedition (the only perm-3 row is the camp 4307 recover and the camp is that declaration's HQ); the policy byte at `+0x90` is `siege_master` (decoded from `FUN_392faa00`'s column mapping: `expedition_management` ALB columns 6 = join_siege at `+0x91`, 7 = siege_master at `+0x90`). A chained loot iteration (an earlier row already ran and advanced the phase) skips a denied row silently instead of refusing and refunding a finished cast. Tests: `doodad/permission_tests` (every value, owner/family/guild-role-right/team/raid/account/nation/zone-resident, system- and slave-owned doodads, uproot vs water, the 232 + cast cancel, no owner shortcut for perm 3, the territory-owner nation, the chained skip, the direct recover and CheckGrowth paths, house permission on top of perm 0) and `dominion/larceny_tests::the_window_grants_produce_but_not_the_uproot`; each denial test was mutation-checked against the disabled/old rule. |
| SCDoodad* packets, doodad C->S packets (CSChangeDoodadData/Phase, CSCreateDoodad, CSSetLogicDoodad, CSSaveDoodadUccString, CSUnbondDoodad, CSCofferInteraction) | `doodad/packets.rs`, `doodad/coffer.rs` | done | Byte-exact tests. `SCWorldInteractionCanceledPacket` 0x07f (`bc unitObjId, u32 doodadObjId`, reader `FUN_397c6540`) is defined and byte-tested but never sent: its trigger is capture-pending (CAP-INT-1). `SCDoodadPhaseMsgPacket` 0x157 (`bc doodadObjId, u32 funcGroupId`, reader `FUN_397b6460`) is sent on an authored `phase_msg` change (WP-UI1, see the deviations). |
| `CSCreateDoodadPacket` placement checks / 1.2 seed-builder rules | `doodad/manager.rs` (`handle_create_doodad`, `create_player_doodad`, `create_player_doodad_server_spawned`), `doodad/placement.rs` | deviation | **Deviation:** the C# checked only the source item, public farm (`CanPlace`), house permission and labor, and several refusals were silent. The Rust applies the 1.2 client's seed-builder validation (`FUN_393a8fa0`; research `s3-farm-plant-harvest.md`, capture `2026-10-30-capture-farm-cinema.md`) to the one planting entry path, `CSCreateDoodadPacket` (`handle_create_doodad` -> checked `create_player_doodad`), through `placement::refusal`. The fake-trial GM command and crime evidence (bloodstains / footprints) spawn server-side doodads and use `create_player_doodad_server_spawned` through `SocialHooks::create_player_doodad`, so they are not planting paths and are not validated; item-use backpacks (`spawn_backpack_doodad`) and skill `SpawnDoodad` effects are also not plantings. Rules, in `FUN_393a8fa0` order: (1) `restrict_zone_id` mismatch -> `DOODAD_SIM_AREA` 276 (F11: the client shows "Can't be installed here."); (2) water: group 40 (marine crops) must stand in water and every other group refuses it, `DOODAD_SIM_AREA` 276 — the server's `WorldInstance.IsWater` with the client PR's `surface > z + 0.3 m` margin (the decompiled `worldvtbl+0xf4` test and F5 were not decoded: capture-pending); (3) range: at most 30 m in 3D, `TOO_FAR_AWAY` 154 — `FUN_393a8fa0` (~761319) refuses only `900.0 < d²`, so exactly 30 m is allowed (F1's screen-projected sweep cannot pin the boundary). The server measures from its own player position, which lags a moving client; the code base has no placement-range allowance (the 15 m `DOODAD_RANGE_ALLOWANCE` in `systems::skills` covers cast targets, not placement), so none is added; (4) doodads: sphere test `dist3D < r_new + r_other` only when both are round (`FUN_3983b360`: groups 2, 3, 5, 12, 40, 80, template 1219, group 38), radius `max(sim_radius / 1000, 0.1)` m, `DOODAD_SIM_TOO_CLOSE_TO_OTHER_DOODAD` 277. Every mixed/non-round pair uses oriented boxes (`FUN_390318f0` / `FUN_39030ae0`) that are not decoded, and non-round units use a physics collider (`FUN_39180d50`), so those pairs are skipped: the server is permissive, never stricter than the client (real group 72 plantables 5287–5290 / 6591 with 3 m and group 69 3596 with 2 m stay placeable); (5) round doodads against units inside their radius -> `DOODAD_SIM_TOO_CLOSE_TO_OTHER_UNIT` 278; (6) dominion: only item template 29800 ("Blueprint: Territory Statue", `DAT_39ac774c`) is gated (step 7, `dll.c:761340-761378`). The spot must be in a non-zero zone group whose dominion record is owned by the caster's own expedition (not the nation king) and lies within the live tower's 2D `radius_dominion` (`guard_tower_settings.radius_dominion`, the `+0xa0` short; the field order is INFERRED from `DominionTerritoryData`'s wire order), else `LOCATE_INVALID_DOMINION_AREA` 749; every other item skips the gate and the refusal answers before the seed is consumed. Silent drops answer `FAILED_TO_USE_ITEM` 207 for a missing / foreign item or an unknown template — **inference**, not a capture: the 1.2 client uses the same code when a seed use is cancelled, but no server->client answer was captured (a zone-banned item keeps its 440 `ITEM_CANNOT_USE_HERE`). Malformed positions (non-finite / scale outside 0..10) stay silent: they are anti-cheat guards, not a 1.2 client path. Open client steps not implemented: the ±6 m footprint height spread (step 2, `FUN_393a7720`; skipped for groups 2, 3, 5, 12, 37, 40, 80 — footprint sampling undecoded and F7 not driven); the house/farm area rule of step 0 (the server keeps its own house/public-farm checks); the area-box test `FUN_393287c0` (0x114); the overlap skip list `FUN_397a7cc0` / `FUN_397a7ce0` (it compares four template pointers whose ids are not initialized in the decompile); and the farm-plot radius call `FUN_3983b280(..,1)` (the decompiled body ignores the second argument). Research `re-placement-fakeuse.md` sections D1b-D1d records the rest as below the bug bar or blocked: D1b (a doodad cube straddling a shrunken house box, 276) needs the render-model z box and is largely covered by `house_at_location_in_world` + `allowed_to_interact` answering `NO_PERM` for a spot inside a house (score 2, worker-ready NO); D1c (±6 m footprint spread, 279, plus the z snap to the minimum) needs the model local bbox (score 3, worker-ready NO); D1d's terrain material (place skin) rule is implemented behind the default-off `Housing.PlacementStrictSurface` (see the D1d place-skin section below); the kind-4 reachability and the border sample are INFERRED. The hard-coded farm zones vs `common_farms` (S5) belong to slice 11 and are unchanged. Concurrency (Greptile P1 on #25): the final check and the doodad's insertion into the grid are one critical section under `WorldInstance::placement_guard`, so two plantings at the same empty spot cannot both pass the overlap check; the seed is consumed only after the insert succeeds and a failed consumption deletes the doodad again. **Deviation:** C# consumed the seed before `Spawn()`, which left the check-then-spawn window open; the refusal error and the economy callbacks run after the guard is dropped. |
| Heightmaps (`WorldCell` heightmap.dat, `GetHeight`, `GetReferenceHeight`), NPC reference height in `SCUnitStatePacket` | `heightmap.rs`, `world/unit_state.rs` | done | Lazy per cell (`PreLoadTerrain`); with `GeoDataMode` the navmesh height of `systems::ai::geodata` comes first. Median error 0.098 m vs 4 002 real spawn Z values. Used by the skill controllers / displacements, plots and NPC spawns. Low-resolution sector nodes (n_size 2/5/9/17) are resampled with the original client's bilinear rule instead of the C# `UpScale` quirk (see the 2026-11-08 low-resolution-node deviation below). |
| `GimmickManager`, `GimmickGameData`, `GimmickSpawner`, movement handlers | `gimmick/` | done | |
| `AreaTriggerManager` (+ aoe_shapes, wi_group_wis), `SubZoneManager`, `SphereQuestManager` (world side) | `area_trigger.rs`, `subzone.rs`, `sphere_quest.rs` | done | Quest effects via `QuestsBridge`. Area membership compares entity identity, so reconnecting with a reused object ID leaves the retained old session and enters the new one; this corrects the mirrored C# ID-only comparison and restores active school auras. Unchanged entities and overlapping aura counts are retained. Sub-zone index scan of the pak is slow in debug (~25 s). |
| `PortalManager` (recall, district return, summoned portals, portal book, teleports) | `portal/` | done | Trial court check (`systems::social`), mates / slaves on instance change (`systems::vehicles`). |
| `PublicFarmManager`, `FishSchoolManager`, `GameScheduleManager` (+ NCrontab subset) | `public_farm.rs`, `fish_school.rs`, `schedule.rs` | done | `SpecialtyManager` lives in `systems/economy/specialty.rs`; the npcs duplicate was removed (2026-11-09, see the economy specialty note). |
| `SCScheduledEventStartedPacket` / `SCScheduledEventEndedPacket` (0x1ec / 0x1ed), `Events:ScheduledEvents` | `schedule_events.rs`, `config.rs` (`Events:ScheduledEvents`), `npcs/mod.rs` (start), `world/session.rs` (one call) | added | The C# never sent them (no sender at `a2ab80abd^`; only the opcode names existed). Wire from the 1.2 readers: Started `FUN_397b01f0` -> `FUN_398a7ed0` / `FUN_398a7d80` = `u32 id; 21 x i32; i32 changes; u8 priority; u8 startDate; u8 periodDate; u8 hour; u8 min; i32 periodMin; u8 weekDay; str openMsg` (fixed 102 bytes); Ended `FUN_397b0260` = `u32 id; str closeMsg`; both strings max 0x7f bytes. Handlers `FUN_391e1e10` (`OnScheduledEventStarted`, UI event 0x256) and `FUN_391e1f30` -> `FUN_3930b940` (`OnScheduledEventEnded`, UI event 0x257, only for the stored active id); the client keeps **one** active event id. **INFERRED** trigger and text (no original-server capture): a `game_schedules` window edge using the existing `schedule::check_data`, evaluated every 30 s over the configured allow-list; Started on the opening edge, Ended on the closing edge, Started to a player entering the world while the event is active. The first tick after startup seeds silently, so a restart does not re-announce running events. The `values` / `changes` / priority / date fields are sent as zeros (**INFERRED**: the handler stores them but only the message is displayed; the client constructors set priority 0x100 on their own copies). Texts come from the configuration (the shipped `game_schedules.name` values are Korean). Default off, empty list; unknown ids and duplicates are skipped at startup with a warning (the first duplicate wins), overlength messages are truncated on a UTF-8 boundary. Tests: byte-exact both packets, opening/closing edges to two characters, silent restart seed, late join, disabled/reset. |
| `TimeManager.OnTimeOfDayChange` (NPC postures, doodad ToD triggers) | `doodad/manager.rs` | done | 10 s poller started by the doodad part (TimeManager has no callback). |
| Cross-subsystem behaviour | `hooks.rs`, `bridges.rs` | done | `SkillsBridge` (skills), `EconomyBridge` (economy), `QuestsBridge` (quests) in `server.systems.npcs.hooks`; this subsystem implements `NpcSkillHooks`, `NpcEconomyHooks`, `LootNpcHooks`, `TreasureMapSource` (`bridges.rs`). See "Phase 3b wiring". |

**NPC faction on the wire (verified 2026-11-02, owner session).** `NpcManager.Create` sends
`npcs.faction_id` unchanged (`npc/manager.rs` `create`, `world/unit_state.rs` `write_net_unit`
faction pisc). The 1.2 client colours the target frame from the relation of that faction to the
local player: `x2ui/unitframe/unitframe.alb` `ChangeHpBarTexture_forNpc` chooses
`S_HP_HOSTILE` / `S_HP_NEUTRAL` / `S_HP_FRIENDLY` from `X2Unit:GetCombatRelationshipStr`
(`FUN_394c0d80` -> `FUN_39028240` -> `FUN_39838880`), whose relation follows the mother chain
(`FUN_398a37a0` walks `faction+0x20`) and the `system_faction_relations` states 1 hostile /
2 neutral / 3 friendly. NPC name plates are yellow (only players are green), so a yellow name
is not a relation. The owner-session NPCs are therefore 1.2-correct as sent: Amutar (npc 3443,
`faction_id` 2 neutral) has an orange target frame and no attack permission; Mutani (5772,
faction 109, the Harani player's own faction) and the guide Lucius (12078, faction 1) have
green target frames; the Harani player Aouj (character 38) is faction 109. Locked by
`world::unit_state_tests::real_compact_db_npc_unit_state_faction`,
`systems::skills::relation::tests::harani_player_relation_mapping` and
`systems::skills::relation::tests::real_compact_db_faction_relation_for_harani_player`.

### systems::economy

Layering: `models::item_task` (`ItemTask`, `ItemTaskType`, recorded effects `ItemFx`) →
`models::container` (container operations, pure, under the container locks) →
`systems::economy::inventory` (`ItemTxn`: economy operation lock + effect flush in C# order,
`Inv`, `Out` queue for character packets) → feature modules. Other subsystems plug in through
hook slots (`server.systems.economy.hooks.{quests,gear,progression,npcs,summons}`,
`loot.hooks`, `craft.hooks`, `mail.hooks.housing`, `cash_shop.{quests,db}`; internal slots are
connected by `systems::economy::wiring`).

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `Models/Game/Items/Actions/*` (every `ItemTask`, `ItemAction`, `ItemTaskType`), `SCItemTaskSuccessPacket`, `SCSyncItemLifespanPacket`, `SCUnitEquipmentsChangedPacket`, `SCInvenExpandedPacket`, `SCGamePointChangedPacket`, `SCCharacterLaborPowerChangedPacket` | `models/item_task.rs`, `systems/economy/item_packets.rs` | done | Tasks snapshot the item when created (the C# encodes on `SendPacket`). |
| `ItemContainer`, `EquipmentContainer`, `MateEquipmentContainer`, `CofferContainer` | `models/container.rs` | done | `AddOrMoveExistingItem` (new / moved / same container), `RemoveItem`, `ConsumeItem`, `AcquireDefaultItem(Ex)` (timers, charges, crafter in `crafter_id`; older rows / non-craft uses keep `made_unit_id`, see "Item crafter"), `SpaceLeftForItem`, `ApplyBindRules`, `Wipe`, `ReNumberSlots`, coffer `CanAccept`, `OnEnter/LeaveContainer` events. |
| `Inventory` (SplitOrMoveItem(Ex), 2H weapon handling, backpacks, ExpandSlot, coffers, ConsumeItem, counts), `Character` money / game points / labor / `ItemUse` / `SendErrorMessage`, `ErrorMessageType` | `systems/economy/inventory.rs`, `error_message.rs` | done | `ChangeLabor`'s `AddExp` is a hook (`ProgressionHooks`). Equip and item use apply `items.actability_requirement` (error 508, see "Item use gates from data"). |
| Core item C2G (`CSSwapItems`, `CSSplitBagItem`, `CSDestroyItem`, `CSExpandSlots`, `CSDeposit/WithdrawMoney`, `CSSplit/SwapCofferItem(s)`, `CSItem(Un)Secure`, `CSEquipments(Un)Secure`, `CSThisTimeUnpackItem`) | `systems/economy/{item_handlers,item_security}.rs` | done | Deposit / withdraw require a positive amount and a banker NPC (the `banker` template flag) within 5 m of the current interaction object (the C# did neither). The four secure packets are implemented from the 1.2 client (the C# only logged them); see the item-lock deviation. |
| `ItemManager` (all templates / tables, user items, `Create`, timers, `UnwrapItem`, auction search helpers, loot convert fish), `ItemGameData`, `ItemConversionGameData`, `FishDetailsGameData`, item model stats (durability, repair cost, attributes, DPS, armor), procs | `data/items.rs`, `managers/item.rs`, `models/item.rs`, `systems/economy/{items_data,item_timers,procs}.rs` | done | `ItemManager` lists are leaf locks (snapshot before locking containers). Procs are a new feature (the C# never fired one; see the item-procs deviation). |
| `LootGameData`, `Models/Game/Items/Loots/**`, `LootingContainer`, loot packets | `systems/economy/{loot_data,loot,looting,loot_packets}.rs` | done | NPC death / despawn / team rules through `LootNpcHooks`, `LootTeamHooks`, `LootQuestHooks`. **C# PR #1488 (2026-11-08):** a loot row whose item template is missing (185 of the 13 823 1.2 `loots` rows) is skipped at debug level instead of aborting the pack, on the corpse path (`generate_pack_v2` -> `retain_known_items`), the shared player-pack path (`generate_pack_for_player`: item use, `DoodadFuncLootPack`, fishing) and `give_loot_pack`; a quest reward item without a template (282 `quest_act_supply_items` rows) is filtered out of the reward pool once in `distribute_rewards` so it no longer counts against the free-slot check (the rest of the items, exp and copper still pay out), and the fish conversion lottery filters such rows before its roll instead of building an `Item` with an unknown template. |
| Repair (`Character.DoRepair`, `CSRepairSingleEquipment`, `CSRepairAllEquipments`), durability loss | `systems/economy/repair.rs` | done | Combat calls `on_damage_taken` / `on_death`. |
| `CraftManager`, `Models/Game/Crafts`, `CharacterCraft`, `CraftTask`, `CSExecuteCraft`, `SCCraftFailed`, recipe learning (`item_recipes`, `train_craft_effects`) | `systems/economy/{craft,craft_data,craft_learning}.rs` | done | Cast through `CraftHooks::start_craft_cast`; skills call `craft::end_craft`. `TrainCraftEffect` teaches `item_recipes[source item]` (or the effect's craft without a source item), persisted in `character_crafts`; `Crafting.RequireLearnedRecipes` (default off) gates execution. |
| Stores (`CSBuyItems`, `CSSellItems`, buy-back, `CSListSoldItem`, `CSBuyPriestBuff`, `CSBuyCoinItem`, `CSSellBackpackGoods`), `SpecialtyManager`, `TaxationsManager`, `ExpandExpert`, item look conversion | `systems/economy/{store,store_data,specialty}.rs` | done | Merchant packs / NPC flags through `NpcEconomyHooks`; a multi-pack NPC sells the union of all its packs. Checkout validates currency, checked totals and each balance, then stages complete goods / buy-back delivery before committing payment. Vocation charges the configured price displayed by the 1.2 shop, without earnings bonuses. Rejected checkout preserves items and balances. Purchases need an NPC merchant pack, a doodad `DoodadFuncStoreUi` pack, or the money form of the doodad's current-phase `DoodadFuncPurchase` (`CSBuyItems.unkId`, the `doodad_funcs` row id the 1.2 client writes there), and filter the goods by it; selling needs a merchant within 3 m, lists each item once and pays only for items moved to buy-back. Specialty: the empty `CSListSpecialtyGoods` answer, the dropped records packet and the corrected sale values are detailed in the specialty protocol note. |
| `MailManager`, `Models/Game/Mails/**`, `CharacterMails`, mail packets | `systems/economy/{mail,mail_character,mail_kinds,mail_packets,mail_data,mail_system}.rs` | done | Saved in `SaveStage::BeforeItems`; housing tax through `MailHousingHooks`. `/mail admin` sends an `ADMIN` system letter to an online or offline character. `quest_mails` / `quest_mail_attachments` are loaded; `SCQuestMailSentPacket` (0x150) is defined but never sent (trigger capture-pending, see the mail-leftovers deviation). Retention follows the client's `mail_guide` (5/30 normal, 5/90 system, configurable in `Configurations/Mail.json`; system mail with unclaimed attachments is kept, player attachments are returned, REGISTERED/deleted-sender escrow is kept); the level-10 send gate and the 3-repeat limit are enforced (see the 2026-11-08 mail bullets below). |
| `AuctionManager`, `Models/Game/Auction/**`, auction packets | `systems/economy/{auction,auction_data,auction_packets}.rs` | done | Saved in `SaveStage::AfterItems`; results mailed through `wiring`, and an online seller also gets `SCAuctionMessagePacket` for accepted bids and sales (the C# sent neither; see the auction-messages deviation). Requests require an auctioneer NPC (the `auctioneer` template flag) named by the packet or the current interaction object within 5 m (the C# ignored the ids). |
| `TradeManager`, trade packets | `systems/economy/trade.rs` | done | The C# `CanStartTrade` TODO ("Check faction and others") is implemented from the 1.2 client's own checks: a request and its acceptance refuse a dead player, a hostile faction relation and a distance `>= sqrt(25) = 5` m, and a trade across instances; see the trade validation deviation. |
| `CashShopManager`, `Models/Game/CashShop/**`, ICS packets | `systems/economy/cash_shop.rs` | done | Gifts by `CommercialMail` (`wiring`); account credits SQL behind `CashShopDb`. |
| Premium service shop (`CSPremiumServiceList` / `Buy` / `Msg`, `SCPremiumServiceList`, `PremiumDetail`) | `systems/economy/premium_service.rs` | done | Rust only: the C# answered the list with a fixed Russian 30-day entry and only logged buy / msg. The catalogue is the empty-by-default operator table `aaemu_game.premium_services`; the purchase is one atomic MySQL transaction. See the WP-C deviation below. |
| Item skill-effect bodies (grade enchant, socketing, conversion, cap scale, UCC imprint, ...), mate / slave equipment packets | `systems/economy/item_effects*.rs` | done | Called by the skills subsystem's effects. UCC imprint / cleanup require owned source and target items; client UCC casts require the actual source item's configured use skill, preserving the unrelated legacy bound-item portal exception. Dyeing checks the loaded `dyeable_items` allowlist. Invalid UCC / dye effects cancel deferred reagent consumption. Cap scale refuses an `item_cap_scale_forbids` target with SkillResult INVALID_TARGET; the reset-tool gate is unverified until a capture. |
| Item skill-effect stubs given data-derived bodies (`GainItemWithPosImprint` 32, `RechargeItemBuff` 95, `ExpToItem` 96) | `systems/economy/item_effects.rs` | done (data-derived) | The C# `Execute` bodies were `// TODO` stubs, so the behaviour comes from `special_effects` 405 / 546 / 547 / 548 (32: create `value1` x1 with `SkillEffectGainItemWithPos`), 12446 / 17017 (95) and 12462 (96: `value1` XP -> `value2` x1 with `SkillEffectExpToItem`) plus the tool skills that cast them. `GainItemWithPosImprint` stores no position imprint: the 1.2 item details have no such field. `ExpToItem` subtracts the experience directly instead of through `AddExp` (which recomputes the level and could level the character down), sends `SCExpChangedPacket(-value1)`, checks bag room before spending the XP, and cancels so the reagent is kept. `RechargeItemBuff` takes the cast's item target, resets `charge_start_time` (and `charge_count` when the template has one) and re-applies the equipped item's gear buff through `apply_gear_buffs` with the item both removed and added, so the recharge buff is not stacked; the 1 s item timers read the new charge window on their next tick, so nothing is re-scheduled. |

#### AA points: not supported (owner decision 2026-11-06)

AA points are not supported: the per-character wallet, `/aapoint`, the cash -> AA exchange, the
AA prices in the cash / premium shops and the AA shortage cover are gone, and every former AA
path behaves as it did before AA existed (AA-priced cash-shop SKUs are refused with
`INGAME_SHOP_BUY_FAIL_AA_POINT` 592, AA-priced premium services with
`PREMIUM_SERVICE_NOT_ENOUGH_AA_POINT` 775, and `CSICSBuyAAPointPacket` has no handler). The
client still carries AA: `SCCharacterState` reads its third / fourth `i64` moneyAmount as AA and
bank AA (`FUN_394c9580` / `FUN_394c9640` read `+0xc330` / `+0xc338`; the server writes `0, 0`),
`ItemTask` 16 / 17 (`ChangeAaPoint` / `ChangeBankAaPoint`) apply signed deltas through the
client's money task switch (`FUN_397d1030`), and the AA amount is displayed only in the
inventory window (`X2Util.GetMyAAPointString` / `GetMyBankAAPointString`). Confirmed with the
real client 2026-11-06. The `FeatureSet.aaPoint` key (bit 44) stays available but must remain
`false`.

#### Item and craft notification packet definitions (2026-11-05)

`systems/economy/item_notify_packets.rs` defines the four S->C packets the former C# port never
wrote, from the 1.2 client readers only:

* `SCCraftItemUnlockPacket` 0x0f5: two single bytes, reader `FUN_397b4c60`
  (`x2game.dll.c:1708523`). The handler `FUN_391d9700` packs them into an item-slot reference,
  so (slot type, slot index) is INFERRED.
* `SCItemAcquisitionPacket` 0x097: `u8 type, string charName (<=128), u8 count, count x Item`,
  reader `FUN_397c1f80` (`x2game.dll.c:1717976`); the item records use the standard reader
  `FUN_3961f500` (`x2game.dll.c:1301392`), the same record `SCItemTaskSuccessPacket` writes.
  The handler acts only for type 1 (an item link line naming `charName`), which is INFERRED to
  be a party/raid member announcement. WP-IT1 now sends it (see below).
* `SCItemDetailUpdatedPacket` 0x093: `u64 itemId, u8 x, u8 index, ItemDetail`, reader
  `FUN_397b3840` (`x2game.dll.c:1707583`); `ItemDetail` is the `FUN_3961edf0`
  (`x2game.dll.c:1301101`) `u8 detailType` plus its fixed body. The two zeroed bytes between the
  fields are not read, hence not written.
* `SCItemTaskNotifyPacket` 0x091: `u32, u8 action, u8 count, count x ItemTask, u8
  forceRemoveCount, forceRemoveCount x u64, u32`, reader `FUN_397b3810`
  (`x2game.dll.c:1707572`) followed by the `SCItemTaskSuccessPacket` reader `FUN_397d1d00`
  (`x2game.dll.c:1729938`).

`SCCraftItemUnlockPacket`, `SCItemDetailUpdatedPacket` and `SCItemTaskNotifyPacket` are
**capture-pending** and deliberately never sent, because no 1.2 zone code or capture shows their
triggers: **ITEM-NOTIFY-1** (craft unlock: the Rust server never locks craft materials),
**ITEM-NOTIFY-3** (detail update) and **ITEM-NOTIFY-4** (task notify: probably item tasks for an
owner other than the viewer). Their byte layouts are test-locked in the module and they stay in
`rust/tools/opcode_coverage/sc_baseline.json` as typed-but-never-sent; a future trigger must
remove them from that list.

#### Party loot announcement `SCItemAcquisitionPacket` (WP-IT1, 2026-11-06)

`systems/economy/looting.rs` now sends one `SCItemAcquisitionPacket` per loot pickup to the
other team members: corpse / loot-bag picks (`CSLootItemPacket`, `CSLootOpenBagPacket` loot-all)
and party-distributed loot (rotate winner / dice / loot master, through `try_distribute`). The
packet carries kind `1`, the receiving character's name and the looted stacks as they were in the
container (the client filters which templates announce with `FUN_39695dc0`), capped at the wire's
`u8` item count. Recipients come from the container's `KillerTeam` (`active_team`, the same team
`TryLoot` distributes to): every member except the receiver, in the same world instance, within
`MAX_LOOTING_RANGE` (200 m, `looting.rs:48`, the loot distribution range used by
`generate_loot_with`). The recipients are collected while the looting container is locked; the
packets are sent after the container, item-transaction and character locks are released. The
whole feature is behind `Loot:AnnounceToParty` (`Configurations/Loot.json`, default **false**).

INFERRED, not evidenced: kind `1` as the party-loot line (client handler `FUN_391d5760` acts only
for 1 and the `ITEM_ACQUISITION_BY_LOOT` event text), the loot sources and the range. The C#
never sent this packet (it only declared the opcode), so there is no C# behaviour to mirror.

#### Item use gates from data (2026-11-04)

`items.actability_requirement` and `items.actability_group_id` are loaded into `ItemTemplate`
and enforced on the equip and item-use paths. The 1.2 client's check is `FUN_394e1a90` (called
by the use action `FUN_390447d0` and the equip-drag validation `FUN_394e41f0`): when the
requirement is positive, the character's points in the group plus the house act-ability bonus
(`FUN_390b3a80`) must reach it, else the client shows `actability_not_enough_point` 508 through
`FUN_39428370(0x1fc)`. The server sends the same error with the group id as `kind` and refuses
the request from `CSSwapItemsPacket`, `CSSplitBagItemPacket`, the coffer move packets and the
`CSStartSkill` item-caster branch; the core move additionally refuses any other caller that
would put an item into the player equipment container, so the item is never consumed. The ten
1.2 rows are the fishing rods 27309-27312, 27671-27674, 29702 and 30261 (group 7, requirement
10000..50000); the wrapped rods' use skill runs only at the requirement.

`item_cap_scale_forbids` is loaded into `ItemData` (1340 rows, 1335 distinct item ids). The 1.2
client builds the set from `SELECT item_id FROM item_cap_scale_forbids` (`FUN_395ac620`) and
tests it with `FUN_395a72d0`, which backs the item's `scalable` property and the cap-scale
target validation `FUN_3939dc20` (SkillResult INVALID_TARGET 0x22). `ItemCapScale` now fails the
cast with SkillResult INVALID_TARGET before any roll, so the temper item is kept. The
`ItemCapScaleReset` tool uses the same set here, but its client path was not decoded: that part
is **unverified** until a capture.

`CharacterActability.ExpandExpert` with no next `expand_expert_limits` row answers
`store_unknown_error` 228 instead of the C# TODO's silent return. `GainLootPackItemEffect`'s
inherited grade keeps the C# TODO (capture-pending **ITEM-LOOT-GRADE**): the 1.2 client only
parses the descriptor (`FUN_396e91c0`), not the retail rule. `item_secure_exceptions` was
already loaded and used by `item_security::is_lockable` (`FUN_394df4a0`); unchanged.

#### Mate equipment rules (2026-11-05)

`CSChangeMateEquipmentPacket` now enforces the mate's slot permission from
`mate_equip_slot_packs` (`systems/economy/mate_equipment_data.rs`, loaded into
`server.game_data.systems.economy.mate_equipment`; the mate's NPC template names the row through
`npcs.mate_equip_slot_pack_id`; when the template exists but the pack is 0/NULL or its row is
missing, every slot is refused like `FUN_3933a9c0` returning 0. Only a mate template the server
data does not know (test fixtures) stays unrestricted). EVIDENCED from the 1.2 client:
`LoadMateEquipSlotPackDescs` (`FUN_396ce630`, `dll.c:1479277`) fills chest `+0`, feet `+1`, head `+2`, waist `+8` from
`SELECT id, chest, feet, head, name, waist FROM mate_equip_slot_packs`, and `IsMateEquipableSlot`
(`FUN_3933a9c0`, `dll.c:667569`) allows exactly equip slots head 0 (row `+2`), chest 2 (`+0`),
waist 3 (`+8`) and feet 6 (`+1`); the equip-drag validation `FUN_394e41f0` calls it with the
item's slot at `dll.c:1021546`. The server derives the item's slot from
`ItemTemplate::gear_slot_type` + `allowed_gear_slots` (armor slot type 1 -> head 0, 3 -> chest 2,
4 -> waist 3, 7 -> feet 6, the same mapping the data uses). A refused item stays in the bag and is
answered with `SCMateEquipmentChangedPacket{success:false}` plus `ITEM_NOT_FOR_PET` (204);
unequipping to the bag is always allowed, and a bag item swapped onto the mate during an
unequip follows the same two checks. The client's whitelist failure falls to error `0xcc` (204,
`dll.c:1021556`), the slot-check failure uses `0x22f` (559) whose 1.2 `ErrorMessageType` text is
not the pet-equip message; the server uses `ITEM_NOT_FOR_PET` for both (capture-pending
**MATE-EQUIP-1** for the exact slot-refusal text).

`mate_equip_pack_groups` x `mate_equip_pack_items` (the per-mate item list) is loaded through the
same join the client runs (`FUN_395fbbc0`, SQL at `0x399f9490`) and is enforced only while
FeatureSet bit 78 (`flag_9_6`) is set. The client's `FUN_395ac760` (`dll.c:1198859`) call from the
equip validation `FUN_394e41f0` (`dll.c:1021538`) is gated on `(*(DAT_39b02294 + 0x1c) & 0x4000)`,
the client's copy of the server FeatureSet: dword `+0x1c` bit 14 = fset byte 9 bit 6 = feature 78,
server-controlled and clear in `DEFAULT_FSET` (`0x0c`). The server checks
`server.systems.services.features(server).check_bit(78)` per request. The real data also joins
4660 group/item rows of which 1058 name an item whose equip slot the mate's own slot pack forbids
(e.g. effect pack 2 on a combat mate with a waist item); with the feature on this reproduces the
client's list exactly, with the feature off (the default) the list is not applied.

Tests: `mate_equipment_riding_pack_allows_head_waist_and_feet`,
`mate_equipment_riding_pack_refuses_chest_and_keeps_the_item`,
`mate_equipment_combat_pack_refuses_waist_and_keeps_the_item`,
`mate_equipment_pet_pack_refuses_every_slot_and_keeps_the_items`,
`mate_equipment_missing_slot_pack_refuses_every_slot`,
`mate_equipment_pet_pack_still_unequips_to_the_bag`,
`mate_equipment_item_outside_the_client_list_is_still_allowed_when_the_feature_is_off`,
`mate_equipment_item_outside_the_client_list_is_refused_when_the_feature_is_on`,
`mate_equipment_swap_onto_an_occupied_slot` and
`mate_equipment_unequip_swap_checks_the_bag_item` (both swap directions), the loader's in-memory
tests and the `AAEMU_COMPACT_DB`-gated `real_compact_db_mate_equipment`.

#### Mate equipment expiry (2026-11-05)

`SCMateEquipmentExpiredPacket` (0x127) is now defined and sent. `ItemManager.UpdateItemTimers` also
ticks every persistent `EQUIPMENT_MATE` container of an online owner (the C# only ticked equipment,
bag and warehouse; it had the opcode constant but never sent the packet), and an expired item leaves
the container through the same `remove_item_task` path as player equipment (release id + the
container leave event, which updates the mate mirror and broadcasts `SCUnitEquipmentsChanged`) but
with `ItemTaskType.Invalid`, so no bag / equipment item task is sent. The owner instead gets
`SCMateEquipmentExpiredPacket(mate_tl, EQUIPMENT_MATE, slot)`, queued before the leave event.

EVIDENCED from the 1.2 client: the reader `FUN_397b5530` (`dll.c:1708939`, vtable
`PTR_LAB_399b390c`) transfers `u16` (stream slot 0x40), `u8` (0x44) and `u8` (0x44) with no padding
bytes; `OnMateEquipmentExpired` `FUN_391db280` (`dll.c:368834`) calls `FUN_394e5a90`
(`dll.c:1022714`), which acts only when the slot-type byte at struct `+1` is 0xFC
(`slot_type::EQUIPMENT_MATE`), the packet's tl id equals the local player's current pet
(`FUN_3933f5e0`) and the index at struct `+3` is below 0x1c (28).

A mate that is not spawned still has its container (`MateEquipmentContainer` outlives the spawn,
`CharacterMates.SpawnMount` only finds-or-creates it), so its gear ticks and expires too; no packet
is sent in that case because the client has no current pet to apply it to. An offline owner is
skipped, matching the bag rule. None of the 62 `mate_equip_pack_items` items has an expiry in the
shipped data, so this only runs for cash / event gear or GM-set lifespans; whether retail also sends
a lifespan sync for a mate-container item is UNKNOWN (the client path needs only the new packet).

Tests: `mate_equipment_expired_layout` (independent literal bytes), the flow test
`mate_equipment_expires_and_notifies_the_owner` (past end time removes the item, keeps an unexpired
one and sends exactly the expired packet plus the leave broadcast),
`mate_equipment_expiry_skips_the_packet_for_an_unspawned_mate` and
`mate_equipment_timers_skip_an_offline_owner`.

#### Limited-sale purchase limits (2026-11-03)

`items.limited_sale_count` and `items.one_time_sale` are now read and enforced as per-character
purchase limits (`systems/economy/shop_limits.rs`, table `character_shop_purchases`). The 1.2
client loads both columns with the item template (`SELECT ... limited_sale_count ... one_time_sale
... FROM items`) but never reads them: the NPC store UI has no remaining-count field, and the only
sale-limit error in its table is `store_one_time_sale` (752, `0x2f0`). Its GM command list on the
same build carries `limited_sale_add` / `limited_sale_reset` / `one_time_sale_add` /
`one_time_sale_reset`, so the counters are per character and reset by an administrator. The reset
period itself is not available for 1.2 (capture-pending **SHOP-1**): the implementation keeps the
counter per character, never resets it, and does not share it with the account or the server.
`one_time_sale` is a limit of one and the smaller column wins when both are set.

Enforcement is one atomic check-and-claim, before the price or the item moves: `CSBuyItemsPacket`
claims the sum of all lines of one template after the checkout is staged (a refused checkout sends
752 and changes nothing), and `DoodadFuncPurchase` claims before consuming the coin and gives the
claim back if the consumption or the grant then fails (debit first, refund on failure). Selling,
selling back and buying back do not change a counter; GM give, quests, loot, crafting and the cash
shop are not the vendor purchase path and stay unlimited (the cash shop has its own
`ics_shop_items.limited_type` / `limited_stock_max`). Not in the C# port (it never read either
column).

#### Cargo escrow and stale rack references (2026-10-25)

Merchant-ship rack deposits now revalidate the exact equipped pack and a vacant
rack in one economy transaction. Recovery validates System escrow and backpack
space, moves the same item ID, and clears the rack reference together. A rejected
pickup preserves the pack, the current backpack and recorded item effects.
The shared path covers other hulls and ordinary racks; the real-data regression
uses Merchant hull 75, item 23398 and its twenty attachment points 60..79.

Error 14 (`INTERACTION_RECOVER_PARENT`) on this path means the referenced item is
outside System escrow; it does not establish that another pack is physically on
top. The separate persistent-child removal check uses error 207. Provably dangling
persistent cargo references can return to the authored empty phase without moving
or creating an item. Ambiguous references remain blocked with rack/item/container
and hull diagnostics; current child allocation identity and parent/instance must
match before a child can block repair or removal.

Both live and saved-row cleanup receive the original doodad metadata. Original
rack/cargo/Slave references never authorize deleting the current item at a recycled
GUID. Ordinary furniture cleanup requires a unique singleton matching the original
template and owner in that owner's System escrow. Items in inventory, equipment,
bank or another owner's containers are retained. Saved container GUIDs alone never
authorize container deletion. Saved-row removal checks its selected reference
fields before invoking cleanup.

This conservative policy protects unloaded rack aliases and recovered/recycled
items. Metadata matching does not establish allocation generation. Cargo and
ambiguous item references, plus saved containers, may be retained after the last
rack disappears; reconciliation and orphan collection remain open. Regression
sources cover shuffled twenty-rack cycles, stale sources, failed recovery,
allocator reuse and private-MySQL saved aliases. At `198b7d482`, the official lane
check, the game unit suite, the real-compact twenty-rack cycles and both private-MySQL
cargo fixtures pass. A bounded headless live run with one fresh owner (80 positive rack
operations over four passes, forty drops, twenty pickups) kept the twenty original
item IDs through every cycle and ended with twenty System ground references and twenty
empty saved racks; an independent cargo and SQL audit found no P1/P2 for that
positive run. The same run's bag-full negative left items, racks and phases unchanged,
and a hard close with reselect kept the same loaded hull registered with a single
current root. In a second summon of the same item the skill executed (started, fired,
ended); the subsequent loaded-hull replacement was refused with error 801
(`SLAVE_EQUIPMENT_LOADED_ITEM`), the loaded hull, racks, cargo and phases were
preserved and only the initial SCSlaveCreated appears in the captures. That shows
loaded-hull preservation; unloaded replacement and completion of the 600-second
timer were not exercised in this live run. This is observational evidence on the
Rust-server path, not retail parity or global atomicity; the post-STOP SQL proves
persisted state, not save order, and the process stop was forced, so the logs are not
warning- or error-free. Independent cargo/SQL and control reviews support this
bounded observation with that replacement-refusal qualification; wider audit items
remain open.

#### Stale saved cargo racks and client summon points (2026-10-28)

Deviation from the C# `SlaveManager.Create`, which restored a rack's saved item reference
unchecked: the saved-cargo preflight (`PersistentDoodads::validate_slave_cargo`) no longer
refuses the summon forever when a rack's escrow pack is gone (no container holds the id) or is
not the escrowed pack any more (moved out of System escrow, or the id was reused by an
unrelated item). Such a rack is restored empty in its authored empty phase, the cleared row is
persisted, and a warning names the rack and item ids; no item is granted, moved or deleted.
Several holders of the id, an intact pack referenced by two racks or by a live rack of another
hull, and a live-receipt mismatch still refuse. Such rows most likely date from before
`a0a62dbf3`, when deleting a live rack ran `remove_expired_doodad_item` and removed the escrowed
pack while the persistent rack row kept its reference; the surviving logs cannot prove it. Today
cargo, rack and Slave references are retained by `cleanup_doodad_escrow_item`.

Boat summon points are checked with `ship_physics::is_summon_water`: the client picks them on
its wave-displaced sea 0.05-0.6 m above the nominal level (`z = 100.3` over ocean level
100.0), which the strict `IsWater` (`z <= level`; the open ocean has no water volume) refused
as "a ship must be summoned on water". A point up to `SUMMON_WATER_TOLERANCE` (1 m) above the
water is accepted when the terrain under it is below the surface (so beaches stay land).
Land-vehicle placement keeps the strict check. Real-data check (needs `AAEMU_GAME_PAK`):
`cargo test -p aaemu-game real_water_boat_summon_points -- --ignored --nocapture`.

### systems::social

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `TeamManager`, `Models/Game/Team` | `systems/social/team.rs`, `packets/team.rs` | done | Invitations (single / area with `SCAskToJoinTeamArea` / `SCTeamAreaInvited`, 30 s area cooldown, 60 s timeout, 500 ms sweep, 10k cap, stale-reply and owner-rights guards), invitation error replies, create, AskRiskyTeam (leave / kick / dismiss + auto-disband; the direct `CSLeaveTeam` / `CSKickTeamMember` / `CSDismissTeam` packets run the same actions), owner / role / marker / looting rule / ping / move, raid conversion, SetOffline / MemberRemoveFromTeam / UpdateAtLogin / UpdatePosition, 500 ms remote-member sync (delta cache, 5 s forced resync, upper-triangle pairs), zombie-team disband (5 min), loot winner helpers. Events (OnTeamJoin/Leave) are quest-side: not called. See "Area team invitations and invitation error replies". |
| `ExpeditionManager`, `Models/Game/Expeditions` | `systems/social/expedition.rs`, `packets/expedition.rs` (`managers::expedition` re-exports) | done | Load, create (party checks, cost, `SCItemTaskSuccessPacket` money-only form), invite / reply, leave, kick, member role, owner, role policy, disband (stays listed as `$deleted-guild-<id>`), `Expedition.Save` per change, offline leave for character deletion. Guild rename (GM-granted right, see "Guild rename" below), the sponsor id and the report / leader-only checks: see "Guild functions (2026-11-05)". |
| `FamilyManager`, `Models/Game/Family` | `systems/social/family.rs`, `packets/family.rs` | done | Load, invite / reply / create, leave, kick (offline existence check), title, steward, login / logout, disband, save, offline leave for character deletion. |
| `FriendMananger`, `CharacterFriends`, `CharacterBlocked` | `systems/social/friend.rs`, `packets/friend.rs` (`managers::friend` re-exports) | done | Add / remove friends (online or by DB name lookup), block / unblock (online or by `NameManager` for offline names), status changes incl. `InParty`, and the client's failure feedback for every refused add / remove / block / unblock (WP-SL2, see the deviation). The blocked list is enforced on every direct delivery path (whisper, team / area / expedition / family invite, friend add, trade and duel request); see the block-list deviation. |
| `ChatManager`, `Models/Game/Chat` | `systems/social/chat.rs`, `session.rs` (`managers::chat` re-exports) | done | All channel kinds incl. party / raid / court, CleanUpChannels, party / raid / raid-leader / judge routing, per-channel spam delays (`SCChatSpamDelayPacket` + server-side enforcement, see "Chat spam" below). User channels (`CSJoinUserChatChannel`) are log-only as in C#. |
| `FactionManager` relations, `SystemFaction.GetRelationState`, `Unit/Character.SetFaction` | `systems/social/faction.rs`, `packets/faction.rs` | done | Relation table validated against compact.sqlite3 (gated test). Faction CS packets are log-only as in C#; every SCFaction* struct ported. `relation_state_live` overlays the runtime nations on the static table for the combat, team, party and AI paths. |
| Player nations (`nations`, `nation_relations`, independence, immigration, diplomacy, rename, national tax, monument) | `systems/social/nation.rs`, `commands/scripts/nation.rs` | stages 1-4 | Skill 18540 special effect 71 creates a nation from the caster's expedition (owner + dominion, CASTLE_CLAIM backpack); ids come from the expedition allocator; `SCFactionIndependence` + relation rows + `SCUnitFactionChanged`; online members use `set_faction`, offline `characters.faction_id` moves with `expeditions.mother` in the save transaction; login sends nation factions and relations. Stage 2 adds the invite / reply / return-to-origin / kick packets with the client's error order, persisted factions and offline kicks. Stage 3 adds `CSFactionDeclareHostile` 0x019, the type-25 friendly treaty mail (charge paid through `CSPayChargeMoney` 0x0a0) and the periodic relation expiry. Stage 4 adds the nation rename (`SCNationShowRenameUI` 0x010 + the `CSRenameExpedition` nation half), the national tax (`CSUpdateNationalTaxRate` 0x013 -> `SCNationalTaxRate` 0x021) and the territory-statue monument (`SCNationalMonumentChanged` 0x022). See "Player nations" below. |
| `GameData/AchievementGameData` (`achievements`, `achievement_objectives`, `pre_completed_achievements`, `char_records`), `Models/Game/Achievement` | `systems/social/achievement.rs` (`server.game_data.systems.social.achievements`) | done | Loader + port of `AchievementGameDataTests`; gated real-DB test `achievement_game_data_real_compact_db`. The C# 1.2 tree has no manager; the Rust achievement manager / runtime / hooks / rewards (`achievement_*.rs`) read them (see "Achievement producers" below). |
| `DuelManager`, `Models/Game/Duels`, duel tasks | `systems/social/duel.rs`, `packets/duel.rs` | done | Request / accept (flag via hook) / start / stop / cancel, distance + HP checks, end timer (tokio tasks; `set_delays` test hook). The temporary Red / Blue team faction also goes to each participant's active mates and owned slaves (they copy the owner's faction at spawn and `Character.SetFaction` never updates them) through `combat::set_faction_with_temp`, and is restored with the owner's at the end / disconnect; a mate or slave summoned during the duel is restored as well. Test: `mate_and_slave_follow_the_duel_faction_and_get_it_back`. |
| `CrimeManager`, `TrialManager`, `Models/Game/Crime`, `TrialUpdateTask` | `systems/social/{crime,trial}.rs`, `packets/trial.rs` | done | Crime table load / save (`AfterItems`), report / evidence (hooks), bot reports, trial / jury flow with 5 s tick, audience, bounty placement / arrest payout (`systems/social/{bounty,bounty_runtime}.rs`, `bounty_pledges`; see "Wanted-player bounty"). The board answers the empty list by default and the pledge rows behind `Social.BountyBoard` (BOUNTY-board). Arrests, authenticated jury choices, guilty pleas, acquittals, court cleanup and reconnect jail enforcement have packet-flow regressions; prisoner buffs persist remaining online time. |
| Trial sentence formula, refusal penalty, quest jury point (2014 video evidence) | `systems/social/trial.rs` (`calculate_jail_time`, `finalize_verdict`), `systems/quests/acts.rs` (`SupplyJuryPoint`), `systems/social/crime.rs` (`set_jury_point`) | deviation (C# formula corrected; C# packet bug fixed) | **Sentence:** no compact.sqlite3 trial / sentence / jail table exists (prisoner buffs 631 / 2028 are 30-minute templates; the sentence is applied as the buff's runtime duration). The C# `TrialData.CalculateJailTime` (git `a2ab80abd^`) evidence score (murder 20 / theft 8, x10 for victims under level 30) gives 164 minutes for the 2014-10-31 video's record (76 crime points, 26 trial records: 3 murder, 10 assault, 13 theft), but the trial dialogs (Request Trial / Awaiting Trial) carry the text "**15** minutes of penalty time" (the `$2` = `jail_minutes` placeholder; ASSUMED, not confirmed, to belong to this 76-point defendant) and the judge announces **18**. Retail uses the 2014 community formula the C# discarded, in integer arithmetic: `(crime_points / 5) * (1 + infamy / 1000)`; 76 / 5 = 15, and the maximum jury option (the C# 1.2x factor in `finalize_verdict`) is the 18-minute ruling. The C# comment's "seems wrong" compared the base with the jury's chosen (scaled) sentence. The 14 / 28 minute chats are consistent (their inputs are unknown). Evidence limits: the video's infamy was below 1000, so the `/ 1000` term is from the 2014 formula, and the non-maximum jury factors are the C# guesses — capture-pending. Also capture-pending: the offence type (murder / theft / assault) is ignored except through the crime points it adds, the trial evidence count no longer matters, and the infamy multiplier `1 + infamy / 1000` is step-wise (x1 below 1000, x2 from 1000); the owner notes the real sentence depends on several factors including infamy. **Refusal:** the client's ui_text 2666 `ask_imprison_trial` ("a $2 minute sentence will be automatically enforced") and 3378 / 5388 / 5391 pass `SCAskImprisonOrTrialPacket.jail_minutes` / `SCTrialWaitStatusPacket`, and `reply_imprison_or_trial(false)` already applies exactly that `trial.jail_time` through `result_is_guilty(.., plead_guilty = true)`. There is no extra fixed 15-minute penalty in the text (KO: "재판을 신청하지 않을 경우 $2분의 형이 자동으로 집행됩니다"), so none is added; the bug was the 164-minute base. **Jury point:** `QuestActSupplyJuryPoint` wrote `Character.jury_point` directly and sent nothing; the C# property sent `SCJuryPointChangedPacket`, now restored. **Crime / infamy:** they are separate fields (`crime_point` / `crime_record` = `infamy_point` in `characters`) that move together on reports and reductions (2014 video Character Info 13/13, 3/3; wiki: "for every 1 Crime Point you earn, you will gain 1 Infamy Point") and split only on jail: the guilty verdict clears crime points and keeps infamy. Tests: `video_trial_sentence_is_crime_points_over_five_and_the_max_verdict`, `refusing_the_trial_applies_the_displayed_base_sentence`, `supply_jury_point_sends_the_changed_packet`, `crime_points_and_infamy_move_together_on_gain_and_reduction`. |
| Pirate return at 0 infamy + "Repentant" (kind 49) | `systems/social/{crime,trial,faction}.rs`, `db_character.rs`, `world/session.rs`, `systems/quests/bridges.rs`, `systems/skills/effects/special/misc.rs` | added (deviation) | **Deviation from the 1.2 C#:** the C# only returned a pirate to the home faction in the not-guilty trial verdict (`TrialManager.cs:818`, git `a2ab80abd^`). Here every infamy decrease checks the saved faction: `CrimeFactionChange` is decided under the character lock by `Character::check_wanted_threshold` and applied after the lock by `faction::apply_crime_faction_change` for `AddCrime` (crime reports, quest `QuestActSupplyCrimePoint`, GM crime commands), the `CrimePoint` / `InfamyPoint` setters (trial not-guilty, `clear_crime_status`, `set_crime_value`), special effect 102 `GiveCrimePoint` (item 32212 / skill 25281, -100; skills 25280 / 23417 use the same effect) and `CSSelectCharacter` (offline reductions; the DB load leaves the faction to the select like the pirate conversion). Rule: saved faction 161 and infamy exactly 0 (never below 3000; item 32212's text and research `pirate-return.md`). The return uses `set_faction_persistent` with `characters.template(race, gender).faction_id` (position kept, INFERRED: the C# `SetFaction` has no teleport); the existing house / guild / party handling applies (a guild owner hands the leadership to a same-faction successor, else the guild is dismissed, PIRATE-1). The 1.2 client prints ui_text 2297 `pardon_pirate` itself from the `SCUnitFactionChangedPacket` old 161 -> non-pirate (`x2game.dll.c:617340`); no separate ui-text packet exists on the trial path either. The same pardon reports achievement kind 49 `MY_DISHONORABLE_POINT` (`char_records` 2401, value 1) -> achievement 1380 "Repentant". Wanted (crime >= 50) is unchanged. Skill 20632 ("Drink from the Loka River", -200 infamy) is description-only in the data (no effect row), so it is not wired. Tests: quest act flow, trial verdict, infamy 1 / non-pirate, house side effect, item special effect, offline select with `AAEMU_TEST_MYSQL`. |
| `DominionData`, `Models/Game/Achievement`, rename / chat bubble / NPC chat / spam packets | `systems/social/{dominion,achievement}.rs`, `packets/misc.rs` | done (data + packets) | No C# manager drives them in 1.2. The dominion `SCDominionDeleted` / `TaxBalanced` / `NonPvPStarted` / `NonPvPStopped` notifications now have senders (see the `systems::housing / systems::social` notification bullet under Phase 3). |
| `RankingGameData` (later C# history), 1.2 ranking packets | `systems/social/ranking_{data,packets,runtime,manager,persistence}.rs` | done (arena + fishing boards) | Not in the 1.2 C#; Rust only. Request path, MySQL persistence (`character_rank_scores`, `rank_period_payouts`), score hooks and period-close reward letters: see "Ranking boards" under Known deviations. |
| `RadarManager` | – | not social | Telescope / fish school registration from buff templates: skills / npcs. |
| UI / notification packet definitions (`SCScheduledEventStarted/Ended`, `SCNotifyUIMessage`, `SCUnitLocation`, `SCChatLocalizedMessage`) | `systems/social/ui_packets.rs` | defined only | Byte-exact tests from the 1.2 client readers; nothing sends them (trigger capture-pending). `SCWorldMessagePacket` 0x211 is already defined in `packets/g2c_misc.rs` and is likewise never sent. See "UI / notification packet definitions (WP-UI7)" below. |

Cross-subsystem calls: wired in phase 3b (see "Phase 3b wiring": trial court doodads / NPCs after the spawns, `SocialHooks` by social itself, crime evidence / arrests / team and duel queries from skills, loot rules from economy, court checks in portals and instances, `force_dismount` through vehicles). Still open: teleports -> `session::on_moved` (C# `Character.SetPosition` -> `TeamManager.UpdatePosition`; plain movement does not call it in C# either).

#### UI / notification packet definitions (WP-UI7, 2026-11-06)

`systems/social/ui_packets.rs` defines the decoded-but-unsent S->C UI packets from the 1.2
client readers only; the C# tree had only their opcode constants (`SCOffsets.cs`), never the
packet classes:

* `SCScheduledEventStartedPacket` 0x1ec: `u32 id`, opaque 21 x `i32`, `i32 changes` (a 21-bit
  mask), five opaque bytes, `i32 periodMin`, `u8 weekDay`, `string openMsg` (`<= 0x7f`), reader
  `FUN_397b01f0` (+ `FUN_398a7ed0` / `FUN_398a7d80`). What the 21 ints and the five bytes hold
  is UNKNOWN (CAP-UI-1); the trigger (a `game_schedules` window opening) is INFERRED.
* `SCScheduledEventEndedPacket` 0x1ed: `u32 id, string closeMsg` (`<= 0x7f`), reader
  `FUN_397b0260`.
* `SCNotifyUIMessagePacket` 0x1cc: `i32 msgType, i32 value, string argv0 (<= 0xff),
  string argv1 (<= 0xff)`, reader `FUN_397afe30`; the second `i32` is unnamed in the reader.
* `SCUnitLocationPacket` 0x1c7: `bc unitId` + the 9-byte packed world position, reader
  `FUN_397c6f90` (+ `FUN_3961e2c0`); handler `FUN_391e0940` (`OnUnitLocation`) moves the camera
  and minimap onto the unit.
* `SCChatLocalizedMessagePacket` 0x0d2: `i64 id, bc unitId, u8 LanguageType, i32 category,
  string text (<= 0x400), string key (<= 0xff), i32 numParam, string param (<= 0x80),
  string param2 (<= 0x80)`, reader `FUN_397c2730`; both string fields are unnamed in the
  reader.

`SCWorldMessagePacket` 0x211 (`u8 source, u8 messageType, string (<= 0x800)`, reader
`FUN_397b6130`) is already defined in `packets/g2c_misc.rs:169` and is not duplicated here.

All are **capture-pending** and deliberately never sent (CAP-UI-1 / CAP-QU-2): no 1.2 zone code
or capture shows their triggers, so a future capture only has to add the sender. The types are
locked in `rust/tools/opcode_coverage/sc_baseline.json` as typed-but-never-sent.

#### Player nations (stages 1-4, NATION-1..17, RENAME-7..9)

`systems::social::nation` implements the 1.2 "declare independence" slice: a nation is a
top-level user faction (`mother_id == 0`, `owner_type` 0, owner = the founding expedition's
owner), and the founding expedition becomes its child (`expeditions.mother`). The client side
is `FUN_392fb2d0` (`SCFactionIndependence` 0x026: it clones the founding expedition record,
sets mother 0 / name / created time / `diplomacy_target = 1`, then re-factions the expedition's
units). Ids come from the same `ExpeditionIdManager` as expedition faction ids; the 1.2 client
refuses a duplicate faction id (`FUN_398a5280`), so `EXPEDITION_ID` also extracts `nations.id`.

* **NATION-1 (capture-pending):** the packet that carries the chosen nation name was not found
  in the 1.2 client (research section 2.1.1: the Lua `X2Nation.DeclareIndependence(name)` only
  validates it locally). The server uses the founding expedition's name, which is what the GM
  command and the independence effect also send.
* **NATION-2 (INFERRED):** a new nation copies all 46 `system_faction_relations` rows that name
  faction 166 "default faction setup for independence", replacing 166 with the nation id. The
  real compact database has 46 rows (33 `(X, 166)`, 13 `(166, X)`); the copy rule is inferred
  from their contents, not from a decompiled server. `real_compact_db_relation_template_is_faction_166`
  pins the count and the hostile/neutral/friendly samples.
* **NATION-3 (capture-pending):** the nation `SystemFaction` record mirrors the founding
  expedition clone (`allow_change_name` 0, `political_system` 1) with `diplomacy_target` 1 as
  `FUN_392fb2d0` sets it. Whether the live 1.2 server sends `SCNationShowRenameUI` from a
  production trigger and what it checks is not established; stage 4 gates the rename on a
  server-issued offer (RENAME-7).
* **NATION-4 (capture-pending):** no 1.2 error id is known for the "owns no dominion"
  prerequisite: the special effect and the GM command send/use 40
  `FACTION_SEPARATE_RIGHTS`. The GM command's `nation create force` skips only that check for
  test fixtures. The skill's CASTLE_CLAIM backpack (type 1, slot 26) is checked in the effect
  after the authored `unit_reqs` slot requirement.
* **NATION-5 (stage 2 scope):** when the founding expedition disbands, the nation stays as a
  kingless nation (`dismiss_expedition` clears its `expedition_id`; the next save writes 0).
  Load clears a stored id whose `expeditions` row is missing or whose `mother` is not the
  nation, so a guild that later reuses the freed id is neither captured nor refused
  independence. Offline members' stored houses move in the creation transaction, and the
  housing / crime faction lookups use the live nation table. Nation destruction / succession is
  stage 2 (capture-pending).
* **NATION-6 (hot path, documented):** relation lookups take the nations `RwLock` and scan
  linearly. `faction_pair` / `is_nation` avoid cloning and have empty fast paths, but there is
  no id index; the expected nation count makes this acceptable.
* **NATION-7:** a nation member's mother is 0, so nation chat shares `faction_chat(0)` and
  cross-nation whispers are allowed. A real nation channel is stage 2+ work.
* **NATION-8:** a member already in the enter-world path when independence runs can keep the
  race faction (the offline set comes from one world snapshot); the next login loads the saved
  nation faction.
* **NATION-9:** `save_creation` takes `SaveManager::transaction_lock` around its transaction;
  the periodic save runs inside `DoSave`. The GM command and the skill both call the same
  synchronous service, so a concurrent duplicate call is refused by the in-memory check.
* **NATION-10:** a client already in the login burst when independence broadcasts can miss
  `SCFactionIndependence`; it receives the nation record and relations on the next login.
  Expedition records with `mother = N` are sent before N's own record in the login burst; the
  1.2 client's tolerance for that order is unverified (the client stores records independently).
* **NATION-11 (id2 UNKNOWN; timeout chosen):** the 1.2 client passes `id2` from
  `SCFactionImmigrateInvite` (0x027) back unchanged in `CSFactionImmigrationInviteReply`
  (0x016) but never interprets it. The server uses the invitee character id as a nonce: the
  reply must name the nation, the nonce and a pending invite, so a forged or replayed reply is
  refused with 693. The client has no visible invite timer; pending invites expire after 60 s
  (the same timeout as the port's team / expedition invitations) and the list is capped at
  1024; a repeated invite while one is pending is dropped instead of re-sent, and an invite
  from a king on the invitee's block list is dropped silently (`BLOCK-LIST-1`). The reply
  checks follow the
  client's own order (`FUN_398a44d0`: 770 combat, 694 pirate, 693 unknown, 730 already a
  member, 731 owns a user faction, 732 expedition member, 728 below `content_configs` 49 =
  50); the invite re-runs them on the invitee and answers the king with the same codes, after
  its own client checks 697 (not the king) and 736 (oneself) and the online check 50.
* **NATION-12 (capture-pending):** `SCFactionKickToOriginResult` (0x02a) writes
  `(tgtName, id, id2, errorMessage)`; the decompiled handler reserves a 4-byte error slot but
  its read width is not visible, so the C#-derived `i16` is kept. Every outcome (64 not the
  king, 65 oneself, 704 unknown target, 726 expedition member, 705 other faction, success) is
  answered with this packet and its error field, as the client shows a message box for a
  non-zero error (`FUN_392fe6c0`).
* **NATION-13 (INFERRED):** a guild belongs to its mother's top-level faction. The C# checked
  a member's mother only at creation; with runtime nations a nation is top-level
  (`mother_id == 0`) and must count as itself. `expedition::create`, `invite` and
  `reply_invite` compare top-level factions (`nation::top_level_faction`), so two citizens of
  different nations cannot found or share a guild; a citizen of another nation is not invited
  and a forged reply does not join. The check applies to every player, not only nation
  citizens: a faction-102 player still joins a faction-101 guild (one alliance) and a pirate
  cannot join a non-pirate guild; the refusal is silent (no 1.2 error id for it; the C# had no
  check, and `Character.SetFaction` already expels on a mother mismatch). Evidence: the C#
  `CreateExpedition` mother check, the client's top-level scan (`FUN_398a2f40`) and
  `Character.SetFaction`'s mother mismatch kick.
* **NATION-14 (temporary factions):** every immigration check uses
  `db_character::persisted_faction` and refuses a `faction_temporary` character with 693: a
  duel / battlefield Red-Blue team faction is not the player's real faction, so a guildless
  pirate in an accepted duel cannot "return to origin" (the pirate check sees the saved 161)
  and a non-nation player in a duel cannot leave a nation that is not theirs. The same saved
  faction is compared by the guild create / invite / reply rule, so a duel does not refuse a
  legitimate guild join. The client has no code for "temporary faction"; the general
  immigration refusal is used (documented here).
* **Immigration, return and kick (stage 2):** `CSFactionImmigrationInvite` (0x015),
  `CSFactionImmigrationInviteReply` (0x016), `CSFactionImmigrateToOrigin` (empty, 0x017) and
  `CSFactionKickToOrigin` (0x018) now run the verified flows. An accepted invite and a
  successful return use `faction::set_faction` (broadcast, houses, guild and party side
  effects) and immediately persist `characters.faction_id` under `SaveManager`'s transaction
  lock; the periodic save writes the same column. Return refuses the nation owner (734) and
  expedition members (735) with the client's order (770, 694, 693, 733, 734, 735). Kick
  resolves the target online or from `characters` by name (deleted rows excluded); an offline
  citizen's `characters.faction_id` (guarded by the expected nation) and stored
  `housings.faction_id` move in one transaction, and the owner's loaded houses move with them
  under the save lock. A target that logs in between the lookup and the write is kicked
  through the online path instead. The same in-memory house move now happens for offline
  members at independence. The
  origin is `character_templates.faction_id` (the same data as the client's
  `FUN_398a2660(race)`).
* The national tax CS packet is 0x013 (`FUN_392f42f0`; owner-approved exception to the
  never-edit `opcodes.rs` rule). Stage 4 moves it to `systems::social::packets::misc` and runs
  the real handler (RENAME-8); the rename and monument are RENAME-7 / RENAME-9.
* **NATION-15 (friendly treaty; accept path found, mail fields INFERRED):** the 1.2 client's
  `read_mail.alb` `acceptNationFriendButton` calls `X2Mail.PayCurrentMailChargeMoney`, the same
  charge-money button as the house-tax bill, so the accept CS opcode is the existing
  `CSPayChargeMoneyPacket` 0x0a0 (not a new opcode). The server sends mail type 25
  `MAIL_NATION_REQUEST_FRIEND` to the king with `billing_amount` = `content_configs` 51
  (2,000,000 copper); paying it debits that cost, deletes the mail and moves the relation to
  friendly for `content_configs` 46 (168 h) with hostile as the next state. The target faction id
  is carried in the mail's free `extra` tag (the house tax uses it for its zone group / house id);
  that carrier and the exact 1.2 period/next-state values are INFERRED because no 1.2 server code
  or capture shows them. The mail body is plain text (the client's `FillNationRequestFriend*`
  functions render the fields it receives). Treaty mails are sent 48 h (`content_configs` 54)
  before a friendly period ends and once after a declared hostile window really ended; the
  faction-166 template rows (`next_state` 0) are never offered. A per-(nation, target) last
  offer time (table `nation_relation_offers`, monotonic `GREATEST` update) enforces a cooldown of
  at least `content_configs` 54, so a deleted or declined mail is not re-sent every tick and the
  cooldown survives a restart. At startup `rebuild_treaty_mail_tracking` re-reads the stored
  type-25 mails (king, `extra` target, live relation window), re-creates the tracking and
  deletes stale ones; removing a tracking entry (accept, hostile declaration, expiry) deletes
  its mail. Auto-deletion of an unaccepted treaty mail after its deadline is not implemented
  (capture-pending).
* **NATION-16 (hostile declaration; client validator mirrored):** `CSFactionDeclareHostile`
  0x019 runs the checks of `FUN_392f9430`: `FUN_398a4750` first (king / faction rights -> 32
  `FACTION_RIGHTS`, else an invalid subject -> 718), then `FUN_398a3660` (already hostile -> 718
  `FACTION_DIPLOMACY_INVALID_SUBJECT`). Only `system_factions.is_diplomacy_tgt` rows are
  subjects (148 and 149 in 1.2). The 1.2 `ui_texts` ids 523/527 exist, but the client's own
  pre-send path never reaches them (the button is hidden for an already hostile subject); the
  server therefore mirrors 718 and leaves 523/527 capture-pending. Success writes
  `(state, expire_at, next_state)` immediately (the periodic save writes the row again) and
  sends `SCFactionDeclareHostileResult` to the king plus
  `SCFactionSetRelationState` to the online members of both sides (the client refreshes its
  nation UI only when the relation involves its own nation, `FUN_392f9540`; members of both
  sides is INFERRED as the useful target set). `diplomacy_link_id` is loaded into
  `SystemFaction`; a nation's relation to a linked guard / front faction is resolved through the
  linked alliance before the nation's stored row (compact links: 134 -> 149, 135 -> 148,
  157 -> 149, 167 -> 148, 168 -> 149, 171 -> 148, 173 -> 149), so a treaty with 149 turns the
  Haranya guards (134/157/168/173) friendly even though the faction-166 template stores
  explicit HOSTILE rows for them (INFERRED; verified by a real-template test).
* **NATION-17 (relation expiry; period units INFERRED):** a 60 s subscriber on the existing
  `services::tick::TickManager` (started by social's `System::start`) moves every relation whose
  `expire_at` is due to `next_state` (0 keeps the current state), clears the expiry, persists
  the row (immediate `UPDATE` on the tick plus the periodic save), and sends
  `SCFactionSetRelationState` then `SCFactionRelationExpired` (only when the state actually
  changed) to both sides. Hostile declarations set `expire_at` = now + `content_configs` 50
  (21) and `next_state` = hostile: the period is the no-treaty window, and the relation stays
  hostile until a treaty (the dialog text `hostile_setting_dlg` says so); the unit is UNKNOWN and
  INFERRED as hours. When that window ends the row keeps `next_state = hostile` as the
  "window ended" marker (the template rows keep 0 and are never offered); a friendly expiry
  deletes the 48 h lead mail and waits a fresh lead period before another offer. Friendly
  treaties use 46 (168 h) and 54 (48 h before the end), also INFERRED hours: the 1.2 client
  does not read 46 / 50 / 54, only 49 (immigration level) and 51 (cost). A GM-only
  `nation relation` command shortens an expiry for live checks.
* **Stage 3 known deviations (not fixed, < 5):** the debit, mail deletion and relation write
  are not one transaction; a crash between them can leave a charged mail or a payable treaty.
  Per-change `tokio::spawn` persistence can reorder relation writes (offers use `GREATEST` and
  are monotonic). The treaty pay path keeps `pay_charge_money`'s lock order (economy op -> mail
  store -> character), while `store_quest_rewards_locked` takes character -> mail store. The
  live `nattack` step proves the positive attack; the unit tests cover the refusal before the
  declaration (relation overlay).

#### Guild rename (expedition)

`CSRenameExpeditionPacket` (0x009, `{ id, name, is_expedition }`) renames a guild when
`is_expedition` is true; the nation half (`false`) is the nation rename (RENAME-7, below). This
section documents the guild flow (the GM-granted right model is described under "Guild
functions (2026-11-05)"); the nation specifics follow below.

Evidence (1.2 client):

* The rename dialog has no client-side opener. `SCExpeditionShowRenameUI` (0x00f, empty body)
  is handled by `FUN_391d2310`, which calls `FUN_392f8db0(0)`; that looks up the player's
  expedition and pre-fills the current name, and `FUN_398a36e0` rejects a player who is not
  the faction owner. The dialog's OK handler (`dialog_handler.alb` proto 25.0) calls
  `X2Faction.RenameExpedition(newName)`; the native side (`FUN_39472d70` -> `FUN_392f8f50` ->
  `FUN_397ab420`) fills `{ id = my expedition id, name, is_expedition = 1 }`.
* `SCFactionRenamedPacket` (0x011, `{ id, name, byGm }`) is handled by `FUN_391d23f0` ->
  `FUN_392fdce0`: it replaces the cached faction name, posts the `change_expedition_name`
  chat notice, and clears the cached `allow_change_name` when `byGm` is false.
* `SCFactionRetryRenamePacket` (0x012, `{ xpdt, errorMessage, name }`) is handled by
  `FUN_391e5520`: `xpdt` non-zero re-opens the expedition dialog with `name` pre-filled
  (`FUN_392f8db0`), zero the nation dialog. The handler does not read `errorMessage` in the
  decompile, so the code is informational for 1.2; the re-opened dialog is the visible result.
* The error codes are the creation ones (compact/client error table `FUN_3956b860`):
  296 `expedition_name_character`, 297 `expedition_name_length`, 298
  `expedition_name_badlist`, 299 `expedition_name_exist`. Creation and rename share
  `systems::social::expedition::validate_name` (length, the configured regex, the
  character-name block list, and a case-insensitive uniqueness test that skips dismissed
  guilds and the guild itself).
* The 1.2 compact database has no rename item, skill, special effect or `content_configs`
  entry; the only 1.2 client trigger is the server packet. The GM hook `/testguild rename`
  sends it (the access-level entry `testguild: 100` already existed without a C# command).

Rules and deviations:

* **Owner only.** The client's own pre-check and the server both require the guild owner
  (`faction.owner_id` plus membership; a request naming another guild is refused).
* **Right plus offer.** The leader needs the GM-granted `allow_change_name` right
  ("Guild functions" above): no right is `FACTION_RIGHTS` (32). On top of it, the server keeps
  the one-shot offer created when it sent `SCExpeditionShowRenameUI`; the offer stores
  `character -> expedition id`, so a request for another guild is refused even when the
  character holds one. A request with the right but without the dialog is a forged packet and
  is refused with error 307 `EXPEDITION_BAD_STATUS` (a server-side addition: the 1.2 C# only
  logged the packet). The offer survives an invalid / taken name so the retry dialog can
  correct it, is consumed on success, and is dropped on leave-world
  (`social::session` / `world::session::leave_world_task`), on guild disband
  (`dismiss_expedition`) and on an owner change (`apply_owner_change`). The map is capped
  (`MAX_RENAME_OFFERS`). `grant_rename` and the login prompt record the offer too.
* **Name rules shared with creation.** `validate_name` enforces the UTF-16 length, the
  configured regex, the character-name block list and a case-insensitive uniqueness test that
  skips dismissed guilds and the guild itself (re-submitting the current name in another case
  is therefore a valid case change). The C# create path compared exactly and did not use the
  block list.
* **One shared namespace (RENAME-3).** Guild creation, guild rename, independence and nation
  rename also check `rename::faction_name_in_use` over static factions, expeditions and
  nations (exact comparison, the guild / nation being renamed excluded);
  `rename::lock_names` keeps that check and the name write together across the expedition and
  nation stores, which closes the create-vs-rename race (RENAME-6).
* **Broadcast scope.** `SCFactionRenamed` goes to every online player
  (`WorldManager.broadcast_to_server`), because the faction list (`SCFactionListPacket`) and
  `SCFactionCreatedPacket` are server-wide too; sending it only to members would leave every
  other client's guild list stale. The guild chat channel is renamed as well, and the right is
  spent unless the rename is a GM one (`by_gm = true` keeps it).
* **Persistence** is the existing `Expedition.Save` (`REPLACE INTO expeditions`, including
  `allow_change_name`), awaited before the broadcast.
* **capture-pending (RENAME-1):** the production trigger (item / merge collision / quest) is
  not established for 1.2; only the GM commands and the test hook are wired. The nation rename
  reuses the shared shape / retry / broadcast helpers and the shared namespace (RENAME-7,
  below).
* **Known, not fixed (RENAME-2, 4, 5):**
  * **RENAME-2:** a failed save still broadcasts. `save()` only logs the error (and restores
    the pending removals); the in-memory name is broadcast as if persisted, and there is no
    periodic expedition save to retry it (`systems::social::System::save` covers ranking /
    nations / bounties / crime, not expeditions). The C# call site was synchronous too. A
    nation rename has a periodic nation save, so its failed immediate write is retried.
  * **RENAME-4:** the guild uniqueness test lowercases with Rust's Unicode rules, so case
    variants collide (`Knights` / `knights`); space and Cyrillic look-alikes remain distinct
    (`K nights` / homoglyphs), and the cross-store namespace check is exact. This matches the
    MySQL `utf8_general_ci` intent but not its full collation.
  * **RENAME-5 (capture-pending):** the `SCFactionRetryRenamePacket.errorMessage` field width
    is unverified. The C# writes `i16`; the decompiled handler reads the name at `+0x14`,
    which suggests four bytes of error/padding, and it does not read the field at all. The
    C# layout is kept until a capture or server decompile settles it.
#### Nation rename (RENAME-7)

`CSRenameExpeditionPacket` (0x009) with `is_expedition = false` now renames the caller's
nation. The 1.2 client's paths are `SCNationShowRenameUIPacket` 0x010 (`FUN_391d2380` reads
nothing and calls `FUN_392f8e80(0)`, which opens the dialog only when the player owns a user
faction) and the dialog's OK handler (`X2Faction.RenameNation` -> `FUN_392f8fa0` ->
`FUN_397ab420` writes `{id = my nation, name, is_expedition = 0}`). `SCFactionRenamed` 0x011
updates the cached name and `SCFactionRetryRename` 0x012 with `xpdt = 0` re-opens the nation
dialog with the rejected name (`FUN_391e5520`). Who sends 0x010 in production is UNKNOWN; the
GM hook `/nation rename` (and the optional name form) and the tests call the same service.

* **King and offer only.** `rename_nation` requires the one-shot offer created when
  `SCNationShowRenameUI` was sent, bound to `character -> nation id`; a forged request without
  it is refused with 748 `ONLY_NATION_OWNER`, a non-king with the same code. The offer is
  consumed on success, survives an invalid / taken name (retry dialog), and is dropped on
  leave-world and on a king change. The map is capped (`MAX_NATION_RENAME_OFFERS`).
* **One shared namespace.** The name shape is `rename::validate_faction_name` (the guild
  config regex and 32 UTF-16 units). `rename::faction_name_in_use` rejects a nation name that
  an expedition, another nation or a static faction already uses; the founding expedition is
  excluded so a nation may carry its guild's name (NATION-1). The same check now runs in
  guild creation, guild rename and independence; `rename::lock_names` makes the check and the
  write one step across the two stores.
* **Wire and persistence.** Success writes the in-memory nation name, immediately `UPDATE`s
  `nations.name` under the save lock (the periodic nation save retries a failed write) and
  broadcasts `SCFactionRenamed { id, name, byGm = false }` to every client (the faction list
  is server-wide). An invalid name or a taken one answers `SCFactionRetryRename { xpdt =
  false, errorMessage, name }` with 296 / 297 / 299; the client does not read the error field
  in the decompile (RENAME-5), the re-opened dialog is the visible result.
#### National tax (RENAME-8)

`CSUpdateNationalTaxRatePacket` 0x013 `{zoneGroup u16, rate u32}` (`FUN_392f42f0` ->
`FUN_397ab4d0`) is handled in `systems::social::packets::misc` (moved from the log-only
`packets/c2g_misc.rs`). The client's confirm validator `FUN_392f4820` is mirrored: `rate >
1000` -> 684 `DOMINION_NATIONAL_TAX_INVALID_RATE`; not the owner of the dominion-owning
expedition's top-level faction (or an unknown / unowned dominion) -> 685
`DOMINION_NATIONAL_TAX_NO_PERMISSION`; the same rate is ignored
(`equal_tax_rate_warning`); a change within 86400 s -> 725 `DOMINION_NATIONAL_TAX_LATER`.
Success stores the rate (permille/10, 0-1000) and `last_national_tax_rate_changed_at` on the
`dominions` row (a targeted `UPDATE` of those two columns only; the full row, including the
treasury, stays with the periodic financial save, which is the only path allowed to commit
treasury changes with houses / mail / wallets) and broadcasts `SCNationalTaxRate` 0x021
`{zone, taxRate, prevTaxRate, changedTime}` to every client, because the client handler
`FUN_392f39b0` updates a server-wide dominion record and shows ui_texts 5675.

* The rate is a king/lord split of collected tax excluding the peace fund (ui_texts 5536).
  A successful change also stores mail 23 (`NATIONAL_TAX_RATE`) for the nation's citizens
  (online at once; offline through the saved `characters.faction_id` query after the housing
  lock is released): sender `.nationalTaxRate`, title `title(<zone group>)`, body
  `body('<nation>','<zone>','<new>','<old>','<changedAt>')`. The parameter order is the
  client's `locale.mail.nationalTaxRate.body` (proto 110 of `x2ui/baselib/locale_helper.alb`);
  `read_mail.alb` reads `oldTaxRate` / `newTaxRate` back from the table. The audience and the
  exact 1.2 body are INFERRED from the client body fields and the nation member broadcast
  paths, and are capture-pending (`NATION-TAX-MAIL-1`). The weekly receipt mail 24 and the
  split arithmetic remain unimplemented: no 1.2 server code or capture defines them
  (capture-pending `NATION-TAX-MAIL-2`). A new dominion starts at 0; the C# set 500 on claim,
  which is not 1.2 evidence.
* `CSUpdateDominionTaxRatePacket` 0x012 success also stores mail 8 (`TAXRATE_CHANGED`) for
  the owners of houses in the territory (deduplicated): sender `.taxRateChange`, title
  `title(<zone group>)`, body `body('<old>','<new>')` (the client's
  `locale.mail.taxRateChange.body`, proto 79). The recipient set is INFERRED from the client's
  `X2House.GetTaxations` property-tax breakdown and the existing house-tax mail audience;
  capture-pending (`TAX-RATE-MAIL-1`).
* 684/685/725 are the client's own error ids (`error_message.rs`), so a forged packet gets
  the same answer the client would have produced locally.
* Test hook: GM `nation dominion <zone>` seeds an **in-memory-only** `DominionState` for the
  caller's guild with a placeholder tower identity (no live castle tower is needed for the tax
  handler). The state is marked `ephemeral`, so `prepare_save` never writes the placeholder
  tower (which would fail `validate_loaded_identity` on the next startup); the live suite uses
  it because a castle cannot be claimed headlessly.

#### National monument (RENAME-9)

`SCNationalMonumentChanged` 0x022 `{zone u16, nationalMonumentDbId i64, x, y, z}` (handler
`FUN_392f35f0` stores the db id and the three coordinates in the dominion record; the same
fields are in `DominionData`). The monument is the territory-statue doodad named by
`siege_zones.monument_doodad_id`: the six 1.2 castle zone groups map to doodads 7229-7234
("Territory Statue: Akrites/Calleil/Ezi/Gon/Hora/Nui"), matching the placement skills
23681..23688 and the removal skill 23682 by name. Placement spawns that persistent doodad and
writes its `doodads` row with `save_now` *before* storing the db id and world position on the
`dominions` row (`national_monument_db_id`, `_x`, `_y`, `_z`); a crash leaves an orphan statue,
never a dominion pointing at a missing one. The reference is written with a targeted `UPDATE`
of the four monument columns; the full row stays with the periodic financial save.
`DominionData` at world entry carries the same values. Removal zeroes the dominion columns
before deleting the doodad row/object and broadcasts db id 0. A change of owner (settlement
claim, siege capture, forced release) clears the fields, broadcasts db id 0 and deletes the old
owner's statue through the same ordered path.

* **Gates (client error ids, text in `ui_texts`).** 748 `ONLY_NATION_OWNER` for a caller who
  is not a nation king; 757 `NATION_OWNER_AT_POS_ONLY` for the king of another dominion or
  more than 20 m from the point (the statue skills' `skills.max_range`; the exact authored
  rule is capture-pending); 750 `NO_MORE_NATIONAL_MONUMENT` when the dominion already has one
  or has no statue template; 751
  `CANT_PUT_UP_NATIONAL_MONUMENT_DURING_SIEGE_PERIOD` while a siege is scheduled or active.
  Removal is idempotent and not blocked by a siege.
* **capture-pending:** the real placement trigger (skills 23681..23688 are InteractionEffect
  rows with no supported server special effect), its cost / item and the authoritative range;
  whether 750 is per dominion or per nation (implemented per dominion, because the client
  stores the fields per dominion); and the client's reaction to a removal broadcast with db
  id 0 (implemented as an inferred clear).
* The migration `SQL/updates/2026-11-03_aaemu_game_dominion_monument.sql` adds the four
  columns and the base `SQL/aaemu_game.sql` carries them.
* **Stage 4 known, not fixed:**
  * **Expedition id leak:** `create_expedition` and `declare_independence` allocate the
    expedition id before the final shared-name re-check under `rename::lock_names`; if that
    re-check fails, the id is not released (an allocator gap, not a duplicate). The early
    check makes it unlikely; no test covers the race.
  * **NATION-1 shared-name edge cases:** a nation created with its founding guild's name
    cannot later rename back to that name (the shared check excludes the nation but sees the
    guild); re-submitting the current name is an early no-op that consumes the one-shot
    offer without sending `SCFactionRenamed`. The 1.2 client's behavior for these cases is
    unverified.
  * **Test gaps:** the temporary `nation-stage4` live suite is not committed (fixed names and
    the memory-only dominion hook); a castle capture / forced release with a live statue is
    covered by unit tests and the ordered DB path, not by a live siege. The MySQL-gated tests
    use connection-local temporary tables.

### systems::housing

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `HousingGameData` (`item_housings`, `housings`, `housing_binding_doodads` + `Data/housing_bindings*.json`, `housing_build_steps`, `housing_decorations`, `item_housing_decorations`, `housing_groups` incl. `allowed_tax_delay_week`, `PostLoad`), `TaxationsManager` (`taxations`) | `systems/housing/data.rs` | done | Gated real-DB test `housing_real_compact_db` (row counts, bindings positioned, shipped tax delays). Duplicate build steps fail the load (C# `Dictionary.Add`). A NULL / missing `allowed_tax_delay_week` column loads as `None` (WP-H1). |
| `HousingManager` (all methods), `House` behaviour (`CurrentStep` setter + bound doodads, `AddBuildAction`, `Spawn` / `Delete` / visibility of `AttachedDoodads`, `AllowedToInteract`, `OnDeath`, `Save`), `HousingTaxTask`, `MailForTax` content | `systems/housing/{manager,ops,db}.rs` | done | Build (tax certificates or gold), permission, rename, recover toggle, demolish (+ `ReturnHouseItemsToOwner` mails), sale / sale markers / cancel / buy (+ mails, furniture owner), decoration, taxes (bills, untouchable / removal buffs, demolition timer every 10 s), death, `GetHouseAtLocation`, act-ability bonus. Items / money / mail go through `housing::hooks::HousingEconomyHooks` (economy installs it). **Tax grace (WP-H1, housing-farm-ucc#4 / tables-ignored-columns#8):** `tax_due_date` is `protection_end - allowed_tax_delay_week * 7 d` of the housing group containing the house (resolved like placement, first area/group wins); a missing group / area / world / column, or a negative value, keeps the C# fixed 7 days. Shipped data: only group 13 authors 0 and it is `can_extend = false`, so it never receives a bill and keeps its authored 7-day lifetime; its `SCHouseTaxInfo` plaque no longer counts the last week as an unpaid week (`weeks_without_pay` stays -1 until the protection end). Every other shipped group authors 1, so billing, removal and mail dates are unchanged. `MailForTax` (economy) carries the resolved `allowed_tax_delay_week` the same way. |
| Housing packets (11 CS, 14 SC incl. `SCLoginCharInfoHouse`, `SCMyHouse`) | `systems/housing/packets.rs` | done | Byte-exact tests. `CSChangeHousePayPacket` has opcode 0xfff in 1.2 (not registered). `SCHouseOwnerNameChangedPacket` is social's (`packets/misc.rs`). |
| `HousingManager.ReturnHouseItemsToOwner` demolish mail delay | `systems/housing/ops.rs` (`return_house_items_to_owner`) | deviation | **Deviation:** the C# set `RecvDate = UtcNow.AddHours(failedToPayTax ? HoursForFailedTaxToReturnHouse (22) : 0)`, holding the design and returned furniture of a tax-failure demolish for 22 hours. The Rust delivers every demolish mail immediately (server owner's decision, 2026-11-08: players found their house gone and nothing in the post); `HoursForFailedTaxToReturnHouse` was removed. `failed_to_pay_tax` still means no deposit refund. |
| `HousingManager.SetRemovalDebuff` (abandoned house) | `systems/housing/manager.rs` (`set_removal_debuff`), `mod.rs` (`is_abandoned_house_removal_damage`) | deviation | **Deviation:** the C# applied only buff 2250 "Removal Debuff" (10 damage / 15 s, so a 150,000 HP Chalet needed about 2.6 days and never visibly burned). The Rust also applies the unused 1.2 buff 39 "Burning House" (`불난집`, "The house is ablaze."; duration 99999999, tick 1000 ms, not `system`, remove_on_death; tick effect 286 = DamageEffect 106, 30 fixed Magic damage) to abandoned, demolished houses, so they visibly burn and fall in about 83 minutes (server owner's decision, 2026-11-08). It follows the removal debuff exactly (on with it, off otherwise; never on lodestones or forts). Effect 106 from the buff-39 template is authorized like 1876 (caster == target == the unowned house, ptr-equal server template, live ACTING buff entry); because buff 39 is not `system` that flag is not required for it. Tests: `abandoned_house_burning_*`, gated `real_compact_house_burning_buff_*`. **Deviation:** hits by other units on an abandoned (tax-demolished, ownerless) house deal at least `max_hp / Housing.AbandonedHouseHitsToBreak` (default 5; 0 disables), `housing::abandoned_house_hit_damage` in the `DamageEffect` path; the house's own burn/removal ticks are unchanged (server owner's decision, 2026-11-08). Tests: `abandoned_house_hit_*`. |
| `House.Hp` persistence (`housings.hp`, commit `1766edd85`) | `systems/housing/manager.rs` (`load_house_row`, `loaded_house_hp`, `set_current_step`, `construction_hp_after_step`) | deviation | **Deviation:** the C# never persisted HP (`MaxHp => Template.Hp`; HP was full after every load). Rust persists it, and the first version clamped an unfinished house's HP to its construction-step maximum on load without raising it on completion, so finished houses came back at 25% HP and were drawn in the heaviest damage state (testers, 2026-11-08). Now an owned, ordinary, unfinished house loads at its step maximum (a dead one stays at 0; ownerless and tower/fort houses keep their stored value), every runtime step change raises HP by the same delta as the effective maximum (`hp + new_max - old_max`, clamped), and a finished owned non-tower house whose stored HP is exactly one construction-step maximum is restored to full on load (logged at info). Real damage is kept. Tests: `manager_tests::*construction*`, `finished_house_*`, `unfinished_house_*`, `real_damage_survives_a_step_change`. |
| Siege larceny window (`ui_texts` 3334 `in_siege_warning`, category 70) | `systems/housing/dominion/larceny.rs`, `dominion/state.rs` (`siege_larceny_window_open`), `npcs/doodad/{phase.rs,funcs/skills.rs,funcs/items.rs}`, `social/crime.rs`, `Housing.SiegeLarceny` / `Housing.SiegeLarcenyCrime` | added (INFERRED, capture-pending) | The only evidence is the 1.2 client string (grade A): KO "공성측이 수호탑을 점령하고 있는 동안에는, 농작물과 설치물을 타인이 사용할 수 있습니다."; EN (`localized_texts` 172569) "If a guard tower is seized by opposing forces, opponents can loot any crops, animals, or trees on those lands." The Korean says "anyone else" (타인), the English says "opponents" — a wording conflict; the EU default is `Opponents` and `Anyone` implements the Korean reading. No 1.2 table implements the rule (no siege/permission column in the doodad, house or farm tables), so the rule set is server logic: (1) the window is open only while the territory's siege is active (`SiegeState::active_at`) **and** its Archeum Lodestone is destroyed/occupied (`DominionState::tower_destroyed`, not a sentry tower) — the exact retail trigger is UNKNOWN and this is the closest state; it closes on siege end, on `reset_tower_destroyed`, and on a capture / release (both clear the flag); scope is the territory's zone group of the *house*; (2) who: `Opponents` (default) = the existing faction relation between the player and the house faction (`update_owned_housing_faction` mirrors the owner; same relation `can_attack` consults), `Anyone` = any non-owner; unknown/zero factions are never opponents; (3) what: `doodad_owner_type::HOUSING` doodads in `doodad_groups` 2 (벌채 - 나무), 3 (채집 - 약초, the plantable herbs / flowers: rose, lily, cotton, lavender, mushroom, ginseng, sunflower, ...), 5 (축산 - 가축), 12 (농사 - 작물), 40 (농사 - 해양 작물) — crops, livestock, trees, herbs/flowers and their garden installations; houses, furniture, doors, coffers and storage are not in those groups and keep their own gates; public farms are `CHARACTER`-owned and already public, so unaffected; (4) the window grants the produce, never the uproot (`skills.id` 13789, `larceny::is_removal_skill`; it takes the crop to a `DoodadFuncFinal` phase), so `phase::use_doodad` and the `funcs::skills::use_` fallback both skip the window for it; every `Doodad.Use` entry path passes `phase::use_doodad` (skill-bound `DoodadFuncUse` and the skill-less `DoodadFuncLootItem` / `LootPack` / `Cutdowning` form of `CSLootOpenBag`), `funcs/skills.rs::use_` keeps its own house gate for direct `use_func` callers (e.g. `CheckGrowth`), `DoodadFuncRecoverItem` is **not** relaxed: it keeps the normal house permission, because a recover hands the whole installation (barn, aging rack, coop, young animal, flower bed) to the player as an item, which is not "loot" (the use/harvest funcs — `Use`, `CropHarvest`, `Cutdown`, gather, `LootItem`/`LootPack` — are the window's scope; the `doodad_funcs.perm_id` gate still applies inside it, so the group-5 (baby livestock) and 4594 rows that carry perm 1 stay owner-only and the client filter hides them too); (5) crime: by default no footprint evidence inside the window (`Housing.SiegeLarcenyCrime = false`; the text says the crops are "used", not stolen). An explicit `true` turns the normal evidence back on only when the window (not normal house permission such as PUBLIC) granted the access; a house-garden theft's victim is the house owner (`social/crime.rs` now resolves `HOUSING`); item 18461 / 3 crime points are the ordinary trial result and are untouched. **Client:** no server packet change; the 1.2 client shows `inSiegeWarning` from its own `X2House:IsSiegePeriod()` (Lua `housing/maintain_window.alb` blocks 5/7, `housing/material.alb` block 8; reader `FUN_3947c030`), which is true for the whole siege period and is therefore broader than the server window; the actual interaction gate is server-side `SCErrorMsg` `interaction_permission_deny` (232), so the client warning cannot be narrowed to the tower state. **Deviation (central gate, 2026-11-09):** the new `phase::use_doodad` gate also refuses the skill-less `DoodadFuncLootItem` / `LootPack` / `Cutdowning` form of `CSLootOpenBag` on house-land farm doodads for players without house permission **outside** sieges. On main (and in the C#) only `DoodadFuncUse` checked the house, so that path could loot a private garden without permission; the shared gate closes that existing permission gap. **Deviation (actor scope):** `Opponents` means the existing hostile **faction** relation to the house owner's faction, not the siege side: a registered attacker expedition that shares the defender's faction cannot loot, while a hostile-faction player who is not in the siege can. The alternative reading (`siege.attacker_expedition_id`) is a possible future option; no 1.2 evidence distinguishes them. **Capture-pending:** exact "occupied" trigger, KO-vs-EN actor scope and the no-crime choice. Tests: `dominion::larceny_tests` (opponent allowed only while the lodestone is held; closes on retake, siege end and release; herbs/group 3; non-opponent vs `Anyone`; house/furniture/coffer refusal; installations cannot be recovered while their produce can be harvested; skill-less loot path; evidence default off and switch on; PUBLIC access is not theft; real-compact group ids). |
| `CraftEffect` (`WorldInteractionGroup.Building`) | `systems/housing/mod.rs::apply_effect` | done | Routed first by `skills::ext::ExtHooks::route_effect`; quest `DoDoodadInteractionEvents` after it. |
| `SpawnManager.Load` / `SpawnPersistentDoodads` housing parts, `LoadAccount` / `CSListCharacter` / `CSSelectCharacter` / `Character.Load` / `DeleteCharacterAssets` housing parts | `npcs/spawn_manager.rs`, `packets/c2g.rs`, `world/session.rs`, `db_character.rs` (one call each) | done | `LoadPlayerHousing` + `SpawnAll` before the persistent doodads, house parenting + bound doodads, `ReconcileBoundDoodads` (`UsePersistentHouseDoodads`). |
| Placement validation | `systems/housing/placement.rs`, `systems/npcs/subzone.rs` | done | Original 1.2 housing XML `value1` tags resolve `housing_areas`/groups/categories; finite world positions, the whole `4n` footprint grid (`FUN_39331f70`/`FUN_39331ba0`), snapped-box house overlap (`FUN_39331f30`/`FUN_393349e0`), unit/doodad overlap with house-attached doodads skipped (`FUN_39335860`), authored `max_construct_count`, account eligibility, and nonrenewable beginner plots are checked server-side under the world placement guard. The placement / purchase / decoration range uses the caster's composed world position, so a mounted rider, a seated player or a ship passenger is measured at their world location instead of the parent-relative seat local. Terrain and mode-1 (n=0) model-box checks are omitted because the server has no render-model bounds (never stricter than the client). World-scoped land checks also guard planting and housing crafting benches. Real-data test: `placement::tests::real_area_338_footprint_accepts_only_the_fitting_spot`. |

Economy hooks (`server.systems.housing.hooks`): installed by `economy::bridges::EconomyForHousing`. Housing API for others: `housing::{house_by_id, house_at_location, allowed_to_interact(_by_id), pay_weekly_tax (MailManager tax payment), act_ability_bonus_at (CharacterCraft), update_owned_housing_faction, demolish_by_player}`.

### systems::quests

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `QuestManager.Load` (contexts, components, base acts, all 64 `quest_act_*` detail tables, supplies, item / NPC groups, `UpdateQuestComponentActs`) | `systems/quests/data.rs`, `templates.rs` | done | Same table order (act order inside a component = objective indices), first-base-act linking, tutorial category skipped. Validated against compact.sqlite3 (`real_compact_db_quest_tables`: 4775 quests, 17720 components, 19047 acts, per-type counts). Exhaustive record accounting and outstanding gameplay failures: `Docs/wiki/Quest-Audit.md`, `rust/tools/quest_audit/audit.py`; port coverage does not certify every quest as playable. |
| `Models/Game/Quests` (`Quest`, `NewQuestCode`, `QuestStep`, `QuestComponent`, `QuestAct`, all `Acts/*` + `UnusedActs/*`) | `state.rs`, `engine.rs`, `acts.rs` | done | Live quests on `Character::quest_state` (handles + act paths instead of object references); `c.quests` rows re-derived after every operation. Step machine, objective status / percent, rewards, drop / cleanup, event subscriptions in invocation-list order (`-=` removes the last occurrence). |
| `CharacterQuests` (AddQuest*, DropQuest, completed flags, reset / daily reset, LetItDone, item destroy, `GetActiveActsWithUseItem`, Load) | `engine.rs`, `events.rs` | done | Save = existing `Character.Save` rows. |
| Quest failure (`FailQuest` from `CheckTimer` expiry and `CheckGuard` NPC death) | `engine.rs` (`fail_quest`), `acts.rs` (`CheckGuard`, `CheckTimer` deadline check) | added | The C# `FailQuest` only set `Step = Fail` (no status, no packet). The Rust sets `Status = Failed` and sends `SCQuestContextUpdatedPacket` with that status: the 1.2 client learns a failure from the status, not from 0x146 (`SCQuestContextUpdatedPacket` -> `OnQuestContextUpdated` `FUN_391dbe50` -> `FUN_3937bdf0`, whose status-2 branch raises the quest condition event and logs "failed"; `ui_texts` category 6 `condition_fail_msg`). 0x146 stays the answer to a refused client report. A failed quest stays failed and droppable. During `CharacterQuests.Load` (`st.loaded == false`) the update is skipped because the following `SCQuestsPacket` already carries the Failed row. `CheckTimer` ignores a queued `TimerExpired` while the live deadline (`q.time_ms`) is still in the future, so a stale task cannot fail a restarted run. |
| `CSRestartMainQuestPacket` (0x0fd) | `packets.rs` (`CSRestartMainQuestPacket`), `engine.rs` (`try_restart_quest`, `restart_quest`, `install_quest`) | added | The 1.2 client sends it when the player asks to restart a main quest: `X2Quest.QuestContextRestart` -> `FUN_3949b780` -> `FUN_3937b200` / `FUN_3937a8d0`, gated to an active quest whose status is not 5 (Completed), `detail_id == 2` and its own 3 s / labor checks; the reply is a normal `SCQuestContextStartedPacket`, which `FUN_3937b5b0` turns into `QUEST_CHAT_RESTART` (`ui_texts` 391). The server mirrors the active/status and `detail_id == 2` gates, adds a 3 s per-character throttle, and gates on `restart_on_fail` (**INFERRED**: the client never reads the flag; 211 of the 222 flagged quests are `detail_id` 2). The restart discards the run (items, subscriptions, timer, quest id; the Start-component `ConAcceptItem` acceptor items are kept) and installs a fresh quest object. A refused request only logs and changes nothing. The original server's exact reply sequence is **QUEST-1 (capture-pending)**. |
| `quest_contexts.successive` | `templates.rs` (`QuestTemplate.successive`) | done (client display only) | 37 rows. The 1.2 client reads it in the quest objective text functions `FUN_394a3b60` / `FUN_394a3f20`: when set, the tracker lists only the current Progress component's objectives (status "successive"), otherwise every unfinished Progress objective; Ready/Failed show their status. No packet or server logic: **QUEST-2 resolved**. |
| `Quest.DistributeRewards` exp / copper pool | `engine.rs` (`distribute_rewards`), `acts.rs` (`SupplyExp` / `SupplyCopper`) | deviation | **Deviation:** a Reward component `QuestActSupplyExp` / `QuestActSupplyCopper` act replaces the level-based `quest_supplies` exp / copper instead of being added on top of it (the C# added both). 1.2 client oracle + 2014 data, details in the deviations list. |
| `QuestManagerEvents` + other C# entry points (OnItemUse, OnKill, cinema, mate level) | `events.rs` | done | Public event API; nested calls from hooks are queued. |
| `QuestManager` evaluation queue / timers / daily reset task | `mod.rs` | done | `QuestManagerRunQueueTask` as a 1 ms tokio task; timers as tokio tasks; daily reset at 00:00 UTC. |
| `UnitRequirementsGameData` (`CanComponentRun`, `CanTriggerSphere`) + `UnitReqs.Validate` | `unit_reqs.rs` | done | `ReqSubject` / `ReqEnv` traits (owner = locked character, target = snapshot). `CanUseSkill` is the skills subsystem's (can reuse `validate`). |
| `SphereQuestManager`, `SphereQuest*`, `SphereGameData` (spheres, sphere_quests, `IsInsideAreaSphere`, `GetSphereIdFromQuest`) | `spheres.rs`, `data.rs` | done | `quest_sign_sphere.g` of `main_world`; one manager per server (500 ms tick). Other `sphere_*` tables not loaded (unused by quests). |
| Imported quest sign spheres (`quest_sign_sphere_fill.json`) | `spheres.rs` (`load_sign_sphere_fill`), `mod.rs` (start), `app/Data/Worlds/*/quest_sign_sphere_fill.json`; converter `rust/tools/quest_sign_sphere_import/` | added | The 19 quests of `Docs/wiki/Quest-Audit.md` whose objective sphere has no 1.2 client sign sphere (1012, 1024, 1033, 1039, 1372, 1392, 1398, 1400, 1650, 1897, 5272, 5439, 5716, 5898, 5979, 6039, 6050, 6213, 6216). One offline converter reads the 10.x `world_server/quest_area_sphere.g` (`stype` = `spheres.id`) and the 10.x client `client/quest_sign_sphere.g` (`qtype` / `ctype`), both zone-local in the source, and writes **8 spheres in 7 quests** in world coordinates (`source_zone_origin * 1024 + local`, z as is; the loader uses fill rows as world and adds no origin): 1650 (main_world 142), 5716 (main_world 186), 5979 (instance_training_camp 198, two areas), 6039 / 6050 (instance_prologue 300), 6213 (main_world 144), 6216 (main_world 158). The other 12 quests are dropped with a reason: 1012 / 1024 / 1033 / 1039 because the 10.x `spheres` row was reused for another quest (`sphere_mapping_mismatch`; e.g. 10.x sphere 945 -> quest 5804 while 1.2 uses it for 1033), 1400 because its 10.x component id differs (7021/36416 vs the 1.2 7020), and 1372 / 1392 / 1398 / 1897 / 5272 / 5439 / 5898 because their sphere ids have no block in the 10.x files at all. 5716's second block is dropped because it lies in zone 159 while the quest is authored in zone 186; 6039 / 6050's second files are dropped because zone 348 does not exist in 1.2. Guards: the 10.x and 1.2 `spheres` / `sphere_quests` rows must name the same detail and quest, the 1.2 component must have a `QuestActObjSphere` / `QuestActCheckSphere` act for the same `sphere_id`, the zone must belong to the named 1.2 world and be open (`zones.closed = 'f'`), a 1.2 client sphere for the same (quest, component) is never overridden, and a 1.2 `mission_mission0.xml` `AreaSphere` with the same `value1` within 1.5 m must agree on the radius. The loader repeats the runtime guards, loads the file after the client spheres and rejects duplicates; the quest sphere triggers compare the sphere's `world_id` with the player's world (a main_world area no longer fires inside an instance). The 10.x client sign sphere file contributed no kept entry (its only matching old quests are in zones closed in 1.2), and the 10 contexts without components are untouched. Location note: the active manager/loader is `systems/quests/spheres.rs`; the world-side copy `systems/npcs/sphere_quest.rs` is not started (`start` is a no-op), so the fill is wired into the active path. |
| Imported area spheres (`SCAreaChatBubble` 0x153, `SCAreaTeamMessage` 0x155) | `spheres.rs` (`SphereArea`, `load_sphere_areas`, `tick_area_spheres`), `packets.rs` | added / INFERRED | Enter / leave of a `sphere_area_fill.json` area in the same 500 ms tick as the quest spheres (run first, the quest sphere code is unchanged): a `SphereBubble` row sends `SCAreaChatBubble(enter, player unit, sphere_detail_id)` and any row with a non-blank `team_msg` sends `SCAreaTeamMessage(enter, sphere id)` to that player; leave only when `enter_or_leave` is set. **INFERRED trigger and audience** (CAP-UI-7 / CAP-UI-6): the 1.2 client has the readers and handlers (`FUN_397b6330` / `FUN_391dc340`, `FUN_397b63b0` / `FUN_391dc460`) but no captured sender; `is_personal_msg` is true for all 67 shipped `team_msg` rows and the Rust sends to the player only (the non-personal audience is UNKNOWN). `SphereSkill` / `SphereSound` / `SphereDoodadInteract` / `SphereBuff` areas load but have no effect (a separate content decision). A missing JSON file is an empty table; ids are re-guarded against the loaded `spheres` table. |
| `QuestActObjZoneKill.OnZoneKill` level window | `systems/quests/acts.rs` (`level_in_range`) | deviation | **Deviation:** a level bound of 0 is "unset". The 1.2 data has `lv_min_npc` / `lv_max_npc` = 0 / 0 on 103 of the 110 NPC-kill acts (`lv_min` / `lv_max` are the questor's window and differ from the NPC bounds on the 7 acts that set both), and the C# rejected every real victim against the raw 0..0 range, so those quests could never complete. The player-kill window uses the same rule. |
| `QuestActObjZoneKill` team share | `systems/quests/acts.rs`, `events.rs` (`run_deferred`) | deviation | **Deviation:** only the original killer shares the kill (the `ObjTalk` / `ObjInteraction` guard). The C# re-delegated `OnZoneKill` from every receiver ("to avoid loops") and bounced between members holding the same quest without end. NPC deaths raise the event for each eligible party member, who all share it again, so in the C# a member counted the kill once directly and once per sharing member. **Deviation:** each member now counts a kill once per act. `Npc.DoDie` (`npcs/npc/death.rs`) notes the victim in the `direct_kill_credits` of everyone it hands a direct event (eligible players, the fallback killer, Tag Share contributors) before the first event is raised, and a team-shared `ObjZoneKill` ignores a victim in that list and counts a shared victim once per (quest, act) (`CharacterSkillState::claim_shared_kill_credit`, 30 s memory because object ids are reused). Team members out of range, who get no direct event, still receive the share (once). |
| `QuestActObjZoneKill` PvP kills | `systems/skills/combat.rs` (`raise_pvp_zone_kill`, from `character_death_tail`) | deviation | **Deviation:** the C# only raised `OnZoneKill` from `Npc.DoDie` (`DoOnMonsterHuntEvents`), so the 16 `count_player_kill > 0` acts (e.g. quests 5982-5991) never progressed. A player killed by another player's hand (hostile relation, the same branch that awards PvP honor) now raises `ZoneKill` (zone group of the victim, killer and victim as `KillUnit`s) for the killer only; `team_share` acts hand it to the killer's team through the usual `TeamEvent`. Friendly-fire kills and mate / slave kills raise nothing. |
| `QuestActObjAggro` acceptor | `systems/quests/acts.rs` | done | Mirrors the C#: the victim must be the quest's acceptor NPC. All 30 aggro quests are started by `NpcTemplate.EngageCombatGiveQuestId` (`AddQuestFromNpc`), which sets that acceptor; a quest added by another path (GM command) cannot count aggro. |
| `Transform.World` for quest positions | `systems/quests/events.rs` (`world_position`), `spheres.rs` | deviation | **Deviation:** quest environment / target / nearby-NPC / sphere-starter positions are world positions (composed through the parent chain). The raw local transform of a mounted rider or ship passenger is parent-relative and never entered a sphere. |
| `QuestActConAcceptItemGain` start | `systems/quests/events.rs` (`on_items_acquired`), `engine.rs` (`start_quests_from_item_gain`), `data.rs` (`item_gain_starters`) | deviation | **Deviation:** gaining an item starts every quest with a `ConAcceptItemGain` on its Start component (not active, not completed unless repeatable, count in the bag reached, unit requirements met) with the item as acceptor. The C# never did: `Inventory.cs` has the `OnItemGather` call commented out and `RunAct` only compared the acceptor, so the Blue Salt Brotherhood letters (4881-4888), 5450, 6160 and the level 1 "Today's To-do" quests (1955-1961, 2140-2146) could not be started. Inferred from the data (those items have no `item_accept_quests` row); the retail trigger is not verified. The 15 "starter item without `starter_quests` entry" simulator notes were all of this kind: the quests have no `item_accept_quests` binding at all, so there was no mapping to fix. |
| Sphere state across logout | `systems/quests/spheres.rs` (`forget_character`), `mod.rs` (`on_leave_world`) | deviation | **Deviation:** leaving the world drops the character's sphere triggers and its last starter-sphere position. The C# kept both, so a re-login inside a quest sphere compared against the position from the previous session and never raised the enter event (stuck sphere quests). The tick also prunes positions of characters that are no longer in the world. |
| `with_quests` panic safety | `systems/quests/events.rs` | deviation | **Deviation:** a panic in a quest operation (or a queued one) is caught around the locked section, the quest state is put back, the thread-local active entry removed and the panic resumed. Without it the character's later quest events on that thread were queued forever. |
| Quest packets (CS: StartQuestContext, CompleteQuestContext, DropQuestContext, QuestTalkMade, QuestStartWith, TryQuestCompleteAsLetItDone, RestartMainQuest, SaveTutorial, RequestPermissionToPlayCinemaForDirectingMode, StartedCinema, CompletedCinema; SC: QuestContextStarted/Updated/Completed/Reset, QuestContextFailed, QuestUnitReqFailed, QuestRewardedByMail, DoodadQuestAccept, DoodadCompleteQuest, TutorialSaved, AppellationGained) | `packets.rs` | done | Byte-exact tests. `SCQuestsPacket` now writes `Quest.Id` as `long`. `SCQuestContextFailedPacket` (0x146) is `u32 questId + u8 err`; the client reader `FUN_397b5c90` (class registered with opcode 0x146, vtable `399b3a5c`) reads the fields `type` and `err`, and `OnQuestContextFailed` (`FUN_391dbcd0`) raises `QUEST_ERROR_INFO`, which ends quest directing mode. `SCQuestRewardedByMailPacket` (0x14e) is `bool success + u32 questId`: the client reader `FUN_397b5ec0` (class registered with opcode 0x14e, vtable `399b3aa4`) reads `success` through the named bool reader (slot `0x78`) at `+0xc` and the quest id as a `u32` (slot `0x3c`) at `+0x10`; `OnQuestRewardMailSent` (`FUN_391dc150` -> `FUN_3937a520`) looks the quest up and raises `QUEST_REWARDED_BY_MAIL`, or quest error 0xb (`cant_supply_rewards`) when the flag is false. **Deviation:** the C# `SCQuestRewardedByMailPacket` wrote `u8 count + u32[]` (a list) and the port copied it; the 1.2 client's class holds a single quest, so one packet per quest is sent, with `success = true` (the packet is only sent after every reward mail was stored; a failed mail still sends `MAIL_UNKNOWN_FAILURE` and no packet, as before). |
| Unsent quest packet definition (`SCShowQuestArea` 0x1af) | `systems/quests/ui_packets.rs` | defined only | Byte-exact test from the 1.2 client reader `FUN_397b7940` (`u32 questType`, the `Area` block `FUN_3961e270` = two `i64` + one `f32`, then `f32 radius`). Nothing sends it (trigger UNKNOWN, CAP-QU-2). |
| `SCQuestAcceptConditional` 0x14a on `SphereAcceptQuest` sphere entry | `systems/quests/ui_packets.rs`, `spheres.rs` (`tick_area_spheres`), `data.rs` (`sphere_accept_quest_quests`, `accept_quest_quests`), `engine.rs` (`can_offer_quest`) | added (INFERRED trigger) | Field roles from the 1.2 client (grade A): reader `FUN_397b5d80` reads `u32 sphereId, u32 questContextId`; handler `FUN_391dbed0` -> `FUN_39376690` raises `START_QUEST_CONTEXT_SPHERE` with `(questType, sphereId)` and the client answers `CSStartQuestContext` 0xd5 with that sphere id. Sent to one player on entering a `SphereAcceptQuest` sphere whose area is in the shipped `sphere_area_fill.json` (1040, 1453 and 2236; the 1.2 `quest_sign_sphere.g` pack has no area for these quests, and 1520 has no `sphere_accept_quest_quests` row). Eligibility is the shared non-forced start refusal list (`start_refusal`: existing template, not active, not completed unless repeatable, game-schedule window, supply-item backpack rule, Start-component `unit_reqs`), the sphere's `can_trigger_sphere` and the Start component's acceptor check for that sphere (`can_offer_quest`). The offer repeats at most once per 30 s per character and sphere (INFERRED edge hysteresis) and also on an eligibility transition while inside (e.g. a level-up); the client's `CSStartQuestContext` must pass a server-side position check (inside the `sphere_area_fill.json` area or the `quest_sign_sphere.g` starter now, 5 m client-lag tolerance), so a spoofed sphere start from anywhere is refused and the offer does not make that easier. The quests stay out of the automatic `QuestActConAcceptSphere` starter path: `sphere_quests` has no row for them. **INFERRED (CAP-QU-2):** no capture shows the retail send; the data supports the trigger (3 link rows, `spheres 1040/1453/2236`, area rows) and the reply path already existed (`events::start_quest_context`). The 0x14a opcode constant is misnamed `sc::CSQuestAcceptConditionalPacket` in `opcodes.rs`; it is referenced as it is and `opcodes.rs` is never edited. Tests: `systems::quests::sphere_accept_tests` (flow through sphere 2236 + quest 5730, ineligible level / completed / active, far / tolerance / inside client starts, automatic sign-sphere starter, cooldown and level-up re-offer, real compact + area-fill data). |
| Doodad report hand-in (`SCDoodadCompleteQuestPacket` 0x14d) | `packets.rs`, `engine.rs` (`report_doodad_ready`), `events.rs` (`on_server_report`) | added | The C# had the opcode (`SCOffsets.cs` 0x14d) but never sent it; `DoodadFuncQuest.Use` reported a doodad server-side (`QuestManager.DoReportEvents` -> `QuestActConReportDoodad.OnReportDoodad`). The 1.2 client reader `FUN_397b5e50` takes the doodad object id as a 3-byte `bc` then the u32 quest id; `OnDoodadCompleteQuest` (`FUN_391dc050` -> `FUN_393773a0`, event 241 `COMPLETE_QUEST_CONTEXT_DOODAD`; `quest_context.alb` proto 19) makes the client answer with `CSCompleteQuestContext` naming that doodad. Rust deviates: a server-side report (`DoodadFuncQuest`) whose quest is ready at that doodad (`QuestActConReportDoodad` names the doodad template and the objectives meet the act threshold) now sends 0x14d and leaves the report to that client request instead of completing it server-side; every other server-side report still only logs refusals. All 15 live 1.2 `QuestActConReportDoodad` acts are on Ready components and their doodads carry `DoodadFuncQuest` (`real_compact_db_report_doodads_are_ready_doodad_func_quests`). |
| Refused quest reports | `packets.rs` (`quest_error`, `SCQuestContextFailedPacket`), `engine.rs` (`send_report_refused`, `finish_report`), `acts.rs`, `events.rs` | done | Every refused `CSCompleteQuestContext` report and `TryCompleteAsLetItDone` is answered once with 0x146, independent of the WARN repeat dedupe. A server-side report (`DoodadFuncQuest`, a doodad used on an owned quest: `events::on_server_report`) is only logged: the client did not enter quest directing mode, and a 0x146 there would show a quest error on every early click. The client error table `locale.questContext.errors` is built 1-based (`x2ui/baselib/locale_helper.alb`, `SETLIST` at instruction 654: `already_have`, `invalid_quest`, ...); the `QUEST_ERROR_INFO` handler `quest_context.alb` proto 22 indexes it directly with its first argument, and the client's local pre-checks use the same numbers (`FUN_3937a2d0`: 2 for an invalid quest, 0x14 after its can-interact check fails). Reason mapping (inferred from the client key names and those local codes, not captured from a 1.2 server): objectives not ready / wrong status -> 12 `invalid_quest_status`, wrong or missing NPC -> 7 `invalid_npc_or_quest`, wrong or missing doodad -> 19 `invalid_doodad`, quest not active -> 2 `invalid_quest`. |
| Refused quest starts (`SCQuestUnitReqFailedPacket` 0x148) | `packets.rs`, `engine.rs` (`add_quest`, `first_failed_component_req`), `unit_reqs.rs` (`first_failure`), `events.rs` (`start_quest_context`) | added | The C# only logged "unit requirements of the start component ... not met". The 1.2 client reader `FUN_397b5d00` reads `u32 quest + bc unitObjId + bag`; the bag (`FUN_397b1ef0`) is one flag byte and then only the set parts (0x01 `u8` kind, 0x02 `u16` message id, 0x04 `u32` value; zero parts are omitted, an all-zero bag is `00`). Handler `FUN_391dbdd0` -> `FUN_39376590` acts only when `unitObjId` is the local player, builds the "unit requirement failed" text (`FUN_39390900`: the system message id when set, otherwise text by kind and value -- kind 0x16 `NO_BUFF` prints the buff name of the value, 0x24 `EXCEPT_COMPLETE_QUEST_CONTEXT` the quest name) and raises `QUEST_QUICK_CLOSE_EVENT` (0xf4), which closes the quest directing dialog. Sent from `AddQuest` only for the four `CSStartQuestContext` contexts (NPC / doodad / sphere / object-free), never for automatic starters (level up, item gain, sphere entry, ...), mirroring the existing refusal log suppression; the `SCQuestContextFailedPacket` report answers are unchanged. `UnitReqsData::first_failure` keeps `check_all`'s AND / OR order (AND: the first failing row; OR: the first row when no alternative matches); `check_all` now delegates to it. **Inferred** (not captured, CAP-QU-1): `kind = unit_reqs.kind_id`, `message_id = 0` (`unit_reqs` has no message column) and `value = value1`; whether the original sent the packet for every start refusal is unverified. `CSQuestStartWithPacket`'s reader now matches `FUN_397c5450` / `FUN_397be540` (`u32 quest, u32 component, u32 act, u8 type` and a tail by type: 1 = six u32, 2 = `u8` level, 3 = two u32, 4 = u32) and stays log-only at `debug!`: type 2 is the level-up start the server already performs in `events::on_level_up`, so the packet is an acknowledgement; the meanings of types 1 / 3 / 4 are unknown and capture-pending (`QUEST-START-WITH-1`). |
| `QuestCommandUtil` / `QuestCmd` (GM) | `systems/services/commands/scripts/quest_cmd.rs` | done | `quests::events::send_quests` / `send_completed_quests` added. |

### systems::instances

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `IndunGameData` (actions, events, zones, rooms), `Models/Game/Indun/{IndunZone, IndunRoom}` | `systems/instances/data.rs` | done | Same six event queries / four action queries in C# order; `EnterCount` hack (1000 for group 49 / channel dungeons, else 3); localized names. Real DB: 104 actions, 70 events, 20 zones, 13 rooms. `indun_zones.pvp` / `has_graveyard` are now enforced (`systems/skills/{relation,resurrect}.rs`; see the `systems::skills` dungeon-flags bullet). |
| `BattlefieldGameData` + `Data/battlefields.json`, `TowerDefGameData`, `Models/Game/{InstantGame, TowerDefs}` data | `data.rs` | done | 7 battlefields (last `game_rule_sets` row wins), 24 tower defs / 89 valid progs. Tower defense skips orphan parents and orders waves by id. |
| `WorldManager.CreateWorldInstance` / `RemoveWorld` / `CreateStaticInstances` (AutoCreate) / `WorldIdManager`, `WorldInstance.CleanupInstance` / `ChannelId` / `HasCharacter` / `GetCharacterCount` / `ListPlayerNames` | `worlds.rs`, `mod.rs` (start), `world/mod.rs` (`has_world`, `remove_world`, `leave_other_instance`, `WorldInstance::transfer_out`), `npcs::spawn_manager::SpawnManagers::remove` | done | Non-instanceable templates reuse their instance; fixed ids <= 100; per-instance `SpawnManager` created with the world and loaded by the dungeon loader / a background task. The cross-instance region switch happens when the client sends `CSInstanceLoaded` (the character is re-filed in the arrival instance; the next move finds it filed), as the C# `AddVisibleObject` region change. |
| `Character.MainWorldPosition`, `Transform.CloneDetached` / `ApplyWorldSpawnPosition` / `MainWorldPosition.Clone()` | `worlds.rs`, `models/character.rs` (`main_world_position: Option<Transform>`), `db_character.rs` (save rule) | done | Saved inside an instance: the return point is written (C# `saveFromInstanceReturn`). |
| `IndunManager` (requests, requirements, entry history, system instances, deletion, leave, `DoIndunActions`, info tick) | `indun.rs` | done | |
| `Dungeon` + `DungeonLoaderTask` (queue, add / move, leave, team / disconnect handlers, `LeaveDungeonTick`, `AreaClearTick`, destroy) | `dungeon.rs` | done | `DestroyTeamDungeon` (never called in C#) not ported. |
| Party dungeon admission/reset and 1.2 mentor/mentee sealed chest quests | `indun.rs`, `dungeon.rs`, `mentoring.rs`, `scripts/dungeon.rs` | extended | Serialized run selection, pending-entry capacity, one charge per character/run, owner/leader-only empty resets, safe destruction; four paired daily quests use actual chest phases/seal items and existing inventory/quest rewards. See `Docs/wiki/Dungeons.md`. |
| `Models/Game/Indun/Events/*`, `Actions/*`, `WorldInstance.Events` (`OnUnitSpawn`, `OnDoodadSpawn`, `OnUnitKilled`, `OnUnitCombatStart/End`, `OnAreaClear`), `CharacterEvents` (`OnTeamJoin/Leave/Kick`, `OnDungeonLeave`, `OnDisconnect`), `UnitEvents.OnKill` | `events.rs` | done | Raised from `npcs` spawners, `skills::combat` (`PostUpdateCurrentHp` / `DoDie`), `social::team`, `world::session::on_disconnect`. |
| `InstantGameManager`, `InstantGame` (+ `InstantGameEvents`, team members / results) | `instant_game.rs` | done | Queue / matchmaking / invite / join / enter / start / kills / scores / end / leave; `Character.SetFaction` override (housing hook, guild kick, party kick); `ResetAllSkillCooldowns`. |
| Packets: `CSInstanceLoaded`, `CSUnknownInstance`, `CSSelectSysInstanceChannel` (0x13d), `CSApplyToInstantGame`, `CSCancelInstantGame`, `CSJoinInstantGame`, `CSEnteredInstantGameWorld`, `CSLeaveInstantGame`; `SCProcessingInstance`, `SCSystemDungeonState`/`SCSystemDungeonChannel` (0x206/0x207), `SCAppliedToInstantGame`, `SCInviteToInstantGame`, `SCInstantGameJoined/Start/End/AddPoint/Kill/Killstreak`, `SCInstantGameRemainTime` (`SCInstantGameUnkPacket` opcode), `SCTowerDefList/Start/End/WaveStart`, `SCSkillCooldownReset`, the unsent `SCRaceCond/SCRacing/SCRacingResult` (see the racing bullet below) | `packets.rs` | done | Byte-exact tests. `SCLoadInstancePacket` is the npcs portal one. See "System-dungeon channel select (WP-X1)". |
| Doodad funcs `EnterInstance`, `EnterSysInstance`, `ExitIndun`, `RemoveInstance`; skills hooks `leave_instance` (`ExitArchemall`) / `main_world_position` (`Return`); portal `UsePortal` court check | `npcs/doodad/funcs/world.rs`, `skills/ext.rs` (default bodies), `npcs/portal/mod.rs` | done | `DoodadFuncEnterSysInstance.selective` drives the channel list; see "System-dungeon channel select (WP-X1)". |
| Tower defense runtime, `TowerDefenseAttackerAiCharacter` | `tower_def.rs`, `config.rs`, `scripts/tower_def.rs`, `ai/framework.rs` | extended | Rust adds event start/end, first-wave delay, timed/kill AND/OR progression, NPC controller/summon cleanup preserving normal corpse timers, client lists, GM controls and game-time/war schedules that are derived from the 1.2 data by default (an explicit `Events` list replaces them, `Enabled: false` turns them off). The attacker's C# behavior graph already matches `AiKind::TowerDefenseAttacker`. Unsupported target kinds and absent spawn positions fail preflight; rewards/victory event chains are not added. |

Cross-subsystem: mates are despawned on dungeon entry and `PlayerCanEnter` checks the summoned vehicle (`systems::vehicles`); dungeon entry items use the economy's checked `EconomyBridge::bag_consume_item`; the permanent `Character.SetFaction` updates the owned houses (`SocialHooks::update_owned_housing_faction`, housing). Battlefield corps factions are temporary and skip every side effect (see the RPA-09 deviation below).

#### System-dungeon channel select (WP-X1)

`systems::instances` / `npcs::doodad::funcs::world` — extended. The C# `DoodadFuncEnterSysInstance`
always entered channel 0 and had no CS 0x13d handler, no 0x206 list and no 0x207 answer. The 1.2
client picks the channel with CS 0x13d `{u32 instId, bc doodadObjId}` (writer `FUN_397bca60`,
x2game.dll.c:1714704), after the server fills the dialog cache with SC 0x206 `{u32 zoneKey,
i32 count, count x {i32 index, u32 instId, i32 restrict, i32 current}}` (reader `FUN_397b8c30`,
x2game.dll.c:1711542; the client clamps the count to 0x20). The row `instId` is a world instance
id and `index` is 0-based (the client shows index+1). CS 0x13e is only the post-load notice of
the world instance the client is in (writer `FUN_397bcad0`; sender dll.c:379135 after the world
load when the zone group has `select_channel` and the current instance id is non-zero), not a
channel pick; it stays read-and-ignore. This corrects content.md WP-X1 section 1.4, as
`re-indun-interact.md` shows.

Rules (all **INFERRED** from the client decompile; no 1.2-server capture):
* A selective `DoodadFuncEnterSysInstance` (`doodad_func_enter_sys_instances.selective`, rows
  2-9 in the 1.2 data, now loaded) of a `select_channel` zone group sends SC 0x206 with the
  dungeon's live instances (world id order, enumerated 0-based `index`, `restrict =
  indun_zones.max_players` — 190 for the library, `current` = players inside) and does not move
  the player. Non-selective entrances and groups without `select_channel` enter channel 0 as
  before. With no live instance the first player creates channel 0 and enters it as before;
  what the original server listed for an empty channel is UNKNOWN (only live instances here).
* The 0x206 `zoneKey` is the entrance func's `zone_id` (`zones.id`, 209/210/211): the client's
  `GetSystemDungeonName` looks it up in a table keyed by `zones.id` and compares the record's
  `zone_key` with the current zone (`FUN_397f2140`, x2game.dll.c:1823669/1823723).
* CS 0x13d is validated like the client's `FUN_3930c420`: the doodad exists, its template's
  first `DoodadFuncEnterSysInstance` func (client `FUN_3983c230`) is selective, the sender is
  within the server's doodad interaction reach (`in_func_reach`: the func's skill range, else
  5 m, plus the 15 m doodad allowance; the client checks 2.1 m itself), its zone group has
  `select_channel`, and `instId` is a live instance of that zone group (any live instance, also
  one created after the list was sent). Unknown / stale ids are ignored. The player is queued
  into the instance through the normal `QueuePlayer` / `SCLoadInstance` flow, so a full instance
  answers the normal `INSTANCE_QUOTA` (471) error. Both entrance paths save the same
  `MainWorldPosition` (`ensure_main_world_position`: when it was never set or the player is still
  in the default instance); the pick saves it after every check, so a refused pick (bad doodad,
  out of reach, non-selective, stale id) leaves an existing return point untouched.
* SC 0x207 `{i32 zoneId, u32 instanceId, i32 instanceIndex}` (reader `FUN_397b03b0`) answers the
  pick with the zone the load sent (`SCLoadInstance.zoneId`, the dungeon zone key), the world
  instance id and the 0-based index. It is sent directly after `SCLoadInstance` when the channel
  was already loaded, otherwise after the client's `CSInstanceLoaded`; the client only stores
  the index when the instance and zone match its current ones. INFERRED: no capture shows the
  original server's reply to CS 0x13d beyond the load. A pending answer is dropped if a later
  load targets a different instance id.

### systems::vehicles

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `MateGameData`, `SlaveGameData` (templates, unit modifiers, initial items / buffs, passive buffs, doodad / healing point / slave bindings, drop doodads, mount skills, repairable slaves, `slave_equip_slots`, `item_slave_equipments`, `allow_to_equip_slaves` / `allow_to_equip_slots`, `model_bindings` (model -> declared attach points), `slave_attach_points*.json`), `TransferGameData` (templates, bindings, binding doodads, paths, client `transfer_path.xml` roads + `GetOwnerPaths`), `ShipyardManager.Load`, `item_shipyards`, `shipyard_rewards`, `TaxationsManager` table, `ModelManager` vehicle / ship models | `systems/vehicles/data.rs` | done | Gated test `real_compact_db_vehicle_tables` checks the row counts (15 shipyards, 13 `item_shipyards`, 9 `shipyard_rewards`); `real_transfer_follows_its_client_route` loads the roads from the game_pak. |
| Mount rider / operator skill lookup (`GetMountAttachedSkills` -> `mount_attached_skills`) | `systems/skills/handlers.rs`, `systems/skills/mount_authority.rs`, `systems/vehicles/{data,mod}.rs` | deviation | **Deviation:** the C# `MateGameData.GetMountAttachedSkills` scanned `mount_skills` for the *skill id*, so a skill with two rows always resolved to the first one in table (rowid) order. The 1.2 data has such duplicates: 15546 is both 226 '물 퍼올리기(견본)' (rowid 132) and 59 '물 퍼올리기' (rowid 316); the C# scan picked 226 -> rider 15548 (the display-only "견본" skill, no effects) instead of 59 -> 21505 (`GainLootPackItemEffect` 29504 -> loot pack 7732, 10 x item 15694 Water), so the Red Farm Hauler's Create Water produced no water (live owner run 2026-11-01 14:53:16/14:53:28: `SCSkillStarted`, `SCUnitPoints`, `SCSkillFired`, `SCSkillEnded` for the mount cast plus the instant 15548, no item). The Rust uses the `mount_skills` row the client names in `CSStartSkillPacket` (`SkillCasterMount.MountSkillTemplateId`; the real 1.2 client sends the row id: 59 for 15546, 16 for 10473, 201 for 20704, 369 for 27032 in the owner captures) and requires that row to be on the vehicle's bar (its own `slave_mount_skills` / mate `skills` entries or an active `buff_mount_skills` grant) and to cast the pressed skill. The rider is scheduled exactly as in the C#: `Character.UseSkill(rider)` right after the mount skill is accepted, so the mount cast and the rider's own cast (15546 and 21505 are both 5000 ms) run together and the item is granted once at the end of the rider cast. The same duplicate-skill-id rows (15450 -> 56 '물 뿌리기' vs 225, 16650 (4 rows), 17097, 17649, 20291, 20249, 11017, 15580) now resolve by the pressed row; the datas' 20 `mount_attached_skills` rows whose mount skill is on no slave/mate/buff bar are untouched (unreachable through the bar, not resolved by skill id either). Tests: `water_rider_follows_the_mount_skill_row_the_client_pressed`, `water_rider_needs_the_driver_attach_point`, `watering_rider_follows_the_mount_skill_row_and_consumes_ten_water`, and the gated `real_farm_hauler_driver_gets_ten_water_from_skill_15546`. |
| `MateManager` (per instance), `CharacterMates` (load / save / spawn / despawn), `Mate` (`AddExp`, `MateXpUpdateTask`, `RegenTick`, `DoFallDamage`, `OnZoneChange`, stats) | `systems/vehicles/mates.rs`, `stats.rs` | done | Mount / dismount (`SCUnitAttached` / `SCUnitDetached`, rider becomes a transform child), rename, target, user state, mate equipment packet (item move through `VehicleItemHooks`), kill exp, stun / unmount skills, death despawn. **Deviation (one summoned mate):** the 1.2 client has one player pet (`X2Mate:IsPlayerPetExists` / `DismissPet` operate on the one mate the client's `playerpet` resolves to, the pet frame and action bar are single, and `SCMateSpawnedPacket` carries no slot or kind), and the C# `CharacterMates.SpawnMount` despawned the active mate(s) and returned, i.e. using the summon item of the mate that is out dismisses it. The Rust keeps that toggle for the same summon item (same item id / `mates` row) and, unlike the C#, a different mate's item despawns the old mate and spawns the new one (the C# returned with nothing, so a different summon left the player with no pet). The despawn runs only after the item / template / dead-mate checks, so a refused summon keeps the current mate (the C# despawned it first). **Deviation (seats):** the passengers are the attach points the mate's model declares in `model_bindings`, the list the 1.2 client's seat test scans (`FUN_390c9f70`; the client mate object has five attach slots, `FUN_3933ac10`): horse 2, cow 1, elephant 5, battle pets none. The C# hard-coded driver + passenger0 for every mate. Tests: `second_use_of_the_same_summon_item_dismisses_the_mate`, `a_second_whistle_of_the_same_template_replaces_the_mate`, `a_second_mate_of_another_kind_replaces_the_first_and_spawns`, `a_refused_summon_keeps_the_current_mate`, `mate_passenger_seats_come_from_the_models_attach_points`. Kill exp: `reward_kill_exp` only calls `Mate.AddExp` (which sends the mate's `SCExpChanged`); the C# English "Pet gained N XP" system chat is not sent because the 1.2 client formats its own localized pet-exp line from that EXP_CHANGED event (`chat_msg_event.alb` -> `GetAddPetExpString`). Test: `kill_exp_death_despawn_and_force_dismount`. |
| `SlaveManager` (per instance), `SlaveSpawner`, `SummonSlave` details, `Slave` (`DoDie`, `DoHpChangeTrigger` / repair points, `RegenTick`, `Save`, `OnZoneChange`, stats) | `systems/vehicles/slaves.rs` | done | Create (saved slave / persistent doodads / saved child slaves from MySQL, position, boats on the water surface, doodad and slave bindings at the model attach points), bind / unbind (the seat check - a non-driver point must be declared by the slave's model in `model_bindings` - client reach and reservation run under the slave's one lock; a missing or taken seat is silent as in the C#, an already-seated character is refused with `slave_unbind_first` 0x66, a cannon refuses the passenger seat with `slave_cannot_bind` 0x60, and the owners-mark driver refusal keeps 0x61), delete with portal-time despawn, Rider's Escape, repair cooldown, `RepairSlaveEffect`, `SendMySlaveTask`, logout (+10 min) / disconnect despawn task, `slaves` table (`AfterCharacters`). `ApplySlaveBonuses` stores the flat `unit_modifiers.value`; all 452 Slave rows of the 1.2 data have `linear_level_bonus = 0`, so the unused C# `LinearLevelBonus` term cannot change a bonus (gated test `real_slave_unit_modifiers_have_no_linear_level_bonus` pins that data assumption). |
| `TransferManager` (per instance, 100 ms tick), `TransferSpawner`, `Transfer` route following (`GoToPath`, `MoveTo`, `NextPoint`) | `systems/vehicles/transfers.rs` | done | Carriage motor + boarding part + bench doodads, `SCOneUnitMovementPacket` (transfer move type) for both parts. Spawned from `SpawnManager.SpawnAll`. |
| `CSBoardingTransferPacket` 0x067 (board / unboard) | `systems/vehicles/{packets,transfers}.rs`, `models/transfer.rs`, `world/visibility.rs` | done / capture-pending server side | The 1.2 client sends `tl` = the transfer's `TlId` (u16, shared by the motor and its boarding part) and `ap`: the first free seat 2..7 of the targeted transfer (`FUN_393565c0` + `FUN_393546b0`) when boarding, or 0 from `UnBoardingTransfer` (`FUN_39440d30`). The server resolves the part whose model declares `ap` in `model_bindings` (motor 654 has only point 30; the carriage part 653 has 2/3; the floating ship 314 has 2..8), checks liveness, that the player is not on a slave/mate/another transfer or bonded to a doodad seat, a free seat and the client's 3 m range to the requested seat itself (`DAT_39ac79fc` = 9.0 is the squared range `FUN_39093860` compares with `SQRT(param)`; `FUN_3989bf90` measures to the target's bounds). The seat position is the bench doodad parented to the part, else the C# Passenger0/1 local offsets; an unresolved seat falls back to 3 m + the `ship_models` half extent, else 9 m. It then reserves the seat under the transfer's one lock. The attachment state is adopted only while the seat is still reserved after the broadcast; if it was released in between, a compensating `SCUnitDetached` is broadcast. The confirmation is `SCUnitAttachedPacket(child, ap, BOARD_TRANSFER 7, transfer)`; unboarding removes the seat, detaches the transform and broadcasts `SCUnitDetachedPacket`. A late viewer gets the seat from the transfer's visibility add. Cleanup: `force_dismount` (instance-change / GM `/move` / `/teleport`) plus `session::teleport_ended`, which every same-world `SCTeleportUnit` sender reaches (same-instance portals, the NPC teleporter, dominion / instant-game / move-all / nudge / sphere teleports), death, logout, disconnect and transfer despawn detach the passengers; a boarded passenger cannot mount a mate, bind a slave or take a doodad bench seat (refused with `slave_unbind_first` 102), and `detach_passenger` clears `attached_point` only when it still holds the removed seat and clears a sticky parent that still points at the transfer. **Deviation:** C# `Transfer.AttachedCharacters` was a `List<Character>` with no seat and the packet was log-only; the Rust keeps `(point, obj_id)` because the client needs the point (the C# never sent `SCUnitAttached` for a transfer). **Capture-pending (transfer-boarding-1):** the original 1.2 server's answers for a taken / undeclared seat and an out-of-range board are unknown; the Rust uses the client's clearing codes `slave_already_has_master` 97, `slave_cannot_bind` 96, `slave_unbind_first` 102 and `too_far_away` 154 (all in `FUN_393457e0`'s pending-bind clear set) and the client's `TRANSFER_BOARDING` / `TRANSFER_UNBOARDING` events confirm the attach / detach UI. Tests: `boarding_a_transfer_seats_the_player_on_the_boarding_part`, `boarding_a_transfer_refuses_an_undeclared_seat_and_an_out_of_range_request`, `unboarding_a_transfer_detaches_the_passenger_and_is_idempotent`, `a_late_viewer_receives_the_transfer_passenger_seat`, `a_server_teleport_frees_the_transfer_seat`, `logout_and_disconnect_free_the_transfer_seat`, `death_frees_the_transfer_seat`, `a_transfer_despawn_frees_the_passenger`, `a_boarded_passenger_cannot_mount_a_mate_or_bind_a_slave`, `a_bonded_bench_seat_refuses_transfer_boarding`, `unboarding_keeps_a_different_attachment_point`, `boarding_measures_to_the_seat_not_the_transfer_origin`, `a_same_world_teleport_ended_frees_the_seat`, `a_boarded_passenger_cannot_take_a_doodad_bench`, `unboarding_clears_a_stale_sticky_parent_without_detaching_another_parent`, `a_gm_move_frees_the_transfer_seat`. |
| `ShipyardManager` runtime, `Shipyard` build steps, `CraftEffect` shipyard branches, `ShipyardTickTask`, `ShipyardCompleteTask` | `systems/vehicles/shipyards.rs`, `effects.rs` | done | Requirements via the skills item hooks + money (`SCItemTaskSuccess` money form). **Design item:** `CSCreateShipyard.designItem` is the bag item object id the client placed (u64, a bag item id, not a template id); `create` validates ownership + `SlotType.Inventory` and maps its template through `item_shipyards` (fallback `origin_item_id` only when the shipyard has no row and it is nonzero), then consumes exactly that item under one bag lock (`ItemSkillHooks::consume_bag_item_by_id`) before reagents / money. A mismatch or a missing / foreign item is refused with `NotEnoughItem` and nothing is consumed; this is what makes shipyards 2-4 and 10 / 11 (`origin_item_id` 0; 16163 -> 2, 15687 -> 3, 15564 -> 4, 18124 -> 10, 18149 -> 11) buildable. Shipyard 14's design item (28013) maps to its duplicate 15, so 14 itself stays unbuildable (the 1.2 data has no row for it); shipyard 1 has neither. **Rewards:** `shipyard_rewards` (9 rows, all doodad 1330) spawns `count` debris doodads inside `radius` of the shipyard on `on_death` (`spawn_rewards`); the trigger is INFERRED from the schema (wreckage), not capture-proven, so normal completion spawns nothing until a capture shows otherwise. Debris is temporary (not persisted). Doodad 1330 keeps its authored phase chain (2146 Timer 1159: 300 s -> 2147 LootItem 740 -> 2147 Final 451: 120 s, which deletes it); the spawn only adds a delete task for a template with no Timer/Final phase func (`doodad_almighties.min_time`-`max_time` or a 600 s fallback, not the authored lifetime). `on_water` rows return the water surface when the computed ground is in water / below it, otherwise the collision / heightmap ground. `ShipyardData.Type` still carries `origin_item_id` (capture-pending: the C# value; the 1.2 client meaning is unproven). The AABB remains ignored (AABB use UNKNOWN) and is logged. `CSCreateShipyard.autoUseAAPoint` is parsed and ignored (AA points are not supported). Tests: `shipyards_10_and_11_build_from_their_item_and_consume_it_exactly_once`, `wrong_missing_or_foreign_design_item_is_refused_and_kept`, `origin_item_id_is_the_fallback_without_an_item_shipyards_row`, `real_use_skill_14879_requires_and_consumes_its_reagents`, `destroyed_shipyard_spawns_the_reward_doodads`, `on_water_reward_uses_the_water_surface_over_a_seabed`, `reward_points_are_inside_the_radius`, and the gated `real_data_reward_doodad_keeps_its_authored_phase_chain`. |
| `ShipHarpoonRopeController`, `CSUnhangPacket` | `systems/vehicles/harpoon.rs` | done | Tow forces are physics (`ShipHarpoonTowPhysics`): the per-tick entry points are exposed for the physics port. |
| `CSMoveUnitPacket` vehicle branches (ship request, vehicle, mate xp, riding, standing on objects) | `systems/vehicles/movement.rs`, `world/session.rs` | done | Ship-deck support accepts the actor standing flag independently of the movement header, preserving valid GC-relative positions and the current-parent sentinel during controller updates. This corrects the mirrored C# header-only attachment test; clearing both support flags still disembarks. A unit no longer hanging (`HangingFromObject` unset) loses its sticky parent unless it is boarded on a transfer. Mate-movement authority is enforced in `world::session::move_unit` (only the character itself or its owned mate may submit unit motion), before `on_mate_unit_move`. |
| `BaseUnit.GetDistanceTo` for shipyard targets (skill range check, `Skill.cs` `max_range`) | `systems/skills/geometry.rs` (`distance_to`), `systems/vehicles/shipyards.rs` (`shipyard_radius`) | deviation | **Deviation:** the C# measured skill range to the shipyard's center point (shipyard models are prefab models without an actor radius, so `ModelSize` was 0), which made the shipyard build skills (`min_range 0`, `max_range 4`) unusable anywhere but the exact center of a drydock ("Target is too far"). The Rust subtracts the shipyard template's `build_radius * Scale`, like the house `GardenRadius` rule. |
| Mate / slave / transfer / shipyard C2G and G2C packets | `systems/vehicles/packets.rs` | done | 18 C->S, 16 S->C (byte-exact tests). `CSChangeSlaveTargetPacket` has no 1.2 opcode (0xfff). |
| `CSSpawnSlavePacket` 0x02e | `systems/vehicles/{packets,slaves}.rs` | done / entry point UNKNOWN | The C# only logged the packet. The 1.2 client sends it from its placement-preview click handler (`FUN_393459b0` <- `FUN_394d31a0` <- `FUN_394d8c70` <- `FUN_39366480`, when the previewed kind `+0x48` is 8); which item/UI enters that mode is UNKNOWN (capture-pending). The Rust synthesizes the item-use cast the client sends for a summon item: `itemId` must be the sender's inventory `SummonSlave` item whose template's `slave_id` is the packet's `slaveId` (missing / foreign / wrong-template / zero items start no cast and only warn), and the cast is the item's `use_skill_id` with a `SummonPos` position target at the client position / `zRot` plus an internal `SkillObject::Unk5(hideSpawnEffect)` marker. It runs through `handlers::start_skill` -> `skill::use_skill`, so every item-cast gate applies (ownership, item-only skill, `item_use_zone_allowed` 440 / zone tag 58, actability, dead / stunned / swimming, cast time and interrupt, cooldown / GCD, reagents, item consumption). The `SpawnSlave` effect sees the marker and validates the placement with `validated_client_spawn` (the client yaw and the 1.2 10 m land rule); the spawn itself still runs `create_for_owner`, so the active-vehicle replacement, repair, item-update (once) and one-active checks are exactly the item path's. Placement refusals answer `SlaveSpawnErrorInvalidArea` 583; the 1.2 full placement check answers `SLAVE_SPAWN_SHIP_NEED_MORE_SPACE` 100. Tests: `csspawnslave_spawns_at_the_client_position_and_commits_once`, `csspawnslave_refuses_dead_stunned_and_cooldown_senders`, `csspawnslave_refuses_a_zone_banned_item`, `csspawnslave_refuses_a_far_high_or_wrong_surface_spot`, `csspawnslave_boat_on_water_spawns_on_the_surface`, `csspawnslave_refuses_a_hull_box_blocked_spot_with_100`, `csspawnslave_without_an_owned_matching_item_spawns_nothing`, `csspawnslave_keeps_the_item_path_one_active_vehicle_refusal`, `a_refused_land_summon_keeps_the_old_vehicle_and_binding`, `an_ownerless_spawn_at_a_cliff_skips_the_item_height_rule`, `a_client_placement_over_the_sea_floor_is_refused`. |
| `SpawnEffect` slave dir ids (`PosDirId` / `OriDirId`) | `systems/vehicles/effects.rs` | done / partial | Dir 1 = relative to the effect target (a position target falls back to the caster as the NPC branch does), dir 2 = relative to the caster; the position is `relative unit position + AddDistanceToFront(pos_distance)` along `relative unit yaw + pos_angle` (the C# slave branch moved along the caster's rotation; unlike the NPC branch's absolute `PosAngle`), and the orientation is the orientation-relative unit's yaw + `ori_angle`. Every 1.2 slave `spawn_effects` row uses pos dir 1/2 and ori dir 1/2/3; ori dir 3 and `mate_state_id` have no evidenced slave meaning (UNKNOWN), so they keep the caster-relative orientation / are ignored and are logged once. The C# `PosDistance = 2` default and the doubled roll / pitch are unchanged. Gated real-data test: `real_spawn_effect_slave_dirs_and_templates`. |
| `CSTurretStatePacket` 0x036 (turret / cannon aim) | `packets/c2g_misc.rs`, `systems/vehicles/slaves.rs` (`set_turret_state`), `world/unit_state.rs`, `systems/npcs/npc/packets.rs` | deviation / capture-pending | **Deviation:** the C# `CSTurretStatePacket.Read` only logged the three fields and `Unit.ModelPosture` always wrote pitch/yaw 0, so no client ever saw a crewed cannon turn. The Rust decodes `bc unitId, f32 pitch, f32 yaw` (1.2 client writer `397c7640`), accepts it only from a character attached to any seat of the slave named by `unitId`, stores it on that slave and relays the same values to the other players in range as `SCUnitModelPostureChanged` 0x103 posture type 8 (`u8 type, bool isLooted, f32 pitch, f32 yaw`; client reader `397bda20` -> `397b1df0` applies it through `390bdec0`). A viewer that arrives later gets the stored aim in the slave's `SCUnitState` posture block. The server clamps the model-frame radians to the slave's `vehicle_models` turret limits before storing / relaying (degrees -> radians, and a full-circle yaw wraps into +/-pi like the client's key aim); a non-numeric limit (the text `'f'` the real rows store in `turret_yaw_angle_max`) is no limit, and a reversed window is sorted instead of panicking; it ignores a slave whose model is not a `VehicleModel` (the client only sends turret aim for a `VehicleUnitModel`, `FUN_390b3750`). The 1.2 client itself sends `CSTurretState` at most once per 200 ms and only when the aim changes (`39356990`: `DAT_3a169940 + 200 <= now`), and the server additionally never relays an unchanged aim and delays a changed aim that arrives less than 150 ms after the last relayed one for that slave; the newest aim is always stored on the slave at once (a late viewer's `SCUnitState` reads it) and the slave tick relays a pending aim once the window ends, so a final update inside the window is not lost. The stored aim is not cleared when the gunner leaves the seat, so late viewers keep seeing the last posture (as the two-client live run showed). **Capture-pending (S12):** the 1.2 *server* side is not proven. Capture with two clients: A mans a ship cannon (GM `/slave spawn 22` spawns a hull with eight cannons; `/slave spawn 25` a ground cannon) and pans / fires while B records 0x103; confirm whether the original server relays the aim, the packet's unit id (cannon child vs hull vs gunner), whether the gunner also receives the echo, that the angles are model-local radians and whether the original server clamps them, and whether a passenger seat (tank gunner, point 2) is a valid aim path. Record the result here and in the S12 research. |
| `SpawnEffect` Mate branch | `systems/vehicles/effects.rs:44-49` | no reachable content | The `BaseUnitType::Mate` arm only breaks. Four `spawn_effects` rows reach it: 1986 and 1998 (NPC skill 23588, monster summon), 2014 (skill 24045 "walking pet summon", cow 9323, no item uses it) and 2245 (horse 3599, no skill uses it). None is player-reachable in the shipped data, so the arm stays empty (score 2; do not reopen). |
| Hooks / physics | `systems/vehicles/{hooks,ship_physics}.rs` | done | `VehicleItemHooks` (`EndCraft`, economy). Ship physics and water call `systems::ai::physics` directly (`AddShip` / `RemoveShip`, helm request, driver seat, `GetWaterSurface` / `IsWater`); `ShipPhysicsHooks` (harpoon lines / tension / lifetime, `Throttle` / `Steering` after a move) and the radar ship lists are installed here. The npcs / social hooks call the mate / slave / dismount functions directly. |
#### Summon replacement and retained cargo (2026-10-25)

Player summons use root-only owner lookup across worlds and serialize requests by
owner and persistent summon item. Session/departure generations and allocation
identity are checked again after loading. Saved hull, child and doodad rows load
before replacement; query failures, item-update failures, loaded-hull removal
refusal and invalid explicit placement stop without replacing the prior hull.
Invalid placement now refuses rather than falling back to another position.

The final synchronous publication boundary coordinates ordinary item operations,
direct despawn and current-session identity. Loaded cargo receipts are checked
against the exact live hull/rack allocation and unique matching System escrow
before metadata changes or deletion. This rejects a stale loaded row fetched
before its original pack was recovered. Different inactive saved hulls can still
restore legitimate cargo belonging to another player. The existing 600-second
offline retention and delayed portal rendering remain intentional.

The per-server item outer scope is thread-bound and reentrant; the inner economy
ops mutex remains nonreentrant. Callback-bearing paths acquire the outer scope
before existing auction/housing/spawn locks; callbacks retain their normal
placement outside ops/object guards. No synchronous guard spans an await.
Guardless quest item operations and direct writes bypassing coordinated APIs
remain explicit limits, rather than a claim of global lock freedom.

The read-only `/slave audit` command reports registered and retained allocations,
pending task leases, item identities and rack observations with bounded replies.
It explicitly returns `consistent:false` and `authority_consistent:false`: queue,
HP, world-removal and temporal phase/transform observations do not share a global
snapshot boundary. Normalized, filtered headless control captures supplement these
observations; they do not establish native UI behavior or original compressed wire
bytes. Deterministic flow tests cover the concurrency cases separately. The
combined source has passed independent reviews and all-target compilation;
all 57 Testclient library and 16 control/config tests pass at `96a67a5a7`.
These include literal native decoding, filtered compressed batches, real loopback
socket closure and timeout completion. Synchronous checks reject expired output,
command and global budgets before polling gameplay: an already-expired Tokio
timer alone can initially return Pending. Final combined Game tests, lane CI and
live acceptance remain pending.

### systems::services

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `Services/WebApi/**` (`WebApiService`, `WebApiServer`, `WebApiSession`, `RouteMapper`, `RouteDefinition`, `WebApiGet/PostAttribute`, `IController`, controllers, models) | `systems/services/webapi/` | done | axum (HTTP/1.1) listener on `WebApiNetwork` (`*` = any); every request goes through `on_received_request` (`OnReceivedRequest`): unanchored, case-insensitive regex routes on the raw URL, `MatchCollection` with .NET group order, 404 "Not found", 500 HTML page. Responses use the Newtonsoft defaults (`json::JValue`), requests the System.Text.Json defaults (`json::Stj`). Status, World, Expedition, Character (MySQL), Command, Mail, Auction controllers, plus the additive read-only tooling routes `GET /online` and `GET /dominions` (WP-SL7; see the deviation below). Ports of `RouteMapperTests` and `WebApiSessionTests` (+ controller tests). |
| `Services/DiscordBotService` | `systems/services/discord.rs` | done | twilight gateway + HTTP, unprivileged intents, "Playing with N other players" every 5 s, `/online`. Not started without `DiscordToken`. |
| `GameService`, `Program.UpTime`, `ManagerOrchestrator` | `systems/services/game_service.rs`, `main.rs` | done | Port of `GameServiceTests` and `ManagerOrchestratorTests`. |
| `TickManager` (+ `TickEventHandler`) | `systems/services/tick.rs` | done | Thread loop (20 ms), sync / async subscribers, busy skip. Port of `TickManagerTests`. |
| `TaskManager2`, `Models/Tasks/Task` | `systems/services/tasks.rs` | done | Delayed / repeating / cron tasks (NCrontab subset of `systems::npcs::schedule`), ids, cancel, `RemoveTasks`, `GetQueueCount`. Port of `TaskManagerTests`. |
| `AccountManager` remainder | `systems/services/accounts.rs`, `db.rs` | done | Since WP-B the per-account runtime state also holds the payment and premium point (`AccountManager::state`), loaded from `aaemu_game.accounts` under `lock_of` and refreshed at `Add` / `CSListCharacter`; the disconnect drops the point to `premium_configs.disconnect_point`. |
| `TimedRewardsManager`, `TimedRewardsTask` | `systems/services/timed_rewards.rs` | done | The tick reads the account payment / premium grade from `AccountManager` (WP-B) instead of the C# connection default; `max_labor` is `premium_benefits.max_labor`. |
| `TimedRewardsManager` Loyalty (EU 1.2 rule) | `systems/services/timed_rewards.rs` (`hourly_loyalty_update`, `tick_hourly_loyalty`), `systems/services/accounts.rs`, `db.rs`, `accounts.loyalty_hour_*` | deviation (owner rule, 2026-11-09) | The C# had one 5-minute loyalty tick whose amount came from `Loyalty.TickAmount` / `TickAmountPremium` (both 0 in the shipped config) and no play-time rule. Rust: every account is granted `Loyalty.DailyLogin` (5) once per UTC day, then `HourlyAmount` (1) for every full `HourlyMinutes` (60) of **in-world** play, at most `HourlyDailyMax` (3) per UTC day, counted per account across characters and relogs, persisted in `accounts.loyalty_hour_{day,seconds,taken,updated}` so a restart neither loses nor repeats play time. **No Patron / premium condition anywhere** (the client's premium text `ui_texts` 6242 is ignored; the 1.2 client DB has no `active_take` Loyalty row, so the owner's automatic rule is server-side). Each grant adds `accounts.loyalty`, pushes `SCBmPoint` (0x46) and the `msg_gain_bmmileage` system line (`ui_texts` 5450) to every online character of the account, and updates their `bm_point` (the stale-sibling fix, research round 6 §3.6). The old 5-minute loyalty tick is retired; `Loyalty.Tick*` keys are no longer read. The day boundary stays 00:00 UTC. Config: `Loyalty.HourlyAmount` / `HourlyMinutes` / `HourlyDailyMax` (`app/Configurations/CharacterSettings.json`); `0` disables the rule. Tests: `systems::services::timed_rewards::tests::hourly_loyalty_*` (rule, cap, relog carry-over with the sample-gap guard, UTC reset, disabled rule) and the gated `hourly_loyalty_tick_grants_without_premium_and_stops_at_the_cap`. |
| `TimedRewardsManager.DoDailyAccountLogin` (double grant) | `systems/services/timed_rewards.rs`, `systems/services/accounts.rs`, `db.rs` (`claim_daily_reward`), `accounts.last_daily_reward` | deviation (bug fix, 2026-11-09) | The C# (and the port) ran the daily credits / loyalty grant from the first login of a UTC day **and** from the midnight quest reset for every online account, so an account online across 00:00 UTC was paid at midnight and again at its next login (its `last_login` was still the previous day). Rust claims the reward atomically on `accounts.last_daily_reward = today` (one transaction with the credits / loyalty grant and the divine-clock reset), so the midnight reset pays the online account and the later login is refused for that day. Gated test: `daily_login_reward_is_granted_once_across_a_midnight` (two calls on one day → +5 once, one `SCBmPoint`). |
| Account payment / premium (`premium_grades`, `premium_benefits`, `premium_configs`, `pcbang_buffs`, `SCAccountInfo`, `SCUpdatePremiumPoint`) | `systems/services/premium.rs`, `systems/services/accounts.rs`, `packets/{c2g,proxy,g2c}.rs` | done | Rust only: the C# never loaded these tables and sent the permanent `AccountPayment` default (`SCUpdatePremiumPoint(1,1,1)` hard-coded at `FinishState(7)`). See the deviations below. |
| `CharacterManager` remainder (`GetEffectiveAccessLevel`, `PlayerRoll` + `CSRollDicePacket` / `SCDiceValuePacket`, `StartOnlineTracking` / `CharacterOnlineTrackingTask`, `AddOfflineCrimePoints`, `ApplyBeautySalon` + `CSEnter/ExitBeautySalonPacket`, `CSBeautyshopDataPacket`, `SCCharacterGenderAndModelModifiedPacket`) | `systems/services/{character,beauty_salon}.rs` | done | Beauty salon item work through `CHARACTER_ITEM_OPS` (economy). **Deviation (hair validation):** `ApplyBeautySalon` only accepts an `item_body_parts` row in the hair slot (`slot_type_id = 24`), not `npc_only`, whose `model_id` is the `characters` template model for the character's race/gender (`beautyshop_only` is allowed; a missing character model does not restrict). A refused request, including the previously silent missing-certificate path, sends `SCErrorMsg` `FAILED_TO_CHANGE_ITEM_LOOK` (the salon-specific id is UNKNOWN) and changes no appearance, item, certificate or money. The C# put any client-supplied template into the hair slot. **Deviation (`CharacterOnlineTrackingTask`):** the C# read and rewrote the account's divine clock per character (`GetDivineClock` + `UpdateDivineClock`, 2 round trips and a commit each, every 5 s). The Rust collects the per-account seconds during the pass and writes them after it with one atomic `UPDATE accounts SET divine_clock_time = LEAST(divine_clock_time + ?, 4294967295) WHERE account_id IN (...)` per distinct non-zero delta (500 ids per statement, `db::add_divine_clock_time`), so it can no longer overwrite the 0 that `timed_rewards::do_daily_account_login` stores in between. The column is `INT UNSIGNED`; the sum saturates at `u32::MAX` where the C# `uint` wrapped. |
| Character packets: `CSActiveWeaponChangedPacket`, `CSBroadcastVisualOptionPacket`, `CSChangeAppellationPacket` (`CharacterAppellations.Change`), `CSIdleStatusPacket`, `CSRefreshInCharacterListPacket`, `CSRequestUIDataPacket`, `CSSaveUIDataPacket`, `CSUpdateActionSlotPacket`, `CSRestrictCheckPacket`, `CSSetLpManageCharacterPacket` and the log-only `CSCharDetailPacket`, `CSCleanupLogicLinkPacket`, `CSConsoleCmdUsedPacket`, `CSCriminalLockedPacket`, `CSEditorGameModePacket`, `CSRankCharacterPacket`, `CSRequestCharBriefPacket`, `CSRequestSecondPasswordKeyTablesPacket`, `CSSetupSecondPassword`, `CSResturnAddrsPacket` (+ `SCActiveWeaponChangedPacket`, `SCUnitVisualOptionsPacket`, `SCAppellationChangedPacket`, `SCUnitIdleStatusPacket`, `SCRefreshInCharacterListPacket`, `SCResultRestrictCheckPacket`, `SCLpManagedPacket`) | `systems/services/character_packets.rs` | done | Byte-exact tests. `CSHangPacket` / `CSUnhangPacket` are `systems/vehicles/harpoon.rs` (sticky parents, rope break); `CSResurrectCharacterPacket` is `systems/skills/resurrect.rs`. The remaining log-only handlers are client-local / diagnostic and log at `debug!`: `CSConsoleCmdUsedPacket` (no server console), `CSCriminalLockedPacket` (no server state), `CSEditorGameModePacket` (client editor mode), `CSResturnAddrsPacket` (anti-cheat report) and `CSUnknownInstancePacket` (post-load notice; `systems/instances/packets.rs`). No 1.2 UI or server action for them was found; capture-pending (`MISC-CS-1`). |
| `CharacterManager` `bag_expands` / `expand_expert_limits`, `Inventory.ExpandSlot`, `CharacterActability.Regrade` / `ExpandExpert`, `CSExpandSlotsPacket`, `CSExpandExpertPacket`, `CSUpgrade/DowngradeExpertLimitPacket` (+ `SCInvenExpandedPacket`, `SCExpertLimitModifiedPacket`, `SCExpertExpandedPacket`) | `systems/services/expands.rs` | done | Item checks / consumption through `CHARACTER_ITEM_OPS` (economy). |
| `CommandManager`, `AccessLevelManager`, `Models/Game/ICommand`, `Utils/Scripts/**` | `systems/services/commands/` | done (infra) | Ports of `SubCommandBaseTests`, `NumericSubCommandParameterTests`, `AccessLevelManagerTests`, `DoodadChainSubCommandTests`. AStar sub commands need `ASTAR_HOOKS` (AI path finding). `Scripts/SubCommands/Doodads/DoodadSetAgeCommand` is ported (`commands/doodads.rs`, port of `DoodadSetAgeCommandTests` through a test root with the `DoodadCmd` aliases); the `DoodadCmd` root itself is the scripts phase. |
| `ExpressTextManager`, `CSExpressEmotionPacket`, `SCEmotionExpressedPacket` | `systems/services/express_text.rs` | done | Deviation: an emote whose sender object id is not the sender's own character is dropped (the C# broadcast the client-supplied id). |
| `LocalizationManager` | `systems/services/localization.rs` | done | 263 635 rows / 8 locales on the real DB. |
| `ModelManager`, `Models/Game/Models/*` | `systems/services/models.rs` | done | Counts checked against compact.sqlite3 (gated test). |
| `MusicManager`, `SongData`, music packets (`CSSaveUserMusicNotesPacket`, `CSSendUserMusicPacket`, `CSRequestMusicNotesPacket`, `CSEndMusicPacket`, `SCUserNoteLoadedPacket`) | `systems/services/music.rs` | done | `CreateSheetMusic` item work through `SheetMusicInventory` (economy; its `ItemSkillHooks::midi_cache` / `create_sheet_music` call this manager). Upload sanity is Rust-only (`music_note_limits` per Artistry grade, `Music.MaxSongChars`, owned source item; a UTF-16 size mismatch only logs a warning and the item is not required to be an instrument, the client sends the music paper; see the deviation below). |
| `RadarManager`, `TelescopeRegistrationEntry`, telescope / fish finder packets | `systems/services/radar.rs` | done | Registered from `BuffTemplate.Start` / `Dispel` (skills), unregistered on leave. Ship lists through `RadarShipHooks` (vehicles). The radar / telescope feature is ported, not missing (gap-audit 2026-11-02 research `stealth.md` §0.1). Open items, none of them a port gap: shipyards are not sent in `SCTelescopeUnits` because the 1.2 client has no shipyard list wire format (only transfers, fish schools and the slave/ship list), so the C# "Add Shipyards" TODO is closed; the GM range is `World.RadarGmAccessLevel` (default `100`, the previous hard-coded value) instead of a hard-coded access-level check (`radar.rs` `gm_range_comes_from_config`); `evade_telescope` (buffs 4887 / 4950, "free from the telescope's gaze") is not consumed, as in the C# (which unit carries the buff is UNKNOWN). |
| `Models/Tasks/ShutdownTask`, `SCNoticeMessagePacket` | `systems/services/shutdown.rs` | done | Count-down notices, final save, process exit; held in `server.systems.services.shutdown_task` (`SaveManager.ShutdownTask`). Buy-back wipe through `BUYBACK_WIPE` (economy). **Deviation:** the C# saved and exited without telling the clients (every client showed a disconnect error and quit). At T-0 the Rust task first broadcasts "The server is shutting down for maintenance.", refuses world entries (`GameServer::is_shutting_down`: `X2EnterWorld` / `CSSelectCharacter` / `CSSpawnCharacter` get `SCKicked(2)` + close), sends every connection `ConnCommand::ReturnToWorldList` (in-world: `LeaveWorldTask` to character select with no timer, i.e. save + `SCLeaveWorldGranted(1)` + `ChangeState(0)`; the client stays on its game connection and loads the lobby, so the world list request follows 2 s later: `GLPlayerReconnect` -> `LGPlayerReconnect` -> `SCReconnectAuth`, the `CSLeaveWorld(ServerSelect)` path, re-sent every 2 s to every connection still there, new ones included), waits up to 10 s for the clients to disconnect, kicks the rest with `SCKicked(2, "Server maintenance")`, then does the final save (3 tries), `shutdown::stop_server` (shared with the Ctrl+C path in `main.rs`) and the exit. A failed final save aborts the shutdown (as before: no exit) and reopens world entry; `/scripts shutdown cancel` is refused once T-0 passed. `/scripts forcedshutdown` still exits at once. |
| `SusManager` | `systems/services/sus.rs` | done | `audit_char_sus` log + speed analysis. All 12 C# `LogActivity` sites are wired: crime / trial / coffer through `social::crime::sus_log`, `CSDestroyItem` (negative count) through `SusManager::log_activity_player_deferred` (`porting-md#10`). The 5 s `FinalizeTransform` caller runs from the move handler (see deviations). |
| `GameData/Framework` (`GameDataManager`, `IGameDataLoader`, `GameDataAttribute`) | `data/mod.rs`, `systems::SystemsData::load` | done (by design) | The reflection-driven loader list is the explicit `GameData::load` + per-subsystem `Data::load`; `Load` / `PostLoad` order is the call order there. |
| `Models/Tasks/SaveTask/SaveTickStartTask` | `managers/save.rs` | done (by design) | The periodic save is a Tokio interval instead of a `TaskManager` task (not counted in `GetQueueCount`). |
| `Utils/{MathUtil, NumericExtensions, Converters/*, DB/HoldablesSchemaCheck}` | `utils/` | done | `Utils/DB/SQLite*` = `data::sqlite`, `Utils/IdManager` = `managers::id`. `SphereCommandUtil` / `QuestCommandUtil`: `services::commands::scripts::{sphere, quest_cmd}`. |

#### Character rename (`SCCharacterRenamed` 0x05a, WP-AC2)

Rust only: the 1.2 client has no packet or UI that starts a character rename (the only client
rename packet is `CSRenameExpedition`, guild / nation), so the trigger is the GM command
`/rename <old> <new>` (`commands/scripts/rename.rs`) through
`systems/services/character_rename.rs`. It works online and offline.

* **Wire format (EVIDENCED).** `SCCharacterRenamedPacket` is `{ u32 charId, string oldName,
  string newName }` (client serializer `FUN_397b7820`). The client handler `FUN_391df6e0`
  updates the unit-name cache (`FUN_393c8520`), the chat / friend caches (`FUN_393de080`) and
  the expedition / family member names (`FUN_392fb550`). It was defined but never sent.
* **Recipients (INFERRED).** Viewers of the character (its world instance's
  `players_around`), its online friends (owners that keep it in `friends`), its guild and
  family members, and the character itself, one packet per recipient. The C# never sent the
  packet, so there is no sender to copy; the set follows what the client handler updates.
* **Rules.** The name goes through `NameManager::validate_character_name_with_blocks` (regex,
  blocked texts, `allowed_name_chars` when loaded); the same name, a name held by another
  character and a deleted / pending-deletion row are refused. `NameManager::rename_character`
  swaps the id and name maps under one write lock (no reader sees neither or both) and refuses
  to clobber another character's name. The check, the `characters.name` UPDATE and the cache
  swap share `SaveManager::transaction_lock` with character create / the lobby edit, so a
  concurrent create of the new name has exactly one winner.
* **Persisted copies and their memory caches.** The **authoritative** `characters.name` and the
  copies the server reads back are updated in the same database transaction:
  `mails.sender_name` / `receiver_name`, `expedition_members.name`, `expeditions.owner_name`,
  `family_members.name`, `nations.owner_name`, `shipyards.owner_name` (the completion / decay
  path resolves the owner with `character_by_name`, so the live shipyard's `owner_name` and
  unit name are updated too), `auction_house.client_name` / `bidder_name` and
  `character_rank_scores.account` (the character name the client shows where the login account
  is unknown). The managers that loaded these rows and save from memory are updated after the
  commit while the save transaction lock is still held, so a later save cannot put the old
  names back: `MailManager`, `NationManager`, `AuctionManager`, `RankingManager`, the guild /
  family / shipyard caches, and the runtime-only `TrialManager` defendant name and
  `CrimeManager` offline-victim cache (neither is persisted). `housings` stores only the owner
  id and resolves the owner name from the name cache, so no house row changes. `friends` /
  `blocked` store ids only; `mates.name` and `slaves.name` are pet / vehicle names, not
  character names.
* **Deviation from the C#.** The C# had the packet definition and the lobby edit's name change
  but no world-rename path; the Rust adds the GM command, the broadcast and the copy updates.
  There is no 1.2 client trigger to capture, so the recipients stay INFERRED.

### systems::ai

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `NpcManager.LoadAiParams` (NLua), `Models/Game/AI/v2/Params/**`, `AiGameData` (commands, command sets, chat bubbles, events), `ModelManager` (actor models + game stances, ship models) | `systems/ai/{lua,params,data}.rs` | done | A Lua 5.4 subset evaluator with `System.Convert` semantics; all 2 489 `npc_ai_params` rows produce the same values as NLua (differential harness, gated test `real_compact_db_params_match_nlua`; rows 443 / 835 / 1975 fail to parse in both). |
| `NpcAi` framework, `AiUtils.GetAiByType`, all `AiCharacters/**`, every `Behavior` (incl. Archer / BigMonster / Flytrap / WildBoar / AlmightyAttack / Talk / FollowPath / Return / Dead / Despawning / Spawning / Hold / Idle / Roaming / Alert / RunCommandSet / DoNothing) | `systems/ai/{framework,behaviors}.rs` | done | State machine, GoTo overrides, transitions, check intervals, skill queue / selection, aggression / alert / help, return, follow path, greetings (`SCNpcChatMessage` + `SCPlaySound`). **Deviation (melee chase range):** the C# `PickSkillAndUseIt` returned `TooFarRange` for the melee base skill 2 beyond 4 m before `SetWeaponRange` ran, and `SetWeaponRange` read the holdable of the *target*; `MoveInRange` stops at `attack_start_range_scale * max_weapon_range`. After a ranged InCombat skill (range up to 20) went on cooldown the NPC kept that stale range and stood 20 m away until the player walked into melee reach, and with a scale of 1.7 the skill-2 stop distance (1.7 x 4 or 1.7 x 3 m) exceeded the 4 m guard. The Rust sets the weapon range before the guard, takes the weapon from the NPC itself (fist default 0 / 3 m kept), and caps the skill-2 `max_weapon_range` at `MELEE_ATTACK_RANGE / attack_start_range_scale` (`MELEE_ATTACK_RANGE` = 4 m, shared with the guard). Tests: `ai::behaviors::chase_tests::{fallback_to_melee_while_the_ranged_skill_cools_down_closes_in, melee_base_skill_chase_range_is_capped_by_the_attack_start_scale}`. **Deviation:** `UpdateAggroHelp` skips a linked NPC without an AI (one warning per NPC) and keeps helping the remaining NPCs; the C# `NullReferenceException` ended the whole loop. **Added (data-inferred, capture-pending AGGROLINK-1):** a member of an `aggro_links` group helps another member of the same group even when its template (`accept_aggro_link` / `aggression` / `aggro_link_special_rule_id`) would decline; the link path keeps the helper's `aggro_link_help_dist` and applies `aggro_link_sight_check` only when enabled, while non-members keep the template rules exactly (including the C# sight gate in the special-rule branch). AI 11 `siegeweapon_place` (61 placeholder / test / cutscene NPCs) and AI 12 `bomb` (NPC 376 and two flying-machine pilots) are no AI by design: the C# `AiUtils.GetAiByType` / `AiParams.CreateByType` return null and the 1.2 data has no `npc_ai_params` row for either type; `flow_tests::hooks_create_ai_by_file_type_and_detach_on_delete` pins it. |
| `AIManager` (100 ms tick, `AddAi`, region activity) | `systems/ai/manager.rs` | done | Per-AI mutex + queued calls; activity-based candidate selection and bounded `World.AiWorkerThreads` chunk batches (see deviations). |
| AI members of `Npc` / `Unit` (`MoveTowards`, `LookTowards`, `StopMovement`, `SetTarget`, `CurrentAggroTarget`, `CurrentGameStance`, `BaseMoveSpeed`, `OnDamageReceived`, `CheckIfEmptyAggroToReturn`, `ClearAllAggro`, `FindPath`), `NpcEvents` | `systems/ai/{npc,combat,skills,npc_events}.rs` | done | `SCOneUnitMovementPacket` movement, aggro tables through the skills API. |
| `AiPathsManager` (`Data/Path`), `AiPathHandler`, `Units/Route/Simulation` | `systems/ai/{path_handler,simulation}.rs` | done | Every `Data/Path/*.path` file loads (test). Routes are generated from the 10.x client, see the 2026-10-16 note. |
| `AiGeoDataManager`, CryEngine `.bai` readers (net / areas / verts missions), A* `PathNode`, `GetHeight`, `ReducePath` | `systems/ai/geodata/` | done | `GeoDataMode` only. Engine-faithful readers and a merged cross-sector graph (see "geodata deviations" below). 1.2 pak: 9 447 net (2 299 of them v54) + 9 447 verts (2 299 v1) + 3 265 areas files parse with 0 trailing bytes; 279 path areas files below v21 are rejected by version like the engine (they do not fit the v21 layout either), the 7 399 `hidemission` files are not read. The areas reader matched the C# reader on every dumped file (differential harness; path areas < v21 now excluded on purpose). Gated tests: `real_ai_*`, `real_nav_*` (`AAEMU_GAME_PAK`, `AAEMU_BAI_SAMPLES=D:\aa-re\zone\agent_geom\data`). |
| `Gimmick.MoveTowards`, flytrap gimmick | `systems/ai/gimmick.rs` | done | |
| Hooks for npcs / skills (create AI by `ai_file_id`, spawn / engage / death / delete / faction, NPC control effect, spyglass), AStar GM commands | `systems/ai/hooks.rs` | done | `AiHooks` in `server.systems.hooks.ai`; the npcs `NpcSkills` calls the AI parts of `NpcSkillHooks` directly (no decorator); `AiAStarHooks` installed into the services `ASTAR_HOOKS`. |
| `Physics/**`, `PhysicsManager` (ship hulls: Jitter2 world, `ShipController`, shore / ship–ship / doodad / cliff / static barrier contacts, buoyancy, whirlpool, storm lamps, harpoon tow, replication), `ShipSiegeAoEHit` geometry | `systems/ai/physics/` | done | The Jitter2 subset is bit-exact against Jitter2 2.8.8 for the integration (embedded reference test). Slaves call `add_ship` / `remove_ship` / `set_ship_request` / `set_ship_driver` / `ship_state` and may install `ShipPhysicsHooks` (harpoon rope state). |
| `WaterBodies`, `WaterBodyArea`, `Models/CryEngine/Objects` (`object.dat`) | `systems/ai/physics/{water,objects_file}.rs` | done | Water areas of all 1 294 client `object.dat` files equal the C# output bit for bit (differential harness, gated test `real_ai_object_dat_water_bodies`), except for one deviation: `WaterPointToWorld` always adds the cell offset. The C# took points outside `[-512, 2048]` for world coordinates, but every water point is cell-local, and 12 main_world volumes were stretched over dry land (drowning on land; gated test `real_water_no_phantom_water_on_land`). Gimmick `FloatToSurface` uses them. |
| `ShipTuningDebug`, `HarpoonMechanicsDebug` logging, `ShipHarpoonRopeController` | – | not ported / slaves | Debug code is hard-wired off in the C#; the rope controller belongs to the slaves subsystem. |
| (Rust only) the 10.x zone server's collision world: terrain heightfield, `brush.dat` / `vegetation.dat` statics from CGF `MeshPhysicsData`, doodad colliders, ocean / water-volume buoyancy areas, living-entity proxies of units, ray / sweep query service, `GetBlinkPosition`, ZWUnitFell, ZWReportMoveHack, ZWUnitCollision, CryPhysics hulls | `systems/ai/collision/`, `systems/ai/physics/cry.rs` | done | On the CryPhysics port (`aaemu-physics`), `Configurations/Physics.json`. See "systems::ai collision" below. |

#### systems::ai collision (the zone's physics, Rust only)

**Vehicle test fixes (2026-10-11):** the physics thread measures elapsed time after sleeping and
advances its previous tick to that instant. The former pre-sleep timestamp produced alternating
waited/back-to-back iterations and repeated elapsed time; at 25 TPS with 3 ms of work it stepped
about twice as often. Clock tests now verify 25 ticks integrate exactly 1 s, including fractional
milliseconds and late wake-ups. Empty physics instances skip simulation/environment queries
after removing their final CryPhysics hull; the removal test checks both cleanup and idle queries.
`CSDespawnSlavePacket` copies the instance id before calling vehicle cleanup so its temporary
player data lock is released. A live scroll dismissal previously deadlocked there; the item-skill
flow test now casts an owned item, binds the driver and dismisses through this packet with a
bounded completion check. Cars retain the 1.2 client's authoritative `VehicleMove` simulation;
these changes affect server ship timing and vehicle cleanup.
Ships now load the normal prefab reference from `ship_models` and prefer the authored `buoy*`
brushes, preserving the CGF physics geometry and local position/quaternion/scale. They replace
the fallback tube rather than adding a second displaced volume. The geometries share the
collision model cache and are loaded on summon, not each tick. The real 1.2 clipper prefab has
two such brushes; the rowboat has none and retains its explicit tube. A gated real-data test
checks both placements, settled draft and driven motion at two headings.
The server's ship transform now uses the same visual quaternion as its movement packet. This
fixes the mirrored C# yaw-in-pitch bug for attached passengers and parts without changing the
wire order; a placement test compares the transform and quantized packet quaternion.
Standing-on-object movement preserves the current parent for `GcId == 1`. Every packet with
actor flag 0x20 carries a primary position local to the supporting object and an auxiliary world
position; `gcFlags` only selects the id space of `GcId` (1 unit, 2 gimmick, 4 doodad — 10.x
`x2game-dev_dedicate` writer/readers, matching captured 1.2 ship and seat-doodad packets). A
packet whose supporting object is unknown or has disappeared uses the world backup for any
`gcFlags`; only the unit case did before, so standing on an unresolved doodad placed the
character at the world origin, under the sea (false `SCUnderWaterPacket` on dry land). A packet-flow test
checks a rotated ship, the current-parent sentinel, walking off with a stale header flag and
the missing-parent backup. This fixes false origin jumps when leaving a ship.
Seated slave passengers ignore late unit walking updates until the ordinary discard clears
their seat membership. A steering request that restores a missing parent converts the driver's
world transform to the ship's local frame. The live client exposed the old detach/re-attach
sequence placing the driver's world coordinates under a ship parent; the new flow test covers
boarding, a stale walking packet, recovery and discard. Missing-zone debug logs include the
object id, parent and both coordinate frames to diagnose future attachment problems.
During live-stack validation, the comment/trailing-comma JSON reader was found recursively
stripping the remainder after each commented comma in `Scripts/Commands/kits.json`. It now scans
only to the next token, retaining Newtonsoft-compatible comments and trailing commas without
exponential startup work. The shipped kit file and quoted/commented tokens have regression tests.

References: `rust/docs/physics/ZONE-PHYSICS-USAGE.md`, `D:\aa-re\zone\agent_geom\{sub_cgf,sub_x2game}.md`,
the x2game / cry3dengine decompiles (addresses below). Config `Physics` (`Configurations/Physics.json`):
`Enable` (true), `Geometry` (`Terrain` / `Brushes` / `Full`), `LoadRadiusCells` (1), `LoadOnQuery`
(true), `UnloadAfterSeconds` (600), `SyncPeriodMs` (1000), `DoodadColliders` (true),
`PreloadAllCells` (false), `MovementVerifyEnable` (false = `movement_verify_enable 0`),
`FallDamageZoneCap` (true), `CryPhysicsShips` (true). The service is off without client data
(`HeightMapsEnable`); every caller then keeps its previous (C# / heightmap) answer.

* **World per instance** (`collision::world::PhysWorld`): entity grid over the world (at most 1024
  cells per axis, z origin −256), terrain heightfield from the `heightmap.dat` samples (2 m step;
  fn-pointer callback slots, `collision::terrain`), the ocean as the global buoyancy area at
  `oceanLevel` (density / resistance 1000), `SetWaterMat` index 1 *(inferred)*, `gEnv+289` set (no
  DrawRayOnGrid limit).
* **Cells** load lazily (`collision::residency`): around players and ships every `SyncPeriodMs`, by
  queries that touch a cell (at most 16 cells per query); idle cells are unloaded. The zone loads
  its zone's cells at start (`SeamlessSystem::LoadZone` 0x365704D0).
* **Brushes** (`CBrush::Physicalize` 0x30111970): static entity at the brush matrix (scale =
  |column 0|), parts 0x1000 (0x18FFF, 0x10FFF with a no-collide geometry, `| 0xA` with render flag
  0x400000), 0x1001 (0x8000), 0x1002 (0x4000), entity flags `|= 0x8000`. The foreign-data class bits
  of render flags 0x40 / 0x80 / 0x2000 are not modelled. **Vegetation** (`CVegetation::Physicalize`
  0x30103DC0, matrix 0x30102840): static entity, density 800, flags 0x04018FFF, 0x1002 when present;
  groups whose model only has a no-collide geometry (most grass / foliage) get no entity, like the
  server (0x1001 needs the CVar ≥ 2). Not applied: the terrain alignment of `bAlignToTerrain`
  groups. The sector order of `brush.dat` / `vegetation.dat` is x-major (fixed from the data: 99.2 %
  of the trees sit on the terrain with it, 23 % with y-major).
* **Models** (`collision::{cgf,models}`): every `MeshPhysicsData` slot of a node's mesh through
  `LoadPhysGeometry` (blobs that share the render mesh get its positions / u16 indices / subset
  material ids); node transforms are not applied (`AddPhysGeom` 0x300400D0 does not). Material
  mappings / surface types are not loaded (hits report surface −1 / 0).
* **Doodads** (`collision::objects`): model class by prefix (`sub_39451CE0`: `cgf://`,
  `vegetation://`, `cga://`, `prefab://` = the Brush objects of the prefab library; else no geometry)
  from `doodad_func_groups.model` (read by `collision::data`; the C# discards the column), flags by
  `_UpdateCollisionParams` (`sub_39451190`: `collide_vehicle` 0x08, `collide_ship` 0x80,
  `no_collision` → 0x8000 only). Synced from the doodads of the loaded cells every residency tick
  (`sync_doodad` for immediate updates). Houses: the 1.2 DB has no house model paths (`prefab_models`
  has ids only), so there are no house colliders. Gimmicks: not built.
* **Units** (`collision::living`): inactive `CLivingEntity` proxies (P5) from
  `actor_models.physics_living_*` + radius / height, placed on every client move (players, mounts)
  and every residency tick (NPCs). The client stays authoritative; the zone's per-frame actor
  simulation is not run.
* **Queries** (`collision::{query,mod}`): `terrain_height` = `GetTerrainElevation` (`sub_300D4E00`:
  triangle split on `dx + dy = 1`, 0 when a corner is 0 or below the first sample), `ground_height` =
  first static / terrain hit (0x101, rays 0x8F) from z + 2 m down, `raycast` (any mask),
  `line_of_sight` (0x101), `sweep_capsule` (PWI), `water_level` / `is_water` (the C# water bodies),
  `surface_type` (0x11F). `displacement::blink_position` = `GetBlinkPosition` (`sub_3925E670` /
  `sub_3925D990`).

**Behaviour changes (with the service on):**

* Blink (`WZBlinkUnit` 0x39369B10, terrain only) ends where the unit's collider stops, 0.1 m before a
  contact, following 30° slopes; leap / dash end points and NPC knockback / impulse / explosion
  pushes stop at static / rigid geometry (0x107) and land on the ground; teleport-to-unit uses the
  ground at the end point; in-place resurrect snaps to the ground (`GetResurrectablePosition`
  `sub_3925D3B0`).
* Ground-targeted plot positions, doodad spawns / respawns, backpack drops, land slave summons and
  transfer parts use the physics ground (static + terrain) instead of the bilinear heightmap; ship
  summons read the sea floor with `GetTerrainElevation`.
* Walking NPCs (not flying / holding / idling) stand on the physics ground (`reference_height_in`);
  NPC spawner points snap to it within 1 m (`npc_spawn_on_terrain`). Real data: 1 487 of the 1 524
  NPC spawns of the 4 busiest main_world cells are within 1 m of the physics ground (1 407 of the
  terrain elevation alone; 178 stand on brushes).
* **Deviation:** `npc_spawn_on_terrain` (`systems/npcs/npc/spawner.rs`, `spawn_ground_z`) snapped a
  spawn to the ground only within 1 m. When the ground probe (2 m above the spawn, downwards) hits
  a floor (terrain, a building, a cave floor) the spawn now snaps onto it when it is less than
  `SPAWN_GROUND_SNAP` (5 m) above it (or sunk up to the probe's 2 m); spawns authored 1-5 m
  above the ground (about 5 % of main_world) no longer float. Larger gaps (structures, sea-surface
  swimmers, instance data) and probe misses (caves with no floor, no collision service) keep the
  C# 1 m rule, with three additions: a spawn 1 m or more above a floor under the water surface
  (a surface swimmer) stays; a probe miss under solid, non-hole terrain less than
  `SPAWN_SINK_SNAP` (6 m) deep is lifted onto the terrain (the client drew it buried), and
  anything under a hole stays; a floor at the heightmap ceiling (`max_height` = 4096 m, the u16 height storage
  saturates) is not trusted; a spawn 1 m or more above a floor
  and inside the bounding box of a loaded brush at least 6 m tall (a building / castle shell,
  `inside_brush_bounds`) keeps its Z: the Two Crowns throne hall (about 100 crowd NPCs at
  Z 242.8 around 13575-13636, 10427-10475) has no floor in our collision (the stairs brush's slab
  covers only a diagonal band, `castle_nuia_001` has no floor geometry), so the probe falls through
  to the terrain 4 m below and a snap would sink them under the hall floor. Flying and `underwater_creature`
  NPCs are not snapped.
* **Terrain holes:** the 1.2 `heightmap.dat` marks cave / basement entrances with surface id 31
  (`raw & 0x1F` of a node-v5 sample; `cry3dengine` `IsHole` 0x300CAB30: the containing sector's
  lower sample, a node with fewer than 33 samples is read at `floor(i * (size - 1) / 32)`, shared
  sector edges belong to the positive-side sector). `NodeCell.surface` keeps the ids (taken
  before `RescaleToInt` drops the fifth bit), `WorldHeightmap` a per-cell hole bitmap
  (`grid_is_hole`), and the physics heightfield's `getSurfType` returns the default `typemask`
  bit for a hole, so rays pass through (`TerrainSource::is_hole`, `terrain_is_hole`). The
  C# ignored the holes (and `GetTerrainElevation` still answers over a hole, as native does).
  Real data: cells 20,8 / 21,8 / 8,13 have 7 / 3 / 2 leaves with `hasHoles` and SID 31
  (`real_terrain_holes`). Unchanged and unfixed: the C# height decode `data >> 4` (12 bit) differs
  from the native `raw & 0xFFE0` (mean 0.05 m, max 0.25 m on the measured cells).
  **Visibility deviation (systems::npcs / world::unit_state, 2026-10-11):** NPC unit-state packets
  preserve the spawn/movement transform instead of the C# serializer's `GetReferenceHeight`
  mutation. That mutation could replace a collision floor with a coarse navmesh plane or reset
  an idling NPC to its original spawn Z after movement. In those 1 487 grounded spawns, 324
  navigation heights were over 1 m above ground (maximum upward difference 10.008 m), and 126
  were over 1 m below. The real collision test checks encoded positions, including static floors;
  byte-exact unit-state tests cover AI-less, walking, idle and flying NPCs and repeated visibility.
  Height queries now stay out of visibility serialization and its world/object locks. Flying and
  elevated authored spawns keep their existing positions; the 1 m spawn snap policy is unchanged.
* Fall damage (`collision::movement`, ZWUnitFell 0x3935C850): the zone's tracker (`sub_392D7600`:
  start z, max downward speed, landing on the physics ground) gates and caps the client's
  `fallVel`: no damage when `min(v, √(40·Δh)) < 20 m/s`, else the client value capped at that
  speed. The client value is converted with 32000 / 63.25 units per m/s *(inferred from the C#
  curve)*. Swimming / hanging / attached units keep the client value (the movement path
  `sub_39232570` forwards it). `FallDamageZoneCap = false` restores the C#, except for the
  safe-speed gate and the never-negative damage of `do_fall_damage` (see the `systems::skills
  (fall damage)` bullet below), which apply to every client value.
* Movement verification (off by default): reasons 0, 11, 12, 15–17 and 19 are logged (no consumer).
  Deck-relative movement is composed through the live parent before collision proxy, terrain,
  water and verification queries. A vanished supporting object uses the packet's auxiliary
  world position. Supported actors reset fall tracking; the reported client fall velocity is
  still forwarded, and ordinary on-foot fall caps are unchanged.
* Ships (`physics::cry`, `CryPhysicsShips`): hull entities per `ShipUnitModel::InitPhysicalEntity`
  (`sub_39286C50`) from `ship_models`; engine buoyancy (ocean + water-volume boxes) and collisions
  (terrain, brushes, vegetation, `collide_ship` doodads, other hulls) replace the Jitter2 step, the
  C# buoyancy force and the shore / ship–ship / doodad / barrier / steep-terrain depenetration.
  Hull damage comes from the ZWUnitCollision aggregation: closing speeds ≤ 2 m/s are ignored (the C#
  dealt 1 % on any overlap); the pair maximum per 500 ms (clamped to 10 m/s) gives the C#'s
  speed-scaled percentage (nose-cone rule kept). Deviation: each pair also has the C# 1.5 s damage
  cooldown (`HULL_COLLISION_DAMAGE_COOLDOWN_SEC`) and a terrain / static pair only damages on a
  new impact (no impact in the previous 500 ms window), so grinding or bouncing contacts no longer
  chip up to 20 % of max HP per second. The 1.2 data has neither the expression constants
  (12 / 13 / 21) nor the `physics_collision_*_damage_mul` columns. The keel width (0.2 m) and the
  default tube offset (half beam, the zone's `bbox.x / 2`) are inferred / taken literally. Prefab
  buoy brushes take precedence over that fallback, as in the engine.
* Water volumes (`collision::water_areas`): world slot 83 (2-D polygon areas) is not ported, so
  polygons are bounding boxes and rivers per-segment oriented boxes.

**Measured** (release, 1.2 pak, `tests/collision_measure.rs`): the 3×3 cells (9..11, 13..15): 7 849
statics, 675 models, 13 MB of blobs, 37 MB heap, 146 ms per cell, 3.5 µs per vertical ray. The whole
main_world (1 330 cell slots): 152 059 brush + 68 809 vegetation entities, 4 295 models, 90 MB of
blobs, 1.0 GB heap of which about 600 MB are the heightmap cells, 8 s; 1.0 µs per ray.

#### systems::ai geodata deviations (engine behaviour instead of the C#)

* **Combat chase (2026-10-13)**: routes are retained for stationary targets and regenerated
  after target movement exceeds the actor radius (minimum 0.5 m), correcting the reversed
  C# comparison. The cached destination is the victim's world position rather than its
  enclosing graph node. Route endpoints use actor positions, avoiding detours through the
  start/goal triangle centers; reached corners are consumed in the same tick with a 0.25 m
  tolerance, independently of weapon range. Grounded actors reach corners in XY and use the
  collision floor; flyers and underwater creatures retain 3D movement. An arrival keeps the
  travelled heading instead of evaluating `atan2(0, 0)`. References: 10.x
  `cryaisystem_dedicate` direct route `0x31062728`, `COPStick::RegeneratePath` `0x310F18E8`
  and target-movement regeneration in `COPStick::Execute` `0x310F24D8`. These are adaptations
  of the existing 1.2 chase and wire format, not a complete native goal-pipe implementation.
* **Combat chase follow-up (2026-10-16)**: (1) flyers (`ActorModel.MovementId == 2`) and
  underwater creatures never plan or follow a navmesh route: the graph is a walking surface,
  its ground-level, detouring waypoints dragged them off the straight 3D line to the victim
  (`behaviors::find_path` / `moves_freely`; any leftover `found_path` is dropped). (2) Replan
  hysteresis: a route is replanned when the victim has moved more than
  max(2 m, 20 % of the NPC-to-victim distance) from the route end and at least 500 ms have
  passed since the last plan, immediately for a new victim, and when a successfully planned
  route is exhausted while the victim is still out of range (same 500 ms limit). The 0.5 m
  model-size threshold replanned on position wobble and flipped the route around obstacles.
  (3) `found_path`, the cached end point and the plan bookkeeping (`NpcAi.chase_path`) are
  cleared on entering any combat behavior, `ReturnState` or `Dead` and by `SetTarget(null)`,
  so a re-aggro against a victim standing in the same spot plans from the NPC's new position.
  Each replan logs at `debug` with its reason (`new target` / `target moved` /
  `path exhausted`).
* **Walls on straight walks (2026-11-08, Rust only)**: the C# walked roaming, returning and
  path-following NPCs in a straight line and only fixed their Z, so NPCs crossed walls
  (testers' reports). With the collision service on, `collision::walk::wall_distance` casts one
  ray against static geometry (mask 0x001, no terrain) 0.5 m above the ground from the NPC to
  the target (ground Z interpolated between the ends); hits on floor-like surfaces
  (`|normal.z| > 0.7`: ramps, stair treads) are skipped, so slopes and stairs never count as
  walls. **Roaming** (`behaviors::choose_roaming_position`): up to 4 random targets are tried;
  if all are blocked the farthest walk is cut 0.6 m before its wall (when at least 1.5 m
  remain), else the roam cycle is skipped (3-6 s). **ReturnState** (`plan_return_route`): when
  the straight way to the idle position is blocked and `GeoDataMode` is on, the NPC walks a
  navmesh route (`geodata::plan_route`, the chase machinery; corners reached in XY within
  0.25 m); without a route it walks straight and the 20 s timeout teleport stays the backstop.
  The check runs once per chosen target / return, not per step. Flyers and swimmers
  (`moves_freely`) are not checked. With `Physics.Enable` off nothing changes. **FollowPath**
  is unchanged: of 1508 authored ground segments of `Data/Path` in main_world only 7 (0.5 %,
  5 routes, mostly flying / golem routes) cross a wall. Tested by `ai::behaviors::walk_tests`
  and (gated) `collision::real_data_tests::{real_roam_path_block_rates, real_walk_check_rates}`.

Reference: the 10.0.2.13 `cryaisystem_dedicate.dll` (image base 0x31000000), notes in
`D:\aa-re\zone\agent_geom\sub_navmesh.md` (§2 formats, §5 gaps) and
`rust/docs/physics/ZONE-PHYSICS-USAGE.md` §2.5. The C# (`AiGeoDataManager`, `*MissionReader`,
`PathNode`) is no longer mirrored on these points:

* **Areas version** (sector reader 0x31149764, whole-level reader 0x3107AB58): path sector
  files must be v21 (1.2) or v22 (10.x; same layout). The C# rejected `> 21`, so every 10.x
  areas file failed (no forbidden areas / boundaries / nav modifiers). Path files below v21 are
  rejected like the engine's sector reader (279 files of the 1.2 pak, v19 / v20; 272 of them
  do not parse with the v21 layout either); zone
  folder files stay accepted up to v22 (the whole-level reader still has the old-version
  branches).
* **Net / verts versions**: the 10.x engine wants net == 55 (`CGraph::ReadNodes`, 0x311030A8)
  and verts == 2 (0x311A2AC4); the C# accepted anything `<=`. Rust accepts net 55 and 54 and
  verts 2 and 1 and rejects every other version. **Deliberate deviation from the 10.x engine
  (1.2 data compatibility):** 2 299 path sectors of the 1.2 pak are net v54 + verts v1 (the same
  sectors), with the v55 node / link and v2 vertex layouts: all of them parse with 0 trailing
  bytes (gated test `real_nav_pak_12_old_sector_versions_parse`).
* **`hidemission*.bai` is not loaded** (no engine reader references it; stale v54 graphs). The
  C# read them into `HideMissionReaders` and never used them.
* **Nav-modifier header** (0x31149764): `u64 type; u64 waypointConnections; u8 altered` (17
  bytes), not `i32, i32, u8, f32, f32`; `altered` was always true (it read the low byte of the
  second u64 = 2). The `ver <= 16` (extra u32) / `ver >= 18` (light level byte) branches now
  use the file's version instead of the constant 21.
* **Links** (0x310FB7E8): byte +42 bit 0 is the engine link flag 0x800000
  (`LinkDescriptor.is_pure_triangular_link`); the C# tested `== 1`, false for every 10.x link
  (9 / 8 / 10 / 165). Byte +43 (`simple_passability_check` in the C#) is not read by the engine
  and is kept raw as `unused_43`; the floats at +28..+36 (C# length / maxWaterDepth /
  minWaterDepth) are not read by the engine and are kept as `unread_28`.
* **Merged cross-sector graph** (`geodata/graph.rs`): every folder that loads is merged into
  one `NavGraph` per world template like the engine's `CGraph`: vertices de-duplicated by XY
  (< 1e-4 m, 0x311A1D14), nodes by XY (< 1e-4 m) + equal navType (0x31101C68), links
  re-attached (0x310FB7E8; a repeated link overwrites the earlier one's attributes, radius
  stored as i16 centimetres like `SetRadius` 0x31083160, self links skipped, links to unknown
  ids dropped). Positions are compared in exact f64 world coordinates (the engine uses f32
  origin-relative ones). The engine streams a 3x3-sector window around one focus; Rust keeps
  its lazy per-cell loading (the 4x4 sectors of a cell on first use, never unloaded) and merges
  incrementally. A* runs on the graph, so routes cross sector and cell borders (a whole 1.2 cell
  merges 151 border nodes / 193 vertices); before expanding a node the cells its triangle
  touches (+0.5 m) are loaded so the neighbour cell's copy of a border triangle is merged.
* **Pass radius**: links with `maxPassRadius` below the agent radius, and blocked links
  (negative radius; -1 on ~17 % of the links), are not traversed. `PathNode::find_path` (all NPC
  callers) uses `graph::DEFAULT_AGENT_RADIUS` = 0.25 m, the X2 walkability probe radius
  (`ai_DefaultWalkability` 1, constants at 0x311FF070); `find_path_with_radius` takes the
  caller's.
* **Start / goal node and `GetHeight`** (`GetEnclosing`): the triangle node whose XY triangle
  (its 3 corners in the verts file) contains the point; the height is interpolated on the
  triangle's plane. The C# nearest-node behaviour is kept when no triangle encloses the point,
  and when the C# nearest node of the position's bai is not a triangle node: the zone instances
  navigate indoors on human waypoints (navType 4) under a few huge placeholder triangles (the
  engine picks the waypoint graph through the indoor nav modifiers, which are not modelled).
  With several enclosing triangles the one whose plane height is closest to the query z wins.
  Data caveat: the node positions of the real files lie inside their triangles but ~0.2 m above
  the plane (median; p90 ~1 m, up to ~20 m on large triangles over uneven ground, and the huge
  sparse coastal triangles can be off by 100+ m), so plane heights differ from the old
  nearest-node heights by that much.
* **Zone-geodata routes**: on the 1.2 instances only 5 of 16 synthetic test routes (dummy node 1
  at the origin to the middle node) succeed; the others' goals are not linked to the start or
  only through blocked links
  (raw-file BFS agrees); the old "routes" were presumably the C# position-based neighbour lookup jumping
  between unconnected nodes and the iteration cut-off.
* **Zone BAI selection (2026-11-05)**: `WorldTemplate.GetBaiByPos` returned the first loaded
  zone `.bai` (C# TODO as well), so every query in a zone-`.bai` world used zone 1's mesh. The
  1.2 client file manager finds zone files in 16 worlds (161 files under
  `game/worlds/<w>/zone/<key>/*.bai`); e.g. `instance_library` ships zones 296-298 and
  303-306 (86 files over 7 zones), while `main_world` has no zone `.bai` (its `zone/<key>/`
  folders hold only env / level data), so the path-block loading is unchanged. `AiGeoData` now
  keeps a copy of `WorldTemplate.ZoneKeyByRegions` and `bai_by_pos` selects the loader of the
  position's 64 m sector via `zone_key_of` (same `REGION_SIZE` lookup as
  `WorldTemplate::zone_id_at`). A position whose zone ships no files keeps the inherited
  first-zone fallback (debug log); out-of-world positions behave as before. Tests:
  `bai_by_pos_uses_the_position_zone` (fake `BaiFiles` provider with zones 100 / 200) and the
  gated `real_zone_bai_files_are_selected_by_position` (`AAEMU_GAME_PAK`).
* **A* cost** (`astar.rs`; replaces the C# quirk `sub_navmesh.md` §5.11): `G` is the accumulated
  2D route length (node position to the middle of the shared edge to the next node), `H` the 2D
  distance from the node to the goal node (admissible), the open set is a binary heap with
  re-opening on a cheaper `G`, and the iteration limit is `ceil(2D distance * 20) + 500`. The C#
  used `G` = distance from the link's source node to the goal, 3D, i.e. a greedy search on node
  heights. Node z values of huge navmesh triangles are unreliable (a centroid can sit 100 m
  below the ground its triangle covers), which sent chasing NPCs on ~100 m detours around a
  12 m gap (Solzreed Peninsula harpies: triangles 7349 and 7263 fanned around one apex, the
  3-hop corridor through two z~11 centroids lost against an eastern chain at the goal's z).
* **Smooth routes** (`funnel.rs`, `PathNode::find_smooth_path`, used by `npc_find_path`): a
  straight segment that only crosses linked, passable triangles (a triangle walk over the
  shared edges) and no forbidden area is the route without a search; otherwise the A* corridor
  is pulled tight with the funnel algorithm through the shared edges (corners moved
  `DEFAULT_AGENT_RADIUS` inward), rejected when a leg crosses a forbidden area. The way points
  are the NPC, the corners and the target (as the native direct path does, actor positions
  instead of graph centroids); corner heights come from the raw heightmap, else are
  interpolated, since the triangle plane and node z are unreliable on large triangles. When the
  navmesh cannot give such a route (waypoint nodes, unknown edges, iteration limit, forbidden
  area) `npc_find_path` keeps the C# pipeline: node positions, `ReducePath`. The `AStar`
  command and `Ai.FindPath` hook still use that pipeline (on the new search).
* Kept on purpose: the iteration limit (larger), the 2 m Douglas-Peucker of `FindPath`,
  `FindClosestToTheCurrent`, `ReducePath`, `CheckImpossibleWalk` are unchanged. Not done: link
  exposure costs, node flags, nav modifiers in the search, physics walkability
  (`CheckWalkability` 0x3100B214), the engine's 3x3 streaming window.

## Phase 3b wiring (cross-subsystem hooks)

Every subsystem installs its implementations of the other subsystems' hook traits from its
`System::start` (`install_hooks` / `bind`; tests call the same functions). Where both sides are
ported, the implementations call the owner directly (no wrapper chains); a hook slot only remains
where the owner is a later / optional subsystem (`server.systems.hooks.ai`, the services
statics) or the call crosses a lock boundary.

| Hook (slot) | Implemented by | Notes |
|---|---|---|
| `skills::ext::NpcSkillHooks` (`skills.ext.npcs`) | `npcs::bridges::NpcSkills` | NPC stats, `Npc.DoDie` (before / after, respawn time), doodad hits / spawns, gimmicks, portals, mates / slaves (direct `systems::vehicles` calls), the npc effects (`Interaction`, `Spawn`, spawner (de)spawn, `KillNpcWithoutCorpse`, `NpcControl` -> `systems::ai`, `SpawnGimmick`, `OpenPortal`, `ScopedF`, `MoveToRezPoint`), the AI parts of `OnDamageReceived` / `OnNoAggroTarget` / `CheckIfEmptyAggroToReturn` / move state. `MainWorldPosition` / `LeaveInstance` use the trait defaults (`systems::instances`). |
| `skills::ext::ItemSkillHooks` (`skills.ext.items`) | `economy::bridges::EconomyForSkills` | Gear stats / holdables / counts / consumes, `UpdateGearBonuses` buffs (equip effects, weapon wield, armor grade, item sets), the item `SpecialEffect`s through `economy::item_effects` (GainItem, emblem imprint, conversion, fishing loot, grade enchant, look convert, socketing, tempering), dyeing, skinize, sheet music (services `MusicManager`), the item effects (`GainLootPackItem`, `ImprintUcc`, `CleanupUcc`, `PutDownBackpack`, `RecoverExp`, `Conversion`, collect / craft `CraftEffect` -> `Craft.EndCraft` + `DoDoodadInteractionEvents`). |
| `skills::ext::QuestSkillHooks`, `SocialSkillHooks` | `quests::bridges::QuestsForSkills`, `social::bridges::SocialForSkills` | Item use / aggro / engage quests, `AcceptQuest` / `Cinema` effects; teams, crime evidence, `ReportCrime`, bot reports, dominion, appellations. |
| `systems::UnitHooks` (`hooks.unit`) | `skills::SkillUnitHooks` | Buff events, HP / combat side effects. |
| `npcs::hooks::{SkillsBridge, EconomyBridge, QuestsBridge}` | `skills::bridges::SkillsForNpcs`, `economy::bridges::EconomyForNpcs`, `quests::bridges::QuestsForNpcs` | NPC loot = `LootManager.GenerateLoot`; doodad func items, coffers, fish trophies, loot packs, money / labor; family / guild lookups (social / expeditions). |
| `quests::hooks::QuestHooks` (`quests.hooks`, `quests.economy_hooks`) | `quests::bridges::GameQuestHooks` -> `economy::bridges::EconomyForQuests` for the item / money / mail part | Locked hooks: the character lock is held; world work is deferred with `quests::events::after_unlock`. |
| `social::hooks::SocialHooks` | `social::bridges::GameSocialHooks` | Doodads, heights, evidence funcs, PvP enemy HP, judge, buffs, dismount (vehicles), housing faction (default = housing). |
| economy `QuestItemHooks`, `LootQuestHooks`, `CashShopQuestHooks` | `quests::bridges::QuestsForEconomy` | |
| economy `GearHooks`, `ProgressionHooks` | `skills::bridges::SkillsForEconomy` | |
| economy `NpcEconomyHooks`, `LootNpcHooks`, `TreasureMapSource` | `npcs::bridges::NpcsForEconomy` | |
| economy `LootTeamHooks` | `social::bridges::SocialForEconomy` | |
| economy `CraftHooks`, `ItemEffectHooks`, `MailHousingHooks` | `economy::bridges::EconomyComposite` | Skills cast / cancel, doodad func permission, housing zones / bonus / tax, quests `OnCraft`, public farm, house at location, backpack doodad spawn. |
| economy `AuctionMailHooks`, `CashShopMailHooks`, `StoreHooks`, `SummonItemHooks` | `economy::wiring`, `economy::mate_equipment` | Economy-internal. |
| housing `HousingEconomyHooks` | `economy::bridges::EconomyForHousing` | Items, money, coffers, housing mails, tax mail updates. |
| vehicles `VehicleItemHooks` | `economy::bridges::EconomyForVehicles` | `EndCraft`. |
| ai `ShipPhysicsHooks` (`hooks.ship_physics`), services `RadarShipHooks` | `vehicles::ship_physics::VehicleShipPhysics`, `vehicles::RadarShips` | Harpoon lines / tension / lifetime, physics throttle / steering; radar ship lists. |
| ai `AiHooks` (`hooks.ai`), services `ASTAR_HOOKS` | `ai::hooks::{AiSubsystemHooks, AiAStarHooks}` | |
| services `WebApiEconomyHooks`, `CHARACTER_ITEM_OPS`, `SheetMusicInventory`, `BUYBACK_WIPE` | `economy::bridges::{EconomyForServices, EconomySheetMusic}` | |

Integration tests: `systems/integration_tests.rs` (skill kill -> hunt quest + exp, doodad
interaction quest, force-attack crime evidence, party-tagged kill exp sharing, TagShare credit,
quest item hooks under the character lock, skills item hooks on the inventory, death exp loss).

Deviations of the wiring:

* Quest "locked hooks" run under the character lock: their world parts (level-up broadcast,
  item task packets, quest item events, quest reward mails) run once the lock is released
  (`after_unlock`), after the rest of the quest step; a failed reward mail `Send()` is reported
  then (`MailUnknownFailure`) instead of being counted by the quest step.
* The item operations of the quest hooks use an `ItemTxn` without the economy operation lock
  (taking it under a character lock could deadlock with an operation waiting for that
  character).
* `AcceptQuestEffect` approximates the C# `ImplId` lookup by the effect's quest id.
* Item procs are implemented by `systems::economy::procs` (set, item binding and holdable
  sources) and rolled from the damage pipeline; the C# never fired one (inverted cooldown,
  sources never registered at runtime, off-by-one roll). The capture-pending choices are listed
  under "Known deviations from the C#" (`systems::economy (item procs)`).
* `ItemSkillHooks::create_sheet_music` and `TimedRewardsManager.DoDailyAccountLogin` run their DB
  work as tasks (the C# blocks).
* AStar GM commands read the NPC's `PathNode` only when its AI is not in the middle of a step
  (`manager::post`); otherwise they see the defaults.
* The client time-of-day override in storms (`TimeOfDayObserver` + `StormShipLogic`) resolves
  the character through a weak reference held by its time observer.

C# bugs mirrored by the wiring: a fall death generates crime evidence against the victim
itself (the faller is its own "killer"). The killing blow's aggro / tagger used to be added to
the dead NPC after `DoDie` cleared them (`OnDamageReceived` runs after `ReduceCurrentHp`); that
is no longer mirrored (see "Known deviations", dead NPC returned and healed).

## Known deviations from the C#

* systems::social (blocked users): `CharacterBlocked` was stored and sent at login but never
  read, so a blocked player could still whisper, invite (party / raid / area, expedition,
  family), add as a friend, request a trade or a duel. Every one of those delivery paths now
  checks the recipient's list under the recipient's own lock
  (`systems::social::friend::is_blocked_by`) and drops the delivery when the sender is on it.
  Blocking / unblocking also resolves offline names through `NameManager` (the C# only accepted
  online names); a failed add / remove / block / unblock answers the client's own packet with
  the error id (551 unknown name, 352 self-block, 354 not blocked; 353 already blocked is
  INFERRED, see below), and the successful `SCAddBlockedUser` / `SCDeleteBlockedUser` replies
  carry the resolved id / name, so
  the login `SCBlockedUsers` snapshot and the reply describe the same `blocked` row. The
  sender-side reply is silent: the 1.2 client has no "target blocked you" error, and its
  whisper display (`chat/chat_msg_event.alb`, function 20) has no block-list check; the
  block / unblock UI only uses the existing 352 / 353 / 354 and 551 / 553 codes.
  **Capture-pending** (`BLOCK-LIST-1`): 1.2 retail's exact sender-side reply is unknown.
  Friend requests to an **offline** target are not blocked because an offline character's list
  is not loaded (`BLOCK-LIST-2`), and mail is delivered as before: no 1.2 client mail filter or
  error / system text for mail from a blocked player was found (`BLOCK-LIST-3`).

* systems::social (friend / block failure feedback, WP-SL2): the C# `CharacterFriends.AddFriend`
  / `RemoveFriend` and `CharacterBlocked.AddBlockedUser` / `RemoveBlockedUser` returned silently
  on every refusal (its own TODOs) and `AddFriend` accepted the owner itself. Every refusal now
  answers `SCAddFriend` / `SCDeleteFriend` / `SCAddBlockedUser` / `SCDeleteBlockedUser` with
  `success = false` and the client's error id: 551 unknown name (the client prints
  "$1 does not exist." from `FriendInfo.name`), 501 self-add, 502 already a friend (INFERRED:
  the client has no dedicated text), 556 not on the friend list, 352 self-block, 353 already
  blocked (INFERRED), 354 not blocked. Self-add is therefore a **deviation from the C#** (which
  accepted it); the client's own 501 text makes the refusal the better-supported behaviour.
  The already-a-friend and already-blocked ids have no client text and are INFERRED. A friend
  add whose target blocked the owner is answered with 551 as if the name were unknown (no
  block text exists in the client; capture-pending, `BLOCK-LIST-2`). There is
  no list-size cap: no client constant was found (UNKNOWN).

* systems::npcs (persistent doodads): `SpawnPersistentDoodads` skips the housing lookup for SLAVE-owned rows. In the C# every row with `houseId > 0` was resolved as a house, but for vehicle cargo racks `house_id` holds the hull id, so it logged "can't find it's owning house" (or, if a house with that id existed, parented the rack to it before the slave re-attach).

* systems::npcs (2026-11-05, WP-UI1 doodad phase messages): the C# had the
  `SCDoodadPhaseMsg` opcode (`SCOffsets.cs` 0x157) but no sender, and loaded neither
  `doodad_func_groups.phase_msg` nor `is_msg_to_zone`. The Rust loads both columns (NULL
  tolerant, `'t'`/`'f'` text like the other boolean columns) and sends
  `SCDoodadPhaseMsgPacket` from the `FuncGroupId` setter after the change committed and the
  doodad lock was released. The 1.2 client reader `FUN_397b6460` is `bc doodadObjId ; u32
  funcGroupId`; its handler `FUN_391dc720` -> `FUN_3902e0f0` looks the group up in the client's
  `doodad_func_groups`, needs the doodad locally unless `is_msg_to_zone` is true, and raises
  `DOODAD_PHASE_MSG` (0xfd) for a non-empty `phase_msg`. **Inferred (not captured):** the
  trigger is a real func-group change of a doodad already in the world — the spawn-time
  initial group (`DoodadManager.Create` / the saved-state restore applied before `Spawn`) sends
  nothing; normal groups go to the doodad's viewers (the same `GameObject.BroadcastPacket`
  recipients as `SCDoodadPhaseChanged`); `is_msg_to_zone` groups go to every character whose
  zone is the doodad's zone **key** (`WorldManager::broadcast_to_zone`, Rust only), not the
  zone group. Empty / whitespace-only messages (two `' '` rows in the shipped data) send
  nothing. The shipped data has 160 groups with a message, 29 of them zone-wide on 5 templates
  (7185 school-of-fish notices, 7261/7281/7282/7283 "lord's order quest activated").

* systems::npcs (2026-11-02, NPC groups): the C# `NpcGroupGameData` loaded `npc_groups` /
  `npc_group_members` but never used them: `NpcSpawnerNpc.SpawnNpcGroup` just called
  `SpawnNpc`, so the row's `MemberId` (an `npc_groups.id`) was created as an NPC template id.
  All 147 `NpcGroup` spawner rows resolve to a group; 134 of those group ids also exist as an
  unrelated NPC template id (so the wrong NPC spawned) and the other 13 spawned nothing. Rust
  deviates: a `"NpcGroup"` member spawns every `npc_group_members.npc_id` of `MemberId`'s group
  at the spawner position plus that member's `formation_offset_x/y/z`, each with the ordinary
  spawner link, population registration and respawn bookkeeping. The member -> spawner template
  index (`GetSpawnerIds`) is unchanged: it is keyed by `npc_spawner_npcs.member_id` only. The
  shipped main_world data places every group member NPC individually under its NPC id, and each
  of those ids has an ordinary (mostly AUTOCREATED) member row, so those placements keep
  selecting the ordinary single-NPC spawner and never the group template. A group template is
  selected by a placement only where the authored `UnitId` is the group id itself: groups 32,
  42, 44, 45, 79 and 80 in main_world (21 placements), which are exactly the rows that
  previously spawned the colliding NPC template (group 32 "Juggernaut slave overseer" was
  placed as NPC 32 "porter"). GM `/spawn` of an NPC template id that equals a group id
  resolves the group template the same way; event spawners and `SpawnEffect` build/select
  ordinary `"Npc"` templates and do not reach groups. The group batch is one authored
  encounter: it spawns whole even when fewer population slots remain (group 44: 9 members at
  `max_population` 1; group 80: 5 at 1), so it can exceed the spawner cap and the per-tick
  activation budget; ordinary `"Npc"` members keep the hard per-unit cap. This is a documented
  deviation, not fixed. The population target still counts individual NPCs, so a partially
  killed group may not schedule a per-member respawn while enough members live; it refills
  when the spawner empties. `get_npc_spawner_at` (`SpawnEffect`) now rewrites only an ordinary
  `"Npc"` member row; a summoned unit whose id equals a group id keeps the generated single-NPC
  fallback instead of replacing the shared group template's rows. Capture-pending:
  `formation_offset_*` are applied as authored
  world-axis deltas, not rotated by the spawner yaw (the 1.2 client `game_pak` only exposes
  runtime AI formation strings such as `FORMATION_POINT` / `FormationManager`, no placement
  rule). `aggro_rule_id` (group aggro), `is_leader` / `is_move_leader` (leader following),
  `formation_tension` and `enable_respawn` (true for 42 of the 141 groups used by spawners) are
  loaded but not applied.

* systems::ai (2026-11-02, aggro link groups; capture-pending AGGROLINK-1): the 1.2
  `aggro_links` (130 groups, Korean comments "they help each other") / `npc_aggro_links`
  (643 rows, 572 NPC templates, one duplicate row) are loaded by the 10.x zone server, but no
  consumer was found in its decompile, so the runtime rule is UNKNOWN. The Rust rule is
  data-inferred: `update_aggro_help` now also lets a member help another member of the **same**
  group when its own template rules would decline (`accept_aggro_link` false, `aggression` false
  with rule 0, an `aggro_link_special_rule_id` that does not match, or a rule-1..4 template with
  `aggro_link_sight_check` false). The template path itself is unchanged and mirrors the C#
  `UpdateAggroHelp` (`Behavior.cs`, `a2ab80abd^`): the special-rule branch requires
  `aggro_link_sight_check` to be enabled AND the abuser visible, so a non-member with the flag
  off never helps through rules 1..4. The link path keeps the helper's
  `aggro_link_help_dist`, applies the sight check only when `aggro_link_sight_check` is true,
  and still skips an NPC already in battle. Data rationale: 75 of the 572 linked templates
  never help under their own rules (20 accept false, 40 non-aggressive rule 0, 15 rule 1/2 with
  sight check off), 52 of the 126 groups contain such a member, and the shipped spawn data has
  115 main_world spawn pairs (24 distinct template pairs) where such a helper is within its own
  `aggro_link_help_dist` (e.g. group 87: NPC 1007 + 1008; group 66: 508 + 7624; group 70: 8219
  + 8220). Another 28 spawn pairs are in `instance_cuttingwind_deadmine` (15),
  `instance_library` (4) and `instance_sal_temple` (9); 35 distinct template pairs exist across
  all worlds. 74 of the 126 groups consist only of aggressive, rule-0, accepting members, which
  already help each other through the template rules; the link changes nothing there.
  Live verification drives one GM-spawned pair from group 70; the remaining AGGROLINK cases
  (whether a link may override distance/battle at all, and how it orders against rules 1..4)
  stay open until a retail 1.2 zone server can be recorded.

* systems::ai (2026-11-05, WP-5a FollowUnit / WP-5b `ai_command_sets.can_interact`): these are
  the first consumers of two C#-era data paths. Evidence: the 1.2 `ai_commands.cmd_id = 1` rows
  (sets 1 "test_npc", 20 "Q851 fled soldier", 33 "Q1307 bring the lumberjack", all with
  `param1 = 1`, `param2 = '0'`) and the `ai_command_sets.can_interact` column (302 'f', 23 't').
  The C# `RunCommandSetBehavior` only logged the FollowUnit command as not implemented, and no
  consumer of `CanInteract` exists in the C# or the 1.2 decompile, so both behaviours are
  **INFERRED** here:
  * FollowUnit follows `Ai.FollowUnitObj` when one is set, else the nearest player within 100 m
    (the search of `DoFollowNearestNpc`, `framework.rs`), and enters `FollowUnitBehavior`; with
    no target the command ends and the set continues. `param1 = 1` is ignored and logged at
    debug because its meaning is UNKNOWN. Sets 20 and 33 are unreachable through
    `quest_components.ai_command_set_id` (no row names them) and set 1 is a test set, so the
    shipped quest content does not use this path.
  * **Opt-in (`Interaction.CommandSetLock`, default `false`):** the 1.2 client only loads the
    column (`x2game12.c` `LoadAiCommandSetDescs`, no consumer found) and 302 of 325 sets are 'f', so
    the rule below is off unless the flag is set.
  * While an `ai_command_sets` row with `can_interact = 'f'` runs, the NPC's
    `Npc.interactable` flag is false: `SCNpcInteractionStatusChanged{npc, false}` goes to the
    players around it at set start, the visibility path repeats it to players that enter sight
    later, and `SCNpcInteractionEndedByZone{npc}` closes a window a player still has open on it
    (the server tracks that as `Unit.CurrentInteractionObject`, so this is not a tracking gap).
    `true` is sent when the set ends or a command of a different `can_interact = 't'` set
    starts. Leaving the command-set run (`RunCommandSet` / `FollowPath` / `FollowUnit`) for
    combat, talk or any other behavior abandons the set -- the mirrored state machine never
    returns to `RunCommandSet` -- so the lock is released there too instead of leaving the NPC
    non-interactable. Sets with 't' and NPCs whose commands name no set send nothing and keep
    the old behaviour. `CSStartInteraction` is refused silently for a non-interactable NPC: no error
    message is evidenced (UNKNOWN). Death and despawn also close an open window and release the
    lock (INFERRED from the packet name; the client handler `FUN_391d9000` only closes the
    window, it does not tell the server, so the tracked `CurrentInteractionObject` is cleared
    too). The despawn path is the `NpcManager.Delete` AI hook, which runs before the object
    leaves the world. The client's default interaction state on spawn is
    UNKNOWN (`SCNpcInteractionSkillList` writes `1`); this code assumes interactable unless the
    AI says otherwise. Capture-pending WP-5B-1: whether retail also sends these packets for
    command sets, and whether a non-interactable NPC answers with an error.

* systems::npcs (2026-11-05, NPC interaction reach): the 1.2 client measures the NPC interaction
  window against the target's shape, not before sending `CSStartInteraction` (decompile:
  `FUN_393bef10` opens and `FUN_3932a3b0` keeps the window, both compare with `sqrt(9.0)` =
  3.0 m through `FUN_39093860` / `FUN_39093800`; the send `FUN_393be030` has no position test).
  A forged request can therefore arrive from any distance, and the server now refuses one
  farther than 3.0 m plus `Interaction.NpcReachToleranceMeters` (default 2.0 m, **INFERRED**
  slack for movement / latency) with a `debug!` and no reply: no 1.2 error for this case is
  evidenced. `Interaction.NpcReachCheck` (default `true`) turns the gate off and restores the C#
  behaviour. The distance is `skills::geometry::distance_to` (both scaled actor radii subtracted,
  the same helper the skill range checks and the doodad func reach use). The C# had a
  `// TODO: Distance-check` and answered every target. Doodads, mates, slaves and stale ids are
  unchanged. The doodad-side `SCWorldInteractionCanceledPacket` 0x07f (`bc interactedUnitId,
  u32 interactedDoodadId`, reader `FUN_397c6540`) is defined with a byte test but never sent:
  its trigger is capture-pending (CAP-INT-1).

* systems::services (beauty salon): `ApplyBeautySalon` creates the requested hair item when
  the equipment hair slot is empty. The C# guard required an existing hair item, so a paid
  change could consume a Salon Certificate yet report hair 0. Same-hair requests still keep
  the item; receipts still report actual equipment. Hair-creation failure still logs and
  continues with the model update, receipt and certificate consumption, as in C#; failed
  certificate consumption still logs without undoing the already broadcast appearance.
  The requested template must be a hair body part of the character's own model
  (`data/items.rs` `body_part`, `characters` template model); an arbitrary template,
  an `npc_only` hair or another race/gender's hair is refused with `SCErrorMsg`
  `FAILED_TO_CHANGE_ITEM_LOOK` (generic; the salon-specific id is UNKNOWN) before any
  change or charge, and the missing-certificate path uses the same error instead of
  returning silently. The C# accepted any template.

* systems::services (music): 1.2 CS123 has independent `isTooltip` / `showBuffOnly`
  bool bytes (native constructor `397acd70`, serializer `397b1480`). The C# packed
  ushort switch ignored uncached playing `(false,false)` and `(true,true)`. Native
  `39082550` sends `(isTooltip,false)` on a cache miss and `(false,true)` for cached
  playing. Requests with `showBuffOnly=false` now load notes and echo `isTooltip`
  in SC1e0; `showBuffOnly=true` retains the buff-only ScoreMemorized (6010) path,
  including its lack of a song-existence check. `(true,true)` is representable but
  is not emitted by the observed native cache flow; it follows the same buff-only
  branch. Missing-song empty replies and existing read/ownership behavior remain.
  Request u64 / lookup u32 / response 32-bit widths are unchanged: native SC1e0
  sign-extends its signed32 note ID at `391e1691` before callback `391e169b`.
  Arbitrary high request bits still alias the low32 song key; IDs >= 2^31 can
  differ between zero-extended sheet IDs and sign-extended native cache keys.
  No evidence establishes a safe ID-width or ownership-policy change here.

* systems::services (music uploads, WP-M1, 2026-11-06; note limits 2026-11-07): the C# stored any
  `CSSaveUserMusicNotes` body. The Rust refuses (warning, nothing queued, and
  `SCErrorMsgPacket(UserNoteCannotSave)`) when the song is longer than the player's
  `music_note_limits` row, when it exceeds the outer `Music.MaxSongChars` sanity bound (default
  3000, above the data's 1000, so it never binds by default), or when `item_id` is not an item
  owned by the player. A `song_size` that differs from the UTF-16 length is only logged (the
  client writer's size unit is not evidenced). The item is NOT required to be an instrument: the
  client looks up a bag item by its 64-bit id (`FUN_39079640` / `FUN_390799E0` -> `FUN_3961e9a0`,
  id at the packet's second field), which is the music paper the following item cast (skill
  22215) consumes; `instrument_sounds` is loaded by the client for its own UI (`dll.c:1559064`).
  The limit is the 1.2 client's own: it loads `music_note_limits` (`dll.c:1207337`),
  `X2UserMusic.GetCompositionLimitInfos` returns `{grade, compositionLimit}` per row
  (`FUN_39079ea0`), the composition UI matches the player's `X2Ability.GetMyActabilityInfo(33)`
  grade (actability 33 `예술`, Lua `user_music.alb`), and `SaveNotes`/`PreSaveNotes` allow a song
  only when `XlStringLen(song) <= limit` (`FUN_39079640` / `FUN_390799e0` call `FUN_39082130`).
  Its save error is `0x2aa user_note_cannot_save`, which is only shown when the note conversion
  fails (`FUN_39428370(0x2aa)`); the UI also hard-caps the score input at 1000 characters
  (`SetMaxTextLength(scoreEdit, 1000)`). A grade above the table's last step uses the highest
  authored limit (1000), never a lower one. The note grammar is still not parsed: no server-side
  format is evidenced, so a grammar failure remains a client-side refusal (0x2aa).

* systems::skills (music): the C# (and this port) send `SCSkillStarted` only for casts with a
  cast time. The play (22213), pause (22214) and close-the-score (22217) skills are instant
  (`casting_time` 0, `effect_delay` 200 ms), so the 1.2 client never received the own
  `SCSkillStarted` it reacts to with CS121 (MIDI) / CS124 (End): the play effect then
  broadcast an empty MIDI cache. A validated player cast of a skill whose catalog join
  `skill_effects -> effects(SpecialEffect) -> special_effects` contains type 93 (`PlayUserMusic`)
  or 94 (`PauseUserMusic`) now sends `SCSkillStarted` (same timeline id as the fire, cast times
  0, no extra data) directly before its `SCSkillFired` (`skill::schedule_effects`,
  `SkillRunState::music_started_pending`). No id list, no `application_method` filter, no change
  for other instant skills, NPC casters, rejected casts (they keep the failure `SCSkillStarted`)
  or the item-caster refusal of 22213 (sheet 28051 has no `use_skill_id`). Not a retail capture:
  derived from the client contract. The authored 200 ms `effect_delay` is unchanged, so the
  client's CS121 / CS122 must reach the server within `200 ms - one-way latency` of the fire
  (compose relies on the same window for CS122); the live lane measured the CS121 arriving
  ~0 ms after the Started on loopback. CS124 (`CSEndMusicPacket`) matches the C#: it is a no-op
  while the 30 s ScoreMemorized (6010) buff is active and otherwise removes the tag-1155 play
  buffs (6176 / 6177); pause (dispel tag 1202) keeps 6010, close (dispel tag 1155) removes it.

* systems::skills / systems::npcs (`Interaction` special effect 10): the former C# class was a
  TODO stub. The 1.2 `special_effects` rows put a `WorldInteractionType` in `value1` (row 11030:
  19 `Use`, 36: 10 `DigTerrain`, 215: 29 `SummonBeanstalk`) and an optional doodad id in
  `value2`. The effect now runs the same authored interaction classes as `InteractionEffect`
  through `npcs::bridges::interaction_effect`; values without a 1.2 class (10, 14, 29) log
  "unknown world interaction" exactly like the template path.
* systems::skills (`GetSiegeTicket` special effect 64): row 5583 (values 0) is the only row,
  on skill 16975. It grants one `defense_ticket_id` from the `siege_zones` row of the caster's
  zone group (zone id -> `zones.group_id`). Holding that zone's `offense_ticket_id` sends
  `SIEGE_ONLY_DEFENSE_OR_OFFENSE_TICKET_AT_ONCE` (463) and grants nothing; a caster zone
  without a siege row logs and grants nothing.
* systems::housing / systems::social (`StartDominionNonPvpDuration` special effect 110): row
  14465 carries `value1` 120 (minutes) on skill 23992 "Dominion peace". The start and the
  authored duration are persisted on `dominions` (`non_pvp_start_at`, `non_pvp_duration_min`;
  update `2026-10-20_aaemu_game_dominion_non_pvp.sql`) and sent in `SCDominionData`. The
  handler requires the live owned territory at the caster and its current expedition leader;
  a second cast while the window is active sends `ALREADY_IN_NON_PVP_DURATION` (759). The
  skill's 4-hour reuse cooldown and five reagent crystals remain the skill system's job.
  A successful start also sends `SCDominionNonPvPStarted` (0x024; the opcode table still names
  the constant `CDominionStartUnkPacket`; native `OnDominionNonPvPStarted`, Deserialize
  `0x397B7BB0`: zone group, DateTime start, u16 minutes). `SCDominionData`'s `nonPvPDuration`
  is **seconds**, not the authored minutes: the native record serializer
  (`0x3983A470`, ~1835164-1835174) stores `start + duration` with no scale factor (its writer
  sends `end - start`), so the snapshot sends `duration_min * 60` as a u16 and drops the
  snapshot when that does not fit; the 0x024 packet keeps the minutes. The dominion tick clears
  every window whose `start + duration_min*60` has passed and sends `SCDominionNonPvPStopped`
  (0x025; opcode table `SCDominionEndUnkPacket`; Deserialize `0x397B7C00`: zone group) once.
  The client derives the same end from the start packet, but keeps its cached end until the
  stop packet and its peace-skill precheck (`FUN_398391c0` -> error 0x2f7) refuses while it is
  set, so natural expiry is the only 0x025 edge; early termination by a siege declaration is
  UNKNOWN and is not invented. Peace windows use the real wall clock in both the cast and
  `runtime::tick_once`'s expiry: the calendar's test clock is restricted to the calendar
  phases. A forced release that clears the owner and a fresh claim (`release_expedition`,
  `declare_unowned_with`) clear the persisted window, because the released record is dropped
  on the client (0x01e, no 0x025) and a later owner must not inherit it. All four dominion
  notifications below go to every online player: the client stores one cached record per zone,
  fed by the server-wide `SCDominionData`, and none of the handlers filters by owner or zone
  (the client only refreshes the territory window when the player is in that zone,
  `FUN_392f4d30`).
* systems::housing / systems::social (dominion notifications 0x01e / 0x023): a forced release
  that clears the owner (`DominionManager::release_expedition` -> `apply_released`, the pirate
  last-member dismissal path) sends `SCDominionDeleted` (0x01e; Deserialize `0x397B79F0`: zone
  group) after the phase snapshot, so the client drops its cached record; a release that only
  cancels a siege keeps the owner and sends nothing. A successful property-tax collection
  (`/collectdominion <zoneGroupId>` -> `tax::collect_locked`, the only existing settlement
  path) sends `SCDominionTaxBalanced` (0x023; Deserialize `0x397B7B60`: zone group, peace-fund
  money, peace-fund AA points) with the current peace-fund values and no accrual (the AA field is
  written from the treasury column, which stays 0 because AA points are not supported). Peace
  Fund rates, sources and the historical automatic payout remain UNKNOWN (see
  the property-tax bullet above). Known, not fixed: 0x023 has no sender after a weekly
  settlement because no settlement hook exists yet (the calendar changes periods only);
  `tax::collect_locked` reads `authority_for_zone` before the collection call, so the
  tower/owner/generation re-check inside `collect_property_tax_with` is the only guard against
  a capture between the two reads (the leader/fort gate is not re-read under the manager lock);
  the tests cover the collection sender, the forced release and the natural expiry, but not a
  full weekly settlement or a siege-caused early termination (UNKNOWN).
* systems::vehicles (`DestroyAndSpawnSlave` special effect 72): row 8224 (values 0) is the only
  row, on skill 19165, carried by slave template 125's mount skill. A spawner-backed slave is
  removed through its `SlaveSpawner` (`Despawn` broadcasts the removal and releases the slot)
  and the spawner creates the replacement at its authored transform; a slave without a
  spawner is destroyed and logged, with nothing respawned.
* systems::skills / world::session (zone-group buffs across teleports): the C# `Transform.ZoneId`
  setter ran `Character.OnZoneChange`, which is what removes a zone group's buff when the group
  is left (open sea: buff 7743 "irregular sea wind", the turbulence debuff, on zone groups
  29/30/36/39/40/59/60). Several ported teleports assigned the zone directly and skipped it, so
  Recall / portals / GM moves / resurrections out of open sea kept the buff until the next
  region change. Every server-forced destination now goes through
  `session::teleport_character` (`world/session.rs`), which applies the transform, moves the
  instance object table and dispatches `OnZoneChange`; the arrival is filed by
  `CSInstanceLoadedPacket` (`systems/instances/worlds.rs`) or, for a same-instance teleport, by
  `session::file_teleport_arrival`. A rider, a bound seat or a doodad bond is released first
  (`vehicles::force_dismount`, `PrefabChanged`, as the GM teleport and the trial do); the
  parentless transform would otherwise leave it half attached on the server. **Deviation:** `CSTeleportEndedPacket` now follows the
  client's reported landing position and refiles the region before repainting (the C# only ran
  `ResendVisibleObjectsToCharacter`), so GM `/move`, same-instance portals and navi teleports
  re-evaluate the zone without waiting for a movement packet. Only a teleport armed through
  `DisabledSetPosition` is followed; an unsolicited completion still repaints but changes
  nothing. The reported position is world-frame, so a character standing on another object is
  detached first. Resurrections
  (`CSResurrectCharacterPacket`, `Escape`) apply the statue position immediately because the 1.2
  client does not answer `SCCharacterResurrected` with `CSTeleportEnded`. Tests:
  `systems/skills/zone_buff_tests.rs`.
* systems::instances (`return_to_main_world`): **Deviation:** the C# clone's new `Transform`
  fired `OnZoneChange(0, mainZone)`, leaving the instance zone's chat channel and zone-group buff
  behind; the Rust passes the zone actually left (`world/session.rs::teleport_character`), so a
  zone-group buff of the instance zone is removed.
* systems::skills (`SextantPos` special effect 69): row 7181 (values 0) is the only row, on
  skill 18416 "measure my current position" (item 24795 Sextant). The caster receives a system
  chat message with the world position. No authored system-message coordinate format exists in
  the 1.2 `ui_texts` / `localized_texts` data (only the treasure-map item descriptions), so the
  current text is the placeholder `X: {x}, Y: {y}` and needs in-game verification.
* systems::skills (`NotifyQuest` 70 / `DeclareIndependence` 71): left log-only. `NotifyQuest`
  already fires from `plots::finish_item_plot` (a second event would double-count the quest
  objective); `DeclareIndependence` has no faction ownership runtime in 1.2.
* systems::npcs (`DespawnOnCreatorDeath`): the 1.2 C# loaded `despawn_on_creator_death` for
  `SpawnEffect` and `NpcSpawnerSpawnEffect` but never read it, so summoned NPCs outlived their
  creator. Rust registers every effect-spawned NPC under the caster's obj id and despawns those
  summons from `Unit.DoDie` of the creator (player or NPC). A creator that logs out or is
  despawned without dying still leaves its summons, as in the 1.2 C# (the later 10.x C# only
  forwards the flag to the native Zone: `WorldIntegration.cs` `PublishNpcSpawn` /
  `WZNpcSpawnerEventPacket`).
* systems::npcs (`no_penalty`): the 1.2 C# never loaded `npcs.no_penalty` (the 1.2 client's
  `npcs` query does). Rust loads it as `NpcTemplate.no_penalty` for the death path in
  `systems::skills::combat`; see the systems::skills death-penalty bullet for the behaviour and
  the capture-pending items.

* systems::services / systems::vehicles (mount speed analysis): `SusManager` samples the
  driver's (or owner's) `teleport_seq` and `disabled_set_position` for mates, and successful
  ride / unride operations reset the mate baseline. C# `SusManager` / `CharacterMates` did
  not invalidate that baseline for rider teleports or seat changes. This prevents a short
  teleport from being audited as mount speeding without changing either speed threshold.
  Speed reports also include object id, baseline / current positions, elapsed ms and instance
  id, captured by the analysis before advancing its baseline.

* Logging (`aaemu_commons::logging`, login and game): `Logs/` lives under the application
  directory instead of the executable's directory, so servers built into one target directory no
  longer share (and delete) each other's files. NLog's `deleteOldFileOnStartup` became "move the
  previous run's `Server.log` / `Error.log` to `Logs/Previous/`" (newest 9 kept). Additionally,
  `aaemu_commons::crash` logs every panic with a backtrace and appends it to `Logs/Crash.log`.
  On Windows x64 it also catches fatal native exceptions (stack overflow, access violation) and
  writes the unwound, symbolized stack and a minidump (`Logs/Crash/`). Release builds carry line
  tables (`[profile.release] debug = "line-tables-only"`) for those traces.
  The default levels are `Info` for the console and `Server.log` (NLog: `Debug` / `Trace`), since
  the per-packet `Debug` / `Trace` lines dominated the send path with many players in view
  (`RUST_LOG=info,aaemu_game::net=trace` restores the packet log); the log files are written by
  a background thread.
* systems::skills (item-cast plot end): `Plot.RunAsync` holds the `SkillSourceItem` object resolved
  when the cast started and sends `ItemUnlock` for it even when the plot consumed it. Here the
  item is looked up again at the plot end (`plots::finish_item_plot`,
  `ItemSkillHooks::unlock_item`); a source item that is gone (the last stone of a stack of the
  stone-throwing plot 349, skill 11268, a `use_skill_as_reagent` item the effects consume) has
  nothing left to unlock, so the packet is skipped with a `debug!` (plot / skill / item id)
  instead of the former `ERROR` ("SkillSourceItem ... no longer exists").
  `Character.ItemUse(item)` of the plot end takes the template id captured at cast start
  (`QuestSkillHooks::item_use_template`) instead of looking the item up by id, so the quest
  item-use event still fires for a consumed source item (quests 1315, 3541), as in the C#.
* net (game connections): sending never blocks and C# had no limit; here a client with more than
  16 MiB queued, or one that accepts no data for 20 s, is disconnected (`net::outbox`,
  `net::listener`). Broadcasts encode a packet once for all recipients.
* stream (`:1250`, public): a stream connection that has not completed `CTJoin` and sends an
  unknown opcode (or a frame shorter than an opcode) is closed, logged at `warn`; C#
  `HandleUnknownPacket` logged at error and kept the connection. A connection that does not join
  within 30 s of connecting (`stream::listener::JOIN_TIMEOUT`, also covers a stuck partial frame)
  is closed. Joined connections keep the C# behaviour (log and ignore). Unknown-packet dumps
  (stream, game, login, login<->game link) are capped to 64 bytes plus the total length
  (`aaemu_commons::net::hex_dump`) so a peer cannot flood the log; a malformed stream packet body
  logs at `warn`, and an unjoined connection's read failure no longer logs at `error`.
* systems::skills (character creation): the chosen ability's first starting spell occupies
  hotbar slot 1. The shared action originally assigned there moves to the starter's free slot,
  retaining the other authored utility and supply shortcuts. Further starting spells skip
  occupied slots. This replaces the C# layout that puts all shared defaults ahead of the
  chosen starter; saved hotbars are only loaded, not rebuilt.
* `LGPlayerEnter` for an already registered connection id: C# `Dictionary.Add` throws inside the
  login handler, which has no catch (the exception escapes into the network client, the world is
  shown inactive afterwards). Rust replaces the stale entry and replies 0 (see "Live testing").
* World entry and disconnect (`EnterWorldManager`, `GameConnection`, `net::listener`): the C#
  ignored `WorldManager.TryAddCharacter`'s result and removed the account
  (`AccountManager.Remove`) *before* the departure save. Rust refuses an occupied world entry and
  keeps a per-character reservation from the select (before the MySQL load) until the departure
  save has finished; the account is freed only after that save, so a reconnect cannot load the
  row while the previous session is still writing it. A duplicate account is still refused with
  the C# result (`AddAccount` -> `GLPlayerEnter` result 1 -> login `ACEnterWorldDenied(reason 1)`,
  the "enter world denied, reason 1" kick), now also atomically at `X2EnterWorld` because two
  cookies can be granted before either client registers the account; that race closes the
  losing connection like the invalid-token path and logs the account and session. The
  per-character reservation and the disconnect unwind guard are Rust-only. The listener cleanup
  runs under the guard: a panic in `on_disconnect` still releases the reservation, removes the
  character from the world and frees the account. Every refusal logs one `WARN` with the
  character name and the reason (`world/session.rs`, `managers/enter_world.rs`,
  `packets/c2g.rs`, `net/listener.rs`).
* Login world queue (2026-11-03, WP-H): the C# had no queue. The login server may now hold
  `CAEnterWorld` in a per-world FIFO when `GameServers[].MaxConcurrent` is reached and admit it
  when a slot frees. To make the game's occupancy authoritative, `GL_GAME_SERVER_LOAD` and
  `GLPlayerEnter` gain a trailing `u32 online` and `u32 occupancySequence` (the C# bodies end
  after the load byte / result byte; readers treat a missing extension as 0 = queue gate off).
  `online` is `AccountManager.count()` plus world cookies, including a cookie whose account
  registration is in flight (`login()` marks it accepted instead of removing it; `forget()` drops
  it after the registration). An unconsumed cookie stops counting toward the queue occupancy after
  `EnterWorldManager::PENDING_TOKEN_LIFETIME` (60 s, Rust-only) so a client that takes its cookie
  and vanishes cannot block the queue; `login()` still accepts a cookie of any age, so with the
  queue disabled its behaviour is exactly as before. The game also reports load right after
  registration and after an account leaves, so the queue head moves promptly. Premium queue lanes
  are not implemented: the premium-entrance bit of `CAEnterWorld.flag` is capture-pending
  (account WP-H).

* systems::skills / systems::ai (dead NPC returned and healed): the `DamageEffect` tail
  (`SCAiAggroPacket`, `AddUnitAggro`, `Npc.OnDamageReceived`) is skipped when the hit killed the
  NPC, and `CheckIfEmptyAggroToReturn` never sends a dead NPC to `ReturnState`. In the C# order the
  killer was re-added to the corpse's aggro table; when the NPC's `GoToDead` ran after that tail
  (its AI busy on another thread), `DeadBehavior`'s `ClearAllAggro` sent the corpse home and
  `ReturnState` restored its hp, so town guards kept fighting the mob they had killed (gray hp
  bar in the client). Test: `ai::flow_tests::guard_kill_while_the_mob_ai_is_busy_keeps_the_mob_dead`.
  (its AI busy on another thread), `DeadBehavior`'s `ClearAllAggro` sent the corpse home and
  `ReturnState` restored its hp, so town guards kept fighting the mob they had killed (gray hp
  bar in the client). Test: `ai::flow_tests::guard_kill_while_the_mob_ai_is_busy_keeps_the_mob_dead`.
* `Character.CheckWantedThreshold` (InfamyPoint >= 3000) / PIRATE-1: the Wanted / Contemptuous
  buffs are added under the character's data lock, but the faction is no longer assigned there.
  The check returns a "became a pirate" signal and every caller that has a live player runs
  `Character.SetFaction(Pirate)` after releasing the lock: the crime report / GM crime-points
  paths, the quest `AddCrime` path and `CSSelectCharacter` (for infamy that crossed while the
  character was offline; `Character.Load` itself is also used for the lobby list and deletion
  checks, where there is no player to notify). The offline quest `AddCrime` writes only the
  points, like `add_offline_crime_points`; it no longer writes `characters.faction_id`, so the
  login conversion still gets the live player, the broadcast and the house / guild / party side
  effects. The check compares the **saved** faction (`persisted_faction`), not the live one: a
  duelling / battlefield pirate shows a temporary Red / Blue / corps faction, and the live check
  re-fired `SetFaction(Pirate)`, ended the temporary faction and removed the pirate from the
  party. The trial verdict collects its `SetFaction` as an `Effect` like the other re-entrant
  calls (the trial mutex is held); a direct call there broke the module's lock rule.
  The change broadcasts `SCUnitFactionChangedPacket` once (the 1.2 client turns it into
  "$1 became pirate.", x2game `sub_396A1C20` with faction 161) and runs the ported
  `Character.SetFaction` side effects: house faction sync, guild removal and party removal.
  Deviation: the C# `ExpeditionManager.Kick(this.Connection, ...)` throws on the null lobby
  connection and aborts `CSListCharacter`; the Rust applies the change at select where the
  connection exists. The party removal looks the team up by membership instead of `InParty`:
  the select conversion runs before `CSNotifyInGamePacket` -> `TeamManager.UpdateAtLogin`, which
  would otherwise re-join the new pirate, and an ordinary member could not satisfy the owner
  check of the client kick path.
  The guild removal is a dedicated server-side path (`expedition::leave_forced`), not the
  client `Kick` handler: the C# `Kick`
  only acts on roles whose policy has `Expel`, so ordinary members stayed in the guild after
  becoming pirates. Evidence for the kick itself: the 1.2 loading tips `ui_texts` 2833
  (`tips_of_day_84`) and 2848 (`tips_of_day_99`) say a guild member who becomes a pirate is
  forcibly removed ("If you belong to a guild and become a pirate, you'll be kicked out of your
  guild"), with no owner exception; the C# implementation matches for ordinary roles but never
  handled the owner (`Kick` removed an owner like anyone, leaving `OwnerId` on a non-member).
  A guild whose alliance already is the new faction's alliance is left alone, so a pirate guild
  keeps its pirates (`alliance_id` of the mother vs the new faction; `ui_texts` 2847 allows
  pirate guilds).
  **Owner decision (2026-11-03, PIRATE-1, replaces the earlier dismissal rule):** a guild owner
  who becomes a pirate is removed too, but the leadership first passes automatically to a
  remaining member. The successor is chosen by the highest `role`, then the longest membership.
  `ExpeditionMember` stores no join timestamp: `last_world_leave_time` is the last world leave
  (set at join and refreshed on login / logout), so it measures activity, not tenure. The
  lowest `character_id` is the stable stand-in (ids are allocated in creation order, so the
  first-created character wins); this is an approximation. The forced hand-over and the old
  owner's removal happen in **one** expedition write section (the client `ChangeOwner` path
  shares the transfer, so both move the nation king and drop rename offers), so a concurrent
  forced removal cannot see the already-pirate old owner as an ordinary member and hand the
  guild back. A candidate whose top-level faction, live for an online member and saved
  `characters.faction_id` for an offline one, is not the guild mother's top-level faction
  (alliance / runtime nation) is skipped: a pirate heir would immediately undo the hand-over,
  while a same-alliance heir stays eligible. The hand-over emits the normal
  `SCExpeditionOwnerChanged` / role packets and saves like a client transfer. The 1.2 error 568
  (`expedition_owner_cannot_change_during_siege`) guards the voluntary `ChangeOwner` (Guild
  functions); the forced path bypasses it, as a pirate conversion cannot wait for a siege. The guild keeps its territory, siege registrations and nation;
  the nation's king (`nations.owner_character_id` / `owner_name`, taken from the founding
  expedition's owner) moves to the new owner. The king is written in the same transaction as
  the expedition save (no window with king = pirate and owner = successor) and broadcast with
  `SCFactionOwnerChangedPacket`; the 1.2 client's handler `FUN_391d2570` -> `FUN_392f82a0`
  copies the faction record, sets the owner id / name and applies it (`OnFactionOwnerChanged`).
  Only an owner who was the **last** eligible member dismisses the guild. That path takes the
  same `housing.operation_lock` and territory / siege check as the voluntary disband
  (`expedition.rs`) and re-checks the member count under the lock, but cannot refuse (error 301
  / 469) with no member left to hold the dominion: it releases the territory
  (`DominionManager::release_expedition`: owner cleared, unfinished siege cancelled), applies
  the same tower runtime and participant / phase broadcasts as the GM cancel, persists the
  released dominion with the dismissal, and clears the nation's king (`owner_character_id` /
  `owner_name`) so a pirate cannot keep the invite / kick rights of `owned_by`. The release is
  INFERRED (the rules only forbid a disbanded guild from keeping a dominion; no retail capture
  of this path). **Capture-pending: PIRATE-1** (no 1.2 capture of an owner turning pirate, a
  last-member dismissal, or the release).
  Known, not fixed: the login conversion's spawned guild removal can run after the login guild
  info was already sent (the client then shows the guild until the next refresh; score 2).
* `CheckForDeletedCharactersDeletion`: the rename / `deleted = 1` / packets are done;
  `DeleteCharacterAssets` (houses, guild, family, mail returns) is a later phase.
* `GameConnection.OnDisconnect` → `UpdateLoginTime(0)` for connections that never entered (inserts
  an `accounts` row with id 0 in C#) is skipped.
* `CheckForDeletedCharactersDeletion`: the rename / `deleted = 1` / packets are done;
  `DeleteCharacterAssets` (houses, guild, family, mail returns) is a later phase.
* `GameConnection.OnDisconnect` → `UpdateLoginTime(0)` for connections that never entered (inserts
  an `accounts` row with id 0 in C#) is skipped.
* Session ids are a counter, not `RemoteEndPoint.GetHashCode()`.
* Stats: `CalculateWithBonuses` is the identity (no buff / gear / passive bonuses) and equipment
  attribute contributions are 0, so MaxHp / MaxMp are the base unit formulas of the level. A missing
  unit formula evaluates to 0 (C# throws). (Superseded: see systems::skills below.)
* Buffs never expire on their own (no effect / dispel tasks), are never removed on move
  (`BuffRemoveOn.Move`) and have no effects; buff packets (`SCBuffCreated` etc.) are not sent.
  `Buffs.RemoveBuff` removes only the first matching effect, as in C# (phase 2 removed all).
* Phase 3a object model: C# virtual getters that read templates (`Npc.Scale`, `RaceGender`,
  `AnimActionId`, `MaxHp`, `Doodad.TimeLeft` funcs, `House.Template.*`) are stored fields that
  the owning subsystem keeps current. `DynamicBonus` copies `StartTime` / `Duration` of its
  source buff instead of referencing it. The NPC `SCUnitStatePacket` height adjustment
  (`GetReferenceHeight`) is not done (no heightmaps).
* `ChatIdManager` / `PrivateBookIdManager` now use the 1.2 C# ranges (`1..0xFFFE`,
  `0x1000..0xFFFFFF`); phase 2 had `1..0xFFFFFF` / `1..0xFFFFFF`.
  `save_rule_id` / `real_time` default to 0 / false when the `buffs` table lacks those columns.
* Quests: `Character.Load` reads the rows; `systems::quests::on_character_selected` builds the
  live quests from them (template check, `QuestInitialized` / `RequestEvaluation`, daily reset).
* Mail counts at select: deliberate deviation. The C# `Character.Load` -> `GetCurrentMailList` ran
  before the character was in the WorldManager, so login always reported 0 unread mails and mail
  that arrived offline never lit the client alert. The Rust counts the unread mail (`recv_date`
  passed, receiver, not READ) at enter world (`mail::count_unread_mail_for`, 2026-11-08, testers
  missed offline-created tax bills); no `SCGotMail` is sent at select.
* `Character.OnZoneChange` into a closed or unknown zone: the system message is sent but the
  forced transport out (5 x `SCErrorMsg(ClosedZone)`, then `SCCharacterResurrected` at the closest
  return portal) is not done (needs PortalManager return portals / resurrection).
* Unknown faction ids: C# `Faction` is null (NRE in `Write` / `Faction.MotherId`); Rust writes the
  stored id and uses mother id 0 (-> the null chat channel).
* `CSMoveUnit`: only player characters exist, so slave / vehicle / mate / transfer moves are
  ignored like a non-matching `targetUnit` cast (superseded: vehicles, sticky parents, fall
  damage and the Sus analysis are ported); `LastPacketActivityTime` is `systems::services`.
* `LeaveAllChannels` iterates channels in key order (C# `ConcurrentDictionary` order is
  unspecified). Court (trial) channels exist and are included in the leave set.
* The C# keeps `Connection.Characters[id]` and `ActiveChar` as the same object; Rust copies the
  active character back into the lobby list when `LeaveWorldTask` runs.
* Combat / slave logout delays (x30, +10 min) are not applied (no combat state / slaves).
* systems::economy: item operations are serialised by the economy operation lock (`ItemTxn`) and
  their packets / quest events are flushed after the container locks are released, in the C#
  order; character packets sent during an operation go through the same queue (`Out`).
* systems::economy (item lock, 2026-11-03): the C# only logged `CSItemSecure`, `CSItemUnsecure`,
  `CSEquipmentsSecure` and `CSEquipmentsUnsecure`; Rust implements them from the 1.2 client. The
  client sends `CSItemSecure`/`CSItemUnsecure` for one bag item and the equipment pair for the
  whole equipped set (`FUN_394ea660`, `FUN_394ea730`, `FUN_394e5dd0`, `FUN_394e5ea0`). Only
  items in an `item_categories.secure` category and not listed in `item_secure_exceptions` can be
  locked (`FUN_394df4a0`; the 1.2 DB has 135 secure categories and the single exception row
  3006, an item absent from the 1.2 `items` table). Locking sets `ItemFlag.Secure`; unlocking
  sets `unsecure_time = now + content_configs[43] * 60` (the client's
  `X2Item.GetSecurityUnlockDelayTime` reads `FUN_395720e0(0x2b)`; its dialogs divide the value
  by 60 to print hours, so the 1.2 value 4320 is 4320 minutes = 72 hours) and keeps the flag, so
  the item stays locked until `UpdateItemTimers` clears it and sends `ITEM_UNLOCK_EXCESS`. The reply task is `SCItemTaskSuccessPacket` with `ItemUpdateSecurity`;
  the `isUnsecureExcess` / `isUnSecureSet` / `isUnpack` combination is inferred from the client's
  userdata field names and the `ITEM_LOCK` / `ITEM_UNLOCK` / `ITEM_UNLOCK_EXCESS` task types
  (**CAPTURE-PENDING**, case item-lock-flow: no 1.2 capture of these server replies exists yet).
  A locked item is refused by the NPC store sale, specialty-pack sale, player trade put-up and
  completion, mail attachment (mail result `ITEM_LOCKED` 26), auction put-up, destroy, item-look
  conversion, disenchant, split/merge/move/swap (the client's drag validation returns 664 for
  either side, `FUN_390455d0`/`FUN_39045850`), coffer moves, locked design/furniture placement,
  regrade and lunagem targets, and putting down or dropping a locked trade pack, with
  `item_secure_condition` (664; the 10.x zone server uses 664 for the same item operations).
  `ItemContainer::ConsumeItem` skips locked stacks; crafting, quest hand-ins and reagent checks
  no longer count them, and a cast whose only reagent stacks are locked is refused with the
  client's `SkillResult.ItemLocked` (0x25). `CSEquipmentsSecure` locks every equipment slot
  including slot 26: the client's `FUN_394e5dd0` loops that slot too (the special `0x1a` entry),
  and it checks the flag rather than the item category, so the server mirrors that. The lock
  handlers send their error after the bag lock is released. Equip/unequip of a locked item is
  refused by the same move guard (the client's drag check covers it).
* systems::economy (item procs, 2026-11): procs fire where the C# never did. Sources, rebuilt
  by the gear-bonus pass (`apply_gear_buffs` -> `procs::refresh`, so equip / unequip, login,
  repair, gear break and gem changes all refresh them): set
  `equip_item_set_bonuses.proc_id` when the piece count reaches `num_pieces` (destroyed pieces
  still count, like the set buffs), `item_proc_bindings` of every equipped non-destroyed item
  (the C# never loaded the table), and `holdables.item_proc_id` of every equipped
  non-destroyed weapon (the C# never read the column). Deduplicated by proc id per unit.
  Deviations from the C#: the roll is `rng in [0, 100) < chance_rate` (the C# passed at
  `rate + 1` percent; `item_level_based_chance_bonus` is ignored, **CAPTURE-PENDING
  item-procs-1**); the cooldown blocks until `cooldown_sec` after the proc last fired and the
  stamp is per unit / proc id, surviving a re-equip in the session (not persisted; a refused
  cast puts the old stamp back), where the C# checked it inverted and never stamped; kinds 1-8
  roll on the attacker for a landed melee / magic / ranged / siege hit, 9-16 on the victim
  (`SkillHitType` supplies the crit filter), kinds 17-20 have no 1.2 data and are not rolled.
  The proc skill is cast by the owner, bypassing the GCD; a self-targeted skill (75 of the 92
  proc skills) targets the owner, any other targets the other unit of the damage event
  (**CAPTURE-PENDING item-procs-2**); the cloned template has `casting_time` / `mana_cost`
  zeroed (two proc skills carry a 2.0 / 2.67 s cast time; 14921 also has mana 100). The rest
  of `use_skill` still runs (**CAPTURE-PENDING item-procs-3**): a proc is refused while the
  owner is casting or channeling (and can stop a channel for a skill with
  `stop_channeling_on_start_skill`), it resets the owner's `skill_cancelled` /
  `condition_chance` mid-skill, a unit requirement failure warns per hit, and an NPC
  `NO_TARGET` pokes its AI.
  A proc skill's own synchronous damage does not roll procs again (thread-local cast guard);
  its projectile / `effect_delay` / `effect_speed` / plot effects and the DoTs it applies fire
  later, outside the guard, and can roll procs again (bounded by the item-proc and skill
  cooldowns; **CAPTURE-PENDING item-procs-4**). A proc skill's hit therefore does not roll the
  victim's take-damage kinds (**CAPTURE-PENDING item-procs-5**). DoT ticks and Heal damage
  effects do roll procs in this implementation. The proc packets are broadcast before the
  triggering `SCUnitDamaged`. The hot path allocates the per-kind id lists and takes up to six
  unit locks per hit; left as is. The active procs and cooldowns live on
  `UnitBase.item_procs`; death does not clear them (the C# has no reference either).
* systems::economy (account attributes): the C# `AccountAttributeManager.Apply` saved synchronously;
  the Rust applies the effect to the cache and persists in a background task (skill effects are
  synchronous), serialised per account by the account lock. Each save takes the cache snapshot
  while holding the lock, so overlapping effects (saved buffs re-applied on login, bot add) cannot
  interleave their DELETE / INSERT or commit an older snapshot last.
* systems::economy: `CSRepairPetItemsPacket` repairs owned mate equipment containers at the
  current nearby blacksmith (the native 10.x button at `0x399093D0` supplies its NPC id).
  Repair bills cannot exceed available money; repairing destroyed equipped gear restores its
  bonuses. Mate gear changes and item unpacking require the authenticated owner. Received-mail
  read, attachment and delete operations require the receiver; sent-mail reads require the sender.
  Auction cancellation requires the seller. Coffer opens and moves reuse the existing chest
  permission rules and require a nearby live coffer; moves require the caller's open session.
  Ordinary inventory packets resolve item ids only inside the caller's inventory containers.
* systems::economy: `AddOrMoveExistingItem` with the item's own container as target relocates the
  item (the C# inserts the same object a second time into `Items`). `SplitOrMoveItemEx` looks up
  the source / target items in the two containers only (the C# `GetItemByItemId` searches every
  item; an item in a third container is treated as a phantom item). Where the C# throws
  (`KeyNotFoundException`, `NullReferenceException`, `AcquireDefaultItem` failure), the port logs
  and aborts the operation; `Wipe` drops an item `CanDestroy` refuses instead of looping forever.
  The "no longer exists, possibly a phantom item" case (client request naming an item the player no longer holds) is
  looked up in the player's containers only and logged at `warn` with the full request, not at the C#'s `Logger.Error`.
* systems::economy: trade items are kept by id (an item no longer in the giver's containers or put
  up twice is an error); a missing trade partner is ignored instead of half-applying the change.
  Leave-world and hard disconnect cancel the departing actor's trade and prune its invitations
  before world removal, notifying the peer through the existing cancellation packet. Cleanup
  is idempotent and rejects an old player handle when the object id belongs to a new session.
* systems::economy: auction lots are enumerated in id order (`ConcurrentDictionary` order in C#),
  search / cheapest lot skip lots whose item or template is missing; mails are kept in a map that
  reproduces the C# `Dictionary` enumeration order; mail dates are whole unix seconds.
* systems::economy (auction messages, 2026-11-04): `SCAuctionMessagePacket` (0x133) is sent to an
  online seller when a bid is accepted (`msgType` 0, `AUCTION_BIDDEN`) and when a lot sells
  (`msgType` 1, `AUCTION_BOUGHT_BY_SOMEONE`; both the buy-now and the timed-winner path), with
  the item template id and the money. Offline sellers get only the result mail. The C# never
  sent this packet. Layout from the 1.2 client reader `FUN_397b56f0` (u8, u32, i32); the msgType
  meaning from the handler `FUN_394cf660` (events 0x1fc/0x1fd) and the seller-side ui texts
  3047/3048. Deviation: the outbid refund uses `MailType.AucBidFail` (17, the client's
  `AUCITON_BID_FAIL`) instead of the C# `AucOffFail` (15), so its mailbox label is the
  bid-failure one.
* systems::economy (auction declare tab, 2026-11-05): the 1.2 client's declare-siege tab is the
  normal `CSAuctionSearch` with `filter = 1` (`X2Auction.SearchDeclareSiege`, EVIDENCED in the
  client decompile): it lists only lots whose item template has `auction_a_category_id = 11`
  (`declare_siege`, the six Siege Camps). `filter = 0` is unchanged and still lists every lot
  (whether retail also hid the camps from the normal tab is UNKNOWN); any other value is
  treated as 0 and logged once. The C# ignored `filter`.
* systems::economy (system auction lots, WP-SD1b, 2026-11-05): the retail GM command
  `auction_post_siege_items` (name in the 1.2 client; arguments UNKNOWN, assumed
  `[duration] [startPrice] [buyoutPrice]`) posts one lot per distinct
  `siege_zones.declare_item_id` (the six Siege Camps) with `client_id = 0` and an empty seller
  name. Defaults: 48 h duration class, the item's own `price` as the start price, no buyout.
  Each item is created once through `ItemManager::Create` and kept in the auction escrow
  (loose, `SlotType.Auction`, owner 0) so the normal settlement path can mail it; a failed run
  discards every item it created. Every settlement path skips the seller leg for
  `client_id == 0`: the sale mails only the buyer (`AucBidWin`), the money is destroyed (no
  seller mail or live notice, no `AUCTION_SOLD` record), and an expiry without a bid deletes
  the item instead of returning it. The optional `auction_cancel <lotId>` command removes an
  unbid system lot and deletes its item (the lot-id argument and the refusal of player lots /
  lots with a bid are assumed). `client_id = 0` rows load normally on restart. The C# had
  neither command and no system-lot path.
* systems::economy (expired auction items, 2026-11-08): the item timer only visits the
  equipment / bag / bank containers of online characters, so a timed item on the auction house
  never expires while it sits in escrow (a re-listed camp lot, or any timed item, could be sold
  and delivered after its lifetime ended). Rules: an already expired item cannot be posted
  (refused with the client's existing invalid-item code 240, ui_texts 241 "cannot put up the
  item", item kept); a bid or buyout is refused with the same code before any money moves, so
  a buyout can neither lose the buyer's debit nor refund an already-refunded old bidder a
  second time; a lot whose escrow item expires before settlement is not delivered -- the
  winner is refunded in full by the normal bid-fail mail and the item is deleted, with no
  seller share and no `AUCTION_BUY`/`AUCTION_SOLD` credit (the refund mail is delivered before
  any state change, so a refused mail leaves the lot for the next tick; a winner that no
  longer resolves to a character is released with an error instead of retrying forever); and a
  delivery taken from the mail after its item expired is deleted instead of entering the bag
  and reported to the client like an expired bag item (`SCSyncItemLifespanPacket`, `added =
  false`); this covers mails persisted before this fix and items that expire while the mail
  waits (the delivery mail carries no price, so no refund is minted there). Timed items are
  otherwise still accepted: only an already expired one is refused (no 1.2 evidence for a
  general timed-item rule). The shared `item_timers::expired_at` matches the container timer's
  definition. Tests: `an_expired_item_cannot_be_posted_and_reports_the_invalid_item_code`,
  `an_expired_lot_refuses_bids_and_buyouts_before_any_money_moves`,
  `an_item_that_expires_in_escrow_refunds_the_winner_and_is_deleted`,
  `a_failed_expired_lot_refund_keeps_the_lot_and_retries_once`,
  `an_unresolvable_expired_lot_winner_deletes_the_item_once_and_removes_the_lot`,
  `an_expired_auction_delivery_is_deleted_instead_of_taken`.
* systems::economy (mail retention, 2026-11-08; attachment-safety follow-up): the flat 14-day
  receipt policy is replaced by the 1.2 client's `mail_guide` (`ui_texts` 4465): normal mail
  (types 1-3, 4) is kept 5 days after it was read or 30 days unread; system mail is kept 5 days
  after it was read or 90 days unread. `Configurations/Mail.json` (`ReadRetentionDays`,
  `UnreadRetentionDays`, `SystemReadRetentionDays`, `SystemUnreadRetentionDays`) overrides
  them. The read clock is `open_date`, written when the mail is opened or an attachment is
  claimed; legacy read rows without it use the receipt date (as the 10.x server does). Mail
  whose attachments are still unclaimed keeps the unread deadline. **Escrow is never destroyed
  by the timer:** unread player mail and read player mail with unclaimed attachments are
  returned to the sender; system mail with unclaimed attachments is kept until the receiver
  claims it (there is no player sender to return it to) unless the operator sets
  `DeleteSystemMailWithAttachments = true`; REGISTERED mail, self-addressed mail, a deleted
  sender's mail and a letter already returned once are kept with a warning. Only
  attachment-free mail is deleted after the window. A returned letter with unclaimed
  attachments is kept forever (it never bounces twice). The commercial mailbox
  (`CHARGED`/`PROMOTION`/`MIA_*`) is not on this timer: its own guide (`ui_texts` 4464) says
  read mail is deleted at once and unread mail is kept.
  **Upgrade grace:** the `2026-11-08_aaemu_game_mail_retention.sql` marker's
  `updates.install_date` is the first start with this version; mail that already existed then
  gets `deadline = max(rule, marker + window)` (`Mail.RetentionGraceStartUnix`, filled by
  `main.rs`; 0 disables it). The marker script is `DO 0;` and records itself in `updates`.
* systems::economy (mail send limits, 2026-11-08): `send_mail_to_player` refuses a sender below
  level 10 (`SCLevelRestrictionConfigPacket.mail`, the hard-coded C# value) with
  `MailResult.LevelTooLow` (22), and refuses a fourth identical mail in a row with
  `SentSameMailTooManyTimes` (29). The latter is `MAIL_REPETITION` (`ui_texts` 6315: the same
  mail was sent "3 or more times in a row"); allowing three and refusing the fourth is
  INFERRED from the text. The key is (receiver, title, body text): the same text to another
  character is a different mail, and a body-less item-only mail is never refused (it has no
  content to compare); an empty-body send still breaks the streak. The streak is per sender,
  in memory, and cleared on leave world. `MAIL_COOL_TIME` (`ui_texts` 6314) names no interval
  in the 1.2 client or its data, so no cool-time check is enforced.
* systems::economy (mail leftovers, WP-I4): `/mail admin <receiver> <title> <text> [money]
  [[itemId count] ...]` is a Rust-only GM command (the C# had no such command); `ADMIN` (5) is
  one of the mail types the 1.2 client lists (`mail/common.alb`). It stores a sender-id-0 system
  mail through `mail::send_system_mail`, so the receiver (online or offline) gets the same
  `SCGotMailPacket` / mailbox / attachment behaviour as other system mail; items are created
  once, up to `MAX_MAIL_ATTACHMENTS`, and discarded again if the store refuses the mail. The
  sender name is the literal `Admin` (the client has no special case for an `ADMIN` sender).
  `quest_mails` / `quest_mail_attachments` are loaded read-only into
  `game_data.systems.economy.quest_mails`; a mail whose non-zero attachment row is missing is
  skipped. `SCQuestMailSentPacket` (0x150) is defined from the client reader `FUN_397b5f00`
  (wrapper `FUN_391b19b0`, one `u32` through slot `0x3c`) and is never sent: the trigger and the
  meaning of the id are capture-pending (**CAPTURE-PENDING QUEST-MAIL-1**). Mia mail (types 11
  `MIA_SEND` / 12 `MIA_RECV`) is the client's separate commercial mailbox (`comercialmail/*`,
  `X2MiaMail`, `unreadMiaMail`) fed by the cash-shop path; the C# notifier branch was empty and
  the existing counter routing is unchanged (**CAPTURE-PENDING MIA-MAIL-1**). The tax-rate
  notices now use the same `send_system_mail` path: `TAXRATE_CHANGED` (8) on a dominion tax
  rate change and `NATIONAL_TAX_RATE` (23) on a national tax rate change (see "National tax
  (RENAME-8)"). `BILLING` (6) already carries the house-tax bill / overdue notice
  (`MailForTax`). `USER_BILL` (22), `PROMOTION` (10) and `SPAM` (4) have no client mail view
  or text in 1.2 (`read_mail.alb`; `round6-items-mail.md` 5), so they are closed as not 1.2
  features. `NATION_TAX_RECEIPT` (24) has client texts (`ui_texts` 5635-5636) and a plausible
  weekly national-tax trigger, but no sender yet; the MIA / hero / mobilization types stay
  capture-pending.
* systems::economy (money guards, 2026-10-20): `CSDepositMoney` / `CSWithdrawMoney` refuse
  non-positive amounts and require a banker NPC (`banker` template flag) within 5 m of the current
  interaction object; `CSBuyItems` accepts goods only from a pack of the interacting NPC (every
  `merchants` row, the client's union) or the doodad's `DoodadFuncStoreUi` pack (the C# validated
  only the NPC's last pack and sold any template through a doodad); `CSSellItems` requires a
  merchant within 3 m, lists each item once and pays only for items actually moved to buy-back
  (the C# paid repeated entries and failed moves, with
  wrapping arithmetic); auction requests require an auctioneer NPC (`auctioneer` template flag)
  named by the packet ids or the current interaction object within 5 m (the C# ignored both ids
  and the `AuctionManager` only null-checked). The 5 m bank / auction range matches the other NPC
  service interactions (`repair.rs`, `mates.rs`); retail's own range is unverified.
* systems::economy (loot range, 2026-10-20): `CSLootItemPacket` (and loot-all from the bag)
  requires the player within 5 m of the loot owner. The C# defined
  `LootingContainer.MaxLootingRange` (200 m) only for loot *eligibility* at generation and never
  checked the take; 5 m matches the other NPC service interactions (`store::SERVICE_RANGE`,
  `portal::NAVI_USE_RANGE`). Refusals answer `SCLootItemFailedPacket(TooFarAway)`.
* systems::economy: `CSChangeItemLookPacket`, `CSSpecialtyRecordLoadPacket`,
  `CSBuySpecialtyItemPacket`, `CSReturnMailPacket` have no 1.2 opcode (0xfff): decoders exist, the
  1.2 client never sends them. (`CSSetCraftingPayPacket` is not one of these any more: the
  owner approved its 1.2 opcode `0x8d` on 2026-11-05, see the craft grade section.) The `Specialty` config section is now read at startup (ratios,
  ticks and `TradePackMailDelayInMinutes`), so those values no longer come from the C# defaults
  alone. `CSSpecialtyRecordLoadPacket` is still not registered: the 1.2 client has no request
  opcode for the history, and the server deliberately sends no `SCSpecialtyRecordsPacket`
  (the client discards it; see the specialty protocol note below).
* systems::economy (native 1.2 protocol, 2026-10-25): correction to the preceding ReturnMail
  claim: the Return button (`read_mail.lua:946`) calls `ReturnMailById` (`3948B630` ->
  `393CD290`), which sends opcode `0xa3` with an i64 mail id (`397BA450`). The preserved
  opcode table misnames that slot `CSReportSpamPacket`; economy now registers ReturnMail
  using that legacy constant, and social no longer claims it. The historical C# registry
  fixture stays intact with an explicit native correction in its comparison test. Native
  `SCLootItemFailedPacket` reader `397C2F00` consumes `ErrorMessage:i16`, an eight-byte
  lootable and `type:u32` (14 bytes); the C# int error width is no longer mirrored.
* systems::economy (specialty goods / records / sale values, 2026-11-04):
  - `CSListSpecialtyGoodsPacket` (0x044, sent by the 1.2 client's check-goods skill /
    `FUN_393bf840` with the interacted NPC object id) now answers `SCSpecialtyGoodsPacket`
    (0x09a). The 1.2 reader `FUN_397aec90` reads an `i32` count (capped at 20) and calls
    `FUN_398a9e30` per record: `u32 itemId`, `i32 refund`, 1-byte `Currency`, 1-byte unknown
    (10 bytes on the wire). The list content is not evidenced, so the server sends an empty
    list (count 0) until a capture shows which goods/prices belong there.
  - `SCSpecialtyRecordsPacket` (0x09b) is **not implemented or sent**: its 1.2 reader
    `FUN_397b3930` is `i32 zoneId`, `u32 itemId`, `i32 count` (capped at 256), then
    `FUN_3961edb0` records (`i32 ratio`, `u64 recorded`), but the body is discarded
    (`FUN_394fb300` is an empty stub). Keeping the captured trader-talk sequence exact means
    no 0x9b on the 0x043 answer, and no price-history table is kept.
  - `CSSpecialtyRatioPacket.id` is the pack template, not a zone: the client's 0x43 writer
    (`x2game.dll.c:781792`) sends `*puVar3`, where `puVar3 = FUN_39695dc0(character+0x1840)`
    is the equipped backpack's template. The handler answers for the requested id and falls
    back to the equipped pack when the id is not a specialty pack.
  - `SCSpecialtyCurrentPacket` (0x1f4) stays in the 1.2 reader's order (`FUN_397b87c0`):
    `i32 count`, `u16 from`, `u16 to`, then (`u32 itemId`, `i32 ratio`) pairs — the same order
    the 1.2 C# used; the earlier "reader order" note in this branch was wrong.
  - Sale labor is the C# flat 60 by default. `Specialty:ScaleSaleLaborByCommerce` (default
    false) applies the 1.2 `Actability.GetLaborCostMultiplier` table (Commerce step,
    1.00/1.00/0.95/0.90/0.85/0.80/0.77/0.77) instead, never below 1; the generic 1.2 skill
    path applies the same table (`Skill.cs:1394`). **Capture-pending (LABOR-1):** the 1.2 C#
    charged the flat 60 for a pack sale and skill 25205 has `consume_lp = 0`, so scaling on
    this path is a server option, not a captured retail rule.
  - `BackpackProfitShare` is read from the global runtime `FeaturesManager.Fsets`
    (`server.systems.services.features`), not `FeatureSet::default()`; the 1.2 sell / mail UI
    reads the same bit via `X2Player.GetFeatureSet("backpackProfitShare")`.
  - The crafter id comes from `Item::crafter()`: the dedicated `crafter_id` column, or the
    legacy `made_unit_id` for packs crafted before that column existed (see "Item crafter").
    The seller and crafter mail titles use the
    1.2 client `ui_texts` keys `sell_backpack_title` / `mail_sell_backpack_title_seller` /
    `mail_sell_backpack_title_crafter` from `localized_texts` (4745 / 6193 / 6194), with the
    client's en_us strings ("Specialty Payment", "[Delivery]", "[Crafter]") as the fallback
    (never the untranslated `ui_texts.text` column). The 6192 `mail_actual_gain_tip` split
    wording and its percentages are rendered by the client from the body's `receiverCase` and
    the seller/crafter counts. A sold pack with a crafter
    is covered by a flow test through the real mail bridge
    (`sold_pack_with_a_crafter_delivers_the_crafter_mail`).
  - **Trade-pack mail delay (2026-11-08):** the 1.2 client says the sale price plus 5 % interest
    arrives by mail after 22 hours (`SOLD_BACKPACK_REFUND_SENT_BY_MAIL`, `ui_texts` 4743;
    `sell_backpack_content`, 6195). The C# default `TradePackMailDelayInMinutes = 480`
    (8 hours) is replaced by 1320 (22 hours) in `Configurations/Specialty.json` and in both
    defaults; `/settradepackmaildelay` still overrides it. The demolition-notice delay (22 h in
    the C# for a failed-tax demolish) is a separate deviation: see the housing row above.
  - **Capture-pending (NEGOTIATION-1):** the 1.2 client `ui_texts` (`sell_backpack_body2`,
    `bargaining_money`, `SPECIALTY_MARGIN` 626 / `SPECIALTY_COIN_MARGIN` 677) prove a Commerce-
    proficiency bargaining bonus and its notice mails, but no formula, threshold or percentage
    is evidenced in the 1.2 client or data. `Specialty:NegotiationBonusPercent` (default 0,
    clamped to 0..100, the C# behaviour) applies a configured percentage to the pre-interest
    sale price; the bonus, its mail field and the 626/677 notices are implemented, the retail
    numbers are not claimed.
  - **Video payouts pinned; duplicate npcs sale removed (2026-11-09):** the live sale is this
    file's `sell_specialty` through `store::CSSellBackpackGoodsPacket`. The second
    implementation, `systems/npcs/specialty.rs` (never registered, no live caller, used
    `FeatureSet::default()` and a fixed 0 negotiation bonus), was deleted with its data,
    manager, timers, packets and `EconomyBridge` sale hooks. The 2014-10 NA trade-run videos
    (grade C, `D:/aaemu-audits/video-evidence/trade-runs-2014-10-ayin.md`) pin four packs
    against the real data and the live flow (gated tests
    `real_compact_db_specialty_payouts_match_the_2014_video` and
    `real_compact_db_specialty_mail_split_and_delay_match_the_2014_video`): Arcum Iris
    Lavaspice at Kalina 124 % base 34 310 -> 42 544 shown / 44 672 mailed; Falcorth Snowlion
    Yarn at Robert 111 % base 34 310 -> 38 084 / 39 988; Tigerspine Grape Jam at Jasmine
    130 % base 39 910 -> 51 883 / 54 477; Mahadevi Elephant Cookies at Delara 130 % base
    10 750 -> 13 975 / 14 674 = 1 Gilda Star. The sale window shows the pre-interest amount;
    the mail carries it +5 % after 22 h (1320 min), and the crafter/seller split is 20/80
    (Tigerspine: 10 895 / 43 582). The video's 2 c (Arcum) and 1 c (Falcorth) display
    differences come from the rounded ratio shown ("124 %" for 124.0x).
* systems::services: authenticated `CSRequestCharBriefPacket` and `CSCharDetailPacket` requests
  return online character profiles through the existing 1.2 outbound formats (the C# inbound
  handlers only logged). Offline, absent and unauthenticated requests receive no reply; offline
  lookup and failure-response semantics remain unresolved.
* systems::services (UI data caps, 2026-10-20): `CSSaveUIDataPacket` accepts only keys `0..=16` and
  values of at most 16 KiB. Values observed from the real 1.2 client are 280, 4 708 (key bindings)
  and 313 bytes; 16 KiB is 3.5x the largest and stays below the 64 KiB `options.value` TEXT
  column. Unknown keys and oversized values are dropped with a warning (the C# stored any
  `ushort` key and any length in memory).
* systems::economy (multi-pack NPC shops, 2026-11-05): the `merchants` table is additive; the 1.2
  client joins it with `merchant_goods` per NPC (`strings.txt:13227`), so every pack of an NPC
  sells. The C# kept only the last `merchant_pack_id`, which lost goods on the 38 NPCs with
  several rows (NPC 626 lost all 26 horses of pack 154). Duplicate pack rows (NPC 11413 has pack
  145 twice) are collapsed; pack order does not decide the catalog.
* systems::social (direct team packets, 2026-11-05): `CSLeaveTeam` (0x7d), `CSKickTeamMember`
  (0x7e) and `CSDismissTeam` (0x84) are the direct forms the 1.2 client's confirmation callback
  sends (`FUN_39309670` -> `FUN_393095c0`); they run the same `AskRiskyTeam` action as
  `CSAskRiskyTeamAction` (same membership / owner checks, target-in-team check and broadcasts).
  The C# only logged them.
* systems::social: family title and steward requests require targets in the caller's own family.
  Authorized packet changes call the existing family snapshot save immediately; the C# handlers
  only changed memory. Team risky actions require membership and known actions, with owner
  authorization for kick/dismiss and a self target for leave before any side effects.
* systems::social: `SCChatMessagePacket` writes the packed 64-bit chat identity `(type, sub type,
  faction)` in its first eight bytes (direct Say / whisper / system text uses `(type, 0, 0)`); the
  sender faction stays in the sender block. Channel copies substitute the actual `ChatChannel` sub
  type / faction while keeping the requested chat type (zone Shout / Trade / LFG and raid /
  raid-leader included).
* systems::social: **Chat spam.** The 1.2 client applies the delays of
  `SCChatSpamDelayPacket` itself (native `FUN_393C2540`: chat types `0..=14` map to a group and a
  message inside the group's delay is refused with localized message `0x108`). The server now
  sends the configured groups/delays (`Configurations/Chat.json`, `ChatConfig`) and enforces the
  same limits before routing a message, answering with `SCErrorMsgPacket(0x108)`, the code the
  client's own throttle displays. Whisper (`-3`) and user channels (`15`) stay unlimited, as in
  the client. The client stamps its `last_send` when it sends, the server when the message
  arrives, so a stock client's message at the delay can arrive early under jitter; the server
  grants `min(250 ms, delay / 4)` of grace and uses a monotonic clock. The retail delay values
  are not in `compact.sqlite3` and were not captured, so the shipped `Chat.json` keeps every
  delay at 0 (the previous C# behaviour, **capture-pending CHAT-SPAM-1**). To enable the limit,
  set `Chat.ChatGroupDelays[i]` (milliseconds) and optionally `Chat.ChatTypeGroups` in
  `Configurations/Chat.json`; the same values are sent to the client and enforced by the server.
* systems::social: **`SCChatFailedPacket`.** The 1.2 reader `FUN_397c2950` is
  `i16 ErrorMessage ; string chname (max 10)`; its handler `FUN_391d8de0` raises the client's
  `CHAT_FAILED(error, chname)` event (the `chat_msg_event.alb` handler displays the error text).
  With `Chat.ChatFailedPacket` on (`Configurations/Chat.json`, default `false`) the server
  answers the channel-bound failures it already detects with this packet: the user-channel
  join / leave / message errors `CHAT_NO_CHANNEL` 270, `CHAT_CHANNEL_ALREADY_EXISTS` 269,
  `CHAT_ALREADY_JOINED_CHANNEL` 271, `CHAT_PRIVATE_CHANNEL` 272, `CHAT_WRONG_PASSWORD` 273 and
  `CHAT_NOT_JOINED_CHANNEL` 467 carry the requested or stored channel name (capped at the
  packet's 10 bytes), while the "not in" errors of party 267, raid 268, expedition 311 and
  family 412 carry the labels "Party", "Raid", "Expedition" and "Family" (the 1.2
  `chat_msg_event.alb` handler displays only the error, so these strings are informational and
  retail's exact values are not captured). Every other chat
  failure (whisper 265 / 437, the raid-owner check 496, the spam delay `0x108`) keeps
  `SCErrorMsgPacket`. **Capture-pending (CHAT-FAILED-1):** which failures retail 1.2 sent as
  `SCChatFailed` is unknown, so the flag defaults to off and the frames stay exactly as before.
  Tests: `chat_failed_writes_i16_error_then_the_channel_name`,
  `chat_failed_channel_name_is_capped_at_ten_bytes_on_a_char_boundary`,
  `channel_bound_chat_failures_use_scc_chat_failed_only_when_enabled`.
* systems::social: **Chat level gate and GM mute (WP-SL4b).** `Chat.MinLevelByChatType`
  (`Configurations/Chat.json`, empty by default) maps a chat type to a minimum character level;
  a sender below it gets `SCErrorMsgPacket(CHAT_CANNOT_SEND_SINCE_LEVEL_LOW 786, 0, true)` and
  the message is not routed. The 786 text is "You must be at least Lv$1 to chat.", but the
  `SCErrorMsgPacket` argument / `$1` substitution mechanism is UNKNOWN, so no level argument is
  sent. Characters with `access_level >= 100` are exempt (the closed-zone GM threshold; the
  account-level elevation is not read on this synchronous path). The retail GM command
  `/restrict_chat <character> [minutes]` (name EVIDENCED, argument shape UNKNOWN; the default
  of 60 minutes and `0` = lift are INFERRED) writes `characters.chat_restricted_until` (UTC):
  an online character through its live state (saved by the save manager), an offline one with
  a direct row UPDATE. Both outgoing chat paths (`send_chat_message` and the user-channel
  `send_user_chat`) refuse a muted sender with `SCRestrictInfoPacket` 0x1c8: the 1.2 reader
  `FUN_397c4570` -> `FUN_397beb40` is `chname (max 11) ; i32 index ; u8 restrictCode ; u32
  duration ; i64 startDate ; i64 endDate`, and the handler `FUN_391e0be0` passes only `duration`
  to `FUN_393c4170`, which shows `restrict_remain_time` with `duration / 60 + 1` minutes
  (so `duration` is remaining seconds). The client reads neither `index`, `restrictCode` nor
  the dates; the server sends `index 0`, `code 0` (chat), `duration` = `chat_restricted_until
  - now`, `start` = that now and `end` = `chat_restricted_until` (the mute start is not
  persisted; INFERRED, ignored by the client). Retail's trigger for the notice is UNKNOWN and
  the refused chat attempt is INFERRED (**`CHAT-RESTRICT-1`**). Starting or lifting a mute of an
  online target sends `SCResultRestrictCheckPacket(code 0, result 1 / 0)`; `FUN_391e0ea0` turns
  result 1 into the client's chat-restriction flag plus `restrict_start`, and 0 into the flag
  restore plus `restrict_end`. The push is not sent when the mute expires on its own (no timer;
  the client's flag/label is only restored by a lift packet or a relog) and is not persisted
  across a relog. `CSRestrictCheck(code 0)` answers from the same persisted mute; `code 1` (the
  lobby connection check, the only code the stock client sends) always answers 0 because there
  is no login-restriction store yet. `chat_restricted_until` is loaded and saved with the
  character. Retail's per-type levels are not evidenced (**capture-pending CHAT-LEVEL-1**).
  Tests: level gate / default off / GM exemption, mute + expiry (the notice carries the
  remaining seconds), user-channel bypass, `restrict_chat` online / offline / non-GM (online
  start/lift send result 1/0), `CSRestrictCheck` code 0 muted/unmuted/expired and code 1, the
  packet byte layout, and the MySQL character round trip.
* systems::social: **Words.** `blocked_texts` is applied to character names only (`check_name`);
  the client's chat masking (`check_chat`, `ignore_texts`) happens in the 1.2 client before the
  message is sent (`FUN_393C76B0` / `FUN_393C78E0` / `FUN_393C7A10`), and `replace_chats` /
  `replace_chat_keys` / `replace_chat_texts` are the client's local expression easter eggs
  (`FUN_39375920` called from the Say path). No 1.2 server-side handling is evidenced, so the
  server does not rewrite or mask chat text. The `chat_spam_rules` keyword chain is loaded by
  the client but never read in 1.2 (only its loader touches the tables), and the C# server never
  read it either: it stays unused, **capture-pending (CHAT-SPAM-2)**.
* systems::social: **Character names.** Retail 1.2 validates a character name against
  `allowed_name_chars` (2352 rows: lowercase Latin and 2326 Hangul syllables; native
  `FUN_39572620` / `FUN_395728B0`): a one-byte ASCII letter or space always passes, every other
  character must be listed, so digits and Cyrillic are refused. The server loads the table into
  `CharacterData.allowed_name_chars`, but installing it into `NameManager` is **opt-in**:
  `CharacterNameUseAllowedChars` (default false) avoids breaking the stock regex (digits,
  Cyrillic) and the test client's generated names; when enabled, one startup log says the
  whitelist overrides `CharacterNameRegex` for names it does not list. A missing table or a
  NULL/malformed row is skipped, not fatal. `blocked_texts` with `check_name` already refused
  forbidden words. Guild creation uses only `XlIsValidName` in the client (`FUN_392FA240`), so
  `Expedition.NameRegex` stays its only rule.

* systems::social: justice repairs intentionally differ from C#: crime/infamy accumulation and
  sentence durations saturate instead of wrapping; a live evidence doodad can be reported once.
  Arrests cannot create duplicate cases, and the holding-cell offender buff is applied. Jury
  acceptance requires an actual invitation, current eligibility and the court's faction; only
  the occupied seat's player can confirm records or submit a valid, final vote. Repeated pleas
  and terminal-case cancellation cannot resentence the defendant. Justice teleports update
  server transforms, instance membership and visibility, and yaw is consistently in radians.
  Every exit path cleans jurors and removes trial buffs; returns respect `AllowJuryEscape`,
  and cleanup releases only its own court.
  Disconnects vacate jury seats or close the defendant's abandoned case while retaining the
  pending sentence (minimum one stored minute) and court region. Login enforces it independently of jury
  eligibility. The normal-save bad buffs 631/2028 persist even their final seconds; their
  non-real-time duration resumes on reconnect. The existing 1.2 plurality sentencing choices
  and first-seat tie ordering remain. Tests cover all six verdicts, default sentencing,
  invalid/duplicate votes, occupied courts, disconnects and real compact prisoner definitions.

* systems::quests: rewards, items, money, exp, skills, NPC faction, teams, mails go through
  `quests::hooks::QuestHooks` (`GameQuestHooks`; the item / money / mail part is the economy's).
  World effects (component skills, NPC faction, team-shared events) run after the character
  lock is released instead of inline; item hooks that raise quest events are queued to the
  end of the running operation.
* systems::quests: model sizes are 0 (no model data) in `GetAround` / distance checks; buff tags
  are not loaded (`CheckBuffTag` false); `EquipShield` unit req is false; debug messages use the
  character access level only. The labor margin reads the account's real payment / premium grade
  (`unit_reqs::max_labor` -> `premium_benefits.max_labor`, WP-B); the old note said "non-premium
  cap 5000".
* systems::quests: C# exceptions inside quest code are modelled where reachable with the 1.2
  data (`SetObjective` on a non-objective act, `MaxObjective` division by zero, the timer step
  switch): they abort the event invocation list / sphere tick. Out-of-range objective reads
  that would throw in RunAct / score sums read 0 (no 1.2 quest triggers them).
* systems::quests: the daily reset runs at 00:00 UTC (C# cron in server local time).
* systems::quests (cinemas, 2026-11-01): the C# `CSCompletedCinemaPacket` re-sent every visible
  object (`SCUnitState` / doodads) after **every** cinema. The Rust server re-sends them only
  after a CUT_SCENE cinema, or after one it cannot identify. 1.2 client evidence (x2game.dll):
  `BeginCutScene` (`0x3915B960`) removes every unit except the local player and clears all
  doodads only for a sequence whose root `Flags` has bit `0x8` (CUT_SCENE) set, `SCUnitState`
  is dropped while it plays (`0x391D4C30`), and nothing is restored at the end. The other
  cinemas (92 of the 111 quest-component cinemas, e.g. every `sl_*` story video) remove
  nothing, so after them the unconditional re-send repeated the create of every nearby unit and
  doodad the client still had, without a remove (in a live run the client kept one entity per
  id, so it is redundant rather than visible, but it replays e.g. the rider attach). `cinemas.name`
  is looked up in `game/worlds/<world>/cinemas/<name>.xml` at startup (`quests/cinema.rs`,
  `main_world` first) and only the `<Sequence ... Flags="...">` attribute is read; a cinema
  without a client file is "unknown" and keeps the C# behaviour (an empty world after a cut
  scene is worse than a redundant re-send). Which cinema plays is known from
  `CSRequestPermissionToPlayCinemaForDirectingMode`, whose `u32` is the quest **component** id
  (not the quest context id; body `u32 component, bc npc, bc doodad`; the client never waits
  for an answer): the component's `cinema_id`, else its `QuestActObjCinema`. A `CinemaEffect`
  applied to a character names its cinema the same way. The client sends the request when the
  quest dialog opens and `CSStartedCinemaPacket` only when the player accepts / turns in (32 s
  later in a live run, minutes are possible), so a request names the next started cinema for
  10 minutes (`REQUEST_TTL_MS`); the client plays `QuestActObjCinema` objectives on quest start
  without a request. `QuestActObjCinema` now only claims a started cinema that is unknown or
  its own (the C# claimed whichever cinema started, so the cinema of another component could
  complete or block the objective).
  A cut scene also clears the client's own-mate records, and the re-send then re-creates the
  horse (`SCUnitState`, `SCMateState`, `SCUnitPoints`) and re-attaches the rider
  (`SCUnitAttached`). The client's attach-to-mate path (x2game `0x39355C80`, from
  `OnUnitAttached` `0x391D49D0` -> `0x39362770` -> `0x390B3F10`) looks the mate record up by tl
  id (`0x3933F000`) without a null check and crashes reading `0x20` in `0x3933E600` (a mounted
  player after a cut scene: 2 of 2 live runs); a horse that is out but not ridden survives
  but is lost as the pet (`X2Mate:IsPlayerPetExists()` false). Only `SCMateSpawnedPacket`
  (`OnMateSpawned` `0x391DB180` -> `0x3933FD00`) creates the record, so before the re-send of a
  cut scene (or unidentified cinema) the owner gets an `SCMateSpawnedPacket` for each of its
  spawned mates again (`vehicles::mates::resend_owner_mates_spawned`; the summon builds the
  same packet in `mate_spawned_packet`).
* systems::quests (component named in the update, 2026-11-01): `Quest.SetStep` clears
  `ComponentId` ("used by packet only"), so the final C# sent `SCQuestContextUpdatedPacket`
  with component 0 after every step change (`engine.rs` `run_current_step`). The 1.2 client
  plays the cinema of a non-dialog component when an update names it: the handler `0x3937BDF0`
  runs `0x39378580(component)` for the five trailing ids of the packet, which applies the
  component's client effects and, unless the component is dialog-driven (`0x397D42D0`: Start
  with act type 13/16, Progress with act 3, Ready with act 20/21), starts its cinema
  (`0x39056110`). So the 15 non-dialog Progress components with a cinema (e.g. quest 4376
  component 19053 `zq_19_life` on `QuestActObjItemUse`, 2264/9983 `nu_01_scott` on
  `ObjItemGather`, 2386/10257 `el_01_dagger` on `ObjInteraction`) never played theirs. The C#
  before the quest rewrite (`Quest.Update`, `50130e2b2~1`) sent the current component id.
  Deviation from the final C#: the update of the evaluation that **completes the Progress
  step** carries the component `run_components` left in `component_id` (the last passing
  one), once; the updates of the new step (and of evaluations that do not complete a step) are
  as before. Every quest gets this. Other steps are not named: `SCQuestContextStartedPacket`
  already named the Start component, and the client re-runs a component's effects for every
  packet that names it. When that component has
  a cinema (`QuestData::component_cinema_id`) it becomes the character's requested cinema
  (`QuestState::requested_cinema`), like a permission request, so the following Started /
  Completed packets know the cinema (cut-scene re-send decision, logs).
* systems::quests (SaveTutorial, 2026-10-20): `CSSaveTutorialPacket` accepts only real tutorial
  entries (`quest_contexts.category_id = 45`, 101 rows in the 1.2 data, kept in
  `QuestData::tutorial_ids`) and at most 256 of them. The C# stored any id as a completed quest,
  so a modified client could grow the completed-quest block set without bound.
* systems::quests (reward exp / copper pool, 1.2 oracle): the Reward component's own
  `QuestActSupplyExp` / `QuestActSupplyCopper` act **replaces** the level-based
  `quest_supplies` value instead of being added on top of it (`engine.rs`
  `distribute_rewards`). The act itself still runs immediately (`acts.rs` `SupplyExp` /
  `SupplyCopper`): the immediate exp keeps its unscaled grant, and the let_it_done reward
  ratio keeps scaling whatever lands in the reward pool — the copper act pool when the
  copper act is present, the level supply otherwise — as before. The C# added both
  (`Quest.cs` `DistributeRewards`). Sources: the 1.2 client's
  `X2Quest.GetQuestContextRewardExp` / copper lookup (`x2game` FUN_394a1370 / FUN_394a0fd0)
  scans the Reward component for act type 0x18 / 0x19 and falls back to `quest_supplies`
  only when the act is absent, and the Oct–Dec 2014 archeagedatabase.net export shows
  exactly the act value on 103 quests and the level supply on 897 others (quest 2785
  level 35: 8,400, not 8,400 + 3,224; quest 1213: 110, not 110 + 420). In `compact.sqlite3`
  917 of the 1,569 quests with such an act give exp 0 and copper 0 through it, 376 of them in
  `quest_categories` 79 ("탐험", exploration; all 402 quests of the category are level 50, and
  377 carry the acts): the act is what makes them pay nothing, and the C# sum would hand each of
  them the full level-50 `quest_supplies` exp / copper on top.
* systems::quests (reward scaling, `Quest:RewardScaling`, default `false`; `porting-md#2`,
  capture-pending): the 1.2 client's quest window scales its displayed reward — the
  `quest_supplies` fallback by `quest_contexts.degree` in `FUN_397d5eb0` / `FUN_397d5dd0`
  (0 / 1 -> x0.5, 2 -> x0.75, 3 -> x1.0, 4 -> x1.25, 5 and above -> x1.5; a NULL degree
  reads as 0), then exp (the Reward act value or the scaled fallback) by the quest-vs-player
  level gap in `FUN_397d3810` (`g = ((int)((p+1)*0.2+0.5) > 4) ? p : 5`, `d = g - p + q`:
  full for `d >= 0`, 50 % for `d < -4`, otherwise `(int)(d*base*0.1 + base)`; from player
  level 22 the reduction is disabled). Copper has no gap factor, an act value is never
  degree-scaled, and the let-it-done ratios are unchanged. `systems/quests/reward_scale.rs`
  implements exactly that; `engine.rs` applies it to the `quest_supplies` fallback and
  `acts.rs` (`SupplyExp`) to the exp act. **Server authority is EVIDENCED** (the quest
  packets carry no reward amount; the client formula only decides the displayed number), but
  whether the original zone server applied the same factors is **INFERRED** from the client
  UI: showing a number different from the payout would be a visible bug on every degree != 3
  quest. The 2014 archeagedatabase.net export still lists unscaled supply values for the 221
  non-degree-3 quests, but it is a database dump, not a grant record, so it cannot settle the
  question. The flag keeps today's unscaled payout until the live check decides the default:
  read `X2Quest.GetQuestContextRewardExp/Copper` in the 1.2 quest window for a degree-2 quest
  and for a quest far below the character's level, complete them, and compare with the exp /
  copper actually gained (`SCExpChanged` / money). Equal with the flag on means the retail
  server paid the displayed number; the lean is then to default the flag on.
* systems::quests: the October 2026 quest audit corrects timer event isolation, explicit NPC
  reporting, per-quest NPC/doodad reporting, doodad reward selection/finalization, and OR
  requirement evaluation. Timed saved deadlines, aggro credit, CheckSphere, and authored
  content gaps remain tracked in `Docs/wiki/Quest-Audit.md`.
* systems::quests (quest restart, 2026-11-03): the C# `FailQuest` only set `Step = Fail`
  (no status, no packet), so a timed-out or escort-failed quest stayed `Progress` on the wire
  and in the journal. `fail_quest` now sets `Status = Failed` and sends
  `SCQuestContextUpdated` with it; the 1.2 `OnQuestContextUpdated` status-2 branch is what
  shows the failure. There is **no automatic restart**: the 1.2 client never reads
  `restart_on_fail`, and restart is the player's `CSRestartMainQuestPacket` (0x0fd) request.
  The server implements that packet, gated on an active non-completed quest with
  `detail_id == 2` and `restart_on_fail` (**QUEST-1 INFERRED gate**; the original server's
  exact reply sequence is capture-pending), and answers with `SCQuestContextStarted`. A
  failed quest is not restarted on its own. `successive` is client display only
  (**QUEST-2 resolved**).
* systems::quests (restart follow-ups, known and not fixed, score < 5):
  - A restart discards and reinstalls the quest, so the Start component's skills run again on
    every accepted 0x0fd (7 restartable quests cast Start skills, some with
    `NpcSpawnerSpawnEffect`). No shipped 1.2 UI sends the packet; the 3 s throttle bounds a
    crafted client.
  - Supply items that were mailed or moved out of the bag are issued again on restart. The
    restartable supplies are category 64 items priced 0 (low value).
  - If the fresh install fails after the old run was discarded, the quest is lost (WARN only);
    no 1.2 data quest is known to reach that path.
* systems::quests (engine fixes, quest-audit 2026-10-17):
  - `TryCompleteQuestAsLetItDone` (C# only set `Step = Reward`): the quest also gets the
    `Completed` status, and a quest without a Reward step is finished at once (C# hung there:
    quest 5134 and the data stubs). With a Ready step but no Reward step the early completion
    turns the quest in at the Ready step (report acts satisfied, so its copper / items are paid);
    with neither step it completes directly. No level based exp / copper is added for a quest
    without a Reward step: the C# only added it while running the Reward step's components, and
    the 328 level>0 quests without a Reward component are equipment upgrade stages, instance
    staging quests and `reserve` stubs whose reward is the item or nothing.
  - `DropQuest` runs the act cleanup (`QuestCleanup`, consuming cleanup-only items) for every
    quest that reached its Reward step or `Completed` status, so early-completed quests no
    longer keep their cleanup items (the C# ran it for every drop; the Rust port restricted
    it to completions, which the early-completion path never matched).
  - Selective rewards: the client sends a 1-based index (`quest_context_task` Lua:
    `GetSelectiveItemIndex()` must be >= 1 when `GetQuestContextRewardSelectiveItemAllCount` is
    > 0, and is passed unchanged to `TryCompleteQuestContext`), matching the 1-based
    `ThisSelectiveIndex`. Quests that reach their Reward step without a valid choice (no report
    act: auto-complete / journal reports send 0) now get the first selective item instead of none.
  - A saved quest with status Ready on its Progress step is legal for LetItDone and score
    quests (the status shows "ready to turn in", the step is left by a report). On any other
    quest the status is stale: `load_from_rows` resets it to Progress and the load evaluation
    promotes the quest to Ready when the objectives are complete.
  - A score quest whose Progress step has no objective acts (5641: score 100, no acts) counts
    its score as reached (C#: stuck forever). A `COMPLETE_QUEST_CONTEXT` requirement that names
    the component's own quest (6596) is dropped at load (C#: never acceptable).
  - `CompletedQuest.id` is 32 bits (the C# `ushort` wrapped for quest ids >= 4194304 and marked
    nonexistent quest 3805700 instead of the event quests 8000001-8000004). The column is
    `int unsigned`, stored rows are unchanged. `SCCompletedQuests` has a 16 bit block id, so
    such blocks are not sent (the client cannot show them); a warning names the first one.
  - Refused quest requests (`AddQuest`, `TryCompleteQuestAsLetItDone`) are logged at WARN with
    character, quest and reason; the same refusal repeated within a minute goes to DEBUG.
* systems::social: saves of guilds / families run inline in the handler (C# synchronous); a
  failed guild save keeps the removed-member ids for the next attempt. `ExpeditionConfig`
  defaults to the shipped `Expedition.json` (C# throws without the section). Player names /
  positions in team packets are snapshots taken when the packet is built. `DeleteCharacterAssets`
  does not send the guild/family packets to the lobby character. The duel flag height comes from
  the npcs heightmaps (`SocialHooks::terrain_height`). Crime record packets prefetch offline
  victims (C# `Character.Load` inside `Write`).
* systems::skills: supersedes the phase-2 bullets above on stats (`CalculateWithBonuses`,
  `UpdateGearBonuses` are ported), buffs (expiry, `BuffRemoveOn.Move`, effects, buff packets) and
  `CSMoveUnit` fall damage.
* systems::skills (damage reflection, 2026-11): the C# loaded `reflection_chance` /
  `reflection_type_id` / `reflection_ratio` / `reflection_target_ratio` and never read them; the
  Rust applies the type/chance/target-ratio core (36 real rows, per-type). **REFLECT-1
  (capture-pending):** `reflection_ratio` is ignored and the reflector still takes the full hit
  (no 1.2 server evidence for "share still taken" vs "share returned"). **REFLECT-2
  (capture-pending):** several matching reflect buffs are not summed; the first successful
  chance roll is the one reflection (summing two 100% buffs would exceed the received damage).
  **REFLECT-3 (capture-pending):** the reflected hit is an ordinary `SCUnitDamagedPacket` cast
  from the buff (`CastAction::Buff`), with the hit category's normal `SkillHitType`; the 1.2
  `SkillHitType` has no Reflect value, so the `REFLECT` combat-text key cannot be requested.
  The reflected damage is applied through `reduce_current_hp` only: it rolls no item procs,
  no combat buffs and no `Attack`/`Damaged` events of either side, and it never re-enters
  `damage_with_aoe`, so two reflectors cannot ping-pong. A reflected kill uses the normal death
  path (death packet, kill credit, durability); the attacker may die before its own hit's tail
  side effects finish (the hit still lands).
* systems::skills (`use_damage_amount`, 2026-11): the C# ignored the trigger column and the
  `Damage` event (kind 3) never set `IsTrigger`, so fixed heal/mana values were never scaled by
  the event damage. `RestoreManaEffect` now scales a fixed roll by the amount when the trigger
  sets `use_damage_amount` and skips its `percent` maximum-mana scaling (753 restores 3% of the
  damage, not 3% of max mana); `Damage` events carry the real amount (806, 4257). Only heal and
  restore-mana effects read the amount; the Charge `SpecialEffect` rows are left unchanged.
* systems::skills: `CompressedGamePackets` (DD04) are written with stored (uncompressed) deflate
  blocks; the client accepts any valid deflate stream.
* systems::skills (stealth, 2026-11): the C# stealth predicate was buff tag 100
  (`TagsEnum.Stealth`), but the 1.2 client hides a unit from the `buffs.stealth` column
  (`ClientUnit::ApplyStealth` / `CanDetect`, x2game `FUN_3935c270` / `FUN_3935a2b0`; the client
  ships the same buff table). The column differs from tag 100 in 40 rows: 71 stealth buffs, of
  which 30 lack tag 100 (255, 719, 1078, 1080, 1371, 1432, 1443, 1502, 1514, 1573, 2284, 2341,
  2345, 2486, 2554, 2979, 3155, 3527, 3594, 3752, 3772, 3866, 4099, 4566, 4631, 4648, 5205, 6058,
  6474, 7003), and 10 tag-100 buffs that are not stealth (557, 1678, 1899, 1941, 1966, 3637, 5901,
  6260, 6380, 7164). `buffs::has_stealth` uses the column for the AI `CanSeeTarget` and plot
  conditions 15/16 (deviation from C#). `skills.keep_stealth` (33 skills, 30 with
  `cancel_ongoing_buffs`) was loaded by the C# but never consumed: a keep_stealth skill now skips
  its active stealth buffs in its own start/use cancel loop (`trigger_remove_on_keep_stealth`) and
  in its `SkillUse` (special 33) chain, while attacking / taking damage still break stealth
  through the buff's own remove flags. The real-DB test lists the 71 / 51 / 33 rows.
  Detection range / angle (2026-11, research `stealth.md` task B): `ai::npc::can_detect` mirrors
  the client's `ClientUnit::CanDetect` (`FUN_3935a2b0`). A stealthed unit is visible only inside
  `(100 + unit attribute 94) / 100 * formula` metres: formula 4 `DetectStealthRangeFront` when it
  is in front of the observer (the client's local-Y dot, i.e. the ±90° facing sector) and formula
  10 `DetectStealthRangeBack` behind (`DAT_3999cf90` = 0.01). The variables are the unit levels
  (`source_level` / `target_level`) and `stealth_level` = the highest `ab_level` of the target's
  active `buffs.stealth` effects, which is what the client caches in `ClientBuffMan` +4
  (`FUN_3989f230` reads the buff instance word +0x2E, the `sourceAbLevel` of `SCBuffCreated`).
  `buffs.anti_stealth` on the target sets the client's "cannot hide" byte (`ClientBuffMan` +8),
  so such a target is always visible. `buffs.detect_stealth` on the observer reveals stealthed
  units at any range (server rule; the client's `CanDetect` never reads that column, so this is
  the server applying the column's documented meaning; the C# consumed neither column either).
  A missing formula or a formula that fails to evaluate leaves stealthed units visible and logs
  once per process, instead of blinding the AI (`FormulaKind::DetectStealthRangeFront` / `Back`
  are the only formulas the predicate needs).
  Party / raid members that are not hostile are visible (the client's `FUN_39308ca0` rule and
  its `FUN_39358ab0` relation check, which the server approximates with the faction relation;
  `FUN_39358ab0` actually compares fields +0x24 / +0x2c of the +0x3c28 block, capture-pending),
  as are the observer's owner-linked mate / slave (the client only checks the observer's owner;
  the server also accepts the reverse link). The C# `< 1.0` / `< 0.5` fallbacks in
  `Behavior.CheckAggression` are gone, so they no longer decide. Deviations: the client compares
  the range against the collider distance, the server uses `BaseUnit.GetDistanceTo` (planar
  centre distance with both model radii subtracted); the client's party lookup is the local
  player's slot list, the server asks the team manager (players only). The AI greeting
  (`players_in_range`) and `npc_faction_set` paths still ignore stealth, as in the C# (parity).
  Open: no 1.2 data row applies the `anti_stealth` buff 5212 (a script would have to), and a
  capture of the original client at 5/10/20/40 m front/back has not been made (the formula,
  attribute and constant are from the decompile and the shipped data).
* systems::skills: `SpecialEffect` repeats are scheduled tasks instead of `Thread.Sleep` loops.
* systems::skills (cuboid AoE): `AreaShape.ComputeCuboid` tested the triangles (v0,v1,v2) and
  (v1,v2,v3) of a rectangle whose vertices run around it, so the two overlapped and the wedge at
  the v3-v0 edge was never hit, and the strict `< 0` sign test dropped points exactly on a diagonal.
  Skill AoEs (`geometry::around_shape`) and doodad area triggers (`area_trigger::AreaShape::
  compute_cuboid`) now share `utils::math::point_in_cuboid`, which rotates the point into the
  shape's frame and checks `|along length| <= value1 && |along width| <= value2` (edges included),
  so both agree on the same `aoe_shapes` row (the doodad side used to mirror the bug).
* systems::skills (2026-10-12 audit): OR requirements now require one success and use the resolved
  packet target, including doodads. Equipped-item and NPC-group requirements check their actual
  membership; crime-point limits and overnight time windows are enforced. Source/target life,
  swimming, stun, walking, angle, range and optional physics
  obstruction checks run before GCD/buff/resource changes and again at cast completion. Missing
  mana preserves ongoing buffs and cast timing, including mana drained during a cast. Labor and
  reagent availability are checked before starting and firing, including alternative reagents.
  Active direct casts cannot be overwritten; a
  new cast either stops a channel under its configured policy or is rejected.
* systems::skills (2026-10-12 audit): channel buffs have distinct caster/target owners. Direct
  channel ticks spend mana and apply effects; channels without ticks apply their effects once
  at completion. Cancellation invalidates timers, cleans up both buffs and emits one end
  notification without clearing a newer task. Repeated stops cannot complete a timeline twice;
  canceled delayed effects are suppressed, and a reused skill resets its per-cast state.
  Repeated special actions carry their cast generation, so canceled or reused casts cannot
  act through old callbacks after their timeline ID has been released.
  Delayed effects and repeated actions require the captured world handles to remain live;
  despawned or recycled objects are skipped, and an abandoned delayed cast releases its timeline.
  Channels also stop on despawn; target buff cleanup compares the original handle identity
  before removing anything from a reused object ID.
  Delayed SkillUse actions retain both entity handles and their originating cast generation;
  canceled or reused casts cannot launch a child skill. Delayed disturbance, buff timers and
  controllers also reject removed or recycled entity handles.
  Auto-attack callbacks require the current skill and cancellation token, so a stale loop
  cannot attack through a replacement character or clear a newer auto-attack slot.
* systems::skills (2026-10-12 audit): direct AoE respects cone angles; individual guaranteed-hit
  effects bypass miss results without changing other effects' cached rolls. Magic miss/resist
  joins the miss set. Percentage rolls use the exact 0/100 endpoints, retaining SkillUse's
  explicit 0 sentinel. Buff unit events follow the owner; Landing, Attacked, ChannelingCancel,
  RemoveOnMove and RemoveOnDamage execute, including short jump landings without fall damage.
  NPC AI faces a non-self target before cast angle validation; turning only after a successful
  cast prevented sideways melee attacks. Self-targeted skills retain their heading.
* systems::skills (2026-10-12 audit): Create Mana/resource conversions, percentage health/mana
  restoration, server cooldown resets, direct-cast interrupts, spawner despawn and Escape's
  MoveToRezPoint timeout now execute. Charges accumulate before refresh/notification; target
  charges use their own buff and multiplier; both actor radii scale. Explicit buff removal
  dispels once, knockback immunity reads text booleans, doodad summons keep radians and the
  selected frame, Return keeps radians in its fallback and sends both notifications, and
  negative breath changes clamp at zero. FakeDeath clears incoming aggro without killing the
  target (native WZFakeDeath handler 0x3936F060). PlayLogEffect is recognized as telemetry.
* systems::skills (2026-10-12 audit): the extracted 1.2 client database and server database have
  the same 15126 skill IDs and 9816 buff IDs. Both lack 33 unique buff templates referenced by 35
  BuffEffect definitions. The loader warns with both IDs; replacement templates are not
  fabricated. RedeemBuff's second and third value slots (the damage / cap slots of other special
  actions), recipe/account/cinema placeholders and several other special actions still need
  authoritative behavior or protocol evidence. The generic Rope / Anchor / Flowgraph controllers
  are decided in the WP-9 subsection (Rope handled by the harpoon family, Flowgraph test-only,
  Anchor two monster skills). Passing covered tests does not certify every data-defined skill.
* systems::skills (2026-10-27, `check_obstacle`): the C# tree had no obstacle check at all
  (`git show a2ab80abd^:AAEmu.Game/Models/Game/Skills/Skill.cs`), but the 1.2 skills data has
  the retail column. The 1.2 client is the oracle (`x2game_unpacked.dll`; Ghidra `aa12u`):
  `FUN_390B5540` calls `RayWorldIntersection(Game)` with objtypes **7** (static + sleeping rigid
  + rigid), or **0x107** when `check_terrain` is set, ray flags **0x2048F** (colltype-any,
  collision class 2, ignore back faces, pierceability 15) and one hit; a hit maps to skill
  result 0xC (`skill_obstacle`). `FUN_390B5650` shortens the end point by the target radius and
  casts no ray when `distance <= radius`. Unit targets (`FUN_390B5C60` -> `FUN_3989C0D0`) run at
  unit position height and skip the source and target colliders; position (ground) targets
  (`FUN_390B5A30` -> `FUN_390B5790`) use the constant 0.5 (`DAT_3999C47C`) and skip only the
  source. The former Rust ray was 1 m above both ends, used static+terrain (0x101) and the
  default collision class, and had no radius gate; it rejected ground-anchored doodad
  interactions through nearby doodad colliders (entrance doodad template 3674 / skill 10999,
  exit 4535 / skill 16560) and re-checked at fire time with the same wrong rule. Rejections are
  logged at `warn` with character, skill, target and result key (NPC casters at `debug`: AI
  retries blocked casts). The flow tests in
  `cast_validation_tests.rs` cover the height/mask/class rule, the radius gate, the 0.5 m
  position radius and the terrain gate. Not modelled: the multi-part target variant
  `FUN_3989DAE0` (loops the target's sub-shapes and passes if any part passes).
  **Doodad targets are exempt** (2026-10-28): the client's cast target check for wire type 4
  (`FUN_3939E030`, IDA `sub_3939E030`) only looks the doodad up, and `FUN_390B5540` (skill
  result 0xC) is reached only for unit (`FUN_390B5C60`) and position (`FUN_390B5A30`) targets,
  so a doodad's origin inside static map geometry (production: craft 6231 / skill 16766 at the
  Two Crowns specialty bench, doodad 4235 at 13824.67 10604.76 120.54, whose ray always hit map
  brush cell (13, 10)) must not refuse the interaction. This supersedes the earlier skip of
  the owning house's collider for its fixtures.
  **Ray height** (2026-10-29): the ray is raised 1 m above both ends again
  (`OBSTACLE_RAY_HEIGHT` in `validation.rs`, applied after the radius gate and the radius
  shortening), superseding the "unit position height" (feet level) of 2026-10-27. On the shared
  test server the feet-level ray refused 65 ordinary casts that the 1.2 client had allowed and
  sent, every one a hit on plain terrain on a gentle slope (skill 11918, `check_terrain`, hit
  normals within 0.17 of straight up). The height the 1.2 client really uses is not verified; 1 m
  is the former Rust/C# value. NPC casters use the same rule.
* systems::skills: the labor margin requirement uses `GetMaxLabor(Payment.PremiumState)` with the
  account's real payment and premium grade from `systems::services::premium` (`premium_benefits`
  `max_labor`: 2000 / 5000), loaded from `aaemu_game.accounts` (WP-B). The C# had the permanent
  default `AccountPayment` (premium until 2030), so its cap was always 5000.
* systems::skills (in-place resurrection, 2026-10-20): `CSResurrectCharacterPacket(in_place =
  true)` requires a pending `SCNotifyResurrectionPacket` offer (`skill_char.resurrect_offer`, set
  by the `Resurrection` special effect and the free resurrection below
  `minimum_exp_loss_level`). The C# never checked, so any dead client could revive itself in
  place without an offer.
* systems::skills (resurrection scrolls on dead targets, 2026-11-08): the three skills the
  `Resurrection` special effect (type 22) audit singled out — 15066 "부활", 25518 "전투 중 부활"
  (items 18456 / 32568 / 34022 through `items.use_skill_id`) and 26647 "저승의 문턱 탈출
  주문서" (items 34018 / 34021, used by the dead character on itself) — are castable on a DEAD
  player. `validation::check` honours the real flags (`target_alive 'f'`, `target_dead 't'`;
  26647 also `source_dead 't'`) and the type-22 handler sends `SCNotifyResurrectionPacket`; no
  dead-target filter along `CSStartSkill` -> `use_skill` refuses them. Regression tests:
  `systems::skills::resurrection_target_tests` (hand-built data with the real flags, and an
  ignored real-data test casting the shipped scroll items 32568 / 34018). `core-character#8`
  is closed: every skill-based offer path goes through the one effect handler and the packet
  stays as the 1.2 reader expects (no 10.x `inPlaceRemainCnt` / `reason`).
* systems::skills (corpse self-casts, 2026-11-09): the C# `Skill.Use` never checked
  `target_alive`; our `validation::check` does, which refused every `OnDeath` np_skill aimed at
  the dying NPC itself (`source_dead 't'` + `target_alive 't'`, e.g. Muddy Sarracenia's 11478
  that summons the quest 817 "Invasive Species" seed doodad 1082). A `source_dead` skill whose
  target is its own caster is no longer held to `target_alive`; other dead targets still are.
  Test: `cast_validation_tests::a_corpse_casting_a_source_dead_skill_on_itself_is_not_refused_as_a_dead_target`.
* systems::skills: `Character.DoDie` is complete (exp / durability loss, escalating respawn timer,
  PvP honor / assists / kill counts, War-zone honor loss, `DropTradePackToFloor`). The PvP
  damage / heal histories are lists of (character id, unix ms) in `skill_char`.
* systems::skills (death penalty and respawn wait, 2026-11-03): `Character.DoDie` now reads the
  two 1.2 tables the port ignored. `npcs.no_penalty` (88 rows: guards, siege cannons, battlefield
  pets, alliance NPCs; the 1.2 client loads the column in its `npcs` query) suppresses the exp
  and death-durability loss when the killing attribution (skill / DoT caster) is such an NPC;
  the respawn timer still runs. `resurrection_waiting_times` replaces the port's hard-coded
  `[15, 30, ... 240]` s table and its 5-minute reset: the 1-based consecutive-death step selects
  the row, `penalty_duration` (600 s on every 1.2 row) keeps the step alive, and an active
  dominion siege in the death zone selects `siege_waiting_time` (20/15/10/5/0 s) over
  `waiting_time` (0/5/45/90/180 s). The client loads the same table but only displays the
  `resurrectWaitTime` the server sends in `SCUnitDeathPacket`, so the server values drive the
  countdown. **Deviation:** the C# table (15..240 s, 5-minute reset) has no 1.2 source and is
  kept only as the fallback for a database without rows. **Capture-pending DEATH-1:** the
  row-is-the-death-step mapping and the seconds unit are inferred from the table values (normal
  waits rising to the documented 180 s, siege waits falling to 0) and the client load; no 1.2
  server binary exists. **DEATH-2:** whether the revival debuff (`Weakened Body`) is also
  suppressed by `no_penalty` is unknown; only exp / durability are skipped, as the column name
  and the guard rows establish no more. **DEATH-3:** the first step's 0 s wait and the 600 s
  window replacing the C# 5-minute reset are data-driven, not captured.
* systems::skills (dungeon `pvp` / `has_graveyard`, 2026-11-07): both `indun_zones` columns were
  loaded but never read (the C# read neither). The server now mirrors the 1.2 client; the server
  rules themselves are inferred.
  * `pvp = 'f'`: `relation::can_attack` refuses a character-owned attacker (a player or a
    mate/slave whose `GetOwnerCharacter` is a player) against another character-owned unit when
    the target's zone group's `indun_zones` row has `pvp = false`. Evidence: the client builds
    its relation context with `flag = (record+0x19 == 0)` (`FUN_390281c0`, x2game.dll.c:39376,
    stored at relation+0x30), and the relation resolver `FUN_39838880` (~x2game.dll.c:1833560)
    returns kind 2 (not hostile) early when `FUN_39837a40` reports that flag for two valid
    combatants, before the faction / hostility branches. The client's kind-2 mapping is
    **INFERRED** to mean non-hostile. `relation_to` is unchanged (the client's buff/heal mapping
    is unproven) and NPC targets are not gated. `Dungeons.EnforceNoPvp` (default `true`)
    restores the faction-only decision. Real data: groups 49 (Mirage Isle), 62 / 64 (Immortal
    Isle) and 70-76 (Library) are `pvp = 'f'`. Tests:
    `relation::tests::{no_pvp_instance_blocks_character_and_owned_unit_attacks,
    pvp_instances_and_the_open_world_keep_faction_attacks,
    skill_casts_are_refused_in_a_no_pvp_instance, config_off_restores_the_faction_only_decision}`;
    gated `instances::tests::real_instances_data`.
  * `has_graveyard = 'f'` (only groups 49 and 68): `respawn_portal` uses the closest open-world
    return portal (`PortalManager.GetClosestReturnPortal`) and moves the character out of the
    instance (`SCLoadInstancePacket`), then the Temple Priestess, then the saved
    `MainWorldPosition` entry, then an in-place revive with a warning. Evidence for the split:
    `X2Player.IsInIndunWithGraveyard` (`FUN_39495b20` = current zone's record exists and
    `record+1 != 0`) selects `revive_indun` / "Revive at the entrance" (ui_texts 3843/3844) vs
    the ordinary `revive` between `has_graveyard = t/f`; the exact `'f'` destination is
    **INFERRED** (no capture). `Dungeons.NoGraveyardUseReturnPortal` (default `true`) restores
    the C# priestess-only path; `has_graveyard = 't'` keeps the C# behaviour. Tests:
    `resurrect::tests::{no_graveyard_instance_uses_the_return_portal,
    graveyard_instance_keeps_the_priestess_only_behaviour,
    no_graveyard_without_a_return_portal_uses_the_entry_position,
    config_off_keeps_the_priestess_behaviour_for_no_graveyard_instances}`.
* systems::skills: skill controllers and NPC displacements snap to the ground through the npcs
  heightmaps (`geo_data_height`); the wandering (fear) speed is the 7 m/s fallback (actor model
  stances not loaded); `Npc.StopMovement` writes the C# default stance / alertness; `OnMovement`
  is not raised.
* systems::skills: `BubbleEffect` does not sleep the effect thread for the bubble's read time;
  account updates of special effects run as async tasks.
* systems::skills: `saveFactions` KeyNotFound (C# throws) is logged as an error.
* systems::skills (surfaced by the `AAEmu.UnitTests` ports): a Rust `Skill` always has a
  template — the C# `new Skill()` / `new Skill(null, null)` (null template) is a skill of the
  default id-0 template; `Unit.Name` is a `String`, so the C# `null` name is the empty string;
  `ObjId` lives on the world handle (`Entity<D>::obj_id`) and is fixed at creation (no setter);
  buff / cooldown time checks take the clock as an explicit argument (unix ms) instead of
  reading `DateTime.UtcNow`; `local_impulse_to_world_displacement` takes horizontal (x, y)
  positions only, because the C# zeroes the direction's Z so the vertical offset never
  contributes.
* systems::npcs (2026-10-13): corpse timers read the current despawn field, so fully looting
  an NPC applies the existing shorter deadline instead of retaining the five-minute loot
  extension captured at death. Only living NPCs count towards spawner population limits;
  corpses remain available for loot/event cleanup without blocking the authored respawn
  delay. Native `Npc::OnDeath` (`x2game-dev_dedicate` `0x393452D0` -> `0x39345220` ->
  `0x39344AA0`) releases its population slot at death. This intentionally replaces the
  C# `HasCorpse` gate; spawn scheduling and corpse expiry remain separate timers.
* systems::npcs (2026-10-13): generated fallback spawners are kept for the entry that created
  them, so an NPC without authored spawner IDs retains its first placement; this intentionally
  fixes the mirrored C# omission that never spawned a generated spawner's first member.
  Duplicate filtering compares the authored position including Z (< 2 m on each coordinate),
  so identical entries collapse while cave / building floors stay distinct. Real main_world
  recount: 25 118 raw entries, 1 011 true duplicates, 0 missing templates, 24 047 ordinary
  retained placements, 38 174 ordinary spawner clones and 60 event entries (six placements /
  eight clones more than the earlier 38 166). Ordinary populations target the authored
  `Count` / `min_population` baseline (default 1), grow with nearby players and stop at
  `max_population` or a positive `suspend_spawn_count`; capped member selection uses the
  authored weights. Living populations exclude corpses and include the NPCs of alternate
  spawners sharing the caps; each pending respawn slot keeps its own authored deadline, so
  staggered deaths respawn independently. Back-end tests cover the limits and deadlines
  (including a 55 s authored respawn); a freshly built release server and a real 1.2 client
  entered the Bleakbreath Cave interior (GM teleports to the lower cave and upper passage,
  no spawn commands): screenshots show the Mistfiend and Wisp, and telemetry reported all
  six `npc_fogmonster` Mistfiend entities at cave heights below the terrain, plus a Mist
  Skeleton. This does not establish retail population density or validate every cave.
* systems::npcs (2026-10-14): scheduled NPC groups now honor their event window through the
  whole ordinary spawn lifecycle (Vile Gourdhead 14312 is the reference: its member rows
  produce AUTOCREATED 15942 and NORMAL 15948 with schedules 53/54/55). `HasScheduledSpawner`
  evaluates each candidate spawner's own schedule rows instead of looking every entry up
  through the clone's template, so the AUTOCREATED fallback cannot spawn the event NPC before,
  during or after the event (`NotStarted` / `InProgress` / `Ended` all suppress it). The
  despawn check was inverted for live events: `InProgress` retains the event NPCs and
  `NotStarted` / `Ended` schedule removal through the ordinary despawn / respawn tick, while
  ordinary no-schedule NPCs and authored game-clock windows behave as before. Dated event rows
  do not become recurring: the shipped October 16-30 2013 Halloween window stays expired.
  Regression tests: `npc/spawner_schedule_tests.rs`.
* `systems::npcs`: doodad func timers are tokio tasks (cancel = generation bump); `DoodadManager.Create`'s `Task.Run(InitDoodad)` is a spawned task; `TimeManager.OnTimeOfDayChange` runs from a 10 s poller. `GetNpcSpawner` template rewrites do not propagate to already loaded spawners; the "first" concurrent-dictionary entry is the lowest key; spawn files are processed in sorted name order (C# `Directory.GetFiles` order is unspecified). Portal book deletions are written at the next periodic save. Debug messages check only the character's access level. Coffer slot counting stops at 255 (C# loops forever). Where the C# would throw on a null / failed cast the Rust code returns quietly. Sticky parents are not modelled.
* systems::npcs / systems::services (GM `/despawn npc`, from PR #3): an NPC that still has a
  queued respawn or corpse timer keeps its object id until the spawn manager's timers are done
  with it (the C# released the id at once). The respawn still happens and the spawner's
  `_scheduledCount` credit stays backed by it; releasing early let a new object reuse the id
  while the timers were keyed by it. NPCs without timers are released immediately as before.
* systems::housing: dates are stored with second precision (unix seconds); `DateTime.MinValue`
  rows load as 0, so a manually inserted house with both dates at MinValue gets 1970-01-15 as
  protection end (C#: 0001-01-15; both are "expired"). `Connection.Houses` is not snapshotted:
  `SCLoginCharInfoHouse` / `SCMyHouse` read the account's houses when they are sent. A house row
  with an unknown template is skipped (C# NRE aborts the load). The non-persistent bound doodads
  of a deleted house are removed from the world's object table (the C# leaks them). Sale signs
  fall back to the house Z when no heightmaps are loaded. The house `Expedition` (owner's guild)
  is stored on ownership changes instead of being read live. Where the C# throws inside an
  operation (null decoration design / doodad template / taxation, missing furniture item), the
  operation is aborted and logged.

* systems::instances: the dungeon loader awaits the spawn tasks before registering the indun events again (C# does not await `Task.WhenAll`, a race) and reads the spawn files there instead of in `CreateWorldInstance`; `DestroyInstantGame` cleans the instance up and releases its id right away (C#: the finalizer, later); `Environment.TickCount` is a process-relative monotonic clock; the `World.Template.SpawnPosition` mutation in `MoveCharacterTo*` is a local value; paths that throw in C# (missing action / room / zone / rule set, `_members[x]` of a non-member, a destroyed dungeon's `World`) log and return, keeping the side effects done before the throw; `EndGame().Start()` throwing after a victory is modelled as "the rest of `OnKill` (respawn) does not run"; the cross-instance region switch removes the object from every old neighbour once (no double notification of its own sector).
* systems::instances (battlefield rules, 2026-10-19): `game_score_rules` is now loaded instead of the hard-coded 30-point kill. A kill uses event 200 (`event_value` 0) and an assist event 204; a corps-bound row wins over the `rule_set_corps = -1` row. Shipped rows: rule set 28 (Gladiator) event 200 -> 30 (id 93); rule sets 8/17/22/26/27 event 200 -> 3 and events 202/203/204 -> 2 (ids 26/53/68/73/76 and 81-92); rule set 2 authors no kill row, so its kills award 0. The assist rows do not distinguish healer / dealer in 1.2, so one 204 row is paid per assist (the three rows carry the same value). `game_rule_events` is loaded (rule set 28: 4 rows; 8/17/22/26/27: 1 each) but has no consumer with defined semantics in the 1.2 C#, in the 10.x zone decompile (loader only) or in the 1.2 client UI, so no behaviour is invented for `condition_id` / `kind_id` / `param1` / `param2`. `battle_field_buffs` is applied at match start (after the reset that clears buffs) with the row's `value` as the buff charge and removed on leave / disconnect: battlefield 7 has 8 rows (6266/100, 7006/1, 7007/2, 7061/3, 7062/4, 8246/1, 8247/1, 8000007/10) and battlefield 11 has 3 (8246/1, 8247/1, 8000007/1); the 3.0 successor `battle_field_honor_buffs.basic_value` repeats the same buff ids and values. Regression: `battlefield_kill_and_assist_scores_come_from_game_score_rules`, `battlefield_buffs_are_granted_at_start_and_removed_on_leave`, `battlefield_data_loads_with_json_quirks` and `real_instances_data`.
* systems::instances (battlefield resurrection delay, 2026-11-05): `game_rule_sets.time_resurrection_delay` replaces the C# hard-coded 6 s wait before `OnKill` resurrects the victim. The 1.2 rows author 15 s for every rule set and 5 s for the Gladiator (28); the unit is INFERRED as seconds (the client loads the column with the rest of the rule set, `LoadGameRuleSetDescs`, but its death countdown comes from `SCUnitDeathPacket.resurrect_wait_time`). A rule set with 0 / NULL keeps the 6 s fallback. Tested by `instances::flow_tests::battlefield_resurrection_waits_the_rule_set_delay`, `gladiator_resurrection_waits_five_seconds`, `battlefield_resurrection_without_delay_keeps_six_seconds` and `instances::tests::battlefield_rule_set_loads_kill_conditions`.
* systems::instances (instant game hardening, 2026-11-06): (1) `SCInviteToInstantGame.qualified_id` is now the game's zone instance (`zone_id << 32 | instance_id`) plus a per-game invitation counter instead of the constant `1`; `CSJoinInstantGame` and `CSEnteredInstantGameWorld` ignore a value that no longer matches the live invitation (a re-invite replaces it, a removal drops it, an expired invitation's timer only acts on its own value) and log at `debug`. What the client echoes is the invite value; no other meaning is evidenced. (2) `InstantGameManager.PlayerCanEnter` (present in the C# but never called) now gates `ApplyToBattlefield` with its existing checks (combat, instanced world, dead, tradepack, rebirth trauma, summoned vehicle) plus the C# TODO's states via the existing state: a duel (`DuelManager`), a seated juror (`JURY` buff 3621) and jail (`FORCIBLY_AWAITING_TRIAL` 3619, `PRISONER_NUIAN` 631, `PRISONER_HARANYAN` 2028); the refusal sends no packet because no 1.2 error id is evidenced. (3) The kill teleport restarts the `ZWUnitFell` fall tracker at the spawn z for the victim and (Gladiator) the killer (`FallTracker::reset_at`), so a fall from before the death cannot be measured across the respawn (`Prevent fall damage for both killer and victim on teleport`). (4) A player removed from a game unbinds its `OnKill` handler in `remove_player`, so every removal path (leave, decline, re-invite) and `DestroyInstantGame` leave no `CharacterEvents` subscription behind. (5) A game that never fills has no start expiry: only the 60 s invitation timeout exists, because no shipped rule set carries such a value (`game_rule_sets` has no such column), and none is invented. Tests: `instances::flow_tests::battlefield_stale_qualified_ids_are_ignored`, `battlefield_reinvite_replaces_the_qualified_id`, `battlefield_apply_refuses_a_duel_jury_or_jail`, `battlefield_kill_teleport_resets_fall_tracking`, `battlefield_end_unbinds_kill_events_without_an_origin_faction` and `systems::ai::collision::fall::tests::reset_at_restarts_a_fall_at_the_teleport_z`.
* systems::instances / systems::social (RPA-09 temporary factions): duel Red / Blue team and battlefield corps factions are temporary. `UnitBase::faction_temporary` marks them, `SCUnitFactionChangedPacket.temp` is `true`, and they skip `Character.SetFaction`'s housing update, guild kick and party removal. `db_character::persisted_faction` writes the origin faction, so an autosave, logout or crash during a duel / battlefield never stores the temporary faction; the faction is also restored on duel end, battlefield leave and departure/save (`world::session::save_and_remove_from_world`). A duel applies the same temporary faction to each participant's active mates and owned slaves and restores it with the owner's (they copy the owner's faction at spawn and are not covered by `Character.SetFaction`). Tested by `duel::tests::temporary_team_factions_keep_guild_and_restore`, `disconnect_during_a_duel_restores_the_origin_faction`, `mate_and_slave_follow_the_duel_faction_and_get_it_back`, `instances::flow_tests::battlefield_entered_requires_actual_instance_and_subscribes_kills_once` and `db_character::tests::a_temporary_faction_is_never_persisted`.
* systems::instances (tower defense): the C# command only sent packets to its caller and hard-coded zone group 5. The Rust runtime reserves controller positions from the existing NPC JSON by compact spawner membership; these controllers no longer spawn permanently. Manual controls affect the caller's instance/group. Automatic schedules validate every selected event at startup. Orphan waves/targets no longer truncate later valid rows. Kill notifications use only `DoDie`, deduplicate the NPC handle and retain id-reuse semantics. Timers use monotonic real seconds; `tod` uses the accelerated game clock and day intervals count from scheduler startup. Spawn effects carry explicit event ownership for cleanup even when Unit.OwnerId is zero. Unsupported `DoodadAlmighty` kill targets and spawner templates without authored positions remain unavailable rather than being synthesized (the rift waves, group sites and victory follow-ups are in the next entry).
* systems::instances (tower defense, rift waves; observed with player bots on Crimson Rift, definition 3): the waves never reached their target and most definitions could not run.
  * **AI settling cancelled the wave plot.** The rift summoners (`AiKind::Flytrap`, ai file 18) cast their wave plot as their `OnSpawn` skill (every 2 s, 25 tickets, `SpawnEffect` of the wave monsters). The AI walks `Spawning` -> `RunCommandSet` -> `Idle` -> `HoldPosition` within that time and the C# `Idle`/`HoldPosition` `Enter` call `InterruptSkills` (the Rust behaviors mirror them), which cancels the plot before the first tick. `ai/behaviors.rs` `interrupt_unless_event_controller` skips the interrupt for NPCs the running event owns (`tower_def::owns_npc`); every other NPC is interrupted as before.
  * **`SpawnEffect` at a plot location.** The plots place the wave monsters through a plot `Location` target (`PlotTargetUpdateMethod` RandomArea, 15 m ring, 500 m above the landing point). The C# spawner reads the target's transform; the Rust bridge used the caster as stand-in for a position target, so the monsters appeared on the summoner. `npcs/bridges.rs` `spawn_effect` now spawns at the position (`SpawnManager::get_npc_spawner_at_pos`), on the ground below it for non-flying templates (`ground_level_for`).
  * **Rift flight speed.** `Data/Path/razlomPPZden1-3.path` (the rifts' fly-in, ai 17) had no `Speed` action, so the path walker used the 1.0 m/s default and a rift needed 14+ minutes for its ~800 m although the first wave starts after 170 s (`tower_defs.first_wave_after`). The three files now start with `Speed|x|y|z|5` (same x/y/z as the first point): about 155-165 s, so the target reaches its stand before wave 1. The retail speed is not in the data; 5 m/s is derived from path length / `first_wave_after`.
  * **`NpcGroup` wave spawners** (definitions 8, 28, 29, 31, 32, and 14 whose group has no positions: `npc_spawner_npcs.member_type = 'NpcGroup'`). The C# `SpawnNpcGroup` is `SpawnNpc` (the group id used as NPC template id), so nothing spawned; the spawn JSON has no group entry either, it lists every member of the group (one entry each, duplicates included) at the group's spawn site, which would have made the members permanent world NPCs. The loader (`SpawnManager::resolve_tower_groups`, data in `TowerDefData.{groups,spawner_groups,group_npcs}`) holds those entries back, treats a position whose entries are exactly an `npc_group_members` set as a group site, and registers one event spawner per member at site + `formation_offset` (rotated by the site yaw). Entries that complete no group are released as ordinary spawns (none in the shipped data). Group 143 (spawner 14344, definition 14) has no entries.
  * **Victory follow-ups.** A definition whose `kill_npc_id` is a wave kill target of another definition (19 -> relic 13661, 20 -> relic 13647, both kill targets of wave 105 of the Golden Plains War 18; the other definitions use `kill_npc_id` to end their own event) is a follow-up: the death of that NPC in the running wave (kill count met) starts it in the same world, zone group and zone, the scheduler never starts it by clock, and the same NPC does not end it. Nothing else of a reward is synthesized (the C# had no tower defense runtime at all).
  * **Default open-world schedule (2026-11-09).** The shipped `Configurations/Dungeons.json` has no `TowerDefense` section, so neither the C# nor the old Rust runtime ever started the 1.2 open-world events. `Dungeons.TowerDefense.Enabled` now defaults to `true`; with no explicit `Events` list, `tower_def::default_events` derives the schedule from the data: every open `conflict_zones` row with a `war_tower_def_id` (group 22 Halcyona -> 18) and the victory follow-ups its waves can start (19, 20), plus every `tower_defs` row with `tod > 0` whose target controller positions stand in an open 1.2 zone (3/13 Cinderstone Moor, 5/15 Ynystere, 12 Hasla, 28/31 Lilyut Hills, 29/32 Tigerspine Mountains). Definitions in a closed zone (6 Sungold Fields, 8/27 Reedwind, 33 Mistmerrow is absent from `tower_defs`) and definitions without authored controller positions (14, 16, 17, 21, 23, 24, 25, 26) are not scheduled; the startup log lists the enabled events and warns about each skipped default. An explicit `Events` list still replaces the derived one and `Enabled: false` turns everything off. Evidence: the NA Sep 2014 - Jan 2015 videos show the Halcyona Golden Plains Battle (war banner, flag stage, relic stage, Haranya victory, 80-minute war; `D:/aaemu-audits/video-evidence/world-events-2014-15-ayin.md` section 1), the Ynystere Crimson Rift (section 2) and Grimghast Rift (section 3) running in the 1.2 era; the 1.2 patch notes list Oblivion Rift / Clockwork Rebellion. Tested by `tower_def_default_events_cover_the_1_2_open_world`, `tower_def_explicit_events_override_defaults_and_disable_empties_them` and `tower_def_default_halcyona_war_starts_and_stops_with_zone_22`; the gated `real_tower_def_event_readiness` pins the twelve default ids and validates each against the shipped compact and main_world spawn data.
  * **Still unavailable (no authored positions or content in the 1.2 data):** 4 (test dungeon: spawner 9880/NPC 8854 and wave NPCs 8842/8840/8841 have no position in any world file, a `DoodadAlmighty` kill target 3499), 14 (caravan escort: spawner 14460 and group 143 have no entries), 16 and 17 (fog-of-war notices: spawners 12271/12272), 21 (no target spawner, no waves), 23 (sun field test, spawner 15446), 24 (abyss entrance, spawner 15363), 25 (generic test spawner template 1), 26 (developer test, spawner 15687 and doodad 7297). Definition 33 (Mistmerrow's conflict) is absent from the compact. `/towerdef list` names the missing spawner for each.
* systems::instances / world (2026-11-04, destroyed instances): in C# the gimmick, sphere quest, mate and transfer managers, the fish schools and the physics were members of the `WorldInstance` and went away with it. The Rust keeps them in per-instance-id tables, and nothing dropped them when a dungeon was destroyed, so the 50 ms gimmick tick (and the 500 ms sphere tick) kept calling `GetWorld` for the gone instance: `GetWorld: No such World Instance 100` was logged about 17 times a second for 4 hours (160,374 lines) after Sharpwind Mines was destroyed. `ai::gimmick::npc_gimmick` also created the gimmick registry of whatever instance an NPC stood in, even one without gimmicks. **Deviation:** `instances::worlds::remove_world` now ends with `forget_instance`, which drops those tables (gimmicks, spheres, fish schools, mates, transfers, ship physics, collision world); the gimmick and sphere ticks drop a registry whose instance is gone instead of asking for it; `npc_gimmick` and `despawn` only read the registry. `WorldManager::world` (`GetWorld`) counts every miss per instance id (`missing_lookups`) and logs a warning for the first one only (it was an error per call); the counter restarts when the id is created again, and `world_if_exists` is the quiet lookup for periodic walks. Tested by `instances::flow_tests::a_destroyed_dungeon_leaves_nothing_that_asks_for_its_instance`, `a_recycled_instance_id_starts_without_the_old_instances_registries`, `a_lookup_of_a_missing_instance_is_counted_and_logged_once` and (gated by `AAEMU_COMPACT_DB`) `real_dungeon_spawns_load_and_leave_returns_to_main_world`.
* systems::ai (2026-11-04, instance hall floors): the NPCs of Kroloal Cradle stood about 191 m below their spawns, out of reach of a party (0 kills). The halls of the instance dungeons are brushes of the cell's `visareas.dat` (the collision loader read `brush.dat` and `vegetation.dat` only at the time; corrected 2026-11-08, the halls were never `object.dat` / `big_object.dat` models, see below), so the collision world had no floor there and `ground_height_probe` (2048 m deep) only finds the world template's terrain far below: the NPC spawns of Kroloal Cradle are 191-200 m above it, Howling Abyss up to 86 m, Sharpwind Mines (`instance_cuttingwind_deadmine`) 138 m, the Library 77 m, Sal Temple 50 m, Nachashgar 44 m, the Mirage Island 72 m (Burnt Castle Armory and Hadir Farm, which are terrain floors, stay within 4 m). `reference_height_in` accepted that hit for every walker step (roam, chase, return), so the first step of every walking monster dropped it to the terrain. **Deviation:** a walker's floor must now lie within `ai::npc::WALKER_MAX_DROP` (12 m) below its current level, for the probe and for the navmesh / heightmap fallback alike; a deeper floor is no floor, and the navmesh height (`GeoDataMode`, what the C# `GetReferenceHeight` returns) decides next, else the walker keeps its level. The hall floor model itself was not part of the collision world then (it is since 2026-11-08, see the `visareas.dat` entry below; this bound stays as a safety net): leaps, in-place resurrection (`GetResurrectablePosition`), doodad / plot / housing ground snaps and fall detection in these halls still use `ground_height` and can find the terrain; they are not changed here. Tested by `ai::npc::npc_height_tests::{hall_walker_does_not_drop_to_the_terrain_far_below, walker_steps_down_a_ledge_but_not_to_a_floor_beyond_the_drop_limit, hall_walker_takes_the_navmesh_height_when_the_terrain_is_far_below}` and (gated by `AAEMU_GAME_PAK`) `ai::collision::real_data_tests::real_instance_walkers_keep_the_floor_of_their_hall`, which resolves every NPC spawn of eight instance worlds against the real navmesh. **Also bounded (2026-11-08):** testers saw the Profane Temple end boss Akmit (12189, `instance_sal_temple`) knocked under the hall. The displaced / blinked / teleported NPC snaps used the unlimited `collision::ground_height`, which found the terrain (about 29 m below the boss at (615.5, 434.4, 148.8)). `skills::effects::special::movement::bounded_ground_height` now accepts that floor for knockback / impulse / physical explosion (`move_npc`, displaced) and `TeleportToUnit` only within `WALKER_MAX_DROP` below the start (the target for `TeleportToUnit`, players included) and not above the probe's step-up; otherwise the C# rule applies (`GeoData.GetHeight` when positive and within 5 m of the computed end Z, else the computed end Z), and the `FloatingSkillController` fall ground (`skills::controllers::ground_height`) ignores a height more than `WALKER_MAX_DROP` below its start Z (uses `_startZ`). `Blink` was already bounded by its sweep. Tested by `ai::npc::npc_height_tests::displaced::*` and (gated) `real_sal_temple_boss_knockback_stays_on_the_hall_floor`.
* systems::ai collision (2026-11-08, `visareas.dat` and cave voxels): the collision world now loads the two static sources it was missing. (1) `cells/X_Y/client/visareas.dat` (`cells::read_visareas`, strict octree walker per `COctreeNode::LoadCellObject`; any deviation or leftover byte logs one warning for the file and skips it): its brushes (type 1, model index into the file's own name table, no sector offset, position = matrix + cell origin) go through the same `brush_instance` path as `brush.dat`, its vegetation records (type 2, same `vegetation.xml` group ids) through `vegetation_instance`. These are the instance halls (Akmit's floor in `instance_sal_temple` at z 148.84, the Library, Cradle, Abyss, ...), not `object.dat` / `big_object.dat` as the 2026-11-04 note assumed. (2) Voxel objects (type 6; the caves of main_world, e.g. Dahuta) of `visareas.dat` and `object.dat` (`cells::read_object_dat_voxels`, same octree grammar; its brushes duplicate `brush.dat` and are not loaded): the first zlib block of each record is a CGF 0x745 whose `MeshPhysicsData` is a trimesh, loaded through `models::load_model` and placed by the record matrix + cell origin as a static brush-like instance (`ent_flags_or` 0x8000). **Deviations:** the engine's layer filter on visareas brushes is not resolvable from the decompile, so all are loaded; `object.dat` type-2 vegetation is loaded as well (see the 2026-11-08 `ObjectVegetation` entry below); the other record types (decals, water volumes, roads, ...) are walked and skipped by exact size. Config `Physics.VisAreas` / `Physics.Voxels` (default true) switch the parts off. Audit (gated `real_spawn_floor_audit`, NPC spawns with a `ground_height` floor within 2 m of their Z): 23 648 of 29 023 before, 26 980 after (main_world 23 280 to 23 620 of 25 299; sal_temple 0 to 112 of 126; cradle 0 to 74 of 77; howling_abyss 0 to 210 of 213 and `_2` 0 to 247 of 253; library 0 to 2 202 of 2 277; nachashgar 2 to 84 of 101; cuttingwind_deadmine 0 to 63 of 66). Tested by `cells::tests::{visareas_*, object_dat_voxels_*, voxel_blocks_inflate}` and gated `real_visareas_parse_every_file`, `real_visareas_and_voxel_floors`, `real_spawn_floor_audit`.
* systems::ai collision (2026-11-08, `object.dat` vegetation): the `object.dat` octree also holds type-2 vegetation records (same 64-byte layout as `visareas.dat`, cell-local positions) that are NOT in `vegetation.dat` (`cells::read_object_dat` now returns them next to the voxels; its brushes still duplicate `brush.dat` and are skipped). Gated audit `real_object_dat_vegetation_audit`: records / colliding model / same-group-same-0.1 m-position duplicates of the cell's `vegetation.dat`: main_world 82 956 / 48 023 / 0; instance_hadir_farm 630 / 105 / 0; arche_mall_world 876 / 734 / 0; instance_prologue 109 / 97 / 0; instance_training_camp 44 / 44 / 0; immortal_isle (and `_easy`) 49 / 6 / 0; login2 15 / 1 / 15 (a copy of its `vegetation.dat`); all other worlds 0. Loading them through `add_vegetation` adds 48 023 collision instances to main_world's 234 570 (+20.5%, `StaticInstance` 56 bytes each plus the engine entity, i.e. a few MB of instance data and one physics entity per tree). On by default: measured on a lane live stack (2026-11-08), it adds about 56 MB private bytes with every main_world cell preloaded (1 472 to 1 528 MB, +3.8%) and about 28 MB with the default lazy cell loading; startup time is unchanged. `Physics.ObjectVegetation: false` switches it off (it needs vegetation, i.e. `Geometry: Full`). Tested by `cells::tests::object_dat_returns_vegetation_and_skips_brushes`.
* systems::instances (2026-11-04, dungeon request refusals): the C# `RequestDungeonInstance`, `RequestSystemInstance` and `CreateSystemInstance` refused a request for an unknown zone, a zone without a world or a dungeon entry, and a dungeon the server could not build (no zone keys, no world template, `CreateWorldInstance` failing) without sending anything, and `DoodadFuncEnterInstance` dropped the use of a character in a court case the same way; the 1.2 client shows nothing then and the door looks dead. **Deviation:** each of these refusals now sends `SCErrorMsg` (`NO_SERVER_INSTANCE_RESOURCE` 253 for the data and resource cases, `CANNOT_USE_PORTAL_IN_TRIAL` 611 for the court case) and logs a warning. The entry rules already answered: the entry limit (`enter_count` entries per 4 hours) sends `INSTANCE_VISIT_LIMIT` 453 (checked against the 1.2 `ui_texts`), level 470, party 671, capacity 471, the ticket item 632; the notification bytes of 453 are pinned by a test. Tested by `instances::flow_tests::{an_entry_over_the_visit_limit_is_answered_with_the_visit_limit_error, every_refused_dungeon_request_is_answered}`; the court-case answer has no test (a trial cannot be set up from the instances tests).
* systems::instances (2026-11-04, silent dungeon door): a party member that died, respawned and cast the Kroloal Cradle entrance skill 18701 four times (and again 11 minutes later) produced a `StartSkill` line, never `DoodadFuncEnterInstance`, and no error. The live log of that session is not available here, so the exact trigger could not be reproduced; the silent paths of the entry flow that can produce it were found by reading the flow and are fixed, each with a test. (1) `request_dungeon_instance` step 1 returned `true` without a packet when the dungeon's character table listed the character. The C# calls `Dungeon.AddPlayer` there, which moves the member into the dungeon again ("Most likely an error in logic?"). A registration can outlive the move out of the dungeon (an earlier session of the same character that was not cleaned up, a transform that left the instance without the table following), and then the door never worked again for that character. (2) `Operation::enter` drops a use that meets another thread's reservation of the doodad (the C# `Doodad.Use` waited on its lock); a dropped use of an instance door gave no answer. (3) `DoodadFuncExitIndun` ignored the `false` of `request_leave_instance` (the character is in no instance's table), so the exit door of such a character was dead too. (4) `queue_player` of a dungeon destroyed in between returned `false` unannounced. **Deviation:** a table entry only counts when it is this very character standing in the instance (`rejoin_member`): an entry of another session or one the character's transform no longer backs is dropped and the character goes through a normal admission (no second entry is charged for a run it was admitted to), a character really inside is moved to the entrance again like the C# does (one event subscription per event); a busy instance door answers `TRY_LATER_INSTANCE`; an exit door for a character in no table returns it to the main world when its transform stands in an instance and answers `INVALID_RETURN_POS_INSTANCE` otherwise; a destroyed dungeon answers `TRY_LATER_INSTANCE`. A refused client cast of a doodad is logged at info with the result code (it was a debug line), so the next report shows the reason. Not changed: a cast the skill validation refuses (range, cooldown, a target in another instance) is still answered by the `SCSkillStarted` result code only, as before. Tested by `instances::flow_tests::{a_stale_dungeon_registration_does_not_swallow_the_door, a_member_inside_who_asks_for_the_door_is_moved_in_again_once, the_registration_of_an_earlier_session_does_not_block_the_new_one, an_exit_door_answers_a_character_no_instance_lists, a_busy_instance_door_tells_the_player_to_try_again}`.
* systems::instances (dungeons/mentoring): replaces the mirrored double entry-item charge and unsafe dungeon destruction below. Each character pays once per run; re-entry is free, capacity counts pending admissions, and creation obeys `World.MaxInstances` including concurrent requests. Empty owner/leader resets remove the whole run binding; destruction cancels its loader and returns occupants before removing their world. Party join grants access without inserting a player into another world's object table. Mentor eligibility is captured at boss death for nearby living party members with complementary active quests, rechecked during interactions, and bound to selected chest participants. Four explicit 1.2 quest/chest bindings are verified by a gated real-data test. Missing reward chests use the authored templates at the boss position (two metres apart for the two-chest dungeons); this placement fallback and qualification policy are emulator extensions, not claimed native-server execution parity. Authored phases grant seal items through existing inventory/quest hooks and daily persistence; Gilda is not issued directly by mentoring.
* systems::instances (2026-10-21, `CSInstanceLoaded` re-file): the reference 1.2 C# had the same gap - `LeaveDungeonInstance` / `LeaveSystemInstance` changed the transform and sent `SCLoadInstancePacket`, but `CSInstanceLoadedPacket` only sent `SCUnitStatePacket` and the time of day, so the client's emptied world stayed bare until the next move ran `Transform.FinalizeTransform` -> `AddVisibleObject`. Rust's `worlds::instance_loaded` now leaves the old instance's region grid (keeping the stale removal of its objects) and adds the character to the arrival instance's visibility grid, which sends the units / doodads / players in view and shows the character to them at once. A later C# commit (`ef0d41982`) fixed the same gap with `Transform.FinalizeTransform()` plus `ResendVisibleObjectsToCharacter(me, clientDroppedVisibility: true)`; that unconditional repaint could resend states, so Rust re-files through `add_visible_object`, which does nothing when the character is already filed in that region (a repeated `CSInstanceLoaded` or the next move sends nothing new). Tested by `instances::flow_tests::instance_return_refiles_character_and_sends_main_world_objects_without_movement`.
* systems::instances (racing packets, 2026-11-05): `SCRaceCond` 0x1f6, `SCRacing` 0x1f7 and `SCRacingResult` 0x1f8 are defined from the 1.2 client readers (`FUN_397b88d0`: u8 rc, u32 type, u32 after; `FUN_397b8950`: u8 rc, u32 record, i32 v1; `FUN_397b0340`: i32 count, then `min(count, 10)` x {u32 type, u32 record} through `FUN_397a8870`), byte-exact tested. Nothing sends them yet: no 1.2 CS racing packet or join/leave Lua call exists, `race_track_shapes` carries no coordinates, and the meaning of `after` / `v1` / `record` is INFERRED only, so the race manager (WP-R2) and rank board (WP-R3) stay capture-pending (CAP-RACE-1). `SCRacingResultPacket` truncates to the 10 rows the client reads and warns. The C# had only the three opcode constants, no packet classes. The types stay in the opcode-coverage never-sent baseline until a sender lands.

* systems::vehicles: supersedes the phase-2 `CSMoveUnit` and `LeaveWorldTask` bullets above:
  slave / vehicle / mate moves, riding, standing on objects, the mate / slave logout handling
  and the +10 minute logout delay with an active vehicle are ported (combat x30: skills).
* systems::vehicles: the transfer motor is not a transform child of its boarding part (the C#
  sets and immediately clears that parent); the boarding part is moved explicitly by the motor's
  tick, as the C# does for its packets. The transfer's own sticky link to its motor is not
  recorded (only `IsBoardedOnTransfer` reads sticky parents of transfers, and the C#
  `AttachedCharacters` list is never filled).
* systems::vehicles: `Transform.Parent = null` keeps the world position (`detach_child`); the C#
  keeps the local values until the next position update.
* systems::vehicles (2026-10-31, passenger seats): `BindSlave` takes the reach from its caller:
  the 12076 driver bind passes 4.0 m (`FUN_39346290` -> `FUN_39093860(..., 16.0)`, strictly
  under 4.0), the 13863 passenger interaction passes the passenger reach (`FUN_393bf270` ->
  `FUN_39093860(..., 9.0)`, strictly under 3.0, widened to 12.25 = 3.5 m by the target's
  `+0x3c26` flag), and the doodad BoardTransfer path passes `None` because its doodad func has
  its own use-range check. The gap is measured against the ship model's mass box when the slave
  has a ship model (the 2026-11-02 bullet below replaces the land-vehicle fallback with the
  model's collision bounds when the client assets are available), else the requested attach
  point's world position, else the smaller of the
  slave origin distance and the nearest located attach point of the model
  (`slave_attach_points*.json`); the client measures the minimum distance from the character's
  collision primitive to the target's collision parts (`FUN_39093860` -> `FUN_390b3ae0` for the
  parts, `FUN_3989bf90` -> `FUN_398b1ab0` for the per-shape distance), and the located attach
  points stand in for those parts. The origin bound is kept so the fallback never tightens a
  reach: the 2026-11-01 14:22 live log had the Red Farm Hauler (slave 146, model 1553, no ship
  model, point 2 declared by `model_bindings` but located only by its cannon points 9-14)
  refuse the passenger seat beside the rear hull (3.2-5.0 m from the origin, 1.9-2.6 m from
  the located points) and the 14:22:50 driver re-bind (4.9 m from the origin); tested by
  `passenger_beside_the_hull_is_in_reach_without_a_ship_model_or_a_located_seat` and
  `a_discarded_driver_rebinds_beside_the_hull_and_a_far_one_is_still_refused`. A model with no
  located point at all keeps the origin (carts: the origin sits at the seat); a ship's bare
  origin is never the measure (a merchant ship's seat / helm can be far from the origin). The
  3.5 m branch keys on the character's
  `is_riding`, but the decompile reads `+0x3c26` from the target (where it is also the dead
  flag), so this is capture-pending (S11-C5). A bind outside the reach answers `TOO_FAR_AWAY`
  0x9a. A character already in any slave's `attached_characters` is refused with
  `slave_unbind_first` 0x66 (`FUN_39346290`); a mate is not in that list, so S11-C5 stays open.
  The passenger seat is refused on a cannon (slave kind 7 / 8) with `slave_cannot_bind` 0x60.
  A taken seat answered silently in the C#; it now answers `slave_already_has_master` 0x61
  (S11-C2 capture-pending for the exact code), and 0x61 is also kept for the C# owners-mark
  driver refusal. A missing seat answers `slave_cannot_bind` 0x60. Every refusal uses the
  clearing SCErrorMsg form described in the 2026-11-01 bind-refusal bullet below. A character
  who dies is unbound from any slave seat through the same
  `unbind_slave` path as logout (the C# only did this for cannon gunners, so a ship driver /
  passenger kept a stale seat and the one-slave guard then refused every later bind).
  **Capture-pending (S11-C6):** whether 1.2 refuses an enemy / non-owner ship is unproven; the
  `+0x3c26` flag may be a faction / owner flag and no faction rule is implemented.
  `CSBindSlave` 0x031 has no attach
  point in the 1.2 packet (client `FUN_393464a0` writes only the target tl) and its only builder
  is 12076 "occupy", so `bind_slave_by_tl` keeps driver point 1; the passenger seat is skill
  13863 -> `AttachTo` (`special_effects` 2378 type 53 value1 2), which reaches point 2 now that
  `CSStartInteractionPacket` offers 13863 once the driver seat is taken (NPC interaction row).
  `CSDiscardSlave` 0x032 has no server-side speed check in the C#; the 1.2 client refuses to
  send it above 5 m/s with message 0x67 (`FUN_39345aa0`), so the server keeps none.

* systems::vehicles (2026-11-02, bind reach measured to the body): the 1.2 client's 12076
  driver bind and 13863 passenger interaction both measure to the vehicle's collision
  geometry, not to the model origin or the attach points (`FUN_39346290` ->
  `FUN_39093860(..., 16.0)` and `FUN_393bf270` -> `FUN_39093860(..., 9.0)`;
  `FUN_3989bf90` -> `FUN_398b1ab0` is the per-part shape distance). The live 2026-11-02
  01:43:54-58 log (original client, Red Farm Hauler slave 146 / model 1553) had `CSBindSlave`
  tl 0x56 refused with `TOO_FAR_AWAY` four times from beside the cab and bound only at
  01:44:17; the land-vehicle fallback measured the model origin and the rear cannon attach
  points. Land vehicles now measure to the union of the prefab body's collision proxies
  (`vehicle_models.normal`; the prefab's static `AnimObject` body is included). The 1.2 `.chr`
  body model carries no `MeshPhysicsData`; its same-named `.cgf` holds the body proxies.
  Ships keep the mass box and the 4.0 m / 3.0 m reach constants are unchanged. The union AABB
  over-approximates the target's collision parts: in a concave gap between parts the measured
  gap can be smaller than 1.2's shape-to-shape minimum (`FUN_3989bf90`), never larger for the
  same body, so the reach is never stricter than the client for the same geometry. A model
  without client geometry keeps the attach-point fallback. The Bevy client is being changed in
  parallel to measure its reach to the drawn body AABB, so client and server now both measure
  to the body. Tested by
  `land_vehicle_bind_measures_the_body_bounds_not_the_rear_attach_points` and the gated
  `real_hauler_bind_measures_the_client_body_collision_bounds`.

* systems::vehicles (2026-11-01, bind refusal replies): the 1.2 client sets a pending bind
  request (local actor `+0x10` = target tl) in the `CSBindSlave` builder (`FUN_393464a0`,
  decompile 678169-678180 / asm `0x393464d8`), refuses a repeat while it is set with
  `already_requested` 0x13 (`FUN_39346290`, decompile 678120-678125 / asm `0x39346383`), and
  clears it in `FUN_393457e0` for codes 0x48/0x60/0x61/0x66/0x9a (decompile 677413-677444);
  `OnSlaveBound` also clears it (`FUN_391d4760` -> `FUN_39345d00`, decompile 363755-363762).
  `HandleErrorMessage` (`FUN_391e5070`) only calls `FUN_393457e0` when the packet's `isNotify`
  byte is false (decompile 377162-377170); with it true it takes the notify branch
  (`FUN_391e4f00`) and returns first. The live 2026-11-01 14:22:50 bind refusal sent
  `isNotify = true`, so the client showed `already_requested` on every later F press and sent
  nothing until 14:40. Every bind refusal (dead, too far, cannon passenger, missing seat, taken
  seat, owners mark, already on a slave, missing/despawned target) now answers an `SCErrorMsg`
  with `isNotify = false` and a clearing code; the exact original reply for the formerly silent
  cases is capture-pending (S11-C2), but the clearing reply is required. The 14:22:50 refusal
  itself was the origin-only gap fixed by the passenger-seat bullet above (4.9 m from the
  origin, 1.5 m from the located rear points), not the discard: `CSDiscardSlave` cleared the
  seat (the second bind was not refused with `slave_unbind_first`). Tested byte-exactly by
  `passenger_skill_13863_seats_point_two_and_a_taken_seat_answers_a_clearing_error`,
  `a_taken_seat_answers_a_clearing_error_and_keeps_the_occupant`,
  `model_bindings_decide_which_seats_a_slave_offers_and_accepts`,
  `a_slave_outside_the_client_reach_is_refused_and_an_in_reach_one_is_seated`,
  `a_character_on_one_slave_cannot_bind_a_second_one`,
  `a_cannon_refuses_the_passenger_seat_skill` and
  `a_discarded_driver_rebinds_beside_the_hull_and_a_far_one_is_still_refused`.

* systems::vehicles (2026-11-01, seat bindings): the 1.2 client offers the passenger skill 13863
  only when the slave's model declares point 2: `FUN_393bbb80` scans the model's binding list
  (`FUN_390c9f70`) for the point. The server never loaded `model_bindings`, so it offered and
  accepted point 2 on every non-cannon slave. The Farm Wagon (slave 60, model 1008) declares only
  points 1, 35 and 9-12 (helm and cargo doodads), so a passenger rode at the model origin on a seat
  retail never had. `model_bindings` is now loaded in `systems/vehicles/data.rs` (model id ->
  declared points, row `owner_type='Model'`) and `bind_slave` refuses any non-driver point the
  slave's model lacks with a debug log and the clearing `slave_cannot_bind` 0x60; the C#
  accepted any point.
  The interaction list offers 13863 only when the model declares point 2. Driver point 1 keeps the
  C# rules, cannon kinds keep `slave_cannot_bind` 0x60, and declared cannon / turret points (for
  example point 9 on ship model 512) still bind. Tested by
  `model_bindings_decide_which_seats_a_slave_offers_and_accepts`,
  `real_model_bindings_pin_the_farm_wagon_seat` and the updated
  `real_cart_and_ship_slaves_offer_the_passenger_seat`.
* systems::vehicles (2026-10-29): a rider is seated with its local rotation zeroed as well as its
  position (`mates::reset_rider_transform`, in `MountMate` and in the riding re-attach of
  `CSMoveUnitPacket`). The C# `MountMate` only did `Local.SetPosition(0, 0, 0)`, so the heading the
  character had when it mounted stayed as its local rotation and was added to the mount's heading
  in `Transform.World`. Nothing in the C# read that heading while riding (it never checks
  `target_angle`), but the Rust `target_angle` check of `skill_outof_angle` uses the caster's world
  heading, so Charge / Overwhelm (`source_mount`, `target_angle` 180) from a mount were refused
  whenever the leftover angle put the rider more than 90 degrees off the mount. Dismounting was
  never affected (it uses the mount's world position and rotation).
* systems::vehicles: `Thread.Sleep(50)` between `SCMateSpawnedPacket` and the mate spawn is not
  reproduced. Slave weapon dps is 0 (mate / slave `UpdateGearBonuses` is ported).
  Ship physics, water and the storm cannon block are `systems::ai` (`ship_physics`).
* systems::vehicles: logout removes the active mate list by character id, matching its key;
  the C# used `OwnerObjId`, leaving deleted mates listed and potentially removing another
  character's list when the ids collided. Repeated stale-mate deletion still avoids releasing
  its obj / tl ids a second time.
* systems::vehicles: deviation from C#, which consumed `shipyards.origin_item_id` and ignored
  the packet's `designItem`. `CSCreateShipyard` now validates and consumes the design item the
  client names through `item_shipyards` (see the runtime row above), so a client cannot build a
  shipyard from a mismatched design item and shipyards 2-4 and 10 / 11 (origin 0) work. The
  `origin_item_id` fallback keeps shipyards without an `item_shipyards` row on the old path
  (shipyard 14's item maps to its duplicate 15; shipyard 1 has neither). `shipyard_rewards`
  debris on destruction is INFERRED
  (capture-pending); normal completion is unchanged.
* systems::vehicles: deviation from C#, which kept shipyards runtime-only (a restart lost every
  drydock and its build progress). They persist in the `shipyards` table
  (`systems/vehicles/shipyards_db.rs`): `System::save` (periodic and shutdown save) rewrites every
  live shipyard, `remove_shipyard` deletes the row at once and queues the delete for the next save
  as well, and `System::start` restores the rows with a fresh obj id (build step, actions, hp,
  position, owner, `spawned`; nothing is consumed) and runs the protection / decay update once.
  `ShipyardIdManager` now reserves the ids of the `shipyards` table. Decay keeps running from the
  stored `spawned` time, i.e. server downtime counts towards the 3 day tax protection.
* systems::vehicles: DB failures of the saved-slave lookups behave like "no saved slave"; the
  slave / mate saves outside `DoSave` are fire-and-forget tasks.
* systems::vehicles: a missing unit formula evaluates to 0 (C# NullReferenceException); index
  errors of `NextPoint` / `ShipyardSteps[-1]` are logged and end the call (C# exceptions).

* systems::services / skills / economy (RPA-02): the C# `LaborPower` setter wrote
  `accounts.labor` on every change; Rust records the change in `Character.labor_power_unsaved`
  and `save_character` adds it to `accounts.labor` in the save transaction (save acks subtract
  only the captured delta). Online regeneration adds to the cached balance instead of
  rewriting the account row from the database value. Offline labor is one atomic capped update.
  The tick resolves the character under the save transaction lock (serialized with
  `select_character`'s load and world registration). Before an offline write, it flushes a
  departing sibling's pending labor (`SaveManager::flush_account_labor_locked`).
* systems::economy (item persistence, 2026-10): socketing (success, failed roll and DawnStone), lunastone, temper
  and temper reset write plain `ItemDetails` fields; the C# setter-less fields did not mark
  `IsDirty`, so `ItemManager.Save` skipped them. Rust marks `Item.dirty` in those paths and the
  regressions assert the next prepared save snapshot contains the item.
* systems::economy (lunagem socketing rules, 2026-11): the C# `ItemSocketing.Execute` only
  checked the 7-element `GemIds` array. Rust also applies the rules the 1.2 client itself checks
  (`FUN_397ceb80` in the client decompile): the target's `items.LEVEL` must be >= 20
  (`content_configs` id 62, kind 24); a lunagem may only go into gear whose
  `items.level_requirement` is at least its `item_socket_level_limits.level`; the gem's
  `item_sockets.equip_slot_group_id` must map through `equip_slot_group_maps` to the target
  template's `EquipmentItemSlotType`; and the target may hold at most
  `item_socket_num_limits[slot type][grade]` gems. `item_socket_num_limits.slot_id` is the
  **equip slot type** (the client feeds the target's slot type straight into the lookup), not an
  equip slot group. The level check runs before the slot-group/limit checks. A missing or zero
  num-limit row means 0 sockets (`FUN_395aca00` returns 0) and the client reports 18
  (`invalid_target`), not 742; only an existing limit already reached is `item_sockets_full`. A
  missing level-limit row means no limit (`FUN_395aca70` returns `INT_MAX`), and the 15
  `item_sockets` rows whose item has no `items` row (test/removed items) are skipped at load.
  Rejections send the 1.2 error codes 18 (`invalid_target`, wrong slot group or no socket
  limit), 742 (`item_sockets_full`),
  743 (`item_sockets_empty`) or 781 (`socket_target_level`) and cancel the cast, which refunds
  the gem and skips the roll; the C# returned early without cancelling, so the
  `UseSkillAsReagent` gem was consumed even on a rejected attempt. DawnStone removal keeps its
  old behavior except for the client's `item_sockets_empty` rejection. Existing items with more
  gems than the current limit are left untouched at load, and regrade keeps their gems unchanged
  (as the C# did; the client only warns that a downgrade can lose them).
* systems::economy (grade enchanting, 2026-11): the C# `GradeEnchant.Execute` never read
  `items.grade_enchantable`, so it regraded any item. Rust loads the column into
  `ItemTemplate.grade_enchantable` and refuses a non-enchantable target with error 18
  (`invalid_target`) and `skill_cancelled` (the socketing refusal pattern), before the regrade
  roll, the gold debit or the charm take; cancelling refunds the scroll, which the cast debits
  as its `UseSkillAsReagent` source item. This follows the 1.2 client's own check: its
  `GradeEnchant` special-effect validation (`FUN_393a0130`, case 0x5c) returns SkillResult
  `INVALID_TARGET` (0x22) when the target template's `grade_enchantable` is false. Only
  `GradeEnchant` uses the flag; 1.2 has no awakening/evolving upgrade path. Real data: 4 564
  `'t'` rows (all equipment) and 16 918 `'f'` rows. The rest of that client target check is
  ported too: the target's `level_requirement` below the scroll's `special_effects.value2`
  answers `LEVEL_LOW_TO_USE` (518, client sub-code 0x206), an item class other than
  `value3` (1 weapon, 2 armor, 24 accessory; -1 = any) answers `INVALID_TARGET` (18), and a
  grade-11 target answers `GRADE_ENCHANT_MAX` (665, client sub-code 0x299). Every refusal
  (including a locked target, missing money, an unknown template, the charm grade windows
  and the invalid gold-cost kind) now sets `skill_cancelled`, so the cast refunds its
  debited scroll; before, the scroll was lost. The cast-path tests
  (`grade_enchant_cast_refunds_the_scroll_*`) drive `CSStartSkillPacket` and assert the
  scroll count.
* systems::economy / skills (renew and physical enchant, 2026-11, stage 1): the C#
  `RenewEquipment` (57), `PhysicalEnchantWeapon` (20) and `PhysicalEnchantArmor` (21) actions only
  logged. **Renew** ports the 1.2 client's pair predicate `FUN_397cca90` (`X2RenewItem.Renew` ->
  `FUN_394e92d0`): armor needs a `base_equipment` source and a non-`base_equipment` target with
  the same armor-type record; a weapon needs the same holdable slot type and renew category, with
  the renew category in 1..3. The client's toast ids are sent for the same refusals (20 slot out
  of range, 27 item missing, 206 not a bag item, 1 not a renewable pair).
  **Renew is not reachable in the 1.2 client:** `x2ui/renew_item.alb` and
  `renew_item_view.alb` are 60-byte empty chunks (header plus one RETURN) in the r208022 pak, so
  no player can open the window that calls `X2RenewItem.Renew`; the texts and the tooltip hook
  exist but the dialog is not shipped (`round6-items-mail.md` 1a). Stage 2 and
  `IT2-RENEW-SLOT` are therefore moot for real clients.
  **Physical enchant is a single-item cast and does not use that pair predicate.** Skills 10521 /
  10522 (`기본강화-무기` / `기본강화-방어구`) target type 9 (Item) and are common skills
  (auto-learn, no learn requirement, ability 0), so the client sends a plain `CSStartSkill`
  (caster = the player, target = one `SkillCastItemTarget`); the NPC dialog texts
  (ui_texts 1428-1463) describe one item with money + archium and a 1x / 2x / 4x archium choice.
  The enchanted item is the target's item id, looked up in the bag; `SkillCastItemTarget`'s
  `obj_id` is the item owner (`FUN_394e92d0`), not a slot. The effect applies from
  `application_method_id 2` (Source), so the effect target is the caster; the standard
  `target.ObjId == targetCaster.ObjId ? targetCaster : SkillCastUnitTarget` rewrite (the C#
  `ApplyEffects`, mirrored in Rust) therefore keeps the item target because the owner id is the
  caster's. Refusals use `ItemCannotUse` (24, invalid item / wrong arm), `NotReady` (206, NPC
  ability ladder) and `NotEnoughFriendship` (28); missing items use `NotEnoughItem` (27). The
  enchanter's `physical_enchant_abilities` row (npc template id, armor flag, next enchant step)
  is checked: `min_friendship > 0` is always unmet because 1.2 grants no NPC friendship
  (`track_friendship` is false for every NPC), and the row's `success_ratio` is rolled. The next
  enchant step is INFERRED as `temper_physical - 100 + 1` (`IT2-ENCHANT-LEVEL`). Stage 1 never
  changes an item: what a success consumes / transforms is capture-blocked (`IT2-STAGE2`); the
  outcome is logged. The money / archium formula, the multiplier effect, the great-success chance
  and the "enchant durability" storage are capture-blocked (`IT2-COST`, log-only, never refused
  on a guessed cost). The enchanter NPC is the cast's NPC operand when it has one, else the
  character's current interaction object (`CSInteractNPC`), because the 1.2 cast carries no NPC.
  **HARD RULE:** stage 2 of physical enchant (any change to the item: grade, durability,
  destruction) must not land until the `IT2-COST` rule (money + archium, the 1x / 2x / 4x
  multiplier) is settled and enforced; otherwise it is a free enchant.
  **OPEN (`IT2-RENEW-SLOT`):** `item_target_slot` (`systems/skills/effects/special/items.rs:120`)
  treats `SkillCastItemTarget.obj_id` as a bag slot, while the enchant evidence says `obj_id` is
  the owner's id. If that is right, real renew casts fail the slot range check. The renew
  window is not shipped (above), so no real cast reaches this; the item is moot unless a
  modified client sends it, and renew is left unchanged.
* systems::economy / npcs / skills (lunafrost, 2026-10): the C# `InteractionEffect` passed the
  `SkillItem` caster and `SkillCastItemTarget` to `MagicalEnchant.Execute`; the Rust bridge dropped
  them, so a lunafrost never applied. They are forwarded now. `MagicalEnchant` also looks the item up
  in every item container (like `ItemSocketing`), not only `Bag.GetItemByItemId`, so a worn item can
  be enchanted. `Unit.UpdateGearBonuses` (Rust `gear_bonuses`) now includes the unit modifiers of the
  item's `rune_id` (the C# only counted the item and its gems), and socketing / enchanting a worn item
  recomputes the gear bonuses immediately.
* systems::services: the WebApi 500 page shows the Rust error chain instead of a .NET stack
  trace, and System.Text.Json / NotSupportedException messages are close paraphrases. Character
  `CreatedAt` of characters created in the running session is written without the `Z` of a
  `DateTimeKind.Utc` value. The auction controllers serialise the lot's `Item` with its stored fields only
  (Id, TemplateId, Count, Grade, OwnerId, SlotType, Slot).
* systems::services (WebApi tooling, 2026-11-06): the C# has no `GET /online` or `GET /dominions`.
  They are additive Rust-only read-only routes behind the existing Bearer auth (WP-SL7):
  `OnlineController` lists the world's online characters by id (id, name, level, race,
  faction id/name, expedition id/name, zone id; no account data, addresses or positions;
  `is_online` follows the session's `Character.IsOnline` flag), and `DominionController`
  reports each persisted dominion zone group (name, owner expedition id/name or null, 1.2
  siege period id, the next siege window from an unfinished explicit siege record or the
  declared calendar cycle, and the declaration attacker). GM-seeded ephemeral dominions are
  not reported.
* systems::services (WebApi security, 2026-10-25): unlike C#, the HTTP listener defaults to
  loopback and rejects non-loopback binds without a configured `WebApiNetwork.ApiKey`.
  Every route except read-only `GET /status` requires a Bearer credential, compared using
  `subtle::ConstantTimeEq`; an absent or empty key grants no administrative access, including
  on localhost. `Enabled: false` disables the listener. Keys and bind settings are snapshotted
  at listener start; changes require restart. Existing tools must add the Authorization header.
* systems::services: `TaskManager.GetQueueCount` (WebApi `/status`) only counts the tasks of
  this queue; the other subsystems run their timers as Tokio tasks.
* systems::services: `TimedRewardsManager.DoTick` skips connections without an account (C#
  `GetAccountDetails(0)` inserts an `accounts` row 0). The payment comes from
  `AccountManager::state` (WP-B), not a default. `AccountState` lives in the account registry
  because the tick only has a `ConnectionHandle`; a lazy expiry check at the start of the labour
  tick deactivates an expired premium account (`deactivate_point`, `SCAccountInfo` +
  `SCUpdatePremiumPoint` + grade-buff removal).
* systems::services: `AppConfiguration.Instance` mutations (World set commands,
  `ReloadConfiguration`) replace the `ConfigCell` snapshot; subsystems that copied values at
  start (skills config, doodad growth rate) are updated by the World set commands only.
* systems::services: `SusManager` analysis runs from the move handler's 5 s
  `FinalizeTransform`; every `LogActivity` call (social, coffer, trial, `CSDestroyItem`) inserts
  its row as a task because the C# sites are synchronous. `CSDestroyItem` with `count <= 0`
  reports `CategoryCheating` with the C# text and then destroys the whole stack
  (`systems/economy/item_handlers.rs`; `porting-md#10`).
* systems::services (commands): a command whose C# body is synchronous but calls an async Rust API
  (saves, reloads, DB, mails) runs through `execute_async`; `ReloadConfigs` replaces the whole
  configuration (C# `Bind` keeps runtime values of keys absent from the files) and refreshes the
  copies the subsystems keep; `Announce` only knows the web color names (no system colors);
  `DisconnectMe` uses `ConnCommand::DetachAndClose`; `scripts reloadslavepoints` keeps the
  reloaded table in `vehicles::System` (game data is immutable); `ChangeLevel` help shows the
  level caps until the experience table is reachable; the `Fly` cache is per command instance;
  `Kit` sorts kit names case-insensitively (culture sort approximation); `ShowInventory`
  "Container Error" compares with the listed container; `CrimeFakeTrial` awaits `ReportCrime`;
  `QuestCmd` truncates acceptor type / status to bytes; the sphere list uses the global sphere
  manager; `Help` sorts ordinally (same order as the culture sort for lower-case names and `_`).
  The harness gives players fixed obj ids (`0x100 + id`) that can collide with ids from
  `server.ids.object` in tests that spawn many objects.
* systems::services: the Discord bot runs in the background (the C# `StartAsync` never returns
  when a token is set, blocking the host start).
* systems::skills (once-only death transition, 2026-10-21): the C# `ReduceCurrentHp` checked
  `Hp <= 0`, applied the damage and called `DoDie` without a lock, so two concurrent lethal
  hits (a skill hit and a DoT tick, fall / drowning damage, `GM /kill`, PvP, an NPC death) could
  both run the death path: two `SCUnitDeathPacket`s, double exp / durability loss, two loot
  generations, double quest kill credit and two respawn timers. Rust adds the Rust-only
  `UnitBase::death_processed` claim, checked and set atomically under the unit's data lock
  (`skills::combat::try_claim_death`); `ReduceCurrentHp` re-checks HP under the same lock that
  applies the damage, `PostUpdateCurrentHp` claims before its kill events, and a direct `DoDie`
  on an already-claimed life is refused with a WARN. The claim is cleared when a living unit is
  damaged again or HP is restored (resurrection, repair, load), so a repaired tower or a
  resurrected character still dies once per life. Tests:
  `systems::skills::death_once_tests` (two racing hits on a player / an NPC, looped).
* systems::skills (fall damage, 2026-10-30): the C# `Unit.DoFallDamage` deals
  `MaxHp * (fallVel - 8600) / 15000`, which is negative for every client `fallVel` from 1 to
  8599, and `ReduceCurrentHp` applied `Hp = Max(Hp - value, 0)` to it (`Hp` is a plain property,
  no `MaxHp` cap). So a small jump landing HEALED the unit, past `MaxHp`: in water, without a
  physics world and with `FallDamageZoneCap` off the client value reaches `DoFallDamage`
  unchanged (a bot was healed 237 hp by a jump landing in water). The C# carries
  `TODO: Maybe adjust formula & need to detect water landing?`. The damage after `FallDamageMul`
  could also go negative: the unit modifiers of that attribute (`unit_modifiers`, attribute 92)
  are 8 buffs at -10 to -25 % and 3 items at -20 / -10 / +50 %, and they stack (the six -22 / -25
  buffs alone add up to -135 %), which healed on every fall. Rust:
  `do_fall_damage` deals no damage (and sends no `SCEnvDamagePacket`) up to
  `pl_fallDamage_SpeedSafe` (20 m/s, `collision::fall::SPEED_SAFE`, the threshold of the zone's
  own fall check; the client unit is the inferred 32000 / 63.25 per m/s, so `fallVel <= 10118`
  is safe and 10119 hurts), clamps the damage after the multiplier at 0 (a fall cancelled to 0 neither stuns nor sends a packet), and keeps the C# curve
  above the gate (`fallVel >= 32000` still kills, the 5 % floor and the stun are unchanged).
  `reduce_current_hp` treats a negative damage value as 0 (a WARN; every caller passes a
  positive value, heals use `effects::combat::heal`) and keeps the result in `[0, max_hp]` (a
  unit whose `max_hp` is 0 is not capped). Water: neither the C# nor the data has a water rule
  (the C# only carries the TODO above); the 1.2 zone's tracker skips swimming landings but its
  movement path forwards a client fall velocity above 20 m/s, so water gets the same gate as
  land and no deep-water exemption is implemented. **Needs capture** (real client in the water):
  the `fallVel` and stance it sends when diving from a height. **Open:** the 10.x wire scale
  is `u16 / 65535 * 128` m/s (`D:/aa-re/zone/agent_proto/zone_protocol.md`, 512 per m/s,
  safe up to 10239) against the inferred 505.9 used here (also by the zone cap), unverified for
  1.2. Fall damage immunity (2026-11-05): 185 `buffs` rows carry `fall_damage_immune` (human-
  cannon shields, ghost form, float-up skills, the "immune to fall damage" buff, e.g. 1649),
  loaded into `BuffTemplate` and unused by the C#. The client's own `LoadBuffDescs` selects the
  column and reads it as a boolean
  (dll.c:1579862, 1579986), but the consumer of that field was not traced, so the effect (a
  whole fall negated) is INFERRED from the column name and the rows. Rust's `do_fall_damage`
  now returns 0 while the unit has a live buff whose full template has the flag: no HP loss, no
  `FALL_STUN`, no `SCEnvDamagePacket`; dispelling the buff restores the damage on the next fall.
  The client's per-controller `CSCreateSkillController` flag stays logged only and is not
  trusted. Tests: `systems::skills::fall_damage_tests`, `collision::fall::tests`,
  `collision::flow_tests::a_landing_in_water_forwards_the_client_value_and_never_heals`.
* systems::skills (damage immunity, 2026-11-05): the C# `Buffs.CheckDamageImmune` treated every
  buff with a matching melee / spell / ranged / siege flag as total immunity and never read
  `immune_damage`. The 1.2 data has 229 flagged buffs with `immune_damage = 0` (full immunity,
  e.g. 131 Invincible) and six flagged buffs with a positive value — 275 (50), 276 (100), 176
  (137), 2071 (2000), 3615 (1 000 000), 4609 (9 999 999) — whose descriptions say "ignore hits of
  N or less". Rust keeps `check_damage_immune` for the zero-threshold buffs and adds
  `damage_immune_threshold`: `damage_with_aoe` computes the final post-mitigation hit, and when
  it is `<=` the best matching buff's threshold it sends `SCUnitDamaged` hit type `Immune` and
  applies no HP loss, reflection, threat or steal. Buff 3642 (a threshold without a type flag) is
  left unchanged because no flag selects a damage type. The client loads the column
  (`LoadBuffDescs`, dll.c:1579862, read at `dll.c:1580052`), but its consumer is not traced
  further; the threshold meaning comes from the six descriptions. Tests:
  `systems::skills::effects::damage_pipeline_tests` (below / at / above 50, a melee-only buff
  against magic, and a zero-threshold flag against a 1 000 000 hit).
* systems::ai: calls into an NPC's AI from outside its tick (skills, aggro, npcs hooks) are queued and run right after the current AI step instead of re-entering it (`manager::post`); the AI tick runs on a blocking thread every ~100 ms; behaviour instances are kept per AI and accessed by kind. `UpdateAggroHelp` reaching a helper NPC without an AI logs and continues (the C# throws a `NullReferenceException` that cuts the whole behaviour tick short). `Npc.OnDamageReceived` on an AI-less NPC does nothing (C# NRE).
* systems::ai: deletion marks an AI removed without waiting for its state mutex; the current holder detaches its owner and discards queued callbacks. This permits an NPC skill to delete its own caster and avoids the spawn-lock / AI-lock inversion during despawn. The global tick skips an AI whose state mutex is busy and retries on the next tick, so one external AI call cannot freeze movement and combat for every NPC.
* systems::ai physics: Jitter's narrow phase (`MprEpa`) and contact solver are replaced by a box-vs-heightmap-triangle SAT with one contact per triangle and sequential impulses (Jitter's default friction / bias); the integration itself is exact. One physics world per instance is created with the first ship (C# starts a thread per instance at boot). Add / remove / helm / driver changes go through the pending queue (next tick); hull damage, movement packets and transform writes are applied after the tick; readers use per-tick snapshots. Water is kept per world template and a cell's `object.dat` is ingested when the physics or a water query sees the cell loaded. A height lookup error counts as height 0 (the C# exception skips the rest of that ship's tick). A ship whose game object is gone is skipped. Ship–ship harpoon tow resolves an explicit hook basis among the ships simulated this tick.
* `WorldManager.Initialize` (`ActiveRegionTick`, 1 s, `useAsync`) and `IndunManager.Initialize`
  (`IndunInfoTick`, 30 s, `useAsync`) do not subscribe to `TickManager.OnTick`: they run as
  non-overlapping Tokio interval tasks started once per server
  (`npcs::spawn_manager::start_active_region_tick`, `instances::start_ticks`); the spawner
  interval is `World.NpcSpawnTickMs` (default 250 ms, see the systems::npcs entry below) and the
  indun interval stays 30 s.
  The C# `WorldManager` query API is split by data owner: template / zone-key lookups are on
  `game_data.worlds` (`WorldTemplates`), `GetAreaShapeById` / `GetWorldInteractionGroup` on
  `game_data.systems.npcs.area_trigger`, the rest on `world::WorldManager` (`GetTargetOrSelf` is
  `target_or_self`, `None` = the C# `null` name). Unit tests: `world/world_manager_tests.rs`.
* systems::npcs: `Doodad.PlantTime` is unix seconds and the guard-time checks (`IsProtectedByPublicFarm`, `IsPublicPropertyByAge`) take `now` in unix milliseconds, so their deadlines resolve to the millisecond instead of the 100 ns `DateTime` tick (the comparisons themselves are the C# ones).
* systems::ai: flying NPCs (`CanFly`) keep the interpolated 3D movement step of `MoveTowards` instead of being pinned to the spawner's Z, and skill min / max range is a 3D check for them (ground NPCs keep the 2D range; the melee skill-2 four-meter guard and the skill selection use the same 3D distance), so a flyer cannot cast at a victim far below before closing in; `ReturnState` completes on the 3D distance to the idle position (arrival at `MoveTowards`'s 1 m tolerance; the timeout teleport counts vertical displacement), so a flyer that fought below its spawn altitude climbs back to it instead of stopping at the home XY.
* systems::ai (2026-10-13): walking floor queries (`reference_height_in`) start at the walker's
  current level and accept only a collision floor within the 2 m upward allowance; a valid
  Z = 0 floor is accepted, terrain above a cave roof is not, and the current height is kept
  when no reachable floor exists. Flying / holding / idle NPCs keep the documented height
  behavior. Real 1.2 data: the six Mistfiend (NPC 4227) points in Bleakbreath Cave have
  terrain 91–145 m above the authored positions, no downward probe reaches a floor and the
  corrected walking height stays at the authored cavity level (556.25–593 m).

* systems::npcs (2026-10-14): the inherited main_world portal placements (entrance doodads,
  template 4894) are reconciled with the original 1.2 `map_data/doodad_map/*.dat` marker
  positions: ten extra records removed, two positions corrected and five missing records
  restored (109 placements, one-to-one marker matches within 0.03 m). Authored rotations are
  retained; restored records use the default zero rotation because the markers carry only
  positions.

* systems::npcs (2026-10-14): a direct `Npc.Delete` (`manager::delete`) reconciles the exact
  `NpcRef` out of the shared `SpawnedNpcs` membership before the world removal, so a deleted
  object no longer counts against the population caps and blocks its replacement (the C#
  `Npc.Delete` detaches only the AI and leaves the stale entry). The reconciliation takes the
  spawners' reentrant spawn lock like every spawner entry point, so an activation tick cannot
  interleave with the membership change. Removal matches the whole `Arc` identity: a recycled
  object id in any instance, or an identical member of another placement, is never touched, and
  deleting an already removed NPC stays idempotent. `despawn_with_respawn` keeps the delete, its
  reserved respawn slot and the population credit as one step. Authored corpse lifetimes and
  each pending slot's individual respawn deadline are preserved. Focused tests in
  `spawner_population_tests.rs` cover the limits, deadlines and direct-delete reconciliation.

* systems::npcs (2026-10-16): a corpse / queued despawn timer that fires after its NPC was
  already deleted directly (and so reconciled out of `SpawnedNpcs`, see above) is an idempotent
  no-op: `NpcSpawner::despawn` logs the missing entry at debug instead of the C# warning
  `NPC ... not found in SpawnedNpcs` when the object has already left the world; the warning stays
  for an NPC that is still a live world object but missing from its list. A stale handle whose
  object id was released and reused by another NPC (GM `despawn` of a corpse with pending
  timers) is skipped entirely, and `check_respawns_tick` no longer releases the id / spawner link
  of the NPC that reused it (the C# `Despawn` and its caller act on the id regardless).
  Tests in `spawner_population_tests.rs`.

* systems::npcs (2026-10-14): the active-region spawner tick is a bounded activation pass
  instead of the per-second sweep that updated every spawner whose authored player radius was
  entered. Ordinary spawners are indexed per instance and authored 64 m region and queried
  around present players from cached candidate queues, rebuilt only when the instance's
  player-activity revision, the spawner membership or the prewarm margin changed. The client
  visibility radius (2 regions) is unchanged; `World.NpcPrewarmRegionMargin` (default `1`,
  clamped to `0..=4`) adds that many rings beyond it as deliberate prewarming (default 7x7
  regions around a player), and `World.NpcSpawnTickMs` (default `250`, `0` -> `250`, clamped to
  50..=60 000 ms) is the pass interval. Placements whose authored 3D `TestRadiusPc` /
  `TestRadiusNpc` (x 50) reaches beyond the rings keep their full reach through a grouped coarse
  index (radius-sized cells; the nine cells around a player are probed, then the exact 3D
  sphere), so prewarming never narrows an authored trigger. `World.NpcSpawnsPerTick` (default
  `64`, `0` -> `1`) is the global budget of NPCs actually created per pass: a pass fills
  client-visible neighbourhoods before prewarm candidates, nearest first; each queue continues
  from its cursor and the starting instance rotates, so a small budget cannot starve later
  candidates. The player-activity revision is published on player arrival, movement (within one
  region included) and departure, so proximity caches follow immediately while an unchanged pass
  reuses the cached queues (the full spawner table is not walked per pass); a membership change
  also clears the count cache anchored to a spawner's first NPC. A player of another instance at
  the same coordinates never activates a spawner, and a placement whose member list is still
  empty stays queued and is retried once it is initialized, without movement or a reload.

* systems::npcs / systems::services (2026-11-08, stacked NPCs; Rust only, the server owner's
  request: glitched NPCs stacked on the test server). The spawn data places all of a spawner's NPCs
  (`Count` / `min_population`, e.g. 12 Death Hunters), and several entries (different templates of
  one spawner group), on identical coordinates, and idle NPCs never move, so they stay stacked.
  (1) A spawn (`NpcSpawner::spawn_npc_template`) that would land within `World.NpcSpawnScatterTrigger`
  (default 1.0 m, 3D) of another live, unparented NPC of the instance is scattered onto a ring of
  `World.NpcSpawnScatterMin` .. `Max` (defaults 1.5 .. 3.0 m) at an evenly spread, randomly started
  angle, ground-snapped like a spawn (`spawn_ground_z`, at most 1.5 m height change; no resolvable
  ground = no scatter). The AI home / idle position is the scattered spot (it is read from the
  transform in `spawn_positioned`), the spawner position is unchanged. `NpcSpawnScatterMax = 0`
  disables it. Not scattered: `NpcGroup` formation members (authored offsets), spawns with an
  owner (summons, quest spawns), flying and underwater NPCs, and parented NPCs.
  (2) `systems::npcs::stacked_npcs` logs one `WARN` per stack (alive, unparented NPCs without an
  aggro target within `World.StackedNpcRadius`, default 0.5 m) that persisted
  `World.StackedNpcMinSeconds` (120); the pass runs every `World.StackedNpcCheckSeconds` (60, 0 =
  off); a stack is logged again when its member set changes or after an hour.
  (3) GM `/stackednpcs [radius|all]` (alias `/npcstacks`, access level 50) lists the stacks
  (obj id, template, spawner id + template, position); `/stackednpcs fix [radius|all]` keeps the
  first NPC of each stack and moves the others with the scatter logic (default ring when scattering
  is disabled), sets their AI home position and broadcasts `SCOneUnitMovementPacket` (stop move)
  at the new position.

* systems::npcs (2026-10-14): prepared ordinary placements are kept in the warmed registry after
  their warm set loses its last player: the existing NPCs keep their state for
  `World.NpcWarmRetentionSeconds` (default `60`; `0` permits reclaiming on the first eligible
  pass) and a return clears the stable countdown. Expired placements are reclaimed per live NPC
  under its own bounded per-pass budget (`World.NpcSpawnsPerTick` again), completing the removal
  in the pass instead of a queued despawn (which could delete a re-shown NPC), keeping the
  population reservation credit and leaving corpses / pending corpse / respawn deadlines to
  their own timers; only the warmed registry is swept. Nonaggressive ordinary quest givers /
  reporters (static `ConAcceptNpc` / `ConAcceptNpcEmotion` / `ConReportNpc` acts) and service
  roles (merchant, banker, skill trainer, auctioneer, priest, blacksmith, teleporter, ability
  changer, stabler, trader, repairman, specialty, expedition, look converter; `npc/residency.rs`)
  stay resident after their first preparation while `World.NpcResidentServiceNpcs` (default
  `true`) is enabled; a mixed placement decides per live NPC, and runtime-owned (`Unit.OwnerId
  != 0`) or creator-only actors are never residents. Authored schedule / management despawns
  keep priority. A cast / plot / skill controller, persistent or `ActivateAiAlways` AI work
  (including the same finite work: an active command after its queue drained, spawn settling,
  return home) and actual combat against a live opponent inside the authored return leash
  protect an NPC from reclamation.

* systems::ai (2026-10-14): the 100 ms tick does not visit every registered AI (the C#
  `AIManager.Tick` walks the whole `_npcAis` list). Candidates are the NPCs of the 2-region
  neighborhoods around active players (their region plus two rings) plus the explicitly
  maintained sets: persistent paths / follow targets / queued commands and other finite work
  still to finish (`NpcAi::has_persistent_ai`: an active command even after its queue drained,
  `Spawning` / `RunCommandSet` settling to `Idle`, `ReturnState`, or a combat behavior with
  aggro entries to clear) / pending calls (`woken`), newly added AIs for a 20-tick initialization
  grace, and `NpcTemplate.ActivateAiAlways`. The predicate widens the C# `HasPersistentAi`; a
  settled `Idle` / `Roaming` / `DoNothing` / `Alert` stays dormant, and the manager's wake /
  maintenance sweep, the framework tick guard and cold-NPC reclamation share it, so the
  initialization → `Idle` settle, return-home and empty-queue command work keep ticking.
  Candidates are deduplicated and ordered by their monotonic `AddAi` ordinal, so a replaced
  handle can never mix into the selection and a dormant AI far from every player is neither
  locked nor visited. A maintenance sweep every 50 ticks drops stale owners and NPCs deleted
  without the `npc_deleted` hook. `World.AiWorkerThreads` (default `1`, clamped to `1..=4`) runs
  one tick batch as at most that many contiguous chunk jobs on the existing Tokio blocking pool:
  `1` performs the single sequential pass and keeps the strict global `AddAi` execution order,
  higher values let the chunks run in parallel. The chunk jobs are awaited and their diagnostics
  aggregated once per batch; the serialized batch runs under its own supervisor, so a cancelled
  caller cannot release the batch lock while its jobs are still running. No separate process or
  thread pool is created. Focused tests in `systems/ai/manager.rs` cover the candidate selection
  and batching.

* systems::npcs (2026-10-14): the Halloween schedules of the 1.2 compact data are one-off rows
  (`53`/`54`/`55` "Halloween event 12:00/17:00/22:00" 2013-10-16 .. 10-30, `57` "Halloween
  event NPC" 2013-10-16 .. 10-30, `58` "Halloween event craft doodad" 2013-10-16 .. 11-06,
  `8000001` "NA Halloween doodad placement Ezna/Austera" 2014-10-20 .. 11-04), all expired.
  `Events` (default `Halloween.Enabled = true`) rewrites those six rows in memory at startup
  (`ScheduleData::apply_halloween_window`, wired in `main.rs` after `GameData::load`) instead
  of editing the read-only `compact.sqlite3`: the yearly mode keeps each row's month / day /
  time and uses the current UTC year (next year when the rolled end is already past); an
  explicit `Start` / `End` pair (`YYYY-MM-DDTHH:MM:SSZ`, parsed strictly, both or neither)
  replaces the window while the daily hours of `53`-`55` stay; invalid values log a warning and
  fall back to the yearly window. `Enabled = false` leaves the retail rows expired.
  `ScheduleData` also builds the `quest -> schedules` map the C# left as `//TODO: quests data`
  in `LoadGameScheduleSpawnersData` and exposes `period_status_quest`.

* systems::npcs (2026-10-15): the schedule items (event timer bar) are live
  (`schedule_item_session.rs`, `schedule_item_packets.rs`); the 1.2 C# never sent or took them,
  so this follows the 5.0 C# (`CSTakeScheduleItemPacket`, `SCScheduleItemSentPacket`) and the 1.2
  client. World entry (`CSNotifyInGame`) joins the account session and sends
  `SCScheduleItemUpdatePacket` (0x1eb); a 60 s tick credits the account clock and re-sends the list
  only to characters whose rows changed (progress, a window opening / closing, a daily reset or a
  take through another character of the account); leaving the world credits the open interval
  first. The take is `CSTakeScheduleItemPacket` (**0x13b**, one `u32` schedule row id; the opcode
  and body were read off the 1.2 `x2game.dll` Lua binding: it builds a packet object with opcode
  field `0x13b` and the id at `+0xc`, and `OnScheduleItemSent` reads a `u32` plus a `u8`). It
  reserves, hands the reward over (bag if the whole stack fits, otherwise one `SYS_EXPRESS`
  letter, like the achievement rewards) and only then records the receipt, all under the economy
  `ops_lock`, so a window pays once; a failed hand-over leaves the term ready. The answer is
  `SCScheduleItemSentPacket` (0x151: `u32` schedule id, `u8` by-mail), followed by a fresh list.
  Deviations / unverified: the take packet's body width is inferred (the serializer writes the
  field through a generic stream call whose width was not traced; it matches the 5.0 C#);
  eligibility is `paid_patron` + the real premium grade from the account state (WP-B), so rows
  needing a grade are listed for the matching tier; the 1.2 client has no PC-bang membership flag,
  so `pc_bang` stays false and PC-bang rows stay unlisted; the progress is loaded at startup
  (`ScheduleItemManager::restore`) and saved inside the financial snapshot transaction
  (`SaveManager`), not by the C# per-account saves.

* systems::npcs (2026-10-14): a `DoodadFuncRatioRespawn` replacement initializes the new doodad
  synchronously (`Spawn` -> `InitDoodad` -> `DoPhaseFuncs`), so a cyclic definition recursed
  without bound until the stack overflowed; the 1.2 compact data has five template pairs whose
  phase functions respawn into each other at ratio 10000 (e.g. 4345/g10678/f520 ->
  4349/g10680/f521 -> 4345), and the C# per-doodad `ListGroupId` recursion guard resets with the
  replacement. Each spawner now tracks its active synchronous replacement chain: a chain deeper
  than 8 entries, or a repeated source `(template, func group)` whose hops since its first
  occurrence all always pass their roll (ratio >= 9999, an unconditional cycle), is rejected with
  a warning, the current doodad stays in its phase, and finite A -> B -> C chains are unchanged.
  A repeat through a hop with a chance below 1 is allowed (bounded by the depth limit only), as
  in the C#: the weather marker 2768 (func group 7055 respawns into 2768 / 3085 / 3086 / 3225 /
  3226 at 20% each) reaches 7055 again synchronously from its initial phase 7026 through its
  RatioChange funcs (about 7% of spawns) and ends by chance; the first guard version rejected
  those replacements ("replacement chain rejected") and the doodad waited for its Final to
  respawn the base template.

* systems::npcs (2026-10-16): world-spawned ("natural") doodads keep their growth state across
  server restarts. The C# (and the first Rust port) recreated every doodad of
  `doodad_spawns*.json` in its Start phase with the full growth timer on each start, so with
  restarts more frequent than the longest growth chain (about 9 400 of the 42 670 main_world
  spawns need more than 24 h, the longest about 103 h) trees, ore, wild crops and cotton never
  matured. Deviation: doodads whose template waits at least 5 minutes in some phase (`Timer` with
  a next phase, `Growth`, `Final` with `After`; about 10 900 main_world spawns, 143 templates;
  shorter chains restart in seconds and static props have nothing to keep) are stored in the new
  table `doodad_spawn_states`, keyed by `(world id, spawn key)` where the key is the spawn
  entry's authored template id + position at 0.1 m (not the doodad's current template, which a
  `DoodadFuncRatioRespawn` swaps, and not the JSON `Id`, present on 7 entries only). The row holds
  the current template, func group, plant / growth / phase time, `Data` and the interaction
  counters. Writes are coalesced per key and flushed in batches every 5 s, by the periodic and
  shutdown save (`doodad::save`); the row is deleted when the doodad is deleted (harvest, Final,
  despawn), so a respawn starts fresh as before. At startup (`SpawnManager::load`, main world
  only) all rows are read once; `DoodadSpawner::spawn` restores the first doodad of an entry with
  the player-doodad load mechanism (`FuncGroupId` setter, then `OverridePhaseTime = PhaseTime`),
  so the phase timer resumes with its remaining delay and an expired phase fires promptly.
  Downtime counts as growth time. Rows whose template or func group no longer exist or whose
  template is no longer timed are dropped with one summary warning; two spawn entries sharing a
  key are only tracked for the first. Tracking stays off if the table cannot be read. Limitation
  (as for player doodads): a phase that has both a `Timer`/`Growth` and a `Final` with `After`
  lets only the first consume the saved phase time.

* systems::npcs (2026-10-16): `DoodadFuncTod` takes its `curTime >= Tod` catch-up on every
  entry, which ignores that the clock wraps; the 20 day/night board templates (West Gate
  4345-4352, Blue Salt 6175 / 6213 and their variants: day board Tod 20:00, variant Tod 02:00 or
  06:00, each respawning into the other through `DoodadFuncRatioRespawn`) therefore swapped back
  and forth synchronously for the whole game evening (20:00-24:00, about 40 real minutes every
  4 hours), tripping the replacement-chain guard about 880 times a minute and churning doodads
  and packets until the base template's Final respawned it. A doodad created by a
  `DoodadFuncRatioRespawn` replacement (`DoodadSpawner::spawn` with `RespawnDoodadTemplateId`
  set) now skips the Tod catch-up during its entry phase (`suppress_tod_catch_up`, cleared by
  `InitDoodad`): it was created at this time of day, so its Tod refers to the next occurrence,
  which the time-of-day watcher fires on crossing. The C# has no such suppression (its chain
  recursed). Boards respawned by their own Final, and every other spawn, keep the catch-up.

* systems::npcs (2026-10-22): `DoodadFuncOreMine`, `DoodadFuncRockMine` and `DoodadFuncShear`
  were log-only stubs in the C#, so their phases never advanced and ore / rock veins and the
  shearing doodads could not be gathered. The 1.2 compact tables carry no loot, labor or tool
  columns either (`doodad_func_ore_mines` / `doodad_func_rock_mines`: `id` only;
  `doodad_func_shears`: `shear_type_id`, `shear_term`), so the Rust funcs now do what the
  authored phase graph describes, like `DoodadFuncCutdown`: set `ToNextPhase` and let the
  engine enter the func's `NextPhase` group. That group holds the `DoodadFuncLootItem` rows
  (e.g. vein 326: 255 OreMine -> 425 three loot rolls -> 426 `Final` / `RatioRespawn`;
  green ore 1532: 2371 -> 2372 -> `RatioChange` branches) and, for shearing, the regrow
  `Timer` (56 Shear -> 57 loot + 30 s timer). Loot keeps its C# task type
  (`RecoverDoodadItem`), so a full bag still sends `BagInvalidItem` and leaves the phase
  unlocked. Labor is the gathering skill's `consume_lp`, enforced by the skill cast. The
  tool is data only: the C# server never validated `SkillTemplate.MainhandToolId` (the 1.2
  mining / shear ids 2654 and 15607 do not even resolve to `items` rows) and neither does
  Rust, so there is no server-side tool refusal at 1.2 parity.
  Authorization hardening (2026-10-23): phase progression now requires the current func's
  authored skill before cast resources/effects and again at phase execution. A gathering
  caster must be a character: an owned mate/slave cannot advance the phase while
  bypassing character labor/quest checks, leaving a free loot retry for its owner. The generic
  `GetFunc` skill-less fallback is still used for automatic loot chaining, but cannot authorize
  gathering. Legacy `OreMine`/`RockMine`/`Shear` rows have zero/null `func_skill_id`; their
  intentional server compatibility adapter uses the shipped basic skills 11106 for ore
  (3 LP), 11107 for rock (0 LP), and 11642 for shearing (0 LP, quest 266 requirement). Skill
  names, tools and effects support these pairings, but local 1.2 native/UI selection was not
  verified; this is a defined server deviation, not a claim of native dispatch parity. An
  explicit row skill takes precedence. The adapter selects a matching legacy function
  before the generic fallback, including phases exposing multiple gathering types. Free Use
  skill 15212 cannot mine ore 1534 (phase 2387 -> loot 8077), nor can the zero-labor rock skill
  replace the ore skill. Skill-driven `Use` and `FakeUse` gathering keeps its binding (shearing
  13802 on template 518 phase 5649, rock 11107 on template 1354 phase 2244). Gathering dispatch
  is recognized by compact's tool asset and interaction metadata; authorization is
  always an exact row/template/implicit skill match, without requiring every gather to cost
  labor or requiring inventory tools that do not exist in the 1.2 item data.
  Content limitation: the shipped compact retains rock 310's loot reference to item 4734
  and sheep 307's references to item 3, but neither item exists in `items`. This skill
  adapter does not create those production item definitions or replace their loot; normal
  economy validation still refuses those missing drops with `BagInvalidItem`.
  The ignored compact regression explicitly supplies in-memory, test-only item templates
  for 4734 and 3 to exercise the authored rock/sheep dispatch and quantities. Ore 1534 uses
  real item 8077. Passing the stand-in cases is not proof of complete live/client content.

* systems::npcs (2026-11-05, WP-K1): the 22 gather / collect funcs (`DoodadFuncButcher`,
  `Catch`, `CerealHarvest`, `CropHarvest`, `CrystalCollect`, `DairyCollect`, `Dig`,
  `DigTerrain`, `DyeingredientCollect`, `Feed`, `FiberCollect`, `FruitPick`, `GassExtract`,
  `MachinePartsCollect`, `MedicalingredientMine`, `Mow`, `PlantCollect`, `SeedCollect`,
  `SkinOff`, `SoilCollect`, `SpiceCollect`, `TreeByProductsCollect`) were log-only in the C#,
  so a player harvesting their doodads got nothing and the phase never advanced. Shipped data:
  each kind has 20-32 funcs whose `NextPhase` group holds the authored `DoodadFuncLootItem` /
  timer chain (apple tree 317: FruitPick 27 -> 3278 LootItem 995 -> 2215 grown tree timer),
  and the test doodad 1568 carries one of every kind. INFERRED rule (no capture): a player
  caster whose func row has `NextPhase > 0` sets `ToNextPhase` and enters that group, like the
  `OreMine` / `RockMine` / `Shear` port; rows with no next phase (`NULL`/`-1`/0) keep the old
  behaviour (the `-1` despawn path is not entered), and a non-player caster does not advance.
  No tool or labor gate is added: the interaction skill that opened the cast already pays its
  own cost. UI / marker funcs (`Climb`, `OpenPaper`, `Bubble`, `Craft*`, `*Ui`, `Navi*`, ...)
  stay log-only. `DoodadFuncEvidenceItemLoot`'s `Use` arm now logs at trace instead of warn
  (its effect is the crime system's phase-func contribution).

* systems::ai (2026-10-15): the optional compact patch
  `SQL/patches/compact/2026-10-14-fix-npc-ai-param-syntax.sql` repairs malformed Lua syntax
  in `npc_ai_params` 443, 835 and 1975 only when the original bytes match. It preserves authored
  values and is idempotent; 1975 recovers the five complete skill entries present in the source,
  without reconstructing any potentially missing entries. Back up the database, apply the patch
  offline to the server's compact copy, then restart; runtime loaders remain read-only. AI tick
  diagnostics now report plan, tick and maintenance time with active-work counts. Parallel tick
  time is elapsed batch time, including dispatch and await overhead, rather than summed job time.

* systems::ai (2026-10-16): retail NPC route files are not shipped (`Data/Path` has 13 files, while
  `ai_commands` references 280 distinct FollowPath names such as `public_mate2`, `public_garden3`
  and `tuto_move`). As in the C# (`Simulation.GoToPath`: warn and return, the NPC stays put and
  keeps `IsInPatrol`), `Simulation` stops without moving, but the "Stop moving... no recorded
  path file" warning is now logged once per file name (`System::first_missing_path`) and at debug
  afterwards, since a repeating skill such as the tutorial NPC's `tuto_move` fired it every 30 s.
  Quest-component follow paths (`QuestComponentTemplate.AiPathName`, unused by the C#) are
  reported the same way, once per name, instead of an error on every run.
  The `Impossible to parse npc_ai_params` warning now names the compact patch that repairs rows
  443, 835 and 1975.

* systems::ai (2026-10-16): route data is now generated from the 10.x client. The designers' AI
  paths are the `<aiPath>` entities of `game/worlds/<world>/level_design/zone/<key>/editor/npc.xml`
  in the 10.x `game_pak`; `aaemu-aipaths` (`crates/aaemu-tool-aipaths`, see its README) writes the
  ones the compact database asks for (`quest_components.ai_path_name`, FollowPath `ai_commands`)
  as `Data/Path/<name>.path` in world coordinates (zone origin cell from the 1.2 `world.xml`, plus
  the entity position, plus the scaled and `Rotate`d local points; the direction of `Rotate` was
  settled against the 1.2 terrain, `real_ai_generated_paths_follow_the_terrain`). It never
  overwrites a file that exists, so the hand recorded routes stay. Deviation: a name used by
  several zones (`public_farm*`, `public_mate*`, `tuto_move`) is written as `<name>@<zone key>.path`
  variants and `AiPathsManager::resolve_path_file` picks the variant whose route passes closest to
  the NPC, for both `AiCtx::load_ai_path_points` and `Simulation` (whose per-NPC route cache is
  keyed by the file stem). Not generated: names the 10.x data lacks (`public_garden*`,
  `caravan_maha_*`, `sunrise_745_*`, ...), spawner-attached paths (`AIPath_<zone>_<npc>_<n>`, whose
  points are relative to their spawner) and zones whose origin differs between 1.2 and 10.x (none
  of the asked-for names is in one). `Rec._Speed`, `Rec._PathType` and `Closed` are not encoded:
  every asked-for path has `Closed=0` and the route plays once.

* systems::ai (2026-10-17): `Simulation` no longer indexes a route with another file's length.
  The C# `ReadPath` returns early for an already cached file name without refreshing `MovePath`
  (the raw lines of the last file read), while `OnMove` / `GoToPath` took the end of the route and
  the first target from `MovePath` and the points from `Path`. After route A -> route B -> cached
  A the two disagreed and `Path[MoveStepIndex]` threw in the C# (a swallowed exception in the task
  there); in the Rust port it panicked in the scheduled move task, poisoned the NPC's simulation
  lock and every later AI tick of that NPC panicked on `simulation poisoned` until a restart
  (`index out of bounds: the len is 5 but the index is 5`). Deviation: all length, emptiness and
  target checks use `Path` (the parsed points of the selected route) via `Simulation::advance`, an
  index outside it (also a one-point route turning back) logs a warning with the NPC and file name
  and stops the move, and `lock_sim` recovers a poisoned simulation lock (clears the poison)
  instead of panicking. The back and forth semantics are unchanged.

* systems::npcs (2026-10-16): `Data/Portal/worldgates.json` has generated entries. The `Return`
  special effect (type 25) with a non-zero `value1` teleports to the world gate of that
  `return_points.id`, but the file only held 26 hand recorded gates, so quest skills such as
  "The Crazy Son" (quest 532: skill 13507, buff 1494, buff trigger -> `Return` 30, "near Liania")
  logged `Cannot find return WorldGate`. The retail positions are the `ReturnPoint_<editor_name>`
  spheres of `game/worlds/main_world/level_design/zone/<key>/editor/return_point.xml` in the 10.x
  `game_pak`; `aaemu-aipaths return-points` (see the crate README) appends 40 gates in a marked
  section of the file (world X/Y = zone origin cell * 1024 + the zone-local position, Z as is,
  Yaw from the `Rotate` quaternion, zone key and name checked against the 1.2 `world.xml` and
  the compact `zones`). The method reproduces the hand recorded gates 402-405, 513, 514, 614 and
  389 within 1-4 m. `SubZoneId` is the gate id (only the dictionary key, never looked up by
  sub zone). `real_worldgates_follow_the_terrain` checks every generated Z against the 1.2
  heightmap, or a navmesh node within 20 m (the upper floors of gates 42, 304 and 306); gates
  399, 400, 533, 627 and 681 failed both and are left out (`--skip`). Still missing: 100 of the
  166 distinct `Return` ids. 79 have no point in the 10.x main_world files (instance worlds such
  as Nachashgar, the libraries and Howling Abyss, which need their own world origins, plus 54, 68,
  69, 73, ...), 16 have no `return_points` row at all (19, 20, 40, ...) and 5 were skipped. The
  warning stays and prints the entry to add. Ten hand recorded gates are more than 5 m from their
  10.x point (221, 372, 617, 619, 628, 672, 673, 354, 652, and 48 by 6 m); they are untouched.

* systems::npcs (2026-10-22): `Data/Portal/worldgates.json` has 67 more generated gates for the
  instance worlds. `aaemu-aipaths return-points --world <instance_world>` (repeatable) reads the
  world's own `world.xml` (instance zones have their own origin cells, identical in the 1.2 and
  10.x files of the worlds used) and its `level_design/zone/<key>/editor/return_point.xml` in the
  10.x `game_pak`, looks the points up by the 1.2 `return_points.editor_name` and replaces the
  terrain check by a nearest-1.2-spawn warning (the Nachashgar points lie 0.3-7 m from a portal
  spawn; the library "up" floors have their nearest spawn 40 m lower, the floor pitch). Added:
  Nachashgar 524-532, 546-553, 561, 625, 626; Nachashgar easy 572-581; library 630-647, 654-669,
  679; Howling Abyss 484, 525; Cradle of Destruction 488. Left out: 399, 400, 533, 627, 681 (main
  world, fail the ground check), 555 and 567 (Immortal Isle, 134 m from the nearest spawn), 68 and
  69 (reassigned ids), 670 and 671 (zone 315 is not in 1.2), ids without any point (386, 568, ...)
  and the 16 ids no version defines. `SubZoneId` is the gate id, `100000 + Id` for 578 and 632.
  Research: Return gates of an instance resolve in the caster's world (retail ships a
  `return_point.g` per instance world).
  Not verified in a live run (no live suite yet: no stack was run for this change).

* systems::skills (2026-10-22): `Return` (type 25) keeps a player inside its instance. The C#
  `Return.cs` is a TODO placeholder: a missing gate fell back to `character.MainWorldPosition`
  and loaded the main world, so an instance Return id without gate data ejected the player
  (the instance Return ids had no gate data at all). The Rust now sends `SCErrorMsg`
  `NO_RETURN_POINT` (439, the retail error string `no_return_point`) and leaves the player where
  it is when the caller is in an instance and the gate is missing or its zone is not a zone of
  the caller's instance world. A gate of the instance's own world moves the player inside it
  (zone change hooks fire, only `SCTeleportUnit`, no `SCLoadInstance`). The character is re-filed
  in the visibility grid immediately after the teleport is sent, outside its data lock, with
  any grid zone changes dispatched afterwards. Distant gates remove it from departure observers
  and show it to arrival observers without a movement packet; `CSTeleportEnded` repaints from
  the arrival region without repeating observer deltas or zone changes. Outside an instance a
  gate whose zone exists only in instance worlds is treated as missing (never instance
   coordinates in the main world). Outside an instance a destination that cannot be resolved
   (the Memory Tome's recall, or a world gate whose zone is unusable) still falls back to
   `MainWorldPosition`: this is the C# `Return.cs` TODO behaviour and the single recorded
   deviation (retail's fallback rule is UNKNOWN). Retail resolves a Return gate in the caster's
   own world (instance worlds ship their own `return_point.g`) and never falls back to the main
   world.
   The C# closed-zone check of `Return` / `OpenPortal` (`Skill.cs`, inline) is ported as
   `portal_zone_check` (`systems/skills/skill.rs:928-1029`): for access level < 100 it resolves
   the same destination the effect would use (world gate, district return point or private
   portal, including the `MainWorldPosition` fallback) and refuses an inaccessible zone with
   `NO_PERM` (an `OpenPortal` outside the main world with `INVALID_LOCATION`) from `use_skill_impl`
   and again from `cast`, before the cast is committed.

* systems::npcs (2026-10-15): `DoodadFuncClout` with `UseOriginSource` takes the first doodad
  within 4 m of the caster as the area trigger's owner, and the C# scheduled the
  `DoodadFuncCloutTask` on that owner, so it changed that doodad's phase to the clout's
  `NextPhase`, a function group of the other template. NPC 10119 near Arach Hollow summons doodad
  5007, whose clout moved the Amber Boulder (4252) into 5007's `DoodadFuncFinal` phase. That
  deleted the boulder, so quest 3447 "Fellow Searchers" could not be completed. 991 clouts use
  the origin source and 602 of them have a next phase. The task now changes the phase of the
  doodad that owns the clout.

* systems::npcs (2026-10-30): a `DoodadFuncClout` whose doodad is initialised before its `Spawn`
  (the `SummonDoodad` path runs `InitDoodad` first, then `Spawn`) keeps that doodad as the area
  trigger origin even with `UseOriginSource`. The C# anchor moved the area to the first doodad
  within 4 m of the caster; when a planted crop stood next to the caster of Comet's Boon (skill
  18222, summoned doodad 5017), the caster sat 2.002 m off the clout's 2.0 m half-length and
  buff 3771 was never applied, leaving the doodad's speed trail without its buff. Already
  spawned doodads keep the C# anchor. `Unit.DecrementTriggerCount` now clamps at 0 as the C#
  `Unit.DecrementTriggerCount` does: a clout with `NextPhase == -1` is removed by the doodad
  delete and again by its task, and the former -1 suppressed the buff of the next clout on the
  same unit.

* systems::npcs (2026-10-30): `SpecialType.SpawnDoodad` with `value2 > 0` runs `InitDoodad()`
  and then `Spawn()` together when the delay expires (a doodad task), instead of initialising
  at the cast and spawning from a bare tokio task. The C# order (`InitDoodad(); if (delay > 0)
  Thread.Sleep(delay); Spawn()`) is kept inside that step: the start phase runs on the still
  invisible doodad, so no viewer gets a phase change of its init, and the area trigger of a
  clout is first ticked after the spawn (the tick needs the owner's region, which `Spawn`
  assigns after it made the owner visible). Deviation: the init moved from the cast to the end
  of the delay. The port's delayed spawn ran on the task runtime, and a clout whose `duration`
  was shorter than `value2` ended (and deleted the doodad) while it was still pending. The
  start phase now starts at the spawn, so e.g. the clout of a `SpawnDoodad` summon is applied
  for its full duration after the doodad appears (the C# phase tasks were scheduled at the
  cast, so an over-due clout task fired right after the spawn). The owner unit that
  `InitDoodad()` hands to the phase (`ParentWorld.GetUnit(OwnerObjId)`; `DoodadFuncClout` takes
  `areaTrigger.Caster = caster is Character ? caster : owner` from it) is taken at the cast, as
  the C# init ran there: a caster who leaves during the delay still owns the clout, whose
  target relation is not judged against the doodad instead. A delayed spawn that can no longer
  run (its instance is gone) releases the doodad's object id, which nothing else owns.

* systems::npcs (2026-10-30): an `AreaTrigger` is removed once. `RemoveAreaTrigger` drops the
  trigger from its owner's `AttachAreaTriggers`; the C# ran `OnDelete` again on every removal
  and left the trigger attached until the owner doodad was deleted. A clout with `NextPhase ==
  -1` is removed by both `Doodad.Delete` and its `DoodadFuncCloutTask`, and two casters' comets
  over one ally share the buff's trigger count: the duplicate `OnDelete` decremented it twice,
  so the ally lost the buff when the first clout ended. The first removal now marks the trigger
  removed and takes its `Units`; a repeated removal does nothing. A removed trigger is neither
  ticked nor entered again, although the manager keeps it in its list until the end of the next
  tick: its owner can stay visible (a clout with `NextPhase != -1`, or the doodad a
  `UseOriginSource` clout borrowed), and an update would count every unit inside once more and
  re-apply the buff, which nothing removes afterwards. A removal that lands while `UpdateUnits`
  applies its enters and leaves (the doodad task runtime and the area tick run on different
  threads) leaves releasing the counted units to that update.

* systems::vehicles / managers::item (2026-10-15): the shared death hook ignores self-deaths
  of objects other than characters and mates instead of reporting a missing mate owner. A mate
  whose owner really is missing still logs the mate and owner object IDs. A successful item
  deletion with fewer affected rows now logs at debug: the other queued IDs were already absent
  (for example, an item consumed before its first save). SQL errors and transaction handling are
  unchanged, and deletion IDs are acknowledged only after commit.

* systems::skills (2026-10-19): `CharacterSkills.AddSkill` refuses a skill whose `ability_level`
  the character's ability has not reached, and the *stored* character skill level saturates to
  `1..=127` instead of wrapping (`stored_skill_level`; runtime `SkillRun` levels, NPC skills
  included, keep the C# byte result unchanged. C#: `(byte)((abLevel - AbilityLevel) / LevelStep + 1)`). The 1.2 data has 24 retired /
  login-stage rows with `ability_level` 99 (e.g. 10385, 11854; `show = f`) above the level cap 55;
  the C# expression goes negative and the byte cast wraps it (243), which MySQL rejects when
  saving `skills.level` (signed TINYINT, `1264 Out of range value for column 'level'`), so every
  character save of the account failed and the Rust departure retry then refused the next login
  with "Previous character save is pending" until restart. The 1.2 client only offers a skill from
  its ability level on (and never a `show = f` row), so retail never learns those; the check drops
  them without a packet, like the other silent refusals of `AddSkill`. The C# (and its upstream
  master) has no such check and fails the same way.

* systems::skills timeline ids (2026-10-19, upstream issue #753): the C# `SkillTlIdManager`
  slowly ran out of its ~65k ids (`0x000A..=0xFFFE`) because several `Skill` / `PlotTree` paths
  returned without `ReleaseId` (the item-reagent rejection inside `Skill.Cast` and the plot
  entry failures among them); once full, `GetNextId` logged `Fatal` and returned 0 forever, so
  every later skill was sent with timeline id 0. The Rust port returns the id on every path: the
  item-reagent rejection (`stop`), `cast()` on a cancelled skill, plots refused by the dominion
  runtime or without a tree (`do_plot_end`), and cast / channel / delayed-effect tasks whose
  scheduler is gone (finished inline). A reused `SkillRun` that still holds an id (auto attack
  overlapping a delayed apply) has the old id recovered and counted (`replaced`). Exhaustion
  never blocks or panics: it logs a throttled `warn` with the holders' ages and hands out 0 (the
  cast continues without a timeline), and the manager exposes in-use / peak / gets / releases /
  unused / exhaustions / replaced / leaked-over-10-min counters (`TlIdStats`), shown on the Web
  API `/status` page and in the every-500-gets debug line. Covered by
  `skills/tasks.rs` unit tests and `skills/tl_id_flow_tests.rs` (a 250-iteration sweep of instant,
  interrupted, completed and channelled casts, plus failed validation, item-reagent rejection,
  refused / treeless plots, a replaced reuse and an exhausted pool; every case asserts the pool
  returns to baseline).
  A timed cast flagged `cancelled` by something other than `stop` (a cancel effect) is ended by
  `cast()` from its CastTask (which also clears the task slot), exactly once: `end_skill` takes
  the id, so the `stop` path (token cancelled, task returns early) cannot double-release. Stored
  skills dropped at load because their ability level is not reached are logged (`warn`).

* systems::skills log noise (2026-10-16): `CSStopCasting` with `tl == 0` and a plot tl for a plot
  that already ended (the client repeats it ~30 times per plot) is logged at debug instead of
  the C# warning; no acknowledgement packet is sent (client behaviour unverified; possible
  follow-up: answer with SCPlotCastingStopped/SCPlotChannelingStopped as for a mismatched
  plot). "Failed to remove unit aggro" is still a warning for direct callers, but the
  `AddUnitAggro` NoFight/Returning guards and the death fan-out of `clear_incoming_aggro`
  (which the C# also logged) go to debug. `aggro_watch` (Rust-only reverse index of the C#
  `OnHealed`/`OnDeath` subscriptions) is now unregistered when an NPC's whole table is dropped
  (`combat::drop_aggro_table`, NPC death and `ClearAllAggroTargetsAndCheckCombatState`), so
  stale holders no longer receive the victim's death fan-out.

* Static-data gaps (2026-10-15): buffs 2149, 1142, 1933 and 1434 reference absent skill controllers
  3554, 1806, 7749 and 2395 in the available 1.2 compact copies (the 1.2 client's own
  `compact.sqlite3` lacks them too; the 10.x and C# data set these buffs' controller id to 0). Their controller behavior remains
  unsupported pending authentic definitions; no replacement parameters are invented. The supplied
  BuffEffect warnings also reference 33 absent buff templates, ten referenced by skill rows.
  Restoring them requires an authentic 1.2 dataset. Counts (compact.sqlite3): 338 buffs
  reference 113 absent skill controllers (also 6121 for buff 4341, 2883 for buff 1643) and 35
  BuffEffect rows reference 30 absent buffs (16 of them used by skill effects). The C# warned on
  every buff start; the Rust port's startup summary warns for each gap (per-row detail at debug); the
  runtime skip on buff start logs at debug. Skill 14602
  already exists and uses the inherited fallback cast path for Memory Tome interactions; its
  "undefined use type" diagnostic does not mean the skill template is absent: it is the normal
  path for doodad / quest-interaction skills (14907, 14599, 16520..16620, 19015 ... are
  unlearned, mostly target-type-8 skills) and is logged at debug here (the C# warned). Quest skill 16881
  (Detection Orb, quest 2394) references NPC group 200, whose members 7976/7977 have no available
  NPC templates. Its unique `unit_reqs` kind-34 `value2 = 1` is a negation: the 10.x zone server
  (`sub_39770380`) passes a TargetNpcGroup requirement when the target NPC is in the existing
  group and `value2 == 0`, or is not in it and `value2 != 0`. The C# only checked that the
  target is an NPC. The Rust port checked strict membership, which rejected every Detection Orb
  cast with result 93 ("Invalid target"). It now follows the zone server. Missing authored `tuto_move`,
  `public_farm*` and `public_mate*` paths also remain data gaps.
* `BuffTrigger.Execute` / `TimeoutBuffTrigger.Execute` logged "Owner is not a Unit" and returned for every trigger of a buff owned by a doodad (hostile area skills such as 16486 hit factionless doodads, so e.g. the shrink buffs start on them). The trigger can never run for a doodad, so `subscribe_triggers` now skips non-unit owners (trace log) instead of subscribing a trigger that only warns; the `execute_trigger` guard stays as a safety net and names the buff and owner. Behaviour is otherwise unchanged.
* systems::economy (quest item events, 2026-10-17): the C# raised no `OnConsumedItem` for a partial
  `CSDestroyItem` (only the full destroy, through `RemoveItem`) and none for the giving side of a
  trade (`AddOrMoveExistingItem` only notified the destination container's owner). `ItemGather`
  objectives therefore kept counting items that had left the character. Here the partial destroy
  (`item_handlers::CSDestroyItemPacket`) pushes `ItemFx::Consumed` for the destroyed units and
  `ItemContainer::add_or_move_inner` pushes it for the source owner when the item moves between two
  characters' bags (trade), so the objective is recounted when items leave.

* systems::economy (trade validation, 2026-11-03): `TradeManager.CanStartTrade` and
  `StartTrade` only checked the running-trade state (C# TODO "Check faction and others"); the
  block list was added earlier. The 1.2 client (`x2game.dll` `X2Trade:RequestTrade`
  `FUN_394fddf0`, `X2Trade:StartTrade` `FUN_394fe180`) refuses a request/acceptance itself while
  either player is dead (`X2Unit:IsDead`, `unit + 0x3c26`, silently), while the target is not
  within the trade range (`FUN_39093860` with the constant `25.0` at `0x39ac9b84`:
  `>= sqrt(25) = 5` m edge-to-edge, error 154 `too_far_away`), and it never offers "Request
  Trade" on a hostile target (`popup_menu_proc.alb` proto 48 skips `requestTrade` when
  `GetTargetFactionRelationship() == 1`; `FUN_394bb930` maps 1 to `hostile`). The server now
  applies the same checks before forwarding a request and again when the invitation is accepted
  (the invitation can be 60 s old). Refusals: dead is silent (the client shows nothing), a
  hostile relation sends `SCCannotStartTradePacket(reason = 0)`, which the 1.2 client's
  `OnCannotStartTrade` (`FUN_394ff1b0`) displays as error 538 `trade_target_is_not_possible_state`
  and a distance refusal sends error 154, the code the client uses for the same check. A
  different instance is refused with error 539 `cant_trade_with_other_view` (the client's own
  "other view" check, `FUN_390476a0() + 8`, is not identified). The exact 1.2 **server** answers
  are unknown (the checks above are client-side): individual packet codes are capture-pending,
  cases `TRADE-1` (dead), `TRADE-2` (hostile), `TRADE-3` (other view) and `TRADE-4` (distance).
  Tests: `trade_tests::{hostile_players_cannot_trade_but_the_same_faction_can,
  a_faction_change_between_request_and_accept_refuses_the_trade,
  out_of_range_trade_request_and_accept_are_refused,
  dead_player_cannot_request_or_accept_a_trade}` (each fails with its check removed).

* systems::economy (trade safety, 2026-10-19): a trade is made only when both sides locked and
  both confirmed (`OkTrade` previously accepted a confirmation with one side locked). Only the
  put-up amount changes hands: a larger stack is split at finish, the receiver's
  `SCOtherTradeItemPutupPacket` shows the offered amount, and the giver's `SCTradeMade` task names
  the slot the item had in the giver's container (the C# moved the whole item and built the
  removal from the moved object, i.e. the receiver's slot). Negative money put-ups are rejected
  (the C# stored them unsent and skipped them at finish), money transfers use checked `i64`
  arithmetic, and trade ids are released on cancel / departure / finish. `FinishTrade` first
  checks every offered item (still held by the giver with at least the offered amount, several
  put-ups of one stack added up; receiver bag has a free slot the item is accepted in) under the
  item operation lock and cancels the whole trade for both sides when any check fails; money then
  moves as deltas under one guard per character (the C# applied money first and counted failed
  item moves in `hasErrors`, leaving a one-sided trade). Taking an item down removes only that
  item's put-up (the C# cleared the whole list when it held at most one entry), the peer's
  `SCOtherTradeItemTookdown` shows the offered amount like the put-up (the C# sent the whole
  item, as it also put up the whole item), and `CSTradeLock(false)` resets both OK flags like
  `UnlockTrade` (the C# kept them). Live oracle: two
  `aaemu-testclient` bots (lock one side only -> no trade; negative money -> rejected; 5-unit
  stack, 2 put up -> only 2 move, 3 stay).
* systems::economy (shop currency checks, 2026-10-19): `CSBuyItems` refuses when *any* required
  currency (money, honor or vocation) is short; the C# checked `money > have && honor > have &&
  vocation > have`. Checked totals and per-currency `checked_sub` already reject overflow.
  Covered by `craft_store_tests::shop_checkout_rejects_money_only_and_mixed_currency_shortages`.
* systems::economy (cash shop, 2026-10-19): loyalty is charged the displayed
  `DiscountPrice > 0 ? DiscountPrice : Price` (already so since the port, test
  `checkout_charges_displayed_discount_for_coins_and_loyalty`; the C# debited `Price`); above `level_max` the buyer gets `LEVEL_HIGH_TO_USE` (1.2 `ui_texts`
  4157, "level too high to use") instead of `INGAME_SHOP_BUY_LOW_LEVEL` (4708, generic "(level)");
  page 0 still shows page 1 on purpose: the 1.2 `x2ui/baselib/page.alb` page control starts at
  `currentPage = 1` and clamps previous/next to `1..maxPage`, so the client never sends page 0,
  and the C# `Skip(itemsPerPage * (page - 1))` clamps the invalid value to the first page.
* systems::economy (auction, 2026-10-19): the "ExpireDate" sort uses `end_time` (the C# sorted by
  `PostDate`) and a buyout sends `SCAuctionBidPacket` with `isBuyout = true`, the lot's
  `direct_money` and the buyer's identity (the C# buyout branch sent nothing although the 1.2
  packet carries the flag). Seller / item mail delivery is unchanged.
* systems::skills (buff charges, 2026-10-28): `Buff.WriteData` (`SCBuffCreatedPacket`) already
  carried the real `Charge` in the C# and does here; this is not a deviation. The deviation is
  `SCUnitStatePacket.WriteBuff`, which hardcoded `tickIndex 0, stack 1, charged 0`. The 1.2
  client's NetBuff serializer (`FUN_397c9230`) names that field `charged`, so a re-sent unit
  state dropped remaining charges; Rust now writes the live `Buff.Charge`. `stack` stays 1 as
  the C# `SCUnitStatePacket.WriteBuff` wrote it (the model has no per-effect stack counter:
  stacks are separate effects, one row each); the 1.2 meaning of that field is not verified.
  `tickIndex` stays 0 (the C# value; no source for another).
  `SCBuffUpdatedPacket` (0x0b9, 15 bytes, client serializer `FUN_397c63f0`) is also new: the C#
  had only the opcode constant and no sender. It is broadcast, to the same audience as
  `SCBuffCreated` / `SCBuffRemoved`, when `Buff.ConsumeCharge` leaves a non-passive buff alive
  (passive buffs send no buff packets), and not when the buff exits. Its `stack` is likewise 1
  as the C# wrote it, meaning unverified. The send timing is inferred: no retail 1.2 server
  capture exists, and whether 1.2 also sent it on refresh / duration change is unknown;
  refreshes keep re-sending `SCBuffCreatedPacket` as before.
* systems::skills (ability change cost, 2026-11-02): `CSSwapAbilityPacket` now validates and
  charges the 1.2 cost; the C# `CharacterAbilities.Swap` only reset and swapped. Evidence: the
  1.2 ability change UI (`x2ui/abilitychange/ability_change.alb` 0.8) calls
  `X2Ability.CanBuyAbilityChange` and shows `X2Ability.GetAbilityChangeCost`; the client's
  `FUN_39386600` (`SwapAbility`) refuses unless `FUN_398a1580` sees exactly three active
  abilities, the old one active and the new one not ("all active abilities must be activated" /
  "the ability is already active" / "the ability is already deactivated"). `FUN_3943c460`
  prices a change as `FUN_395720e0(0x27) * level` (`FUN_398a19d0`; the byte at player+0x4b4 is
  the character level, setter `FUN_390b4930`). `0x27` is `content_configs` id 39 = 200 copper,
  so a change costs `200 * level` copper in gold: `GetCurrencyForAbilityChange`
  (`FUN_3943bbf0`) looks up `currency_configs` kind 10, and the 1.2 table is empty, so the
  currency is 0 = `CURRENCY_GOLD` (`FUN_39744420` returns 0). The level-10 ability activation
  (`X2Ability.LearnAbility`, `x2ui/skill/tab_combat.alb` 5.2 -> `FUN_39384a20`) instead sends
  `old = AbilityType.None` and never calls the cost check, so activating an empty slot is free.
  The server accepts new ids 1..=10 (`FUN_398a1710`), refuses an already active target, requires
  the old ability to be a slot (and all three active for a paid change), checks and debits the
  money under the same character lock as the swap, sends
  `SCItemTaskSuccessPacket(AbilityChange, [MoneyChange(-cost)])`, and answers a failed debit with
  `SCErrorMsg(NOT_ENOUGH_MONEY)`. The 1.2 `formulas` table has no rows 41/42, so
  `SwapAbilityCost` / `SwapAbilitySetCost` are not the 1.2 source; `SwapAbilitySetCost` belongs
  to a later ability-set feature that the 1.2 client does not have and is not implemented.
  Capture-pending: the server's exact answer for an invalid or duplicate swap (the client
  prevents both before sending; the gate logs and drops, no packet).
* systems::skills (ability level up, WP-UI3, 2026-11-05): a rise in an active ability's level
  (its exp through `ExperienceManager.GetLevelFromExp`, the same table as the character level)
  re-derives every learned skill of that ability and sends one `SCSkillUpgradedPacket { skill_id,
  level }` per skill whose `SkillTemplate::level_for` crossed a `level_step`; the stored
  `Skill.Level` is updated and the packets go out after the character lock is released (from
  `add_exp_world_tail`). `add_exp_locked` is the only production writer of `Ability.Exp`:
  `Character.AddExp`, the GM `/level` / `/setlevel` commands and quest rewards all pass through
  it; the ability reset / swap paths remove or move skills without raising a learned skill's
  level, so they send nothing. **Trigger INFERRED (CAP-UI-2):** the 1.2 client prints "skill
  upgraded" from `SCSkillUpgraded` (`FUN_397c67c0` / `FUN_391da210`), but no 1.2 server capture
  of an ability level-up exists. `SCAbilityExpChanged` is deliberately never sent: its client
  handler adds the amount itself and `SCExpChanged(shouldAddAbilityExp)` already does the same,
  so sending both would double count. Test:
  `an_ability_level_rise_upgrades_only_the_learned_skills_that_cross_a_step`.
* systems::economy (auction settlement, 2026-10-20): the C# ignored `MailForAuction.Send()`
  (`RemoveAuctionLotSold` / `RemoveAuctionLotFail` / `CancelAuctionLot` removed the lot anyway),
  so a mail that was not stored lost the seller's money or the buyer's item. Here a settlement
  removes the lot only after every one of its mails was delivered; otherwise the lot (or, for an
  outbid refund in the bidding path, just the refund) stays queued in `AuctionManager::pending`
  and `update_auction_house` retries the still-missing mails on its next tick, remembering which
  ones are already out so nothing is delivered twice. The queue is in memory: a restart inside
  the retry window loses a pending outbid refund, and a pending sale that has not reached its
  `end_time` reverts to the lot's saved bidder fields. A restart while a lot is partially settled
  re-settles the whole lot, so a mail that was already sent (e.g. the seller's payment) can be
  sent again. A settlement whose mail is refused `MAX_SETTLEMENT_RETRIES` (720, one hour of 5 s
  ticks) times, e.g. for a deleted receiver, is logged at error level and dropped, and its lot
  is then removed.
* systems::economy (mail templates, 2026-11-02): the 1.2 client reads `title('…')` /
  `body(…,'…')` in two stages and the C# wrote the item names unescaped (the cash shop escaped
  only `'`). Stage A (native, `sub_3948E4C0` / `sub_3948C420`, only `CHARGED` mails on en_us / de /
  fr clients) runs `sscanf("title(%[^)])")` and `sscanf("body(%[^,],%[^,],%[^)])%s")`, which stop at
  the first raw `)` and know no escapes, so `Resplendent Armor Regrade Scroll (100)` showed as
  `…(100` and its body as raw text. Stage B (every sender starting with `.`) is `GetMailText`'s Lua
  `loadstring`, which decodes `\'`, `\\` and the Lua 5.1 `\ddd` decimal escape. Rust therefore
  writes every quoted template argument through `mail_template::quote_arg` (`\` -> `\\`, `'` ->
  `\'`, `(` -> `\040`, `)` -> `\041`): stage A sees no `)` and Lua restores the name. `|` is not
  escaped (the client splits names on it; no 1.2 item name has one). The auction, trade pack,
  house sale and house tax mails (stage B only) quote item / house / character names through the
  same helper: house names are player-set without any character policy (`change_house_name`), so
  an unescaped `'` let a name break out of the receiver's `loadstring` call. **Deviation:** `load_rows`
  repairs stored `CHARGED` mails written before this (raw parentheses inside the `title('…')`
  argument and the third `body(…)` argument, nothing else touched, idempotent) and marks them
  dirty so the next save persists the fix.

* systems::npcs (doodad phase packet, 2026-11-03): the C# `CSChangeDoodadPhasePacket` only
  logged a warning, so a `DoodadFuncBubble` / `DoodadFuncOpenPaper` never advanced the phase. The
  1.2 client sends the packet itself once its own bubble / paper UI finishes, with body
  `bc objId`, `u32 skill`, `u32 nextPhase`, `u32 funcId` (`doodad_funcs.id`; writer
  `FUN_397bbbb0`, builder `FUN_397ac8d0`, senders `FUN_393b8240` / `FUN_393b8450`, and only
  when the func row's `next_phase > 0`). Rust accepts it only for a Bubble / OpenPaper func of
  the doodad's current phase group whose `next_phase` equals the sent value, from a live caster
  and a doodad not scheduled for despawn, and then runs the normal `DoChangePhase` (phase funcs
  + `SCDoodadPhaseChangedPacket`). The reach check uses the func's own skill only when that
  skill is known and has a positive range; a skill-less, unknown or unbounded func gets the
  5 m doodad interaction distance (`portal::NAVI_USE_RANGE`, also `looting::LOOT_TAKE_RANGE`)
  plus the doodad allowance the cast target check uses. The packet's client-chosen `skill` is
  never used (Bubble funcs 6279, 9407 and 9965 carry no skill, and the field is client input).
  **Deviation:** the C# moved nothing; every refusal is debug-logged and silent (the client
  sends only after its own successful use).
* systems::economy (doodad purchase, 2026-11-03): `CSBuyCoinItemPacket` names a `doodad_funcs`
  row (`bc objId` + `u32 funcId`; writer `FUN_397bae00`) and now only runs a
  `DoodadFuncPurchase` of the doodad's current phase that is priced in a coin item
  (`coin_item_id > 0`, `coin_count >= 1`) within the 3 m doodad store range; the phase check and
  the purchase run under the doodad's phase-operation guard. `FUN_393b80d0`
  (`x2game.dll.c:774526`) opens a purchase dialog in both branches: the money form (vtable
  `399d3158`) when the purchase's coin item is the sentinel or its coin count is below one, the
  coin form (vtable `399d3178`) otherwise; only the coin form sends `CSBuyCoinItemPacket` (0x0af)
  and it shows `not_enough_coin` (483) when the bag lacks the coins. Rust answers the shortage
  with 483 (the C# used `NotEnoughItem` 27) and returns the coin if the grant still fails. The
  money form of the same func (`coin_item_id == 0`) checks out through `CSBuyItemsPacket`, whose
  `unkId` holds the func id (`FUN_393b8040` / `FUN_397ac3f0`, writer `FUN_397bac90`): Rust sells
  that func's own `item_id` at its template price, requires the grade the func offers
  (`fixed_grade` when the template has one, else basic 0) and a count that is a whole multiple
  of the func row's count, and holds the doodad's phase-operation guard through the checkout.
  Without the grade rule a forged high-grade line was bought at the template price and sold
  back at the grade's refund multiplier (item 8661: 64 500 price, 32 250 x 6 refund at grade
  11). The C# left `unkId` unused, used the client grade unchecked and executed whatever func
  id `CSBuyCoinItemPacket` named. After a granted purchase both paths advance to the func's
  `next_phase` through `DoChangePhase` (301 of the 468 purchase rows carry `next_phase > 0`,
  e.g. `doodad_funcs.id` 10159 -> group 11449); the C# never advanced because
  `DoodadFuncPurchase.Use` set no `ToNextPhase` and the coin packet called the template
  directly. **Deviation:** that advance is added behaviour, not a C# port.
* systems::economy (shop limits, 2026-11-03): the `DoodadFuncPurchase` path claims the purchase
  limit, debits the coins and grants the item as three separate steps; they share the economy
  operation scope but not one `ItemTxn`, so a crash between them can lose the coin and the
  claimed copy. The claim is released on a failed debit or grant (debit first, refund on
  failure), but a process exit cannot run that release. Known, not fixed.
* systems::economy (shop limits, 2026-11-03): `ShopLimits::release` changes the in-memory count
  only. `prepare_save` omits zero counts and `write_prepared` is an upsert without a delete, so
  a released claim that returns a template to zero stays in `character_shop_purchases` and is
  loaded again after a restart (the limit is then one copy too strict). Known, not fixed.
* systems::economy (lunagem socketing, 2026-11-03): the 1.2 socket rules read their minimum
  item level from `content_configs` id 0xf8 (248) in the client; the server still uses the
  constant 20 from `content_configs` id 62 (`ITEM_SOCKET_MIN_ITEM_LEVEL`). The 1.2 compact row
  248 is not confirmed to match, so the config read is not ported. Known, not fixed.
* systems::vehicles (mate summon, 2026-11-03): a different mate's summon item despawns the old
  mate before the new one is allocated; a failed allocation after that leaves the player with
  no mate. The item / template / dead-mate checks run first, so only an allocation failure
  after the despawn can lose the old mate. Known, not fixed.
* systems::vehicles (mate summon, 2026-11-03): a dead mate's summon item carries the corpse flag
  and `spawn_mount` refuses it with `MATE_CANNOT_SPAWN_WHILE_CORPSE`; because the same item is
  the dismiss toggle, the corpse flag also blocks dismissing the corpse. Known, not fixed.

* systems::vehicles (siege catapult kill, WP-SD6b, 2026-11-05): destroying slave template 2
  (the siege catapult) during an active siege credits `char_records` kind 26 with `value1 = 2`
  (the authored row's slave id), `value2 = 1` to the killer when the killer and the catapult's
  owner are on opposing roster sides (`record` text 상대진영; the `value2 = 1` meaning is
  INFERRED). Own side, non-participants and a kill outside an active siege credit nothing. The
  C# had no producer.

* systems::services (account payment / premium, WP-B, 2026-11-03): the C# read the payment from a
  permanent `AccountPayment` default (Premium, 0 .. 2030-01-01) in five places. Rust stores
  `pay_method`, `pay_location`, `pay_start`, `pay_end` (unix seconds) and `premium_point` on
  `aaemu_game.accounts`; `pay_method = 0` means `Account:DefaultPayment` (owner decision
  2026-11-03: `premium` by default, configurable to `none` / `demo`). **Owner decision
  2026-11-03: the configured default premium is permanent** (`pay_end = 0` means no end date in
  `AccountPayment::premium_state`); the C# placeholder end 2030-01-01 is no longer inherited.
  GM grants always carry their own end date. A MySQL error while loading the state at login
  keeps the session on the configured default (the login is not refused and the error is
  logged). The premium grade is the
  client's own `premium_grades` lookup (`FUN_397a7d10`: highest `grade_id` with `point <= p`,
  default 1; the 1.2 rows are grade 1 at 0 and grade 2 at 1). `premium_configs` supplies
  `connect_point` (granted on connect), `disconnect_point` (on disconnect) and
  `deactivate_point` (when premium ends), capped at `max_point`; its `max_grade` has no 1.2
  consumer and its meaning is UNKNOWN, so it is loaded but unused. `premium_benefits.max_labor`
  drives `GetMaxLabor`; the online / offline tick amounts stay in `CharacterSettings.json` and a
  startup mismatch logs a warning (they agree in the shipped data: 5/0/2000 and 10/5/5000). The
  only `pcbang_buffs` row with `active = 't'` is 8000001 -> buff 8000011 (grade 2); the Korean
  PC-bang rows are inactive, the client has no PC-bang membership flag, and `pc_bang` in the
  schedule-item eligibility therefore stays false. **Owner decision 2026-11-03:** default
  premium accounts are grade 2, so the grade-2 VIP schedule reward row 8000001 is now listed
  (on existing servers too; it was never listed while the old default had no grade) - a
  deliberate behaviour change. Buff 8000011 is no longer in
  `UNIT_STATE_FORCED_BUFFS`: it is applied for premium grade 2 on login / grade change and removed
  on logout / grade change (it is display-only: no `buff_modifiers`, `duration = 0`, never saved).
  `SCAccountInfo` (i32 method, i32 location, i64 start, i64 end, i64 realPayTime = 0;
  `FUN_397aeb90`) is sent at the two C# moments with the real state; `SCUpdatePremiumPoint`
  (i32 point, u8 oldPg, u8 pg; `FUN_397b8b00`) is sent at `FinishState(7)` with the real point and
  grade (old == new on the initial sync), and with old/new grades on GM / expiry changes.
  Deviation: the C# hard-coded `(1, 1, 1)` at `FinishState(7)` and a literal `1` in the
  `SCUnitStatePacket` premium field; Rust writes the real flag (0 for a normal account).
  GM `/premium <account|name|self> <on|off|days> [days]` (Rust only, access 100) and the premium
  service shop (`systems::economy::premium_service`, WP-C) are the grant paths. The
  grant/removal upserts the `accounts` row (an account with only a login row gets one) and
  answers an error instead of "granted" when the write fails.
* systems::economy (premium service shop, WP-C, 2026-11-04): the C# answered
  `CSPremiumServiceList` (0x136, empty body, writer `FUN_391877d0`) with one hard-coded Russian
  30-day entry and only logged `CSPremiumServiceBuy` (0x135, `i32 cId`, serializer
  `FUN_397b17b0`) and `CSPremiumServiceMsg` (0x13c, `i32 stage`, serializer `FUN_397b17d0`,
  writer `FUN_39187870`). Rust answers the list from `aaemu_game.premium_services` (column
  meanings and an example insert in `SQL/updates/2026-11-04_aaemu_game_premium_services.sql`);
  `compact.sqlite3.premium_points` has no catalogue (0 rows) and the table is empty by default
  because no 1.2 retail row is evidenced. `SCPremiumServiceList` (0x1d8, serializer
  `FUN_397c49b0` -> `FUN_397bf920`) carries one detail per packet (`FUN_391e25e0`); `size` is the
  expected **total** row count, not the per-packet count: the handler appends a detail only while
  `(end - begin) / 0x1b0 < size`, so every packet carries `size = rows` and `isEnd` is set only on
  the last one. A `size = 0` packet marks the list ready (empty catalogue); it does not clear a
  list. The catalogue is capped at 255 rows (the `u8` field, logged when truncated);
  `exchangeRatio` has no source and stays 0. A buy validates the row (`is_sell`, positive
  `hours`, non-negative price) and the currency, then runs the debit and the premium grant in one
  MySQL transaction under the account row lock and `AccountManager::lock_of`: guarded `credits` /
  `loyalty` debit (`... >= price`), payment extension (`pay_end = max(now, pay_end) +
  hours * 3600`; a GM window that starts in the future is replaced by `now..now + hours`, not
  extended), an `audit_ics_sales` row, then commit. A permanent premium (`pay_end = 0`,
  including the configured default) is refused with 773 **before** the debit: there is nothing
  to extend. A row that still uses `Account:DefaultPayment` keeps `pay_method = 0`; the purchase
  stores only its window (`pay_start` / `pay_end`), which `resolve_payment` reads back as
  explicit premium (a `pay_method = 0` row with a stored window is premium, not the default
  payment). An explicit payment row becomes `Premium`. `premium_point` is
  `max(details.premium_point, premium_configs.connect_point).clamp(0, max_point)` - a stored
  higher point is never lowered. Credits and loyalty are debited in the transaction; an AA
  price is refused with `PREMIUM_SERVICE_NOT_ENOUGH_AA_POINT` 775 (AA points are not supported).
  Other currencies are refused with 773. A loyalty debit also sets the buyer's `bm_point` and sends
  `SCBmPointPacket`. The buy result has no dedicated 1.2 SC: the client's error handler
  (`FUN_391e5070`) routes error ids 772..775 through `FUN_39055e80` / `FUN_39055a90` to UI event
  0x266 (`PREMIUM_SERVICE_BUY_RESULT`, PSBFR_NONE/NORMAL/CASH/AA_POINT), so success / failure are
  `SCErrorMsg` with `PREMIUM_SERVICE_BUY_SUCCESS` 772, `PREMIUM_SERVICE_BUY_FAIL` 773,
  `PREMIUM_SERVICE_NOT_ENOUGH_AA_CASH` 774 or `PREMIUM_SERVICE_NOT_ENOUGH_AA_POINT` 775. The
  client handler stores `exchangeRatio` from the `isEnd` packet; the shop's own `SCAccountInfo`,
  `SCUpdatePremiumPoint`, `SCICSCashPoint` (credits) / `SCBmPointPacket` (loyalty) follow through
  the WP-B publish path. A real-client purchase capture is still pending
  (**ACCOUNT-PREMIUM-BUY-1**).
  `CSPremiumServiceMsg` is a client trigger (stage 1 at character-select init, stage 2 at world
  entry) and gets no reply; the C# `SCAccountWarned("Premium ...")` is removed. `premium_services.id`
  (client detail `id`, loaded from `grade`) and `bcount` (`buy_max`) are copied for display only:
  their 1.2 meanings are UNKNOWN and `buy_max` is not enforced.
  Known limits: an ambiguous MySQL commit error (for example the connection drops during
  `COMMIT`) answers 773, and the cached premium state stays stale until the next refresh /
  login even though the transaction may have committed. The new state is published after the
  account lock is released, so a concurrent GM `set_payment` can publish in between and the
  client briefly shows the purchase state until the next `SCAccountInfo`; the database row is
  whichever transaction wrote last. There is no rate limit on `CSPremiumServiceBuy`: every
  packet is one charge.
* systems::services (second password): not supported (owner decision 2026-11-06); the 1.2 client
  showed no second-password option even with `FeatureSet.secondpass` on (real-client run
  2026-11-06). `CSRequestSecondPasswordKeyTables` and `CSSetupSecondPassword` are log-only as
  they were before the feature existed; the four 0x126..=0x129 packets have no handler,
  `SCInitialConfig.secondPasswordMaxFailCount` stays 0 and the lobby is not gated.

* systems::services (`SCInitialConfig` account parameters, WP-J, 2026-11-04): the 1.2 client
  reader `FUN_397b2a00` reads `host`, the 11-byte fset, `count` + entries, `initLp`, the seven
  `canPlaceHouse` / `canPayTax` / `canUseAuction` / `canTrade` / `canSendMail` / `canUseBank` /
  `canUseCopper` booleans (`FUN_397b1fb0`), `secondPriceType` and `secondPasswordMaxFailCount` as
  bytes (stream method `+0x44`, confirmed by the `SCLevelRestriction` reader `FUN_391a5100`) and
  `idleKickTime` as a `u16` (method `+0x40`; `FUN_39017bf0` masks it with `0xffff`). Rust now
  sends `Account:Restrictions:<premium|demo|none>` (seven booleans selected by the effective
  payment method) and `Account:IdleKickTime` (raw `u16`, default 0). Every default keeps the
  exact bytes the server sent before (seven `true`, idle 0). The per-method restriction source is
  **INFERRED** (`ACCOUNT-RESTRICT-1`, capture-pending): the client only uses the flags to gate
  its UI and the server has no restriction store, so the server does not enforce them.
  `secondPriceType` stays the region constant 2 (NA loyalty token).
* systems::services (idle kick, WP-J, 2026-11-04): the client kicks itself from
  `SCInitialConfig.idleKickTime`; the 1.2 `SCIdleKick` reader (`FUN_39184540`) is empty and
  `SCPlaytime` is a no-op, so the server sends neither and implements no idle kick. The unit of
  `idleKickTime` is **UNKNOWN** (the `CGame::Update` consumer was not located): the config is
  the raw wire value, not minutes or seconds. **Needs one real-client run** (not a capture and
  not driveable headlessly: the kick is decided by the real client, which `aaemu-testclient`
  does not emulate; do NOT start the real client from a worker): set `Account:IdleKickTime=1`
  and log in; if the client leaves after about 1 s the unit is seconds, if after about 1 min it
  is minutes. Write the result here and in the config comment; until then the raw value stays.
  `CSIdleStatus` was already handled (it broadcasts `SCUnitIdleStatus` to observers) and is
  unchanged.
* systems::services (`SCRestrictInfo` 0x1c8, WP-J, 2026-11-04): this is a per-character penalty
  record (`chname`, `index`, `restrictCode`, `duration`, start/end times;
  `FUN_397c4570`/`FUN_397beb40`), not the seven `canXxx` flags. The server has no restriction
  store, so no sender is implemented; **capture-pending** for the moderation area
  (`ACCOUNT-SCRESTRICTINFO-1`).

### Ranking boards (Rust only)

The 1.2 C# never answered `CSRankCharacterPacket` (empty, log-only) and has no `CSRankSnapshotPacket` (0x130, unnamed in the opcode tables, `ranking_packets.rs`). The Rust server answers both from a global store (`systems::social::ranking_manager`, `server.systems.social.ranking`):

- **Requests.** 0x12f answers `SCRankCharacterPacket` (one row per supported board, zeros where the character has no score; `type_id` 0 because the request carries nothing to echo). 0x130 answers `SCRankSnapshotPacket` with the current period's standings (<= 100 lines); the racing board and unknown boards are not answered. The snapshot scope list stays empty (the raw scope figures are unresolved). The row's "account" string is the character name: the game database does not know the login account names. 0x130 is the one registration the C# registry-parity test exempts.
- **Persistence.** `character_rank_scores` (one row per character, board and period) and `rank_period_payouts` (periods already paid), loaded at startup and upserted by every `SaveManager` pass (`AfterCharacters`). Every row is rewritten on each save (idempotent, so a rolled-back save loses nothing); `Character::ranking_state` is unused (standings need offline characters, so the store is global).
- **Events.** The arena board scores at the end of an instant game (`instant_game::record_rankings`, once per game): win/lose/draw/invalid points are `game_rule_sets.rank_*_point` (only rule set 28, the gladiator duel in zone group 77: win 4, lose 1). Because `EndGame` leaves both corps `LOSE` when the timer runs out (mirrored C# bug), a timed-out duel scores both players as losers. The two fishing boards score each trophy fish a player lands (`fishing::record_catch`, from `DoodadFuncConvertFish` and the equipped-fish conversion) in the player's zone group; they only count inside the board's authored window, which ended in 2014, so on the shipped data no catch scores. The racing board is never recorded.
- **Period close.** A once-a-minute task (first tick at start-up) mails every closed, unpaid period: place -> `rank_scope_links` band -> `rank_rewards` item (count and grade as authored) as a `SYS_EXPRESS` letter (offline winners included), then `SCRankRewardMailPacket` to online winners. The period is marked paid before the letters are built, inside the economy operation gate; a crash between the letters and the next save can therefore lose (never duplicate) the mark. `rank_rewards.weeks` (how long the reward lasts) and appellation rewards (none authored) are not applied.
- **Alarms.** The once-a-minute task also broadcasts `SCRankAlarmPacket` (0x205) `{ board id, kind }` to every online character when the tick crosses an authored alarm: `start_time_alarm` / `end_time_alarm` are signed minutes relative to the contest window start / end (next to the daily slot hours in `ranks`), and an authored message string is required (a board without one sends nothing). Each `(board, kind, period)` is sent once in memory. Only the fishing contest authors alarms (`-5` / `0` with messages) and its authored window ended in 2014, so on the shipped data nothing fires; the code follows data or a clock that puts a board in its window. `rank_alarm_kind` numbering is INFERRED (CAP-UI-4): the client's handler branches on `1` for the end text and otherwise the start text, matching the SQL column order (0 start / 1 end); no capture proves it. The alarm moment uses the contest slot, not the weekly period anchor: the authored start message announces the contest five minutes before its Saturday 20:00 slot.

### Startup warnings (Rust only)

- **`FamilyIdManager` and family 0.** `characters.family` is 0 for a character without a family. The C# `FamilyIdManager` passes `Exclude = []`, so every start logged "Object ID 0 in DB is less than 1". `IdManagerSpec` now carries the C# `_exclude` list (checked before the first-id warning, as in `IdManager.Initialize`) and `FAMILY_ID` excludes 0. Every other manager keeps an empty list and still warns about ids below its first id.
- **Orphan tower defense waves.** The shipped `compact.sqlite3` has `tower_def_progs` rows for `tower_def_id` 9, 22 and 33 with no `tower_defs` parent (20 waves). The C# `return`s at the first one; the Rust loader skips them (see the instances section) and now logs one info summary with the count and the distinct parent ids; the per-row line is `debug`.
- **Client sources not found.** `Configurations/ClientData.json` lists four default sources and array settings merge by index (as in Microsoft.Extensions.Configuration), so a `Config.Local.json` with two sources leaves the last two defaults in place. `ClientFileManager::from_sources` logs a source that does not exist at `info` while at least one source is valid, and at `warn` for each (plus the existing error) when none is. `add_source` itself only logs not-found at `debug`.

## Client FeatureSet configuration (Rust only, 2026-11-06)

The client FeatureSet is the 11-byte blob in `SCInitialConfig.fsets` (`FeatureSet::write`, a
`u16` length prefix plus the bytes). The 1.2 client keeps it at `DAT_39b02294 + 0x14`, and
`FUN_39496b20` (`x2game.dll.c:956456`) builds the Lua table behind `X2Player:GetFeatureSet()`
from it; bit = byte * 8 + bit (byte N = struct offset `0x14 + N`). `DEFAULT_FSET` plus the
`FeatureSet::initialized` overrides are the C# `FeaturesManager.Initialize` bytes; a retail
capture has not been found, so whether they equal retail is UNKNOWN (see
`D:/aaemu-audits/gap-audit-2026-11-02/research/live-in-1.2.md`).

Rust adds `Configurations/FeatureSet.json` (section `FeatureSet`), applied in `main.rs` with
`FeatureSet::apply_config` right after `GameData::load`. This is a Rust-only deviation: the C#
had no FeatureSet configuration. Every named key is one `GetFeatureSet()` switch, and turning
it on or off sets exactly its bit; `ExtraBits` is a `bit -> bool` map that overrides unnamed
bits (and can turn a default-on bit off). The shipped defaults reproduce the current bytes
exactly, so an absent section or the shipped file changes no behaviour. `levelLimit` (byte 1)
and `mateLevelLimit` (byte 8) still come from the `levels` table through `GameData::load` and
are not configurable here. The GM `/feature set` command still changes the runtime copy; the
config is the baseline.

| Key | Bit | Default | What the client uses it for |
| --- | --- | --- | --- |
| `siege` | 0 | `Housing.SiegeCalendar` (profile) | Siege / dominion UI (`FUN_39496b20:956485`). The Lua key is read by the siege UI; the exact window is INFERRED. The default is the resolved server flag (on for `EuAuroria` / `Kr12`, off for `EuLaunch`); an explicit value wins. |
| `nations` | 6 | `Social.PlayerNations` (profile) | Player-nation UI (C# `Feature.Nations`, not in the `GetFeatureSet()` Lua table). The default is the resolved server flag (on for `Kr12`, off for the EU profiles); an explicit value wins. |
| `premium` | 4 | on | Premium / Patron HUD and labor UI (C++ reads at `x2game.dll.c:703203`, `708636`). |
| `itemSecure` | 45 | on | Item-secure / lock handling (`FUN_397cc350`, item byte flag `0x4`). |
| `secondpass` | 46 | off | Second-password entry and its opcode path (`x2game.dll.c:45293`, `617228`, `108647`). The second password is not supported; keep it off. |
| `ranking` | 36 | on | Ranking window (the `ranking` UI calls `GetFeatureSet`). |
| `slave_customize` | 48 | on | Vehicle / ship customization UI (INFERRED from the key and the customizing UI callers). |
| `bm_mileage` | 43 | on | Cash-shop mileage display (INFERRED; Korean business model). |
| `ingamecashshop` | 38 | on | In-game cash-shop open guards (`x2game.dll.c:62540`, `62632`). |
| `customsaveload` | 40 | on | Custom UI save/load (INFERRED). |
| `beautyshopBypass` | 52 | on | Beauty-salon bypass entry (customizing UI; INFERRED). |
| `ingameshopSecondpass` | 55 | off | Second-password gate on cash-shop purchases (INFERRED). Not supported; keep it off. |
| `backpackProfitShare` | 56 | on | Trade-pack sale profit share and sale mails (`systems/economy/specialty.rs`; the later note in this file). |
| `sensitiveOpeartion` | 58 | off | Sensitive-operation confirmation prompts (INFERRED; the key's spelling is the client's). |
| `taxItem` | 59 | on | Tax-item payment UI (the server also gates `TaxItem` behavior on this bit). |
| `achievement` | 63 | on | Achievement window. |
| `dwarfWarborn` | 74 | off | Dwarf / Warborn race selection. The shipped `characters` data has races 7/8 `creatable = 'f'`, so turning the bit on does not by itself make those races creatable. |
| `mailCoolTime` | 75 | on | Mail send cooldown display (INFERRED). |
| `hudAuctionButton` | 81 | on | Auction-house button in the HUD. |
| `auctionPostBuff` | 83 | on | Auction posting buff / UI (INFERRED). |
| `aaPoint` | 44 | off | AA-point amounts in price and currency displays (`x2game.dll.c:66363` sets `grade_enchant_cost` / `grade_enchant_currency`, also `633707`, `659611`, `733342`). AA points are not supported; keep it off. |
| `ExtraBits` | any | `{}` | Override map for the unnamed `flag_x_y` bits, `0..=87`. The C++ reads unnamed bits 32, 34, 41, 42, 47, 60 and 73 (purpose unresolved); 78 is the mate equipment whitelist (`PORTING.md`, mate equipment). Defaults: 32 on (plus inherited 16-19, 24-27, 35, 37, 49); 34, 41, 42, 47, 60, 73 and 78 off. |

The C#-named bits that have no `GetFeatureSet()` Lua key (`allowFamilyChanges` 1, `houseSale` 3,
`customUiButton` 39, `freeLpRaise` 53, `houseTaxPrepay` 84) have no named config switch; they
keep their current defaults and `ExtraBits` can override them like any other bit. `nations` (6)
now has the named key below because it follows the era profile.

## Server era profile (Rust only, 2026-11-08)

Trion's "Version 1.2" was a label over several builds; the owner decided on 2026-11-08 to
emulate the EU **Auroria** era by default. `World.Profile` selects the era; its derived values
are defaults, so every explicit key still wins. The server logs the active profile once at
startup. Timeline and sources: `D:/aaemu-audits/gap-audit-2026-11-02/research/trion-1.2-eu-na.md`.

| Area | `EuAuroria` (default) | `EuLaunch` | `Kr12` |
|---|---|---|---|
| Retail window | Trion EU Conquest of Auroria to the day before Secrets of Ayanad: **2014-11-04 (NA) / 2014-11-06 (EU) to 2015-03-09** (engadget 2014-11-04; mein-mmo 2014-11-06; Total ArcheAge 2015-03, "level cap 55" with Secrets of Ayanad on 2015-03-10) | Trion NA/EU launch: **2014-09-12 (head start) / 2014-09-16 (launch) to 2014-11-03**, before Auroria (gameinformer 2014-08-30; tentonhammer 2014-09-12) | XL KR 1.2 data our templates come from (`r208022`) |
| Player level cap | 50 | 50 | 55 |
| Player nations (`systems::social::nation`, NATION-1..17) | off | off | on |
| Weekly siege calendar (`Housing.SiegeCalendar`) | on | off | on |
| Auroria claims / client `FeatureSet.siege` (profile; not the calendar key) | on | off | on |
| Archeum Lodestone max HP | 4,100,000 | data 1,000,000 | data 1,000,000 |
| Defense Tower max HP (`housings` 119/169) | 1,500,000 | data 600,000 | data 600,000 |
| Override keys | `Housing.SiegeCalendar` (calendar only), `Housing.LodestoneMaxHp`, `Housing.DefenseTowerMaxHp`, `FeatureSet.siege` (client bit), `Social.PlayerNations`, `FeatureSet.nations` | same | same |

* **Level cap.** `ExperienceManager.PlayerLevelCap` is no longer a constant: `GameData::load`
  passes `ServerProfile::player_level_cap()` to `ExperienceTable::from_templates`. A character
  saved above a lowered cap keeps its level; a gain is trimmed at that character's level, the
  same as at the cap, and `level_from_exp` never returns a level above the cap even though the
  1.2 `levels` table has **101 rows** (the C# search range could run past the cap; from level 1
  it could return level 56 at cap 55). The client `FeatureSet` level limit (byte 1) follows the
  table. Mate cap stays `MATE_LEVEL_CAP = 50`.
* **Partial exp above the cap is trimmed.** On load (`Character.ValidateAndFixExpAndLevel` ->
  `validate_and_fix`) and on every exp gain the character's partial progress beyond the cap is
  trimmed to the current level's total (save stores that result); the level itself is never lowered. Switching the profile back to a
  higher cap therefore does not restore the trimmed progress.
* **Open risk (client level limit with an above-cap character).** How the 1.2 client renders the
  exp bar, the level number and tooltips when its `FeatureSet` level limit is 50 and the
  character's level is above 50 is **UNVERIFIED**; it needs a live original-client check. The
  server keeps the level and trims the exp as described above, so the client does receive a
  level above its configured limit.
* **Nations.** No public Trion 1.2 source shows player-nation founding (research §1.4/§3);
  the research treats it as unavailable in 1.2. The server gate is
  `Social.PlayerNations` (profile default), and the client `FeatureSet.nations` bit follows the
  same value. A refused founding uses the existing precondition refusal (`FACTION_SEPARATE_RIGHTS`
  40 for the skill / GM path), not a new error id. Existing nations loaded from the database are
  not deleted by the switch (a profile change is not a migration).
* **Sieges and claims are decoupled (2026-11-08 follow-up).** `Housing.SiegeCalendar` drives
  only the inferred weekly siege calendar; its default follows the profile (on for `EuAuroria` /
  `Kr12`, off for `EuLaunch`) and an explicit value wins. Auroria territory claims and the
  client `FeatureSet.siege` bit follow the **profile** (`World.Profile`), not the calendar key:
  an operator under `EuAuroria` with `Housing.SiegeCalendar=false` (GM-scheduled sieges only)
  still claims territories and still shows the siege UI. An explicit `FeatureSet.siege` value
  wins for the client bit; there is no separate claims key. A refused claim uses the existing
  `DOMINION_CLOSED` 662 (the "not a claimable region" refusal). Trion's launch era had no
  sieges or Auroria; Auroria opened on 2014-11-04 (NA) / 2014-11-06 (EU) (research §2/§4). The
  Trion siege itself was a Lodestone system; our KR dominion siege with camp lots is a **server
  policy approximation** of it, not a claim that the two rules match.
* **UPGRADE (2026-11-08 follow-up).** A configuration without `Housing.SiegeCalendar` now gets
  the weekly siege calendar from the default profile (`EuAuroria`: on). Before this change the
  key defaulted to off (explicit GM schedules only). The calendar rules are INFERRED from the
  1.2 tables with corroborated times; to keep the old calendar behaviour set
  `Housing.SiegeCalendar=false`. That no longer disables Auroria claims or the client siege UI.
* **Castle structure HP follows the era (2026-11-09, r6-siege-video).** The video audit of
  the NA 2014-11-30 Heedmar siege (`D:/aaemu-audits/video-evidence/siege-2014-11-30-ollo.md`,
  grade C) shows a 4,100,000/4,100,000 Archeum Lodestone HUD bar and a "Nuian Defense Tower"
  target frame at 1,500,000 HP; the `r208022` data has `housings.hp` 1,000,000 (rows 139,
  184-192, 271, 272) and 600,000 (119/169) and the target client's own
  `D:/AA1.2/game/db/compact.sqlite3` agrees. The Nov 2014 change is a Trion Auroria server
  balance value, not a formula output (the period r/archeage thread describes walls 400k ->
  1.2M and the lodestone 1M -> 4M), so it is modelled as an era-profile value:
  `ServerProfile::lodestone_max_hp` / `defense_tower_max_hp` (EuAuroria 4,100,000 / 1,500,000,
  EuLaunch and Kr12 keep the data value) with explicit `Housing.LodestoneMaxHp` /
  `Housing.DefenseTowerMaxHp` overrides, applied in `housing::manager::structure_max_hp`
  (lodestone = `guard_tower_setting_id != 0`; defense tower = `housings` 119/169). A row is
  scaled proportionally on load against the maximum it was saved under
  (`loaded_house_hp` / `scale_loaded_structure_hp`); the basis is persisted in the new
  `housings.hp_basis` column (`SQL/updates/2026-11-09_aaemu_game_housings_hp_basis.sql`; NULL
  means the template HP). A full tower stays full, a damaged one keeps its fraction, and a
  reload does not scale again: a tower stored at 600,000 loads at 1,500,000 under `EuAuroria`
  and at 600,000 under `Kr12`; switching profiles converts by the old/new basis factor in both
  directions. (Without the basis, the first version healed a saved 750,000 tower to
  1,500,000 on every restart.) The **Tower Cannon** needed no change: slave 100's formula 48
  gives 4,796 at level 50 and the 1.2 `unit_modifiers` row `(Slave, 100, MaxHealth, +100000)`
  brings it to **104,796**, matching the video's target frame. Tests:
  `era_profiles_scale_castle_structures...`,
  `era_scaled_structures_scale_their_persisted_hp_on_load`,
  `era_scaled_hp_scales_once_and_survives_save_reload`,
  `switching_era_profiles_converts_the_stored_hp_both_ways`, gated
  `real_compact_castle_structure_hp_matches_the_recorded_data_values` and gated
  `real_compact_tower_cannon_max_hp_is_the_formula_plus_the_authored_bonus`.
* **Castle walls stay at the `r208022` data values on purpose (2026-11-09, r6-siege-video).**
  Wall templates 121-130 and 158-167 keep 400,000 HP (gates 1,000,000). The only source for a
  1.2M wall value is the press/forum description of the Nov 2014 change ("Wall HP went from
  400k to 1.2M"); no video shows a wall's HP bar and no gate value is reported. The press
  value (1.2M) is recorded here for reference only and is **not** applied.
* **PvP deaths have no death penalty (2026-11-09, r6-siege-video).** The same video's siege
  death dialog reads "You lost 0 XP" (§2.8, grade C); the 1.2 client has the `ui_texts` "No XP
  lost." and the period rules list exp/durability losses only for PvE (fandom Combat "Death by
  Player" has no negative effects; Ars Technica 2014-09 "If you die in PvP, you lose 0 XP";
  mmogah "you don't lose experience or item durability"; the 1.2 patch notes list exp-loss
  exemptions only for Guard NPCs, summoned NPCs and Origin mobs). `Character.DoDie` asks
  `skills::combat::death_penalty_applies`: false when the killer is another player or a mate /
  slave owned by one, in addition to the existing `no_penalty` NPC rule; a self-inflicted death
  (fall damage passes the victim as attacker) still pays the PvE penalty. The C# applied the
  exp loss unconditionally on the PvP path (deviation, deliberate). Tests:
  `player_kills_lose_no_experience_or_durability` (also pins the `character_death_losses`
  durability return), `character_death_by_another_player_loses_no_experience`.
* **Not modelled (owner decision):** Patron / free-player rules, APEX and labor potions are out
  of scope for the profile.

## Live testing (phase 5): fixes and deliberate deviations

Found by running the servers against MySQL, the real `compact.sqlite3` / `game_pak` and the
`aaemu-testclient` bot, and by diffing the S->C traffic of the same scenarios against the C#
servers (see `rust/LIVE-TESTING.md`). Fixed porting bugs are in the git history (commit
"live-test porting fixes"); these are the remaining, deliberate differences:

* Game DB sessions run at `READ COMMITTED` (`after_connect`). Under the server default
  (`REPEATABLE READ`) concurrent character saves (`DELETE FROM character_active_buffs WHERE
  character_id = ?` + `INSERT`) deadlock on next-key gap locks (seen with 50 bots logging out at
  once; the C# has the same pattern but rarely overlaps since its saves are slower).
* Periodic world ticks (active-region spawning, regen, area triggers, gimmicks, quest spheres,
  instances, vehicles) run on the blocking pool (`systems::run_blocking`), one run at a time per
  ticker, instead of on the async workers.
* The heightmap cell of an NPC is loaded before `WorldInstance::spawn` takes the grid lock
  (the C# loads it lazily while writing `SCUnitStatePacket`); same result, no multi-second
  stall of the whole world grid on the first visit to an area.
* Positions are saved with full `f32` precision. The C# (MySql.Data float parameters) stores
  them with ~6 significant digits (`15578.041` -> `15578.0`), so a re-login spawns a few cm away
  from where the C# would put it.
* Vehicle / ship / transfer velocity shorts use the client's scale: `m/s = wire * 30 / 32767`
  (1.2 `x2game.dll` type 2/3/5 movement readers; `_DAT_399abe78` = 1/32767,
  `_DAT_3999f7c0` = 30.0), so 1 m/s is `32767/30` (~1092.2) wire units. The C# wrote ship
  `VelX/Y/Z` with `const int velMultiplier = 2048` (`PhysicsManager.SendUpdatedMovementData`)
  and `TransferData` read/wrote its velocity with a 50 factor (wire *655.3). The Rust ship
  encoder and `TransferMove` now use the 30/32767 scale; positions and rotations are unchanged.
  A live capture of the original client on 2026-10-29 measured 1/1092 on its own `CSMoveUnit`
  type 2 bodies (ratio 0.994 at 1/1092 vs 1.864 at 1/2048, n=640).
* `X2EnterWorldResponsePacket.token` (= game session id) is a counter (see above), so stream
  `CTJoin` tokens are small integers rather than `RemoteEndPoint.GetHashCode()` values.
* A character created in this session keeps its in-memory state until it is selected (as C#
  keeps the `Character` object in `Connection.Characters`): its gear buffs (indices 2..8) exist
  before the first select and are not announced again. A character loaded from the DB on a
  later session gets fresh buffs on select in both servers.
* `UpdateGearBonuses` is ported for characters, NPCs, mates and slaves (the "Stats" bullet
  above predates it).
* A client that takes its world cookie (`CAEnterWorld`) but never connects to the game server
  leaves `EnterWorldManager._accounts[connectionId]` behind, and the login server leases the
  same (lowest free) connection id to the next client. C# then throws in `AddAccount` and stops
  answering the login server (every later enter-world is denied, the world shows as inactive).
  Rust replaces the stale entry and answers.
* Regen stats follow the retail client (the real client's stat oracle, 2026-10-18:
  levels 1-55 plus the persistent-regen buffs 5872 / 4698). `HealthRegen` / `ManaRegen` are
  `trunc(base formula + bonuses) + Persistent(raw) / 5` (client 47.4000015 at L55 with buff
  4698). The C# `+ Spi / 10` in `ManaRegen` is removed: the client never has it (pre-fix server
  47 vs client 37 at L50, 23 / 21 at L10, 62 / 47 at L55). `PersistentManaRegen` divides the
  bonus-inclusive value by 5 like `PersistentHpRegen` (pre-fix a +5 modifier read 5 instead of
  1); both keep their fraction, and `/getattribute` prints the four regen kinds as floats. The
  out-of-battle `RegenTick` (`regen.rs:141`) adds the composed value truncated; the in-battle /
  post-cast branches are unchanged. Added the missing `IncomingSiegeDamageMul` getter
  (attribute used by 9 buff `unit_modifiers` rows).
* Quest evaluations requested while a character is selected but not yet in game are parked
  until its `CSNotifyInGameCompletedPacket` (`systems::quests::on_in_game_completed`). The C#
  `QuestManager.EnqueueEvaluation` ran `QuestManagerRunQueueTask` 1 ms after `Quests.Load`, so
  `RunCurrentStep` sent `SCQuestContextUpdatedPacket` before `CSSpawnCharacterPacket`; the 1.2
  client crashes (access violation) on that packet when the quest's component carries
  `ai_command_set_id` / `play_cinema_before_bubble` (quest 63, component 8101). The same
  updates now arrive right after the character is in the world. Entries of a character that
  leaves first are dropped (`on_leave_world`); characters already in the world (world switches)
  are unaffected. Quest timers expiring before the spawn are not deferred.

* `SusManager` speed analysis: baselines carry their time, instance and a teleport counter
  (`Character::teleport_seq`, bumped by `Character::set_disabled_set_position(true)`, the C#
  `GameObject.DisabledSetPosition` setter that called `ResetAnalyzePlayerDeltaMovement`). The
  speed is taken over the real time since the baseline (at least the nominal 5 s), and a
  baseline older than 10 s, from another instance or another teleport counter is re-seeded
  instead of judged. The C# kept baselines fresh differently: `FinalizeTransform` propagated to
  the child transforms of a mount / slave (riders), and the setter reset them on teleports;
  the Rust move handler only finalizes the packet's target, so riders and teleported players
  were reported as 40 to 3000 m/s runs (and re-summoned mates, whose database id outlives the
  spawn, as pets). The riding / slave-bound / `disabled_set_position` early returns of the move
  handler, mate spawn / delete and leaving the world reset the baseline explicitly
  (`CharacterMates.CreateNewMate` reset it too). Thresholds (25 / 27.5 m/s) are unchanged.
* `CSMoveUnitPacket` for an object the world no longer holds (a mount the server just
  despawned: summon toggle, dungeon entry, death) logs `Invalid target` at debug, not warn: the
  client keeps sending a few moves after the despawn.
* `Character.ChangeLabor` inside an item operation (`ItemTxn`: labor-costing doodad placement, auction
  mail money, trade-pack sale) queues its `AddExp` as `ItemFx::Exp` instead of calling it inline. In the C#
  `AddExp` runs inline (exp packet, level broadcast, `DoOnLevelUpEvents`, in that order) before the labor
  change and its packet; in Rust `AddExp` ends in a quest operation that takes the economy ops lock, which
  the transaction already holds on that thread (not re-entrant: the live server froze on 2026-10-16). The
  effect now runs in `fx::flush` after the lock is released, still before the labor packet, and after the
  item task packets queued earlier (the C# sends those inline first too). `quests::events::with_quests`
  logs an error and returns `None` when the thread already holds the ops lock instead of hanging.

* `CharacterManager.Create` (starter action bar, WP-N1, 2026-11-06): the C# assigned every supply sequentially from slot 10 in `character_supplies` order, so the teleport scroll and the two emote items landed at 10 / 11 / 12 while the 1.2 client's `default_action_bar_actions` authors 18791 -> 10, 18792 -> 11, 4045 -> 12. The Rust loads that table (`data/character_templates.rs`) and places the granted supplies at the authored slots; supplies without a row take the next free slot from 13 on (417 -> 13 in the shipped data), so class supplies no longer share the default bar. The client also loads the table itself (`dll.c:1688905`); its use there is UNKNOWN (likely the reset action-bar button), so matching the authored slots is the safe reading. `default_inventory_tabs` / `default_inventory_tab_groups` are client-only: the 1.2 client loads both itself (`dll.c:1475546-1475711`) and the server has no tab packet or persisted tab state to apply them to, so they stay unread server-side.
* `CharacterManager.Create` saved only the `characters` row (`SaveDirectlyToDatabase`); the starting supplies (`item_containers` / `items`) waited for the next `SaveManager` run or the departure save. A server stop or crash in that window kept the character but lost its items, and the next select built empty containers. `db_character::save_new_character_locked` now writes the new character's dirty containers and items in the same transaction (`ItemManager::prepare_save_for_owner`). Creation holds the save transaction lock from before inventory registration through commit or cleanup; setup errors and failed saves discard the rejected containers/items before releasing the name and character ID, without consuming other owners' changes or queued deletions. Canceled item deletions are recorded at each starter allocation, including IDs another owner releases after setup begins, and restored on rollback. Suite `rust/verify/newchar-items.vtest` uses a fresh character on every run, including reruns of its provisioned account.

Parity status of the scripted scenarios: `login`, `lobby` and `full` (fresh character) produce
the same S->C traffic as the C# servers after id / time normalisation (0 findings); a second
`full` round with the existing character only differs in the saved position precision above
and in NPC wandering / buff remaining times.

## C# bugs noticed (mirrored, not fixed)

* systems::social: `AchievementGameData` reads the `'t'` / `'f'` text booleans of compact.sqlite3 with
  `GetBoolean(column)` (`GetInt64 != 0`), so `is_active`, `is_hidden`, `complete_or` and
  `or_unit_reqs` always load as `false`.
* systems::social: `AskRiskyTeam` makes the acting unit (the kicker) leave its chat channels;
  a member joining an existing team never joins the party / raid
  chat (but can still talk into it); `IsMarked` compares mark object ids with character ids;
  `ExpeditionManager.ChangeOwner` does not update `OwnerName`; `ReplyInvite` does not check an
  existing guild; disbanded guilds stay in
  the list; family invitations always carry family id 1; hard disconnects keep the family member's
  character set; `GetTrialChat` throws when the faction has no court and the player is in no trial.

* `WorldManager.RemoveVisibleObject` calls `RemoveFromCharacters` on the object's own region and
  then again for every neighbour, which includes that region: players in the same sector receive
  `SCUnitsRemovedPacket` (or `SCDoodadRemovedPacket`, ...) twice (and the leaving player twice the
  list of its sector).
* `WorldManager.AddVisibleObject` on a first placement shows the children through
  `GameObject.AddVisibleObject` and then places each region-less child itself, whose own
  `AddToCharacters` shows it again: a child is sent twice.
* `Region.RemoveFromCharacters` for a character: gimmicks are units, so their ids are in both
  `SCUnitsRemovedPacket` and `SCGimmicksRemovedPacket`.
* `House.RemoveVisibleObject`: the `SCDoodadsRemovedPacket` chunk offset is
  `i * MaxCountPerPacket` with `i` already stepping by `MaxCountPerPacket`, so with more than 400
  attached doodads the second chunk throws; only the first chunk is sent (an error is logged).
* `Region.AddObject` / `RemoveObject` swap-remove: an object that leaves a sector moves the last
  entry into its slot, which changes the order other players later receive the sector in.
* `new SCFactionListPacket()` / `new SCFactionRelationListPacket()` (empty lists) use the
  hard-coded type ids 0x007 / 0x008, which are `SCTrionConfigPacket` / `SCFactionListPacket` in
  the 1.2 table.
* `Character.Save` writes the rotation columns crosswise (`roll` <- Rotation.Z,
  `yaw` <- Rotation.X), consistently with `Load`, so the round trip works. The former omitted
  `reported_as_bot_count` column is fixed in the feature audit below.
* `CharacterSkills.Load` re-learns stored skills through `AddSkill(id)`, re-checking the ability
  tree and skill points and recomputing the level (the stored level is ignored).
* `CharacterManager.Load`: `character_supplies` creates `_abilityItems` entries that
  `character_equip_packs` then `Add`s again (throws for an ability with both).
* `FriendMananger.Load` keys all friends by `id` although the table key is `(id, owner)`.
* `SCUnitStatePacket.Write` adds buffs 8000011 / 8000012 to the character ("Wrong place"), and an
  exception inside any `Write` (e.g. no `VisualOptions.Stp`) truncates the packet, because
  `PacketStream.Write(PacketMarshaler)` swallows it. Both reproduced.
* `SCUnitMovementsPacket` writes no move-type byte per entry (unlike `SCOneUnitMovementPacket`).
* Hard disconnects do not leave chat channels (only `LeaveWorldTask` does).
* systems::quests: `QuestActObjZoneMonsterHunt` (non-score) needs `count > Count` but the
  objective is capped at `Count`: never completes.
* `systems::npcs`: the "spawn all" schedule check is inverted; `DoRandomSpawn` leaks an obj id and spawns twice; a doodad despawn may release its obj id twice; gimmick spawn rotations are not converted from degrees; scheduled doodad spawns stack cron tasks; the player-in-radius check scans every player; NPCs are not despawned when players leave.
* `systems::npcs`: dated game schedules compare `StartTime` / `EndTime` (usually 0) instead of the start / end hours (a one-day event counts as ended at 00:00); the cron builder tests the captured day of week instead of its parameter, and undated schedules produce day / month 0 (rejected by the cron parser); `VendorExist` reads the `id` column; deleting a portal sends no packet; `NotifySubZone` re-sends the portal book once per recall portal of the sub zone; the exit portal's yaw is the book rotation in degrees.
* `systems::npcs`: corrected coin double credit and failed trade-pack acquisition; `BuyFish` uses the checked economy sale. Phase operations reserve live object identity, honor interaction counts, enter functions on time-of-day changes, and cancel superseded timers. Saves/deletes use a FIFO writer drained by the save lifecycle; partial interaction counts use the doodad-interactions SQL update. Scheduled spawns recreate deleted doodads, deduplicate cron callbacks, and retain original world identity. Logic links/families route matching true/false pulses with per-object state and cycle guards; client-created wires require ownership and reach. Immediate gates are supported; delay descriptors 2/3 need reference behavior. `SpawnGimmick` creates the authored object with offsets/faction/kinematic motion; `SpawnMgmt` requires explicit authored `SpawnGroupId` membership. Prefab flow graphs are client-owned. `CheckGrowth` still runs funcs with `nextPhase = 0`; child metadata templates (e.g. `DoodadFuncBuyFishItem`) remain in the phase dictionary.
* systems::economy: `CSWithdrawMoneyPacket` reports `ItemTaskType.WithdrawMoney` (84; the C#
  handler called `ChangeMoney` without a task type, so it defaulted to `DepositMoney`) and
  `ExpandSlot` sends its `MoneyChange(-price)` task with the expansion (the C# collected the task
  into a list it never sent, in both the economy and the services copies).
* systems::economy: the two "is not welcome in this container" rejections of `SplitOrMoveItemEx`
  (an item the target / source container refuses, e.g. armor sent to a wrong equipment slot) are
  logged at `warn` (C#: `Error`) and include the whole request (from / to type, slot, item ids,
  owner). The client is the source of the request and `CanAccept` already warns with the cause.
* systems::economy: `BindType.HasFlag(...)` on the plain `ItemBindType` enum is a bit test
  (`BindOnUnpack` matches 4..7, `BindOnEquip` matches 3 and 7); mirrored.
* systems::economy: loot dice are won by the highest roll. The C# `FinishRolling` took the lowest
  positive roll as the threshold and then picked a random winner among all rollers `>=` that
  threshold, i.e. uniformly among every positive roller whatever the values. Only players the
  dice was offered to (the loot's eligible players) may roll, and only once; a repeated or
  outsider roll is ignored (the C# overwrote the roll). The 1.2 client presents
  the roll as an item bid and only reports values: `ui_texts` 4305 `loot_dice_roll` = "Roll"
  (입찰), 4318 `loot_dice_title` = "Item Bid" (아이템 입찰), 2412 `msg_dice_value` = "$1 rolled a
  $2 on the dice. (1-$3)"; its `loot_dice.alb` bytecode only calls `X2Loot.DoDiceAction` /
  `RequestNextDiceItem`, so the winner is the server's decision and a bid over 1-100 is won by
  the highest value.
* systems::economy: taking loot releases the item id it allocates for the grant checks once
  `AcquireDefaultItem` created its own item (the C# leaked one id per non-coin, non-trade-pack
  loot grant).
* systems::economy: `AddOrMoveExistingItem` releases a stack that is merged into another
  container's stack (the C# removed it from the source without releasing it and kept it as a
  loose item, so its row stayed and a mail attachment merged into a bag stack was saved as an
  orphan `slot_type Mail` row; the same fix as the fork's C# `47a3a9e77`). The mail acquired /
  consumed event uses the slot type captured before the move (the C# read the overwritten
  `item.SlotType`, so moving an item into a mail container raised no quest event). A new item created straight into
  a mail container (no source container) raises no consumed event.
* systems::economy: `ExpandExpert` adds the vocation cost; craft / backpack / weapon booleans read as numbers are
  false for `'t'` texts; `Specialty.VendorExist` reads the `id` column.
* systems::economy: `HDps` uses the magic DPS grade value; fish conversion only uses the first
  pack. (`ItemProc` no longer mirrors the C# non-firing behaviour; see the item-procs deviation
  above.)
* systems::economy: auction — original cancellation-copy and missing balance checks
  are corrected in the quest/mail audit below.
* systems::economy: `ItemManager.LoadUserItems` deletes the row of a loaded stack that was merged
  into an existing stack (the C# kept it, so the same units were loaded and merged again on every
  start; related to upstream issue #1189, "sold items reappear after relogin"). The id is freed
  and the row is removed by the next save.
* systems::economy: `SplitOrMoveItemEx` gives every item the owner of the container that holds it
  after the operation (a split item belongs to the target container, and a moved / swapped item
  to its new holder; the C# assigned the *source* owner to the moved source item and the
  *target* owner wherever the target ended up) and the target item's leave event names its
  previous target slot (`toSlot`; the C# passed `fromSlot`). The split item used to keep owner 0
  and could be lost on relogin (upstream issue #412).
* systems::economy: mail — opening a mailbox re-sends a "got mail" per unread received
  mail; FinalizeForBidFail still uses AucOffFail; receiver lookup is case-sensitive.
  The old return failure and sender unread-count bug are corrected below.
* `systems::instances`: `RegisterIndunEvents` runs in the `Dungeon` constructor and again in the loader, so every indun event handler is subscribed twice; a dungeon kill raises `OnUnitKilled` (and the victim's `OnDeath`) in `PostUpdateCurrentHp` and again in `DoDie` — NPC-killed action chains run four times; `IndunEventDoodadSpawneds.UnSubscribe` adds the handler (`+=`); `AreaClearTick` stops at the first cleared room (`return`); a new dungeon verifies the requirements twice (the entry item is consumed twice) and the creator gets `SCProcessingInstancePacket` twice; `Transform.CloneDetached(character)` / `MainWorldPosition.Clone()` fire `OnZoneChange(0, zone)` (the instance zone chat is never left); `ApplyWorldSpawnPosition` stores the spawn rotation in degrees; leaving an instance sends the (radian) return rotation through `DegToRad` again and `SCLoadInstancePacket` names `MainWorldPosition.WorldId`; `DestroyDungeon` removes the players from the instance without moving them out; `InstantGame.RemovePlayer` looks the corps lists up by 0 / 1 instead of the faction ids (never cleaned); `AddScore` always sends `Corps1` / `score2 = 0`; the kill packet's victim count is the victim corps' kills; entering a battlefield set the corps faction through `Character.SetFaction`, which kicked guild members of another mother faction from their guild and party (no longer mirrored: the corps faction is temporary, see the RPA-09 deviation above); offline queue applicants are never pruned (`CharObj` is never null); `RemoveGame` leaves the game in the queue list; battlefields.json loops stop (`return`) at the first orphan row.
* systems::housing (2026-10-31, placement validation): the server now applies the original
  1.2 client's builder checks to `CSCreateHouse` (`ops::build` -> `placement::validate_house`),
  the only player path; GM / preset / server-spawned houses (`manager::create`, the test-only
  `TestHouse` fixture, fort and DB loading) do not run them. Footprint: `n =
  int((garden_radius*2 + 3) * 0.25)` (`FUN_39897170`), the grid origin snapped to 4 m
  (`FUN_39326980`), `n` x `n` cells of 4 m, every cell's four corners in one housing polygon
  (`FUN_39331f70`/`FUN_39331ba0`) -> `0x70`; `n >= 12` yields no footprint (the client logs
  "house too big", `FUN_39897170`). Category check for categories 1, 7-13, 15-18
  (`FUN_39897070`): no area at the centre or a group without the category -> `0x244`
  (`FUN_39897280`). Houses: the new snapped grid square vs each existing mode-2 house's own
  snapped axis-aligned `4n` box, strict inequalities as in `FUN_39331f30` via
  `FUN_393349e0` -> `0x71`; `rot_z` is not used for mode-2 houses. Units/doodads: CryPhysics
  overlap of the footprint box with entity mask `0x1f` (`FUN_39181170` called by
  `FUN_39335860`), ignoring houses, the placing character / its own mount, and every doodad
  attached to an existing house (the client's skip list) -> `0x72`. `max_construct_count`
  (DB column; the client decompile only carries the `0x2fe` text, no producer) is enforced
  per account, group and category. The check and the insert/spawn are one critical section
  under `WorldInstance::placement_guard`, as for planting; race test
  `two_house_builds_at_the_same_spot_race_and_one_wins`. Open deviations: terrain is not
  checked because the client samples the turned ghost model box (`FUN_39326c10` from
  `FUN_39332180`, and the rotated 3 x 3 plot grid of `FUN_39337fb0`) and the server has no
  render-model bounds; n=0 "mode-1" designs (walls, towers, gates) and existing mode-1 houses
  use the model box via `FUN_390318f0` / `FUN_39332c10`, so their house/unit/terrain checks
  stay lenient; the template flag at desc `+9` (`FUN_39897130`) that also clears a doodad from
  the overlap query is not mapped to a server field. Overlapping polygons are accepted when
  any one polygon contains all four cell corners; with CryPhysics disabled the unit/doodad
  check is skipped. Tests:
  `placement_flow_tests::area_338_captured_spot_is_refused_and_the_fitting_spot_is_accepted`,
  `..a_unit_inside_the_footprint_is_refused_with_overlap_unit`,
  `..doodads_attached_to_a_house_are_ignored_by_the_unit_overlap`,
  `..a_rotated_house_uses_the_clients_snapped_axis_aligned_box`,
  `..two_house_builds_at_the_same_spot_race_and_one_wins`; the real 1.2 polygon and
  `max_construct_count` row are checked by the gated test above.
* systems::housing (2026-11-03, build / decoration / purchase guards): the C# `Build`,
  `DecorateHouse`, `SetForSale` and `BuyHouse` had no range, height, tax or buyer eligibility
  checks (the C# TODOs "validate house by range", "validation position and some range",
  "check tax due maybe ?"). Each player entry path now applies the 1.2 checks that exist:
  - `CSCreateHouse` / `CSConstructHouseTax`: the caster must be within the placed house's
    `garden_radius + 30 m` (3-D) of the target (`FUN_39333d40`; the 1.2 constant
    `_DAT_399cf898` at `0x399cf898` is 30.0) -> `TOO_FAR_AWAY` 154. The Z must be within
    100 m of the heightmap (`WorldInstance.GetHeight`); above/below -> `house_cannot_locate_
    terrain_too_high` 115 / `..._too_low` 116. The game data has no vertical tolerance, so
    the 100 m guard only refuses clearly impossible forged packets (sea farms and the
    client's auto-Z pass). With no heightmaps installed the check is skipped. **Capture-
    pending HOUSE-2:** the client's real slope rule samples the turned ghost model box
    (`FUN_39332180` / `FUN_39337fb0`, 6 m spread) and cannot be reproduced without render-
    model bounds; **HOUSE-1:** `FUN_39333d40` adds the radius at desc `+0x40`, the server uses
    `garden_radius` (never smaller in the shipped data).
  - `CSDecorateHouse`: 30 m from the caster to the house-local target (`FUN_393a8fa0`
    refuses only `900.0 < d²`, i.e. exactly 30 m is allowed; the 1.2 table has
    `too_far_to_decorate_house` 199) -> 199; the target must stay in the house's garden
    square used by `GetHouseAtLocation` (`|x|,|y| <= garden_radius`) -> `house_cannot_
    decorate_out_of_garden` 289; the overall furniture cap -> `house_too_many_decorations` 124 (hard cap =
    `absolute_deco_limit`, the template struct `+4` / SQL column 1 compared by the client's
    `CanDecorateHouse` `0x39327C50`; `deco_limit` is only the base value its counter shows and
    is no longer a refusal; only doodads with `owner_type == HOUSING` count, so crops sharing
    the house id do not) **Deviation:** the 1.2 client adds every doodad created with the house as parent to the house's child map (`HandleDoodadCreated` `0x390382D0` -> `0x3935FC10`) and `CanDecorateHouse` `0x39327C50` compares that map's size, so crops and dropped backpacks counted as furniture client-side; `Doodad::write` sends such non-furniture house children (`client_unparented`, set by `housing::manager::mark_client_parent`) with parent 0 and world coordinates, while the server keeps them attached to the house and the `housing_deco_limit_elems` act-ability group and the `housing_deco_limit_elems` act-ability group
    cap -> `acatbility_deco_arrang_fail` 629 (the codes of the client's own
    `CanDecorateHouse`); a house still under construction / another instance ->
    `house_not_decoratable_state` 123; malformed pose / wrong parent / denied interaction ->
    `house_cannot_decorate` 122. Every refusal is logged (debug for the cheap validation
    failures, info for 124 / 629 with the count and limit). **Capture-pending HOUSE-6:** decoration surface/overlap and
    the vertical floor/ceiling window are not checked (the client's checkers are not
    decoded); the server is permissive there.
  - `CSBuyHouse`: the buyer must be within 30 m of the house (the sale window comes from the
    name plaque; 154). **Capture-pending HOUSE-5:** the exact 1.2 interaction range (the
    10.x house list uses 18 m). The buyer must pass the same account group/category rules as
    a builder (`validate_groups`: `houseless` 615, `existing_category_id` 616,
    `max_construct_count` 766), so buying cannot bypass a construction limit; the house's
    present area/group is used. **Capture-pending HOUSE-3:** retail purchase eligibility and
    faction/dominion rules (the C# had none; the client table has no purchase-specific
    eligibility text, so the construction codes are reused) and **HOUSE-4:** a buy-specific
    overdue-tax answer (none exists in the 1.2 table; the seller's is 645).
  - `CSSetHouseForSale`: a player seller is refused when not the owner (642), while the
    house is under construction (668), when the tax bill is already due (645), when the
    template is not sellable (644), for an unknown designated buyer (648), for a zero price
    (650), and when a *different* listing is already active (646; the same price/buyer stays
    idempotent). GM calls with `seller == None` (`/test_house sale`, `/test_house cancel`)
    keep the old checks and bypass the new ones; `/build_house` uses `manager::create`
    directly and bypasses placement, as do forts, preset and DB-loaded houses.
  - Tests (each mutation-checked by reverting the guard): `placement_flow_tests::
    a_build_request_from_far_away_is_refused_with_too_far_away`,
    `..a_house_z_far_above_or_below_the_terrain_is_refused`,
    `..a_purchase_from_far_away_is_refused_with_too_far_away`,
    `..a_purchase_that_would_exceed_the_accounts_group_limit_is_refused`;
    `access_tests::decoration_from_far_away_is_refused_with_too_far_to_decorate_house`,
    `..decoration_outside_the_garden_is_refused`,
    `..decoration_over_the_house_limit_reports_too_many_decorations`,
    `..delayed_tax_blocks_sale_listing`, `..an_under_construction_house_cannot_be_listed`,
    `..a_listed_house_cannot_be_relisted_at_another_price`.
* systems::housing (2026-11-05, WP-C1 `content_configs`): the sale certificate count and the
  appraisal certificate item now come from the shared `content_configs` table loaded into
  `GameData` (id 37 = copper per certificate, the client's `FUN_39897100` divisor; id 57 = the
  appraisal item, read by the client's sale window), with `COPPER_PER_CERTIFICATE` (1,000,000)
  and `APPRAISAL_CERTIFICATE` (28,085) kept as fallbacks for a missing / invalid row. The
  shipped 1.2 values equal the constants, so behaviour is unchanged; ids 38 / 39 are loaded but
  UNKNOWN (RE-2, no reader). Tests: `certificate_config_values_fall_back_to_the_constants`,
  `certificate_config_values_follow_content_configs` and the gated
  `real_content_configs_load_every_row_by_id` (53 rows, ids 37 / 57).
* systems::housing: abandoned-house removal buff 2250 now applies its original effect
  1876 through the normal damage path. C# rejected the house attacking itself, so the
  building never decayed. Only the active server-owned removal tick on that same unowned
  house bypasses the relation check; immunity and ordinary attack checks remain intact.
  The original 10 HP / 15-second tick and death cleanup are tested, including an original
  compact.sqlite3 gate. Demolition still clears ownership before the later decay death.
* systems::housing: Castle reference loading now includes guard_tower_settings, guard_tower_steps,
  siege_plans and siege_zones, preserving stage order and naive reference dates. These
  records remain read-only; persisted Dominion authority now handles claim, retained-tower
  damage, roster membership and the original five-minute engraving capture. Schedules are
  explicit GM operations using authored zone durations, with self-registration through
  `/joinsiege [zoneGroupId]` and `/leavesiege [zoneGroupId]`. The zone defaults to the caller's
  province; the scheduled guilds determine the side, and the current guild rank's JoinSiege
  policy controls enrollment. All shipped ranks allow it. Players cannot enroll another
  character or change the schedule; leaving also cancels the caller's engraving.
  `systems/housing/dominion/calendar.rs` adds the weekly period calendar from the 1.2 tables
  behind `Housing.SiegeCalendar` (default off, so the explicit GM path and its tests are
  unchanged) and `Housing.SiegeCalendarTimezone` (empty/`utc` = UTC, the default; `local` =
  server local; the default is UTC because the Western launch-era schedule lists the same
  wall-clock times as GMT, `round6-siege.md` 1, grade C). Period ids/order, boundary
  arithmetic, the five wire durations and the peace `started` = next auction start are
  EVIDENCED by the client (`FUN_3956fc70`, `FUN_3946ba60`, `FUN_39838ff0`/`FUN_39839040`,
  `FUN_39839740`, `FUN_3946be90`); weekday 0 = Sunday is INFERRED from the Sunday
  `siege_plans` weeks and corroborated (grade C) by the November 2014 Western schedule's
  Sunday sieges at the table's times (`round6-siege.md` 1). The
  plan-week -> siege-Sunday mapping is also INFERRED: a plan week starts on Sunday and the
  authored warmup/siege weekday 0 is the Sunday that ends that week (a lower weekday rolls
  forward), the only reading that keeps the authored order auction Wed -> declare Fri ->
  warmup/siege Sun; reading the same weekday 0 as the plan-week Sunday would move every siege
  one week earlier. The calendar runs auction -> declare and, without a declared attacker,
  declare -> peace
  (client "unearned_win", `FUN_392f7db0`); a declared siege follows 2 -> 3 -> 4 -> 5. Only
  the week's rotating zone pair gets a cycle; other weeks stay in peace with the next cycle's
  auction start. The three-week pair rotation is corroborated (grade C) by the November 2014
  Western schedule, whose consecutive weeks siege different pairs (`round6-siege.md` 1); the
  plan rotation is deduplicated from `siege_plans` and, after the 2014 table, extended by the
  three-week modulo policy measured from 2014-01-05 (the extrapolation stays an INFERRED
  server policy; the retail recurrence beyond the table is UNKNOWN). Explicit GM schedules
  keep their own timers and
  override the calendar for that territory. `/dominion clock <unixSeconds|off>` sets a
  non-persistent test clock for live verification of every period; it only drives the weekly
  calendar view. Explicit siege deadlines/tick transitions and the persisted state always use
  the real clock, and the command is refused while `Housing.SiegeCalendar` is disabled.
  GM scheduling with player registration is the chosen server policy for actually running a
  siege; the camps can also be posted as system lots with the GM `auction_post_siege_items`
  command (WP-SD1b). Automatic posting at the auction period (WP-SD1c) is implemented by
  `systems/housing/dominion/auction_post.rs`: on the first tick of a zone's auction period it
  posts that zone's `siege_zones.declare_item_id` as a system lot. The lot count and item ids
  come from the data (one per zone in the running rotation week; the six distinct camps in the
  shipped table), the lot is anchored at the cycle's `auction_start` with the 48 h duration
  class (the authored auction -> declare gap), the start bid is the item template's `price`
  (5,000,000 copper), there is no buyout and the seller is the system (`client_id = 0`).
  **INFERRED, capture-pending:** one lot per zone in the running pair (the six distinct camp
  items cover the three pairs), the 48 h class rather than a lot that could be cut short, the start bid and
  no-buyout choice, and that no siege-specific bid rule exists (increment, tie-break, outbid
  refund and bid eligibility stay the normal auction engine's). Posting is idempotent per
  cycle through the persisted `dominions.auction_posted_cycle` marker (a restart inside the
  auction period posts nothing twice; a failed item creation releases the marker so the next
  tick retries). The won camp item expires when the declare period ends (the cycle's
  `warmup_start`) through the item's own `expiration_time` timer. Tests:
  `auction_start_posts_one_system_lot_per_running_zone_from_the_authored_data`,
  `posting_is_idempotent_on_a_repeated_tick_and_across_a_restart`,
  `a_later_rotation_week_posts_its_own_zones_lots`,
  `a_won_camp_item_expires_when_the_declare_period_ends_and_the_timer_removes_it`,
  `a_disabled_calendar_posts_nothing`, `a_zone_without_a_camp_item_posts_nothing`, the gated
  `auction_post_real_compact_db_prices_and_auction_gap` and
  `dominion_auction_post_marker_round_trips_through_its_row` (MySQL).
  The rest of the dominion / siege work from the round-2 research (`siege-social-2.md`) is
  open: the offense ticket purchase WP-SD5 (CAPTURE-BLOCKED:
  `doodad_func_purchase_siege_tickets` has no rows) and siege hostility WP-SD7 (decision-blocked:
  no evidence shows how retail made the two sides attackable). The siege-shop period / side
  gate WP-SD4a is implemented behind `Housing.SiegeShopGate` (default off; the rule is
  INFERRED from the pack names, capture-pending).
  Known, not fixed:
  the calendar is rebuilt from the tables on every tick/snapshot (cache later); an authored
  time inside a DST gap makes that zone's view return None instead of shifting it (the shipped
  20:30..22:30 times never hit one); `view_at` returns None for the whole zone when its next
  rotation cycle cannot be built; and without WP-S2 (declaration) the calendar never enters
  warmup/siege on its own, so periods 3/4 exist only through an explicit GM schedule.
* systems::housing (2026-11-08, calendar timezone default): `Housing.SiegeCalendarTimezone`
  now defaults to **UTC** instead of the server's local offset (`""`/`utc`/`z` = UTC;
  `local` and explicit `+HH:MM` offsets are still accepted). This is a **behaviour change**
  for servers that never set the value: the territory claim window moves too, because
  `claim_window_at` uses the same offset regardless of `Housing.SiegeCalendar` (the switch
  only gates the weekly periods; the authored `open_weekday/open_hour/open_duration_hours`
  claim window always follows the configured offset). The resolved offset and whether it came
  from the configuration or the default are logged once at startup
  (`systems/housing/mod.rs::System::start`). Evidence: `round6-siege.md` 1 (grade C, Total
  ArcheAge 2014-11-20/25 lists the same wall-clock times as GMT); the original data's zone is
  not proven, so `local` remains for servers that want it. Tests:
  `timezone_parses_offsets_and_defaults_to_utc`,
  `timezone_display_and_the_config_source_are_loggable`.
* systems::housing / systems::social (WP-SD2a, 2026-11-06): the siege declaration is now
  persistent state, and the weekly calendar hands it over to the explicit siege record at the
  cycle's warmup start. `DominionState.declaration` holds the attacker expedition, the
  accepted time, the cycle's `auction_start` and the camp position; at most one declaration
  exists per zone and cycle (a second one is `DominionError::AlreadyDeclared` /
  `DeclaredByOther`, mapped by the caller to client 442/443). It is persisted in the new
  `dominions.declaration_attacker/at/cycle/hq_x/hq_y/hq_z` columns (the doodad obj id is
  runtime-only and re-bound after a restart). The declaration is cleared by cancel, by a
  capture or forced release, and when its cycle's warmup materialises it. The calendar reads
  `Calendar::view_for_declaration`: a declaration of the running cycle keeps the authored
  2 -> 3 -> 4 -> 5 walk (`FUN_392f7db0`); a declaration from another cycle changes nothing.
  At warmup the record is created with the cycle's real `warmup/siege` start and siege end
  (not `now + delay`), with the defender's authored `siege_settings` limits frozen in; the
  announcement is `SCSiegeDeclared` (offense team) followed by the phase `SCSiegeState`. A
  calendar-materialised record stays described by the calendar (the GM test clock walks its
  periods; the authority tick uses the calendar clock for it, which equals the real clock in
  production), while a GM `/dominion schedule` keeps its own timers and overrides. A
  declaration whose expedition disbands, or that is still pending when warmup/siege/peace
  passes, is dropped and announced with a zero offense team (`SCSiegeDeclared`), the
  client's "nobody declared" state (INFERRED: the client's cancel path is the zero team,
  `FUN_392f7cc0`; the disband/owner-change drop is INFERRED, low risk). On restart a
  declaration older than one cycle (7 days) is discarded. **Assumption (capture-pending):**
  the attacker `ni/nr/limit` in the declaration snapshot use the only evidenced cap, the
  defender `siege_settings` row's `num_defenders + num_reinforcements` (siege.md 1.3 says
  the attacker's values are UNKNOWN). The declaration route (camp use) is WP-SD2b; the
  cancel trigger's UI and the defense HQ offset are WP-SD2b/SD2c. Tests:
  `second_declaration_is_refused_for_self_and_other_expeditions`,
  `cancel_clears_only_the_declaring_expedition`,
  `materialisation_creates_exactly_one_siege_with_the_cycle_times`,
  `a_declaration_requires_an_owned_territory_and_never_coexists_with_a_live_siege`,
  `release_expedition_drops_a_declaration_it_attacked_or_accepted`,
  `a_declaration_of_the_running_cycle_keeps_the_warmup_and_siege_walk`,
  `calendar_snapshot_carries_a_matching_declaration_and_zero_team_after_cancel`,
  `declaration_materialises_at_warmup_and_the_calendar_walks_periods_three_to_five`,
  `dominion_declaration_round_trips_through_its_row` (MySQL),
  `a_declaration_that_cannot_belong_to_a_running_cycle_is_discarded_on_restart`.
* systems::housing / systems::skills (WP-SD2b, 2026-11-06): the Siege Camp item use now runs
  the declaration. The `SpawnDoodad(4307)` effect of skill 16918 (the six
  `siege_zones.declare_item_id` items) asks `dominion::declare::handle_camp_use` before it
  spawns; the check runs under the housing operation lock and only while
  `Housing.SiegeCalendar` is enabled (with the calendar off, the camp use keeps its old plain
  spawn, so GM-scheduled sieges are untouched). Rules, in this server's order, with the
  client's ids: zone has no owned dominion 342; the caster's expedition is the owner 343; the
  camp item is not this zone's `declare_item_id` 344; not the declare period (2) 386;
  horizontal distance to the live lodestone below `guard_tower_settings.radius_declare`
  (250/450 m, the authored value, not the old `OFFENSE_HQ_MIN_DISTANCE` constant) 345; the
  caster's expedition rank lacks the `SiegeMaster` policy 459; pirate expedition 444; the
  expedition already has an unfinished siege 639; a declaration already exists 442/443
  (`DominionError::AlreadyDeclared`/`DeclaredByOther`). On success the declaration is
  recorded, the camp is spawned and its real obj id/position are bound into the declaration
  (a second `SCSiegeDeclared`, then the phase `SCSiegeState`, carries the offense team); the
  source item is consumed by the normal cast debit. Every refusal cancels the effect
  (`effect_cancelled`), which makes the cast path refund the item — a refused declaration
  keeps the camp. A spawn failure after acceptance also rolls the declaration back and
  cancels the effect. Cancelling is `dominion undeclare [zone]` (the GM command), which clears
  the declaration and broadcasts the zero offense team; the in-game trigger is UNKNOWN
  (destroying/removing the camp is the candidate), so it is capture-pending.
  The error split is now decided from the 1.2 texts and policy labels: 459
  `SIEGE_MASTER_ONLY` (ui 2613 "only the siege commander") is the declaration's
  `siege_master` refusal, and 581 `SIEGE_NO_RIGHTS_IN_EXPEDITION` (ui 4661 "you have no
  siege-participation rights in the expedition") is the roster join's `join_siege` refusal
  (ui 4656 "can join siege"); both player and GM roster-registration paths use
  `dominion::rights::join_policy_allows` (`/joinsiege` and `/dominion register`), so no path
  can add a member whose rank forbids joining. **Threshold sources:** 625 (resurrection
  point, about 100 m) and 633 (houses / farmland, about 10 m) follow GameMeca 2013-04-22
  (grade C, `web-capture-substitutes.md` Part B.3); 661 (another zone) and 741 (obstacle)
  remain INFERRED, and the "siege too imminent" window behind 760 is UNKNOWN and not
  enforced. See the WP-SD2b placement-refusals bullet below. The cancellation trigger's UI
  is UNKNOWN. The check order is this server's policy. Tests:
  `camp_use_records_a_declaration_consumes_the_item_and_announces_the_team` (real
  `CSStartSkillPacket` item cast, item consumed once, cancel zero team),
  `camp_use_refusals_use_the_client_ids_and_keep_the_item` (342/343/344/386/345/459/444/639/
  442/443, item kept each time), `unrelated_stale_or_rank_ineligible_members_cannot_join`
  (581 on the join path), `the_gm_register_path_also_applies_the_join_policy`.
* systems::housing (WP-SD2b placement refusals, 2026-11-08; thresholds reworked
  2026-11-09): the four declaration placement refusals run after the authored
  `radius_declare` (345) and before the rank policy (459), each with a
  `Housing.SiegeDeclare*` threshold. GameMeca 2013-04-22 (Park Hanjin,
  https://www.gamemeca.com/view.php?gid=276862, summarised in `web-capture-substitutes.md`
  Part B.3) is a **grade C** Korean 1.2-era source: the camp may be declared anywhere
  except within "about 100 m" of a resurrection point and "about 10 m" of houses (incl.
  farmland). Its distances are approximate ("about"), so they are the defaults of 625/633,
  not exact bounds. 661 and 741 stay **INFERRED** (`round6-siege.md` "Thresholds"). The ids
  exist in the client error table (`x2game.dll.c` cases 0x271 = 625, 0x279 = 633,
  0x295 = 661, 0x2e5 = 741, 0x2ed = 749):
  * **633 `SIEGE_CANNOT_DECLARE_NEAR_HOUSE`** (ui 5020 "cannot declare near a house").
    Data: a house's `garden_radius` square, the same `GetHouseAtLocation` query the server
    uses (`house_at_location_in_world`), with `Housing.SiegeDeclareHouseMargin` metres
    beyond the plot (default **10**, GameMeca "about 10 m"; farm and garden plot house
    designs use the same square, so they are included). Farmland is additionally refused
    with `Housing.SiegeDeclareFarmMargin` metres (default **10**, same source): a public
    farm area (a `PublicFarmManager._farmZones` sub zone polygon: 966 farm, 967 ranch,
    968 nursery, 974 mount stable, 998 farm; the data the server already uses for
    common-farm planting; the margin is ring-sampled, the same INFERRED approximation as
    661) and a player-planted farm doodad (owner a character or a house; a template with a
    `DoodadFuncHouseFarm` phase func, or a doodad carrying a `farm_type`; the distance is to
    the doodad's world position). "Player-planted" is required: the crop / livestock
    templates are also the wild gathering nodes (clover 2277, azalea 2276, mushroom 453,
    rubber tree 369, cotton 1798, pearl clam 3511 and others — 8,702 of the 43,463 static
    `main_world` doodad spawns would match the template alone). Only the sectors within the
    margin are scanned. The two margins are server-configurable; `0` refuses only a spot
    inside the plot / farm area.
  * **661 `SIEGE_CANNOT_DECLARE_NEAR_OTHER_ZONE`** (ui 5159 "too close to another zone",
    INFERRED source text). Data: the authored zone at the spot (`WorldInstance.zone_id_at`
    -> `zones.group_id`) must be the besieged dominion's group; `Housing.SiegeDeclareZoneMargin`
    metres additionally refuses when a different group is within that distance (ring
    sampling of the zone grid, INFERRED approximation). Default `0` checks the spot itself
    only; the retail margin is UNKNOWN, capture-pending.
  * **625 `SIEGE_CANNOT_DECLARE_NEAR_RESURRECT_POS`** (ui 4837 text "cannot declare near a
    peace area"; the key name and GameMeca say resurrection point, which is the reading
    used here). Data source: the `Data/Portal/respawns.json` portals
    (`PortalTables::respawns`, loaded by `PortalManager.Load`; the server's resurrection
    points, `portal::closest_return_portal` / `respawn_zone`). Their zone is always computed
    against `main_world` (`portal::respawn_zone`), so the rule applies in the main world
    only. `Housing.SiegeDeclareResurrectMargin` metres (default **100**, GameMeca "about
    100 m", horizontal distance to the nearest point) refuses when
    `Housing.SiegeDeclareResurrectCheck` is on (default **true**). An empty respawn table
    refuses nothing. Only these static respawn portals count: dynamic points (territory
    statues, the siege camp / HQs and other runtime objects) are not resurrection points
    for this rule. The previous peace-area reading (zone conflict `Peace` state, default
    off) is removed: the source and the key name name a resurrection point, not a zone
    state.
  * **741 `SIEGE_CANNOT_DECLARE_OBSTACLE`** (ui 5783 "there are obstacles nearby", INFERRED
    source text). Data: the house builder's collision query (`collision::box_obstacles`,
    mask 0x1f) at the spot; houses are the 633 rule, vegetation / brush / the caster and its
    own mount do not refuse, and an incomplete (saturated) query counts as blocked (unlike the house
    builder, `housing/placement.rs`, which skips that marker; for a 0.35 m box the difference is
    theoretical). `Housing.SiegeDeclareObstacleRadius`
    defaults to the player capsule radius 0.35 m (INFERRED; the retail radius is UNKNOWN,
    capture-pending).
  * **D1a 749 `LOCATE_INVALID_DOMINION_AREA`** for item 29800 "Blueprint: Territory
    Statue" is unchanged: it is implemented in `systems::npcs::doodad::placement`
    (`FUN_393a8fa0` step 7, `fort::state_at`, `re-placement-fakeuse.md` D1a) and covered by
    the `CSCreateDoodadPacket` row above and the `territory_statue_*` tests; this change
    adds no second gate.
  Tests: `camp_placement_refuses_a_house_plot_and_honours_the_house_margin`,
  `camp_placement_refuses_farm_plots_farm_doodads_and_public_farm_areas`,
  `camp_placement_refuses_resurrection_points_and_honours_the_margin`,
  `camp_placement_refuses_another_zone_group_and_honours_the_zone_margin`,
  `camp_placement_refuses_obstacles_and_honours_the_radius` (each drives the real
  `CSStartSkillPacket` camp cast, checks the 9/11 m and 99/101 m boundaries and fails
  without its check), `siege_declare_placement_thresholds_default_and_bind` (config
  defaults and overrides).
  The "siege too imminent" window behind 760 remains UNKNOWN and is not enforced.
* systems::housing (WP-SD2c, 2026-11-06): the headquarters follow the declaration. The camp
  doodad bound by SD2b is the offense HQ from acceptance: `Headquarters::bind_declaration`
  stores it against the declaration (no siege record yet) and `bind_materialised` stamps it
  with the siege when the calendar materialises the record, so the offense teleport (17046)
  and the `SCSiegeState` team position use the real camp. A restart re-binds the retained
  4307 nearest the persisted camp position (`restore_declarations`, five metres); the GM
  bind of a retained doodad still exists for explicit schedules. The defender HQ (3410) is
  spawned at the warmup at the lodestone plus `Housing.DefenseHqOffset` (three floats); the
  retail spawn rule is UNKNOWN, so the default `[0, 0, 0]` disables the spawn and keeps the
  existing GM-bound behaviour. The authored 10-minute camp lifetime stays owned by the WP-S3
  phase chain, as before. Tests: `declaration_camp_becomes_the_offense_hq_and_survives_a_rebind`,
  `a_materialised_declaration_keeps_the_offense_hq_and_the_teleport_works` (offense teleport
  packet at the camp position after materialisation),
  `defense_hq_spawns_only_with_a_configured_lodestone_offset`.
* systems::housing / systems::skills (WP-SD3, 2026-11-06): the 13 `siege_items` rows gate item
  casts in the one client item-cast branch (`skills/handlers.rs`, after the item template is
  resolved from the owned stack and before `skill::use_skill`, so a refusal keeps the item and
  answers only `SCErrorMsg` 431). The client loads the table and shows the row's `USAGE` text
  for 431, so this is defence in depth for forged casts. The mapping is INFERRED from the row
  shapes: a `true` flag means the situation allows the use; situations are the current period
  (`period::current_period`: no dominion/declare/peace), `radius_siege` around the lodestone
  (inside/outside the siege area) and `radius_offense_hq` around the registered side's bound
  HQ (the defense radius is UNKNOWN and uses the offense value). Rows: 26397/26424/26428 are
  allowed only inside the siege circle during the siege, 27300/27302 only inside the offense
  HQ circle during the siege, and the escape/Eanad scrolls 34018/34021/34060-34065 everywhere
  except the siege/HQ circles during warmup and siege. The gate is active only when
  `Housing.SiegeCalendar` is on or the zone has an explicit siege; outside every dominion
  zone the `outside_siege_zone` flag decides (a tank mine is unusable there, an escape scroll
  is usable). Period 1 (auction) has no authored flag and is not gated. Unlisted items are
  never refused. The table is loaded beside `SiegeZone` in `housing/data.rs` (a missing table
  disables the gate); the decisions live in `dominion/siege_items.rs`. Tests:
  `the_three_authored_classes_allow_exactly_their_situations`,
  `siege_items_are_refused_outside_their_authored_situations_and_kept` (real item casts in
  period 2 and 4, inside/outside the circle, item kept on refusal), the loader assertion in
  `castle_references_preserve_stage_order_dates_and_zero_open_duration`, and the gated
  `housing_real_compact_db` row-count check (13 rows).
* systems::housing / systems::npcs (siege-time shop gate, WP-SD4a, 2026-11-06): with
  `Housing.SiegeShopGate` on (default off, so both packs keep selling as before), merchant
  packs 224 / 225 are sold only during the warmup or siege period (3 / 4) of the NPC's
  territory and only to a character on the matching side of that siege's roster: pack 224 to
  defenders, 225 to attackers. The period is the one `social::dominion::snapshot` shows for
  the territory (calendar or explicit siege), and the roster is the persisted participant
  list. Refusals are silent like the other `CSBuyItems` catalog checks; an allowed line is
  still charged in honor through the item's `honor_price`. A pack that matches but is not
  one of the two named packs is never gated, so an item also offered by an ordinary pack
  stays on sale. The rule is **INFERRED** from the two `merchant_packs` names ("defense-side
  goods sold during a siege" / "siege-side goods sold during a siege") only: the retail
  period/side restriction is capture-pending, as is which pack belongs to which side (224 =
  defense, 225 = attack is the name reading). The gate lives in the real NPC catalog bridge
  (`npcs/bridges.rs::merchant_sells`, called from `economy/store.rs`), the switch in
  `systems/housing/config.rs`. Tests:
  `siege_shop_gate_sells_224_to_defenders_and_225_to_attackers_in_warmup_and_siege`,
  `siege_shop_gate_refuses_registered_sides_outside_warmup_and_siege`,
  `siege_shop_gate_refuses_non_participants_during_the_siege`,
  `siege_shop_gate_off_keeps_both_packs_selling_at_any_time_to_anyone` and the config binder.
  Live capture of the restriction and side rule is still pending.
* systems::housing (siege result achievements, WP-SD6a, 2026-11-05): a finished siege credits
  `char_records` kinds 57/59 to the attacking roster and 58/60 to the defenders with
  `value1 = zone_group_id` (mapping EVIDENCED from the authored records and the Korean
  achievement texts). `AttackerCaptured` credits 57/60 and `DefenderHeld` 59/58; a
  `Cancelled` outcome credits nobody. The persisted `dominions.siege_outcome_credited` marker
  is a check-and-set under the authority lock, so a repeated tick, a capture racing the tick or
  a restart cannot credit one siege id twice; it is written with the dominion row and an online
  participant's record values in the same character save transaction. An offline participant's
  increment is written straight to `character_records` so the next entry snapshot evaluates it
  (INFERRED: the counter is cumulative; the C# had no producer). That offline write is a
  separate transaction from the dominion save, so a crash between the two can credit an offline
  participant twice (undocumented retail policy; low risk).
* systems::housing / systems::social (WP-S6 claim gating): the existing lodestone claim
  (`social/dominion.rs::declare_effect`) now applies the authored `siege_zones` claim window,
  the pirate refusal, the owned-dominion limit and the fresh-claim siege delay. The 1.2 client
  has **no claim validator**: it only maps the server's error id to a text (`FUN_3956b860`),
  so the ids are EVIDENCED but the check order is this server's policy. Window: a zone with no
  `siege_zones` row or `open_duration_hours == 0` is `DOMINION_CLOSED` 662 (client case `0x296`;
  ui_texts 5165 "not yet a region where a territory can be proclaimed", the shipped zero rows
  are zone groups 54/56); outside the recurring `open_weekday/open_hour/open_duration_hours`
  window it is `DOMINION_NOT_DECLARE_TIME` 582 (case `0x246`; ui_texts 4662, whose `$2 weekday
  $3 o'clock $4 hours` parameters are exactly those columns). The window is computed by
  `dominion::calendar::claim_window_at` (weekday 0 = Sunday, as corroborated above) and
  applies **regardless of `Housing.SiegeCalendar`**: that switch drives the inferred weekly
  cycle, not the authored claim columns. It uses the **real wall clock**; the GM
  `/dominion clock` is restricted to the calendar view. `open_duration_hours >= 168` covers the
  whole week and is always open; shorter windows are local wall-clock intervals whose end is
  `start + N hours` in local time and then converted, so a DST change inside the window keeps
  the authored local end, and a start in a spring-forward gap moves to the first valid local
  instant. Zone groups 54/56 (`open_duration_hours = 0`) are therefore **unclaimable** (662),
  and a zone group without a `siege_zones` row is also Closed. Pirates are
  refused with `DOMINION_NOT_AVAILABLE_FOR_PIRATES` 351 (case `0x15f`; ui_texts 2298 "Pirates
  cannot proclaim a territory") when the expedition's mother faction is the pirate faction 161
  or the leader's **persisted** faction is (`db_character::persisted_faction`, so a temporary
  duel / battlefield team faction does not count); this is EVIDENCED text, and the pirate-guild
  research (PIRATE-1) is the authority for the expedition test. The owned-territory limit is
  `TOO_MANY_OWNED_DOMINIONS` 441 (case `0x1b9`; ui_texts 2500) from the new configurable
  `Housing.DominionClaimLimit` (default 10, from `siege_settings` rows 0..=10; the retail limit
  is UNKNOWN). The count is taken inside `DominionManager::declare_unowned_with_limit` under
  the same lock as the claim, so concurrent claims cannot both pass, and the pack is consumed
  only after it. GM-seeded ephemeral dominions (`nation dominion` test hook) do not count.
  Order: window (662 before 582) -> pirate 351 -> limit 441. The
  `world_var_defaults.var_delay_siege_x_days_after_dominion_declare` = 30 delay is EVIDENCED
  data with INFERRED meaning: a freshly **claimed** dominion gets no cycle for 30 days.
  `DominionState::claimed_at` (new persisted `dominions.claimed_at` column) is set only by
  `declare_unowned_with_limit`; a siege capture updates `reign_start_at` but clears
  `claimed_at`, and a forced release clears both, so a captured territory does not inherit or
  start the protection. `view_at_protected` keeps a claim in peace and names the first rotation
  cycle at or after `claimed_at + 30 days` (the calendar and snapshot paths pass it; an explicit
  GM schedule still overrides). The delay does **not** refuse a claim and has no client error id (UNKNOWN
  in the research), so no claim error is invented for it. GM `/dominion` commands are
  unchanged. Tests: `a_claim_outside_the_authored_open_window_is_refused`,
  `the_gm_calendar_clock_does_not_move_the_claim_window` (582 through the data),
  `a_claim_in_a_zone_whose_window_never_opens_is_refused_as_closed`,
  `a_pirate_expedition_cannot_claim_a_dominion`, `a_pirate_leader_cannot_claim_a_dominion`,
  `a_temporary_duel_team_faction_does_not_make_the_leader_a_pirate`,
  `a_temporary_team_on_a_saved_pirate_leader_is_still_refused`,
  `a_claim_over_the_owned_dominion_limit_is_refused`,
  `ephemeral_seeded_dominions_do_not_count_toward_the_claim_limit`,
  `claim_gate_errors_follow_the_documented_order`,
  `a_fresh_claim_has_no_siege_cycle_during_the_authored_delay`,
  `a_captured_territory_does_not_get_the_fresh_claim_protection`,
  `a_claim_records_claimed_at_and_restart_keeps_it`,
  `claim_window_uses_the_authored_open_weekday_hour_and_duration`,
  `claim_window_uses_local_end_and_falls_forward_across_a_dst_change`,
  `fresh_claim_protection_refuses_cycles_until_the_delay_expires`. Live (lane stack, headless
  `aaemu-testclient` on the seeded Archeum Lodestones): claiming the open zone group 33
  succeeded (`SCHouseBuildProgress`, `SCDominionData`); the never-open zone group 54 refused
  with 662 and kept the pack; a pirate guild (mother 161) in the open zone group 33 refused
  with 351; a fresh claim in zone group 34 reported period 5 with `started` about 45 days out
  (the first rotation cycle after reign + 30 days). The recurring 582 case needs a narrow
  `open_*` window and was not live-driven because all four shipped open rows are 168 h.
  Any registered current attacker may
  engrave; initial claim requires the current guild leader and exact Purified Archeum pack.
  Capture transfers the nine independently itemized fort designs to the new guild leader,
  preserves ordinary property owners, and applies authored fort-count protection stages.
  Guild-owned fort limits, leader-only construction, sale rejection and design returns are
  world policies inferred from original 1.2 data and contemporary guides, not recovered world
  binary rules. Native wall slope/mirror variant selection remains unverified.
  Participant buffs grant the original headquarters teleport skills. Headquarters bind to
  retained original doodads for the current siege and require explicit GM rebind after restart;
  registered dead participants can revive there. The SiegePeriod doodad phase funcs are
  implemented in WP-S3 (see the deviation below); phase cycles are bounded for all groups.
  Commander eligibility and payout calendars are not inferred; roster caps and the
  defender reinforcement wave are implemented in WP-S4 (see the deviation above). Building HP
  persists through restart and lodestone collisions follow the native
  CGA/ANM root curves and settled static damage proxies. Absolute render/collider animation
  phase synchronization remains unverified. Property, authority, mail, items and wallets use
  one short prepared snapshot before SQL awaits; logout and shutdown serialize the same saves.
* systems::housing / systems::npcs (2026-11-03, WP-S3 doodad siege-period phase funcs):
  `DoodadFuncSiegePeriod` now waits until the territory period named by `siege_period_id`
  ends and then applies `next_phase` (`-1` = despawn/delete) instead of the C# "apply the
  destination on any character use". Evidence: the authored rows and chains in
  `doodad_func_siege_periods` / `doodad_phase_funcs` — 4307 `10581 (period 2) -> 10582 (3) ->
  10779 (4) -> -1`, 3304 `7627 (2) -> 7631 (3) -> -1`, 3410 `8173 (period 0) -> 8238 (3)`;
  a "when period 2 starts" reading would fire the declare-period camp chains immediately,
  while "until period 2 ends" walks declaration -> siege position -> gone. The watcher
  `npcs/doodad/siege_phase.rs` polls the read-only `housing/dominion/period.rs` view every 2 s
  (the calendar tick is unchanged) and applies a change only while the doodad is still in the
  observed group, under one phase-operation reservation. The view mirrors the snapshot: an
  unfinished explicit GM schedule uses the real clock (warmup/siege), otherwise an owned
  territory uses the weekly calendar (GM test clock); owned without either source is peace and
  unowned is period 0. The watcher starts only at the end of `housing::System::start`, after
  the dominion rows are loaded (`npcs::doodad::start` runs earlier), and the first observation
  of a zone is silent, so a boot never records the empty period 0: a later `0 -> owned` change
  cannot fire the 3410 period-0 func. That func now fires only when the target period is warmup
  (3), never for `previous == 0` alone. The watcher keeps an index of the doodads whose current
  phase has a `SiegePeriod` func (updated on create / func-group change / delete) instead of
  scanning every world object every 2 s, and skips a tick while neither the calendar nor an
  unfinished explicit schedule can change a period. A transition that finds a doodad busy with
  another phase operation is left unrecorded and retried on the next tick (the expected-group
  check makes the retry idempotent); the periods skipped between two observations are walked
  in calendar order (2 -> 4 ends 2 and 3), so a missed poll cannot strand 10582. A func-group
  change also removes the area triggers the old group created: the C# kept a duration-0
  `DoodadFuncClout` until the doodad was deleted, so the 3410 `safezone_up` revive zone never
  went away and accumulated one trigger per warmup. `DoodadFuncDeclareSiege` (the one
  `doodad_func_declare_sieges` row, a regular `doodad_funcs` row of 3304 group 7622, skill
  15067, next phase 7627) stays inert: the C# registers the class only in
  `_phaseFuncTemplates`, so `GetFuncTemplate` does not find the row and nothing advances it.
  The declaration validation, broadcast and the `7622 -> 7627` entry belong to WP-S2; wiring
  this row early would let any backpack user strand the item in 7631. Direct write targets are
  the phase machine's `do_change_phase_if_group` / `despawn_or_delete_if_group`, so a period
  event cannot move a doodad that changed phase in between. UNKNOWN (capture-pending): 3410's
  period-0 func (safe default as above; the `defense` flag is unused), 4307's authored
  10-minute lifetime vs the 90-minute siege (the phase chain wins), whether a skipped period's
  chain should really end when the calendar jumps over it (the current walk does), and
  catching up a persisted mid-cycle phase after restart (the first observation of a zone is
  silent).
* systems::housing / systems::social (2026-11-04, WP-S4 roster caps and reinforcements):
  `siege_settings` is now loaded (`CastleReferenceData::siege_settings`, keyed by
  `total_castles`) and the explicit siege snapshot carries team limits frozen at scheduling
  and the authored reinforcement delay instead of the hard-coded 100/100/100. Evidence (1.2 client): a team is
  `id, obj_id, x, y, z, ni, nr, limit, count, ids[]` (`FUN_39839c80`, `FUN_39839c10`);
  `IsReinforceArrived` (`FUN_3946c500`) is period 4 and `defender.limit == defender.nr`;
  `GetReinforceRemainDate` (`FUN_3946c3f0`) is 0 when `ni == nr`, `limit == nr` or past
  `start(siege) + bdm` (`FUN_398390a0`); `SCSiegeReinforce` 0x0eb sets the defender
  `limit = nr` (clamped to 100, `FUN_392f3670`/`FUN_39839320`). Encoding: `ni =
  num_defenders`, `nr = num_defenders + num_reinforcements`; before the wave the defender is
  `ni/ni`, after it `ni/nr`; no reinforcements means `ni == nr` and no packet. `bdm` is the
  zone's `reinforce_defense_delay_mins * 60` (the shipped rows all author 20 minutes).
  `total_castles` is INFERRED to be the number of dominions the defending expedition holds;
  more than 10 clamps to the last shipped row. Chosen reading: the count includes the
  territory being defended, so it is always >= 1 during a siege and row 0 (`70/0`) is
  unreachable; the alternative reading (count of *other* castles) would give a one-castle
  defender the 70 total at once. The data does not disambiguate the two (capture-pending).
  The caps are frozen into the persisted `SiegeState.defender_ni`/`defender_nr` columns, so a
  mid-siege change in the defender's castle count (or a test-only ephemeral GM dominion lost
  on restart) cannot move them. Existing participants above a frozen cap are not evicted:
  there is no evidence for it, and the authority checks only on join. An out-of-range
  authored `bdm` saturates to `i32::MAX` instead of dropping the territory's whole snapshot.
  The wave is announced once, from the real clock, by
  `runtime::tick_once -> reinforce::tick` when `now >= starts_at + bdm`; the
  `dominions.siege_reinforced` flag (new column) is check-and-set under the authority lock
  before the broadcast, so repeated ticks and restarts do not normally repeat the packet. A
  crash between the broadcast and the periodic save can still repeat it once on the next
  boot; the client just sets `limit = nr` again. The packet
  goes to every client through `broadcast_to_server`, like every other dominion record; the
  1.2 client keeps all dominion records server-wide (the retail audience is
  capture-pending). Roster joins (`/joinsiege` and GM `/dominion register`) now pass the
  data cap into `DominionManager::register_participant_with_cap`, which checks and inserts
  under one lock: a defender before the wave is refused with `SIEGE_BLOCK_DEFENSE_REINFORCE`
  457, a full roster with `SIEGE_PARTICIPANT_FULL` 445 (sent as `SCErrorMsgPacket` on the
  player path; the GM path reports text only). Missing `siege_settings` rows fall back to the
  native 100-character array rather than inventing a cap. UNKNOWN (capture-pending): the
  attacker cap (the same evidenced total is used for `ni/nr/limit`), whether
  `total_castles` is global or per defender, and ticket-holder membership (a wave only raises
  the cap; no member is enrolled automatically). Tests:
  `missing_or_oversized_settings_do_not_invent_native_capacity`,
  `held_castle_count_selects_the_authored_settings_row`,
  `bdm_is_the_authored_reinforce_delay_in_seconds`,
  `the_defender_cap_waits_for_the_wave_and_the_attacker_uses_the_total`,
  `frozen_siege_limits_do_not_follow_the_live_castle_count`,
  `a_missing_settings_row_keeps_the_native_cap_for_every_side`,
  `out_of_range_reinforcement_delay_is_clamped_instead_of_dropping_the_snapshot`,
  `private_mysql_dominion_rollback_failure_retry_and_restart_preserve_authority` (round
  trips `siege_reinforced` and the frozen `50/70` limits through MySQL),
  `snapshot_carries_data_driven_team_limits_and_reinforcement_delay`,
  `siege_state_body_carries_bdm_and_team_limits_at_native_offsets`,
  `reinforcement_wave_is_announced_once_at_start_plus_bdm_through_the_tick`,
  `joinsiege_enforces_the_data_caps_with_the_clients_error_ids`. A test-only ephemeral
  dominion (`/nation dominion`) still counts as the defender's held castle for the
  `siege_settings` row; the flag only keeps it out of the database and out of the WP-S6 claim
  gate. Live (lane stack, headless `aaemu-testclient`): a GM-seeded ephemeral dominion on a
  live guard tower in zone group 33 was scheduled against a seeded second expedition; both
  bots received `SCSiegeState` immediately (its body's bdm and 50/70/50 defender limits are
  fixed by the byte test) and exactly one `SCSiegeReinforce` about 21 minutes later
  (`starts_at` + the authored 20-minute `bdm`), with `expectnopkt` confirming no repeat. The
  real client's reinforcement countdown UI is not covered headlessly.
* systems::housing: sale buyer names retain the original case-sensitive lookup. The housing
  audit fixes the first-house tax/deposit omission, reversed paid flag (confirmed against the
  1.2 housing UI bytecode), overdue counter, finished-house construction, duplicate signs,
  stale building counters, and duplicate deletion. Construction and sale resources/mail
  returns are preflighted together; failed returns preserve houses, furniture and coffers.
  Recovery honors house access and the recovery toggle, including stackable furniture.
  Housing, money, mail and inventory saves acknowledge snapshots only after commit; later
  mutations and failed transactions retain pending changes.
  Player crops and dropped backpacks convert their world position/rotation into house-local
  coordinates before attachment and persistence, preserving placement when houses rotate or reload.
  Renewable new houses begin with one prepaid `DaysForTaxPayment` period plus the existing
  seven-day grace interval; subsequent payments still add one configured period. This is an
  inferred correction from 1.2 `ui_texts` 4718 (`build_taxPaymentDesc`: construction pays the
  first week's taxes) and `TaxDueDate = ProtectionEndDate - 7 days`, rather than recovered
  world-server code. The C# starts protection at seven days, immediately marks that prepaid
  period unpaid and removes it at its first week's end. Nonrenewable beginner plots keep
  their authored seven-day lifetime. Exact original bill dispatch/late-fee timing is unverified.
    Dominion property tax is now collected only against current retained live claimed-tower
    authority in the property's zone and instance. Heavy-tax-exempt properties are also
    territory-tax exempt (original `ui_texts` 5896/5897). The configurable world rule
    `Housing.DominionPropertyTaxBase` defaults to `AfterHeavy`; `Base` applies the territory
    percentage only to the template tax. The available 1.2 client proves its displayed
    base-rate calculation but does not recover the world server's heavy-tax ordering or gold
    distribution. This is an explicit server policy: floor the nominal copper surcharge,
    exclude the refundable two-base-tax deposit, and credit only the collected nominal
    surcharge as property gold. The existing certificate ceil charging rule is preserved;
    bill/quote totals are quantized to whole 10,000-copper certificates so the original native
    nearest-half-up formatter (content_configs61 and verified runtime) displays the actual cost.
    Construction/payment and ledger preflight share authority/economy locking; failed or stale
    payments cannot credit revenue. A current owning guild leader may collect with
    `/collectdominion <zoneGroupId>` (ordinary-player access, current guild leader only),
    receiving the original eleven-argument balance-receipt
    mail. Repeated empty collection creates no mail, failed staged delivery retains revenue,
    and Peace Fund remains untouched. Hunting tax was removed by Trion's 1.2 notes. A tax
    bill's `autoUseAAPoint` flag is parsed and ignored (AA points are not supported); the
    collected property surcharge is booked in `cur_house_tax_money` only, and the
    `cur_house_tax_aa_point` / `peace_tax_aa_point` ledger columns stay 0 like `peace_tax_money`,
    because no Peace Fund accrual is defined; Peace Fund percentages, nation splits and historical
    automatic payout scheduling are not invented. Original one-hour territory tax change
    cooldown uses wire error488/UI3544.
  Floors/walls/roofs use every normal-state prefab element and are synchronized when
  construction, residency or demolition changes. Model-specific placement slope/height/footprint
  checks and the developer-only
  category-8 `max_construct_count` rule are not inferred from later versions.
* systems::vehicles: `ShipyardCompleted` builds a `SkillItem` with only `ItemId`, so
  `SlaveManager.Create` returns on its `ItemTemplateId == 0` check: the finished ship is never
  summoned (the owner's active vehicle is still removed).
* systems::vehicles: a slave's `TlId` is never released; the transfer spawner's `Despawn` and
  the spawn manager's despawn tick both release the transfer's obj id.
* systems::vehicles: `DoodadFuncRecoverItem` looks the owning slave up by its DB id as an
  obj id; `GetActiveMates(...).FirstOrDefault()` decides the `Raid` relation mate.
* systems::vehicles: `Unit.DespawnMate` on a mate's own death only despawns when its owner is
  dead. The former `UnitMoveType` driver-detachment bug is fixed (see vehicle test fixes above).
* systems::vehicles: `SpawnEffect` (slave) writes `PosDistance = 2` back into the template and
  `Rotate(Rotation with { Z = .. })` doubles the roll / pitch; an owner-less boat throws in
  `Create` after spawning (`GetWorld(owner.Transform.InstanceId)`). The port now resolves
  `PosDirId` / `OriDirId` 1 and 2 (the C# ignored them); `OriDirId = 3` and `MateStateId` stay
  caster-relative / ignored because their slave meaning is UNKNOWN.
* systems::vehicles: `CSChangeMateUserStatePacket` / `ChangeStateMate` sends nothing (the packet
  is commented out in C#); `RenameMount` broadcasts the un-normalized name.
* systems::services: `RemoveDeadConnections` shuts down every account connection that has
  not sent a `PingPacket` within 30 s, including connections that never pinged (`LastPing =
  DateTime.MinValue`).
* systems::services: `WorldSetLootrateSubCommand` reports "Set GrowthRate {value}";
  `SaveManager.SetAutoSaveInterval` only stores the delay (the running save task keeps its
  interval); `AStarPathFindingSubCommand` reads its optional `templateId` with `parameters[...]`
  (throws when missing); the AStar point commands look for an "ObjId" parameter that is never
  declared.
* systems::services: `MathUtil.GetDistance(.., point3d: true)` adds the Y difference twice;
  `RotatePoint3D` builds its quaternions with the (vector, scalar) constructor and discards the
  conjugate; `NumericExtensions.ToCellOffset` / `ToPathsOffset` subtract the cell index.
* systems::services: `MailController` clamps the request's attachment counts in place (the
  clamped value carries over to the next recipient); `AuctionController.SearchAuctionItems`
  throws on lots without bidder when filtering by `BidderName`; `CommandController` passes the
  raw regex group (query string included) as the command name.
* systems::services: `CharacterActability.ExpandExpert` adds the vocation cost
  (`ChangeGamePoints(Vocation,
  +LifePoint)`) instead of paying it; `CSBroadcastVisualOptionPacket` keeps the members the new
  flag does not mention.
* systems::services: `CSExpressEmotionPacket` passes the animation id as the emotion id to
  `DoOnExpressFireEvents`; `SCUserNotesLoadedPacket` writes the UTF-16 length of the song.
* systems::ai: AI params are shared mutable objects (`RestorationOnReturn` / `GoReturnState` changes leak between NPCs of the same params row); `CheckIfEmptyAggroToReturn` inspects the abuser instead of the NPC; `IdleBehavior` enters combat and then still runs the talk greeting; the archer's delay uses `CastingTime` as seconds; the big monster's hovering phase rises 15 m per tick; `FindPath` sends debug chat lines; `ExtraLink` shapes are added twice; `UsedAreaNames` is global across worlds; `Gimmick.CurrentTarget` is never read.
* systems::ai physics: the heightmap collider's bounds use the sample count as metres, so only the first half of each world axis has terrain contacts; the dry-land "roll correction torque" is added as a force and the beached "anti-gravity" force is not mass-scaled; ships are only simulated while `Transform.WorldId` equals the instance id; the buoyancy sample points are computed but unused. The former `SendUpdatedMovementData` yaw-in-pitch transform bug is fixed (see the vehicle test fixes above).

## Coverage audit (phase 5)

### Feature audit against 1.2 data (2026-10-11)

Registration and C# port coverage are separate from feature correctness. Run
`python -I rust/tools/port_audit/features.py --compact-db <compact.sqlite3> --output <report.json>`
to inventory all 244 supported inbound CS/X2 packets, the compact tables, MySQL base tables,
production/test references and incomplete-code candidates. Optional `--reference <decompiled.c>`
links exact table literals to the local reference functions. The report contains schemas and
counts, not rows or secrets. Keep reports with client data outside git. Test the inventory with
`python -I rust/tools/port_audit/features_test.py`.

Confirmed gaps fixed in the world server (1.2 opcodes and packet layouts retained):

* `CSChangeSlaveNamePacket` now validates ownership and the configured name pattern, normalizes
  the name, broadcasts the existing `SCUnitNameChangedPacket` and saves the slave name.
  A two-player packet flow checks unauthorized/invalid requests and the saved snapshot.
* `skill_reqs`, `skill_req_skills` and `skill_req_skill_tags` restrict casts while the matching
  caster/target buff is active. Blacklists and whitelists support skill ids and tags. Native
  reference: 10.x `sub_393616E0` / `sub_39CEECD0`; failure is the existing 1.2 `SkillReqFail`
  (0x32) with the requirement id. Validation runs before mana, cooldown or start-skill buff
  removal. Tests cover real 1.2 bindings, target tags and the byte-exact failure response.
* `item_accept_quests` supplies the authoritative item-to-quest mapping rather than relying
  solely on `QuestActConAcceptItem` discovery. Item-cast acceptance checks `items.impl_id ==
  AcceptQuest` and preserves the ITEM acceptor. Native loader: 10.x `sub_39AF5840`. Loader and
  item acceptance tests cover the binding, normal quest progression and duplicate use.
* `manual_funcs` enables discrete dynamic buff modifiers. Native `sub_39C06B90` loads at most
  200 integers; `sub_39BF39F0` / `sub_39BF6160` selects by completed buff ticks, clamps to the
  last entry, and requires a positive duration/tick. The existing linear behavior is retained.
  Tests check tick boundaries and removal; real data includes the leg-injury and wing curves.
* `CSCannotStartTradePacket` sends the existing refusal response to a pending requester.
  Acceptance and refusal require a matching server-issued invitation, consumed on reply;
  invitations expire after 60 seconds. Tests check byte-exact refusal, forged/replayed replies
  and the normal exchange handshake. This also closes unsolicited trade acceptance in C#.
* `QuestActObjItemGroupGather` / `QuestActObjItemGroupUse` now count as objectives and subscribe
  to inventory/use events. The existing `quest_item_group_items` bindings define membership;
  gather objectives count current inventory, use objectives count matching uses. Cleanup/drop
  consumes only the required group quantity. This extends the adjacent single-item act model;
  it is not claimed as byte-for-byte native quest execution parity. Tests cover multiple item
  types, removal, unrelated items, duplicate member rows, use events and quest abandonment.
  Real 1.2 quests 5490 and 5489 are checked for assigned objective wire slots.
* Priest blessing purchases now use the existing 1.2 `priest_buffs` data, payment item task
  22 and errors 149/150, with live priest interaction/range, funds, duplicate and buff checks.
  The native descriptor is confirmed by 10.x `sub_39B07970`; the world-only purchase logic is
  not present in that reference. `CSRemoveBuffPacket` now checks the slave's summoner before
  allowing client cancellation, matching the existing owned-mate rule.
* House sale/cancellation is owner-only; construction and decoration items must match the
  loaded design bindings. Decoration uses the house permission checks, which now require
  actual family/guild membership before shared access. Battlefield entry requires the
  player to be in the invited instance and enrolls/subscribes once. Regression flows cover
  foreign sales, forged designs, shared permissions, premature entry and duplicate entry.
* `CSMoveUnitPacket` unit motion is limited to the authenticated character and owned mates.
  Vehicle and ship requests require the server's driver-seat record; ordinary passengers
  cannot drive. Client despawn requests require the slave's summoner. Tests reject forged
  movement/control/despawn while retaining normal motion and driver reattachment.
* Object-free quest reports no longer jump from arbitrary active quests straight to rewards.
  They use the existing validated early-completion path or evaluate an automatic/journal
  Ready step. NPC/doodad report acts still require their object. Packet flows cover rejected
  premature/omitted-NPC requests, journal completion, early-completion thresholds and replay.
* Cinema completion resends neighbouring world objects before raising quest events after a
  CUT_SCENE (or unidentified) cinema only, see the systems::quests cinema deviation. A packet
  flow checks NPC state/points replication and the cinema objective's normal transition to
  Ready.

Additional world-side fixes from the same audit:

* Cash-shop checkout charges the effective discounted price, validates templates and checked
  cart totals before fulfillment, serializes stock updates, and uses guarded SQL credit and
  loyalty debits. Gold still uses the inventory transaction path. AA-priced SKUs
  are refused with `INGAME_SHOP_BUY_FAIL_AA_POINT` 592 (AA points are not supported). A failed
  grant refunds the currency and the reserved stock. `money2` is bank gold, not an AA Points
  balance. Unit flows and private-MySQL tests cover failed debits, limits and
  concurrent overdraft attempts. Debit, audit and commercial-mail delivery do not yet form one
  crash-atomic transaction/outbox.
* Temporary USER chat channels implement the existing join/leave packets and packed eight-byte
  channel identity. Membership belongs to the current player session, chat requires that
  membership, and disconnect releases empty channels and ids. Emulator policy is server-global
  exact names/passwords, USER subtype zero and allocated chat ids; persistent ownership or
  moderation is not inferred from the zone reference. Budgets are 48 UTF-8 bytes for a name,
  64 for a password, eight joined channels per player and 1,024 channels per server. These are
  emulator limits, not a claim about retail 1.2 limits. Packet flows check the exact wire bytes,
  authentication, passwords, stale sessions, malformed/oversized input and capacity cleanup.
  The real 1.2 client creates a password-protected channel and sends through its normal
  `X2Chat` bindings. Explicit native leave parameters remain unverified; disconnect cleanup
  is covered separately.
* Mate pool and regeneration formulas now receive the complete attribute parameter set plus
  mate kind. Slave melee, ranged and spell increments, armor and magic resistance also receive
  level and all six attributes, matching the C# getters (level-50 melee increment: 5,400).
  Real 1.2 level-five horse formulas produce 1,012 maximum HP and 335 maximum MP;
  the old incomplete parameter map failed and produced zero maximum pools. Summoning clamps
  saved pools to the computed maximum and preserves injured and exhausted saves. Mate
  skill requests require ownership or an actual server-managed passenger/parent relation
  before charging resources. The slave child-cannon versus parent-caster mapping remains
  unverified and is not tightened by this mate-only rule.
  A fresh Gray Lilyut Horse scroll (8160) was also checked in the real client: full pools,
  mounting, dismounting and dismissal/resummoning passed. The native Run! action was not
  exercised because the captured pet button had no key binding and its Lua click handler
  only dismissed a tooltip; server cast permission flows are tested separately.
* Dead mates and the stablehand revive (not in the C#, which only logged
  `CSRepairPetItemsPacket` and never marked a mate dead). The 1.2 `SummonMate` item detail is
  `exp i32, dead u8, level u8` (`ItemDetails::mate_dead`, persisted in the details blob). When a
  mate dies (`vehicles::on_death_event` -> `mates::on_death`) the summon item gets the dead flag
  (plus the mate's live level/exp, from which the client prices the revive), the `mates` row
  loses its HP, and the owner receives `SCItemTaskSuccess(MateDeath, ItemUpdate)` and
  `MATE_DEAD`; a repeated death/dismissal sends nothing again. The corpse stays in the world
  until dismissed. The client never checks the flag, so `spawn_mount` refuses a flagged item or
  a saved zero-HP row (a legacy row gets the flag then) with `MATE_CANNOT_SPAWN_WHILE_CORPSE`.
  The stabler dialog (`X2:GetTotalRepairsForPetItems`) sends `CSRepairPetItemsPacket(npc)`, now
  handled by `mates::revive_pet_items`: the npc must be the current interaction object, a
  stabler and within 5 m (`TOO_FAR_AWAY`); every flagged `SummonMate` item in bag and bank is
  revived for `floor(0.5 * level + level^2.3 + 0.5)` copper each (detail level), refused with
  `MATE_CANNOT_REPAIR_WHILE_CORPSE` while one of those mates is still summoned and with
  `NOT_ENOUGH_MONEY` when the bag money does not cover the total. A revive charges the money
  (`DepositMoney` task), clears the flags, restores the rows to the new-mate pool values
  (`MATE_FULL_POOL` 9999, clamped to the computed maximum at the next summon, i.e. full HP/MP)
  and sends `SCItemTaskSuccess(RepairPets, ItemUpdate...)` + `MATE_RESURRECTED`. The old
  blacksmith repair of `EQUIPMENT_MATE` gear on this opcode was wrong (the client only sends it
  to stablers) and is removed.
* Destroyed slave summon items and the repairman (not in the C#, which only logged
  `CSRepairSlaveItemsPacket`). `GetTotalRepairsForSlaveItems` (`FUN_39447c20`) scans item
  `impl_id` 8 (`ItemImplEnum.SummonSlave`; the mate variant scans 11), requires the item's
  destroyed flag and adds the slave template's `cost` (the client's `slaves` query selects
  `id, cost, ...`; `FUN_39696380` +4), not equipment durability (1.2 slave equipment items are
  `impl_id` 28 and carry no durability detail). A repairman (the `IsRepairmanNpc` flag,
  `npc_templates.repairman`) repairs every destroyed `SummonSlave` item in the player's bag
  (the client's preview keeps only the slots whose container is `inventory + 0xc34c`, the bag;
  bank slots are filtered out) for its
  slave template cost: the NPC must be the current interaction object, a repairman and within
  5 m (`TOO_FAR_AWAY`); a summoned vehicle is refused with `SLAVE_CANNOT_REPAIR_ALREADY_SPAWNED`.
  The money is debited first (`DepositMoney` task; `NOT_ENOUGH_MONEY` leaves everything
  unchanged), the update runs only through the player's bag container (no global item-store
  fallback) and an item that vanished before the update is refunded, then the items lose the
  destroyed flag and the 10 minute repair start (`RepairStartTime` 0: the paid service
  completes at once, unlike the repair kit), and the owner receives
  `SCItemTaskSuccess(RepairSlaves 74, ItemUpdate...)` + `SLAVE_REPAIRED` (the client locale has
  `slave_repaired` / `slave_cannot_repair_already_spawned`); when every scanned item vanished
  before the update, only the refund goes out (no empty task / message). Tests:
  `a_repairman_repairs_destroyed_slave_items_for_the_slave_cost`,
  `slave_repair_is_refused_without_gold_and_leaves_the_item_destroyed`,
  `a_spawned_slave_cannot_be_repaired_and_nothing_is_charged`,
  `a_repairman_service_requires_the_interaction_the_role_and_the_range`,
  `slave_repair_is_bag_only_and_ignores_the_bank`, and the unit test
  `repair_updates_only_items_still_in_the_given_containers_and_refunds_the_rest`.
* Character persistence saves `reported_as_bot_count` as well as `bot_reported_count`; the
  former was read but omitted from REPLACE, resetting the reported character's counter on
  every save. A private-MySQL temporary-table test covers repeated saves and adjacent fields.
* Expedition ownership transfer now keeps `owner_name` consistent with the validated new
  owner. Transferring to oneself is a no-op, preserving the owner role instead of demoting
  it. Packet flows and a private-MySQL temporary-table test verify both roles and the saved
  owner identity.

* Safe Portal Mode now honors `CSUsePortalPacket.only_my_portal` against the portal creator's
  character id and sends the existing 609 error for a foreign portal. Shared mode preserves
  public access. The loaded-slave instance check also runs before setting the player's
  movement freeze flag, so rejecting a loaded vehicle leaves movement state unchanged.
  Registry packet flows cover both initial flag states, exact errors and a later valid use.

* Sport-fishing stands now pay the existing 1.2 `items.refund` once and consume the validated
  equipped fish in the same inventory operation. Trophy conversion selects the pack for the
  actual conversion function and preserves the fish when mapping, reward or bag placement
  fails. Successful conversion keeps the stand. A retail weight-dependent sale formula has
  not been established; the existing static refund remains the emulator policy.
  The five rod reactions with `target_my_npc` require an NPC owned by the caster's character
  id before cast, resource, buff or cooldown mutations. Character and object ids are distinct.
  Flow tests cover wrong/correct species chum, active-school expiry replacement, fish sales,
  trophies, replay and failure paths; a real-compact test checks the Bamboo rod's plot 821,
  Yellow Lure and 100-labor consumption. The real client equips the rod and exposes all seven
  mode actions. Native stand confirmation casts also consume one Gargantuan Blue Marlin pack
  for trophy 28004 or pay exactly 400,000 copper (the client shows 40 gold); a later empty-back
  sale attempt leaves the balance unchanged. The stand remains available after conversion.
  Equipment acquisition now rejects multiple packs before any item creation or mutation,
  preserves explicit occupied slots and permits NPC fish pickup into an empty backpack slot
  even with a full bag. Occupied packs and full-bag glider replacement preserve the loot for
  retry. A complete native catch/reaction/loot cycle is still under investigation.
  Native chumming has also activated a Carp school (17243 → 17245), supplied buff 5767 and
  enabled a normal sport cast that consumes one Yellow Lure. Native fish spawning and reaction
  casts are observed; complete capture and corpse pickup remain under investigation.
  NPC fixed-position/self-target Leap controllers now use the reference velocity/acceleration
  math and vertical movement packets, with a kinematic 3D collision sweep. Moving unit targets,
  parented actors and unsupported landing rotation retain the prior linear path; native living
  actor contact prediction is unavailable. Underwater actor models move in 3D without projecting
  the fish onto the seabed. Direct character damage to owned FISH NPCs requires the character
  owner and the loaded fishing exception tag; this conservative FISH-specific rule does not
  infer generic creator/skill-tag exception semantics. Buff caster identity remains the fish.
* Sport-fish movement now uses its authored skill controllers exclusively. Between controllers,
  FISH NPCs stop without changing position or heading. The former experimental C# override
  added continuous movement toward global 10,000-metre offsets (including horizontal drift
  for Dive), beyond the 1.2 Carp's bounded relative endpoints. Regular and real-data tests
  cover cue pauses, controller ownership, subsequent heading and ordinary NPC movement.
* Ship replication preserves the physical quaternion instead of rebuilding it through the
  legacy Euler conversion, including mixed yaw/tilt and positive-W packet reconstruction.
  Solid hull and bottom boxes use transformed authored collision-proxy bounds independently
  of the inertia-only mass box, keeping the authored length/beam (the Longliner's 14 m half
  length). The bounds' z-range is clipped to the hull envelope at `add_ship`: the top is at
  most the mass-box top + 1 m freeboard (`cry::HULL_FREEBOARD_METERS`) and the authored
  bottom/draft is kept, so mast and rigging proxies of the "DEFAULT" union never become part
  of the solid hull. Without the clip the union put the entity bounding box above the
  waterline; the engine's water-area check then let the summoned Cutter sink until buoyancy
  launched it. Native entity bounds can include render and non-brush objects unavailable to
  this loader, so this is a documented proxy adaptation. Explicit buoyancy settings remain
  authoritative; missing model assets retain the prior geometry fallback.
* Doodads attached to a slave (masts, sails, cannons) get no world collider: they move with
  the hull and would otherwise overlap its collision box (CryPhysics then ejects a freshly
  summoned hull). The exclusion is keyed on `doodad_owner_type::SLAVE` and therefore also
  covers cart equipment.
* Summon placement. Player item summons (`SpawnSlave` / skill 15802, item use) follow the 1.2
  client's own pre-cast search (`FUN_39347fb0`), a deviation from the C# `Create` placement:
  120 candidates at `spawn_x_offset + i` metres forward along the player's facing (i = 0..39)
  with lateral offsets `{0, +spawn_y_offset, -spawn_y_offset}`; the first passing candidate
  wins. A candidate passes when (ships) the collision terrain is not above the water surface at
  x/y, (land) the terrain is at most 10 m above the player, a ray from the player's eyes to the
  candidate hits nothing (mask `SOLID_TERRAIN` 0x107, flags 0x8F; ships end at the water surface,
  land at the candidate's terrain point lifted by 0.2 m), the model box (the
  unclipped model bbox from the prefab collision proxies; the inertia mass box of the ship model
  when the model reference is unavailable, e.g. no client assets) rotated to player yaw + 90
  degrees overlaps no blocking static and no terrain heightfield, and the 3D player-to-candidate
  distance is at most 80 m. The land candidate z, its distance check, the model box and the
  final spawn (search, `SummonPos` validation, post-wait recheck and re-ground) are all the
  terrain heightfield only (`GetTerrainElevation`, vtable `+0x1fc`), never statics: a deck,
  bridge or quay above the candidate's terrain lies between the eyes and the terrain point, so
  the 1.2 rule refuses the summon (error 100) instead of placing the vehicle on the floor. The
  0.2 m lift of the land ray end is a deviation: 1.2 ends the ray exactly at the terrain point
  and the lift avoids a float graze between the ray end and the heightfield (the Bevy client
  port lifts its end the same way).
  The box is an oriented `PrimitiveWorldIntersection` query (`box_obstacles`) for statics plus
  `box_overlaps_terrain` for the heightfield (the PWI box query does not report heightfield
  contacts); this 1.2 path has no +1 m top margin, no terrain point sampling and no
  containment rays. Statics with a world AABB under 2 m (buoys, bollards, mooring posts) and
  vegetation are ignored. The box query batches: the engine's per-thread contact pool holds 64
  records and 2040 border points and silently drops further parts, so a saturated round is
  repeated with the entities already seen in the skip list (the batch box stays the client's exact
  box; the caller's identical-box query cache is invalidated before every round with
  `World::invalidate_box_query_cache`) for at most 8
  rounds; a still-saturated query reports the spot as blocked. If none of the 120 passes, the summon is refused with `SCErrorMsg`
  100 (`SLAVE_SPAWN_SHIP_NEED_MORE_SPACE`), the summon item is not committed, the skill
  cooldown and GCD are refunded — on the server and on the client with the same
  `SCSkillCooldownResetPacket` the skill system uses — (also when a client `SummonPos` placement
  fails the data-range or water checks with `SCErrorMsg` 583), and nothing is spawned. A client
  `SummonPos` placement is validated with the same 1.2 checks instead of searching (the old
  data-range and water sanity checks still run first). The old C# depth/overhead probes, the +1 m margin, the
  nine-point terrain sampling and the containment rays remain in `hull_clear_at` only for GM
  `/slave spawn` and spawner placement (no item), and its overhead probe now casts down and up
  the same segment: the upward ray finds the underside of a deck thick enough to contain the
  downward ray's start (`collision::flow_tests::summon_overhead_clearance_sees_a_deck_that_contains_the_ray_start`);
  the player 1.2 model box rejects the same thick deck through the static/heightfield overlap.
  Remaining limitations: the box-box intersect emits only clipped edge points, so a static
  wholly inside the model box is not reported on this path (the old containment rays are
  GM-only); land slaves use the terrain step and line of sight, not a vehicle-model box (only
  ship models are loaded); and without a collision world (tests, Physics off) player placement
  falls back to the C# in-front placement. `Physics:SummonDiagnostics` (Rust only, default
  off) keeps scanning all 120 candidates after the first valid one and logs the 1.2 model box
  against the old hull envelope per candidate (debug only).
Remaining packet stubs and table candidates are still subject to behavior review. Missing SQL
mentions do not establish disuse (some names are selected dynamically); empty tables, client
visuals and schema migration metadata do not imply missing server features. The 10.x reference
may contain modern-only fields or behavior and is not a source of 1.2 wire layouts.

`rust/PORT-AUDIT.md` maps every C# file of `AAEmu.Commons`, `AAEmu.Login` and `AAEmu.Game`
(GM commands and unit tests excluded) to its Rust counterpart with a status (ported / merged /
dropped with the reason); regenerate it with `python rust/tools/port_audit/gen.py
rust/PORT-AUDIT.md` (verdicts of hand-checked types in `rust/tools/port_audit/verdicts_*.txt`).
`packets::tests::registry_matches_csharp_game_network` keeps the C->S registrations equal to
the C# `GameNetwork`. Deviations of the audit ports:

* Sticky parents are an `ObjectCore` marker (not part of `Transform`, so transform copies such as
  `CloneDetached` never carry it); setting one also makes it the real parent with the world
  position kept, as `SetStickyParent` does.
* The zone conflict state lives in `systems::social::conflicts` (copied from the loaded data at
  start); `NextStateTime` is kept in milliseconds for the timers and sent in seconds.
* `ZoneStateChangeTask` is a tokio task holding a weak server reference.
* RPA-10 (zone conflict kill counter): a PvP kill registers with the zone conflict in every
  state (`skills::combat::award_pvp_honor` calls `conflicts.add_zone_kill` before the honor
  branch). The 1.2 C# only called it from the Conflict / War honor branch, where
  `ZoneConflict.AddZoneKill` returns early, so the `conflict_zones.num_kills_0..4` thresholds
  of zone groups 14-27 (70 / 100 / 140 / 190 / 250) were unreachable and the zone stayed in
  Tension after its first Peace -> Tension step. Honor stays Conflict-only / War-only as in
  1.2. Regression: `skills::tests::pvp_kill_advances_a_conflict_zone_that_returned_to_tension`
  walks Conflict -> War -> Peace -> Tension and then kills once at 70 accumulated kills.
* `SusManager.finalize_check` keeps the `_skipCheckTime` / `_lastFinalizeTime` of an object by obj
  id; the analysis itself (database log) runs on a task.
* systems::services (commands): `CrimeCmd` keeps `CommandNames = ["doodad"]` (help / call prefix
  `/doodad`); `GoldCmd` hides `AddGold` and `gold set` adds like `add`; `SetFaction` changes the
  caller's faction; `AddPortals` uses the caller's position and roll; `Heal` without a target does
  nothing; the `Fly` cache toggles off when set to true twice; `TestChatChannel list` never
  resets its name buffer; `ItemUnwrap` passes the Secure flag twice; `QuestCmd
  objective|progress` index missing arguments (IndexOutOfRange); `QuestCmd template` /
  `SphereCommandUtil.GetQuestSphere` read the never-set `DetailType`; `GetSphereList` filters on
  the quest object id; `CrimeCreate` stores degrees as radians and throws without `source=`;
  `TestHouse setforsale` does not normalise the buyer name; `Kick` throws on a non-byte reason;
  `SendPacket` throws on a one-byte payload; `Scripts shutdown now` prints `-0` minutes.


### Quest and mail audit follow-up (October 13, 2026)

The quest/mail audit deliberately corrects C# failures rather than preserving
them. Quest Unix deadlines round-trip, timers are reconstructed from earlier
components on reload, and expired quests become Failed during initialization.
Aggro credit uses the killed NPC; CheckSphere uses local presence and template-ID
cleanup. Completion consumes supplies once. Ability/prerequisite-completion
events, zone filters, guarded NPC death, authored NPC actions, acceptor checks,
ID cleanup and sphere trigger/flag handling are wired and covered by regressions.

Quest overflow reward mail is stored before completion under the economy
operation boundary. Recipient Mail escrow is persistent and legacy ownership is
repaired on load. Player mail validates currency/slots/bind state and rejects
early access; returns keep original IDs and do not charge postage again. Ordinary
item packets cannot withdraw or destroy Mail/Auction escrow. Cancellation returns
the actual listed item and bids reserve only affordable funds.

Auction snapshots join mail/items/wallets in the prepared MySQL save transaction;
commit acknowledgement preserves newer dirty rows/deletions. The wiki audits
record targeted tests, MySQL rollback/reload and the real 1.2 client evidence,
including GM fixture setup and remaining authored content/review limitations.
See Docs/wiki/Quest-Audit.md and Docs/wiki/Mail-Audit.md.

### Spawn data import (October 17, 2026)

| Area | Rust / data | Status | Notes |
| --- | --- | --- | --- |
| Quest NPCs / doodads missing from every `*_spawns.json` | `app/Data/Worlds/{main_world,instance_library,instance_prologue,instance_nachashgar,instance_burntcastle_armory,arche_mall_world}`, `rust/tools/spawn_import/` | data | Positions from the 10.x pak (per-zone `npc_spawners.g`, per-cell `doodad.g`), the 1.2 `npc_spawner_npcs` rows and the git history of upstream spawn JSON. A position is imported only when it sits on the 1.2 heightmap (`|dz| <= 5 m`, ignored test `dump_terrain_heights`), next to an existing spawn of the same world (`<= 40 m`, `|dz| <= 8 m`: interiors, caves, bridges) or on clipped ground (heights are `u16 / 64`, so ground above 1024 m reads 1024) of a zone/frame verified by its other candidates. Editor-tier positions win over git-only ones. Imported: main_world 181 NPC / 58 doodad entries, instance_library 131 doodads, instance_prologue 8, instance_nachashgar 2, instance_burntcastle_armory 1, arche_mall_world 3 NPC / 1 doodad. Entries carry a `Title` naming the basis (the loader ignores it). |
| Zones whose origin differs between the 1.2 and 10.x `world.xml` (190, 192, 193, 200, 227, 251, 266, 281, 286, 287) | same | data | The content sits at the 10.x position (zone origin x 1024 + local): the 1.2 origin never fits the 1.2 heightmap (0 of 154 editor candidates with unclipped terrain), the 10.x frame does (many points within 0.1 m). Z values above the 1024 m ceiling cannot be verified. Candidates whose terrain differs by tens of metres (10.x-only zones such as 157, parts of 190/281) were rejected. |
| 10.x world spawns backported into 1.2 `main_world` | `app/Data/Worlds/main_world/{npc_spawns_10x,doodad_spawns_10x}.json`, `rust/tools/spawn_import/backport_10x.py` | data | Source: the 10.x `main_world` `npc_spawns.json` (28,434) / `doodad_spawns.json` (89,121). Filters in order: template id exists in the 1.2 compact (NPC -2,563, doodad -8,200); 10.x `Title` equals the 1.2 `en_us` name (ids were reused: NPC -2,202, doodad -60); the X/Y lies in a non-zero 1.2 `world.xml` zone (doodad -1,618); excluded doodad names `Restricted Area: Under Construction` (1,105) and `Sunken Treasure Chest` (1,323; no 1.2 system); the terrain rule of the October 17 import (`|dz| <= 5 m` to the 1.2 heightmap, or within 40 m / 8 m of an existing 1.2 spawn, or clipped ground; NPC -256, doodad -10,995) after the match; a one-to-one match per template before the terrain rule: each 10.x entry is paired, greedily by ascending XY distance, with the nearest unmatched existing 1.2 spawn of the same template within 20 m (NPC) / 10 m (doodad) and dropped (NPC -22,828, doodad -30,481), so shifted copies of an existing pack are not doubled and only the surplus survives; 10.x entries within 2 m of each other then collapse (NPC -6, doodad -3,677). Result: 579 NPC and 31,662 doodad entries. The NPC entries and the 670 mining veins (645 Iron Vein, 25 Starshard Vein) ship (the loader's `npc_spawns*.json` / `doodad_spawns*.json` glob reads them; they carry `Title`). The 10.x source is a live-server snapshot: its other surviving doodads were mostly player-planted trees, crops, livestock and housing furniture, which share templates with natural spawns and cannot be told apart, so they were removed (2026-10-27). Zones whose 10.x terrain differs from 1.2 (e.g. most of Rookborne Basin, zone 270: 128 of 145 10.x Iron Veins sit 100-300 m off the 1.2 ground) are correctly rejected by the terrain rule, so that zone gains only 3 veins. Rerun steps are in the script docstring (the heightmap dump is the ignored test `dump_terrain_heights`). |
| Quest 869 giver doodad, quest 1618 cave cobweb and the quest-referenced doodads dropped by the 10.x vein-only filter missing from every spawn file | `app/Data/Worlds/main_world/doodad_spawns_quest_fill.json` | data | One entry: doodad 1137 "Unknown Crate", the giver of quest 869 "Treasure of the Towers" (Mahadevi, level 16, open zone). The position 19065.793, 8919.587, 352.82 from the 10.x `main_world` cell `018_008` `doodad.g` (type 1137) agrees to 0.0 m with the 10.x `map_data/doodad_map/e_mahadevi.dat` and `doodad_map.dat` records (1137); the 1.2 `e_mahadevi.dat` (D:/AA1.2 pak, 124 records) has no 1137 record, as the 1.2 `doodad_map` only holds a subset of map markers and is not an existence source; yaw 66.9998 degrees comes from the 10.x quaternion `(0, 0, 0.551936, 0.833887)`; the position lies in open zone 196 (`e_mahadevi_2`), whose `world.xml` origin is identical in 1.2 and 10.x. The raw 1.2 heightmap reads 241.78 m there, the 1.2 navmesh (the live `GeoDataMode` height path) 352.81982 m, 0.0002 m from the spawn Z; the ignored test `real_data_quest_fill_spawns_on_terrain` re-checks the loaded spawn, the open zone and that height. Second entry: doodad 1877 "Dungeon Cobweb", burned in quest 1618 "Beneath Castle Ronbann" (FakeUse skill 12477, phase 3655, respawn 60 s) at the cave entrance in Lilyut Meadow (`w_lilyut_meadow`); position 12840.637, 16329.311, 372.802 from the 10.x `main_world` cell `012_015` `doodad.g` (cell offset 552.637, 969.311, 372.802), which agrees to 0.0 m with the 10.x `map_data/doodad_map/doodad_map.dat` and `w_lilyut_meadow.dat` records (1877); the quaternion is the identity, so yaw 0 (it was the only spawn in the dropped 10.x backport, commit 007a40d1b). The 1.2 `w_lilyut_meadow.dat` (D:/AA1.2 pak, 123 records) has no 1877/1878/1879 record, again because it is only a marker subset; the neighbouring quest doodads 1878/1879 are already spawned. The 1.2 navmesh height at the cobweb is 367.905 m, 4.897 m below the authored Z 372.802 (zone 195, open): the web is a prefab hung across the cave mouth, so the ignored test `real_data_quest_fill_spawns_on_terrain` allows 1877 alone up to 6 m above the navmesh (every other fill entry stays within 3 m). For comparison the existing 1.2-data spawns of 1878/1879 sit on the navmesh: Z minus height is -0.374, -0.582, -0.685, +0.010 (1878) and -0.586, +0.254 (1879). Third batch (commit 77026d2ad kept only veins of the 10.x server snapshot, dropping quest doodads): every entry's position is a 10.x `main_world` `level_design/cells/<cx>_<cy>/doodad.g` record (world = cell*1024 + local) that agrees to 0.05 m with a `map_data/doodad_map/doodad_map.dat` record of the same template, yaw from the quaternion about Z, in an open zone, within 3 m of the 1.2 navmesh height (the test; 3377 and 1877 are the documented `RAISED_FILL_PLACEMENTS`). Restored: 3307 Giant Beehive x17 (quest 2903, FakeUse + RequireQuest 2903, loot item 18458 = gather act 1207; the hive has no item or farm that plants it), 4162 Swan Feather Arrow x25 (quest 3761, RequireQuest 3761, loot item 20066 = gather acts 1577/2336; 29 in `doodad.g`, the 4 whose navmesh height is 3.2 to 7.6 m above them (9023.4/8450.1, 9100.8/8492.2, 9104.5/8446.7, 9127.0/8434.0) are left out although the raw heightmap agrees to 0.7 m), 3375 and 3377 Pirate Urn (quest 2908; its gather acts 1213-1216 each need their own urn, 3374 and 3376 are already in `doodad_spawns.json`; loot items 13988 and 14022; 3377 at 19234.263, 8635.435 is 5.0 m above both the navmesh and the raw heightmap, the NPC spawns 30-60 m away stand on that 226.36 ground, so it is allowed up to 6 m above, a raised ledge not visually confirmed), 3629 Suspicious Footprints x14 (quest 1969, highlight of sphere act 496; quest-only `quest_footsteps` prefab), 4122 Windheart Lake Altar (quest 3205, highlight of item-use act 518), 7823 Malcolm's Armor x3 (quest 2486, highlight of talk act 969). Excluded: 4347 Eastern Gate and 4348 Northern Gate Task Board (quests 353/371): not missing, they alternate with 4351/4352 at the same spot, which `doodad_spawns.json` spawns; each has a `DoodadFuncTod` phase that ends in `DoodadFuncFinal` + `DoodadFuncRatioRespawn` 524-527 (10000) spawning its twin, and a live client saw a 4347 / 4348 object at the board positions through that chain, so a fill entry would double the board, 1366 Snow Flower x27 (quest 1017, but zone 154 w_bronze_rock is closed in 1.2), 7313 Glass Shard Mound x34 and 7976 Remains x21 (quests 6220/6221, zone 301 o_shining_shore_2 closed, later-patch content), 6212/6215 Blue Salt Brotherhood Bulletin Board (quests 5151/5153 start only from them, but they exist only in the old 10.x server-data snapshot at 15371.62, 11552.05, 108.327 / 20935.4, 13152.78, 114.4854 and not in any `doodad.g` or `doodad_map` of the 10.x client files, so there is no second source; undecided), 1140 Basement Door, 1141 Sacrificial Pen, 1480 Bone Heap, 2779 Belethe (their only act rows reference `quest_components` 3647/4054/5674/10074 that do not exist in the 1.2 data and the candidate quests 795/872/1245/2328/2318 have no act that uses them), 1560 Thief's Chest (needs quest 1483, which has no `quest_contexts` row; act component 6988 missing), 3544 Serpent God Statue (its only act row references the missing component 13163; its RequireQuest is quest 5695 whose gather item 29258 is not looted from it; `mgmt_spawn` is set), 2673 Chick (item 16230 plants it, `farm_group_doodads`; 119 scattered `doodad.g` records are player livestock; its quest use is a phase check in a missing component), 4594 Vita Root (item 23635 plants it; not in `doodad.g`), 6049 Guest Bed (item 18677 places it; the quest only highlights it and the 8 records are player houses). Excluded: 22 NPCs whose 10.x spawner placement and 10.x `npc_map` record agree within 0.5 m but whose quests are in zones closed in 1.2 (Airain Rock 192/193, Sunbite Wilds 157/190, Cradle of Genesis 328, Golden Ruins 281/203, Mistmerrow 307) or are test/dummy (Maisky 13877, Sephvell 14147); the open-zone missing NPCs without a position in both sources (Tame Yata 3450, Asam 1266, Freeblood Maurauder 1772, Trafficker Pirate 12509 and the other one-source/no-source NPCs); library doodads 7992/7993/7994 (level 51-55 later-patch content) and the remaining missing doodads 1082, 2770, 6197, 7404, 4380 (no `doodad.g` + `doodad_map` pair); all missing entities with no 10.x position at all (the majority of the 179 candidates). |
| `NpcSpawner.SelectSpawnerId` for NORMAL multi-member spawners | `systems/npcs/npc/spawner.rs` (`select_spawner_id`) | matches the C# | Not changed. A JSON entry clones every game-data spawner that has its unit as a member and (memberwise clone) shares one `NpcSpawnerIds` list, so with more than one spawner only an AUTOCREATED unscheduled one or a scheduled one is ever selected: a unit that has no JSON entry of its own and is only a sibling of a NORMAL group spawner (Vicious/Silent Treant 3105/3106, Goblin Fighter 527, ...) never spawns. The 13 audit findings of this kind are all `ConAcceptNpcKill` starters, which `starter_accepts` evaluates as an OR over the component's acts, and in every one of the 7 quests other listed NPCs spawn, so no quest is blocked; the data would need real positions for these units (the 10.x editor has no placement of those group spawners). |


### Area sphere geometry import (November 5, 2026)

| Area | Rust / data | Status | Notes |
| --- | --- | --- | --- |
| Area sphere geometry from the 10.x `world_server/quest_area_sphere.g` | `rust/tools/sphere_area_import/sphere_area_import.py`, `app/Data/Worlds/*/sphere_area_fill.json` | data | The 1.2 pack has no `quest_area_sphere` file; the 10.x pack has 150 files / 3,288 blocks (`area / kind / stype / pos / radius`, all `kind` 1, ids = the 1.2 `spheres.id`). `pos` is **zone-local**, not world (the November 5 note wrongly claimed world: the 1.2 `mission_mission0.xml` `AreaSphere` positions it was compared with are zone-local too). Frame proof: 662 same-quest pairs of a 10.x area block and the 1.2 client `quest_sign_sphere.g` sphere in the same zone agree (165 within 5 m, most within 0.2 m); zone 133's area spheres 9/85 match the 1.2 mission `AreaSphere` entities to 0.005 m. The converter writes world coordinates as `source_zone_origin * 1024 + local` (z unchanged), the formula of the 1.2 sign-sphere loader; the origin comes from the **10.x** `world.xml` because the 10.x file is anchored there (zones 192/193 gained a cell row in 10.x; the 1.2 origin would move their spheres a cell south). Guards, in order: the world exists in the 1.2 `world.xml`, the zone key exists in that world, the `stype` exists in the 1.2 compact `spheres` table, the 10.x zone frame exists, the converted position lies inside the 1.2 zone's cell rectangle (`position_outside_zone`: e.g. 1.2 sphere 943 sits in a 10.x-only cell row of zone 193), and the block is not an exact duplicate (one area would otherwise fire twice); malformed blocks are dropped. Kept 2,112 areas in 14 worlds (1,913 `main_world`; 430 `SphereBubble` blocks over 344 ids; 19 spheres with a `team_msg`: 8 `SphereBubble`, 7 `SphereSkill`, 4 `SphereQuest`). Dropped 323 world_not_in_1_2 (10.x-only instance worlds), 239 zone_not_in_1_2 (16 `main_world` zones absent from 1.2), 612 sphere_not_in_1_2 (10.x-only ids), 1 `position_outside_zone`, 1 duplicate. The 10.x pak is read only by the offline converter; the `sphere_area_fill.json` files are the committed input (owner approval 2026-11-05). The ignored tests `real_data_sphere_areas_load_the_guarded_1_2_table` and `real_data_fill_spheres_lie_inside_their_zone_rectangle` re-count and bounds-check them. |

### `RedeemBuff` special effect (type 44)

Removes one of the caster's own beneficial effects (the same filter as ExplodeBuff: `buffs.kind`
1, non-passive, non-system, not-ended, lowest buff slot first) and only then restores `value1`
mana through the fixed-value `RestoreManaEffect` path (`effects::combat::restore_mana`), so the
heal and points notifications match a real mana restore and the gain is capped at `MaxMp`. No
beneficial effect means nothing is removed and no mana is restored.

`value2` / `value3` read 10 / 15 on the only shipped row (763 `[4000, 10, 15, 0]` of 효과 흡수,
player skill 11988) and their meaning is not understood, so they are ignored. The 의기 charge the
skill's description mentions is a separate effect of the same skill and is not part of this
action.

### `MoveToGround` special effect (type 73)

Lands a unit on the terrain floor. The destination XY is the target position when the effect was
given a position unit (the 10.x movement skills that carry the type are `skills.target_type_id` 6
"pos") and the caster's own XY otherwise. The ground sample comes from the collision service
(`ai::collision::ground_height_or`, falling back to `terrain_height_or`, 0 when neither answers);
a sample of 0 means no floor and the effect moves nothing. The snap policy is ported from the C#
`GroundSnapRules` / `TerrainFloor` of commit `c78b3a659` into
`systems/skills/ground_snap_rules.rs`: 0.4 m clearance, the water surface wins over the seabed
when it clears it by more than 1 m, the 3 m near band sits on the floor, a lift up to 8 m lands on
it, a drop up to 12 m lands on it, and anything beyond keeps the raw Z. A player is moved with
`SCBlinkUnitPacket` exactly like `Blink` (the client moves itself and reports the position back);
an NPC goes through `move_npc`. `value1` is not applied (0 on the reachable rows).

Deviation: the C# used the caster's XY even for the position-target rows and returned
`OnFloor(rawZ)` (raw + clearance) on an over-lift; the selected position is used here, and the raw
Z is kept on an over-lift, which is what the C# log line says that branch does. Data: 1.2 row
9639 `[0, 0, 0, 0]` is reached only through `plot_effects` 8677 (plot event 5431 "self move" of
skill 18103).

### `ExplodeBuff` special effect (type 46)

Burns the target's beneficial effects away and deals `value1` damage for each one burned, up to
`max(value2, value3)` of them. Ported from commit `c78b3a659` (`ExplodeBuffRules` / `ExplodeBuff`;
the 1.2 C# only logged the type). The pure policy is `systems/skills/explode_buff_rules.rs`.

Only beneficial (`buffs.kind` 1), non-passive, non-system and not-ended effects are burned, lowest
buff slot first, through `buffs::exit`; passives and system buffs are part of what the unit is, not
something an enemy's skill can strip. The damage goes through
`skills::combat::reduce_current_hp` with the caster as attacker, and an `SCUnitDamagedPacket`
carries the hit to the client (the C# only reduced the HP). No eligible effect means no damage.
The row may hang off a buff Timeout trigger (buff 449), where `args.source.skill()` is `None`;
nothing in the effect reads the skill.

Data (1.2, joined through `effects.actual_type = 'SpecialEffect'`): 5009 / 9531
`[1000, 20, 20, 0]` on 내부 충격 16410 / 20650 (effects 15588 / 27256), 6411 `[2000, 50, 50, 0]` on
the Timeout trigger 2194 of buff 449 (effect 20330).

### Skill rank formulas: mana cost and cast time (1.2 client)

Mana and cast time are evaluated at the skill's own **rank level**, not at the caster's ability
level (the C# `Skill.ManaCost` used `caster.GetAbLevel(AbilityId)` and ignored `Skill.Level`).
Evidence (Ghidra `aa12u`, x2game): the tooltip entry `FUN_3941e750` reads the learned skill level
(`FUN_390b9950` / `FUN_390b9920`; rank 1 for a class skill the character does not have), maps it
through `FUN_39897fe0` to `L = ability_level + (rank - 1) * level_step`, and calls the evaluator
`FUN_398a01b0(ctx, unit, L, ctx2, skill, kind, 0)`:

```
T     = max(0, (L - ability_level) * casting_inc * 0.001)                (FUN_398a7710)
mana  = TRUNC( mana_cost + mana_level_md * (1 + T) * LevelMana(L) )      (kind 0)
cast  = (int)(casting_time * (1 + T))                                    (kind 4)
```

followed by the caster's skill modifiers (`FUN_3989f900`) and, for the tooltip / cast bar, the
cast-speed stat `(stat + 1000) / 1000` (`FUN_393543a0`); the kind-4 cast time is truncated to
an integer before those modifiers. `LevelMana` is the character `unit_formulas` rows 12/13
(`(L * 1.6 + 8) * 3 / 3.65`). The client truncates the final mana value: `FUN_398b56d0` takes
the `DAT_3a5e41f0 = 1` truncation branch, and 32 of the 68 live readings have a fractional
mana part >= 0.5 that the client truncated (Purge rank 1 raw 57.534 -> 57; Freezing Earth
rank 8 raw 946.849 -> 946; Meteor Strike rank 2 raw 1814.795 -> 1814).

`skill::effective_level` / `skill::rank_inc` / `skill::rank_for_level` /
`skill::cast_skill_level` / `skill::level_and_inc` implement `L` and `T`; the mana charge
(`Skill.ManaCost` -> `consume_mana`), the resource pre-check (`validation.rs`) and the plot
`ManaCost` effect all read that same pair, so an allowed cast is charged exactly what it
checked. The cast time is `skill::cast_time_ms` (the announced `SCSkillStarted` base/real time
and the server timer) and the `casting_time` term of `skill::attack_delay`. Plot-driven casts
(`plots.rs`) keep their own plot-event delays and the cast-speed stat; those delays are plot
data, not `skills.casting_time`, so the rank term does not apply to them.

Live confirmation against the original r208022 client
(`D:/aaemu-audits/skill-tooltips-2026-11-09/`): 68 tooltip readings, 27 skills, ranks 1..=19,
maxima `(55 - ability_level) / level_step + 1`, mana residual 0 and cast residual 0 on every
row (the readings were taken with cast-speed stat 0, so the stat factor is 1.000 in all of
them). The gated test `real_compact_skill_tooltips_match_the_1_2_client` pins all 68 rows
against the real `compact.sqlite3`.

Which rank applies (`FUN_3941e750`):

* A character's class skill (`ability_id != 0`) uses the learned rank on the `SkillRun`
  (`FUN_390b9950` / `FUN_390b9920`); variants (`IsVariantOfSkill`, e.g. the combo skills) run
  at the learned base skill's rank, not at rank 1.
* An `ability_id == 0` skill uses the rank from the character's level,
  `(charLevel - ability_level) / level_step + 1`, learned or not (rank 1 when
  `level_step <= 0`). This covers the item / default / interaction skills: 422 item use-skills
  with `mana_level_md > 0` are ability 0, so their `LevelMana` argument is the character level.
* A non-character caster (an NPC) keeps the effective level the old `Skill.ManaCost` used as
  its `LevelMana` argument, the unit level or `ab_level_override`, whatever the `level_step`;
  `T` is derived from that level. This keeps the former NPC cost (e.g. skill 23693 at NPC
  level 54 still costs 11638) and applies only the new `casting_inc` term on top. The 68
  readings are player-side (the client tooltip is player-side), so the NPC-side cost is
  **capture-pending**; deviation from the C#: `Skill.ManaCost` scaled every caster by
  `GetAbLevel(AbilityId)`, which for NPCs is the same level this keeps.

### Plot `ManaCost` special effect (type 39)

The C# charged `value1 + value2 / 6.35` (with a TODO: "value2 is based on skill level somehow"). The 1.2 data shows `value1` = the skill's `mana_cost` and `value2` = `mana_level_md * 100` for 59 of the 68 plot-only skills that carry the effect (e.g. 11854: md 1.55, effect 0/155; 18132: mana_cost 30, effect 30/0). The 1.2 client shows the cost in the skill tooltip as `mana_cost + mana_level_md * (1 + T) * LevelMana(L)` (see the skill rank section above; Ghidra `aa12u`, x2game `FUN_3941e750` -> `manaCost` of `FUN_394ace10`), and the cast pre-check (`validation.rs`) already requires exactly that amount. The effect therefore charges `value1 + value2 / 100 * (1 + T) * scale` with the same `(L, T)` pair as `skill::mana_cost`, so the plot charge cannot decide a cast differently from the pre-check. The scale is no longer hardcoded: `skill::mana_level_scale` evaluates the character `unit_formulas` rows of `compact.sqlite3` (kind 12 `LevelDps` = `(level * 1.6 + 8) * 3`, id 12; kind 13 `LevelMana` = `level_dps / 3.65`, id 13) at `L` through the precompiled formula expressions. The divisor `value2 / 100` was re-checked against the data: of 84 distinct (skill, `ManaCost` effect values) rows reached through the skills' plots, 62 have `value1 == mana_cost` and `value2 / 100 == mana_level_md`; the other 22 have `mana_level_md` 0 or 1 with `value2` 1 or 100-600, i.e. an effect-only charge that the same formula takes at face value. Deviation from the C#: the charge side is inferred from the tooltip/data (no retail charge capture); the 9 plot-only skills whose effect differs from the skill row follow the same rule.

## Achievement producers (live wiring)

The achievement subsystem has no 1.2 C# manager; the behaviour follows `rust/docs/achievement-hook-audit.txt`
(which lists every record kind, its producer and what is still unwired). Deliberate choices:

* **Entry snapshot** runs from `CSNotifyInGamePacket` (after the trial login step): back-credit, the
  `SCAchievementsPacket` list, then pending rewards.
* **Producers** report through `systems::report_achievement(s)` (`server.systems.hooks.achievements`).
  The economy operation lock is not re-entrant, so events raised while the thread holds it (an
  `ItemTxn`, a quest operation) are queued and flushed when it is released; events raised under the
  team / family / expedition managers' own locks use `refresh_achievements_later` and run when the
  packet handler returns. `economy::OpsGuard` tracks the holding thread.
* **MY_GOLD (k14)** reports the absolute carried gold (`money / 10000`, high-water) after every
  upward money change through `inventory::change_money` (the item-operation variant queues
  `ItemFx::Money`; the `Out::Direct` callers report through the achievement deferral) and through
  the quest reward deposit and equipped-fish sale. The report runs from `fx::flush` / the packet
  end, after the character and economy locks are released, and is throttled to one per character
  per second; a value suppressed by the throttle is kept and reported by a later flush. Money
  paths that assign `Character::money` directly (player trade, buyback, expedition refunds, GM
  commands) do not report; the entry snapshot and the next money change still back-credit the
  high-water value.
* **Repeatable quests** (kinds 27/28) credit +1 per completed run on top of the entry back-credit of 1
  per distinct quest (audit Q3). Category counts therefore count repeats live.
* **ENCHANT_FAILURE** credits only a plain FAIL result; BREAK and DOWNGRADE are not proven to be "failures"
  (Q4). **GET_LOOTPACK** credits doodad loot packs only (Q6). **MAKE_HOUSING** credits the owner who
  performs the last build action. **Auction** and **jury** credits go to online characters only.
* **GET_FACTION** is state-derived (entry back-credit plus the crossing of the infamy threshold in
  `CrimeManager::add_crime_points`); other faction changes do not credit (Q8).
* **GET_ITEM_TYPE (k29)** counts only genuine acquisitions. `ItemFx::Acquired` now carries the
  `added` unit count and an `AcquiredSource`: a newly created item (loot, craft product, quest
  reward, shop purchase), new units merged into an owned stack, a mail take, or an item received
  from another character (trade). Moves between the character's own containers (bag <-> bank, a
  house coffer, re-slotting) and merges of already owned stacks are tagged `Transfer` and never
  credit, so warehouse shuffling cannot complete a collection achievement. The report runs from
  `economy::fx::flush` with the same lock rules as the other producers (queued while the economy
  lock is held). All 216 authored records use the `-1` any-grade wildcard, so the event carries no
  grade (a `value2 >= 0` row would need one). `KILL_SLAVE (k26)`,
  `MY_DISHONORABLE_POINT (k49)` and the siege results (k57-60) stay unwired (see the audit for
  why). `SCAchievementResetedPacket` (0x1ff) has no 1.2 trigger and is deliberately never sent.

## PvP duel fixes (duel disconnect, GM faction, quit relog)

* **Duel participant leaves** (`DuelManager::on_participant_leave`, called first in `session::save_and_remove_from_world`): the C# had no handling, so the opponent stayed in the Red / Blue faction with the duel registered until the end timer, and `DuelAccepted` answered "already started". A started duel now ends at once as a **draw** (`SCDuelEndedPacket` `det` 3; the C# end timer instead decided by remaining HP) for both sides and restores both factions, so a disconnect is neither a win for the opponent (no `WIN_DUEL` credit to farm by dropping) nor a surrender; a pending request is dropped. No 1.2 oracle for the retail result.
* **`/setfaction` is persistent** (`faction::set_faction_persistent`): a GM change goes through the `Character.SetFaction` effects (housing, guild, party) and ends any temporary faction, so `characters.faction_id` stores it. Only duel and battlefield factions are temporary.
* **Quit game frees the account** (`leave_world_task`, target 0): the C# kept the connection and the `AccountManager` entry until the client closed the socket or the 30-60 s dead-connection sweep did, so a relog right after quit got "enter world denied, reason 1" (`already_connected`). The server now unregisters the account and closes the connection after `SCLeaveWorldGranted`; `AccountManager::remove_if_owned` makes a late close of the old connection leave a newer login of the same account alone. Saves are unchanged (still under the transaction lock, all-or-nothing).
* **Disconnect after a leave does not save again** (`session::on_disconnect`): a connection keeps its `active_char` after quit / character select (as in the C#), so the closing socket re-ran the departure and re-saved a character that was already saved and removed, and that may meanwhile be online on a newer connection. The disconnect now returns early when the character is no longer the live world entry.
* **Battlefield buffs are never saved** (`Buff::session_only`, set by `instant_game::apply_battlefield_buffs`): `battle_field_buffs` 6266 (1 200 000 ms) and 8000007 (3 600 000 ms) have save rule 1 and a duration, so autosave or a logout to the character select persisted them; they were only removed in leave / disconnect. `Buffs::should_persist` is false for session-only effects.
* **Battlefield assists / duel scoring data**: `game_score_rules` has no assist (204) row for rule set 28 and no rows at all for rule set 24, so the data authors no assist score there and none is invented.

## Duel fixes (2026-10-20)

Oracles: the 1.2-era C# (`git show a2ab80abd^:AAEmu.Game/Core/Managers/DuelManager.cs`) and the 10.x C# `DuelManager` (its source is not in this tree or on this machine: statements below that cite 10.x are from the earlier research notes and cannot be re-checked here). Rust: `systems/social/duel.rs`, `packets/duel.rs`.

- **Countdown timing** (deviates from the 1.2 C#, which sends `SCDuelStarted` and `SCDuelStartCountdown` together from `DuelStart`, `DuelManager.cs:132-135`). `SCDuelStartCountdown` is now sent to both at the acceptance and `SCDuelStarted` follows `DUEL_START_DELAY` (3 s) later, like 10.x `DuelAccepted` (the client runs a fixed 3000 ms countdown, see the 10.x `SCDuelStartCountdownPacket` remarks). The 1.2 client has the `OnDuelStartCountdown` UI event next to `OnDuelStarted` (`x2game.dll`), so it handles the packet on its own.
- **Refuse after the start** (deviates from both C#): `DuelCancel` for a duel that already started ran `DuelCleanUp`, which only dropped the registration and left the flag, the Red / Blue factions and the client duel state behind (later `DuelStop` then found nothing). A refuse / cancel is now honoured only for a pending challenge and only from one of its two participants (the participant check is 10.x `DuelCancel`); otherwise it is logged and ignored, so the started duel ends through the normal stop path.
- **Challenge validation** (10.x `DuelRequest`, `DuelManager.cs:96-136`): self -> `BadDuelSelf` (513), unknown / offline target -> `BadDuelTarget` (504), challenger already registered -> `AlreadyInDuel` (505), target registered (a duel or a pending challenge) -> `OtherAlreadyInDuel` (506), all sent to the challenger. Not in any oracle: a dead target (hp <= 0) also gives `BadDuelTarget`. Neither C# `DuelManager` has a distance or location rule for the request, so `DuelBadLocation` (516), `DuelOtherBusy` (515) and `TargetTooFar` (517) are never sent (no rule invented). The data does carry duel-related client rules that the server does not enforce: `skill_reqs` 78 ("cannot request a duel to a target who already started a duel", `tag_id` 566, tag "Duel") - the server's `OtherAlreadyInDuel` check covers it - and the `cannot_duel_during_force_attack` error id in the client's error table; neither gives a distance or location condition. The check-and-register is one critical section. The ids are the 1.2 client's: its error table lists `bad_duel_target`, `already_in_duel`, `other_already_in_duel`, `target_rejected_duel` ... `bad_duel_self`, `duel_other_busy`, `duel_bad_location` in that order (`x2game.dll`, reversed in memory), matching 504-516.
- Because a pending challenge now blocks both players, an unanswered challenge expires after 30 s (`REQUEST_TIMEOUT`, an UNSOURCED constant: no compact table or 1.2 C# value; 30 s is the figure recalled from 10.x `RequestTimeout`/`DuelRequestExpired`, which cannot be re-checked here; the challenger gets `TargetRejectedDuel`). `DuelAccepted` also follows 10.x in answering `BadDuelTarget` for an unknown challenge and refusing an accept from anybody but the challenged character.
- `SCAreaChatBubble` type 543 (research only): both C# versions send `(true, challengerObjId, 543)` from `DuelStart` (1.2 C# `DuelManager.cs:133`, 10.x `:303`; the 10.x packet is unchanged). In `compact.sqlite3` `bubble_effects` id 543 (kind 2) is a Nuia lore line, part of an unrelated 540-546 series, and no `bubble_effects` row mentions a duel, so the value is probably a guess carried over from a packet capture; no oracle for another value exists, so it is unchanged.
- **Atomic challenge release / dead challenger.** `refuse`, the request timeout, the leave of an unanswered participant and the accept now compete for one `released`/`started` transition under the duel's state lock (`Duel::claim_release`): exactly one wins and owns the clean-up, so a stale refuse or expiry can no longer strip the flag state / Red-Blue factions of a duel that was just accepted, and an accept after a release is answered `BadDuelTarget`. An accept whose challenger died meanwhile releases the challenge with `BadDuelTarget` (the request-time dead check, repeated; no new error id).

## Bevy protocol audit (October 19, 2026)

The Bevy 1.2 client (`AABevy1.2`) is the main client; its reverse-engineering notes
(`docs/aa-net.md`, digest M1..M23) found layout suspects between our encoders and the original
1.2 client's readers. Oracle used here: the original reader in the unpacked 1.2 client
(`x2game_unpacked.dll`, Ghidra, addresses below), decompiled again for each fix marked
*verified*; the rest are marked *per Bevy RE notes*.

| # | Fix | Oracle |
|---|---|---|
| M1 | `SCHouseState` / `SCMyHouse` model is one fixed `i32` (was PISC pair, shifting every later field) | verified: `39897460` reads it with stream slot `+0x4c`, the same slot as `allstep`/`curstep`/`skillCount` |
| M2 | `SCCancelInstantGame` carries only the `i16` error (the branch byte and padding are gone; `clear_queue` and `with_error` are indistinguishable on the 1.2 wire, the handler ignores the value anyway) | verified: serializer `397c3bf0` transfers one `i16` (slot `+0x50`) |
| M3 | The empty faction lists use their real opcodes: `SCFactionList` `0x008` (was `0x007` = `SCTrionConfig`) and `SCFactionRelationList` `0x009` (was `0x008` = `SCFactionList`, which also carried a stray byte) | opcode table; relation body `[bool][u8 count]` kept from the non-empty packet. The C# hard-coded the wrong ids |
| M4 | `SCResponseUIData` trailer is the UTF-8 byte count + 1 (was UTF-16 units + 1; truncated non-ASCII data) | per Bevy RE notes (`391e57e0` -> `memcpy` / `xlcommon::Data::Load`) |
| M5 | The slave item detail (tag 2) is always 29 bytes: `RepairStartTime` is an i64 (0 when unset, the same bytes as before) plus four i32 zeros (was i32 + five i32 when unset, i64 + five i32 = 33 bytes when set, which desynced batched item packets). The split of the 24 trailing bytes is inferred | width verified: `3961edf0` tag 2 = `0x1e - 1` = 29 bytes; field split per Bevy RE notes |
| M6 | `SCMateEquipmentChanged` now carries a real success flag; the rejected-move packet writes `false` (the C# always wrote `true`) | verified: `OnMateEquipmentChanged` `391db200` passes the flag at `+0x330` to `394e1c10`, which copies the item into the bag cache only when it is set; entry/record readers `397d12a0`/`397d1340` match our layout |
| M7 | `CSSpawnSlave` reads container + slot only when the item id is nonzero (37/39-byte bodies; was always 41). The handler still only logs | per Bevy RE notes (client writer) |
| M8 | `SCCraftFailed` never writes more than 20 entries (count clamped to 0..=20). Still unsent | per Bevy RE notes (`391e0840` -> `394d2370` clamp) |
| M9 | A missing trial reporter name is written as an empty string (the C# omitted the length) | per Bevy RE notes (string always has its `u16` length) |
| M10 | Bounty rows write `notGuilty` before `guilty` | verified: row reader `397bebf0` reads `notGuilty` (`+0xc8`) then `guilty` (`+0xcc`) |
| M14 | `SCUnitState` of a character sets flag bit3 (`0x0008`) while force attack is on, so late observers see Bloodlust | verified: the client chain is `397c9480` -> `398a1f90` -> unit+0x3bbc (the force-attack byte) for bit3 of the `u16` flags |
| M15 | `SCCooldowns` has a full writer (`i32` count + `[id, total, remaining]` u32 triples, skills then tags, 150 per list). It is sent after `SCCharacterState` for restored cooldowns and after a cast only where the client cannot start the timer itself (a `plot_only` cast with a plot, which sends no `SCSkillFired`, or a `timing_id == 0` skill), with the shared tag entry when the skill has one. The client starts ordinary skills (`toggle_buff_id == 0 && timing_id != 0`) on `SCSkillFired` and toggle skills on `OnBuffRemoved`; the `Cooldown` special effect is client-applied. Only end times are stored, so `total` equals `remaining` | verified: `397b64d0` -> `397b2350`; `OnCooldowns` `391d43c0` -> `39899d40` appends without de-duplication and caps at 150, and is what `GetActionCooldown` (`3943f310` -> `3989a3e0`/`3989a290`) reads. Template `+0x1bc` = `timing_id`, `+0x1c0` = `toggle_buff_id` (loader columns 0x87/0x88), `DAT_3a17770c` = 0. Cooldowns are persisted in `character_cooldowns` (absolute Unix-ms expiries) and restored by `Character.Load`; the C# kept them in memory only |
| M24 | Slave-equipment items (`item_impl` 28, e.g. item 29725 on slave template 21) are written as detail type 0 (`Invalid`) with no detail body; the server no longer sends `ItemDetailType.SlaveEquipment` (10, 12 opaque bytes), which desynced every field after the item (the Bevy client rejected `SCUnitStatePacket` 0x069 and the original 1.2 client misparsed it) | verified: the 1.2 item detail reader `3961edf0` has cases only for tags 1..9 (bodies `0x38, 0x1e, 7, 10, 0x19, 0x11, 0x11, 9, 5` minus the type byte) and no case 10, so tag 10 reads no body; the C# 1.2 `ItemDetailType` numbering has no 10 either |
| M25 | `SCUnitState` `Inventory_Equip` for slaves writes full items in slots 19..25 (the C# wrote only the template id for every unit type). A hull part there (Harpoon Clipper, template 14, part 29742 in slot 23) made the original 1.2 client read the following bytes as item fields and run off the packet (`sc error; cur=105 prev=97`, `Packet Error !!!`); Housing (3), Mate (5) and Character (0) keep template-only body parts, Npc (1) keeps `templateId, id, grade` outside 19..25 with a full-item Cosplay slot (27), Transfer/Shipyard write only 19..25 | verified: the client equipment reader `3961fa20` reads 19..25 as a template-only `u32` only when `baseUnitType != 2`; for a slave every slot goes through the full item reader `3961f500` |
| M26 | `SCBotSuspectArrested` writes two `u32` ids before the names: `sheriffId, suspectId, sheriffName, suspectName` (the C# wrote only the two strings, like `SCBotSuspectReported`). The packet is still unsent: the bot arrest flow is capture-pending | verified: 1.2 reader `FUN_397b6d10` (`dll.c:1710108-1710128`) takes two `u32` (stream slot `+0x3c`) then two strings capped at `0x80`; handler `FUN_391ddd80` compares the first id with the local unit id (`FUN_3907cb20`) |
| CS | The client CS table entries missing from `opcodes::cs` were added: `0x11f CSRequestSetBountyMoney`, `0x120 CSSendBountyUpdate`, `0x130 CSRankSnapshot` (already handled through a local const, now the table entry), `0x133 CSChangeAutoUseAAPoint` | the Bevy `opcodes.rs` (client table); 0x130 additionally by the RE notes in `ranking_packets.rs`. `0x11f`/`0x120` now answer with `SCSetBountyPermitted` / `SCSetBountyDone` and place the pledge (see "Wanted-player bounty"); `0x133` sets `Character.auto_use_aa_point` and answers with `SCItemTaskSuccess` (`ItemTaskType` 110 carrying the one-byte `ChangeAutoUseAAPoint` task, action 18); the 1.2 C# had the task class but no 0x133 handler, so this reply is inferred, not observed on a live client |

Doc-only items:

- **M11** scheduled rewards: the client keeps at most 16 rows (verified: `397b0190` clamps the
  count to `0x10`, row `3961f100` = `u32,u8,i32,i64`). We still write up to
  `MAX_ITEMS_IN_PACKET` (32); the extra rows are ignored in a plain frame, and the packet is never
  batched, so it is left unchanged.
- **M12** list caps (family > 8, achievements > 50, radar > 10, battlefield members > 50, expedition
  chunks of 20): per Bevy RE notes; our senders already chunk at those sizes. Not re-verified.
- **M17** (f32 vs f64 arithmetic of pet revive/repair cost), **M18** (ranking period: UTC half-open vs
  local inclusive), **M19** (`SCUnitMovements` unsent), **M21** (new factions get
  `political_system 1`, native rename needs 0), **M23** (actability downgrade clamp): per Bevy RE
  notes, not re-verified and not changed; each needs a reproduction against the real client before
  a fix. M20 (party leave/kick log-only) is fixed: the direct packets run `AskRiskyTeam`.

## Portal use range (2026-10-21)

* `CSUsePortal` needs the character within `World.PortalUseRange` (default 5 m) of the gate NPC
  and `CSNaviTeleport` within the same range of the navi doodad, measured like `GetDistanceTo`
  (2D, minus the model radii). The C# `PortalManager.UsePortal` and the `CSNaviTeleport` handler
  checked nothing. The default covers the gate placement (`open_portal_effects.distance` = 3 m;
  the 1.2 `ui_texts` 1291 tooltip says 5 m) and matches `portal::NAVI_USE_RANGE`; the 1.2
  client's only interaction-range CVar, `max_interaction_doodad_distance`, defaults to 2.1 m for
  doodads (Ghidra `proj-aa12u`, `FUN_390f7b40`), so 5 m is the server's slack for movement and
  latency. Refusals answer `SCErrorMsg(TooFarAway, 154)` and log one WARN with the character and
  distance.
* `CSUseTeleport` still charges the fare the client sends: every 1.2 `npcs.teleporter` row is
  false and no fare column or table exists, so there is nothing to compute the amount from.

## Dash heading at the end point (2026-10-20)

`SkillController::step` turned the unit toward `heading(new, end)`. On the last step `new` equals
`end` up to float rounding, so `atan2` of a near-zero vector picked an arbitrary direction (pi or
0, depending on the tick's `dt`): the unit could flip to face backwards on the final tick and the
last movement packet carried that yaw (seen as a Linux-only failure of
`dash_moves_npc_and_broadcasts_movement`). The final step (`travel >= target_dist`) now lands
exactly on the end point, and when the new position is within float noise of it (a tolerance of
4 ulp of the coordinate magnitude, at least 1e-3 m: the f32 ulp is ~0.002 m beyond 16384, so a
fixed 1e-3 m was not enough on large maps) the heading of the step itself (`heading(cur, end)`)
is used. The C# original has the same degenerate `atan2`.

## Quest `ObjMateLevel` cleanup consumes on completion (2026-10-20)

C# `QuestActObjMateLevel.CalculateObjective` deleted the `SummonMate` item (when `cleanup` is set)
the moment the level check passed. `quests::events::on_mate_spawned` now dispatches `MateLevelUp`
on every mate spawn, so summoning a level-50 mate while another objective of the step was open
(quest 5430, Stormdarter plus the essence gather) destroyed the pet item and the next evaluation
reset the objective. The Rust objective only reads state; `act_quest_cleanup` consumes one item of
`item_id` when the quest completes, preferring the item whose mate level satisfies the act.

## Skill source-item consumption over all applied effects (2026-10-20)

C# `Skill.cs` read `consume_source_item`, `consume_item_id` and `consume_item_count` from
`lastAppliedEffect` only. 13 skills in `compact.sqlite3` put the consuming effect first and an
Interaction / Spawn / NpcSpawner / Buff effect last (sealed mount items, skill 22465 for
"Lightningstruck Swift Tempest"), so the item was never consumed while GainItem kept granting
the mate. The Rust port consumes the source item once, using the last applied effect that carries
`consume_source_item`, and the template item once, using the last applied effect with a
`consume_item_id`; `use_skill_as_reagent` still uses the last applied effect's count.

## Trade item integrity and craft materials (2026-10-20)

- `trade::add_item` (C# `TradeManager.AddItem` only checked `item.Count >= amount`) now also
  cancels the trade for a soul-bound item or an item outside the bag (the flag check matches mail
  and auction), and `preflight_items` refuses an offered item that left the bag or became bound
  after the put-up. Repeated put-ups of one stack stay allowed up to its count, as upstream decided.
- Craft materials: the C# counted `Inventory.GetItemsCount` (bag + equipment + bank) but consumed
  from `Bag` only and ignored the result, so a craft with materials in the bank produced the
  product for free. The port counts the bag only (start and again in `hand_out_products`, before
  any product is handed out) and refuses with `NOT_ENOUGH_REQUIRED_ITEM` otherwise. Labor is
  still charged by `Skill.EndSkill` as before.

## Guild invitation and rank guards, loot master membership (2026-10-20)

Deviations from the 1.2 C# `ExpeditionManager` / `TeamManager`, which trusted every client id
(found by a code audit; reproduced in flow tests, no retail oracle for the missing checks):

- `ReplyInvite` joined any guild whose id the client sent. `Invite` now records a pending
  invitation per invited character (guild id, 60 s expiry like party invitations); the reply must
  match it, is consumed by any answer, and a character already in a guild cannot join another.
- `Kick` and `ChangeMemberRole` only checked the caller's policy flag (and `role > new_role`). Both
  now also require the caller's role to be strictly higher than the target's current role, so the
  leader (255) can neither be expelled nor demoted, and equals cannot act on each other. The
  client `Kick` packet keeps this guard; `Character.SetFaction`'s guild removal does not go
  through it (see `leave_forced` above), so an owner can still leave on a
  faction change without allowing a client self-kick.
- `ChangeLootingRule` accepted any character id as loot master. It must now be 0 or a member of the
  team; otherwise only that field is ignored (logged). Over-head markers are unchanged: their
  target is any unit or object, not a team member.
- `ChangeExpeditionRolePolicy` required the editor's `Expel` flag and copied only `Name`, `Invite`,
  `JoinSiege`, `Promote` and `Expel`, so an edit silently dropped `DominionDeclare`, `Dismiss`,
  `Chat`, `ManagerChat` and `SiegeMaster` from the stored policy and the broadcast (the 1.2 packet
  carries all nine flags). The Rust first stored the whole policy; the later "Guild functions
  (2026-11-05)" evidence (the 1.2 role-policy tab edits only the five dialog fields) supersedes
  that: the other four keep their stored value now. It follows the 1.2 client's
  role-policy tab instead of the C# right: `expedition_management.alb` decorates each policy with
  `SET_POLICY = (role == 255)`, and `expedition_management_role_policy_tab.alb`'s
  `CanChangePolicy` requires that flag plus a lower target role. Only the owner may edit, and the
  owner's own policy is not editable. **Capture-pending:** the rule comes from the client UI, not
  from a 1.2 server capture; the C# `Expel` check is not mirrored.

## Area team invitations and invitation error replies (2026-11-03)

The 1.2 client has a separate area-invitation flow: `X2Team.InviteArea` sends
`CSInviteAreaToTeamPacket` (`FUN_393053a0`, 0x7b), the server answers each nearby player with
`SCAskToJoinTeamAreaPacket` and the inviter with `SCTeamAreaInvitedPacket(r, s)`, and the
client's `OnAskToJoinTeamArea` handler carries `isArea=1` through the popup so the accept /
decline answer is `CSReplyToJoinTeamPacket` with `isArea=1` (`FUN_391d8060`, `FUN_391d8800`,
`FUN_39308ec0`; `FUN_393075a0` writes 0x7c with the stored `isArea` flag). The client message says "now inviting characters
into raidteam in range of 100 meters". The C# `TeamManager` never sent either SC packet and
dropped an `isArea` answer in `ReplyToJoinTeam` (`// TODO`).

Deviations from the C# (the server side has no capture; the rules come from the client
decompile and the error-code research in `D:/aaemu-audits/gap-audit-2026-11-02/research/area-invite.md`):

- `InviteAreaToTeam` sends `SCAskToJoinTeamArea` to every eligible player and
  `SCTeamAreaInvited { r: 0, s: true }` to the inviter. The C# called `AskToJoin(..., false, ...)`,
  so the area packet was never used. The `isParty` flag in both the invitation and the SC packet
  is the inviter team's actual `IsParty` (the request's flag when the inviter is not in a team);
  the C# stored the team's flag in the invitation but sent the hardcoded `false`.
- Eligibility: within 100 m (2D, strictly less, as `Region.GetList`/`GetAround`), not in a team,
  friendly or neutral relation, not blocked, not hostile. The C# checked the first two only;
  the block list was already enforced by the Rust `AskToJoin`.
- An `isArea` reply goes through the normal join path. The C# removed the invitation and
  returned (nobody joined).
- **Area cooldown (EVIDENCED in the client, INFERRED server-side):** the client's button is
  gated for 30000 ms (`FUN_39304ef0`, constant 30000.0) and the locale text says
  "(30 sec cooldown)". The server keeps the same per-inviter cooldown and answers a request
  inside it with `SCTeamAreaInvited { r: remaining_ms, s: false }`; the client formats
  `r * 0.001` as "Can use area invite after %.1f seconds". With `s = true` the client ignores
  `r` (the earlier port sent a count there; it is not displayed). `r` is not a count.
- The invitation is keyed by the target: a new named or area invitation replaces a pending
  **area** invitation (a refused/ignored area invite must not block a real invite for 60 s); a
  new area invitation skips a target with a pending **named** invitation silently (the C#
  answered `SCRejectedTeam` with the empty area target name).
- Pending invitations are swept every 500 ms after their 60 s timeout and capped at 10,000. The
  C# removed an invitation only when the target answered, so an expired one blocked every later
  invitation to that target. The inviter's pending invitations are also dropped when the
  inviter logs out / leaves the world (the invitation holds its `PlayerRef`).
- The inviter's team always comes from the team manager's current membership (the client-sent
  `teamId` is ignored; a crafted `0` used to bypass `TEAM_NO_RIGHTS`). On accept the inviter's
  current team must still be the invitation's team (when non-zero) and the inviter must still
  have the right to invite. A reply must name the invitation's owner, and its team id when
  non-zero, so a stale popup cannot consume or reject someone else's invitation.
- The client's "refuse area invitation" option is client-local: `X2Team.SetRefuseAreaInvitation`
  writes the `DAT_39d44ed8` flag and no packet ever carries it; `OnAskToJoinTeamArea` drops the
  popup while the flag is set. The server cannot know it and always sends the invitation; a
  refusing client shows nothing. The live check therefore models refusal by not answering.

Error codes (all to the invitee unless noted; ids are the 1.2 `ErrorMessageType`/locale keys,
`x2game.dll.c:1135910-1135948`):

| Case | Code | Evidence |
|---|---|---|
| Inviter may not invite (not owner; party member; unmarked raid member), at invite time | 72 `TEAM_NO_RIGHTS` to the **inviter** | EVIDENCED as the client's own check (`FUN_39428370(0x48)` in `FUN_393053a0`); sending it from the server is INFERRED |
| Invitee already in a team, at invite time | 77 `TEAM_INVITEE_IN_TEAM` to the **inviter** | INFERRED: the locale text is inviter-facing ("You can't invite anyone in another group.") |
| Invitee already in a team, at accept | 82 `TEAM_JOIN_ALONE` to the invitee | INFERRED: the client's own accept check sends 82 for this state (`FUN_393075a0` -> `FUN_39428370(0x52)`, `x2game.dll.c:626269`) |
| Inviter's team gone, or the inviter moved to another team, at reply | 79 `TEAM_INVITOR_MOVED` to the invitee | INFERRED from the Korean locale text ("the inviter moved to another group"); the English text mistranslates it as "the player you invited" |
| Inviter offline at reply | 78 `TEAM_INVITOR_OFFLINE` to the invitee | INFERRED from the Korean text; a logout drops the invitation, so this is a race guard |
| Accept or reject with no pending invitation | silent | UNKNOWN: no client or server evidence; 80 ("Invite declined") reads as if the inviter refused, and factions' "no invite exists" code has no team equivalent |
| Hostile target | 80 `TEAM_INVITE_REFUSED` to the inviter | from the 1.2 C# `TeamInviteRefused` (capture-pending) |

The C# TODO codes 71 (`TEAM_LOADING`) and 77-to-the-invitee are no longer used. The 10.x
community server (a reimplementation, not an oracle) sends 79 to the invitee when the inviter is
in another team and 82/77 in the same split, which agrees with the inferred mapping.

## Guild and party fixes (2026-11-04)

Found by the player bots (`aaemu-playerbots`, guild / party scenarios) and reproduced in flow
tests (`systems/social/flow_tests.rs`). The 1.2 client's `ui_texts` carry the text of every
`ErrorMessageType` used below (key = the C# enum name in upper case), which is where "the right
error" comes from; there is no retail oracle for which code the original server picked, so each
is the closest text.

- **Hard disconnect = clean leave socially.** The C# `OnDisconnect` never logged the character out
  of its guild / family: guild members saw it online for ever, `last_leave_time` stayed at the
  login time, the family kept the dead `Character`, and the dead session stayed in every chat
  channel (so a party / guild / zone channel never became empty). `world::session::on_disconnect`
  now runs `social::session::on_leave_world` (chat channels, family logout, pending guild
  invitation, guild logout with the `SCExpeditionMemberStatusChanged` to the guild, which is also
  what `leave_world_task` runs) and sets `leave_time`, for a character that still is the live
  world entry. Kicks and server-side drops end in the same `on_disconnect`.
  A connection keeps its `active_char` after a leave, so a later disconnect of a session that
  already left is skipped (the character may be online on a newer session). Friends and the team
  were already covered by `set_online(false)`. Not covered: the panic fallback
  `force_release_connection` (only removes the world entry) and the GM `/disconnectme`, which
  detaches the character on purpose like the C# (it stays in the world as a ghost).
- **Guild leader cannot leave.** `ExpeditionManager.Leave` had no owner check, so the guild was left
  without a role-255 member. The leader now gets `EXPEDITION_OWNER_CANNOT_LEAVE` (310, "you can
  leave after delegating the leader position"); it must hand the guild over (`ChangeOwner`) or
  dismiss it. A character that is not in a guild gets `ONLY_EXPEDITION_MEMBER`. (Deleting the
  leader's character was already refused in the lobby.)
- **Party invitations expire and die with a session.** The 60 s timeout the C# only applied when the
  *answer* arrived is now also enforced by the 500 ms tick (`TeamManager::expire_invitations`; the
  inviter gets `INVITATION_CANCELED_TIMEOUT`), an expired invitation no longer blocks a new
  invitation of the same target, and `TeamManager::set_offline` (every way of going offline)
  drops the invitations the character sent (their targets get `TEAM_INVITOR_OFFLINE`) and
  received. Answering an invitation that is gone gets `TEAM_JOIN`; a late answer gets
  `INVITATION_CANCELED_TIMEOUT` (and the inviter the usual `SCRejectedTeam`).
- **Team chat on every join / leave path.** A member joining an existing team joins the raid (not
  a party) and its party chat channel. `AskRiskyTeam` made the *acting unit* leave the channels,
  so the kicker lost its party chat while the kicked member kept hearing it; the member that
  leaves / is kicked leaves them now (before it is removed, the slot names its party).
- **One team at a time.** Accepting an invitation while in a team is refused with `TEAM_JOIN_ALONE`
  (the invitee) and `TEAM_INVITEE_IN_TEAM` / `TEAM_INVITEE_MEMBER` (the inviter); the invitation
  itself is refused up front for such targets. The team of an invitation is the inviter's own
  (the C# trusted the client's team id, so a member sending team id 0 invited without the
  owner's right, and a stale id could name somebody else's team); a client team id naming another
  existing team is `TEAM_NO_RIGHTS`.
- **Feedback instead of silence.** Party: unknown / offline invitee `TEAM_INVITEE_OFFLINE`, yourself
  `TEAM_YOURSELF`, not the owner (or marker of a raid) `TEAM_NO_RIGHTS`, full team `TEAM_FULL` to
  the inviter (at the invitation, and again at the answer to both sides; an area invitation says
  it once). Guild invite: not in a guild `ONLY_EXPEDITION_MEMBER`, role without the Invite right
  `FACTION_INVITE_RIGHTS`, offline / unknown name `FACTION_INVITE_OFFLINE`, yourself
  `FACTION_INVITE_SELF`, same guild `FACTION_INVITE_SAME`, another guild
  `EXPEDITION_OTHER_EXPEDITION`, and a target that is
  deciding on another invitation `EXPEDITION_OTHER_INVITATION` (the C# overwrote it; a pending
  guild invitation now blocks a second until it is answered or expires). Guild reply: no / wrong
  invitation `FACTION_JOIN_NO_INVITOR`, expired `INVITATION_CANCELED_TIMEOUT`, already in a guild
  `EXPEDITION_ALREADY_MEMBER`, disbanded guild `FACTION_JOIN` (a disbanded guild stays in the
  list, the C# let the reply join it); a refusal tells the inviter `FACTION_INVITE_REJECTED`.
  Guild kick: not in a guild `ONLY_EXPEDITION_MEMBER`, no Expel right or not a higher rank
  `FACTION_KICK_RIGHTS`, not a member `EXPEDITION_NO_TARGET` (a character "kicking" itself, as
  `Character.SetFaction` does, stays silent).
- Not changed here (silent as in the C#; fixed afterwards, see "Guild functions (2026-11-05)"):
  the other guild management failures (`ChangeOwner`, `ChangeMemberRole`,
  `ChangeExpeditionRolePolicy`, `Disband`), the nation / dead-target checks of a guild invite, and
  the SetFaction removal described under "C# bugs noticed".

## Guild functions (2026-11-05)

The remaining guild (expedition) functions, found by comparing the C# with the 1.2 client's UI
(`x2ui/expedition`, `dialoghandler`, the mirrored validators in `x2game12.c`). Error codes are
`ErrorMessageType` values whose texts the client's `ui_texts` carry. Flow tests:
`systems/social/guild_flow_tests.rs`; live suite `rust/verify/guild-funcs.vtest`.

- **Rename (`CSRenameExpedition`, C#: logged only).** The C# never sent `SCExpeditionShowRenameUI`
  and the data has no rename item, skill or cost, so the right is a server policy: the column
  `expeditions.allow_change_name` (also sent as `SystemFaction.allow_change_name`, written as 0
  before) says the leader may rename once. A GM grants it (`/allowguildrename <guild id | name>`):
  the leader gets `SCExpeditionShowRenameUI` at once if online, else at its next login
  (`prompt_rename_if_allowed` in `NotifyInGame`), and every client gets the faction record again
  (`SCFactionList`). The leader sends `CSRenameExpedition {id, name, true}`: not in a guild 808,
  not the leader of that guild 807, no pending right 32 (`FACTION_RIGHTS`; the nation variant
  `is_expedition = false` gets 32 as well, its own rename stage owns it), a pending right but no
  `SCExpeditionShowRenameUI` offer (a forged packet) 307 (`EXPEDITION_BAD_STATUS`, the server-side
  guard from "Guild rename"), a name that breaks the
  rules reopens the client's dialog with `SCFactionRetryRename(true, code, name)`. Success: name
  saved, right cleared, `SCFactionRenamed(id, name, by_gm = false)` to every online player (the
  client renames its faction record, clears its own flag and refreshes name plates) and the guild
  chat channel is renamed. `/renameguild <guild id> <new name>` renames directly with
  `by_gm = true` (the right stays). Both commands are access level 100 and apply the create
  rules. The name rules are one function (`validate_name`) for create and rename, in the client's
  order: UTF-16 length > 32 `EXPEDITION_NAME_LENGTH` 297, the configured regex
  `EXPEDITION_NAME_CHARACTER` 296, the character-name block list (`blocked_texts.check_name`, as
  the lobby applies it) `EXPEDITION_NAME_BADLIST` 298, a name another live guild uses
  (case-insensitive; dismissed `$deleted-` guilds do not count) `EXPEDITION_NAME_EXIST` 299. The
  C# only had the first, second and an exact-match duplicate check (dismissed guilds included).
- **ChangeOwner (C#: silent).** Not a member 808; caller not the leader 32; to oneself 327
  (`EXPEDITION_CHANGE_OWNER_SELF`); to somebody outside the guild 328; during a siege 568
  (`EXPEDITION_OWNER_CANNOT_CHANGE_DURING_SIEGE`, the same siege test the dismissal uses).
- **ChangeMemberRole (C#: silent).** Not a member 808; no Promote right, a target not strictly
  below the caller, a new role not strictly below the caller, a new role without policy or the
  leader's 255: 70 (`FACTION_CHANGE_MEMBER_ROLE_RIGHTS`); unknown target 309
  (`EXPEDITION_NO_TARGET`).
- **ChangeRolePolicy.** The C# let anybody with the Expel right edit any role's policy (a
  Commander could edit the General's or its own). Now only the leader edits, and only roles below
  255 that exist, a packet naming another guild is refused, all with 70; a role name that fails
  the guild name regex / length 296. The 1.2 dialog edits the name and the invite / expel /
  promote / siege-join flags only (`role_policy_tab_view.lua`), so the other flags keep the stored
  value and the **stored** policy is what every member is sent (the packet's flags were
  broadcast before).
- **Invite.** A target of another top-level faction (the guild mother's alliance, with the
  runtime nations overlaid; the character's **saved** faction, so a duel / battlefield temporary
  faction is ignored, NATION-13/14) 304 `EXPEDITION_BAD_FACTION`; a dead target 307
  `EXPEDITION_BAD_STATUS`; a target in a battlefield (instant game) 783
  `FACTION_INVITE_BATTLEFIELD`. The C# returned silently. (The battlefield check has no flow
  test: there is no light fixture for an instant game.) A blocked inviter is still dropped
  silently before these replies (BLOCK-LIST-1).
- **Create.** The C# dropped `CSCreateExpedition.id` and used the owner's faction as the guild's
  `MotherId`. Now the sponsor faction is honored: valid sponsors by the owner's alliance are Nuia
  (148): 101, 102, 103, 105, 106, 107; Haranya (149): 108, 109, 110, 111, 113; outlaws (114):
  161 (the dialog's list); another sponsor 321 (`EXPEDITION_CREATE_FACTION`, "wrong sponsor");
  0 (a client that sends none, the bots) keeps the owner's faction as before. It becomes
  `expeditions.mother`. The sponsor list is chosen by the owner's **saved** faction (a
  temporary duel / battlefield faction does not count, NATION-14), and its alliance is the
  static `MotherId`. The readers of the guild's `mother_id` still work for a sponsor of the
  alliance: the fort owner faction (`dominion/fort.rs`) just takes the sponsor, and
  `Character.SetFaction`'s test compares **alliances** now (it compared the exact faction, so a
  change inside the alliance expelled from a guild founded under the old faction). A party member
  of another top-level faction (nation overlay, NATION-13; saved faction, NATION-14) is 320
  (`EXPEDITION_CREATE_MEMBER_FACTION`; the C# sent 321). Party members
  that are offline are skipped, neither joining nor counting towards `PartyMemberCount` (the C#
  added them as members marked online).
- **Disband.** An owned territory is 301 (`EXPEDITION_OWNER_DISMISS`; was a chat line), a
  registered / running siege 469 (`EXPEDITION_DISMISS_IN_SIEGE`), a non-leader 807 as before. The
  dismissed guild still stays in the list as `$deleted-guild-N` (as in the C#), but
  `faction_chunks()` (`SCFactionList` at login) skips it.
- **Faction change (`Character.SetFaction`).** The C# called `Kick(p, p)` / `MemberRemoveFromTeam(p,
  p, Kick)`, which need the Expel right and a higher rank / the party owner, so an ordinary member
  was never removed. Now a forced leave without any right check (`expedition::leave_forced`;
  `SCUnitExpeditionChanged` with `expel = false`, same broadcasts and saves as a voluntary
  leave) and the party leaves through `LEAVE` instead of `KICK` (looked up by membership, not
  `in_party`, so the login infamy conversion is covered). The guild is left when the new faction
  belongs to **another alliance** (`alliance_id` of the mother, so a change inside the alliance
  stays). The guild **owner** follows the owner decision of 2026-11-03 (PIRATE-1): the leadership
  first passes to the highest-`role` remaining member whose faction is still the guild mother
  (ties: the lowest `character_id`, the stable stand-in for the longest membership; the join date
  is not stored), the old owner is removed in the same write section, and the guild keeps its
  castle and nation (the nation's king moves with the hand-over). Only an owner who is the last
  eligible member dismisses the guild, then releasing its dominion / cancelling its siege. A
  member with the leader role but a different recorded owner uses the upstream rule instead: the
  guild goes to the highest-ranked member (ties: first in the member list) and the guild of a sole
  member is dismissed even if it owns territory or is in a siege. The temporary battlefield / duel
  factions (RPA-09) are untouched.
- Not done (out of scope): the nation packets (0x015-0x019), the beginner expedition (CS 0x010 has
  no opcode), guild chat `chat` flag enforcement, member memo, guild level / summon.

## Client skill cast authority (2026-10-20)

`CSStartSkillPacket` trusted its caster, its target distance and its skill id more than the C#
original ever checked (the C# had no checks here at all). Deviations, each reproduced live with
two bots before the fix (a foreign `Mount` caster, an NPC-only skill and a far doodad all ran):

- **Mount caster.** `SkillCaster::Mount { obj_id }` may name the player itself, a mate the player
  may use (`may_use_mate`), or a slave the player summoned (`summoner_obj_id` / `summoner_id`),
  is attached to (`attached_characters`, or `parent_obj_id`), or that is a harpoon mounted on a
  slave the player summoned. A ship cannon (kind 8 child) answers only its gunner. Any other object (NPC, doodad, another player, a foreign
  slave) is refused with a warning and `SCSkillStarted` carrying `NO_PERM`.
- **Range of doodad and slave targets.** The C# skipped the `TooFarRange` check for every Doodad
  or Slave target (its own TODO asks for a bounding-box check). The Rust now allows
  `max_range` plus a flat allowance (15 m for doodads, 40 m for slaves) on top of the existing
  model-radius subtraction: no model radius is loaded for doodads and the ship `obb_*` columns
  are zero in the 1.2 data, so a constant stands in for the missing bounding boxes. Raise the
  allowances if a very large doodad or ship turns out to be unusable from its edge.
- **"Undefined use type" fallback.** The C# ran any skill id with a template here (boss, NPC and
  GM skills included). It now requires the id to be named by a player-facing source of the static
  data (`SkillData::is_client_sourced_skill`, `CLIENT_SKILL_SOURCES`): doodad funcs / uses / fake
  uses / conditional uses / skill hits, sphere doodad interacts, crafts, NPC interactions, slash
  commands, buff skills, quest accept / supply / component / timer skills, item use skills and the
  housing / shipyard build steps, plus the effect-based recovery skills below. Mount skills,
  mount-attached (rider) skills and NPC AI skills (`np_skills`) are deliberately absent. Possible
  over-blocking: a skill the 1.2 client casts that none of these tables names (the retail rule is
  not confirmed); such a cast is refused with a warning naming the skill id, so it is easy to add
  the table.
- **Ability-tree skills need an active ability and a learned skill.** The C# `CSStartSkill` fell
  through to the catch-all above and cast any skill id. The 1.2 client marks unlearned and
  inactive-tree skills unusable in its UI: the action-slot UI carries `canUse` / `learned`
  states (`scriptsbin/x2ui/components/action_slot.alb`) and the native
  `X2::GameClient::SkillAccessor` publishes `learned`; that the client also refuses the press
  is not shown in the decompile and was not observed live. The result codes are the client's:
  `INVALID_SKILL` 0x1e and `INACTIVE_ABILITY` 0x1f (`out-x2game/x2game.dll.c`). `handlers::class_skill_denial` now refuses a client cast of a
  skill with `ability_id != 0` when that ability is not one of the three active ones
  (`SkillResult.INACTIVE_ABILITY`) or, for `need_learn` skills, when neither the skill nor a
  same-ability-level variant is learned (`SkillResult.INVALID_SKILL`); the 27 tree skills with
  `need_learn` false only need their ability active. Item, mount, default /
  common and auto-attack casts are decided before the check; ability-less skills (buff, quest,
  doodad, craft, NPC interaction, slash command, housing / shipyard) are unaffected, and the
  real-data test pins that every id those tables name is ability-less. The only ability-tree
  skills a client-facing source table names are mount skill 13572 (mount branch) and
  quest/doodad conditions 10664, 10134, 12075 and 11380; they still need their ability active and
  the skill learned, as the client requires. The exact result codes are our choice (the client
  only needs a non-success `SCSkillStarted`).
- **Item-only skills.** `items.use_skill_id` and `item_grade_skills` are player-facing sources,
  so the fallback branch ran an item's use skill as a plain unit cast and consumed nothing
  (e.g. 26611 "Full XP Restoration" for free). `SkillData` now keeps the ids named by those two
  tables and by no other source in a separate `item_only` set
  (`SkillData::is_item_only_skill`); the `CSStartSkill` handler refuses a non-`Item` caster of
  one with `NO_PERM` and only the `SkillCaster::Item` branch runs it (validating the item's own
  use skill and consuming the reagent item). A skill another source also names (doodad / NPC
  interaction, craft, ...) keeps its plain cast.
- **SavePortal special-effect source.** Skills 11215 "Entering location in Teleport Book..." and
  16841 "Editing Location in Teleport Book..." are not named by any source table: the 1.2 portal
  window sends a plain `CSStartSkill` with a `SavePortalInfo` object. `SkillData` now sources the
  skills whose effect is `SpecialEffect` 30 `SavePortal` (`CLIENT_SKILL_SPECIAL_EFFECT_SOURCES`,
  narrow by type id, not by effect class). The effect still requires the Memory Ink reagent
  (`skill_reagents`) and only renames the caster's own portal. The same list now includes type
  53 `AttachTo`: the 1.2 slave action bar (`FUN_393bbb80`) adds 13863 "Take the Passenger Seat"
  and `FUN_39362f50` sends it as a plain `CSStartSkill`, and no source table names it. Its effect
  only binds the caster to the target slave at the value1 seat, and only when the model declares
  that seat in `model_bindings` (see the 2026-11-01 seat bindings bullet).

### Item, vehicle bar and SavePortal hardening (2026-10-30)

- **Item template from the owned item.** The `CSStartSkill` `SkillCaster::Item` branch resolves
  the template from `ItemManager` and rewrites the caster before `Skill.Use`, so the client's
  `ItemTemplateId` never decides the use skill, the reagent or the consumed template. Item-only
  skills require `skill_id ==` the owned template's `UseSkillId`. The C# bind-on-pickup
  exception (`bfdc3268`, PR #615) is kept only for its evidence-backed case: the house
  fireplace's `DoodadFuncUse`, where the packet targets that doodad in range and its current
  func group has a `Use` func for the skill. Consumption is debit-first through
  `ItemSkillHooks::take_cast_source_item` (see the 2026-10-30 section below): the item is
  checked and taken under one container lock before the result, and a cancelled effect refunds
  it. Plot-only item skills verify the same owned item before the plot runs.
- **SavePortal creation.** `SpecialEffect` 30 `SavePortal` only records a new book entry when the
  casting skill pays a reagent (`skill_reagents` or an effect-level `consume_item_id` /
  `consume_source_item`). Skill 16841 "Editing Location in Teleport Book..." has no reagent and
  may only rename an existing portal of the character; 11215 pays its `skill_reagents` Memory
  Ink, and 16387 pays an effect-level Memory Ink taken atomically by the effect and only when
  the cast targets a real fireplace doodad in range with its `DoodadFuncUse`. No portal-book cap
  exists in the C# `PortalManager` /
  `CharacterPortals` or in the 1.2 data (the client UI pages the list by `COUNT_PER_PAGE`); none
  is invented here.
- **Vehicle bar caster.** A `Mount` caster must be a mate the player may use or a slave the
  player owns / is attached to; the player itself is no longer accepted. The skill id must be on
  the vehicle bar: the mate's `Mate.skills` mount-skill ids, the slave template's
  `slave_mount_skills`, or a `buff_mount_skills` entry of an active vehicle buff, each mapped
  through `mount_skills.skill_id`. This closes the NPC / boss / GM skill bypass; the rider /
  operator skill still runs as the player through `mount_attached_skills`.

### Skill source item debit before the result (2026-10-30)

**Deviation:** C# `Skill.ApplyEffects` (`AAEmu.Game/Models/Game/Skills/Skill.cs` at
`a2ab80abd^`, the `if (!Cancelled)` block after the effect loop) collected `consumedItems` /
`consumedItemTemplates` while the effects ran and consumed them only after every effect, so a
cast-time item that left the bag during the cast still granted the products (granted before the
effects) and effects, and the final `ConsumeItem` either missed it or took it from another
container (mail, auction, a trade partner's bag). The Rust port now debits first: right before
the reagents, products and effects, `ItemSkillHooks::take_cast_source_item` verifies the owner,
template and count under the economy operation lock. Only the caster's bag or equipment may hold
the item (never a trade, mail, auction, bank or coffer copy); equipment is a real source (trade
pack bundles such as item 14513 are equipped and cast their `PutDownBackpack` skill), so a full
consume pushes the container's `EquipLeave` event. If the item is not there the whole result is
refused (no reagents, products or effects). A full-stack consume removes the item and sends its
removal at take time, before a product can take the freed slot; a partial consume decrements the
count in place and sends a count update at take time, so the kept stack holds its slot.
`commit_cast_source_item` pushes `Consumed` (the consumed amount) and releases the id for a full
consume; `refund_cast_source_item` re-adds a full item (a bag source to its own slot only while
free, otherwise a free bag slot; an equipment source only to its own slot, otherwise to the bag;
neither ever to another equipment slot, so no wrong `EquipEnter`) or increments a partial stack
back, sending the matching add / count update and saving the item. A full container leaves the
item loose with no slot-add task (it is in no slot); the next relog places it. Effects that read the casting item (UCC imprint/cleanup, dyeing,
loot-pack grade, socketing, grading scroll, look-convert powder, emblem stamp) use the pre-debit
snapshot passed in `EffectSource::source_item`, and `Character.ItemUse` quest events are raised
from that template instead of a live item lookup. Partial consumption also raises `Consumed`, so
quest collect counters drop. The item reservation map (and its trade / sale / destroy checks) is
removed: the item is physically absent (or its count already reduced) while the result is
decided, so those movers cannot reach it. `consume_owned_item` remains only for the plot-only
cast's pre-plot ownership check.
Regression tests: `cast_item_with_products_{destroyed,mailed_or_auctioned,traded}_mid_cast_*`,
`kept_cast_item_grants_its_product_and_effect_and_is_consumed_once`,
`full_cast_item_removal_precedes_the_product_in_the_same_slot`,
`partial_cast_item_debits_in_place_before_the_product_add`,
`cancelled_ucc_cast_refunds_the_debited_source_item_exactly`,
`cancelled_full_ucc_cast_removes_then_restores_the_item_at_its_slot`,
`channeled_cast_cancelled_after_a_tick_consumes_the_item_once`,
`consumable_item_cast_progresses_a_use_item_objective`,
`partial_consumption_updates_a_collect_objective`,
`full_refund_on_a_full_bag_keeps_the_item_loose_and_notifies_the_client`,
`consuming_an_equipped_source_item_runs_the_equip_leave_path`.

## Lost-experience prayer cast (2026-10-30)

The player report "lost XP not given back at Nuia" was the skill-cast hardening above refusing the
priest's prayer: the client shows "You don't have permission." (`NO_PERM`, 0x2D). The 1.2 client
casts skill 17063 "Offer Prayer" (`recover_exp_effects` 1: labor plus a priest within 10 m) as a
plain `CSStartSkill` from its own interaction window
(`scriptsbin/x2ui/messageboximpl/message_box_from_npc_interaction.alb` -> `X2Priest:RecoverExp`);
it appears in no `CLIENT_SKILL_SOURCES` table, and `skill_req_skills` is a peace-zone restriction
list, not a grant. 26611 "Full XP Restoration" is a different skill: `recover_exp_effects` 5 has no
priest and no labor (300 s cooldown), and it is the skill of the XP Restoration Scrolls (items
33965 / 33966 / 34024-34029, `items.use_skill_id`), already client-sourced through `items`; the
priest window never casts it. The C# `CSStartSkillPacket` cast every template skill in its fallback
branch, so the prayer worked there. `SkillData` now also marks the skills whose `skill_effects` row
points at an `effects.actual_type = 'RecoverExpEffect'` row with
`recover_exp_effects.need_priest = 't'` (`CLIENT_SKILL_EFFECT_SOURCES`): data-driven, not an id
allowlist, so it adds only 17063 and item skills stay item-driven, and the effect's own
`need_priest` / `need_labor_power` checks are unchanged. Labor is now spent only when
`need_labor_power` is set, and saturating at 0: the scroll used to subtract the level-based amount
anyway and could wrap labor negative. `recoverable_exp` was already saved and loaded
(`db_character.rs`), so it survives a relog; a second death overwrites it exactly as
`CharacterCombat.DoDie` does (`RecoverableExp = round(LastExpLoss * 0.8)`).

Rechecked 2026-11-01 against the real catalog (not the fixture): the prayer's real 6.5 s cast
restores the recoverable exp both when the client targets the priest and when it targets self;
the four real `npcs` rows with `priest = 't'` are 502 / 1664 / 5614 / 5615, and the effect accepts
all of them by template flag (no faction check). The level-band labor is `formulas` row 7
(`if_negative(pc_level - 51, pc_level, 50 + (pc_level - 50) * 20)`), matching the effect's
formula; above level 50 a level-51 prayer spends 70 labor. The XP Restoration Scroll (33965,
`use_skill_as_reagent = 't'`) works through the `SkillCaster::Item` cast while the character is
dead (skill 26611 has `source_dead = target_dead = 't'`, effect 5 has no priest and no labor).
The real refusals keep the C# codes: nothing to recover answers
`CANNOT_RECOVER_ALL_NOT_ENOUGH` (796) and insufficient labor `NOT_ENOUGH_LABOR_POWER` (29); the
1.2 client itself never sends the prayer with nothing to recover because
`X2Priest:GetCostForRecoverExp` returns nil then. The reported "You don't have permission." on
2026-11-01 is therefore the pre-#21 client-source refusal: the current data sources 17063 through
`CLIENT_SKILL_EFFECT_SOURCES` and both paths restore the experience.

## Ship repair timer and destroyed-vehicle HP (2026-10-20)

Deviations from the 1.2 C# `Slave.DoDie` / `SlaveManager.Create` (the check already existed, but
the death never started its timer):

- `Slave.MarkSummoningItemAsDestroyed` wrote `RepairStartTime = DateTime.MinValue`, so the
  `RepairStartTime.AddMinutes(10) - UtcNow` check in `Create` was always negative and a destroyed
  vehicle could be summoned again immediately. The death now stores the destruction time in the
  `SummonSlave` item, which starts the intended 10 minute wait; a successful summon after it
  clears the timer (the destroyed flag stays until `RepairSlaveEffect` clears it, as in C#).
- The C# reset of a loaded 0-HP vehicle to `MaxHp` ("can't summon with 0 HP") is kept, but only
  when the item has no pending repair time. With the timer above, a destroyed vehicle therefore
  comes back at full HP only after its repair wait, not when it is still on cooldown.
- The C# `SummonSlave.ReadDetails` threw on `RepairStartTime` (`Convert.ToInt32(byte[])`), so a
  saved repair time was always reset to `MinValue` on load and the 10 minute wait was forgotten
  on restart. `Item::read_details` now restores it from the `items.details` blob (i64 after the
  destroyed flag, negative values clamp to 0). Legacy C# rows decode too (unset: i32 0 + five
  i32 zeros; set: i64 + five i32 zeros), and a blob that stops after the destroyed flag still
  restores the header.

## Global cooldown enforcement and plot-only skill cooldowns (2026-10-21)

Deviations from the C# `Skill.Use` (`Skill.cs` 135-153 + `Helper.CheckInterval`):

- The C# refused a GCD / last-use violation only when `CheckInterval` also failed, and that
  returns true (and records) whenever no attempt was made in the last 150 ms. So the first press
  during any GCD, and every press 150 ms after the previous one, got through (a bow skill with a
  700 ms GCD fired 4-5 times a second). `use_skill` now answers `SkillResult.CooldownTime`
  outright while more than `GCD_GRACE_MS` (150 ms, network jitter) of the GCD or of the
  last-use delay is left; `ignore_global_cooldown` skills and GCD-bypassing calls are unchanged.
  The `intervals` map is gone. The skill's own cooldown check (`UnitCooldowns::check_cooldown`,
  250 ms tolerance) is unchanged.
- Plot-only skills return before `Cast`, which is where the C# sets the GCD, and only got their
  own cooldown at `DoPlotEnd`, so they could be recast while the plot ran. The GCD and the skill's
  cooldown are now started when the plot starts (`DoPlotEnd`'s `AddCooldown` keeps the earlier
  entry). Not done: refusing a recast with `skill_on_casting` while `active_plot` runs, since
  plots chain follow-up skills through the same caster state.

## Global cooldown of a player's timed cast (2026-10-29)

Deviation from the C# `Skill.Cast`, which sets the GCD when the cast ends (harmless there because
the C# barely enforced the GCD). With the enforcement above, a player who finished a timed cast
(e.g. Insulating Lens, 2000 ms) had the next skill refused with `CooldownTime` for the ~1 s of
GCD still left at the cast end, although the 1.2 client counts its GCD from the cast start (and
keeps it when the cast is cancelled) and showed the skill as ready; Drop Back "did nothing"
because the client only leaps once the server accepts. `use_skill` now starts the GCD when a timed
cast is accepted for every non-NPC caster (players, player-driven mates and slaves) and flags it
on the skill run (`gcd_started`) so `cast` does not restart it at cast end. NPC timed casts and all
instant casts are unchanged (the NPC AI paces its skills from the cast-end GCD).

## Loot take range (2026-10-21)

C# `TryTakeLootLocked` never checked the distance to the corpse (`MaxLootingRange`, 200 m, only
bounds loot eligibility at generation). The Rust port refuses a take with `TooFarAway` beyond
`LOOT_TAKE_RANGE` (5 m), measured like `GetDistanceTo` (minus the model radii). Evidence: the
retail 3.5 client enables looting at 3.0 m or less on the target frame's distance and walks the
player to the corpse first (the 10.x client decompile has a matching 3 m check, squared 9.0); the
2 m above it are slack for movement and latency.

## Follow-path commands and ground-following arrival (2026-10-21)

Public Farm Manager 11545 (`ai_file_id=15`, hold position) fires command set 146 every 3 minutes:
`FollowPath public_farm`, `UseSkill 18324`, `Timeout 2`, `FollowPath public_farm2`, ...
`public_farm@182.path` starts 0.4 m from the spawner and its second point is authored 1.4 m above
the terrain the walker follows. The NPC stayed in `FollowPathBehavior` with "0 points (1 remaining
in queue)" and emitted a movement packet every 100 ms without advancing (the client animates
running in place). Four mirrored C# bugs and one missing safeguard:

* `RunCommandSetBehavior.TickCurrentAiCommand` called `Ai.LoadAiPathPoints(Ai.AiFileName, ...)`
  before assigning `AiFileName = Param2`: the first FollowPath of a set loaded nothing (the NPC
  went straight back to Idle with the other commands still queued), each later FollowPath loaded
  the *previous* route and the last route was never loaded. Rust now loads the command's own
  `Param2` (empty `Param2` still falls back to `AiFileName`).
* `AiPathHandler.RunCurrentPath` measured arrival in 3D against `Template.Scale`. A ground walker
  has its step Z replaced by the reference height, so a point authored above the terrain is
  never reached; `Npc.MoveTowards` then keeps emitting sub-metre steps forever. Rust walkers now
  arrive on horizontal distance (flyers keep the 3D check) and the radius is at least the 1 m
  movement tolerance, so a template scale below 1 m (which could never arrive in the C#) works
  too. `FollowPathBehavior` passes that same radius as the `MoveTowards` tolerance.
* `FollowPathBehavior.Tick` ran `GoToIdle()` when the path lists were empty *after*
  `RunCurrentPath` had already sent the NPC to `RunCommandSet` (the `ReturnToCommandSet`
  sentinel): the rest of the command set was stranded. Rust only idles while still in FollowPath.
* Not in the C#: a target that makes no progress for 5 s (`TARGET_STALL_TIMEOUT_MS`) is dropped
  with one WARN naming the NPC and the route file, and the route continues with the next point,
  so a blocked or unreachable point cannot produce endless in-place movement.
* `NpcControlEffect` `RunCommandSet` (`systems::ai::hooks`) enqueued the AI commands *and*, for the
  command set's first route, started `Simulation.GoToPath` as well. For an NPC with an AI two
  movers then worked on the same NPC: the simulation stepped it (100 ms) towards its own
  checkpoint while `FollowPathBehavior` stepped it towards the command's route point, undoing
  each other's steps (the NPC wobbled 0.1 m for ~40 s, e.g. the Public Farm Manager, command set
  146, never got past x 10494.7 of a 10497.3 route, and the 5 s stall skip fired). Rust only
  starts the simulation for an NPC without an AI; the AI runs the FollowPath / UseSkill / Timeout
  commands itself. Test: `flow_tests::run_command_set_effect_does_not_start_the_simulation_mover_for_an_ai_npc`.

* `NpcControlEffect` `FollowPath` (and the no-AI `RunCommandSet` route) checked and set `IsInPatrol`
  in two steps. The effect can reach an NPC twice at once (Timeout trigger of a channeling
  target buff: the buff's timer and the end of the channel), and a second caller that passed the
  check ran `Simulation.GoToPath` again, which toggles `MoveToPathEnabled` back off: the walk
  stopped at once (6-9 of 400 simultaneous deliveries). The check-and-set is one step now
  (`hooks::begin_patrol`). Test: `flow_tests::follow_path_effect_arriving_twice_at_once_starts_the_walk_once`;
  the whole quest 2423 'The Windwhips' Chest' chain (Anima Injector -> buff 1728 -> route
  `q2423_fm_tige` -> Suspicious Chest 2906 -> Metal Cylinder) is pinned on the real tables by
  `skills::quest_machine_flow_tests` (`AAEMU_COMPACT_DB`).

`flow_tests::follow_path_command_set_walks_every_route_point` runs the real command-set shape
over three routes with an elevated point and checks every point is targeted in order, the set
finishes and the NPC settles in HoldPosition without further movement.

## Old Memoria Lake Tree spawns for "Low on Lumber" (C# PR #1793, 2026-10-22)

Ported C# `develop` 9549c76b (PR #1793): seven `UnitId` 3848 (`Old Memoria Lake Tree`)
placements in `app/Data/Worlds/main_world/doodad_spawns.json` at the Old Memoria lake
(X ~9049-9098, Y ~9060-9121, Z ~192-197), next to the one placement the file already had. They
make quest "Low on Lumber" (context 2094, level 38) satisfiable: `QuestActObjItemGather` id 890
gathers item 16035 x 5 with `highlight_doodad_id` 3848. Data only: `DoodadSpawnerDef`
(`systems/npcs/doodad/spawner.rs`) already reads `UnitId` / `Position`. Regression test
`spawn_tests::real_data_old_memoria_lake_trees_for_low_on_lumber` loads the shipped file through
the real `parse_list` and checks the eight placements.

## Construction step materials (2026-10-22)

* The construction step skill of an unfinished house needs the consume item of its effects: the
  Scarecrow Garden (design 267) step skill 18553 "Construct Garden" ("Spend up to 10 Labor and
  1 Lumber") keeps its Lumber (item 8337 x1) in `skill_effects.consume_item_id` /
  `consume_item_count`, not in `skill_reagents`. The C# consumed an effect-level item only when
  the player had it and never cancelled the cast, so anyone built houses for free (and the
  Rust port mirrored that). `systems::skills::validation::check_build_materials` now refuses the
  cast, at the start and again at fire time, with `skill_need_reagent` (result 36, the 1.2
  "reagent missing" result the cast already sends for `skill_reagents`) when the target is an
  unfinished house, the skill is the house's current build-step skill and the character lacks
  the item count. Labor is still checked by `check_resources` as before and nothing is spent on
  a refusal. Every other skill keeps the soft effect-level consume. Reason: retail refuses the
  construction without the materials; the step tooltip says it spends 1 Lumber. Regression:
  `systems::housing::lifecycle_tests::construction_step_needs_its_effect_consume_item_before_cast`.
  The error text the
  client shows for result 36 was not re-checked in the client binary.
* `systems::housing`: the final availability check is early feedback, not the payment boundary.
  A selected player-to-house Building `CraftEffect` now checks the live house and current step
  under the housing operation lock, then consumes its full effect-level material through
  `ItemSkillHooks::pay_building_material` immediately before advancing progress. The economy
  bridge preflights inventory, equipment and bank under one item transaction; a defensive short
  debit restores exact container snapshots and discards queued effects. Persistence completes
  under the economy lock, which is released before progress callbacks; item callbacks flush
  after the housing lock. Missing payment cancels with result 36, without progress, labor or
  the success interaction quest event. Rejected/stale house effects never fall back to the
  legacy optional debit. Other effects retain that optional behavior. Regressions remove the
  last real item after the final availability snapshot, retry after replenishing, check exact
  and extra stock in every supported container, and prove non-building NPC damage still applies
  without its optional item. Zero-step designs already finish during placement; a subsequent
  construction action is rejected before payment.
* `systems::economy`: container consumption callbacks now report the actual consumed delta
  exactly once for full, partial and preferred stacks. The C# `ItemContainer.ConsumeItem`
  decremented a depleted stack to zero before `RemoveItem` reported its count; the Rust port
  mirrored that zero-count event (plus a second, correct event for a preferred stack). Removal
  now accepts the consumed delta internally while direct removals keep their remaining-count
  semantics. The callback still precedes `EquipLeave`, and wire item tasks are unchanged.
  Container regressions cover full/preferred/partial/multiple stacks, equipment callback order
  and direct removal; the housing payment callback test covers both the last unit and a partial
  stack, with both operation locks released.

## Slave summon placement and equipment (2026-10-22)

- **SummonPos placement.** The 1.2 C# `SpawnSlave` special effect ignored `targetObj` and
  always called `SlaveManager.Create(owner, skillData)`; the Rust port now follows the 10.x
  `Skill.GetInitialTarget` / placement path. A `SummonPos` cast resolves the client-chosen spot
  (world coordinates / yaw, including the `ObjId1` basis transform), validation sees that spot
  (range, angle, obstacle, `target_only_water`) and the spawn uses it instead of the owner's
  in-front offset. The spot is rejected (with `ErrorMessageType.SlaveSpawnErrorInvalidArea`,
  583) and the 1.2 C# in-front placement is used when it is farther than
  `slaves.spawn_valid_area_range` / the skill's `MaxRange` (the smaller nonzero limit; the boat
  summon 15802 has `MaxRange` 0), when a ship spot is not on water, when a land-vehicle spot is
  in water, or when a 0.5 m sphere just above the surface overlaps collision (`mask::SOLID`).
  Skill 15802 also needs the 10.x `unboundedPlacement` rule: a position target with
  `MaxRange <= 0` is not rejected as `TooFarRange`.
- **`CSChangeSlaveEquipmentPacket`.** The 1.2 C# read only the header and logged it ("coming
  soon"), so the implementation follows the 10.x packet logic (`e533e88b6`, C# `develop`
  2026-08-02) adapted to the 1.2
  client writer: `characterId u32`, `slaveTl u16`, `dbSlaveId u32`, `bts bool`, **one** item
  pair and the four location bytes (`397d12a0`; the 10.x `num` and per-entry `expireTime` do not
  exist in 1.2). Equip / unequip / re-slot move the item between the owner's bag and the slave's
  persistent `EquipmentSlave` container (`SlotType 0xF2`, `ContainerKind::Equipment`,
  `item_containers.mate_id` = slave DB id); the pre-move server snapshots are echoed in
  `SCSlaveEquipmentChangedPacket(success)`, and the changed slots are broadcast as
  `SCUnitEquipmentsChanged`, followed by `SCUnitPoints` when a part carries MaxHealth. Item
  tasks go through the economy's `SplitOrMoveItemEx` (`ItemTaskType.Invalid`, as in C#), and the
  slave model mirror is refreshed before `UpdateGearBonuses` / pool refresh.
- **Ship parts.** Items with `item_impl = 28` (`ItemImplEnum.SlaveEquipment`) are
  `ItemClass::SlaveEquipment`, but the 1.2 client has no detail type 10: its item detail reader
  `3961edf0` only has cases 1..9, and reads no body for tag 10 (the C# `SlaveEquipment = 10` is
  for later clients). The server therefore sends them as detail type 0 (`Invalid`) with no body,
  like every other class without a 1.2 detail; `read_wire_item` consumes detail bytes by detail
  type (gear 55, ...) instead of always reading an `EquipItem`. Bind-on-equip applies in
  `EquipmentSlave` too. A player-owned
  slave gets its starter pack when its equipment container is first created, including a saved
  legacy hull whose summon item already names its DB id. An existing empty container represents
  intentional unequipping and is reused on the next summon / restart without seeding again.
  NPC / spawner slaves keep the in-memory defaults.
- **Deviation from 1.2:** slave equipment persistence and the `0xF2` container do not exist in
  the 1.2 C# at all; they are the 10.x design, recorded here because the 1.2 data already ships
  `slave_initial_items`, `slave_equip_slots` and `item_slave_equipments`.
- **Equipment validation (1.2 data):** equipping checks the item's
  `item_slave_equipments.slave_equip_pack_id` against `allow_to_equip_slaves` (the hull) and its
  `slot_pack_id` against `allow_to_equip_slots` (the target slot); the hull must also declare the
  slot in `slave_equip_slots`. A refused move sends `ErrorMessageType.ItemNotForPet` (204; the
  1.2 client's "can't use that item on a summon" message) and moves nothing. 10.x trusted the
  client for this; its `SLAVE_EQUIP_ERROR_REQUIRE_SLOT_AND_ITEM` / `..._UNEQUIP_FIRST` (804/805)
  are for the client's slot-dependency rules, not for a part the hull does not accept.
- **Equipment visuals (10.x `SpawnEquipmentVisualsFromInventory`):** every occupied equipment
  slot spawns what `item_slave_equipments` maps it to at the slot's `slave_equip_slots` attach
  points: a child slave (Steel Cannon 28852 -> slave 10, Harpoon Cannon 30306 -> slave 48) or a
  doodad (sails, masts, lamps, planks, figureheads, bell, telescope, ...). A slot that drives two
  points (front/rear lamp, both planks) spawns the visual at both. Unequipping removes exactly
  that visual; a resummon respawns it from the persistent container. Static bindings are skipped
  only for slots with initial equipment, so deliberately unequipped parts remain absent. An
  authored default without an exact legal equipment equivalent remains a static binding.
- **Starter pack:** `slave_initial_items` rows are seeded only into slots the hull declares in
  `slave_equip_slots` (slave 110 / 139 reference pack 1 without those slots; slave 21 has slot 0
  at the cannon points and no starter cannons). Missing defaults are inferred from static
  bindings only when one legal item reproduces every binding of the slot (lowest item id breaks
  ties deterministically). The item template must also exist: hull 21's slot-0 cannon visual
  references missing item 28854, so all eight cannons remain static. Hull 14/75's static lamps
  also have no equivalent equipment item. Real-data spawn regressions
  cover hulls 14, 21, 75, 76 and 110; synthetic and MySQL restart tests cover initialization and
  persistent empty slots (`vehicles/pr38_regression_tests.rs`).
- **Attach points:** models 511 (slave 22), 1482 (slave 132) and 1546 (slaves 139/142/143/145)
  were recovered from the `$` helper nodes of the 1.2 client's `galleon_body_test.cgf`,
  `boardship_body_test.cgf` and `middleship_body.cgf` (CGF units / 100, the same method and
  values as the existing Clipper entry). Model 115 (`caravel.ship_body`) references a caravel
  mesh that is not in the 1.2 pak, and model 117 (`galleon.cannon`, the cannon's gunner seat) is
  an Entity `.chr` whose only node is its root (no `$` helpers), so those two still have no table
  entry.
- **No equipment skill link:** the 1.2 C# never connected equipped parts to cannon / vehicle
  skills; slave mount skills still come from `slave_mount_skills`. The child slaves spawned for
  equipment are visuals with the normal child-slave behaviour, as in the 10.x reference.
- **Manned ship cannons (deviation, the C# has none of this):** a kind-8 slave mounted on another
  slave (`slaves::is_manned_cannon`) has one gunner seat, attach point 1 (the `mount_attached_skills`
  row of mount skill 16 / Fire Cannon 10473 -> skill 14372). `CSBindSlave` on it is refused with
  `SLAVE_CANNOT_BIND` unless the character summoned the cannon, owns the carrier ship or is aboard
  it (standing on it or seated on any of its points; the 1.2 data has no seat permission table, so
  party / expedition membership is not required), with `SLAVE_ALREADY_HAS_MASTER` when the seat is
  taken and `SLAVE_UNBIND_FIRST` when the character already rides another slave. Only the gunner
  may cast the cannon's mount skills (`mount_authority::may_use_slave`): its DRIVER seat must name
  the player, and the player's attach point and transform parent must match. Standing on the
  cannon alone grants no shot, even when the seat is empty. The carrier's owner no
  longer fires it remotely. The gunner leaves through `CSDiscardSlave`, death (`on_death_event`),
  teleport / `force_dismount`, the cannon being unequipped, or the carrier being recalled,
  destroyed or despawned (`unbind_child_riders`). Carrier death detaches child riders before
  deleting their cannons, preserving world coordinates, clearing seats and sending `SCUnitDetached`.
  The gunner sits at the model's driver helper if
  `slave_attach_points*.json` has one, else at `GUNNER_SEAT_OFFSET` (0, -0.8, 0) in the cannon
  frame: model 117 has no helper in the data or the pak, and no cannon-like model with a seat helper
  exists, so this is a documented constant. The gunner is a transform child of the cannon, which is
  a child of the ship, so the position follows the moving ship. Fire Cannon is a ballistic position
  skill (4 m area, 4-120 m, fixed 130-170 damage); the area damage and the `SCSkillFired` broadcast
  (the client draws the projectile from the skill's projectile 13) are covered by
  `ship_cannon_gunner_seat_skill_and_leaving`.

## Crafts that stop silently left the craft window locked (2026-10-24)

Testers reported that crafting "Bound Tax Certificate" (craft 76, skill 16767, 200 labor, Building
Plaque) or a long queue of "Disintegration: Gilda Stars" (craft 8000001, skill 8000016, 3 labor,
Archeum Workbench) greyed out the craft button of every workbench until relog. Deviations from the
1.2 C# `CharacterCraft`:

- `Craft` ignored the `Skill.Use` result. A refused cast (not enough labor, reagent, range, GCD,
  ...) therefore ended the batch with no packet and no state change, so the client kept waiting for
  a craft that never started. The cast hook now returns the skill result; a refusal sends the same
  `SCErrorMsg(CraftCantActAnyMore, reason)` the other stop paths use (labor -> `NotEnoughLaborPower`,
  reagent -> `NotEnoughRequiredItem`, range -> `NotNearToTarget`, permission / target ->
  `CraftPermissionDeny`, cooldown -> `CraftCooldown`, else `InternalError`) and runs `CancelCraft`.
  A `Skill.Cast` re-validation failure of the craft skill (`craft::cast_failed`, called from
  `skill::cast` through `ItemSkillHooks::craft_cast_failed`) ends the batch the same way.
- Paths where the C# threw (unknown craft id, unknown craft skill, `SameAccount` permission with an
  offline owner, unknown `DoodadFuncPermission`) and `ScheduleCraft` without a runtime now end the
  batch with an error instead of leaving the client locked.
- `ConsumeLaborPower` was the raw skill cost while `Skill.EndSkill` charges the vocation-discounted
  cost, so a character holding only the discounted amount had its last craft turned into a
  "fictitious step" (no products) that still cost labor. The craft state now stores the discounted
  cost (same formula as `skills::validation::labor_cost`).
- The 1.2 client resets its craft batch (`IsWorkingCraft`, `CRAFT_ENDED`) only on `SCSkillStarted`
  with a nonzero result or `SCErrorMsg` 148 with a nonzero reason and `isNotify == false`. Every
  stop of a started batch therefore ends in `abort_craft_with`, including the not-enough-actability
  path (which also keeps its notify message) and the two "fictitious step" paths of `EndCraft`
  (labor, bag space): the C# kept casting the rest of the batch after sending the error, which the
  client no longer showed; the batch now stops there. `SCCraftFailed` carries item template ids,
  not craft ids (field renamed `item_id`); it stays unsent.
- A refused house-bench access now logs why (`craft::housing_bench_denial`).

The `craft` scenario of `aaemu-testclient` drives the labor-exhaustion stop live.

## Glider re-equip after a trade pack (deviation)

The C# put the glider back (`Inventory.PreviousBackPackItemId`) only in `PutDownBackpackEffect`.
Here every route that takes an item out of the back slot re-equips it: the equipment container
queues `ItemFx::EquipLeave` for the slot, and `economy::fx::flush` calls
`inventory::reequip_previous_glider` (put down, death drop, turn-in / sale / destroy removals,
moves to another container). The record is `(item id, template id)`; the glider is equipped only if
that same item is still in the character's own bag, is a glider, the back slot is empty and the
container accepts it (the same `can_accept` / `SplitOrMoveItem` path as `CSSwapItems`; the port's
`CanAccept` has no level or class check for backpacks). Any failed check consumes the record, as
does the player equipping something else into the back slot. A pack lost while the character is
dead keeps the record until `resurrect_character`. The explicit `SCUnitEquipmentsChangedPacket`
broadcast in `put_down_backpack` is gone (the equipment events broadcast leave and re-equip).

## Dropped trade packs, crop seeds, `zonestate` and request logging (2026-11-04)

- **A put-down or dropped trade pack expires and takes the item with it.** The lifetime is data:
  the backpack doodad of `PutDownBackpackEffect.BackpackDoodadId` has a `DoodadFuncFinal` whose
  `After` is 518,400,000 ms (6 days) for 254 of the 335 doodads that put-down effects name (the
  others 1 s to 1 day; templates 3059, 3304, 7000, 7001 and 7002 have none and never expire;
  seven effects name templates that are not in the data and no item uses them). The final task
  survives a restart (`phase_time` is saved and `override_phase_time_ms` restores the remaining
  time, a pack that ran out while the server was down is deleted at startup). What did not work
  was the cleanup afterwards: the "cargo" retention in `bridges::cleanup_doodad_escrow_item`
  (kept for the escrow of cargo that is still in use) also kept the item of an expired put-down
  pack, so every expired pack stayed in the owner's System container for good. The C#
  (`Doodad.Delete`, "Delete associated item if expired") removed any item whose holding container
  is none or System. Now a doodad whose template is a put-down backpack doodad
  (`is_put_down_backpack_doodad`) is not retained, provided the existing escrow checks hold (the
  item is the unique matching singleton in the owner's System container, not a rack or slave
  item) and no other live doodad still references the item (`item_referenced_by_live_doodad`).
  Picking the pack up again (`RECOVER_ITEM`) moves the item out of System first, so it is not
  deleted. Packs that earlier sessions already orphaned in System containers are not
  reconciled. If `spawn_backpack_doodad` fails, the pack stays in System, as in the C#.
- **A crop harvest does not always return the seed (not a bug).** The loot packs of crops
  (6452, 6453, 6472, ...) give the crop (2 to 4) in group 1 and put the seed and the "fine" crop
  behind `loot_actability_groups` rows with `max_dice` 355 (seed) and 345 (fine) on a 0..9999
  roll, so about 3.6 % of harvests return the seed and 3.5 % the fine crop (`LootRate` and
  `actability_exp_multiplier` are 1.0). `loot::generate_pack_v2` is the C# `GeneratePackNewV2`
  and agrees with the data; the unit tests pin both the algorithm and, against the real
  `compact.sqlite3`, the rates.
- **`zonestate` parses the state case-insensitively and rejects unknown input (deviation).** The
  C# used `Enum.TryParse(args[0], out ZoneConflictType)`, which is case-sensitive, and an
  unparsable argument silently left the state at Tension, so `zonestate war` set Tension. Now the
  name (any case) or the number 0 to 7 is accepted and anything else answers
  `Unknown zone state '<arg>'. Valid states: Tension (0), Danger (1), ...`.
- **Routine request traces are `debug`, not `warn`.** The C# logged the receipt of many
  packets with `Logger.Warn`, which the port copied. The ones for actions that normally succeed
  are now `debug!`: planting and placing a doodad (`CreateDoodad, Id:`, `<name> is placing a
  doodad`, so the blood stain of a death is quiet too), `ChangeDoodadData/Phase`,
  `SaveDoodadUccString`, coffer requests, the auction requests (`Post` was labelled
  `AuctionMyBidList` through a copy-paste, it is `AuctionPost` now), specialty / store /
  `SetCraftingPay` / item secure / cash shop requests, `CancelTrade`, `StartInteraction`,
  `SelectInteractionEx` and `TeleportEnded`. A refused request (not allowed, unknown template, no
  auctioneer in range, invalid item id, ...) stays at `warn`. `test_support::capture_events` is the
  capture helper of the regression tests for this.
  The traces of the instance and quest packets are left to the owners of those areas.

## Glider deploy gate and the gliding error codes (2026-10-31)

The C# defined `CannotInteractWhileGliding` / `CannotChangeBackpackInGliding` but never raised them
and did not check the equipped glider. The Rust adds the 1.2 rules at these entry points
(`systems::skills::glider` holds the shared predicates):

- `CSStartSkill` of a glider skill requires the equipped glider. A glider skill is a deploy skill
  (`skills.skill_controller_id` 2687, 31 rows) or a linked glider skill
  (`skills.link_backpack_type_id` 2: 14901 "glider use" with `link_equip_slot_id` 26, 17657 "glider
  fold"). The back-slot item must be a glider (`item_backpacks.backpack_type_id` 2), and an
  `Item` cast must be exactly that equipped item. This mirrors the client's link check
  `FUN_393925c0` (item backpack type == `link_backpack_type_id` in the linked slot), whose failure
  raises `must_equip_proper_item` 326. The client converts the default glider skill 14901 into the
  equipped item's `use_skill_id` (item 14677 -> 12373), so the deploy arrives as an `Item` cast.
  A refused cast sends `SCErrorMsg(326)` and the `SCSkillStarted` `NO_PERM` result.
- `CSSwapItemsPacket` / `CSSplitBagItemPacket` moves that take an item out of, or put one into, the
  back slot while the unit is gliding raise `cannot_change_backpack_in_gliding` 543, mirroring the
  client's equip validation `FUN_394e41f0`.
- **Gliding state:** `is_gliding` is an active `buffs.gliding = 't'` buff **while airborne**
  (`skill_state.airborne`, set by `on_actor_move`). The 1.2 client's stance-4 check ends on the
  landing movement, while buffs 7431 / 1675 (test / indun flight skills) are not `remove_on_land`
  and would otherwise keep blocking a landed player. The buff alone (`has_gliding_buff`) is used
  only for the fold.
- **Every interaction entry path** uses one shared 334 check (`glider::refuse_interaction`):
  the skill `InteractionEffect` (`interaction_effect`, which is how doodad use / gathering /
  looting a doodad / the mailbox arrive) and the direct `execute_interaction_with` path (the
  CSLootOpenBag recover-item func), `CSStartInteractionPacket`, `CSInteractNPCPacket` (NPC and
  gimmick targets), `CSLootOpenBagPacket` and opening a `CSCofferInteractionPacket`. The client's
  gate is `FUN_393b3d50`, which runs before its interaction action `FUN_390a6af0`. A landed unit
  with a lingering 7431 / 1675 buff is not gliding and is not refused.
- **Fold when the glider is gone:** the back slot can still change mid-flight through a timed
  glider expiring or `CSDestroyItem` on the equipped slot. A fold skill (`link_backpack_type_id`
  2 with `link_equip_slot_id` -1, e.g. 17657) is accepted while a gliding buff is active even with
  no equipped glider, and the successful fold ends every active gliding buff (`glider::end_flight`)
  so the client falls cleanly instead of getting `must_equip_proper_item` 326. Without a flight, a
  forged fold still needs the equipped glider.
- **Not added:** an airborne requirement for deploying. The 1.2 glider skill has only the authored
  `unit_reqs` NotUnderWater and NoBuffTag 306 (`unit_reqs` 28592 / 28593), the client start path
  `FUN_393989e0` checks only the linked item (no stance / air test), and the startup lift
  (`gliding_startup_speed`) is what takes off from the ground. The double-jump deploy
  (`OPTION_ITEM_USE_GLIDE_WITH_DOUBLE_JUMP`, option 57) is a client INPUT preference: the
  client sends the normal deploy `CSStartSkill` when the jump key is pressed in the air, and
  10.x has the same option and no airborne error, so there is no server gate to implement
  (`round6-trial-effects-controllers.md` 4). S14-C4 is closed as not a server gap.
- **Not gated:** the internal automatic backpack replacement (`Inventory.TryEquipNewBackPack` from
  loot / craft / quests). Those callers may hold the character lock, so a `buffs` check there could
  deadlock; the client-side interaction and equip gates that lead to them are covered above.

## Doodad func item requirements at cast start (2026-10-31)

The C# checked the item requirement of a doodad-use func only inside the func body, after the
skill had already started. The 1.2 client has no pre-check for these skills
(`skills.link_equip_slot_id` is -1: 15307, 21910, 22225, 15734, 21904, 22201, 13789, ...) and only
stops a cast when `SCSkillStarted` carries a non-zero result (1.2 client `FUN_3939a810`, about
750316-750338 in `out-x2game/x2game.dll.c`), so the bar and animation played before the effect
refused. `systems::npcs::doodad::item_gate::skill_item_refusal` now answers the same requirement
at the cast start, through `skills::validation::check` (both `Skill.Use` and `Skill.Cast`) and
before the mount/rider branch of `CSStartSkill` for player casters:

- `DoodadFuncConsumeChanger` (trade-pack storage, Hang Fish, Store Fish in Water Tank, tic-tac-toe):
  the equipped `slot_id` item missing -> `SCErrorMsg(UNKNOWN_ITEM 213)`; an item not in
  `doodad_func_consume_changer_items` -> `SCErrorMsg(STORE_INVALID_ITEM 87)`.
- `DoodadFuncRemoveItem`: the bag must hold `item_id` x `count`.
- `DoodadFuncBuyFish` / `DoodadFuncConvertFish`: the backpack slot must hold a pack ->
  `SCErrorMsg(STORE_BACKPACK_NOGOODS 361)`.
- `DoodadFuncPurchase` / `DoodadFuncRecoverItem` stay effect-time only: their reasons (bag space,
  ownership) were not shown to be cast-start checks.

The refusal result is `NEED_REAGENT` (0x24) with the item template id as its value where one is
known; the 1.2 client's result handler (`FUN_39390900`) names that item for 0x24. **Inferred:** the
10.x zone decompile (`x2game-dev_dedicate.dll.c`) contains only the `DoodadFuncConsumeChanger`
descriptor loader and factory (2841893-2842045 and 2605824-2606182) and no use / can-use check, so
the real 1.2 result code is not recoverable; the choice follows the research recommendation
(`Doodad item requirements ...`, 2026-10-31-tradepack-gate.md) and the `SkillResult` 36 =
`NEED_REAGENT` mapping in the same decompile (2826887). The effect-time checks stay as a second
line of defence for an item that changes during the cast.

The Farm Wagon's eco fuel is not a doodad func: skill 20291 (its bar entry, mount skill row 190)
carries the rider skill 20292 (`mount_attached_skills`, attach point 1), whose `skill_reagents`
are one of 26548 / 34583. The mount branch of `CSStartSkill` now runs the rider skill's **full**
`Skill.Use` check (`skill::unit_use_skill_check`, the check-only mode of `use_skill`) before the
slave skill casts, and refuses the press with the rider's result/value when it fails. That covers
reagents, requirements, cooldown and the other checks the late `unit_use_skill` call would apply,
so the missing fuel no longer leaves buff 4841 behind and no slave animation is played. With fuel
the rider skill still runs after the mount skill and consumes the item. The same gate covers every
`mount_attached_skills` row with reagents (the ship speed boosts 25632/22459/22449/22451/22450/
22449, the cannon projectile rider skills, ...), not only the wagon. The 10.x decompile has
`mount_skills.check_attach_point` / `must_use_check` and the `mount_attached_skills` loader
(`x2game-dev_dedicate.dll.c:2614999-2615210`), consistent with a pre-check, but its runtime
consumer was not located; the 1.2 ordering/result is therefore not proven there.
- **RemoveItem now needs the full count before the cast.** Before, the func body consumed what
  the bag held and advanced the phase whenever the remaining balance was >0
  (`bag_consume_item`/`container.rs:806`), so a partial stack paid and advanced. Doodads 7261 and
  7281-7283 (item 29943 x10 / x30) could be advanced with fewer items; the cast-start gate now
  requires `item_id` x `count` (the effect-time consume stays as the second check).
- **The rider's full check can now refuse the vehicle skill itself.** Because the mount skill is
  refused when the rider's `Skill.Use` check fails, unit requirements, cooldowns and target
  requirements that used to fail only after the slave cast now stop the press. Examples: horse
  archery 11018 / 11019 / 17156 need a bow, and torpedo 25538 -> 23123 needs target NPC 13680.
  The same applies to buff requirements and to the rider's own cooldown.
- **`source_cannot_use_while_walk` is not enforced.** The C# loaded the column
  (`SourceCannotUseWhileWalk`) but never read it. The Rust `validation::check` (batch commit
  668e4196b) refused a cast with `skill_cannot_cast_while_walking` (20) whenever the caster's last
  own move had a velocity. That is not the client's walk mode (the move packets carry no walk
  bit), and 60 of the 70 flagged 1.2 skills are mount bar skills or their rider skills (Run!,
  Dash, Overrun, Breakthrough, Skydash, Charge, ...), pressed while running. A mounted rider
  never refreshes the flag (its mate's moves go to the mate), so once the rider pre-check above
  checked the linked rider skill, Stormdarter Run! 17092 (rider 18228) and Keep Running! 17526
  (rider 12815) were refused with result 20 and no visible error after the rider had walked up
  to the mount. The check and `UnitSkillState.moving` are removed; the template field stays
  loaded. Walk mode is client state.
- ConsumeChanger refusals now carry the first accepted `doodad_func_consume_changer_items` id and
  BuyFish carries the func's fish item id as the `NEED_REAGENT` value (they were 0), so the 1.2
  client's item-name lookup (`x2game.dll.c:742315`) always finds a template. The doodad gate runs
  after `gathering::allows_skill` (a disallowed skill keeps `INVALID_TARGET`) and is skipped for
  Mount casters, whose effect caster is the slave.
- **Departure saves write only the departing and financially dirty characters.** C# `Character.Save`
  at logout saved the departing character only; the Rust departure save also commits the global
  property sets (items, mail, auction, houses) so money moved by a counterpart never splits from
  its character row. It used to rewrite every online character per logout (O(N) per logout, O(N^2)
  in a logout wave, all under `transaction_lock`). `SnapshotScope::Departure` now captures the
  pending departures plus online characters whose `(money, money2)` differ from the value last
  committed/loaded (`SaveManager::committed_money`, seeded on enter-world, set by the commit ack,
  dropped on final departure; no entry = dirty) or that hold `labor_power_unsaved != 0`. Autosave
  and shutdown still capture everyone. Money is the only character-row field another character's
  operation changes. The "Saved N active buff(s)" log is `debug!`.

## Cooldown display and persistence (2026-11-02)

The C# `SCCooldownsPacket` was a stub that was never sent, and `UnitCooldowns` lived only in
memory; the port's writer was already correct but only reported a cast when the skill had an
authored `cooldown_tag_id`. Native `OnCooldowns` (`x2game.dll.c:363574`, handler `391d43c0` ->
`39899d40`) appends entries to the client's cooldown manager, and the action bar reads that
manager through `GetActionCooldown` (`3943f310` -> `3989a3e0` -> `3989a290`). `39899d40`
appends without de-duplication and silently drops entries once a list holds 150 (DEC
1930822-1930831), so the server must not resend entries the client already has.

The client starts most cooldowns itself. `OnSkillFired` (`3939b010`) calls
`39368f90` -> `39368440` -> `3989a360` -> `39899f90` when the `SCSkillFired` packet's f bit0
(`+0x154`, DEC 750636) and trailing flag bit1 (`+0x160`, DEC 750694) are 0 and the template has
`toggle_buff_id` (`+0x1c0`) equal to the zero-initialised `DAT_3a17770c` and `timing_id`
(`+0x1bc`) non-zero. The loader's temporary row struct is based at `[ebp-0x2c8]` (its first
column is stored there and `FUN_39719df0` copies from it), and column 0x87 is stored at
`[ebp-0x10c]` = temp+0x1bc, column 0x88 at `[ebp-0x108]` = temp+0x1c0 (disasm
397327fe-3973281d): so `+0x1bc` is `timing_id` and `+0x1c0` is `toggle_buff_id`. Cross-checks:
column 0x27 `cooldown_tag_id` -> `+0x84` (read by `39899f90`) and column 0x28 `cooldown_time` ->
`+0x88` (read by `39368f90`). `DAT_3a17770c` is the zero dword in `.data` at RVA 0x118770c (base
0x38ff0000; no writer in `.text`). The condition is `toggle_buff_id == 0 && timing_id != 0`:
15085 of the 15126 skills, including Recall (14849 / 21586), Escape (20297) and the consumables.
`39899f90` de-duplicates by id and, when the template has a `cooldown_tag_id`, writes only the
TAG entry, so the client starts shared tags for these skills too.

Toggle skills are started by `OnBuffRemoved` (`39389b70` -> `39368f90`, DEC 736146) when the
toggle buff is removed. The `Cooldown` special effect (40) is applied by the client itself
(`OnPlotEvent` special 0x28 -> `39368440`), and the casting / channeling / plot casting-stopped
packets carry their own duration (`39899f90` at DEC 738201, 739427, 747156). None of these
paths needs a server `SCCooldowns` update.

The server sends a per-cast `SCCooldowns` only where the client cannot start the timer:

- `plot_only` with a `plot_id`: `use_skill` commits the cooldown and returns before `cast`, so no
  `SCSkillFired` is sent (133 skills with a cooldown; the other `plot_only` rows have no plot id
  and do fire).
- the 8 `timing_id == 0` skills (2 with a cooldown: 2 and 4).
- the entries are sent with the individual timer and, when the skill has one, its shared tag.

This keeps `OnSkillCooldownReset` (`391d6420`) clearing the single record; the `ResetCooldown`
special effect (43) and `InstantGameManager` reset (`instant_game.rs`, one
`SCSkillCooldownReset` per skill of tag 378) send their resets and clear the matching server
records. Clearing them is a deviation forced by persistence: the C# reset only sent the client
packet, which was enough while `UnitCooldowns` died with the session; here a relog would
otherwise restore a cooldown the battlefield reset removed. The battlefield reset clears each
skill's shared `cooldown_tag_id` record as well: tag cooldowns are persisted too, and a running
tag would keep refusing the skills the reset showed as ready.

Recall's path: skill 14849 / 21586 is **not** `plot_only` and has no `plot_id`; it is a normal
`timing_id == 1`, `toggle_buff_id == 0` skill, so the client starts its 2000 ms cooldown itself.
The original "no countdown after using Recall" report is therefore not caused by a missing
per-cast packet; the confirmed relog symptom was the memory-only store, which is fixed by the
persistence below (the select sync re-sends every restored entry).

Persistence: `character_cooldowns` (`character_id`, `cooldown_type` 0 = skill id / 1 =
`cooldown_tag_id`, `cooldown_id`, `expires_at_ms` BIGINT Unix ms) is rewritten by every
`Character.Save` with the unexpired entries and read by `Character.Load`
(`db_character::fetch_character_cooldowns` -> `apply_cooldown_rows`). The select sync
(`world::session::send_running_cooldowns`) sends the restored list, and the same map backs
`UnitCooldowns::check_skill_cooldown`, so a cast before the restored expiry is refused with
`COOLDOWN_TIME`. **Deviation:** the C# had no persistence and no real cooldown packet; this port
adds both.

## Trial cancellation and jury verdict-count packets (2026-11-05)

The 1.2 client registers `TRIAL_CANCELED` in `usertrial/trial_status.alb` and `VERDICT_COUNT`
in `verdict.alb`; the C# trial flow sent neither packet. Both are now sent from the existing
trial state machine (`systems/social/trial.rs`); their layouts are read from the client:

* `SCTrialCanceledPacket` 0x183, reader `FUN_397afa30`: `u32 trialId`.
* `SCChangeJuryVerdictCountPacket` 0x17a, reader `FUN_397af940`: `i32 count ; i32 total`.

Rules and **INFERRED** audiences (no capture of the 1.2 server has these packets yet):

* `SCTrialCanceled` goes out when the trial reaches `TrialStep.TrialCancelled` (the active jury
  count dropped to 0) and when the defendant sends `CSCancelTrialPacket`, before
  `SCRulingClosed`. The audience is the `TrialData::send_packets` set (the defendant and every
  seated jury member), because the C# defined `SendPackets` as the trial audience and sent
  nothing here.
* `SCChangeJuryVerdictCount` goes to every seated juror who has not voted yet: once when the
  `JuryVerdict` step starts (count 0) and after every accepted vote. `count` is the number of
  votes cast and `total` the number of seated jurors (a seat with a `jury_member`), matching the
  `SCRulingStatus(count, total)` numbers the same step already sends. The audience is INFERRED
  from `verdict.alb`'s "remaining" display.

`CSJurySummonedPacket` is the client's acknowledgement after `SCSummonJury`. It changes no
server state; the handler only checks that the sender holds the seat
(`trial.jury[jury].jury_member`) and logs the result at debug (the C# logged a warning).

GM `crime points <player>` (the no-change stats branch) now prints the trial history from the
`characters` table columns (`arrest_count`, `accept_trial_count`, `accept_guilty_count`,
`guilty_count`, `not_guilty_count`) and the number of the player's judged records in the `crime`
table (`judgement_time` set by `TrialData.ArchiveEvidence` when `Justice.KeepHistory` is on).

Tests: `trial_canceled_and_verdict_count_match_the_1_2_readers` (byte layouts),
`defendant_cancel_sends_trial_canceled_before_ruling_closed`,
`trial_without_jury_is_cancelled_with_the_default_sentence` (empty-jury auto-cancel),
`verdict_count_asks_only_the_jurors_who_still_have_to_vote`,
`jury_summoned_requires_the_seat_holder_and_changes_nothing`, and
`crime_root_help_and_points` (trial/archive counts). The bot suspect/arrest effects 84/88/89
remain capture-pending and are not implemented.

## Wanted-player bounty (2026-11-02)

The 1.2 C# tree only had the packet shells: `CSRequestSetBountyMoney` / `CSSendBountyUpdate`
were logged no-ops and `CSNaviOpenBounty` answered an empty list (the comment there records that
real rows broke the client Lua). This port implements placement and payout from the client
evidence; none of it is recovered 1.2 server logic.

Flow (`systems/social/packets/trial.rs`, readers `397b13a0` / `397bc6b0` /
`397c6fc0` / `397c7080` / `397c7180`):

* `CSRequestSetBountyMoney(name)` resolves an **online** character a player can name and answers
  `SCSetBountyPermitted(targetId, name, curTotal, allowed)`. Only the wanted buff tag allows the
  placement (`check_buff_tag(.., tags::WANTED)`, the same predicate as the arrest); `allowed ==
  false` is the client's error **584** (`FUN_3907dee0`, `0x248`). An unknown name gets
  `SCSetBountyDone(targetId 0, name, 0, 585)`; a refused self-target gets 586.
* `CSSendBountyUpdate(targetId, name, amount)` requires the matching permit from the request,
  checks the id/name again and must still find the target online and wanted. It debits exactly
  `amount` copper through `change_money(Inventory -> None, ..., ItemTaskType.SET_BOUNTY_MONEY =
  87)`, which checks and debits under one character lock, and answers
  `SCSetBountyDone(targetId, name, newTotal, 0)`. Every refusal is error **585** (no such online
  target) or **586** (everything else) through that packet; the client shows the id.
* Pledges **stack** per target (several payers, and several placements per payer) and have **no
  fee** (`SET_BOUNTY_MONEY` task, amount = pledge). `amount` is the client's signed `i32`, so
  `0 < amount <= payer's purse`; the funds refusal is 586 (see BOUNTY-1).
* The delay between the two packets is a server-side permit, `PERMIT_TTL_MS` = 5 minutes, replaced
  by the next request and consumed only by a successful placement. A second
  `CSSendBountyUpdate` without a new request gets 586.

Payout (`systems/skills/combat.rs`, inside the arrest decision of `character_death_tail`): the
arresting player receives the whole accumulated total with `ItemTaskType.PAY_BOUNTY_MONEY = 88`
and `SCBountyPaid(hunter, arrested, total)`; the pledges become paid and the target's total
resets to 0. The hook is inside the `if arrest` branch and pays only when
`arrest_criminal(..)` answered `Some`: an NPC, guard or mob kill (no `arrestor`), a pirate killed
where the arrest is skipped, and a region with no court room or no jail all pay nothing and keep
the pledges (BOUNTY-4). A repeated settlement pays nothing, and a self kill is never paid.

Persistence (`systems/social/bounty.rs`, `SQL/updates/2026-11-02_aaemu_game_bounty.sql` and
`SQL/aaemu_game.sql`): `bounty_pledges(id, payer_id, target_id, amount, created, paid)`. Paid rows
stay in the table as the record of who contributed, but are dropped from memory once their paid
state is committed; `id` continues above the highest saved id on load (unpaid rows only). The
dirty rows (new or newly paid) are captured under the economy operation lock together with the
character wallet snapshot (`managers/save::capture_financial_snapshot` -> `BountyManager::
prepare_save`) and written by `social::System::save(AfterCharacters)` in the same transaction as
`Character.Save`; `commit_financial_snapshot` clears the dirty set only after the transaction
commits, so a rolled-back save is retried instead of losing the pledge. A save with no dirty rows
writes nothing. `CharacterLoad` sends the character's own active total in the verified
`SCCharacterState.bountyMoney` field (X2G 1302074-1302079).

`SCUnitBountyMoney` (`systems/social/bounty.rs`, `packets/trial.rs`): the writer is located
(`FUN_397b3b50` -> `bc objId ; i64 moneyAmount`, client handler `FUN_391d6680`), so the packet is
sent with the **whole** total, never a delta: after a successful pledge to every viewer of the
target (and the target), to a viewer whose client just received the target's entry `SCUnitState`
(both entry paths: `world::session::spawn` and `instances::worlds::instance_loaded`), and 0 after
the payout. A total of 0 sends nothing on entry. When the client draws the value is INFERRED
(`OnUnitBountyMoney`), so sending it after both entry paths is harmless (BOUNTY-3).

Policy decisions / capture-pending (each also a case id in the code):

* **BOUNTY-1** amount bounds and the 585/586 split. The client has error strings only for 584
  ("not wanted", `SCSetBountyPermitted.allowed == false`), 585 ("no such name") and 586
  (generic); the decompile gives no server predicate. Chosen: `0 < amount <= purse`, no fee,
  self-target refused with 586, insufficient funds 586. Needs an original-server capture.
* **BOUNTY-2** offline targets, permit TTL, no expiry. A target must be online to be bountied
  (the client's name comes from a live unit menu); the pledge survives the target's or the
  payer's logout and a restart, and is paid whenever the target is arrested later. Whether 1.2
  allowed an offline target, and whether `bountyTime` (`SCCharacterState`, still 0) means an
  expiry, is unknown; the permit TTL is our bounded-map policy, not client behaviour.
* **BOUNTY-3** `SCUnitBountyMoney` (`0x0bb`). No longer open for the layout: the writer
  `FUN_397b3b50` (`dll.c:375748`) serializes `bc objId ; i64 moneyAmount`, the same bytes our
  struct writes, and the handler `FUN_391d6680` reads the id at `+0xc` and the i64 at `+0x10`.
  The senders are the pledge broadcast, the two entry paths and the payout 0 (see above). Which
  widget uses the value is INFERRED (`OnUnitBountyMoney`); a live capture would confirm when the
  client displays it, not the wire format.
* **BOUNTY-4** `SCBountyPaid` recipients and the no-arrest case. The client's chat text
  (`ui_text` 4764) names the hunter, the arrested and the amount; the recipient scope (hunter /
  zone / faction) is unknown. It is sent to the arresting player only. Whether a pirate killed in
  a desperado zone (where `arrest` is false) should pay is also unknown; the arrest text ties the
  payout to an arrest, so it does not pay.
* **BOUNTY-5** settlement edge cases. The arrester must still be online when the death tail runs;
  otherwise nothing settles and the pledges stay (the copper must reach a live character). The
  death tail may also run while the non-reentrant economy ops lock is held (an item or skill
  effect killed the victim); settlement then logs and keeps the pledges instead of deadlocking.
  A deleted character's pledges are never refunded and stay in the table for its id. Arena and
  instance deaths use the ordinary PvP death tail, so they pay like any other arrest. None of
  these have 1.2 evidence; they are safe defaults.
* **BOUNTY-board** `CSNaviOpenBounty` / `SCBountyList`. A filled list still breaks the 1.2
  client UI, and the 2026-11-06 real-client run (one wanted row, client log
  `ArcheAge-bounty.log` lines 1077-1099) shows the cause is in the client's own
  `usertrial/bounty_bullet.alb`, not in the row values:
  * On every open it logs `?:6422 LocalizeUiText() expect parameter 4 of type String (Provided
    type Number)`. `FillLeftFrame` (lines 42-43) calls
    `locale.trial.trialChoice(tostring(record[12]), record[11])` and
    `rulingChoice(tostring(record[14]), record[13])`; record 11/13 are the numeric
    `acceptGuilty` / `notGuilty` counters built by the client's `GetBountyData`
    (`FUN_39080c00` indexes 11 and 13, type tag 4 = number), and the wrappers
    `baselib/locale_helper.alb:6418/6422` pass them as parameters 3/4 of
    `X2Locale.LocalizeUiText`, which requires strings. The sibling trial scripts
    `crime_records.alb` (0.17) and `bot_records.alb` (0.10) call `tostring()` on both counters;
    `bounty_bullet.alb` omits the second one. No server field can turn a numeric row column into
    a Lua string.
  * Then it logs `?:115 attempt to index global 'listCtrl' (a nil value)`:
    `bounty_bullet.alb` FillData line 115 calls `listCtrl.InsertData`, but no `.alb` in the
    client ever assigns a global `listCtrl` (the page list owns `listCtrl` as
    `wnd.listCtrl` / `pageListCtrl.listCtrl`, and the sibling scripts call
    `pageListCtrl:InsertData`). The row insert therefore aborts for every row.
  * Clicking a board button logs `?:53 attempt to compare number with nil`: the handler compares
    the global `curSelected`, which is never assigned either; the selection handler updates the
    `CUR_SELECTED` upvalue instead.
  The board stays behind `Social.BountyBoard` (default `false`, shipped in
  `Configurations/Social.json`) until the client script is fixed, because the real 1.2 client
  cannot render the rows. When enabled, `SCBountyList{total, count, rows}` reports `total` as
  every character with an unpaid pledge and caps the rows at the client's 20 entries (`0x14`,
  `MAX_BOUNTY_LIST_ENTRIES`), highest total first (ties by character id; the order is a server
  policy, not client evidence, and only decides which rows the cap keeps). Online rows read
  name, level, race, faction, crime point, zone and position from the live character with
  `is_live = true`; offline rows read the same fields plus the arrest / trial counters from its
  `characters` row (`deleted = 0`) with `is_live = false`. A missing or unreadable character row
  is skipped and no longer counted in `total`, so the client can still complete its id-keyed row
  map. The row's `+0xb8` field (called `Type184` in the C#, whose dummy row sent `1`) is the
  **faction id**: the client's `GetBountyData` record 9 feeds it to `FUN_392f8120(id, 1)` and
  uses the result as the left frame's faction name (`FUN_39080c00`; the same call fills the
  field literally named `"faction"` in `FUN_3907e830`). Online rows now send
  `unit.faction_id`, offline rows `characters.faction_id`, instead of the C# dummy. `seconds`
  is `0` (no 1.2 meaning known, no timer is invented). `total > count` only while more than 20
  characters have pledges. The real-client check stays open until a client `bounty_bullet.alb`
  that uses `pageListCtrl.listCtrl` and `tostring()` on both counters is available.

* **STEALTH-C** `SCUnitState` character flag bit 5 (`0x0020`, GM invisible). The C#
  `SCUnitStatePacket.Write` set bit 5 from `character.Invisible`, but the 1.2 client side is
  unproven: `OnUnitState` (`FUN_391d4c30`) applies the parsed state through `FUN_391b90a0`,
  which is a thunk in the x2game decompile, and no reader of the flags byte's bit 5 was located.
  Bit 3 (force attack) is verified (`397c9480` -> `398a1f90` -> unit+0x3bbc); `SetInvisible`
  writes unit+0x3bbb, but nothing in the decompile shows it coming from the state flags. The Rust
  keeps writing only bit 3 (`world/unit_state.rs`), so a late observer of a GM who toggled
  `/invisible` still sees the GM until the next `SCUnitInvisible` broadcast, and `unit.invisible`
  is not persisted across a relog. Capture with the original client (GM `/invisible`, second
  client enters view / relogs) before setting bit 5.

Deviation from the removed C# history: C# never placed, settled or persisted a bounty, so there
is no C# behaviour to mirror. The pre-existing `bounty_runtime.rs` design invented a 24-hour
expiry, a placement fee and a same-account refusal; all three are gone in favour of the
client-evidenced rules above.

## Server features the 1.2 client cannot reach (catalogue, 2026-11-06)

Server functionality that is implemented and tested but that the stock 1.2 client UI cannot
show or start, because of bugs or missing wiring in the client's own UI scripts. The original 1.2
client is left as it is (no client patches are shipped); the Bevy client is where the UI is
fixed. Each entry stays here until a client can use it. Evidence:
`D:/aaemu-audits/gap-audit-2026-11-02/research/bounty-board-workaround.md` (local research
notes, not in git).

| Feature | Server state | Why the 1.2 client cannot use it | How it is reached today |
|---|---|---|---|
| Bounty board list (`CSNaviOpenBounty` -> `SCBountyList`) | rows implemented, behind `Social.BountyBoard` (default `false`, keep it off for the 1.2 client) | `usertrial/bounty_bullet.alb` `FillData` indexes globals that no 1.2 script sets (`listCtrl`, `bountyRecords`, `index`), so any row aborts the window before it is shown; the client also keeps every received row until restart | headless `aaemu-testclient`; Bevy client (fix pending) |
| "Offer a Reward" on the board | placement implemented (`CSRequestSetBountyMoney` / `CSSendBountyUpdate`) | the button handler compares the unset global `curSelected`, so every click fails | as above |
| Placing a bounty (right-click / setup window) | implemented (see "Wanted-player bounty") | the popup-menu entry is never added, `SET_BOUNTY` has no UI handler and `setup_bounty.alb` registers the board toggle to the wrong function | GM `set_bounty_money <character> <amount>`; headless client |
| Wanted-list chat command (`/wanted`) | not built (owner decision 2026-11-06: catalogue only, do not add for now) | - | - |

Arrest payout (`SCBountyPaid`, `PAY_BOUNTY_MONEY`), `SCUnitBountyMoney` and the wanted buff work
with the stock client.

## Water-only skill targets (2026-11-03)

`target_only_water` casts (fishing rods) are admitted against the water surface the 1.2 client
actually sends. The client aims `target_water` / `target_only_water` casts at the water surface
(`FUN_3942b060`, `x2game.dll.c:872033`; `FUN_394322f0`, `:877530`: both pass the surface flag to
the target picker when either water flag is set). Its own admission check `FUN_390b43b0`
(`x2game.dll.c:148796`) probes `(x, y, z - 0.1)` (`DAT_3999ccf0`), requires the water level
there at or above the probe (`probe.z <= level`), and then requires a floor at least 1 m below
the level (`floor <= level - 1.0`, `DAT_3999c488`). That floor query is I3DEngine vt+0x108
taking `(Vec3*, float)` - likely `GetBottomLevel`, a downward ray that includes statics and
answers -1e9 when nothing is hit, which passes the depth test.

The Rust port had probed `(x, y, z + 0.1)` since 668e4196 (`systems/skills/validation.rs`), so a
surface point (the only point the client sends) landed above the water and every rod cast was
refused with `skill_invalid_location` (0x23). `validation::target_in_water` now probes 0.1 m
below, uses `collision::water_level` for the surface and `collision::terrain_height` for the
floor (skipped when the collision service is off and no height is known). The server's floor is
terrain only, so the rule is more lenient than the client's ray when static geometry lies under
the point. Tests:
`systems::skills::cast_validation_tests::water_only_targets_probe_below_the_surface_and_need_one_metre_of_water`.

`source_not_swim` keeps its own probe 0.1 m **above** the caster (`validation::source_in_water`).
That is the port's previous behaviour, not client evidence: applying the target rule to the
source newly allowed a caster wading in water less than 1 m deep.

Not ported: with `target_fishing` (target+0x194) set, `FUN_390b43b0` also refuses with 0x23 a
type-8 water entity whose flag bit 1 is set (`vt+0x150 & 1`, likely a NoFishing volume)
(`x2game.dll.c:148821-148834`); the server does not resolve or test that entity.

Deviation from the removed C#: `SkillTemplate.TargetOnlyWater` was parsed
(`AAEmu.Game/Core/Managers/SkillManager.cs:423`) but never read, so the C# had no water-target
check at all; this follows only the 1.2 client.

## Craft material grades and product grade (2026-11-05)

`craft_materials.require_grade`, `craft_materials.main_grade` and `craft_products.use_grade`
are now honoured at craft execution (WP-I2 A/B; `systems::economy::{craft,craft_data}`).

- **`require_grade` (14 rows in `compact.sqlite3`: crafts 6184-6191 material 30803, crafts
  6889-6894 material 34275, amount 1).** The client loader reads the column
  (`LoadCraftMaterialDescs` `0x3982E4D0`, `x2game.dll.c:1825263`). `GetCraftMaterialInfo`
  (`0x39462600`, `x2game.dll.c:914799`) counts a bag stack only when
  `require_grade == -1 || require_grade == item.grade`, and `GetExecutableCraftCount`
  (`0x394d3cc0`, `:1005386`) passes `require_grade` as the grade argument of
  `FUN_3961fbc0`, which compares `param_3 == item.grade` (`:1301667`). The port therefore uses
  **exact equality** (`-1` = any), not `>=`: a lower grade does not count and neither does a
  higher one. Rows that require a grade are counted, checked at cast start and again before the
  hand-out, and **consumed only from stacks of that grade**
  (`ItemContainer::consume_item_by_grade`, `models/container.rs`). A refusal sends the client's
  `not_enough_required_item` (722; error-name case `0x2d2`, `x2game.dll.c:1137158`) after the
  usual `craft_cant_act_any_more` (148) and changes nothing.
- **`main_grade` (101 rows).** The flagged material's grade is the inherited product grade
  (`X2Craft.GetCraftMaterialInfo` emits `mainGrade`/`itemGrade`; the client's
  `crafting_renewal.alb` proto 9.14 (lines 556-573) takes the `mainGrade` material's
  `itemGrade`). All 101 rows sit on crafts whose product has `use_grade = 'f'`, so the flag
  alone drives the inheritance; before this change they went through the C# "first equipment
  material" heuristic. The inherited grade keeps the C# `FreeRegrade` roll (5 %, `rand(0,20)`).
  Only the main material is consumed in the inherited grade; its first (lowest-slot) stack is
  the grade source, matching the consumption order.
- **`use_grade` (356 products).** The client flag is what the craft window uses to show a
  material grade, but the port keeps the C# "first equipment material" heuristic for *every*
  product when no `main_grade` row exists (audit review, 2026-11-05): restricting it to
  `use_grade` products made ~484 gear-upgrade crafts without grade rows (e.g. 4993, 5285,
  5086) lose the inherited grade and the `FreeRegrade` roll. Order: `main_grade` ->
  equipment-material heuristic -> `item_grade_id` -> `-1`. The C# "proper" branch that read
  these flags was commented out (`CharacterCraft.cs`, `a2ab80abd^`). Open, capture-pending:
  the client Lua also inherits from a material of the product's item implementation
  (`info.item_impl`, proto 9.14 lines 561-571); the port keeps the equipment-material fallback
  until a capture shows the same-impl rule's effect on the result.
- **Repeated rows.** Rows that name the same item and effective grade share the stacks
  (9 crafts list one item twice, e.g. craft 370 material 8318 x3 + x1, craft 1200 material
  8320 x1 twice). The check sums the rows' required units before comparing, so a craft cannot
  start with 3 units for a 4-unit requirement.
- **`CSSetCraftingPayPacket` (owner-approved 2026-11-05).** The 1.2 Lua `X2Craft.SetCraftingPay`
  sends CS `0x8d` (`FUN_394d7230`, `local_14 = 0x8d`; the doodad obj id at packet `+0xc` and
  the money at `+0x10` through `FUN_397b0ca0`). The opcode table entry is now `0x08d` (was
  `0xfff`) and the packet is registered; the handler reads `(bc objId, i32 money)`. For the
  shipyard the packet names it refuses pay < 1, `pay x actions > 2,000,000,000` (the client's
  own cap, `FUN_394d7360`), a non-shipyard target and a sender who is not the shipyard's owner
  (`kind2`, the persisted `owner_id`); the checks and the write happen under one shipyard
  lock. Otherwise it stores the per-action wage in `ShipyardData.money_amount` and re-sends
  `SCShipyardState` to the nearby players. No payout
  or escrow is made: who pays the helper, when, and whether the owner's `pay x actions` is held
  are UNKNOWN (INFERRED store-only).
- **Shipyard wage reading (2026-11-06, INFERRED strong).** `FUN_394d63e0` resolves the target
  only when its kind (`+0x410`) is 6 (`BaseUnitType.Shipyard`); the `SCShipyardState` body
  `FUN_3961ef40` puts the i32 `moneyAmount` at unit `+0x9c` (record `+0x2c`), the same field as
  our `ShipyardData.money_amount` (`models/shipyard.rs:20,43`). The setter `FUN_394d7230` (the
  only caller of the Lua `SetCraftingPay`, through `FUN_39460cc0`) refuses pay < 1, a
  non-shipyard target and overflow, caps `total = pay x record.actions` at 2,000,000,000
  copper (`FUN_394d7360`) and requires the local player's inventory money >= total; it then
  sends CS `0x8d` with the **per-action** pay, not the total. The client has no ownership check.
  The only reader is `FUN_394d7310` -> Lua `GetCraftingPayStr` (label `crafting_pay` = "Wage:"),
  so any player targeting the shipyard can read it. The client never changes the field itself,
  so the UI only updates when the server re-sends `SCShipyardState` with the new `moneyAmount`
  (INFERRED; no dedicated ack). UNKNOWN: who pays the helper, when (per build action or at
  completion), and whether the owner's `pay x actions` is escrowed. **Capture plan before
  storing or paying it:** owner A on a shipyard with several actions + helper B; (1) A sets
  100/action and capture the reply (expect `SCShipyardState`, maybe `SCItemTaskSuccess`); (2) B
  does N build actions and capture the money tasks for A and B; (3) A with less than
  `pay x actions` tries and is refused by the client (nothing sent); (4) set 0 and see whether
  the wage clears.

Tests: `craft_material_requires_its_grade` (low and high grades refused, exact grade pays and
only that stack is consumed; refusals keep the stacks), `craft_main_grade_sets_the_product_grade`
(the flag works on a `use_grade = 'f'` product), `craft_equipment_material_grade_inherits_for_any_product`
and `craft_unflagged_upgrade_inherits_the_equipment_grade` (the restored gear-upgrade
inheritance), `craft_material_rows_of_one_item_are_summed` (craft 370's 3 + 1 shape),
`craft_main_material_lowest_slot_decides_and_is_consumed`, `craft_secured_right_grade_material_is_locked_only`
(`ITEM_SECURE_CONDITION`), `craft_grade_lost_before_the_hand_out_keeps_the_materials`, the
`CSSetCraftingPayPacket` byte test (`bench_tests::set_crafting_pay_reads_the_client_writer_body`),
plus the extended real-data `real_compact_db_craft_and_store_tables` (14 `require_grade` rows).
Each fails on its mutation (grade filter `-1`, main-grade branch removed, equipment fallback
removed, per-row instead of summed counts, highest instead of lowest slot, `LockedOnly`
downgraded to `Missing`, hand-out grade filter removed).

## Item crafter (2026-11-07)

The crafter of a crafted item is sent to the 1.2 client in the item record's `madeUnitId` field
(items-crafting#11 / mysql-schema#8 crafter half; the audit's "items have no crafter id" row was
stale for single-stack products, and stackables were still empty).

- **Client field (EVIDENCED).** The standard item reader `FUN_3961f500`
  (`x2game.dll.c:1301392`) reads `templateId`, `id`, `grade`, `flags`, `count`, `detailType`
  + detail, `creationTime`, `lifespanMins`, one u32, `worldId`, `unsecure`, `unpack`; the u32 is
  the item field at `+0x14` (`Item.Write`'s `MadeUnitId`). The tooltip builder `FUN_39423560`
  (`:865915`) takes `piVar1[5]` (`+0x14`) and resolves a **character name** through
  `FUN_390192e0` / `FUN_39018560`, and the `worldId` at `piVar1[0x2a]` to
  `craftedWorldName`; there is no name string in the item detail (`FUN_3961edf0` has fixed
  bodies only), so the field is an **id**, not a name, and the client resolves it itself.
- **What the server wrote before.** `AcquireDefaultItemEx` filled it only when
  `template.MaxCount == 1` (the removed C# `ItemContainer.cs` did the same), so crafted
  equipment / trade packs carried the crafter but stackable products sent 0. The client reader
  has no such restriction.
- **Rust.** `items.crafter_id` (schema below) is set for **every** crafted product in
  `acquire_default_item_ex`; single-stack products also keep `made_unit_id = crafter_id` and
  the default `world_id`, as the C# did. `Item::crafter()` returns `crafter_id` and falls back
  to `made_unit_id`, so rows written before the column (and the non-craft uses of
  `made_unit_id`: fish template id, music sheet author) keep sending exactly what they sent
  before. `Item::write` and `ItemTask::write_task_item_details` write `crafter()` into the
  client field; specialty reads the same accessor. Mail attachments, auction lots, bank moves
  and trade-packs moved through the system container carry the whole item, and the
  cancelled-cast restore copies `crafter_id`; the craft path is the only writer.
- **Deviation from the C#.** The C# limitation to `MaxCount == 1` is not evidenced by the
  client; the Rust records stackables too. A stack merged from two crafters keeps the first
  crafter's id (the same value the client shows for the stack); this is the best a single
  wire field can express and is recorded here rather than inventing a per-unit crafter.
- **Schema.** `SQL/updates/2026-11-07_aaemu_game_item_crafter.sql` (guarded: adds
  `crafter_id int unsigned NOT NULL DEFAULT 0` after `made_unit_id` only when missing) and the
  base `SQL/aaemu_game.sql`. `ItemManager::save` / `load_user_items` carry the column.
- **Tests.** `item::tests::crafted_item_write_puts_the_crafter_id_in_the_made_unit_field` and
  `item::tests::item_without_a_crafter_column_keeps_the_legacy_made_unit_id_on_the_wire`
  (byte-exact against the `FUN_3961f500` layout), `item_task::tests::item_add_writes_the_crafter_id_in_the_made_unit_field`,
  the craft flow assertions in `craft_batch_consumes_materials_adds_products_and_reschedules`
  and `craft_trade_pack_equips_into_backpack_slot`, and the MySQL round trip
  `private_mysql_item_crafter_survives_save_and_load` (gated by `AAEMU_TEST_MYSQL`).
- **Left as-is.** Craft pay (`CSSetCraftingPayPacket`) is stored on the owner's shipyard only,
  with no payout (see "Craft material grades and product grade"); `doodad_func_craft_grade_ratios`
  has 0 rows in 1.2 (and no table in 10.0), so there is nothing to implement
  (`round6-items-mail.md` 2); the Mia mail variants stay capture-pending.

## Recipe learning and the learned-craft gate (2026-11-04)

`TrainCraftEffect` was trace-only in the C# and in the port, so a recipe item was consumed and
nothing happened. The 1.2 data: of the 2,561 `items.impl_id = 12` recipe items, 2,461 use the
generic skill 11144, one uses 10003, and 99 have no use skill; skill 11144 -> `skill_effects` 1611 -> effect 1750
`TrainCraftEffect` (actual_id 305) -> `train_craft_effects` 305 -> craft 169. Because 11144
teaches the fixed craft 169 for thousands of items, the learned craft comes from
`item_recipes.item_id` -> `craft_id` for the cast's source item; only a `TrainCraftEffect` with
no source item (the 131 plain training skills) uses `train_craft_effects.craft_id`. The loader
is `systems/economy/craft_learning.rs`; it skips the 49 recipe rows (48 distinct crafts) and 1
effect row whose craft is missing from `crafts` (with one warning). The learn-gated set holds only
crafts with a reachable source: an `item_recipes` row whose item exists and whose `use_skill_id`
carries `TrainCraftEffect`, or a `train_craft_effects` row carried by a skill another table grants,
requires or commands (`default_skills`, `slash_commands`, `skill_req_skills`, the quest supply /
accept grants). That is 2,412 of the 2,762 candidate crafts in compact; the other 350 (222 with
no training effect, 128 whose training skills nothing references) would otherwise be locked
forever, so they stay ungated. The counts are logged once at load.

Learn: the effect inserts the craft into `Character::learned_crafts` under one character lock;
the skill pipeline consumes the source item as before (`use_skill_as_reagent = 't'` on every
recipe item). Already learned: the effect sends client error `craft_already_learned` (0x81 = 129;
client `x2game.dll.c:1136026`, `ui_texts` keys `craft_not_learned` / `craft_already_learned` at
`strings.txt:12550-12551`) and cancels the cast, so `Skill.Apply` refunds the pre-debited item
(the same cancel flag the dye path uses). A recipe item whose craft is missing from the data is
also refused and refunded. `UnitReqsKindType.CanLearnCraft` (0x13) was commented out in the C#
(`UnitReqs.cs:96`, `a2ab80abd^`); it is now evaluated as "craft not learned" because the 1.2
data has 9 training skills using it (crafts 3, 4, 6, 8, 9, 10, 11, 169, 182). This is an
INFERRED rule, recorded as a deviation from the C#.

Persistence: `character_crafts(character_id, craft_id, learned_at)` (SQL update + base schema) is
loaded in `db_character::load_character_details`. A learn adds the craft to
`Character::learned_crafts_unsaved`; the character save writes only those ids in one multi-row
`INSERT ... ON DUPLICATE KEY UPDATE` with `UTC_TIMESTAMP()`, and the save ack clears them from the
live character (like the removed-list fields). A pending learn also makes
`SaveManager::is_financially_dirty` true, so the recipe-item deletion and the learned-craft row
commit in the same snapshot. The
gate is `Crafting.RequireLearnedRecipes` (`Configurations/Crafting.json`, default **false**):
when true, `craft::craft` refuses a gated unlearned craft with
`CraftCantActAnyMore`/`craft_not_learned` (0x82 = 130; `x2game.dll.c:1136028`). The default is
false because whether retail 1.2 enforced the gate at all is UNKNOWN (WP-I1 UNKNOWN-1); the
client carries the two errors, the `CRAFT_TRAINED` / `CRAFT_RECIPE_ADDED` event names
(`FUN_39214380`, indices 117/118) and the craft book, but no server evidence.

Capture-pending (WP-I1 UNKNOWN-2): no packet tells the client that a craft was learned. The only
writes to the client's `item_recipes` arrays are its own loader (`x2game.dll.c:1478522`) and
`CRAFT_TRAINED` has no located firing site, so the server sends no announcement; a capture of
the original client using a recipe item is needed to find the carrier (if any).

## Invitation cancellation (`SCInvitationCanceled`) (2026-11-05)

`SCInvitationCanceled` 0x1dd is one `u8 kind` (`systems/social/packets/misc.rs`; byte test
`invitation_canceled_is_one_kind_byte`). The 1.2 client reader `FUN_397b8490` and handler
`FUN_391d6930` -> `FUN_39182930` close **every** popup of the type the kind maps to; there is
no invitation id on the wire (RE-UI1). Mapping: `1,2 -> type 2` (named / area team), `3 -> 4`
(family), `4 -> 3` (expedition), `5 -> 9` (duel), `6 -> 6` (jury), `7 -> 0x29` (nation
immigrate).

Sent by this server when an invitation ends without an answer:

* Team: the 500 ms sweep after the 60 s timeout and `set_offline` when the inviter leaves the
  world. Kind 1 for a named, kind 2 for an area invitation. The 1-vs-2 split is UNKNOWN: both
  kinds close the same client popup type and behave identically, so the mapping is INFERRED.
* Expedition: the 60 s expiry (kind 4). The C# and the previous port pruned the map only on
  the next invite, which left the popup open; a 1 s sweep now runs on the shared
  `TickManager` (no extra thread).
* Duel: the 30 s `request_expired` (kind 5). An accepted challenge never reaches the release
  path, so it sends nothing.
* Nation immigrate: the 60 s expiry (kind 7), on its own 1 s `TickManager` subscription (the
  60 s relation tick would close the popup up to a minute late).

Not sent, because no evidenced trigger exists: kind 3 (family: 1.2 has no server timeout and
the inviter-offline case is UNKNOWN) and kind 6 (jury: trigger UNKNOWN). An accepted or
declined invitation consumes its entry, so it sends nothing. The guard for the "closes every
popup of the type" behaviour is structural: each invitation map holds at most one pending
invitation per invitee (a second team / expedition / nation invitation is refused or
replaces), so a cancellation cannot close another invitation of the same popup type.

INFERRED: that the original server sent this packet on these server-side timeouts (the client
handler is EVIDENCED; the original send sites are not). Capture-pending: none assigned.

## Stealth capture-pending (2026-11)

* **STEALTH-C** `SCUnitState` character flag bit 5 (`0x0020`, GM invisible). The C#
  `SCUnitStatePacket.Write` set bit 5 from `character.Invisible`, but the 1.2 client side is
  unproven: `OnUnitState` (`FUN_391d4c30`) applies the parsed state through `FUN_391b90a0`,
  which is a thunk in the x2game decompile, and no reader of the flags byte's bit 5 was located.
  Bit 3 (force attack) is verified (`397c9480` -> `398a1f90` -> unit+0x3bbc); `SetInvisible`
  writes unit+0x3bbb, but nothing in the decompile shows it coming from the state flags. The Rust
  keeps writing only bit 3 (`world/unit_state.rs`), so a late observer of a GM who toggled
  `/invisible` still sees the GM until the next `SCUnitInvisible` broadcast, and `unit.invisible`
  is not persisted across a relog. Capture with the original client (GM `/invisible`, second
  client enters view / relogs) before setting bit 5.

## Review findings (documented, not fixed)

Independent reviews of the combat / reflect / stealth / ability lanes found the issues below.
Their severity scores are below the 5-point fix bar, so they are recorded here instead of
changed; each one is a real behaviour difference, not a deliberate deviation.

* **Damage (event 3) triggers run from the buff owner, not the original source (score 4).**
  `systems/skills/buffs.rs` (`execute_trigger`, the `s.damaged || s.damage` branch) applies
  the trigger effect with `skill: None` and the buff owner as `caster`; `use_original_source`
  only redirects `source` (and through `effect_on_source` the target), never the caster. The
  authored rows that depend on this are `BuffEffect` 182, `BubbleEffect` 3028,
  `NpcControlEffect` 3659, `DamageEffect` 1792 / 6297 and trigger row 3887
  (`use_original_source`): their effect is cast from the unit that took the damage instead of
  the buff's original caster (or the attacker where the row means the event source).
* **Reflection that kills the attacker still runs the caster-side tail (score 3).**
  `systems/skills/effects/combat.rs` (`damage_with_aoe`) applies reflection before the victim's
  own HP change and does not stop if the reflected damage kills the caster. The rest of the
  function then still runs against the dead caster: summarized damage, health / mana steal,
  criminal checks, `set_in_battle`, item procs, combat buffs, the attack / damage triggers and
  weapon durability.
* **`CancelOngoingBuff` removes stealth even for `keep_stealth` skills (score 2).**
  `systems/skills/effects/special/unit_actions.rs` (`cancel_ongoing_buff`) calls
  `buffs::trigger_remove_on(StartSkill / UseSkill)` directly. The `skills.keep_stealth`
  exception is only applied by `trigger_remove_on_keep_stealth` on the normal skill-start /
  skill-use path (`systems/skills/skill.rs`, `effects/special/combat.rs`), so a
  `CancelOngoingBuff` effect carried by a `keep_stealth` skill still strips the caster's
  stealth buffs.
* **Filling an empty ability slot has no level gate (score 3).**
  `systems/skills/progress.rs` (`swap_ability`, `old == ability::NONE`) only requires one
  ability slot to be empty and makes the activation free. The client's level-10
  `LearnAbility` entry point gates the activation by level, but `CSSwapAbilityPacket` with
  `old = NONE` reaches this function without that check, so a character of any level can
  activate a class ability into an empty slot.
* **`ChatConfig` vectors are not lenient for env / CLI overrides (score 3).**
  `systems/social/config.rs` declares `ChatTypeGroups` / `ChatGroupDelays` as plain `Vec`s, so
  `Character__ChatGroupDelays` / `--Chat:ChatGroupDelays=...` style overrides cannot express the
  array (the generic scalar `lenient` helper is not used for sequences). Configure the arrays in
  `Configurations/Chat.json` or `Config.Local.json` instead. Known, not fixed.
* **A configuration reload does not re-send `SCChatSpamDelayPacket` (score 3).**
  `ReloadConfiguration` swaps the config cell, but the client keeps the delays sent during
  `FinishState(0)`; changing `Chat` requires a reconnect (or relog) before the client's own
  throttle matches the server's new limit. Known, not fixed.
* **Chat / name test shortcuts (score 1).** The chat-spam flow test edits the live `ConfigCell`
  with `update` instead of going through a reload, and the name opt-in test builds two servers
  instead of reloading one; both prove the gates, not the reload path. Known, not fixed.

## Static-table and feature verdicts (2026-11-06, recorded so they are not reopened)

* **`equip_slot_groups` (24 rows) is loaded; `equip_slot_group_maps` has 48 rows, 47 loaded.**
  The group table is the parent of the maps, and the lunagem check now requires it
  (`item_sockets.equip_slot_group_id` -> `equip_slot_groups` + `equip_slot_group_maps` ->
  target slot type, `data/items.rs`, `systems/economy/item_effects.rs`). Map 10 names the
  missing group 9 and is skipped at load, like the orphan `item_sockets` rows. Equipping keeps
  the hard-coded `allowed_gear_slots` (`models/container.rs`, the C#
  `EquipmentContainer.GetAllowedGearSlots`): the data has no target equip-slot column, so it
  cannot express that rule. The real-data guard
  `real_compact_equip_slot_groups_guard_the_hard_coded_slots` checks that every mapped slot
  type is one the hard-coded table accepts and that the nine base groups still name the same
  equip slots.
* **`item_bags` has 5 rows whose ids do not resolve** (no item template references them). The
  1.2 client loads the table itself (`strings.txt:13953`); there is no reachable template and
  no known server rule, so there is nothing to load. Do not reopen.
* **`item_tools` (130 rows) is client-side**: the 1.2 client loads `item_id, asset_id` for the
  tool visual (`strings.txt` 13836), and `crafts.tool_id` is 0 in all 7 010 rows, so no server
  check uses it.
* **`expert_limits` prices are empty:** every `up_price` / `down_price` is 0 and `advantage` is
  a client display value (31 rows checked). The server uses the limit columns for
  labor/expand steps and charges the configured items, so there is no price to enforce.
* **`unit_formulas` kind 35 `AAPointMaxLimitByLevel` is the constant 1500** for owner types
  0-6. Whether it caps the balance or a per-period purchase is UNKNOWN, so it is not enforced;
  an optional guard on the constant is all that is warranted.
* **Doodad UI funcs are client-driven** (see the doodad table above): the 1.2 client loads the
  bank / auction / store / coffer-permission / renew-item UI tables itself, so the server has
  nothing to answer.
* **`SCRestrictInfo` 0x1c8** is a per-character penalty list with an unknown `restrictCode`
  table. A restriction store would be a moderation feature; RE of the client's code values is
  needed first. Recorded, not a package.
* **ARS phone verification needs an operator provider.** The login's `ArsPhoneProvider` seam
  and the session/lockout logic exist; the default `DisabledArsPhoneProvider` refuses such a
  login. Supplying an implementation that places the call is operator integration, not a repo
  task (documented in the wiki).
* **No 1.2 data / client-side, so no package:** the X-day salon certificate (only salon item
  30811; no item with `exp_abs_lifetime` is a certificate), mould tables (0 rows), demo mode
  (no 1.2 trigger), evolving / refurbish / revert look (no `special_effects` rows), doodad
  exchange (0 rows) and buy-fish model rows (prefab strings), `gem_visual_effect_id` (the
  client loads the effect), mate `SpawnEffect` rows (the 4 unreachable rows above), slave
  customizing styles (`slave_customizings` is client-loaded; one customizable slave), radar
  shipyards (no 1.2 shipyard list wire format) and glider tuning (`gliding_*`, `ag_stance`,
  gimmick collision: capture-pending / design-blocked).
* **`conflict_zones.peace_protected_faction_id` / `peace_tower_def_id` are placeholders.**
  All 16 rows carry faction id 5 and a NULL `peace_tower_def_id`; faction 5 does not exist in
  `system_factions` (ids 1, 2, 3 and 101-179) and neither column is ever read
  (`data/world_data.rs:214,293` keeps the faction id in the loaded row only). The used columns
  are `num_kills_*` / `no_kill_min_*` / `conflict_min` / `war_min` / `peace_min`
  (`systems/social/zone_conflict.rs`), `war_tower_def_id` (`systems/instances/tower_def.rs:934`)
  and the two return-point ids (`systems/skills/resurrect.rs:112-113`). Nothing to implement.
* **`world_groups` is client map data.** Its 6 rows (`id`, `name`, `target_id`, `image_map`,
  `x` / `y` / `w` / `h`, `image_x` / `image_y` / `image_w` / `image_h`) describe the world-map
  rectangles the client draws; the server never reads the table. Not a gap.
* **`world_spec_configs` is not read; its one row matches the hard-coded specialty divisor.**
  The only row (`world_01`) has `specialty_mod` 1000 and `specialty_adjust_ratio` 500.
  `economy/specialty.rs:388` hard-codes the `ratio / 1000` divisor that `specialty_mod` mirrors;
  no code reads the table, and `specialty_adjust_ratio` has no known consumer (retail use
  UNKNOWN). No package.

## D1d place-skin terrain material (`Housing.PlacementStrictSurface`)

EVIDENCED (research `re-surface.md`, `re-placement-fakeuse.md` D1d, `research-4-open.md` section 2;
1.2 client decompile and the 1.2 pak):

* The placement ray returns the physics surface index of the terrain hit
  (`FUN_394298c0`, sixth argument `placement+0x28`). `FUN_393a8fa0` step "place skin" maps the
  engine surface type name through `FUN_393a8dd0` (`mat_default` -> 1, `mat_grass` -> 2,
  `mat_grass_tall` -> 3, `mat_mudmat_sand_dry` -> 4) and requires the kind to be one of the
  template's `doodad_place_skins.doodad_place_skin_kind_id` rows, else 276.
* The physics index comes from the cell's `heightmap.dat` header table (32 `u32` world-surface
  ids at offset 32..159, `version >= 24`) at the sample's local surface id (`raw & 0x1F`; 31 =
  terrain hole), resolved through `game/worlds/<world>/surface.xml` and the detail material
  `.mtl` `SurfaceType`. A material with more than two sub-materials uses sub-material 2
  (`CTerrain::UpdateHeightfieldSurfaces`). `cover.ctc` is terrain texture streaming, not this
  chain.
* `doodad_place_skins` has 2,361 rows (755 doodads; kind 1 748, 2 584, 3 585, 4 444). The
  server keys them by `doodad_almighty_id` and keeps every row, like the client vector.

Implemented (`heightmap.rs`, `terrain_surface.rs`, `doodad/data.rs`, `doodad/placement.rs`):
`Hmap::read` now keeps the 32-entry table (the 128-byte read is unchanged); the cell keeps the
per-sample local ids and a `material_at(x, y)` query uses the lower-left 2 m sample, exactly
like `grid_is_hole`; the refusal runs after the existing checks, only with
`Housing.PlacementStrictSurface` on and outside the client skips (templates 1218/1219, group 37,
a house box the player may build on). No usable surface data (no `surface.xml`, no heightmap, a
hole, an id absent from `surface.xml`) allows the placement. A known kind the template does not
allow, a known material whose `SurfaceType` is not one of the four names, and a template without
`doodad_place_skins` rows are refused with `DOODAD_SIM_AREA` (276) before the seed is consumed.

INFERRED / deviations (not stated as client fact):

* A missing or unparsable `.mtl` and an empty `DetailMaterial` use `mat_default` (the engine
  replaces a failed material load with its default material). The real pak has 92 world surfaces
  without a `.mtl` (INFERRED `mat_default`, about 7 k samples).
* PERMISSIVE deviation: `mat_sand_dry` and `mat_mud` are mapped to kind 4. The client name
  `mat_mudmat_sand_dry` is almost certainly a concatenation of mud + sand_dry and is not a
  registered engine surface, so the exact client never produces kind 4; mapping the two real
  names makes the server never stricter than the client. `mat_dirt`, `mat_rock`, ... stay
  unmapped and are refused, as in the client. Exact-client kind-4 behaviour is not offered.
* The sample between two 2 m grid lines is not traced (the physics ray may use another sample at
  a material border): the server uses the lower-left sample. Border spots can therefore be
  decided differently from the client; the choice is permissive in one direction only.
* Placement mode 2 (moving an existing doodad) was not traced; the server applies the rule to
  `CSCreateDoodadPacket` only, like the rest of the seed-builder checks.
* The skip "inside a house box" uses the server's garden square (`house_at_location_in_world`)
  and `allowed_to_interact`, not the client's shrunken box; permissive, never stricter.

Load / memory: the `surface.xml` is parsed lazily on the first strict check of a world (1,012
entries for `main_world`, ~1 MB XML read, ~100 KB retained, 41 ms in a debug build); each
referenced `.mtl` is read lazily and only its kind is cached. The 128-byte cell table is kept for
every loaded cell; the 512 x 512 local-id grid (256 KiB per loaded cell) is kept only with the
flag on. With `PreLoadTerrain` and all 1,205 `main_world` cells loaded that is about 300 MB;
lazy loading touches only the cells a placement check loads (the first cell read cost 41 ms in a
debug build).

Tests: `version_24_keeps_the_32_entry_surface_table_without_moving_the_nodes`,
`surface_types_use_the_root_or_sub_material_two_and_permissive_sand_and_mud`,
`mtl_with_up_to_two_sub_materials_uses_the_root_surface_type`,
`strict_surface_rule_refuses_the_wrong_material_and_keeps_the_item`,
`strict_surface_rule_is_off_by_default`, `strict_surface_rule_honours_the_house_and_group_skips`,
`strict_surface_rule_does_nothing_without_surface_data`, the gated
`real_compact_db_doodads` (2,361 rows) and `real_main_world_surface` (known coordinates).

## Gap audit verdicts (2026-11-07)

Closing batch for the remaining `D:/aaemu-audits/gap-audit-2026-11-02` research of 2026-11-07
(`research/batch-2026-11-07.md`). Client evidence is the 1.2 decompile
`D:/aa-re/client12/out-x2game/x2game.dll.c` ("dll.c") and the packed tables, read-only; server
paths are relative to `rust/crates/aaemu-game/src/`. Verdicts: **client-only** (the client acts
from its own tables / UI, the server has nothing to send), **debug-only**, **no 1.2 trigger**
(nothing in the 1.2 client or data sends it), **capture-blocked** (needs a real-client /
original-server capture) and **done elsewhere**. No behaviour changes come from this batch
except the new, still unsent 0x07e definition.

| item | verdict | evidence |
|---|---|---|
| `skills.start_autoattack` | client-only | Client only: the loader `LoadSkillDescs` (`dll.c:1578270`) maps column 104 to template `+0x158`; `FUN_39390f80` (`dll.c:742551`) special-cases the default melee skill (`DAT_39ac769c` = 2 in `x2game_unpacked_data.dll`) and keeps the combat stance alive for a `+0x158` skill; `FUN_39355ee0` (`dll.c:689991`) refreshes the stance timestamp `+0x5fc0` in 1000 ms windows and sets the actor combat flag `+0x428` for `DAT_39ac7424` = 10000 ms. No packet is sent. Data: 313 of 15,126 skills have the flag; skills 2 / 3 / 4 have `'f'`. Server auto-attack stays default-skill driven (`systems/skills/handlers.rs:471-474`, `systems/skills/skill.rs:2902`, `:2945`). |
| `skills.stop_autoattack` | client-only | Loader column 105 -> `+0x159`; 146 rows. Only the copy constructor reads it; no consumer found by a byte-read search (INFERRED unused). Nothing to port. |
| `skills.auto_fire` | client-only | Loader column 7 -> `+0x19`; readers `FUN_392c6830` (`dll.c:565429`, the key-hold cast loop) and `FUN_39390900` (`dll.c:742241`, suppresses the failure text while repeating). 26 rows. |
| special effects 34-38 (Anim, FxGroup, FxGroupAnim, Projectile, ProjectileAnim) | client-only | The client loads `special_effects` itself (`strings.txt:14202`), so the visuals need no server state; rows 2,319 / 768 / 545 / 1,019 / 168. The server only logs (`systems/skills/effects/special/mod.rs:291`, fall-through `:378-393`; the `PROJECTILE` const is at `:28`). |
| special effect 42 (OpacityControl) | client-only | 26 rows; an NPC spawn/despawn fade the client plays from its own tables. |
| special effect 48 (Combo) | client-only | 29 rows; `value1` = next skill, `value2` = window in ms; the client drives the combo chain. |
| special effect 55 (Detach) | client-only | 15 rows; the C# body only logged and its unmount is commented out (`special/mod.rs:378-379`). |
| special effect 63 (CombatText) | client-only | 20 rows, all `value1..4` 0; nothing carries a text id, so there is nothing to send. |
| special effects 67 (CombatDice) / 70 (NotifyQuest) | done elsewhere | 67 is the plot combat-dice marker (`systems/skills/plots.rs:644`); 70 fires in `plots::finish_item_plot`. |
| special effects 76-78 (ClearProjectile, RetrieveProjectile, AddFxToProjectile) | client-only | 26 / 8 / 7 rows; projectile visuals the client owns. |
| special effect 19 (SpawnBomb) | retired content (not a gap) | 2 rows (skills 10492 "Install Bomb" / 10503 "Summon Bomb", `value1` npc 376 "Bomb"); npc 376 and buffs 2019 / 3177 ("bomb-dropping robot" / "cleaned robot") look like retired test/event content, and 10.x has no SpawnBomb symbol (`round6-trial-effects-controllers.md` 2, 10.x negative). The `SCBombUpdated` 0xaa reader remains defined (`FUN_397c6a10`, `dll.c:1721438`, `bc bombId`); do not schedule. |
| special effects 84 / 88 / 89 (ReportBotArrested, ApplyBotTrial, ArrestBot) | wont (owner 2026-11-08) | 84 is fired by `buff_triggers` 3194 on the buff-4894 "Subdued" timeout, not orphaned (correcting `round6-trial-effects-controllers.md` 1.1); 88/89 are the objection / arrest skills of the same bot flow. The owner decided on 2026-11-08 not to implement bot arrest or bot trial, now or in future (`round6-trial-larceny-2.md` 1). `SCBotSuspectArrested` / `SCSuspectGoingBotTrial` stay typed and unsent (`systems/social/packets/trial.rs:319-320`). |
| special effects 20 / 21 / 57 (PhysicalEnchantWeapon / Armor, RenewEquipment) | done elsewhere | Implemented by `feat/items9` (`systems/skills/effects/special/mod.rs:312-313`, `:338`). |
| `CSQuestStartWith` 0x0db | no 1.2 trigger (log-only correct) | Client writer `FUN_397c5450` (`dll.c:1720446`) + `FUN_397be540` (`dll.c:1715866`, `u8 type` tail). Only two send sites: type 2 level-up, `FUN_39378660` (`dll.c:719962`, call at `:720032`), and type 3 NPC emotion, `FUN_39378930` (`dll.c:720132`, call at `:720204`); types 1 / 4 are never sent. The server already starts level-up quests (`systems/quests/events.rs:970`); the single emotion act 20923 points at missing component 15266 (orphan). Reader and log-only handler: `systems/quests/packets.rs:533-595`. |
| `SCShowQuestArea` 0x1af | debug-only | Reader `FUN_397b7940` (`dll.c:1710613`) = `u32 questType`, `Area{i64,i64,f32}`, `f32 radius`; handler `FUN_391df7b0` (`dll.c:372352`) -> `FUN_393840b0` (`dll.c:731288`) stores `{questType, LOCAL PLAYER position, radius}` and ignores the packet's area; `FUN_393822f0` (`dll.c:729680`) draws each entry as a debug sphere labelled `"stype: %u"` (`dll.c:729703`). Keep defined-only (`systems/quests/ui_packets.rs:15-46`). |
| `SCQuestAcceptConditional` 0x14a | sent (INFERRED trigger) | Reader `FUN_397b5d80` (`dll.c:1709358`) = `u32 sphereId, u32 questContextId`; handler `FUN_391dbed0` (`dll.c:369454`) -> `FUN_39376690` (`dll.c:718056`) raises event 0xec `START_QUEST_CONTEXT_SPHERE`; the native sender `FUN_393762c0` (`dll.c:717811`) validates NPC/doodad reach and the sphere id, then sends `CSStartQuestContext` 0xd5. The 4 `SphereAcceptQuest` spheres (1040, 1453, 1520, 2236) offer the quests linked in `sphere_accept_quest_quests` (1956 / 4281 / 5730; only 5730 exists as a `quest_contexts` row, 1520 has no link); the 485 automatic `QuestActConAcceptSphere` starters are unchanged. See the quest-packet row above and `systems/quests/spheres.rs::tick_area_spheres`. |
| `SCQuestList` 0x065 | client-only (no-op) | Handler `FUN_391dc0d0` (`dll.c:369556`) -> `FUN_39375e40` (`dll.c:717468`) is an empty function. |
| `SCQuestMailSent` 0x150 | no 1.2 trigger | Reader `FUN_397b5f00` (`dll.c:1709428`) = one `u32`; the shipped `quest_mails` table has one test row and no client sender exists. Skip. |
| `SCAchievementReseted` 0x1ff | no 1.2 trigger | Deliberately never sent (see "Achievement producers (live wiring)" above and `systems/social/packets/misc.rs:366-369`). |
| `SCWorldInteractionSkillList` 0x07e | definition done; sender capture-blocked | Reader `FUN_397c2d70` (`dll.c:1718587`) + `FUN_397bef50` (`dll.c:1716216`, vtable `399b36e4`): `bc, bc, {bc, bc, u32, bc, i32 interactCount (at most 10 read), i32 extraInfo, u32[interactCount]}, u8 mouseButton, i32 modifierKeys`. Handler `FUN_391d9080` (`dll.c:367102`) -> `FUN_393b7440` (`dll.c:773715`) copies the 0x40-byte info, resolves the second `bc` through the world-object table (`FUN_39035110`, `dll.c:52819`) as the doodad and starts `skillId[0]` after the `max_interaction_doodad_distance` check (`FUN_39093800`, `dll.c:129660`). Definition and byte test: `systems/npcs/doodad/packets.rs`; not sent (trigger unknown, CAP-INT-1). |
| `SCWorldInteractionCanceled` 0x07f | capture-blocked | Reader `FUN_397c6540` (`dll.c:1721218`) = `bc interactedUnitId, u32 interactedDoodadId`. Defined (`systems/npcs/doodad/packets.rs:110-128`) but never sent. INFERRED trigger: the server ends an active interaction effect; the client also drops the doodad through `SCDoodadRemoved`, so the benefit is unproven. |
| `CSSelectInteractionEx` 0x06b | capture-blocked | Lua `X2Interaction.SelectInteraction` `FUN_3947dac0` (`dll.c:937501`): with an interaction effect object it calls `FUN_393b3120` (`dll.c:770058`), which sends `bc targetId, i32 interactionEx, i32 var1` and closes the window; otherwise the client starts the skill locally and sends nothing special. The original server's reply is UNKNOWN; the server keeps the reader / debug log (`systems/npcs/npc/packets.rs:531-569`). |
| `SCItemTaskNotify` 0x091 | client-only (no-op) | Reader `FUN_397b3810` (`dll.c:1707570`); handler `FUN_391d5370` (`dll.c:364346`) only logs `OnItemTaskNotify` and returns. Definition: `systems/economy/item_notify_packets.rs:121-157`. |
| `SCCraftItemUnlock` 0x0f5 | no server action | Reader `FUN_397b4c60` (`dll.c:1708521`) = one `u8` slot type, one `u8` slot index; handler `FUN_391d9700` (`dll.c:367435`) -> `FUN_397d2200` (`dll.c:1730226`) re-reads the slot as unlocked (a refresh). The server never locks craft materials (`systems/economy/craft.rs:451-453` only checks what the bag can pay), so there is nothing to release. |
| `SCItemDetailUpdated` 0x093 | done elsewhere | Reader `FUN_397b3840` (`dll.c:1707581`); handler `FUN_394e47b0` (`dll.c:1021672`) copies the detail into the slot in place. In-place detail changes already travel in `SCItemTaskSuccess` update tasks. |
| `SCMileageChanged` 0x101 | no source of mileage (capture-blocked if wanted) | Reader `FUN_397c66b0` (`dll.c:1721278`) = `bc unitId, i32 mileage`; handler `FUN_391d9ea0` (`dll.c:367848`) -> `FUN_39357390` (`dll.c:691067`) raises UI event 0xbd, consumed as floating combat text on the mount. `Mate.mileage` is stored (`models/mate.rs:30`, `systems/vehicles/mates.rs:973`) but no code changes it and no compact column feeds it. |
| `SCMineAmount` 0x124 | client-only | Reader `FUN_397af1b0` (`dll.c:1703958`) = `i32 amount`; event `MINE_AMOUNT` (0xd4) is registered but no Lua listens and no native code raises it, and no compact table has a mine-amount column (`round6-items-mail.md` 6). Nothing to send. |
| `SCHouseFarm` 0xc3 | capture-blocked | Reader `FUN_397aece0` (`dll.c:1703709`) = string name, `i32 total`, `i32 harvestable`; the handler shows the `HOUSE_FARM_MSG` chat line. The retail trigger is UNKNOWN and farm bookkeeping is not implemented (`research/housing.md` WP-F1). |
| `SCHouseBuildPayChanged` 0xbf | no 1.2 trigger | Reader `FUN_397b4000` (`dll.c:1707972`) = `u16 tl, i32 moneyAmount`; only the setter `CSChangeHousePay` would send it, and that C->S opcode does not exist in 1.2 (`systems/housing/packets.rs:56-61`). |
| `SCHouseOwnerNameChanged` 0xcb | no 1.2 trigger | Reader `FUN_397b44b0` (`dll.c:1708167`) = `u16 tl, string newName`; 1.2 has no in-world house rename (only the character-select rename), so nothing sends it (`systems/social/packets/misc.rs:248-253`). |
| todos-1#13 leftovers | done (loot grade negligible) | Loot-pack inherited grade: `systems/economy/item_effects.rs:1794-1797` reads the source item's grade (the descriptor `FUN_396e91c0`, `dll.c:1506218`, cannot name the ActAbility). `gain_loot_pack_item_effects.inherit_grade` has 230 true rows, all with `consume_source_item = true` (already implemented); only effects 529-531 (loot packs 8397 / 6988 / 6894) are `inherit_grade = t` with `consume_source_item = f`, so the ActAbility-grade question affects 3 rows and is negligible (`round6-items-mail.md` 3). Skinize on an already skinized item keeps `ITEM_LOOK_CONVERT_AS_NOT_USE_AS_SKIN`: `ui_texts` has no more specific look-conversion text (`systems/economy/bridges.rs:1899-1901`). Expand-expert failure sends `STORE_UNKNOWN_ERROR` (`systems/economy/store.rs:1098`). `SwapAction::DoNothing` is unreachable. |

## Round 6 closes (2026-11-08)

Final doc passes for the round-6 research (`D:/aaemu-audits/gap-audit-2026-11-02/research/round6-*.md`);
the code rows above were added by the lanes that landed them.

* **`indun_zones.restore_item_time` (instances-pvp#10).** The 1.2 client never reads the column
  (no reader in `x2game.dll.c`; `indun-events-2026-11-07.md` 2.1). The best reading — a re-entry
  grace window before another entry item is consumed — is grade D: it comes from 3.0 / 10.0 data,
  and 10.x stores the field without a consumer (`round6-interaction-quest-indun.md` 3). The
  server stays correct to ignore it until capture C5; no default-off guess is added.
* **Bot arrest / bot trial (owner decision, 2026-11-08).** NOT implemented, now or in future.
  The 1.2 data would support most of it (`buff_triggers` 3163 applies buff 4868 "Prisoner" for
  10 h on the 4894 timeout; `buff_skills` 229 / 296 grant arrest / instant judgment; buff 4759
  comes from six cloaks' `items.buff_id` and already works), but the owner declined the feature
  (`round6-trial-larceny-2.md` 1; `round6-plan.md`). Special effects 84/88/89 stay unhandled by
  design; `SCBotSuspectArrested` / `SCSuspectGoingBotTrial` stay typed and unsent.

## Combat, heal and instance-zone fixes (2026-11-08)

Follow-up of the 2026-11-08 bot combat run (`D:/aaemu-audits/bot-combat-2026-11-08`, main
`69074b51e`; line numbers are of the uncompressed `rs-game.log`). Data reference: the 1.2
`compact.sqlite3`, read-only.

| item | verdict | evidence and change |
|---|---|---|
| "melee auto-attack (skill 2) never lands" (1,088 run events, none a hit) | **correct as is — the CSV is an analysis artifact** | In `damage_with_aoe` an avoided attack is broadcast on its own (`effects/combat.rs:256` miss path, before the batch), while a landed hit is added to the skill's `CompressedGamePackets` (`:672`). The audit parsed only the individually traced `SCUnitDamagedPacket` lines (16,137, exactly the CSV rows), so for skill 2 it saw only the misses. The same run's compressed batches carry **19,831 skill-2 MeleeHit packets** (5,046 caster/target pairs; e.g. NPC 45428 hit Bosu 45374 ten times, damage 7-134, lines 96722-133314). The observed avoidance mix is the conditional mix of the misses (NPC attackers have base accuracy 100, so only the defender's dodge/block/parry appear; bots without a shield show dodge+parry only). No code change; a live check (`aaemu-testclient combat` vs the lane stack) also lands player auto-attacks. |
| `GetZoneId: Coordinates out of bounds for WorldId 2` flood (8,518) | fixed (deviation) | Not the main-world template: world template **2 is `instance_sal_temple`** (2x2 cells = 2048 m, zone 240); the run's instance member walked to instance-local x=2049.3/y=1003.5, one metre past the east edge. The movement path's open-position bookkeeping (`world/zone_access.rs`, `world_transform`) looks the zone up for every move and the C# `GetZoneId` logs that position. **Deviation:** `WorldInstance::zone_id_at` (`world/mod.rs:220`) uses the authored sector inside the cell grid (including sectors the XML never assigned, key 0) and, only outside the grid, returns a non-main instance's only zone; `WorldTemplate::zone_id_at` (`data/world_templates.rs:765`) keeps the C# rule (0 and the error) for the main world and multi-zone worlds. Every instance-capable caller uses it: `region_add_object`, `resurrect_at`, the closed-zone checks/`safe_destination`, the AI geodata zone key, doodad phase/placement/siege, housing placement/load/dominion, the nation/dominion commands, and the parallel spawn-manager / social / subzone / public-farm lookups. Tests: `world::world_manager_tests::instance_zone_lookup_covers_positions_past_the_cell_grid`, `world::session::zone_access_tests::instance_movement_past_the_edge_keeps_its_zone_without_an_error` and `data::world_templates::tests::instance_zone_fallback_keeps_unassigned_ingrid_sectors_at_zero` (each fails without its part). |
| buff 399 "생명력 흡수" ticks heal -1 (17 ticks) | fixed | Buff 399 has no `buff_effects` row, but its `buff_tick_effects` use effect 1637 -> `HealEffect` **61** (`use_level_heal`, `level_md` 2, level range 0/0). The caster is an NPC whose `LevelDps` is **-8**: `systems/npcs/npc/stats.rs:167-168` always evaluates the NPC formula with `ab_level = 0` (mirroring the C# `Npc.cs`), and formula id 29 `(floor((ab_level+2)/3)*24-8) * npc_template*npc_kind*npc_grade` therefore gives -8 for every NPC whatever its level (with the same unit variables it would give +400 at level 50). **This is a mirrored C# bug under investigation**, not 1.2 intent: it also makes level-scaled NPC damage effects (the same `LevelDps` feeds `level_range`) and mana effects wrong; the heal/mana guards here are safety nets only, and a separate research task decides the correct `ab_level`. Because the value is negative, the computed heal is negative and the client got `SCUnitHealedPacket` -1 while the HP was *reduced* by 1. `HealEffect` now returns before the packet when the final value is <= 0 (debug log), leaving HP and mana untouched (`effects/combat.rs:891`). Tests: `negative_level_heal_sends_no_packet_and_changes_no_hp` (negative and positive `LevelDps`) and the NPC-path `npc_caster_with_negative_level_dps_gets_no_heal_and_no_restore`. |
| `RestoreManaEffect 68: min > max` (32 errors) | fixed (data quirk handled) | RestoreManaEffect 68 (`level_md` 3, `level_va_start/end` 10) is used by skill 18501. The columns are **not swapped** and no clamp applies: `min` uses the level term `1.0 * level_dps` and `max` the authored `1.1 * level_dps`; with the NPC `LevelDps` -8 (the mirrored C# `ab_level = 0` bug above) both are negative and the 1.1 factor makes "max" smaller than "min" (NPC 21787 cast, `rs-game.log:2145511`). A restore cannot be negative: an authored inversion (`fixed_min > fixed_max` or `level_va_start > level_va_end`) still logs the `min > max` error, while the negative-LevelDps caster is a no-op with a debug log when `max <= 0`; a partially negative range has its minimum clamped to 0 (`effects/combat.rs:1082-1104`). Player casters keep their positive range. Tests: `negative_level_restore_mana_is_a_no_op_without_an_error`, `authored_inverted_restore_range_still_reports_the_error`, and the NPC-path `npc_caster_with_negative_level_dps_gets_no_heal_and_no_restore`. |
| CC durations at 70 % (buffs 138, 763, 1049, 1064, 2795, 2843, 2846) and buff 449 at 130 % | **correct as is — 1.2 data rules** | Matches `buff_tolerances`/`buff_tolerance_steps` plus the target's `buff_modifiers` `InDuration` (attribute 5) rows. Tolerance 4 (tag 6) steps 0/20/40 % over 25 s: buff 2795 on 45696 went 2000 -> 1600 -> 1200 ms (`rs-game.log:1592372`, `:1663193`, `:1738548`). Buff 714 (a gear passive, `tag 6/107/105/385/160/13` InDuration -30 %) is on the run's bots (e.g. owner 45575 at `:1486888`), giving 138 2000 -> 1399, 763 10000 -> 6999 and 2846 2100 -> 1469 ms. Buff 449 is tag 6 too and the +30 % `InDuration` buffs (101/2835/4712) were active on owner 45955 (`[96, 101, 114]` at `:8531140`), giving 1500 -> 1950 ms; owner 45243 without one kept 1500 (`:1471849`). No code change. |

## Tester skill fixes (Rust only, 2026-11-09)

- **Cancelled plot-only casts keep no cooldown.** A `plot_only` skill with a plot commits its GCD and skill/tag cooldown at acceptance (so the client display and spam protection stay as before). The caster's cooldown state from just before the commit is saved on the `SkillRun` (`plot_cooldown_snapshot`); when the plot is cancelled while it is in its casting phase and no damage or other non-special, non-buff effect has landed (`PlotStateInner::effect_landed`), `skill::refund_cancelled_plot_cooldown` restores it (the shared tag cooldown returns to its previous value, not removed outright) and sends `SCSkillCooldownResetPacket` for the skill and for the GCD. A channel cancelled after effects landed keeps its cooldown. The C# never refunded. Test: `plot_cast_tests::plot_only_cancelled_during_casting_restores_cooldown_and_gcd`.
- **Area skills skip dead units.** `skill::apply_effects` now applies the template's `target_alive` / `target_dead` flags to the radius-area candidates (via `geometry::alive_dead_allows`, shared with the plot target filter) before the `target_area_count` trim, so corpses neither take damage (Hell Spear, Summon Crows, Thwart) nor use up a slot. A template with neither flag set (57 radius skills) is left unfiltered, since the strict rule would exclude everyone. Test: `modifier_flow_tests::area_skill_skips_corpses_before_the_target_count_trim`.
- **Area skills ignore height (conservative limit).** `geometry::around_radius` compares only x/y, so Hell Spear (radius 6) hit targets 10 m above the caster. The templates' `valid_height` / `target_valid_height` are loaded but their semantics could not be established from the data alone: 1,734 of 1,977 radius skills have `target_valid_height` 0 (Thwart 16486 included), 243 set it (Hell Spear 1.0, 2.0 on 320 skills overall, 5.0 on 113), `valid_height_edge_to_edge` is 't' on almost every row, and no client Lua or doc describes it. A strict per-skill vertical slab (1 m for Hell Spear) would also miss targets on ordinary slopes. **Open question:** the exact retail meaning (likely an edge-to-edge vertical reach around the area centre). **Implemented:** `apply_effects` drops radius-area candidates whose |dz| from the area centre exceeds `max(radius, target_valid_height, 3 m)` (`geometry::area_vertical_limit`); a ground-target area uses the target point's z, and the explicitly selected primary target is never dropped. `around_radius` itself (aggro, auras, doodads) is unchanged. Test: `modifier_flow_tests::area_skill_ignores_targets_far_above_the_centre`.
- **Bag-full errors.** `DoodadFuncLootItem` answered a failed acquisition with `BAG_INVALID_ITEM` even when the bag was merely full; it now sends `BAG_FULL` (25) when `bag_space_left_for_item` is below the rolled count and keeps `BAG_INVALID_ITEM` otherwise. A store purchase (`CSBuyItems`) whose goods do not fit was silent and now sends `BAG_FULL` (a vanished buy-back item stays silent). The `GainItem` special effect (C#: log only) sends `BAG_FULL` when a known item template could not be added, in both the economy and the skill-effect implementation. Quest rewards that overflow to mail are unchanged. Tests: `loot_item_failure_sends_error`, `loot_item_failure_with_room_keeps_the_invalid_item_error`, `shop_catalog_grades_full_bag_rolls_back`, `shop_checkout_capacity_failure_rolls_back_all_goods_and_buyback`, `gain_item_that_does_not_fit_reports_a_full_bag`.

## Movement guard and teleport acknowledgement (Rust only, 2026-11)

- **Deviation (security).** The C# applied every `CSMoveUnit` position of the player or its mate as sent, and took the position of `CSTeleportEnded` as sent (clearing `DisabledSetPosition` for any report). The Rust server keeps its transform as the baseline:
  - `world::move_guard` holds a token bucket per moved object (player, owned mate, driven land vehicle): horizontal and upward budgets in metres refilled at `World.MoveGuardMaxSpeed` / `MoveGuardMaxUpSpeed` of server time, capped at `speed * MoveGuardBurstSeconds`; falling is free. A move that does not fit (or is non-finite, or claims to stand on an object more than 150 m away) is dropped before any side effect (no buff removal, attachment, broadcast or visibility change); the player gets an `SCTeleportUnit` back to the server position, a mate move dismounts the owner first, a vehicle move dismounts the driver. One warning plus one `audit_char_sus` row (`Cheat`) per unit per 10 s. Moves while standing on the same object are measured in that object's frame (a moving deck is not the player's speed). Characters at or above `MoveGuardExemptAccessLevel` (default 50) and `MoveGuardEnabled=false` keep the old behaviour. Optional kick after `MoveGuardKickAfterViolations` refusals in `MoveGuardViolationWindowSeconds`.
  - `Character::pending_teleport` records the destination of every `SCTeleportUnit` the server sends (`world::session::send_teleport_unit`). `CSTeleportEnded` is ignored when no teleport is pending; otherwise the server lands on the recorded destination unless the reported x/y/z are finite and within 10 m horizontally / 30 m vertically of it (the client knows the ground height). A pending instance load is still finished by `instance_loaded`; a lock without a recorded destination keeps the server position and is logged.
- **Not covered.** Fall damage authority, stun / root gating, world bounds and ship physics. Elevators and gimmicks the player stands on are measured in their own frame; server-driven displacements rewrite the server transform and so need no exemption.

## Game connection gate (Rust only, 2026-11)

- **Deviation (security).** The C# dispatched every registered packet whatever the connection state, kept a connection that never authenticated for ever and accepted any `X2EnterWorld` cookie of any age any number of times.
  - `GameServer::handle_frame` dispatches only `X2EnterWorld` while `conn.account_id() == 0`; any other packet closes the connection (debug log, no handler runs). The headless bots and flow tests send nothing else first.
  - `X2EnterWorld` on a connection that already entered is refused (the connection closes; the first account stays registered).
  - `EnterWorldManager::login` refuses an unconsumed cookie older than `PENDING_TOKEN_LIFETIME` (60 s), like an unknown one. `EnterWorldThrottle` blocks an IP that failed the cookie check (`InvalidToken`, `AccountMismatch`) `Network.EnterWorldFailureLimit` times within `EnterWorldFailureWindowSeconds`; while blocked (`EnterWorldBlockSeconds`) its `X2EnterWorld` is refused without looking at the cookie.
  - `net/listener.rs`: a connection that has not completed `X2EnterWorld` within `Network.PreAuthTimeoutSeconds` is closed; more than `MaxUnauthenticatedPerIp` such connections per IP are dropped at accept; a token bucket (`MaxFramesPerSecond`, `FrameBurstSeconds` of burst) closes a connection that floods frames. `0` switches each of these off. The limits live in the network path only; `handle_frame` (used directly by in-process tests) applies none of the rate or timeout rules.
  - Unknown or unreadable packets are logged at `error` for the first ten per 60 s window of a connection (`BadPacketLog`; one `warn` when a window overflows) and at `debug` for the rest of that window; they never close the connection (the 1.2 client sends opcodes that are not ported yet).
  - A replayed `CSSpawnCharacter` / `CSNotifyInGame` is ignored (`GameConnection::spawn_handled` / `in_game_notified`, reset by `select_character`): the C# subscribed to the time manager again and re-ran the join hooks on every replay.

## Stream server hardening (Rust only, 2026-11)

- **Deviation (security).** The C# stream server used the game connection id (small, sequential) as the join token, removed a token when `CTJoin` named it with the wrong account, served every registered packet to a connection that had not joined, and let upload state grow without bound.
  - `X2EnterWorldResponsePacket.token` is a random nonzero u32 ticket per game connection (`StreamManager::add_token`, same source as `GameServer::new_reconnect_token`, unique among live tickets); `CTJoin` resolves it back to the game connection. A wrong account or an unknown ticket is rejected (response 1) and removes nothing. The ticket is dropped on the game disconnect (`remove_token`).
  - Before `CTJoin` is accepted only `CTJoin` is dispatched; any other packet, registered or not, closes the stream connection.
  - `CTStartUploadEmblemStream` refuses a declared `data_size` above `MAX_UCC_UPLOAD_BYTES` (1 MiB; no 1.2 client limit is documented in the repository, a crest is a small image). A part that carries an upload past its declared size, or past `MAX_UCC_UPLOAD_PARTS` (4096) parts, drops the upload and closes the connection. A closing stream connection drops its upload queue, part buffer and download queue entries (`UccManager::drop_connection`).
  - A stream connection keeps at most `MAX_LIVE_CELL_REQUESTS` (256) cell requests; the oldest is dropped for a new one (the client abandons requests silently, `CTCancelCell` is a no-op).

## World cookie (Rust only, 2026-11)

- **Deviation (security).** The C# world cookie was the login server's connection id (lowest free id: small and sequential), and the pending entry of `EnterWorldManager._accounts` was keyed by it, so (account id, cookie) pairs of a player between `ACWorldCookie` and `X2EnterWorld` were guessable.
  - `EnterWorldManager::add_account` generates a random nonzero u32 cookie (unique among the pending ones, same source as `GameServer::new_reconnect_token`), keys the pending entry by it and keeps the login connection id inside the entry; a reused connection id still replaces its stale (unconsumed) entry, and the occupancy counting is unchanged. `login(account_id, cookie)` works on the random key; a login connection id is refused like any unknown cookie.
  - `GLPlayerEnterPacket` gains a trailing `u32 cookie` after the `online`/`sequence` extension (0 on a refusal). The login server relays it in `ACWorldCookie` (client wire format unchanged). A game server that sends none (absent or 0) makes the login server fall back to the connection id and log one warning per game server registration.
  - **Login and game servers must be upgraded together**: a new game server with an old login server sends a cookie the old login server ignores (it still sends the connection id, which the new game server then refuses); an old game server with a new login server works but keeps the guessable cookie.

## Web API request bodies (Rust only, 2026-11)

- **Deviation (security).** The C# session reads the request body before routing and authorization. The Rust adapter (`webapi::dispatch`) now routes and authorizes from the request line and headers first (`preflight`): an unauthorized request is answered 401 and an unknown route 404 without the body being read, and the public `GET /status` needs no body. An authorized request's body is capped at `MAX_BODY_BYTES` (1 MiB; the largest legitimate request is a mail with attachment items or an auction/command JSON document, a few KiB) and answered 413 beyond it (the previous 64 MiB cap silently dropped an oversize body).

## Skill and passive buff learning (Rust only, 2026-11)

- **Deviation (security).** The C# `CharacterSkills.AddSkill` / `AddBuff` skipped the ability gate for `ability_id == 0` and the ability level is then the character level, so `CSLearnSkill` learned any of the ~14,950 ability-less NPC / boss / GM / item / internal skills (many apply stat-modifier buffs such as skill 23071 -> buff 6541, +50,000,000 damage) and `CSStartSkill` then cast them.
  - `progress::learn_skill` accepts only a `need_learn` skill of one of the character's active abilities (`is_learnable_class_skill`); `learn_buff` only a passive buff of an active ability (`ability_id != 0`). Default / racial skills are server-granted (action slots + `is_default_skill` cast path), never learned by packet.
  - `CSStartSkill`: the learned-list branch runs only for class skills (`ability_id != 0`); an ability-less skill in the list takes the `is_client_sourced_skill` check like an unlearned one. `is_variant_of_skill` never matches an ability-less skill.
  - Load (`db_character::load_character_details`): stored skill rows that are neither a learnable class skill of an active ability nor a default skill, and stored passive buffs with `ability_id == 0`, are dropped, one `warn` per character lists the ids, and the rows are deleted (`save` only upserts). Active buffs (`character_active_buffs`) are not touched.

## Client-trusted ids in creation, family, markers (Rust only, 2026-11)

- **Deviation (security).** `CSCreateCharacter` / `CSEditCharacter` (`managers/character.rs`): the client's `level` is ignored (new characters are level 1), `ability2` / `ability3` are always `ability::NONE`, and a `body_items` entry must be an `item_body_parts` row of the chosen race / gender model for that slot (`slot_type_id - 23`) that is not `npc_only`; anything else falls back to the template default with a `warn`. `beauty_shop_only` parts stay accepted (the creation UI's own filter is unknown).
- **Deviation (security).** `CSFamilyReplyInvitation`: the C# trusted `invitorId` and recorded no invitation, so any client could join (or create a family with) any online character. `FamilyManager` now keeps one pending invitation per invited character (inviter id, 60 s) recorded by `invite_to_family`; a reply must match and consumes it (accept or decline). An inviter that already has a family must be its steward (role 1, the rule of kick / title); the reply re-checks it. `add_family_member` checks the existing membership before pushing the member.
- **Deviation (security).** `SetOverHeadMarker` requires the sender to be a member of the named team (a party let any sender through). `CSSetForceAttack` returns early when the flag already has the requested value.

## Client-chosen shipyard, hang and mount targets (Rust only, 2026-11)

- **Deviation (security).** `ShipyardManager.Create` used the client's x / y / z / rotation unchanged: it now refuses (`SLAVE_SPAWN_ERROR_INVALID_AREA`, before the design item is looked at) a non-finite value or a position beyond `caster_in_range(.., 20 m)` (50 m 3D, the housing builder's rule). No water / ground / collision check (a shipyard sits on the shore; the client's preview decides the exact spot).
- **Deviation (security).** `CSHang` ignores a target farther than 30 m (`harpoon::HANG_REACH`) from the character; before, any object id attached the character to it and a doodad target ran `use_doodad` from anywhere. `MountMate` refuses a mate beyond 6 m (`MOUNT_REACH`, a little above the 4 m slave bind reach) and a character that already sits on a mate or a slave.

## Client-trusted inventory and currency requests (Rust only, 2026-11)

- **Deviation (security).** `SplitOrMoveItemEx` merged two stacks of one template whatever their grade or flags (the merge keeps the target's values, so a soulbound or low-grade stack unbound / upgraded itself); it now merges only when grade, flags and expiry / unpack / unsecure times match (`stacks_merge`), otherwise the swap runs. It also refused no slot beyond the container size (`to_slot = 200` pushed items past a bag, bank or coffer): a non-equipment `from_slot` / `to_slot` outside `size` is refused (`slot_in_range`; unbounded containers excepted).
- **Deviation (security).** `CSSwapItems` / `CSSplitBagItem` with the BANK container as source or destination need the same banker NPC in range as `CSDepositMoney` / `CSWithdrawMoney` (`banker_in_range`); 1.2 has no item or doodad that opens the warehouse. `CSExpandExpert` **charges** `life_point` vocation badges (the C# added them, a mirrored bug; the gain bonuses of `ChangeGamePoints` no longer scale the cost). `CSConvertItemLook` needs the full `required_item_count` before consuming anything. `Craft` clamps the client's batch `count` to `1..=1000` (`MAX_CRAFT_BATCH`; the 1.2 client's own maximum is unknown, a non-positive count was a single craft before). `CSSellItems` takes the economy operation lock before it snapshots the stacks, so a concurrent trade finish cannot leave the seller paid for the old count.

## Loot rights and doodad loot reach (Rust only, 2026-11)

- **Deviation (security).** `CSLootOpenBag` on a func-driven loot / recover doodad (`try_handle_func_driven_loot`: `RecoverItem`, loot packs, cutting) ran from anywhere in the instance; it now needs the doodad within `in_func_reach(.., 0)` (the 5 m doodad interaction distance plus the doodad range allowance) and otherwise does nothing. NPC corpse loot honours `EligiblePlayers` before `make_loot_public` has run: the C# took no rights into account when taking (a tagger-only corpse is `FREE_FOR_ALL`, so anyone within 5 m took it) and `OpenBag` showed the contents to anyone. `lacks_loot_rights` makes `OpenBag` and `TryTakeLoot` answer `NO_PERMISSION_TO_LOOT` to a player that is not in `EligiblePlayers` (the tagger, the tagging team's members in range at the kill, or the untagged killer); the holder of a claimed item (roll winner, `highest_roller`) and everyone after the 180 s public timer (rule `PUBLIC`) are unaffected, as is a container with no eligible players and doodad loot.

## Quest requests name an NPC or doodad in reach (Rust only, 2026-11)

- **Deviation (security).** `CSStartQuestContext`, `CSCompleteQuestContext` and `CSQuestTalkMade` only needed the named NPC to exist in the world, so a client could start, turn in or "talk" any quest NPC from anywhere in the instance. `events.rs` (`npc_out_of_quest_reach`) now requires the NPC within the `CSStartInteraction` reach (`NPC_INTERACTION_REACH_METERS` + `Interaction.NpcReachToleranceMeters`, same `NpcReachCheck` switch) plus `QUEST_NPC_REACH_SLACK_METERS` = 10 m, since an NPC may walk while its dialog is open; a named doodad (start from a doodad, doodad turn-in) must be within `in_func_reach(.., 0)`. A refused start is silent, a refused turn-in answers `SCQuestContextFailedPacket`, a refused talk is dropped. Server-started reports and `add_quest_from_npc` (engage-combat quests) are not checked. **Left alone:** a client start naming no NPC, doodad or sphere still reaches `starter_accepts` (`ConAutoComplete`, `ConAcceptComponent` and act-less starters accept it): 401 quests have a START component with no act and 115 only `ConAcceptComponent`, and nothing proves the 1.2 client never sends those without an object. A sphere start whose sphere has no geometry keeps skipping the position check: 59 of the 476 `quest_act_con_accept_spheres` spheres (17 quests) have no shipped `sphere_area_fill.json` / `quest_sign_sphere_fill.json` row (the client `quest_sign_sphere.g` areas may cover some).
