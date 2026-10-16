# AAEmu client extensions

A small, versioned channel between our server and the 1.2 client, carried by the injected
`aadriver.dll`. It lets features that the 1.2 client was never built for (backported 10.x
content, AAEmu-only tools) add **their own messages, Lua natives and UI** without touching the
client's files. The first user is the housing rebuild ([`backports/HOUSING.md`](backports/HOUSING.md)).

> Status (2026-10-16): implemented and verified live on the 1.2 client: `ext.hello` arrives
> byte-exact, the server's `ext.welcome` reaches Lua through the claimed SC slot (`0x1F2`), and
> the housing rebuild runs entirely over this channel.

Clients without the driver keep working: the server only uses the channel after the client
announced it (`ext.hello`), and every feature must keep a vanilla fallback or simply be absent.

## 1. Components

| Side | Piece | Location |
|---|---|---|
| Client native | transport (send hook, receive slot), Lua natives, main-thread queue, embedded Lua loader | `rust/crates/aaemu-clientdriver/native/ext.{cpp,h}` (+ resources) |
| Client Lua | `core.lua` (JSON codec, channel router, handshake, module registry) and one file per feature | `rust/crates/aaemu-clientdriver/native/ext/` |
| Server | `systems::ext`: the two packets, per-connection state, channel registry, `send` helper | `rust/crates/aaemu-game/src/systems/ext/` |
| Server features | register channel handlers from their own subsystem | e.g. `systems/housing/rebuild.rs` |

## 2. Wire format

Both directions are ordinary 1.2 game frames at level 1 (`[u16 len][dd][01][hash][count][u16 opcode][body]`,
see `net/packet.rs`). Only the opcodes are ours:

| Direction | Opcode | Name | Why it is free |
|---|---|---|---|
| C→S | `0x0C0` | `CSAAEmuExtPacket` | 1.2 CS range `0x0BE`–`0x0CA` has no packet; the client never sends these types |
| S→C | negotiated, default `0x1F2` | `SCAAEmuExtPacket` | the client's SC handler slot is empty (no stock handler); the driver claims the first empty slot of `0x1F2, 0x1D5, 0x159, 0x14F, 0x12A, 0x129, 0x10B, 0x104` at runtime and reports it in `ext.hello` |

Body (identical both ways, little-endian):

```
u8   proto        = 1
u16  channel_len  (1..64)
u8[] channel      ASCII, "<feature>.<message>", [a-z0-9_.]
u32  payload_len  (0..32768)
u8[] payload      UTF-8 JSON (object), may be empty
```

The game frame length is a `u16`, so a message must stay below 64 KB; v1 caps payloads at
32 KB and has no chunking. Receivers drop malformed bodies with a log line; never panic/crash.

### JSON conventions

- Lua numbers in this client are **32-bit floats** (`lua_Number = float`): integers above
  16,777,216 lose precision. Send any value that can exceed that (object ids, db ids, money,
  timestamps) as a **string**; small template ids and counts may be numbers.
- Field names are `snake_case`. Errors are `{ "ok": false, "error": "<code>", "message": "<text>" }`.
- Unknown fields are ignored on both sides (forward compatibility).

### Handshake and versioning

1. After entering the world the client sends `ext.hello`
   `{ "proto": 1, "driver": "<driver version>", "sc_opcode": 498, "features": ["house.rebuild", …] }`.
2. The server stores that per connection and answers `ext.welcome`
   `{ "proto": 1, "features": ["house.rebuild", …] }` (the intersection it supports).
3. The server sends no extension message to a connection before its hello, and uses the
   reported `sc_opcode`. State is per connection and dies with it; a reconnect says hello again.
4. A breaking change to the envelope bumps `proto`; feature-level changes use new channel
   names or optional fields, never a changed meaning of an existing field.

Reserved channels: `ext.*` (handshake, `ext.error` for unknown channels / bad payloads).

## 3. Client native layer (`aadriver`)

Hook points (unpacked 1.2 r208022 `x2game.dll`, Ghidra `proj-aa12u`, preferred base `0x38FF0000`;
resolve at runtime as `GetModuleHandle("x2game.dll") + RVA` and refuse to install on any
other build, like the existing CryScriptSystem check). RE notes:
`D:\aa-re\client-ext-20261015\net\` and `…\lua\`.

| What | RVA | Notes |
|---|---|---|
| `NetClient::Send(Packet*)` | `0x196810` | thiscall, `ret 4`; ECX = `*(void**)(base+0xB12294)` (check non-null). Serializes synchronously, so a stack packet is fine. Body is in a still-encrypted section of the dump — behaviour verified live only |
| CS packet object | — | `+0` vtable, `+4` u32 type, `+8` count u8, `+9` hash u8. vtable `[0]` = `base+0x2C160` (`xor eax,eax; ret`), `[1]` = `base+0x2E1E0` (shared header serializer), `[2]` = our body serializer `thiscall(this, Stream*)`, `ret 4` |
| Stream vtable | — | `+0x1C` IsReading, `+0x28` IsValid, `+0x3C` u32, `+0x40` u16, `+0x44` u8, `+0x4C` i32; each `thiscall(name_cstr, &val, 0)`, callee-clean |
| SC dispatcher | `0x1F6650` | reads u16 opcode; `op < 0x218 && slot[op]` → handler; flag byte selects path |
| SC handler table | `0xAD5728` | slots at `+4 + 4*op` (0x218 entries), flag bytes at `+0x864 + op`. Flag 1 = `handler->vtbl[2](reader, op, owner)` called directly on the receiving thread (unused by stock handlers); flag 0 = `vtbl[1]` builds a job for the main thread. Ours uses flag 1 and only copies bytes into a queue |
| Unknown SC opcode | — | queues a NULL job → `ArcheAge|Packet Error !!!`; never send an ext frame to a client that did not say hello |

Lua C API (CryScriptSystem.dll, all `__cdecl`, `lua_Number = float`, `TValue` tag at `+4`): the full
set of RVAs is in `native/ext.cpp` (source: `D:\aa-re\client-ext-20261015\lua\`).

Natives registered into the **UI** Lua state (re-registered whenever the state is rebuilt, the
same way `auction_ui.lua` is re-applied):

| Native | Returns |
|---|---|
| `AAEmuExt.Send(channel, payload_json)` | `true` or `nil, err` |
| `AAEmuExt.Info()` | `{ proto, driver, sc_opcode, hooked }` |
| `AAEmuExt.Log(text)` | — (driver log) |

Inbound messages are queued by the network-thread handler and delivered on the main thread from
the `CScriptSystem::Update` hook by calling the Lua global `AAEmuExt_Receive(channel, payload_json)`.

## 4. Client Lua layer

`ext/core.lua` (embedded, loaded first) provides:

```lua
AAEmuExt.On(channel, function(msg) ... end)   -- msg = decoded table
AAEmuExt.Emit(channel, tbl)                     -- encode + Send
AAEmuExt.Module{ name = "house_rebuild", features = {"house.rebuild"}, init = function() ... end }
AAEmuExt.Ready()                                -- true after ext.welcome
```

and sends `ext.hello` (with the features of all registered modules) once the player is in the
world. Feature modules are separate embedded files, loaded after `core.lua` in manifest order.

## 5. Server layer (`systems::ext`)

- `CSAAEmuExtPacket` (level 1, `0x0C0`) is registered by `ext::register`; `SCAAEmuExtPacket` is
  encoded with the connection's negotiated opcode. The opcodes are constants in
  `systems/ext/mod.rs`, not in the generated 1.2 tables of `opcodes.rs`.
- Per-connection `ExtState { proto, driver, sc_opcode, features }` lives in the
  `ConnectionHandle` (shared with `GameConnection`, dies with the connection), set by `ext.hello`.
- `ExtRegistry` is built once in `ext::System::default()`, one `register_ext` line per feature
  subsystem. A feature registers its name, its channels and an optional disconnect hook:

  ```rust
  // systems/housing/rebuild/channels.rs
  pub fn register_ext(reg: &mut ExtRegistry) {
      reg.feature("house.rebuild");
      reg.on_typed("house.rebuild.info", info);        // fn(&Arc<GameServer>, &GameConnection, InfoRequest)
      reg.on_typed("house.rebuild.start", start);
      reg.on_disconnect(preview::on_disconnect);
  }
  ```

  `on(channel, closure)` gets the raw `serde_json::Value`; `on_typed::<T>` deserializes first
  (bad payload -> `ext.error`).
- Sending: `ext::reply(conn, channel, &payload)`, `ext::send(&handle, …)`,
  `ext::send_player(&player, …)`; all are no-ops (debug log) for connections without ext state.
  `ext::has_feature(&handle, "house.rebuild")` gates feature code.
- Unknown channel -> `ext.error { "error": "unknown_channel" }`; bad JSON -> `bad_payload`.

## 6. Adding a feature

1. Pick a feature name and a channel prefix `<feature>.`; document the messages in the
   feature's doc (request/response fields, error codes).
2. Server: a `channels.rs` in the owning subsystem with `register_ext`, plus one line in
   `ext::System::default()`. Keep handlers thin; put the logic in the subsystem's ops so the
   vanilla fallback (GM/slash command) shares it. Tests with the flow-test client.
3. Client: `native/ext/<feature>.lua` calling `AAEmuExt.Module{ name, features = {"<feature>"}, init }`,
   `AAEmuExt.On(...)`/`AAEmuExt.Emit(...)`; add one line to `native/ext/manifest.txt`; tests in
   `native/ext/<feature>_tests.lua` (run by `run_lua_tests.py`). Prototype in the live client with
   `aaemu-clientdriver lua` before embedding. Build: `Scripts/build-client-pak.ps1`.
4. Gate the UI on `AAEmuExt.Ready()` and `AAEmuExt.HasFeature("<feature>")` so an old server
   simply shows nothing.
5. Bump the driver version for releases; never change an existing message's meaning.
