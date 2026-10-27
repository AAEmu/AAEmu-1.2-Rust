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
| `Core/Network/Stream` + `Packets/C2S`, `Packets/S2C` (stream server) | `stream/` | done | StreamNetwork listener on `StreamNetwork.Host:Port`, `[u16 len][u16 op][body]` framing, connection table, all 17 CT and 15 TC packets (byte-exact). World lookups go through `stream::world::StreamWorld`, implemented by `stream_world::GameStreamWorld` (names, items/UCC ids, ActiveChar, doodads of a cell via `WorldInstance::objects_in_cell`). Truncated packet bodies close the connection (C# reads zeros). |
| `Core/Packets/C2G` | `packets/c2g.rs`, `packets/c2g_world.rs` | partial | X2EnterWorld, CSLeaveWorld, CSCancelLeaveWorld, CSListCharacter, CSCreateCharacter, CSDeleteCharacter, CSCancelCharacterDelete, CSSelectCharacter, CSSpawnCharacter, CSNotifyInGame, CSNotifyInGameCompleted, CSMoveUnit, CSSendChatMessage. Others todo; full opcode table in `opcodes.rs`. |
| `Core/Packets/G2C` | `packets/g2c.rs`, `packets/world.rs` | partial | Lobby packets + every packet sent by the phase-2 paths (≈45, byte-exact tests). `SCLoginCharInfoHouse`, `SCMyHouse`, item tasks, buff packets todo. |
| `Core/Packets/Proxy` | `packets/proxy.rs` | done | All registered proxy packets (most are inert in C# too). |
| `Core/Packets/CompressedGamePackets`, `Debug` | – | todo | |
| `Core/Managers/World/EnterWorldManager` | `managers/enter_world.rs`, `world/session.rs` | done | AddAccount, Login, Leave (all targets), LeaveWorldTask (mates/slaves/coffers/quest timers are no-ops: not ported). |
| `Core/Managers/AccountManager` | `managers/account.rs`, `db.rs`, `systems/services/accounts.rs` | done | Add (login time + timed rewards), Remove/Contains/Count, GetAccountDetails, UpdateLoginTime, AddCredits/RemoveCredits/AddLoyalty/UpdateLabor/UpdateTickTimes/UpdateDivineClock/GetDivineClock under the per-account lock, RemoveDeadConnections (30 s tick). See `systems::services`. |
| `Core/Managers/FeaturesManager`, `Models/Game/Features` | `data/features.rs` | done | |
| `Core/Managers/ExperienceManager` (+ `SqliteExperienceLevelTemplateLoader`) | `data/experience.rs` | partial | Loader validation, level caps, GetExpForLevel, GetLevelFromExp (both overloads), GetSkillPointsForLevel. Mate/exp-loss helpers todo. |
| `Core/Managers/Id/*` (`IdManager`, `BitSet`, `PrimeFinder`) | `managers/id.rs` | done | Every C# id manager (incl. `distinct` for FamilyIdManager) with the C# allocation algorithm and DB seeding; `UccIdManager` in `stream/ucc.rs`. The static `SkillTlIdManager` is the skills subsystem's. |
| `Core/Managers/NameManager` | `managers/name.rs` | done | Load, ValidateCharacterName (regex from `CharacterNameRegex`), Add/Remove, NormalizeName. |
| `Core/Managers/ItemManager` (templates + user items) | `data/items.rs`, `managers/item.rs`, `models/item.rs`, `models/container.rs` | done | See `systems::economy`. |
| `Models/Game/Items/Containers` (ItemContainer, EquipmentContainer, MateEquipmentContainer, CofferContainer, LootingContainer) | `models/container.rs`, `systems/economy/looting.rs` | done | See `systems::economy`. |
| `Core/Managers/UnitManagers/CharacterManager` | `data/character_templates.rs`, `managers/character.rs`, `db_character.rs` | partial | Load (templates, body parts, buffs, supplies, equip packs, appellations, actabilities, expert limits, CharTemplates.json), Create, SetDeleteCharacter, SetRestoreCharacter, CheckForDeletedCharacters(+Deletion) with the delete task, IsCharacterPendingDeletion, LoadCharacters. Bag expands, beauty salon, rolls, online tracking todo. |
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
| `Core/Managers/ExpeditionManager`, `Models/Game/Expeditions` | `managers/expedition.rs` | partial | Load, GetExpedition, SendExpeditions, SendExpeditionInfo, member refresh / login / logout status packets. Guild management todo. |
| `Core/Managers/FriendMananger` | `managers/friend.rs`, `db_character.rs` | partial | Load, SendStatusChange, GetFriendInfo, FormatFriend. Add/remove todo. |
| `Core/Managers/PortalManager` | `data/world_data.rs` | partial | recalls.json + district_return_points (GetRecallBySubZoneId, GetDistrictReturnPoint). Teleports, world gates, respawns todo. |
| `Core/Managers/ChatManager`, `Models/Game/Chat` | `managers/chat.rs`, `world/session.rs` | partial | Faction / nation / zone / guild / family channels, join/leave/send; CSSendChatMessage for every chat type (party/raid -> "not in party/raid" errors as there are no teams). Trial channels, commands todo. |
| `Core/Managers/TimeManager` (+ `TimeOfDayObserver`) | `managers/time.rs` | done | `Get` / `Set` / `Stop` added (phase 4). Storm-ship time override (`ResolveClientTimeOfDayHours`) per observer (`subscribe_with`). |
| `Core/Managers/SaveManager` | `managers/save.rs` | partial | Periodic + final save of items and online characters; the subsystems' rows through `systems::save_stage` (`BeforeItems`: houses, mails; `AfterItems`: auction, crimes; `AfterCharacters`: slaves). |
| `Core/Managers/World/StreamManager` | `managers/stream.rs`, `stream/cells.rs` | done | AddToken/RemoveToken/Login (wrong account removes the token, as C#), RequestCell/ContinueCell incl. the C# `next - 1` copy off-by-one. |
| `Core/Managers/Stream/UccManager`, `Models/Stream`, `Core/Managers/Id/UccIdManager` | `stream/ucc.rs`, `stream/models.rs` | partial | Load/Save (`uccs` table, data in the MEDIUMBLOB as the default C# build), upload (simple + custom), download, checks, id allocation. `CreateStamp`/`ApplyStamp` (item-side, used by skill effects) todo. `ConfirmDefaultUcc` charges the money and creates the Crest Ink before the save, as in C#. |
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
| `Models/Game/Units/Buffs` behaviour, `Skills/Buff`, `Skills/Effects/{Buff,Damage,Heal,Dispel,RestoreMana,ManaBurn,Aggro,FlyingStateChange,SkillController,...}`, `CombatBuffs`, `BuffTriggers` | `systems/skills/{buffs,effects/*}.rs` | done | Start / dispel / tick tasks, tolerance, stack rules, charges, `TriggerRemoveOn`, tag removal, absorption, triggers, area ticks. Explicit dispel emits Dispelled without Timeout; natural expiry retains Timeout. This corrects the mirrored C# removal path, which invoked the FISH escape trigger when catch plot 792 dispelled FISH before lethal damage. Queued CREATED handling and existing removal packet order are retained. Refreshing permanent non-ticking buffs invalidates old timers without treating GetTimeLeft=-1 as expiry; finite expiration and permanent tick cadence remain scheduled. This corrects the mirrored C# refresh timer path. Special effects routed through `ExtHooks::route_special` where they belong to another subsystem. |
| `Units/Unit` combat (`ReduceCurrentHp`, `DoDie`, aggro / tagging, `SetInBattle`, criminal / force attack, invisibility, `DoFallDamage`), `BaseUnit.CanAttack` / relations, regen, `UpdateGearBonuses`, all `Character` stat getters | `systems/skills/{combat,relation,stats,regen}.rs` | done | MaxHp / MaxMp etc. with buffs, passives and gear (`GearStats` hook). NPC death side effects through `NpcSkillHooks`. |
| Learning / progression (`LearnSkill`, `LearnBuff`, `ResetSkills`, `SwapAbility`, `AddExp`, labor, game points) | `systems/skills/progress.rs` | done | Labor accrual uses zero-based actability steps into ordered `expert_limits`, matching inventory / regrade; step 4 caps at 50,000 (row id 5), avoiding the former row-id lookup that reduced it to 40,000. Capped gains and packet deltas have regression coverage. |
| `Skills/Effects/SpecialEffects/*` (141 named `SpecialType` actions), `BubbleEffect`, `ImpulseEffect`, `PhysicalExplosionEffect` | `systems/skills/effects/{special/*,unit_fx}.rs`, `packets_special.rs` | partial | Ported actions and subsystem hooks are present, but historical logging placeholders do not establish gameplay completeness. Buff stealing, component-selective aggro copy/reset, target loss, NPC despawn, ongoing-buff cancellation and pet recovery now execute. ExplodeBuff, MoveToGround and RedeemBuff now execute (see below). Client visuals use the existing skill/plot notifications. 13 extra S->C packets, byte-exact tests. |
| `Skills/SkillControllers/{Floating,Wandering,Leap,Dash,Rotate}` | `systems/skills/controllers.rs` | partial | Rotate uses native constructor/tick timing (`0x3924E1D0` / `0x392315C0`) with 1.2 controller7678 regression coverage. Generic Anchor and Flowgraph remain unverified; native 10.x Anchor parameter kinds1001..1039 differ from the 1.2 data kinds1..3. Harpoon ropes have a separate vehicles implementation. |
| Movement hooks (`CSMoveUnit.RemoveEffects`, `Character.SetPosition` Move removal, fall damage) | `systems/skills/mod.rs` (`on_move_request`, `on_moved`) | done | Called from `world::session::move_unit`. |
| `Skill.EndChanneling` target buff (`ChannelingTargetBuffId`) | `systems/skills/{skill,buffs}.rs` (`end_channeling`, `finish_effect_by_skill`) | deviation | **Deviation:** the C# `Buffs.RemoveEffect(templateId, skillId)` raised the buff's `OnTimeout` triggers on every removal; the Rust removal skips them for explicit removals (dispels, a stopped channel; see the FISH escape in `buff_removal_tests.rs`). Quest item grants hang on that trigger (GainItem on buff 3896 of skill 18449 'Extract Earth Crystal', 4 s channel under a 6 s buff, and the other channeling-target-buff quest skills of the item triage), so a channel that **runs to its end** now fires the target buff's Timeout triggers before removing it. A stopped / cancelled channel fires nothing (the C# granted the item then too). The caster-side `ChannelingBuffId` and buffs applied by skill effects (buff 1963 of 'Capture Crayfish') are unchanged: the latter expire on their own timer and fire Timeout already. |

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

#### Special effects added from data (2026-10-20)

These `SpecialType` bodies were TODO stubs that only logged in the C# (`git show a2ab80abd^:AAEmu.Game/Models/Game/Skills/Effects/SpecialEffects/...`); the behaviour below is derived from the 1.2 data because value1..4 are 0.

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `SpecialEffects/GenderTransfer` (special 107) | `systems/skills/effects/special/misc.rs` (`gender_transfer`) | added | Data: skill 23469 `성별 전환하기` (beauty-salon NPC interaction, `target_type` any unit, selection target). Flips the affected player (`target` if a player, else the caster) with `new = 3 - old` (1 male <-> 2 female), refuses when an equipped item template has `char_gender` set to a third value with `GENDER_TRANSFER_AS_GENDER_EQUIPMENT` (744), rebuilds `unit.model_id` from the race/gender `characters` template and broadcasts `SCCharacterGenderAndModelModifiedPacket` (existing beauty-salon packet) to the player and observers. `Character.Save` persists the new `gender` with the next save. |
| `SpecialEffects/MateMakeGetUp` (special 51) | `systems/vehicles/mates.rs` (`revive_mate`), `systems/skills/effects/special/misc.rs` | added | Data: skill 13719 `일으켜세우기` (revive an injured summoned mate, self-target, friendly); the former C# body only logged. Resolves the target/caster mate or, for a player caster, the active mate; only acts while `hp <= 0`. Reuses the stablehand revive values: `mates.hp/mp = MATE_FULL_POOL`, clears the summon item's `mate_dead` flag with a `RepairPets` item task and broadcasts `SCUnitPointsPacket`; the live mate is restored to `max_hp` / `max_mp`. |
| `AccountAttributeEffect` + `SpecialEffects/AuctionPostAuthority` (special 117) | `systems/economy/account_attributes.rs`, `bridges.rs`, `world/session.rs` | added | Data: `account_attribute_effects` has 3 rows, all `kind_id = 1` (auction posting authority): item 8000027 `경매장 등록권` adds it account-wide (`is_add`, `count` 0, `time` 0, `bind_world` false) and two `is_add = false` rows remove it (VIP buff trigger, removal ticket item 8000058). Special 117 on buffs 7149 / 11034 / 8000012 grants kind 1 permanently. New `account_attributes` table (PK account/kind/world, nullable `end_time`, `world_id` 0 = not world bound), loaded on character select into `server.systems.economy.account_attributes` and persisted on every change. `is_add` upserts, otherwise every grant of the kind is deleted; `time == 0` is permanent, else `now + time` **seconds (unit inferred: all 1.2 values are 0)**. `has_account_attribute(account_id, kind)` is ready for gating; **the auction is deliberately not gated yet** (every character currently gets buff 8000012 from `UNIT_STATE_FORCED_BUFFS`, so a gate would need that decision first). |
| `SpecialEffects/AddCharacterSlot` (special 91) | `systems/economy/character_slots.rs`, `db.rs`, `packets/c2g.rs` | added | Data: item 8000002 `캐릭터 슬롯확장 주문서` -> skill 8000002. Adds one `accounts.extra_character_slots` (`TINYINT UNSIGNED`, saturating at 255) for the caster's account, persists it and sends `SCGetSlotCountPacket` with the new value; `CSListCharacterPacket` sends that count instead of the old hard-coded 0. Found limits: the game server's `CharacterManager.Create` has **no** per-account character-count check, while `aaemu-login` hard-codes `MAX_CHARACTERS_PER_ACCOUNT = 6` (and `AcAuthResponse.slot_count = 6`); the advertised cap is therefore unchanged by this effect (a login-side read of the game DB would be needed). |

### systems::npcs

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `NpcManager.Load` (customs, body parts, templates, equip packs, postures, unit modifiers, initial buffs, merchants / goods) | `npc/data.rs` | done | 13 382 templates on the real DB. Creatures.xml names, `LoadAiParams` not ported. |
| `NpcManager.Create`, `LoadCustom` (.NET `Random(123456789)` parity), `Npc` stats (Str..KillExp from the unit formulas + template bonuses), `InitializeSpawnBuffs`, `AnimActionId` | `npc/manager.rs`, `npc/stats.rs` | done | Every real template creates without error. The AI is created through `server.systems.hooks.ai` (`systems::ai`). |
| `Npc.DoDie` | `npc/death.rs` | done | Kill exp (team eligibility, `PlayerExpModifier`, mate exp, TagShare contributors), quest kill events, loot (`LootManager.GenerateLoot` through the economy), aggro / tagger clean-up, spawner despawn, `Ai.GoToDead()` (`systems::ai`). |
| NPC interaction packets (CSStartInteraction, CSInteractNPC(End), CSSelectInteractionEx; SCNpcInteractionSkillList, SCAiAggro, SCNpcChatMessage, SCUnitModelPostureChanged, SCAggroTargetChanged) | `npc/packets.rs` | done | Byte-exact tests. Merchant / bank / auction UIs are economy's. |
| `NpcGameData` (npc_spawners, npc_spawner_npcs, member index, generated spawners) | `npc/spawner_data.rs` | done | |
| `NpcSpawner`, `NpcSpawnerNpc`, `DoodadSpawner`, `Spawner<T>` | `npc/spawner.rs`, `doodad/spawner.rs` | done | Schedules / time windows / population caches, respawn + despawn timing, exact-identity reconciliation of direct deletes (see deviations). Ordinary populations keep the authored `Count` / `min_population` baseline (default 1) and per-slot respawn deadlines (see deviations). AI home position / follow path / simulation through `server.systems.hooks.ai` (`systems::ai`). |
| `SpawnManager` (one per instance) + `WorldManager.ActiveRegionTick` spawner part | `spawn_manager.rs` | done | main_world: 25 118 NPC entries -> 24 047 ordinary placements / 38 174 spawner clones (1 011 duplicates and 60 event entries filtered, 0 missing templates), 42 649 doodads, 69 transfers, 14 gimmicks, 5 slaves, 0 parse errors. Ordinary spawning is a bounded per-instance 64 m region-index activation pass (`World.NpcPrewarmRegionMargin` / `World.NpcSpawnTickMs` / `World.NpcSpawnsPerTick`) with warm retention and resident-service retention (`World.NpcWarmRetentionSeconds` / `World.NpcResidentServiceNpcs`; see deviations). Persistent doodads from MySQL. Transfers / slaves: `systems::vehicles`; houses and their doodads: `systems::housing`; trial court doodads / NPCs linked after the spawns (`TrialManager.Initialize*`). |
| `DoodadManager.Load` (templates, func groups, funcs, phase funcs, all 121 func template tables) | `doodad/data.rs`, `doodad/templates.rs` | done | 7 124 templates, 13 974 funcs, 24 465 phase funcs (counts match the DB). |
| `Doodad` phase machine (`Use`, `DoFunc`, `DoPhaseFuncs`, `DoChangePhase`, `InitDoodad`, `OnSkillHit`, `FuncGroupId` setter, ToD triggers), 42 phase funcs, func tasks, `DoodadManager` runtime (Create, CreatePlayerDoodad, ChangeDoodadData, coffers), persistence (`doodads` table) | `doodad/{phase,phase_funcs,tasks,manager,db,logic}.rs` | partial | Serialized interactions, per-object counters, generation and handle guards for timers, ToD transitions, ordered MySQL persistence. Authored immediate logic/family pulses, spawn management groups and moving gimmicks are implemented. FlowGraph runs in the client. Logic delay descriptors 2/3 lack timing semantics; spawn groups 73–76 lack authored membership in shipped data. |
| 79 non-phase funcs, 41 world interactions (`execute_interaction` for the skills InteractionEffect), coffers, bond / seats | `doodad/funcs/`, `doodad/interactions.rs`, `doodad/coffer.rs` | done | Items, loot, coffers through the economy `EconomyBridge`; skills, quests, housing through the bridges; instance funcs (`systems::instances`), `BindSlave` (`systems::vehicles`). Sticky parents are not modelled. |
| SCDoodad* packets, doodad C->S packets (CSChangeDoodadData/Phase, CSCreateDoodad, CSSetLogicDoodad, CSSaveDoodadUccString, CSUnbondDoodad, CSCofferInteraction) | `doodad/packets.rs`, `doodad/coffer.rs` | done | Byte-exact tests. |
| `CSCreateDoodadPacket` placement checks | `doodad/manager.rs` (`handle_create_doodad`, `PLACE_RANGE_SANITY`) | deviation | **Deviation:** the C# checked only the source item, public farm (`CanPlace`), house permission and labor. The Rust adds three checks: (1) the item template must be one of `item_ids_from_doodad` of the doodad (an unrelated item could place any doodad), (2) the doodad's `restrict_zone_id` must match the zone at the target, and (3) a placement range check. The range is horizontal (x / y only) with a 30 m sanity bound (`max(skill.max_range, 30)`): the planting skills (10953 sapling / animal, 25536 seeds) have `max_range` 2, but the 1.2 client does not enforce it (live placements arrive 10.5 m and 15.6 m away) and the server position of a walking player lags, so the earlier 3D `max(max_range, 10)` limit refused ordinary planting silently. A refusal now sends `TOO_FAR_AWAY` (154) to the client. `public_farm::can_place` logs the real reason (count limit / doodad not allowed in the farm kind) at warn. |
| Heightmaps (`WorldCell` heightmap.dat, `GetHeight`, `GetReferenceHeight`), NPC reference height in `SCUnitStatePacket` | `heightmap.rs`, `world/unit_state.rs` | done | Lazy per cell (`PreLoadTerrain`); with `GeoDataMode` the navmesh height of `systems::ai::geodata` comes first. Median error 0.098 m vs 4 002 real spawn Z values. Used by the skill controllers / displacements, plots and NPC spawns. |
| `GimmickManager`, `GimmickGameData`, `GimmickSpawner`, movement handlers | `gimmick/` | done | |
| `AreaTriggerManager` (+ aoe_shapes, wi_group_wis), `SubZoneManager`, `SphereQuestManager` (world side) | `area_trigger.rs`, `subzone.rs`, `sphere_quest.rs` | done | Quest effects via `QuestsBridge`. Area membership compares entity identity, so reconnecting with a reused object ID leaves the retained old session and enters the new one; this corrects the mirrored C# ID-only comparison and restores active school auras. Unchanged entities and overlapping aura counts are retained. Sub-zone index scan of the pak is slow in debug (~25 s). |
| `PortalManager` (recall, district return, summoned portals, portal book, teleports) | `portal/` | done | Trial court check (`systems::social`), mates / slaves on instance change (`systems::vehicles`). |
| `SpecialtyManager`, `PublicFarmManager`, `FishSchoolManager`, `GameScheduleManager` (+ NCrontab subset) | `specialty.rs`, `public_farm.rs`, `fish_school.rs`, `schedule.rs` | done | |
| `TimeManager.OnTimeOfDayChange` (NPC postures, doodad ToD triggers) | `doodad/manager.rs` | done | 10 s poller started by the doodad part (TimeManager has no callback). |
| Cross-subsystem behaviour | `hooks.rs`, `bridges.rs` | done | `SkillsBridge` (skills), `EconomyBridge` (economy), `QuestsBridge` (quests) in `server.systems.npcs.hooks`; this subsystem implements `NpcSkillHooks`, `NpcEconomyHooks`, `LootNpcHooks`, `TreasureMapSource` (`bridges.rs`). See "Phase 3b wiring". |

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
| `ItemContainer`, `EquipmentContainer`, `MateEquipmentContainer`, `CofferContainer` | `models/container.rs` | done | `AddOrMoveExistingItem` (new / moved / same container), `RemoveItem`, `ConsumeItem`, `AcquireDefaultItem(Ex)` (timers, charges, crafter), `SpaceLeftForItem`, `ApplyBindRules`, `Wipe`, `ReNumberSlots`, coffer `CanAccept`, `OnEnter/LeaveContainer` events. |
| `Inventory` (SplitOrMoveItem(Ex), 2H weapon handling, backpacks, ExpandSlot, coffers, ConsumeItem, counts), `Character` money / game points / labor / `ItemUse` / `SendErrorMessage`, `ErrorMessageType` | `systems/economy/inventory.rs`, `error_message.rs` | done | `ChangeLabor`'s `AddExp` is a hook (`ProgressionHooks`). |
| Core item C2G (`CSSwapItems`, `CSSplitBagItem`, `CSDestroyItem`, `CSExpandSlots`, `CSDeposit/WithdrawMoney`, `CSSplit/SwapCofferItem(s)`, `CSItem(Un)Secure`, `CSEquipments(Un)Secure`, `CSThisTimeUnpackItem`) | `systems/economy/item_handlers.rs` | done | Deposit / withdraw require a positive amount and a banker NPC (the `banker` template flag) within 5 m of the current interaction object (the C# did neither). |
| `ItemManager` (all templates / tables, user items, `Create`, timers, `UnwrapItem`, auction search helpers, loot convert fish), `ItemGameData`, `ItemConversionGameData`, `FishDetailsGameData`, item model stats (durability, repair cost, attributes, DPS, armor), procs | `data/items.rs`, `managers/item.rs`, `models/item.rs`, `systems/economy/{items_data,item_timers}.rs` | done | `ItemManager` lists are leaf locks (snapshot before locking containers). |
| `LootGameData`, `Models/Game/Items/Loots/**`, `LootingContainer`, loot packets | `systems/economy/{loot_data,loot,looting,loot_packets}.rs` | done | NPC death / despawn / team rules through `LootNpcHooks`, `LootTeamHooks`, `LootQuestHooks`. |
| Repair (`Character.DoRepair`, `CSRepairSingleEquipment`, `CSRepairAllEquipments`), durability loss | `systems/economy/repair.rs` | done | Combat calls `on_damage_taken` / `on_death`. |
| `CraftManager`, `Models/Game/Crafts`, `CharacterCraft`, `CraftTask`, `CSExecuteCraft`, `SCCraftFailed` | `systems/economy/{craft,craft_data}.rs` | done | Cast through `CraftHooks::start_craft_cast`; skills call `craft::end_craft`. |
| Stores (`CSBuyItems`, `CSSellItems`, buy-back, `CSListSoldItem`, `CSBuyPriestBuff`, `CSBuyCoinItem`, `CSSellBackpackGoods`), `SpecialtyManager`, `TaxationsManager`, `ExpandExpert`, item look conversion | `systems/economy/{store,store_data,specialty}.rs` | done | Merchant packs / NPC flags through `NpcEconomyHooks`. Checkout validates currency, checked totals and each balance, then stages complete goods / buy-back delivery before committing payment. Vocation charges the configured price displayed by the 1.2 shop, without earnings bonuses. Rejected checkout preserves items and balances. Purchases need an NPC merchant pack or a doodad `DoodadFuncStoreUi` pack and filter the goods by it; selling needs a merchant within 3 m, lists each item once and pays only for items moved to buy-back. |
| `MailManager`, `Models/Game/Mails/**`, `CharacterMails`, mail packets | `systems/economy/{mail,mail_character,mail_kinds,mail_packets,mail_data}.rs` | done | Saved in `SaveStage::BeforeItems`; housing tax through `MailHousingHooks`. |
| `AuctionManager`, `Models/Game/Auction/**`, auction packets | `systems/economy/{auction,auction_data,auction_packets}.rs` | done | Saved in `SaveStage::AfterItems`; results mailed through `wiring`. Requests require an auctioneer NPC (the `auctioneer` template flag) named by the packet or the current interaction object within 5 m (the C# ignored the ids). |
| `TradeManager`, trade packets | `systems/economy/trade.rs` | done | |
| `CashShopManager`, `Models/Game/CashShop/**`, ICS / premium packets | `systems/economy/cash_shop.rs` | done | Gifts by `CommercialMail` (`wiring`); account credits SQL behind `CashShopDb`. |
| Item skill-effect bodies (grade enchant, socketing, conversion, cap scale, UCC imprint, ...), mate / slave equipment packets | `systems/economy/item_effects*.rs` | done | Called by the skills subsystem's effects. UCC imprint / cleanup require owned source and target items; client UCC casts require the actual source item's configured use skill, preserving the unrelated legacy bound-item portal exception. Dyeing checks the loaded `dyeable_items` allowlist. Invalid UCC / dye effects cancel deferred reagent consumption. |
| Item skill-effect stubs given data-derived bodies (`GainItemWithPosImprint` 32, `RechargeItemBuff` 95, `ExpToItem` 96) | `systems/economy/item_effects.rs` | done (data-derived) | The C# `Execute` bodies were `// TODO` stubs, so the behaviour comes from `special_effects` 405 / 546 / 547 / 548 (32: create `value1` x1 with `SkillEffectGainItemWithPos`), 12446 / 17017 (95) and 12462 (96: `value1` XP -> `value2` x1 with `SkillEffectExpToItem`) plus the tool skills that cast them. `GainItemWithPosImprint` stores no position imprint: the 1.2 item details have no such field. `ExpToItem` subtracts the experience directly instead of through `AddExp` (which recomputes the level and could level the character down), sends `SCExpChangedPacket(-value1)`, checks bag room before spending the XP, and cancels so the reagent is kept. `RechargeItemBuff` takes the cast's item target, resets `charge_start_time` (and `charge_count` when the template has one) and re-applies the equipped item's gear buff through `apply_gear_buffs` with the item both removed and added, so the recharge buff is not stacked; the 1 s item timers read the new charge window on their next tick, so nothing is re-scheduled. |

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

### systems::social

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `TeamManager`, `Models/Game/Team` | `systems/social/team.rs`, `packets/team.rs` | done | Invitations (single / area, 60 s timeout), create, AskRiskyTeam (leave / kick / dismiss + auto-disband), owner / role / marker / looting rule / ping / move, raid conversion, SetOffline / MemberRemoveFromTeam / UpdateAtLogin / UpdatePosition, 500 ms remote-member sync (delta cache, 5 s forced resync, upper-triangle pairs), zombie-team disband (5 min), loot winner helpers. Events (OnTeamJoin/Leave) are quest-side: not called. |
| `ExpeditionManager`, `Models/Game/Expeditions` | `systems/social/expedition.rs`, `packets/expedition.rs` (`managers::expedition` re-exports) | done | Load, create (party checks, cost, `SCItemTaskSuccessPacket` money-only form), invite / reply, leave, kick, member role, owner, role policy, disband (stays listed as `$deleted-guild-<id>`), `Expedition.Save` per change, offline leave for character deletion. Rename / sponsor are log-only / not in 1.2 as in C#. |
| `FamilyManager`, `Models/Game/Family` | `systems/social/family.rs`, `packets/family.rs` | done | Load, invite / reply / create, leave, kick (offline existence check), title, steward, login / logout, disband, save, offline leave for character deletion. |
| `FriendMananger`, `CharacterFriends`, `CharacterBlocked` | `systems/social/friend.rs`, `packets/friend.rs` (`managers::friend` re-exports) | done | Add / remove friends (online or by DB name lookup), block / unblock (online only), status changes incl. `InParty`. |
| `ChatManager`, `Models/Game/Chat` | `systems/social/chat.rs`, `session.rs` (`managers::chat` re-exports) | done | All channel kinds incl. party / raid / court, CleanUpChannels, party / raid / raid-leader / judge routing. User channels (`CSJoinUserChatChannel`) are log-only as in C#. |
| `FactionManager` relations, `SystemFaction.GetRelationState`, `Unit/Character.SetFaction` | `systems/social/faction.rs`, `packets/faction.rs` | done | Relation table validated against compact.sqlite3 (gated test). Faction CS packets are log-only as in C#; every SCFaction* struct ported. |
| `GameData/AchievementGameData` (`achievements`, `achievement_objectives`, `pre_completed_achievements`, `char_records`), `Models/Game/Achievement` | `systems/social/achievement.rs` (`server.game_data.systems.social.achievements`) | done | Loader + port of `AchievementGameDataTests`; gated real-DB test `achievement_game_data_real_compact_db`. The C# 1.2 tree has no manager; the Rust achievement manager / runtime / hooks / rewards (`achievement_*.rs`) read them (see "Achievement producers" below). |
| `DuelManager`, `Models/Game/Duels`, duel tasks | `systems/social/duel.rs`, `packets/duel.rs` | done | Request / accept (flag via hook) / start / stop / cancel, distance + HP checks, end timer (tokio tasks; `set_delays` test hook). |
| `CrimeManager`, `TrialManager`, `Models/Game/Crime`, `TrialUpdateTask` | `systems/social/{crime,trial}.rs`, `packets/trial.rs` | done | Crime table load / save (`AfterItems`), report / evidence (hooks), bot reports, trial / jury flow with 5 s tick, audience, bounty board (empty list as C#). Arrests, authenticated jury choices, guilty pleas, acquittals, court cleanup and reconnect jail enforcement have packet-flow regressions; prisoner buffs persist remaining online time. |
| `DominionData`, `Models/Game/Achievement`, rename / chat bubble / NPC chat / spam packets | `systems/social/{dominion,achievement}.rs`, `packets/misc.rs` | done (data + packets) | No C# manager drives them in 1.2. |
| `RankingGameData` (later C# history), 1.2 ranking packets | `systems/social/ranking_{data,packets,runtime,manager,persistence}.rs` | done (arena + fishing boards) | Not in the 1.2 C#; Rust only. Request path, MySQL persistence (`character_rank_scores`, `rank_period_payouts`), score hooks and period-close reward letters: see "Ranking boards" under Known deviations. |
| `RadarManager` | – | not social | Telescope / fish school registration from buff templates: skills / npcs. |

Cross-subsystem calls: wired in phase 3b (see "Phase 3b wiring": trial court doodads / NPCs after the spawns, `SocialHooks` by social itself, crime evidence / arrests / team and duel queries from skills, loot rules from economy, court checks in portals and instances, `force_dismount` through vehicles). Still open: teleports -> `session::on_moved` (C# `Character.SetPosition` -> `TeamManager.UpdatePosition`; plain movement does not call it in C# either).

### systems::housing

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `HousingGameData` (`item_housings`, `housings`, `housing_binding_doodads` + `Data/housing_bindings*.json`, `housing_build_steps`, `housing_decorations`, `item_housing_decorations`, `PostLoad`), `TaxationsManager` (`taxations`) | `systems/housing/data.rs` | done | Gated real-DB test `housing_real_compact_db` (row counts, bindings positioned). Duplicate build steps fail the load (C# `Dictionary.Add`). |
| `HousingManager` (all methods), `House` behaviour (`CurrentStep` setter + bound doodads, `AddBuildAction`, `Spawn` / `Delete` / visibility of `AttachedDoodads`, `AllowedToInteract`, `OnDeath`, `Save`), `HousingTaxTask`, `MailForTax` content | `systems/housing/{manager,ops,db}.rs` | done | Build (tax certificates or gold), permission, rename, recover toggle, demolish (+ `ReturnHouseItemsToOwner` mails), sale / sale markers / cancel / buy (+ mails, furniture owner), decoration, taxes (bills, untouchable / removal buffs, demolition timer every 10 s), death, `GetHouseAtLocation`, act-ability bonus. Items / money / mail go through `housing::hooks::HousingEconomyHooks` (economy installs it). |
| Housing packets (11 CS, 14 SC incl. `SCLoginCharInfoHouse`, `SCMyHouse`) | `systems/housing/packets.rs` | done | Byte-exact tests. `CSChangeHousePayPacket` has opcode 0xfff in 1.2 (not registered). `SCHouseOwnerNameChangedPacket` is social's (`packets/misc.rs`). |
| `CraftEffect` (`WorldInteractionGroup.Building`) | `systems/housing/mod.rs::apply_effect` | done | Routed first by `skills::ext::ExtHooks::route_effect`; quest `DoDoodadInteractionEvents` after it. |
| `SpawnManager.Load` / `SpawnPersistentDoodads` housing parts, `LoadAccount` / `CSListCharacter` / `CSSelectCharacter` / `Character.Load` / `DeleteCharacterAssets` housing parts | `npcs/spawn_manager.rs`, `packets/c2g.rs`, `world/session.rs`, `db_character.rs` (one call each) | done | `LoadPlayerHousing` + `SpawnAll` before the persistent doodads, house parenting + bound doodads, `ReconcileBoundDoodads` (`UsePersistentHouseDoodads`). |
| Placement validation | `systems/housing/placement.rs`, `systems/npcs/subzone.rs` | done | Original 1.2 housing XML `value1` tags resolve `housing_areas`/groups/categories; finite world positions, garden overlap, account eligibility, and nonrenewable beginner plots are checked server-side. World-scoped land checks also guard planting and housing crafting benches. Model-specific slope/height/footprint checks remain client-side. |

Economy hooks (`server.systems.housing.hooks`): installed by `economy::bridges::EconomyForHousing`. Housing API for others: `housing::{house_by_id, house_at_location, allowed_to_interact(_by_id), pay_weekly_tax (MailManager tax payment), act_ability_bonus_at (CharacterCraft), update_owned_housing_faction, demolish_by_player}`.

### systems::quests

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `QuestManager.Load` (contexts, components, base acts, all 64 `quest_act_*` detail tables, supplies, item / NPC groups, `UpdateQuestComponentActs`) | `systems/quests/data.rs`, `templates.rs` | done | Same table order (act order inside a component = objective indices), first-base-act linking, tutorial category skipped. Validated against compact.sqlite3 (`real_compact_db_quest_tables`: 4775 quests, 17720 components, 19047 acts, per-type counts). Exhaustive record accounting and outstanding gameplay failures: `Docs/wiki/Quest-Audit.md`, `rust/tools/quest_audit/audit.py`; port coverage does not certify every quest as playable. |
| `Models/Game/Quests` (`Quest`, `NewQuestCode`, `QuestStep`, `QuestComponent`, `QuestAct`, all `Acts/*` + `UnusedActs/*`) | `state.rs`, `engine.rs`, `acts.rs` | done | Live quests on `Character::quest_state` (handles + act paths instead of object references); `c.quests` rows re-derived after every operation. Step machine, objective status / percent, rewards, drop / cleanup, event subscriptions in invocation-list order (`-=` removes the last occurrence). |
| `CharacterQuests` (AddQuest*, DropQuest, completed flags, reset / daily reset, LetItDone, item destroy, `GetActiveActsWithUseItem`, Load) | `engine.rs`, `events.rs` | done | Save = existing `Character.Save` rows. |
| `Quest.DistributeRewards` exp / copper pool | `engine.rs` (`distribute_rewards`), `acts.rs` (`SupplyExp` / `SupplyCopper`) | deviation | **Deviation:** a Reward component `QuestActSupplyExp` / `QuestActSupplyCopper` act replaces the level-based `quest_supplies` exp / copper instead of being added on top of it (the C# added both). 1.2 client oracle + 2014 data, details in the deviations list. |
| `QuestManagerEvents` + other C# entry points (OnItemUse, OnKill, cinema, mate level) | `events.rs` | done | Public event API; nested calls from hooks are queued. |
| `QuestManager` evaluation queue / timers / daily reset task | `mod.rs` | done | `QuestManagerRunQueueTask` as a 1 ms tokio task; timers as tokio tasks; daily reset at 00:00 UTC. |
| `UnitRequirementsGameData` (`CanComponentRun`, `CanTriggerSphere`) + `UnitReqs.Validate` | `unit_reqs.rs` | done | `ReqSubject` / `ReqEnv` traits (owner = locked character, target = snapshot). `CanUseSkill` is the skills subsystem's (can reuse `validate`). |
| `SphereQuestManager`, `SphereQuest*`, `SphereGameData` (spheres, sphere_quests, `IsInsideAreaSphere`, `GetSphereIdFromQuest`) | `spheres.rs`, `data.rs` | done | `quest_sign_sphere.g` of `main_world`; one manager per server (500 ms tick). Other `sphere_*` tables not loaded (unused by quests). |
| `QuestActObjZoneKill.OnZoneKill` level window | `systems/quests/acts.rs` (`level_in_range`) | deviation | **Deviation:** a level bound of 0 is "unset". The 1.2 data has `lv_min_npc` / `lv_max_npc` = 0 / 0 on 103 of the 110 NPC-kill acts (`lv_min` / `lv_max` are the questor's window and differ from the NPC bounds on the 7 acts that set both), and the C# rejected every real victim against the raw 0..0 range, so those quests could never complete. The player-kill window uses the same rule. |
| `QuestActObjZoneKill` team share | `systems/quests/acts.rs`, `events.rs` (`run_deferred`) | deviation | **Deviation:** only the original killer shares the kill (the `ObjTalk` / `ObjInteraction` guard). The C# re-delegated `OnZoneKill` from every receiver ("to avoid loops") and bounced between members holding the same quest without end. NPC deaths raise the event for each eligible party member, who all share it again, so in the C# a member counted the kill once directly and once per sharing member. **Deviation:** each member now counts a kill once per act. `Npc.DoDie` (`npcs/npc/death.rs`) notes the victim in the `direct_kill_credits` of everyone it hands a direct event (eligible players, the fallback killer, Tag Share contributors) before the first event is raised, and a team-shared `ObjZoneKill` ignores a victim in that list and counts a shared victim once per (quest, act) (`CharacterSkillState::claim_shared_kill_credit`, 30 s memory because object ids are reused). Team members out of range, who get no direct event, still receive the share (once). |
| `QuestActObjZoneKill` PvP kills | `systems/skills/combat.rs` (`raise_pvp_zone_kill`, from `character_death_tail`) | deviation | **Deviation:** the C# only raised `OnZoneKill` from `Npc.DoDie` (`DoOnMonsterHuntEvents`), so the 16 `count_player_kill > 0` acts (e.g. quests 5982-5991) never progressed. A player killed by another player's hand (hostile relation, the same branch that awards PvP honor) now raises `ZoneKill` (zone group of the victim, killer and victim as `KillUnit`s) for the killer only; `team_share` acts hand it to the killer's team through the usual `TeamEvent`. Friendly-fire kills and mate / slave kills raise nothing. |
| `QuestActObjAggro` acceptor | `systems/quests/acts.rs` | done | Mirrors the C#: the victim must be the quest's acceptor NPC. All 30 aggro quests are started by `NpcTemplate.EngageCombatGiveQuestId` (`AddQuestFromNpc`), which sets that acceptor; a quest added by another path (GM command) cannot count aggro. |
| `Transform.World` for quest positions | `systems/quests/events.rs` (`world_position`), `spheres.rs` | deviation | **Deviation:** quest environment / target / nearby-NPC / sphere-starter positions are world positions (composed through the parent chain). The raw local transform of a mounted rider or ship passenger is parent-relative and never entered a sphere. |
| `QuestActConAcceptItemGain` start | `systems/quests/events.rs` (`on_items_acquired`), `engine.rs` (`start_quests_from_item_gain`), `data.rs` (`item_gain_starters`) | deviation | **Deviation:** gaining an item starts every quest with a `ConAcceptItemGain` on its Start component (not active, not completed unless repeatable, count in the bag reached, unit requirements met) with the item as acceptor. The C# never did: `Inventory.cs` has the `OnItemGather` call commented out and `RunAct` only compared the acceptor, so the Blue Salt Brotherhood letters (4881-4888), 5450, 6160 and the level 1 "Today's To-do" quests (1955-1961, 2140-2146) could not be started. Inferred from the data (those items have no `item_accept_quests` row); the retail trigger is not verified. The 15 "starter item without `starter_quests` entry" simulator notes were all of this kind: the quests have no `item_accept_quests` binding at all, so there was no mapping to fix. |
| Sphere state across logout | `systems/quests/spheres.rs` (`forget_character`), `mod.rs` (`on_leave_world`) | deviation | **Deviation:** leaving the world drops the character's sphere triggers and its last starter-sphere position. The C# kept both, so a re-login inside a quest sphere compared against the position from the previous session and never raised the enter event (stuck sphere quests). The tick also prunes positions of characters that are no longer in the world. |
| `with_quests` panic safety | `systems/quests/events.rs` | deviation | **Deviation:** a panic in a quest operation (or a queued one) is caught around the locked section, the quest state is put back, the thread-local active entry removed and the panic resumed. Without it the character's later quest events on that thread were queued forever. |
| Quest packets (CS: StartQuestContext, CompleteQuestContext, DropQuestContext, QuestTalkMade, QuestStartWith, TryQuestCompleteAsLetItDone, RestartMainQuest, SaveTutorial, StartedCinema, CompletedCinema; SC: QuestContextStarted/Updated/Completed/Reset, QuestRewardedByMail, DoodadQuestAccept, TutorialSaved, AppellationGained) | `packets.rs` | done | Byte-exact tests. `SCQuestsPacket` now writes `Quest.Id` as `long`. |
| `QuestCommandUtil` / `QuestCmd` (GM) | `systems/services/commands/scripts/quest_cmd.rs` | done | `quests::events::send_quests` / `send_completed_quests` added. |

### systems::instances

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `IndunGameData` (actions, events, zones, rooms), `Models/Game/Indun/{IndunZone, IndunRoom}` | `systems/instances/data.rs` | done | Same six event queries / four action queries in C# order; `EnterCount` hack (1000 for group 49 / channel dungeons, else 3); localized names. Real DB: 104 actions, 70 events, 20 zones, 13 rooms. |
| `BattlefieldGameData` + `Data/battlefields.json`, `TowerDefGameData`, `Models/Game/{InstantGame, TowerDefs}` data | `data.rs` | done | 7 battlefields (last `game_rule_sets` row wins), 24 tower defs / 89 valid progs. Tower defense skips orphan parents and orders waves by id. |
| `WorldManager.CreateWorldInstance` / `RemoveWorld` / `CreateStaticInstances` (AutoCreate) / `WorldIdManager`, `WorldInstance.CleanupInstance` / `ChannelId` / `HasCharacter` / `GetCharacterCount` / `ListPlayerNames` | `worlds.rs`, `mod.rs` (start), `world/mod.rs` (`has_world`, `remove_world`, `leave_other_instance`, `WorldInstance::transfer_out`), `npcs::spawn_manager::SpawnManagers::remove` | done | Non-instanceable templates reuse their instance; fixed ids <= 100; per-instance `SpawnManager` created with the world and loaded by the dungeon loader / a background task. The cross-instance region switch happens when the client sends `CSInstanceLoaded` (the character is re-filed in the arrival instance; the next move finds it filed), as the C# `AddVisibleObject` region change. |
| `Character.MainWorldPosition`, `Transform.CloneDetached` / `ApplyWorldSpawnPosition` / `MainWorldPosition.Clone()` | `worlds.rs`, `models/character.rs` (`main_world_position: Option<Transform>`), `db_character.rs` (save rule) | done | Saved inside an instance: the return point is written (C# `saveFromInstanceReturn`). |
| `IndunManager` (requests, requirements, entry history, system instances, deletion, leave, `DoIndunActions`, info tick) | `indun.rs` | done | |
| `Dungeon` + `DungeonLoaderTask` (queue, add / move, leave, team / disconnect handlers, `LeaveDungeonTick`, `AreaClearTick`, destroy) | `dungeon.rs` | done | `DestroyTeamDungeon` (never called in C#) not ported. |
| Party dungeon admission/reset and 1.2 mentor/mentee sealed chest quests | `indun.rs`, `dungeon.rs`, `mentoring.rs`, `scripts/dungeon.rs` | extended | Serialized run selection, pending-entry capacity, one charge per character/run, owner/leader-only empty resets, safe destruction; four paired daily quests use actual chest phases/seal items and existing inventory/quest rewards. See `Docs/wiki/Dungeons.md`. |
| `Models/Game/Indun/Events/*`, `Actions/*`, `WorldInstance.Events` (`OnUnitSpawn`, `OnDoodadSpawn`, `OnUnitKilled`, `OnUnitCombatStart/End`, `OnAreaClear`), `CharacterEvents` (`OnTeamJoin/Leave/Kick`, `OnDungeonLeave`, `OnDisconnect`), `UnitEvents.OnKill` | `events.rs` | done | Raised from `npcs` spawners, `skills::combat` (`PostUpdateCurrentHp` / `DoDie`), `social::team`, `world::session::on_disconnect`. |
| `InstantGameManager`, `InstantGame` (+ `InstantGameEvents`, team members / results) | `instant_game.rs` | done | Queue / matchmaking / invite / join / enter / start / kills / scores / end / leave; `Character.SetFaction` override (housing hook, guild kick, party kick); `ResetAllSkillCooldowns`. |
| Packets: `CSInstanceLoaded`, `CSUnknownInstance`, `CSApplyToInstantGame`, `CSCancelInstantGame`, `CSJoinInstantGame`, `CSEnteredInstantGameWorld`, `CSLeaveInstantGame`; `SCProcessingInstance`, `SCAppliedToInstantGame`, `SCInviteToInstantGame`, `SCInstantGameJoined/Start/End/AddPoint/Kill/Killstreak`, `SCInstantGameRemainTime` (`SCInstantGameUnkPacket` opcode), `SCTowerDefList/Start/End/WaveStart`, `SCSkillCooldownReset` | `packets.rs` | done | Byte-exact tests. `SCLoadInstancePacket` is the npcs portal one. |
| Doodad funcs `EnterInstance`, `EnterSysInstance`, `ExitIndun`, `RemoveInstance`; skills hooks `leave_instance` (`ExitArchemall`) / `main_world_position` (`Return`); portal `UsePortal` court check | `npcs/doodad/funcs/world.rs`, `skills/ext.rs` (default bodies), `npcs/portal/mod.rs` | done | |
| Tower defense runtime, `TowerDefenseAttackerAiCharacter` | `tower_def.rs`, `config.rs`, `scripts/tower_def.rs`, `ai/framework.rs` | extended | Rust adds event start/end, first-wave delay, timed/kill AND/OR progression, NPC controller/summon cleanup preserving normal corpse timers, client lists, GM controls and opt-in game-time/war schedules. The attacker's C# behavior graph already matches `AiKind::TowerDefenseAttacker`. Unsupported target kinds and absent spawn positions fail preflight; rewards/victory event chains are not added. |

Cross-subsystem: mates are despawned on dungeon entry and `PlayerCanEnter` checks the summoned vehicle (`systems::vehicles`); dungeon entry items use the economy's checked `EconomyBridge::bag_consume_item`; the permanent `Character.SetFaction` updates the owned houses (`SocialHooks::update_owned_housing_faction`, housing). Battlefield corps factions are temporary and skip every side effect (see the RPA-09 deviation below).

### systems::vehicles

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `MateGameData`, `SlaveGameData` (templates, unit modifiers, initial items / buffs, passive buffs, doodad / healing point / slave bindings, drop doodads, mount skills, repairable slaves, `slave_equip_slots`, `item_slave_equipments`, `allow_to_equip_slaves` / `allow_to_equip_slots`, `slave_attach_points*.json`), `TransferGameData` (templates, bindings, binding doodads, paths, client `transfer_path.xml` roads + `GetOwnerPaths`), `ShipyardManager.Load`, `TaxationsManager` table, `ModelManager` vehicle / ship models | `systems/vehicles/data.rs` | done | Gated test `real_compact_db_vehicle_tables` checks the row counts; `real_transfer_follows_its_client_route` loads the roads from the game_pak. |
| `MateManager` (per instance), `CharacterMates` (load / save / spawn / despawn), `Mate` (`AddExp`, `MateXpUpdateTask`, `RegenTick`, `DoFallDamage`, `OnZoneChange`, stats) | `systems/vehicles/mates.rs`, `stats.rs` | done | Mount / dismount (`SCUnitAttached` / `SCUnitDetached`, rider becomes a transform child), rename, target, user state, mate equipment packet (item move through `VehicleItemHooks`), kill exp, stun / unmount skills, death despawn. |
| `SlaveManager` (per instance), `SlaveSpawner`, `SummonSlave` details, `Slave` (`DoDie`, `DoHpChangeTrigger` / repair points, `RegenTick`, `Save`, `OnZoneChange`, stats) | `systems/vehicles/slaves.rs` | done | Create (saved slave / persistent doodads / saved child slaves from MySQL, position, boats on the water surface, doodad and slave bindings at the model attach points), bind / unbind, delete with portal-time despawn, Rider's Escape, repair cooldown, `RepairSlaveEffect`, `SendMySlaveTask`, logout (+10 min) / disconnect despawn task, `slaves` table (`AfterCharacters`). |
| `TransferManager` (per instance, 100 ms tick), `TransferSpawner`, `Transfer` route following (`GoToPath`, `MoveTo`, `NextPoint`) | `systems/vehicles/transfers.rs` | done | Carriage motor + boarding part + bench doodads, `SCOneUnitMovementPacket` (transfer move type) for both parts. Spawned from `SpawnManager.SpawnAll`. |
| `ShipyardManager` runtime, `Shipyard` build steps, `CraftEffect` shipyard branches, `ShipyardTickTask`, `ShipyardCompleteTask` | `systems/vehicles/shipyards.rs`, `effects.rs` | done | Requirements via the skills item hooks + money (`SCItemTaskSuccess` money form). |
| `ShipHarpoonRopeController`, `CSUnhangPacket` | `systems/vehicles/harpoon.rs` | done | Tow forces are physics (`ShipHarpoonTowPhysics`): the per-tick entry points are exposed for the physics port. |
| `CSMoveUnitPacket` vehicle branches (ship request, vehicle, mate xp, riding, standing on objects) | `systems/vehicles/movement.rs`, `world/session.rs` | done | Ship-deck support accepts the actor standing flag independently of the movement header, preserving valid GC-relative positions and the current-parent sentinel during controller updates. This corrects the mirrored C# header-only attachment test; clearing both support flags still disembarks. A unit no longer hanging (`HangingFromObject` unset) loses its sticky parent unless it is boarded on a transfer. |
| `BaseUnit.GetDistanceTo` for shipyard targets (skill range check, `Skill.cs` `max_range`) | `systems/skills/geometry.rs` (`distance_to`), `systems/vehicles/shipyards.rs` (`shipyard_radius`) | deviation | **Deviation:** the C# measured skill range to the shipyard's center point (shipyard models are prefab models without an actor radius, so `ModelSize` was 0), which made the shipyard build skills (`min_range 0`, `max_range 4`) unusable anywhere but the exact center of a drydock ("Target is too far"). The Rust subtracts the shipyard template's `build_radius * Scale`, like the house `GardenRadius` rule. |
| Mate / slave / transfer / shipyard C2G and G2C packets | `systems/vehicles/packets.rs` | done | 18 C->S, 16 S->C (byte-exact tests). `CSChangeSlaveTargetPacket` has no 1.2 opcode (0xfff). |
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
| `Services/WebApi/**` (`WebApiService`, `WebApiServer`, `WebApiSession`, `RouteMapper`, `RouteDefinition`, `WebApiGet/PostAttribute`, `IController`, controllers, models) | `systems/services/webapi/` | done | axum (HTTP/1.1) listener on `WebApiNetwork` (`*` = any); every request goes through `on_received_request` (`OnReceivedRequest`): unanchored, case-insensitive regex routes on the raw URL, `MatchCollection` with .NET group order, 404 "Not found", 500 HTML page. Responses use the Newtonsoft defaults (`json::JValue`), requests the System.Text.Json defaults (`json::Stj`). Status, World, Expedition, Character (MySQL), Command, Mail, Auction controllers. Ports of `RouteMapperTests` and `WebApiSessionTests` (+ controller tests). |
| `Services/DiscordBotService` | `systems/services/discord.rs` | done | twilight gateway + HTTP, unprivileged intents, "Playing with N other players" every 5 s, `/online`. Not started without `DiscordToken`. |
| `GameService`, `Program.UpTime`, `ManagerOrchestrator` | `systems/services/game_service.rs`, `main.rs` | done | Port of `GameServiceTests` and `ManagerOrchestratorTests`. |
| `TickManager` (+ `TickEventHandler`) | `systems/services/tick.rs` | done | Thread loop (20 ms), sync / async subscribers, busy skip. Port of `TickManagerTests`. |
| `TaskManager2`, `Models/Tasks/Task` | `systems/services/tasks.rs` | done | Delayed / repeating / cron tasks (NCrontab subset of `systems::npcs::schedule`), ids, cancel, `RemoveTasks`, `GetQueueCount`. Port of `TaskManagerTests`. |
| `AccountManager` remainder | `systems/services/accounts.rs`, `db.rs` | done | |
| `TimedRewardsManager`, `TimedRewardsTask` | `systems/services/timed_rewards.rs` | done | |
| `CharacterManager` remainder (`GetEffectiveAccessLevel`, `PlayerRoll` + `CSRollDicePacket` / `SCDiceValuePacket`, `StartOnlineTracking` / `CharacterOnlineTrackingTask`, `AddOfflineCrimePoints`, `ApplyBeautySalon` + `CSEnter/ExitBeautySalonPacket`, `CSBeautyshopDataPacket`, `SCCharacterGenderAndModelModifiedPacket`) | `systems/services/{character,beauty_salon}.rs` | done | Beauty salon item work through `BEAUTY_SALON_ITEMS` (economy). |
| Character packets: `CSActiveWeaponChangedPacket`, `CSBroadcastVisualOptionPacket`, `CSChangeAppellationPacket` (`CharacterAppellations.Change`), `CSIdleStatusPacket`, `CSRefreshInCharacterListPacket`, `CSRequestUIDataPacket`, `CSSaveUIDataPacket`, `CSUpdateActionSlotPacket`, `CSRestrictCheckPacket`, `CSSetLpManageCharacterPacket` and the log-only `CSCharDetailPacket`, `CSCleanupLogicLinkPacket`, `CSConsoleCmdUsedPacket`, `CSCriminalLockedPacket`, `CSEditorGameModePacket`, `CSRankCharacterPacket`, `CSRequestCharBriefPacket`, `CSRequestPermissionToPlayCinemaForDirectingMode`, `CSRequestSecondPasswordKeyTablesPacket`, `CSSetupSecondPassword`, `CSResturnAddrsPacket` (+ `SCActiveWeaponChangedPacket`, `SCUnitVisualOptionsPacket`, `SCAppellationChangedPacket`, `SCUnitIdleStatusPacket`, `SCRefreshInCharacterListPacket`, `SCResultRestrictCheckPacket`, `SCLpManagedPacket`) | `systems/services/character_packets.rs` | done | Byte-exact tests. `CSHangPacket` / `CSUnhangPacket` are `systems/vehicles/harpoon.rs` (sticky parents, rope break); `CSResurrectCharacterPacket` is `systems/skills/resurrect.rs`. |
| `CharacterManager` `bag_expands` / `expand_expert_limits`, `Inventory.ExpandSlot`, `CharacterActability.Regrade` / `ExpandExpert`, `CSExpandSlotsPacket`, `CSExpandExpertPacket`, `CSUpgrade/DowngradeExpertLimitPacket` (+ `SCInvenExpandedPacket`, `SCExpertLimitModifiedPacket`, `SCExpertExpandedPacket`) | `systems/services/expands.rs` | done | Item checks / consumption through `CHARACTER_ITEM_OPS` (economy). |
| `CommandManager`, `AccessLevelManager`, `Models/Game/ICommand`, `Utils/Scripts/**` | `systems/services/commands/` | done (infra) | Ports of `SubCommandBaseTests`, `NumericSubCommandParameterTests`, `AccessLevelManagerTests`, `DoodadChainSubCommandTests`. AStar sub commands need `ASTAR_HOOKS` (AI path finding). `Scripts/SubCommands/Doodads/DoodadSetAgeCommand` is ported (`commands/doodads.rs`, port of `DoodadSetAgeCommandTests` through a test root with the `DoodadCmd` aliases); the `DoodadCmd` root itself is the scripts phase. |
| `ExpressTextManager`, `CSExpressEmotionPacket`, `SCEmotionExpressedPacket` | `systems/services/express_text.rs` | done | Deviation: an emote whose sender object id is not the sender's own character is dropped (the C# broadcast the client-supplied id). |
| `LocalizationManager` | `systems/services/localization.rs` | done | 263 635 rows / 8 locales on the real DB. |
| `ModelManager`, `Models/Game/Models/*` | `systems/services/models.rs` | done | Counts checked against compact.sqlite3 (gated test). |
| `MusicManager`, `SongData`, music packets (`CSSaveUserMusicNotesPacket`, `CSSendUserMusicPacket`, `CSRequestMusicNotesPacket`, `CSEndMusicPacket`, `SCUserNoteLoadedPacket`) | `systems/services/music.rs` | done | `CreateSheetMusic` item work through `SheetMusicInventory` (economy; its `ItemSkillHooks::midi_cache` / `create_sheet_music` call this manager). |
| `RadarManager`, `TelescopeRegistrationEntry`, telescope / fish finder packets | `systems/services/radar.rs` | done | Registered from `BuffTemplate.Start` / `Dispel` (skills), unregistered on leave. Ship lists through `RadarShipHooks` (vehicles). |
| `Models/Tasks/ShutdownTask`, `SCNoticeMessagePacket` | `systems/services/shutdown.rs` | done | Count-down notices, final save, process exit; held in `server.systems.services.shutdown_task` (`SaveManager.ShutdownTask`). Buy-back wipe through `BUYBACK_WIPE` (economy). **Deviation:** the C# saved and exited without telling the clients (every client showed a disconnect error and quit). At T-0 the Rust task first broadcasts "The server is shutting down for maintenance.", refuses world entries (`GameServer::is_shutting_down`: `X2EnterWorld` / `CSSelectCharacter` / `CSSpawnCharacter` get `SCKicked(2)` + close), sends every connection `ConnCommand::ReturnToWorldList` (in-world: `LeaveWorldTask` to character select with no timer, i.e. save + `SCLeaveWorldGranted(1)` + `ChangeState(0)`; the client stays on its game connection and loads the lobby, so the world list request follows 2 s later: `GLPlayerReconnect` -> `LGPlayerReconnect` -> `SCReconnectAuth`, the `CSLeaveWorld(ServerSelect)` path, re-sent every 2 s to every connection still there, new ones included), waits up to 10 s for the clients to disconnect, kicks the rest with `SCKicked(2, "Server maintenance")`, then does the final save (3 tries), `shutdown::stop_server` (shared with the Ctrl+C path in `main.rs`) and the exit. A failed final save aborts the shutdown (as before: no exit) and reopens world entry; `/scripts shutdown cancel` is refused once T-0 passed. `/scripts forcedshutdown` still exits at once. |
| `SusManager` | `systems/services/sus.rs` | done | `audit_char_sus` log + speed analysis; the 5 s `FinalizeTransform` caller and the social / npcs log calls are not wired yet (see deviations). |
| `GameData/Framework` (`GameDataManager`, `IGameDataLoader`, `GameDataAttribute`) | `data/mod.rs`, `systems::SystemsData::load` | done (by design) | The reflection-driven loader list is the explicit `GameData::load` + per-subsystem `Data::load`; `Load` / `PostLoad` order is the call order there. |
| `Models/Tasks/SaveTask/SaveTickStartTask` | `managers/save.rs` | done (by design) | The periodic save is a Tokio interval instead of a `TaskManager` task (not counted in `GetQueueCount`). |
| `Utils/{MathUtil, NumericExtensions, Converters/*, DB/HoldablesSchemaCheck}` | `utils/` | done | `Utils/DB/SQLite*` = `data::sqlite`, `Utils/IdManager` = `managers::id`. `SphereCommandUtil` / `QuestCommandUtil`: `services::commands::scripts::{sphere, quest_cmd}`. |

### systems::ai

| Area (C#) | Rust | Status | Notes |
|---|---|---|---|
| `NpcManager.LoadAiParams` (NLua), `Models/Game/AI/v2/Params/**`, `AiGameData` (commands, command sets, chat bubbles, events), `ModelManager` (actor models + game stances, ship models) | `systems/ai/{lua,params,data}.rs` | done | A Lua 5.4 subset evaluator with `System.Convert` semantics; all 2 489 `npc_ai_params` rows produce the same values as NLua (differential harness, gated test `real_compact_db_params_match_nlua`; rows 443 / 835 / 1975 fail to parse in both). |
| `NpcAi` framework, `AiUtils.GetAiByType`, all `AiCharacters/**`, every `Behavior` (incl. Archer / BigMonster / Flytrap / WildBoar / AlmightyAttack / Talk / FollowPath / Return / Dead / Despawning / Spawning / Hold / Idle / Roaming / Alert / RunCommandSet / DoNothing) | `systems/ai/{framework,behaviors}.rs` | done | State machine, GoTo overrides, transitions, check intervals, skill queue / selection, aggression / alert / help, return, follow path, greetings (`SCNpcChatMessage` + `SCPlaySound`). |
| `AIManager` (100 ms tick, `AddAi`, region activity) | `systems/ai/manager.rs` | done | Per-AI mutex + queued calls; activity-based candidate selection and bounded `World.AiWorkerThreads` chunk batches (see deviations). |
| AI members of `Npc` / `Unit` (`MoveTowards`, `LookTowards`, `StopMovement`, `SetTarget`, `CurrentAggroTarget`, `CurrentGameStance`, `BaseMoveSpeed`, `OnDamageReceived`, `CheckIfEmptyAggroToReturn`, `ClearAllAggro`, `FindPath`), `NpcEvents` | `systems/ai/{npc,combat,skills,npc_events}.rs` | done | `SCOneUnitMovementPacket` movement, aggro tables through the skills API. |
| `AiPathsManager` (`Data/Path`), `AiPathHandler`, `Units/Route/Simulation` | `systems/ai/{path_handler,simulation}.rs` | done | Every `Data/Path/*.path` file loads (test). Routes are generated from the 10.x client, see the 2026-10-16 note. |
| `AiGeoDataManager`, CryEngine `.bai` readers (net / areas / verts missions), A* `PathNode`, `GetHeight`, `ReducePath` | `systems/ai/geodata/` | done | `GeoDataMode` only. Engine-faithful readers and a merged cross-sector graph (see "geodata deviations" below). 1.2 pak: 9 447 net (2 299 of them v54) + 9 447 verts (2 299 v1) + 3 265 areas files parse with 0 trailing bytes; 279 path areas files below v21 are rejected by version like the engine (they do not fit the v21 layout either), the 7 399 `hidemission` files are not read. The areas reader matched the C# reader on every dumped file (differential harness; path areas < v21 now excluded on purpose). Gated tests: `real_ai_*`, `real_nav_*` (`AAEMU_GAME_PAK`, `AAEMU_BAI_SAMPLES=D:\aa-re\zone\agent_geom\data`). |
| `Gimmick.MoveTowards`, flytrap gimmick | `systems/ai/gimmick.rs` | done | |
| Hooks for npcs / skills (create AI by `ai_file_id`, spawn / engage / death / delete / faction, NPC control effect, spyglass), AStar GM commands | `systems/ai/hooks.rs` | done | `AiHooks` in `server.systems.hooks.ai`; the npcs `NpcSkills` calls the AI parts of `NpcSkillHooks` directly (no decorator); `AiAStarHooks` installed into the services `ASTAR_HOOKS`. |
| `Physics/**`, `PhysicsManager` (ship hulls: Jitter2 world, `ShipController`, shore / ship–ship / doodad / cliff / static barrier contacts, buoyancy, whirlpool, storm lamps, harpoon tow, replication), `ShipSiegeAoEHit` geometry | `systems/ai/physics/` | done | The Jitter2 subset is bit-exact against Jitter2 2.8.8 for the integration (embedded reference test). Slaves call `add_ship` / `remove_ship` / `set_ship_request` / `set_ship_driver` / `ship_state` and may install `ShipPhysicsHooks` (harpoon rope state). |
| `WaterBodies`, `WaterBodyArea`, `Models/CryEngine/Objects` (`object.dat`) | `systems/ai/physics/{water,objects_file}.rs` | done | Water areas of all 1 294 client `object.dat` files equal the C# output bit for bit (differential harness, gated test `real_ai_object_dat_water_bodies`). Gimmick `FloatToSurface` uses them. |
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
  `sub_39232570` forwards it). `FallDamageZoneCap = false` restores the C#.
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
* Item procs of equip item set bonuses are not registered: `UnitProcs.RollProcsForKind` skips
  every proc whose cooldown has elapsed and `LastProc` starts at `DateTime.MinValue`, so no proc
  ever fires in the C#.
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

* systems::services (beauty salon): `ApplyBeautySalon` creates the requested hair item when
  the equipment hair slot is empty. The C# guard required an existing hair item, so a paid
  change could consume a Salon Certificate yet report hair 0. Same-hair requests still keep
  the item; receipts still report actual equipment. Hair-creation failure still logs and
  continues with the model update, receipt and certificate consumption, as in C#; failed
  certificate consumption still logs without undoing the already broadcast appearance.

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
* systems::vehicles (`DestroyAndSpawnSlave` special effect 72): row 8224 (values 0) is the only
  row, on skill 19165, carried by slave template 125's mount skill. A spawner-backed slave is
  removed through its `SlaveSpawner` (`Despawn` broadcasts the removal and releases the slot)
  and the spawner creates the replacement at its authored transform; a slave without a
  spawner is destroyed and logged, with nothing respawned.
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
* systems::skills / systems::ai (dead NPC returned and healed): the `DamageEffect` tail
  (`SCAiAggroPacket`, `AddUnitAggro`, `Npc.OnDamageReceived`) is skipped when the hit killed the
  NPC, and `CheckIfEmptyAggroToReturn` never sends a dead NPC to `ReturnState`. In the C# order the
  killer was re-added to the corpse's aggro table; when the NPC's `GoToDead` ran after that tail
  (its AI busy on another thread), `DeadBehavior`'s `ClearAllAggro` sent the corpse home and
  `ReturnState` restored its hp, so town guards kept fighting the mob they had killed (gray hp
  bar in the client). Test: `ai::flow_tests::guard_kill_while_the_mob_ai_is_busy_keeps_the_mob_dead`.
* `Character.Load` for a character with InfamyPoint >= 3000: the Wanted / Contemptuous buffs are
  added and the faction becomes Pirate as in C#; `SetFaction`'s side effects (house factions,
  `ExpeditionManager.Kick(this.Connection, ...)` — which throws on the null lobby connection and
  aborts `CSListCharacter` in C# — and the party kick) are not reproduced.
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
* Mail counts at select are always 0 — this matches the C#, whose `GetCurrentMailList` runs before
  the character is registered in the WorldManager and therefore never counts.
* `Character.OnZoneChange` into a closed or unknown zone: the system message is sent but the
  forced transport out (5 x `SCErrorMsg(ClosedZone)`, then `SCCharacterResurrected` at the closest
  return portal) is not done (needs PortalManager return portals / resurrection).
* Unknown faction ids: C# `Faction` is null (NRE in `Write` / `Faction.MotherId`); Rust writes the
  stored id and uses mother id 0 (-> the null chat channel).
* `CSMoveUnit`: only player characters exist, so slave / vehicle / mate / transfer moves are
  ignored like a non-matching `targetUnit` cast (superseded: vehicles, sticky parents, fall
  damage and the Sus analysis are ported); `LastPacketActivityTime` is `systems::services`.
* `LeaveAllChannels` iterates channels in key order (C# `ConcurrentDictionary` order is
  unspecified). Trial channels do not exist.
* The C# keeps `Connection.Characters[id]` and `ActiveChar` as the same object; Rust copies the
  active character back into the lobby list when `LeaveWorldTask` runs.
* Combat / slave logout delays (x30, +10 min) are not applied (no combat state / slaves).
* systems::economy: item operations are serialised by the economy operation lock (`ItemTxn`) and
  their packets / quest events are flushed after the container locks are released, in the C#
  order; character packets sent during an operation go through the same queue (`Out`).
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
* systems::economy (money guards, 2026-10-20): `CSDepositMoney` / `CSWithdrawMoney` refuse
  non-positive amounts and require a banker NPC (`banker` template flag) within 5 m of the current
  interaction object; `CSBuyItems` accepts goods only from the interacting NPC's merchant pack or
  the doodad's `DoodadFuncStoreUi` pack (the C# validated only the NPC pack and sold any template
  through a doodad); `CSSellItems` requires a merchant within 3 m, lists each item once and pays
  only for items actually moved to buy-back (the C# paid repeated entries and failed moves, with
  wrapping arithmetic); auction requests require an auctioneer NPC (`auctioneer` template flag)
  named by the packet ids or the current interaction object within 5 m (the C# ignored both ids
  and the `AuctionManager` only null-checked). The 5 m bank / auction range matches the other NPC
  service interactions (`repair.rs`, `mates.rs`); retail's own range is unverified.
* systems::economy (loot range, 2026-10-20): `CSLootItemPacket` (and loot-all from the bag)
  requires the player within 5 m of the loot owner. The C# defined
  `LootingContainer.MaxLootingRange` (200 m) only for loot *eligibility* at generation and never
  checked the take; 5 m matches the other NPC service interactions (`store::SERVICE_RANGE`,
  `portal::NAVI_USE_RANGE`). Refusals answer `SCLootItemFailedPacket(TooFarAway)`.
* systems::economy: `CSSetCraftingPayPacket`, `CSChangeItemLookPacket`, `CSSpecialtyRecordLoadPacket`,
  `CSBuySpecialtyItemPacket`, `CSReturnMailPacket` have no 1.2 opcode (0xfff): decoders exist, the
  1.2 client never sends them. The specialty / trade-pack mail delay use the C# defaults (no
  `Specialty` config section yet).
* systems::economy (native 1.2 protocol, 2026-10-25): correction to the preceding ReturnMail
  claim: the Return button (`read_mail.lua:946`) calls `ReturnMailById` (`3948B630` ->
  `393CD290`), which sends opcode `0xa3` with an i64 mail id (`397BA450`). The preserved
  opcode table misnames that slot `CSReportSpamPacket`; economy now registers ReturnMail
  using that legacy constant, and social no longer claims it. The historical C# registry
  fixture stays intact with an explicit native correction in its comparison test. Native
  `SCLootItemFailedPacket` reader `397C2F00` consumes `ErrorMessage:i16`, an eight-byte
  lootable and `type:u32` (14 bytes); the C# int error width is no longer mirrored.
* systems::economy: the localized item names are loaded by four modules (items search, auction,
  mail, cash shop); a later cleanup can share one cache.
* systems::services: authenticated `CSRequestCharBriefPacket` and `CSCharDetailPacket` requests
  return online character profiles through the existing 1.2 outbound formats (the C# inbound
  handlers only logged). Offline, absent and unauthenticated requests receive no reply; offline
  lookup and failure-response semantics remain unresolved.
* systems::services (UI data caps, 2026-10-20): `CSSaveUIDataPacket` accepts only keys `0..=16` and
  values of at most 16 KiB. Values observed from the real 1.2 client are 280, 4 708 (key bindings)
  and 313 bytes; 16 KiB is 3.5x the largest and stays below the 64 KiB `options.value` TEXT
  column. Unknown keys and oversized values are dropped with a warning (the C# stored any
  `ushort` key and any length in memory).
* systems::social: family title and steward requests require targets in the caller's own family.
  Authorized packet changes call the existing family snapshot save immediately; the C# handlers
  only changed memory. Team risky actions require membership and known actions, with owner
  authorization for kick/dismiss and a self target for leave before any side effects.
* systems::social: `SCChatMessagePacket` writes the packed 64-bit chat identity `(type, sub type,
  faction)` in its first eight bytes (direct Say / whisper / system text uses `(type, 0, 0)`); the
  sender faction stays in the sender block. Channel copies substitute the actual `ChatChannel` sub
  type / faction while keeping the requested chat type (zone Shout / Trade / LFG and raid /
  raid-leader included).

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
  are not loaded (`CheckBuffTag` false); `EquipShield` unit req is false; labor margin uses the
  non-premium cap 5000; debug messages use the character access level only.
* systems::quests: C# exceptions inside quest code are modelled where reachable with the 1.2
  data (`SetObjective` on a non-objective act, `MaxObjective` division by zero, the timer step
  switch): they abort the event invocation list / sphere tick. Out-of-range objective reads
  that would throw in RunAct / score sums read 0 (no 1.2 quest triggers them).
* systems::quests: the daily reset runs at 00:00 UTC (C# cron in server local time).
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
  them the full level-50 `quest_supplies` exp / copper on top. Open suspect: the
  client decompile also scales the level supply by `quest_contexts.degree` (x0.5..x1.5),
  but the 2014 site shows unscaled values for all 221 non-degree-3 quests; degree scaling
  is not implemented until that conflict is resolved.
* systems::quests: the October 2026 quest audit corrects timer event isolation, explicit NPC
  reporting, per-quest NPC/doodad reporting, doodad reward selection/finalization, and OR
  requirement evaluation. Timed saved deadlines, aggro credit, CheckSphere, and authored
  content gaps remain tracked in `Docs/wiki/Quest-Audit.md`.
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
* systems::skills: `CompressedGamePackets` (DD04) are written with stored (uncompressed) deflate
  blocks; the client accepts any valid deflate stream.
* systems::skills: the radar scans of `SkillTargetingUtil` are not ported.
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
  actions), recipe/account/cinema placeholders, several other special actions and generic
  Anchor/Flowgraph still need authoritative behavior or protocol evidence. Passing covered tests does not certify every data-defined skill.
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
* systems::skills: the labor margin requirement uses `GetMaxLabor(Payment.PremiumState)`; every
  connection has the default C# `AccountPayment` (premium until 2030), so the cap is 5000.
* systems::skills (in-place resurrection, 2026-10-20): `CSResurrectCharacterPacket(in_place =
  true)` requires a pending `SCNotifyResurrectionPacket` offer (`skill_char.resurrect_offer`, set
  by the `Resurrection` special effect and the free resurrection below
  `minimum_exp_loss_level`). The C# never checked, so any dead client could revive itself in
  place without an offer.
* systems::skills: `Character.DoDie` is complete (exp / durability loss, escalating respawn timer,
  PvP honor / assists / kill counts, War-zone honor loss, `DropTradePackToFloor`). The PvP
  damage / heal histories are lists of (character id, unix ms) in `skill_char`.
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
* systems::instances / systems::social (RPA-09 temporary factions): duel Red / Blue team and battlefield corps factions are temporary. `UnitBase::faction_temporary` marks them, `SCUnitFactionChangedPacket.temp` is `true`, and they skip `Character.SetFaction`'s housing update, guild kick and party removal. `db_character::persisted_faction` writes the origin faction, so an autosave, logout or crash during a duel / battlefield never stores the temporary faction; the faction is also restored on duel end, battlefield leave and departure/save (`world::session::save_and_remove_from_world`). Tested by `duel::tests::temporary_team_factions_keep_guild_and_restore`, `disconnect_during_a_duel_restores_the_origin_faction`, `instances::flow_tests::battlefield_entered_requires_actual_instance_and_subscribes_kills_once` and `db_character::tests::a_temporary_faction_is_never_persisted`.
* systems::instances (tower defense): the C# command only sent packets to its caller and hard-coded zone group 5. The Rust runtime reserves controller positions from the existing NPC JSON by compact spawner membership; these controllers no longer spawn permanently. Manual controls affect the caller's instance/group. Automatic schedules are disabled by default and validate every selected event at startup. Orphan waves/targets no longer truncate later valid rows. Kill notifications use only `DoDie`, deduplicate the NPC handle and retain id-reuse semantics. Timers use monotonic real seconds; `tod` uses the accelerated game clock and day intervals count from scheduler startup. Spawn effects carry explicit event ownership for cleanup even when Unit.OwnerId is zero. Unsupported `DoodadAlmighty` targets, NPC groups without authored controller positions and reward/victory chains remain unavailable rather than being synthesized.
* systems::instances (dungeons/mentoring): replaces the mirrored double entry-item charge and unsafe dungeon destruction below. Each character pays once per run; re-entry is free, capacity counts pending admissions, and creation obeys `World.MaxInstances` including concurrent requests. Empty owner/leader resets remove the whole run binding; destruction cancels its loader and returns occupants before removing their world. Party join grants access without inserting a player into another world's object table. Mentor eligibility is captured at boss death for nearby living party members with complementary active quests, rechecked during interactions, and bound to selected chest participants. Four explicit 1.2 quest/chest bindings are verified by a gated real-data test. Missing reward chests use the authored templates at the boss position (two metres apart for the two-chest dungeons); this placement fallback and qualification policy are emulator extensions, not claimed native-server execution parity. Authored phases grant seal items through existing inventory/quest hooks and daily persistence; Gilda is not issued directly by mentoring.
* systems::instances (2026-10-21, `CSInstanceLoaded` re-file): the reference 1.2 C# had the same gap - `LeaveDungeonInstance` / `LeaveSystemInstance` changed the transform and sent `SCLoadInstancePacket`, but `CSInstanceLoadedPacket` only sent `SCUnitStatePacket` and the time of day, so the client's emptied world stayed bare until the next move ran `Transform.FinalizeTransform` -> `AddVisibleObject`. Rust's `worlds::instance_loaded` now leaves the old instance's region grid (keeping the stale removal of its objects) and adds the character to the arrival instance's visibility grid, which sends the units / doodads / players in view and shows the character to them at once. A later C# commit (`ef0d41982`) fixed the same gap with `Transform.FinalizeTransform()` plus `ResendVisibleObjectsToCharacter(me, clientDroppedVisibility: true)`; that unconditional repaint could resend states, so Rust re-files through `add_visible_object`, which does nothing when the character is already filed in that region (a repeated `CSInstanceLoaded` or the next move sends nothing new). Tested by `instances::flow_tests::instance_return_refiles_character_and_sends_main_world_objects_without_movement`.

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
* systems::vehicles: `Thread.Sleep(50)` between `SCMateSpawnedPacket` and the mate spawn is not
  reproduced. Slave weapon dps is 0 (mate / slave `UpdateGearBonuses` is ported).
  Ship physics, water and the storm cannon block are `systems::ai` (`ship_physics`).
* systems::vehicles: logout removes the active mate list by character id, matching its key;
  the C# used `OwnerObjId`, leaving deleted mates listed and potentially removing another
  character's list when the ids collided. Repeated stale-mate deletion still avoids releasing
  its obj / tl ids a second time.
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
* systems::services (WebApi security, 2026-10-25): unlike C#, the HTTP listener defaults to
  loopback and rejects non-loopback binds without a configured `WebApiNetwork.ApiKey`.
  Every route except read-only `GET /status` requires a Bearer credential, compared using
  `subtle::ConstantTimeEq`; an absent or empty key grants no administrative access, including
  on localhost. `Enabled: false` disables the listener. Keys and bind settings are snapshotted
  at listener start; changes require restart. Existing tools must add the Authorization header.
* systems::services: `TaskManager.GetQueueCount` (WebApi `/status`) only counts the tasks of
  this queue; the other subsystems run their timers as Tokio tasks.
* systems::services: `TimedRewardsManager.DoTick` skips connections without an account (C#
  `GetAccountDetails(0)` inserts an `accounts` row 0); the payment of a connection reached from
  the tick is the default `AccountPayment` (it is never changed in C# either).
* systems::services: `AppConfiguration.Instance` mutations (World set commands,
  `ReloadConfiguration`) replace the `ConfigCell` snapshot; subsystems that copied values at
  start (skills config, doodad growth rate) are updated by the World set commands only.
* systems::services: `SusManager` analysis runs only where callers pass positions (no
  `FinalizeTransform`); the social and coffer `LogActivity` calls insert their row as a task.
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
  eligibility is `paid_patron` = the connection payment state (premium for every account) with no
  premium grade and no PC-bang, so rows needing a specific grade or a PC-bang are never listed;
  the progress is loaded at startup (`ScheduleItemManager::restore`) and saved inside the
  financial snapshot transaction (`SaveManager`), not by the C# per-account saves.

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
  coordinates in the main world). The `MainWorldPosition` fallback remains only for the Memory
  Tome (`value1 == 0`) outside instances. Retail resolves a Return gate in the caster's own
  world (instance worlds ship their own `return_point.g`) and never falls back to the main world.
  The C# closed-zone check of world gates (`Skill.cs` `portal_zone_check`) is not ported.

* systems::npcs (2026-10-15): `DoodadFuncClout` with `UseOriginSource` takes the first doodad
  within 4 m of the caster as the area trigger's owner, and the C# scheduled the
  `DoodadFuncCloutTask` on that owner, so it changed that doodad's phase to the clout's
  `NextPhase`, a function group of the other template. NPC 10119 near Arach Hollow summons doodad
  5007, whose clout moved the Amber Boulder (4252) into 5007's `DoodadFuncFinal` phase. That
  deleted the boulder, so quest 3447 "Fellow Searchers" could not be completed. 991 clouts use
  the origin source and 602 of them have a next phase. The borrowed doodad is still the trigger
  origin, but the task now changes the phase of the doodad that owns the clout.

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

* Static-data gaps (2026-10-15): buffs 2149, 1142 and 1933 reference absent skill controllers
  3554, 1806 and 7749 in the available 1.2 compact copies. Their controller behavior remains
  unsupported pending authentic definitions; no replacement parameters are invented. The supplied
  BuffEffect warnings also reference 33 absent buff templates, ten referenced by skill rows.
  Restoring them requires an authentic 1.2 dataset. Counts (compact.sqlite3): 338 buffs
  reference 113 absent skill controllers (also 6121 for buff 4341, 2883 for buff 1643) and 35
  BuffEffect rows reference 30 absent buffs (16 of them used by skill effects). The C# warned on
  every buff start; the Rust port warns once per (buff, controller) pair (repeats at debug) plus
  one startup summary for each gap, with per-row detail at debug. Skill 14602
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

### Ranking boards (Rust only)

The 1.2 C# never answered `CSRankCharacterPacket` (empty, log-only) and has no `CSRankSnapshotPacket` (0x130, unnamed in the opcode tables, `ranking_packets.rs`). The Rust server answers both from a global store (`systems::social::ranking_manager`, `server.systems.social.ranking`):

- **Requests.** 0x12f answers `SCRankCharacterPacket` (one row per supported board, zeros where the character has no score; `type_id` 0 because the request carries nothing to echo). 0x130 answers `SCRankSnapshotPacket` with the current period's standings (<= 100 lines); the racing board and unknown boards are not answered. The snapshot scope list stays empty (the raw scope figures are unresolved). The row's "account" string is the character name: the game database does not know the login account names. 0x130 is the one registration the C# registry-parity test exempts.
- **Persistence.** `character_rank_scores` (one row per character, board and period) and `rank_period_payouts` (periods already paid), loaded at startup and upserted by every `SaveManager` pass (`AfterCharacters`). Every row is rewritten on each save (idempotent, so a rolled-back save loses nothing); `Character::ranking_state` is unused (standings need offline characters, so the store is global).
- **Events.** The arena board scores at the end of an instant game (`instant_game::record_rankings`, once per game): win/lose/draw/invalid points are `game_rule_sets.rank_*_point` (only rule set 28, the gladiator duel in zone group 77: win 4, lose 1). Because `EndGame` leaves both corps `LOSE` when the timer runs out (mirrored C# bug), a timed-out duel scores both players as losers. The two fishing boards score each trophy fish a player lands (`fishing::record_catch`, from `DoodadFuncConvertFish` and the equipped-fish conversion) in the player's zone group; they only count inside the board's authored window, which ended in 2014, so on the shipped data no catch scores. The racing board is never recorded.
- **Period close.** A once-a-minute task (first tick at start-up) mails every closed, unpaid period: place -> `rank_scope_links` band -> `rank_rewards` item (count and grade as authored) as a `SYS_EXPRESS` letter (offline winners included), then `SCRankRewardMailPacket` to online winners. The period is marked paid before the letters are built, inside the economy operation gate; a crash between the letters and the next save can therefore lose (never duplicate) the mark. `rank_rewards.weeks` (how long the reward lasts) and appellation rewards (none authored) are not applied.
- **Not sent.** `SCRankAlarmPacket` (0x205): only the fishing contest authors alarms (`start_time_alarm` -5 / `end_time_alarm` 0 and message strings), that board ended in 2014 and the packet's `kind` / `rank_alarm_kind` values are unresolved, so no trigger is invented.

### Startup warnings (Rust only)

- **`FamilyIdManager` and family 0.** `characters.family` is 0 for a character without a family. The C# `FamilyIdManager` passes `Exclude = []`, so every start logged "Object ID 0 in DB is less than 1". `IdManagerSpec` now carries the C# `_exclude` list (checked before the first-id warning, as in `IdManager.Initialize`) and `FAMILY_ID` excludes 0. Every other manager keeps an empty list and still warns about ids below its first id.
- **Orphan tower defense waves.** The shipped `compact.sqlite3` has `tower_def_progs` rows for `tower_def_id` 9, 22 and 33 with no `tower_defs` parent (20 waves). The C# `return`s at the first one; the Rust loader skips them (see the instances section) and now logs one info summary with the count and the distinct parent ids; the per-row line is `debug`.
- **Client sources not found.** `Configurations/ClientData.json` lists four default sources and array settings merge by index (as in Microsoft.Extensions.Configuration), so a `Config.Local.json` with two sources leaves the last two defaults in place. `ClientFileManager::from_sources` logs a source that does not exist at `info` while at least one source is valid, and at `warn` for each (plus the existing error) when none is. `add_source` itself only logs not-found at `debug`.

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
  existing guild; `ChangeExpeditionRolePolicy` requires the Expel right; disbanded guilds stay in
  the list; family invitations always carry family id 1; hard disconnects keep the family member's
  character set; `GetTrialChat` throws when the faction has no court and the player is in no trial;
  `CSAddFriend` accepts yourself.

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
* systems::economy: `ItemProc` `ignoreRoll` never procs; `HDps` uses the magic DPS grade value;
  fish conversion only uses the first pack.
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
  character or change the schedule; leaving also cancels the caller's engraving. GM scheduling
  with player registration is the chosen server policy rather than historical auction calendars.
  Any registered current attacker may
  engrave; initial claim requires the current guild leader and exact Purified Archeum pack.
  Capture transfers the nine independently itemized fort designs to the new guild leader,
  preserves ordinary property owners, and applies authored fort-count protection stages.
  Guild-owned fort limits, leader-only construction, sale rejection and design returns are
  world policies inferred from original 1.2 data and contemporary guides, not recovered world
  binary rules. Native wall slope/mirror variant selection remains unverified.
  Participant buffs grant the original headquarters teleport skills. Headquarters bind to
  retained original doodads for the current siege and require explicit GM rebind after restart;
  registered dead participants can revive there. The SiegePeriod doodad predicate fails closed
  because its transition semantics are unresolved; phase cycles are bounded for all groups.
  Historical automatic auctions, reinforcement/commander eligibility and payout calendars are
  not inferred. Building HP persists through restart and lodestone collisions follow the native
  CGA/ANM root curves and settled static damage proxies. Absolute render/collider animation
  phase synchronization remains unverified. Property, authority, mail, items and wallets use
  one short prepared snapshot before SQL awaits; logout and shutdown serialize the same saves.
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
    and Peace Fund remains untouched. Hunting tax was removed by Trion's 1.2 notes; AA Points,
    Peace Fund percentages, nation splits and historical automatic payout scheduling are not
    invented. Original one-hour territory tax change cooldown uses wire error488/UI3544.
  Floors/walls/roofs use every normal-state prefab element and are synchronized when
  construction, residency or demolition changes. Model-specific placement slope/height/footprint
  checks and the developer-only
  category-8 `max_construct_count` rule are not inferred from later versions.
* systems::vehicles: `ShipyardCompleted` builds a `SkillItem` with only `ItemId`, so
  `SlaveManager.Create` returns on its `ItemTemplateId == 0` check: the finished ship is never
  summoned (the owner's active vehicle is still removed).
* systems::vehicles: a slave's `TlId` is never released; the transfer spawner's `Despawn` and
  the spawn manager's despawn tick both release the transfer's obj id.
* systems::vehicles: `SummonSlave.ReadDetails` always fails on `RepairStartTime` (so the repair
  cooldown does not survive a restart); `DoodadFuncRecoverItem` looks the owning slave up by its
  DB id as an obj id; `GetActiveMates(...).FirstOrDefault()` decides the `Raid` relation mate.
* systems::vehicles: `Unit.DespawnMate` on a mate's own death only despawns when its owner is
  dead. The former `UnitMoveType` driver-detachment bug is fixed (see vehicle test fixes above).
* systems::vehicles: `SpawnEffect` (slave) writes `PosDistance = 2` back into the template and
  `Rotate(Rotation with { Z = .. })` doubles the roll / pitch; an owner-less boat throws in
  `Create` after spawning (`GetWorld(owner.Transform.InstanceId)`).
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
* Cinema completion resends neighbouring world objects before raising quest events, matching
  the former 1.2 C# packet handler. A packet flow checks NPC state/points replication and the
  cinema objective's normal transition to Ready.

Additional world-side fixes from the same audit:

* Cash-shop checkout charges the effective discounted price, validates templates and checked
  cart totals before fulfillment, serializes stock updates, and uses guarded SQL credit and
  loyalty debits. Gold still uses the inventory transaction path. The formerly free AA Points
  branch now returns the existing failure (778): a distinct authoritative 1.2 AA Points wallet
  has not been established. `money2` is bank gold, not an AA Points balance. Unit flows and
  private-MySQL tests cover failed debits, limits and concurrent overdraft attempts. Debit,
  audit and commercial-mail delivery do not yet form one crash-atomic transaction/outbox.
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
  of the inertia-only mass box. Native entity bounds can include render and non-brush objects
  unavailable to this loader, so this is a documented proxy adaptation. Explicit buoyancy
  settings remain authoritative; missing model assets retain the prior geometry fallback.

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
| `NpcSpawner.SelectSpawnerId` for NORMAL multi-member spawners | `systems/npcs/npc/spawner.rs` (`select_spawner_id`) | matches the C# | Not changed. A JSON entry clones every game-data spawner that has its unit as a member and (memberwise clone) shares one `NpcSpawnerIds` list, so with more than one spawner only an AUTOCREATED unscheduled one or a scheduled one is ever selected: a unit that has no JSON entry of its own and is only a sibling of a NORMAL group spawner (Vicious/Silent Treant 3105/3106, Goblin Fighter 527, ...) never spawns. The 13 audit findings of this kind are all `ConAcceptNpcKill` starters, which `starter_accepts` evaluates as an OR over the component's acts, and in every one of the 7 quests other listed NPCs spawn, so no quest is blocked; the data would need real positions for these units (the 10.x editor has no placement of those group spawners). |

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

### Plot `ManaCost` special effect (type 39)

The C# charged `value1 + value2 / 6.35` (with a TODO: "value2 is based on skill level somehow"). The 1.2 data shows `value1` = the skill's `mana_cost` and `value2` = `mana_level_md * 100` for 59 of the 68 plot-only skills that carry the effect (e.g. 11854: md 1.55, effect 0/155; 18132: mana_cost 30, effect 30/0). The 1.2 client shows the cost in the skill tooltip as `mana_cost + mana_level_md * f(ability level)` (Ghidra `aa12u`, x2game `FUN_3941e750` -> `manaCost` of `FUN_394ace10`, via the unit formulas 12 and 13: `(ab_level * 1.6 + 8) * 3 / 3.65`), and the cast pre-check (`validation.rs`) already requires exactly that amount. The effect therefore now charges `value1 + value2 / 100 * scale` with the same scale as `skill::mana_cost`; at ability level 55 skill 11854 costs 120 instead of 24. The scale is no longer hardcoded: `skill::mana_level_scale` evaluates the character `unit_formulas` rows of `compact.sqlite3` (kind 12 `LevelDps` = `(ab_level * 1.6 + 8) * 3`, id 12; kind 13 `LevelMana` = `level_dps / 3.65`, id 13) through the precompiled formula expressions. The C# `Skill.ManaCost` (`Skill.cs` ~line 1561) hardcodes `((abLevel - 1) * 1.6 + 8) * 3 / 3.65`; the data and the client decompile cited above have no `- 1` (and `CharStats` evaluates `LevelDps` with `ab_level = level` as well), so the data formula is used for both `ManaCost` paths: every skill's mana cost rises by `1.6 * 3 / 3.65 * mana_level_md` (about 1.3 * md mana). The divisor `value2 / 100` was re-checked against the data: of 84 distinct (skill, `ManaCost` effect values) rows reached through the skills' plots, 62 have `value1 == mana_cost` and `value2 / 100 == mana_level_md`; the other 22 have `mana_level_md` 0 or 1 with `value2` 1 or 100-600, i.e. an effect-only charge that the same formula takes at face value. Deviation from the C#: the charge side is inferred from the tooltip/data (no retail charge capture); the 9 plot-only skills whose effect differs from the skill row follow the same rule.

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
* **Repeatable quests** (kinds 27/28) credit +1 per completed run on top of the entry back-credit of 1
  per distinct quest (audit Q3). Category counts therefore count repeats live.
* **ENCHANT_FAILURE** credits only a plain FAIL result; BREAK and DOWNGRADE are not proven to be "failures"
  (Q4). **GET_LOOTPACK** credits doodad loot packs only (Q6). **MAKE_HOUSING** credits the owner who
  performs the last build action. **Auction** and **jury** credits go to online characters only.
* **GET_FACTION** is state-derived (entry back-credit plus the crossing of the infamy threshold in
  `CrimeManager::add_crime_points`); other faction changes do not credit (Q8).
* **GET_ITEM_TYPE (k29), KILL_SLAVE (k26), MY_DISHONORABLE_POINT (k49), siege results (k57-60)** stay
  unwired (see the audit for why). `SCAchievementResetedPacket` (0x1ff) has no 1.2 trigger and is
  deliberately never sent.

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
| M15 | `SCCooldowns` has a full writer (`i32` count + `[id, total, remaining]` u32 triples, skills then tags, 150 per list) and is sent after `SCCharacterState` when the character has running skill cooldowns. The server keeps only the end time of a skill cooldown and no tags, so `total` equals `remaining` | verified: `397b64d0` -> `397b2350`; entries are appended, an empty list is no reset. Cooldowns are not persisted, so a freshly loaded character normally has none |
| CS | The client CS table entries missing from `opcodes::cs` were added: `0x11f CSRequestSetBountyMoney`, `0x120 CSSendBountyUpdate`, `0x130 CSRankSnapshot` (already handled through a local const, now the table entry), `0x133 CSChangeAutoUseAAPoint` | the Bevy `opcodes.rs` (client table); 0x130 additionally by the RE notes in `ranking_packets.rs`. `0x11f`/`0x120` are logged no-ops (the bounty ledger is not wired to the wallet, TODO); `0x133` sets `Character.auto_use_aa_point` and answers with `SCItemTaskSuccess` (`ItemTaskType` 110 carrying the one-byte `ChangeAutoUseAAPoint` task, action 18); the 1.2 C# had the task class but no 0x133 handler, so this reply is inferred, not observed on a live client |

Doc-only items:

- **M11** scheduled rewards: the client keeps at most 16 rows (verified: `397b0190` clamps the
  count to `0x10`, row `3961f100` = `u32,u8,i32,i64`). We still write up to
  `MAX_ITEMS_IN_PACKET` (32); the extra rows are ignored in a plain frame, and the packet is never
  batched, so it is left unchanged.
- **M12** list caps (family > 8, achievements > 50, radar > 10, battlefield members > 50, expedition
  chunks of 20): per Bevy RE notes; our senders already chunk at those sizes. Not re-verified.
- **M17** (f32 vs f64 arithmetic of pet revive/repair cost), **M18** (ranking period: UTC half-open vs
  local inclusive), **M19** (`SCUnitMovements` unsent), **M20** (party leave/kick log-only; the
  client uses `CSAskRiskyTeamAction`), **M21** (new factions get `political_system 1`, native
  rename needs 0), **M23** (actability downgrade clamp): per Bevy RE notes, not re-verified and not
  changed; each needs a reproduction against the real client before a fix.

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
  leader (255) can neither be expelled nor demoted, and equals cannot act on each other.
- `ChangeLootingRule` accepted any character id as loot master. It must now be 0 or a member of the
  team; otherwise only that field is ignored (logged). Over-head markers are unchanged: their
  target is any unit or object, not a team member.

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
  housing / shipyard build steps. Mount skills, mount-attached (rider) skills and NPC AI skills
  (`np_skills`) are deliberately absent. Possible over-blocking: a skill the 1.2 client casts that
  none of these tables names (the retail rule is not confirmed); such a cast is refused with a
  warning naming the skill id, so it is easy to add the table.
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
  only needs a non-success `SCSkillStarted`). Not closed here: a `Mount` caster naming the player
  itself still runs the mount branch (the self allowance above), and a bind-on-pickup item caster
  can still name a different skill id (the legacy fireplace exception, narrowed for UCC effects).

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
  comes back at full HP only after its repair wait, not when it is still on cooldown. (As the
  mirrored `SummonSlave.ReadDetails` bug above notes, the timer itself is still lost on restart.)

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
- **Ship parts.** Items with `item_impl = 28` (`ItemImplEnum.SlaveEquipment`) are now
  `ItemClass::SlaveEquipment` with `ItemDetailType.SlaveEquipment` (12 opaque detail bytes);
  `read_wire_item` consumes detail bytes by detail type (gear 55, ship parts 12, ...) instead of
  always reading an `EquipItem`. Bind-on-equip applies in `EquipmentSlave` too. A player-owned
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
