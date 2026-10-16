-- Tests for core.lua / ext_debug.lua with mocked natives. No client needed.
-- Run from this folder with any stock Lua 5.1:   lua core_tests.lua
-- (or: python -I run_lua_tests.py  -- uses the `lupa` package when no lua binary is installed)
local DIR = (arg and arg[0] and arg[0]:match('^(.*)[/\\][^/\\]*$')) or '.'
if EXT_DIR then DIR = EXT_DIR end

local sent, logs = {}, {}
local hooked = true
AAEmuExt = {
  Send = function(channel, payload) sent[#sent + 1] = { channel, payload }; return true end,
  Info = function() return { proto = 1, driver = 'test-driver', sc_opcode = 498, hooked = hooked, send = true } end,
  Log = function(s) logs[#logs + 1] = s end,
  _native = 1,
}
dofile(DIR .. '/core.lua')
dofile(DIR .. '/ext_debug.lua')
local X, json = AAEmuExt, AAEmuExt.Json

local failures = 0
local function check(cond, what)
  if not cond then failures = failures + 1; print('FAIL: ' .. what) end
end
local function eq(a, b, what)
  if a ~= b then failures = failures + 1; print('FAIL: ' .. what .. ' (got ' .. tostring(a) .. ', expected ' .. tostring(b) .. ')') end
end

-- ---- encode ---------------------------------------------------------------------------------
eq(json.encode({ a = 1, b = 'x', c = true }), '{"a":1,"b":"x","c":true}', 'object with sorted keys')
eq(json.encode({ 1, 2, 3 }), '[1,2,3]', 'sequence is an array')
eq(json.encode({}), '{}', 'empty table is an object')
eq(json.encode(json.array({})), '[]', 'marked empty array')
eq(json.encode({ list = json.array({}) }), '{"list":[]}', 'nested marked array')
eq(json.encode('he said "hi"\n\t\\'), '"he said \\"hi\\"\\n\\t\\\\"', 'string escapes')
eq(json.encode('\1\31'), '"\\u0001\\u001f"', 'control characters become \\u escapes')
eq(json.encode(1.5), '1.5', 'fraction')
eq(json.encode(-3), '-3', 'negative integer')
eq(json.encode(16777216), '16777216', 'float-exact integer')
eq(json.encode(0.1), '0.1', 'shortest float')
eq(json.encode({ n = 0 / 0 }), '{"n":null}', 'nan is null')
eq(json.encode({ 1, json.null, 3 }), '[1,null,3]', 'null sentinel in array')
eq(json.encode({ [1] = 'a', [3] = 'c' }), '{"1":"a","3":"c"}', 'sparse table is an object')
eq(json.encode('\227\129\130'), '"\227\129\130"', 'UTF-8 passes through')
local bad, err = json.encode({ f = function() end })
check(bad == nil and err, 'functions cannot be encoded')
local cyc = {}; cyc.self = cyc
bad, err = json.encode(cyc)
check(bad == nil and err, 'cycles are rejected by the depth limit')

-- ---- decode ---------------------------------------------------------------------------------
local v = json.decode('{"a":1,"b":[1,2,{"c":null}],"d":"x\\ny","e":true,"f":false,"g":-2.5e2}')
eq(v.a, 1, 'number'); eq(v.b[2], 2, 'array element'); eq(v.b[3].c, nil, 'null field omitted')
eq(v.d, 'x\ny', 'escape'); eq(v.e, true, 'true'); eq(v.f, false, 'false'); eq(v.g, -250, 'exponent')
v = json.decode('  [ 1 , null , 3 ]  ')
eq(v[2], json.null, 'null in array is json.null'); eq(v[3], 3, 'array continues after null')
eq(json.decode('"\\u00e9"'), '\195\169', '\\u00e9 -> UTF-8')
eq(json.decode('"\\u3042"'), '\227\129\130', '\\u3042 -> UTF-8')
eq(json.decode('"\\ud83d\\ude00"'), '\240\159\152\128', 'surrogate pair')
eq(json.decode('"\\ud83dx"'), '\239\191\189x', 'lone surrogate -> U+FFFD')
eq(json.decode('"a\\/b"'), 'a/b', 'escaped slash')
eq(json.decode('{}') ~= nil, true, 'empty object')
check(getmetatable(json.decode('[]')) ~= nil, 'empty array is marked')
for _, badText in ipairs({ '', '{', '[1,', '{"a":}', '{"a" 1}', '[1 2]', '"abc', '"\\x"', '{"a":1} x', '01', '-', '1.', 'nul',
                           '{1:2}', '"\\u12"', '[,]', "'a'", '"tab\tinside"' }) do
  local r, e = json.decode(badText)
  check(r == nil and e ~= nil, 'rejects: ' .. (badText:gsub('[%z\1-\31]', '?')))
end
local deep = string.rep('[', 40) .. string.rep(']', 40)
check(json.decode(deep) == nil, 'depth limit on decode')

-- round trip
local rt = { name = '\195\156n\195\175', ids = { '123456789012', 7 }, nested = { ok = true, f = 0.25 }, e = json.array({}) }
local text = json.encode(rt)
local back = json.decode(text)
eq(back.name, rt.name, 'roundtrip string'); eq(back.ids[1], '123456789012', 'roundtrip id string')
eq(back.nested.f, 0.25, 'roundtrip float'); eq(json.encode(back), text, 'stable re-encode')

-- ---- router, modules, handshake ---------------------------------------------------------------
local got
X.On('t.echo', function(msg, channel) got = { msg, channel } end)
AAEmuExt_Receive('t.echo', '{"x":5}')
eq(got and got[1].x, 5, 'handler receives decoded payload'); eq(got and got[2], 't.echo', 'handler receives channel')
AAEmuExt_Receive('t.echo', '')
check(next(got[1]) == nil, 'empty payload = empty table')
got = nil; AAEmuExt_Receive('t.unknown', '{}'); eq(got, nil, 'unknown channel is ignored')
local before = #logs; AAEmuExt_Receive('t.echo', '{broken'); check(#logs > before, 'bad payload is logged'); eq(got, nil, 'bad payload not delivered')
X.On('t.boom', function() error('boom') end)
AAEmuExt_Receive('t.boom', '{}')
check(logs[#logs]:find('boom'), 'handler errors are contained')

-- ext_debug
sent = {}
AAEmuExt_Receive('ext.debug.ping', '{"n":42}')
eq(#sent, 1, 'pong sent'); eq(sent[1][1], 'ext.debug.pong', 'pong channel'); eq(sent[1][2], '{"n":42}', 'pong payload')
check(X.Features()[1] == 'ext.debug', 'ext.debug feature registered')

-- modules
local initCalled
X.Module{ name = 'm1', features = { 'a.b', 'ext.debug' }, init = function() initCalled = true end }
check(initCalled, 'module init runs'); eq(#X.Features(), 2, 'features de-duplicated')
check(X.Module{ name = 'bad', init = function() error('x') end } == nil, 'failing module is rejected')

-- hello / welcome via the tick
_G.X2Unit = nil
sent = {}
AAEmuExt_Tick()
eq(#sent, 0, 'no hello outside the world')
local stage = 3
_G.X2LoginCharacter = { GetCurrentStage = function() return stage end }
_G.X2Unit = { UnitName = function() return 'Tester' end, GetUnitId = function() return 777 end }
AAEmuExt_Tick()
eq(#sent, 0, 'no hello while the lobby stage is not 0')
stage = 0
AAEmuExt_Tick()
eq(#sent, 1, 'hello when the player is in the world'); eq(sent[1][1], 'ext.hello', 'hello channel')
local hello = json.decode(sent[1][2])
eq(hello.proto, 1, 'hello proto'); eq(hello.driver, 'test-driver', 'hello driver'); eq(hello.sc_opcode, 498, 'hello sc_opcode')
eq(hello.features[1], 'ext.debug', 'hello features'); eq(hello.features[2], 'a.b', 'hello features 2')
AAEmuExt_Tick(); eq(#sent, 1, 'no resend before the retry interval')
check(not X.Ready(), 'not ready before welcome')
AAEmuExt_Receive('ext.welcome', '{"proto":1,"features":["ext.debug"]}')
check(X.Ready(), 'ready after welcome'); check(X.HasFeature('ext.debug'), 'server feature known'); check(not X.HasFeature('a.b'), 'unsupported feature unknown')
for _ = 1, 50 do AAEmuExt_Tick() end
eq(#sent, 1, 'no resend after welcome')
-- leaving the world and entering again (or another character) says hello again
_G.X2Unit = { UnitName = function() return '' end, GetUnitId = function() return 0 end }
AAEmuExt_Tick(); check(not X.Ready(), 'welcome state reset outside the world')
_G.X2Unit = { UnitName = function() return 'Tester' end, GetUnitId = function() return 778 end }
AAEmuExt_Tick(); eq(#sent, 2, 'hello again after re-entering')
-- retry without welcome, bounded
for _ = 1, 20 * 10 do AAEmuExt_Tick() end
eq(#sent, 1 + 5, 'hello retried at most 5 times per session')
-- not hooked: stay silent
hooked = false; _G.X2Unit = { UnitName = function() return 'Other' end, GetUnitId = function() return 1 end }
sent = {}; for _ = 1, 30 do AAEmuExt_Tick() end; eq(#sent, 0, 'no hello without a receive slot')

if failures > 0 then print(failures .. ' Lua test failure(s)'); os.exit(1) end
print('core.lua tests passed')
