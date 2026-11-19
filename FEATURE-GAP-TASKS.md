# Feature-gap task list — server vs. 1.2 client (prioritized)

Working backlog derived from the [October 14 audit](rust/CLIENT-DB-GAP-AUDIT.md).
Status below was reviewed against `main` at `e308e7461`. Use the
[`sc_baseline.json`](rust/tools/opcode_coverage/sc_baseline.json) lists for current packet
gaps and [`PORTING.md`](rust/crates/aaemu-game/PORTING.md) for deliberate deviations.

Primary oracle for packet layouts and client logic is now the **unpacked 1.2 `x2game.dll`**
in Ghidra — see the Ghidra section of [`REVERSING.md`](REVERSING.md#tools) for install,
project locations (`ghidra-work/proj-aa12u`, 65 203 functions) and the unpack recipe.
Secondary oracle: the 10.x zone decompile (`Bin64/decompiled/x2game-dev_dedicate.c`).

Priorities: **P0** = finished code that only needs wiring, or cheap guardrails.
**P1** = complete client features with a working oracle. **P2** = rules the shipped data
defines but the server ignores. **P3** = long tail, cleanup, and verify-before-building.
Effort: S ≤ a day, M ≈ days, L ≈ a week or more. Every task ships with the repo's
standard gates (byte-exact packet tests, flow tests, `cargo xtask ci`, live-client check).

---

## P0 — wire what's already built

- [x] **1. Achievements: live producers + entry snapshot.** World entry sends
  the snapshot and pending rewards. The [producer audit](rust/docs/achievement-hook-audit.txt)
  tracks remaining event coverage. The reset packet is part of packet triage (item 20).
- [x] **2. Ranking: request path + persistence.** Board requests and standings,
  arena and fishing scores, persisted period payouts, and ranking alarms are wired
  (`8b262ac6b`, `b558eb6de`, `4e9a13732`). Racing remains under item 17.
- [x] **3. Schedule items: send + claim.** World entry and schedule windows update
  the timer bar. The claim runtime persists progress in `account_schedule_items`
  (`389a37d0f`).
- [x] **4. CI ratchet for packet coverage.** CI and `cargo xtask ci` check
  `sc_usage.py --check` against the tracked baseline (`14e665423`).

## P1 — dead client features, oracle available

- [x] **5. Second password decision.** The feature is deliberately disabled in
  `cd1143af1`. Its client request IDs remain registered for compatibility. Login
  two-factor authentication is a separate feature.
- [ ] **6. Reverse-engineering coverage.** The unpacked 1.2 `x2game.dll` is
  available for handler analysis. See [Reversing](REVERSING.md#tools) for the
  local projects and remaining naming and data-section limits.
- [ ] **7. Packet layout recovery for remaining typeless SC opcodes** (M)
  In proj-aa12u, locate each client handler (start from the SCOffsets dispatch table and
  the strings already extracted, e.g. `OnMouldList`, `RACING_RESULT`), and record field
  layouts in `rust/CLIENT-DB-GAP-AUDIT.md` §1.1 or a sibling notes file. No server code in
  this task — it de-risks tasks 5, 8, 10, 12, 15, 17 and the P3 triage.
- [ ] **8. Battlefield remaining-time packet** (S)
  Queue state, invite timeout, authored scoring, rule events, and battlefield buffs
  are wired. Verify the remaining-time packet's opcode and caller in r208022
  before wiring it.
- [ ] **9. Quest blockers** (S–M)
  Fix or formally mirror-decide `QuestActObjZoneMonsterHunt` never-completes
  (`count > Count` vs capped objective); clear the 10 blocked/no-component quests and the
  43 missing-act-target cases from `quest_audit`; then sample-run the 622 needs-review
  quests live (static audit does not certify playability). Track in
  `Docs/wiki/Quest-Audit.md`.
- [x] **10. Specialty and trade-pack sales.** The server sends the price board
  through `SCSpecialtyGoodsPacket`, calculates negotiation bonuses, and supports
  configured labor rules. The 1.2 client discards `SCSpecialtyRecordsPacket`
  (see `economy/specialty.rs`). Video-derived sale amounts have regression tests.

## P2 — shipped rules the server ignores

- [ ] **11. Conditional buff unit modifiers** (M): implement the tag/ability
  relations from `buff_unit_modifiers` after verifying their consumers. AoE
  diminishing, buff breakers, and buff-granted mount skills already use client data.
- [ ] **12. Remaining NPC interaction and friendship rules** (M–L): review
  `ai_files` and the remaining friendship and world-interaction packet paths.
  Authored aggro links and NPC groups are loaded, and the server sends NPC
  interaction lists, status changes, and zone-driven close notifications.
- [x] **13. Character and vehicle limits from data.** Death waits and attribute
  bounds use client data, and vehicle equipment uses the allowed slave and slot packs.
- [ ] **14. Faction/family/expedition dead-packet cluster (~17)** (M): send the
  rename/immigrate/relation/owner/independence results, family title/name, sponsor —
  or prove from proj-aa12u that the 1.2 client never awaits them and delete the types.
  Includes wiring `CSChangeExpeditionSponsor`/`CSUpdateNationalTaxRate` only if a real
  1.2 opcode exists (today `0xfff`, unregistered — verify in proj-aa12u dispatch table).
- [ ] **15. Economy feedback packets** (M): send `SCCraftFailedPacket` and verify the four
  "TODO not verified" craft error pairs against proj-aa12u; add/send
  `SCItemDetailUpdated`, `SCItemAcquisition`, `SCItemTaskNotify`, `SCCraftItemUnlock`,
  `SCMateEquipmentExpired`, `SCMineAmount`, mail live updates
  (`SCMailReceiverOpened`/`SCMailRemoved`), `SCUnitLootingState`.
- [ ] **16. Housing live updates** (S–M): verify send paths for
  `SCHouseBuildPayChanged`, `SCHouseFarm`, and `SCHouseOwnerNameChanged`.
  The server already enforces the authored decoration limit.
- [ ] **17. Racing / time trials** (L): implement from 1.2 data (`race_tracks` 2,
  `race_track_shapes` 14) + typeless `SCRaceCond/Racing/RacingResult` (0x1f6–0x1f8).
  No 10.x oracle (feature removed there) — proj-aa12u is the only reference
  (`X2Racing`, `RACING_STATE/CONDITION/TEXT`). Includes the racing ranking board
  deliberately excluded from `ranking_runtime`.
- [ ] **18. Premium feedback packets** (S–M): accounts already persist payment
  state and premium points (`a08f62b74`). Review client consumers and send paths
  for `SCPremiumPointChanged` and `SCICSExchangeRatio`.

## P3 — long tail and hygiene

- [ ] **19. Verify-before-building set** (S each): moulds (`SCMould*` packets exist in the
  client — `OnMouldTaken` — but `moulds*` tables are empty in 1.2 data; build only if
  authored content exists elsewhere), demo mode (tables empty — likely classify
  won't-fix), `SCGmDump*`/`SCDump*`/`SCAiDebug` (GM/debug; classify), indun
  channel-select + client-driven-indun packet flows (data already loaded, unused).
- [ ] **20. Remaining packet triage** (M, can split): UCC/emblem family
  (`SCEmblemUploaded/Downloaded`, `SCUcc*`, `SCDoodadUcc*`), gimmick joints
  (`SCGimmickResetJoints/Grasped`), movement/combat (`SCUnitMovements` — writer exists,
  test-only; `SCCastingDelayed`, `SCBuffUpdated`, `SCKnockBackUnit`), portals
  (`SCPortalDeleted/Used`, `SCNaviTeleport`, `SCAboxTeleport`), jury verdict count +
  bounty senders (types exist), `SCPlaytime`, `SCSearchList`, `SCCooldowns`,
  `SCAbilityExpChanged`/`SCSkillUpgraded`, scheduled-event started/ended (0x1ec–0x1ed).
  Close each as implemented or documented won't-fix so the baseline lists shrink to a
  deliberate remainder (feeds task 4's ratchet).
- [ ] **21. Items data verdicts** (M): per-table keep/load/ignore decision for
  `item_recipes` (2 816), `item_conv_*` (~923), socket limits (`item_sockets` +
  `item_socket_num/level_limits`), `item_proc_bindings`, `item_dyeings`,
  `item_open_papers`, `item_cap_scale_forbids`, `tagged_items`, `craft_packs`,
  `merchant_packs`, mate/slave equip-pack tables, `physical_enchant_abilities` —
  several may be client-UI mirrors; proj-aa12u loader names settle which are rules.
- [ ] **22. Chat/name data** (S): use or drop `allowed_name_chars`, `blocked_texts`,
  `chat_spam_rules`(+details), `replace_chat*`, `slash_commands/functions`.
- [ ] **23. Client-file/physics remainder** (M–L): terrain surface types
  (`cover.ctc`), visibility volume semantics, and wheeled-vehicle physics. See the
  [geometry comparison](rust/docs/physics/ZONE-PHYSICS-USAGE.md#27-gap-summary-vs-the-rust-server).
- [ ] **24. Mirrored-bug re-decisions** (S each): trade completes one-sided/whole-stack,
  loot dice lowest-roll, stacked-item DB-row duplication, auction ExpireDate sort,
  mail count always 0 at select, double visibility packets. Each: keep mirrored
  (document why) or fix with a regression test; list lives in
  `PORTING.md` "C# bugs noticed".
- [ ] **25. Schema hygiene** (S): verify whether `cash_shop_item` and
  `mails_items` still have consumers or migration needs before retiring them.
