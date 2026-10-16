-- AAEmu client extension core (Lua 5.1, ArcheAge 1.2 UI script state).
-- Spec: rust/docs/client/CLIENT-EXTENSIONS.md. Loaded first from the embedded bundle by the
-- driver (native/ext.cpp); the natives AAEmuExt.Send / Info / Log already exist at this point.
--
-- 1.2 Lua constraints: lua_Number is a 32-bit float (integers above 16,777,216 are inexact:
-- send ids and money as strings), no '#' on sparse tables, string.format('%d') on floats.

local string, table, math = string, table, math
local type, tostring, tonumber, pairs, ipairs = type, tostring, tonumber, pairs, ipairs
local pcall, error, setmetatable, getmetatable, select = pcall, error, setmetatable, getmetatable, select

local X = AAEmuExt
if type(X) ~= 'table' then X = {}; AAEmuExt = X end

local PROTO = 1
local nativeSend = X.Send
local function log(text)
  if X.Log then pcall(X.Log, tostring(text)) end
end

---------------------------------------------------------------------------------------------
-- JSON (encode/decode). null decodes to nil inside objects and to json.null inside arrays.
---------------------------------------------------------------------------------------------
local json = {}
json.null = setmetatable({}, { __tostring = function() return 'null' end })
local arrayMeta = {}
-- Marks a table as a JSON array (needed for empty arrays; a proper sequence is detected anyway).
function json.array(t) return setmetatable(t or {}, arrayMeta) end

local MAX_DEPTH = 32
local escapes = { ['"'] = '\\"', ['\\'] = '\\\\', ['\b'] = '\\b', ['\f'] = '\\f',
                  ['\n'] = '\\n', ['\r'] = '\\r', ['\t'] = '\\t' }
local function escapeChar(c) return escapes[c] or string.format('\\u%04x', string.byte(c)) end
-- (explicit control-character set: %c depends on the C locale and can match bytes >= 0x80)
local function quote(s) return '"' .. (string.gsub(s, '[%z\1-\31\127"\\]', escapeChar)) .. '"' end

local function encodeNumber(n)
  if n ~= n or n == math.huge or n == -math.huge then return 'null' end
  if n == math.floor(n) and n > -1e15 and n < 1e15 then return string.format('%.0f', n) end
  -- shortest representation that reads back as the same number (float32 needs <= 9 digits)
  local precisions = { 6, 7, 8, 9, 12, 15, 17 }
  local s
  for i = 1, #precisions do
    s = string.format('%.' .. precisions[i] .. 'g', n)
    if tonumber(s) == n then break end
  end
  return s
end

-- Number of elements if t is an array (explicitly marked or a gap-free 1..n sequence), else nil.
local function arrayLength(t)
  local max, count = 0, 0
  for k in pairs(t) do
    if type(k) ~= 'number' or k < 1 or k ~= math.floor(k) then
      if getmetatable(t) == arrayMeta then error('array has non-index key', 0) end
      return nil
    end
    count = count + 1
    if k > max then max = k end
  end
  if getmetatable(t) == arrayMeta then return max end
  if count > 0 and count == max then return max end
  return nil
end

local encodeValue
local function encodeInto(buf, v, depth)
  if depth > MAX_DEPTH then error('nesting too deep', 0) end
  local tv = type(v)
  if v == nil or v == json.null then buf[#buf + 1] = 'null'
  elseif tv == 'boolean' then buf[#buf + 1] = v and 'true' or 'false'
  elseif tv == 'number' then buf[#buf + 1] = encodeNumber(v)
  elseif tv == 'string' then buf[#buf + 1] = quote(v)
  elseif tv == 'table' then
    local n = arrayLength(v)
    if n then
      buf[#buf + 1] = '['
      for i = 1, n do
        if i > 1 then buf[#buf + 1] = ',' end
        encodeInto(buf, v[i], depth + 1)
      end
      buf[#buf + 1] = ']'
    else
      local keys = {}
      for k in pairs(v) do
        local tk = type(k)
        if tk ~= 'string' and tk ~= 'number' then error('unsupported key type ' .. tk, 0) end
        keys[#keys + 1] = k
      end
      table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
      buf[#buf + 1] = '{'
      local first = true
      for i = 1, #keys do
        local k = keys[i]
        local val = v[k]
        if val ~= nil then
          if not first then buf[#buf + 1] = ',' end
          first = false
          buf[#buf + 1] = quote(tostring(k)) .. ':'
          encodeInto(buf, val, depth + 1)
        end
      end
      buf[#buf + 1] = '}'
    end
  else
    error('cannot encode ' .. tv, 0)
  end
end

function json.encode(v)
  local buf = {}
  local ok, err = pcall(encodeInto, buf, v, 0)
  if not ok then return nil, 'json encode: ' .. tostring(err) end
  return table.concat(buf)
end

local function utf8Char(cp)
  if cp < 0x80 then return string.char(cp)
  elseif cp < 0x800 then
    return string.char(0xC0 + math.floor(cp / 64), 0x80 + cp % 64)
  elseif cp < 0x10000 then
    return string.char(0xE0 + math.floor(cp / 4096), 0x80 + math.floor(cp / 64) % 64, 0x80 + cp % 64)
  end
  return string.char(0xF0 + math.floor(cp / 262144), 0x80 + math.floor(cp / 4096) % 64,
                     0x80 + math.floor(cp / 64) % 64, 0x80 + cp % 64)
end

local unescapes = { ['"'] = '"', ['\\'] = '\\', ['/'] = '/', b = '\b', f = '\f', n = '\n', r = '\r', t = '\t' }

local function fail(i, msg) error('at ' .. tostring(i) .. ': ' .. msg, 0) end
local function skipSpace(s, i) return string.find(s, '[^ \t\r\n]', i) or (string.len(s) + 1) end

local function parseString(s, i)
  local out, n, j = {}, 0, i + 1
  while true do
    local a = string.find(s, '["\\%z\1-\31]', j)
    if not a then fail(i, 'unterminated string') end
    if a > j then n = n + 1; out[n] = string.sub(s, j, a - 1) end
    local c = string.sub(s, a, a)
    if c == '"' then return table.concat(out), a + 1 end
    if c ~= '\\' then fail(a, 'control character in string') end
    local e = string.sub(s, a + 1, a + 1)
    if unescapes[e] then
      n = n + 1; out[n] = unescapes[e]; j = a + 2
    elseif e == 'u' then
      local hex = string.match(s, '^%x%x%x%x', a + 2)
      if not hex then fail(a, 'bad \\u escape') end
      local cp = tonumber(hex, 16)
      j = a + 6
      if cp >= 0xD800 and cp <= 0xDBFF then
        local lo = string.match(s, '^\\u(%x%x%x%x)', j)
        local lov = lo and tonumber(lo, 16)
        if lov and lov >= 0xDC00 and lov <= 0xDFFF then
          cp = 0x10000 + (cp - 0xD800) * 1024 + (lov - 0xDC00)
          j = j + 6
        else
          cp = 0xFFFD
        end
      elseif cp >= 0xDC00 and cp <= 0xDFFF then
        cp = 0xFFFD
      end
      n = n + 1; out[n] = utf8Char(cp)
    else
      fail(a, 'bad escape')
    end
  end
end

local function parseNumber(s, i)
  local _, e = string.find(s, '^-?%d+', i)
  if not e then fail(i, 'bad number') end
  if string.find(string.sub(s, i, e), '^-?0%d') then fail(i, 'leading zero') end
  local _, e2 = string.find(s, '^%.%d+', e + 1)
  if e2 then e = e2 end
  local _, e3 = string.find(s, '^[eE][+-]?%d+', e + 1)
  if e3 then e = e3 end
  local v = tonumber(string.sub(s, i, e))
  if v == nil then fail(i, 'bad number') end
  return v, e + 1
end

local parseValue
local function parseArray(s, i, depth)
  local arr, n = {}, 0
  local j = skipSpace(s, i + 1)
  if string.sub(s, j, j) == ']' then return json.array(arr), j + 1 end
  while true do
    local v
    v, j = parseValue(s, j, depth + 1)
    n = n + 1
    if v == nil then v = json.null end
    arr[n] = v
    j = skipSpace(s, j)
    local c = string.sub(s, j, j)
    if c == ',' then j = skipSpace(s, j + 1)
    elseif c == ']' then return arr, j + 1
    else fail(j, "expected ',' or ']'") end
  end
end

local function parseObject(s, i, depth)
  local obj = {}
  local j = skipSpace(s, i + 1)
  if string.sub(s, j, j) == '}' then return obj, j + 1 end
  while true do
    if string.sub(s, j, j) ~= '"' then fail(j, 'expected object key') end
    local key
    key, j = parseString(s, j)
    j = skipSpace(s, j)
    if string.sub(s, j, j) ~= ':' then fail(j, "expected ':'") end
    local v
    v, j = parseValue(s, skipSpace(s, j + 1), depth + 1)
    obj[key] = v
    j = skipSpace(s, j)
    local c = string.sub(s, j, j)
    if c == ',' then j = skipSpace(s, j + 1)
    elseif c == '}' then return obj, j + 1
    else fail(j, "expected ',' or '}'") end
  end
end

-- Returns value, nextIndex. A JSON null yields nil.
parseValue = function(s, i, depth)
  if depth > MAX_DEPTH then fail(i, 'nesting too deep') end
  local c = string.sub(s, i, i)
  if c == '{' then return parseObject(s, i, depth)
  elseif c == '[' then return parseArray(s, i, depth)
  elseif c == '"' then return parseString(s, i)
  elseif c == 't' and string.sub(s, i, i + 3) == 'true' then return true, i + 4
  elseif c == 'f' and string.sub(s, i, i + 4) == 'false' then return false, i + 5
  elseif c == 'n' and string.sub(s, i, i + 3) == 'null' then return nil, i + 4
  elseif c == '-' or string.find(c, '^%d') then return parseNumber(s, i)
  end
  fail(i, 'unexpected character')
end

-- decode(text) -> value | nil, err   (top-level null also returns nil, "null")
function json.decode(text)
  if type(text) ~= 'string' then return nil, 'json decode: not a string' end
  local ok, v, nextIndex = pcall(function()
    local value, ni = parseValue(text, skipSpace(text, 1), 1)
    local rest = skipSpace(text, ni)
    if rest <= string.len(text) then fail(rest, 'trailing characters') end
    return value, ni
  end)
  if not ok then return nil, 'json decode: ' .. tostring(v) end
  if v == nil then return nil, 'null' end
  return v
end

X.Json = json

---------------------------------------------------------------------------------------------
-- Channel router, modules, handshake
---------------------------------------------------------------------------------------------
local handlers = {}          -- channel -> { fn, ... }
local modules = {}           -- name -> definition
local featureList = {}       -- registered features, registration order
local featureSet = {}
local readyCallbacks = {}
local state = {
  welcomed = false, serverFeatures = {}, serverFeatureSet = {},
  playerKey = nil, helloTick = nil, helloTries = 0, ticks = 0,
}

function X.On(channel, fn)
  if type(channel) ~= 'string' or type(fn) ~= 'function' then error('AAEmuExt.On(channel, fn)', 2) end
  local list = handlers[channel]
  if not list then list = {}; handlers[channel] = list end
  list[#list + 1] = fn
  return fn
end

function X.Off(channel, fn)
  local list = handlers[channel]
  if not list then return end
  for i = 1, #list do
    if list[i] == fn then table.remove(list, i); return end
  end
end

-- Encodes tbl (nil = empty payload) and sends it. Returns true | nil, err.
function X.Emit(channel, tbl)
  local payload = ''
  if tbl ~= nil then
    local s, err = json.encode(tbl)
    if not s then return nil, err end
    payload = s
  end
  if type(nativeSend) ~= 'function' then return nil, 'native Send is not available' end
  return nativeSend(channel, payload)
end

function X.Ready() return state.welcomed end

function X.HasFeature(name) return state.serverFeatureSet[name] == true end
function X.Features() return featureList end

function X.OnReady(fn)
  if type(fn) ~= 'function' then error('AAEmuExt.OnReady(fn)', 2) end
  readyCallbacks[#readyCallbacks + 1] = fn
  if state.welcomed then pcall(fn, X) end
end

-- AAEmuExt.Module{ name = "...", features = {"a.b"}, init = function(ext) ... end }
function X.Module(def)
  if type(def) ~= 'table' or type(def.name) ~= 'string' then
    log('AAEmuExt.Module: definition needs a name')
    return nil
  end
  if modules[def.name] then return modules[def.name] end
  if type(def.init) == 'function' then
    local ok, err = pcall(def.init, X)
    if not ok then
      log('module ' .. def.name .. ' init failed: ' .. tostring(err))
      return nil
    end
  end
  modules[def.name] = def
  if type(def.features) == 'table' then
    for i = 1, #def.features do
      local f = def.features[i]
      if type(f) == 'string' and not featureSet[f] then featureSet[f] = true; featureList[#featureList + 1] = f end
    end
  end
  return def
end

-- Called by the driver for every server message (main thread).
function AAEmuExt_Receive(channel, payload)
  local msg = {}
  if payload ~= nil and payload ~= '' then
    local v, err = json.decode(payload)
    if v == nil then
      log('bad payload on ' .. tostring(channel) .. ': ' .. tostring(err))
      return
    end
    if type(v) ~= 'table' then
      log('payload on ' .. tostring(channel) .. ' is not an object')
      return
    end
    msg = v
  end
  local list = handlers[channel]
  if not list then return end
  local copy = {}
  for i = 1, #list do copy[i] = list[i] end
  for i = 1, #copy do
    local ok, err = pcall(copy[i], msg, channel)
    if not ok then log('handler for ' .. tostring(channel) .. ' failed: ' .. tostring(err)) end
  end
end

X.On('ext.welcome', function(msg)
  state.welcomed = true
  state.serverFeatures = {}
  state.serverFeatureSet = {}
  if type(msg.features) == 'table' then
    for i = 1, #msg.features do
      local f = msg.features[i]
      if type(f) == 'string' then
        state.serverFeatures[#state.serverFeatures + 1] = f
        state.serverFeatureSet[f] = true
      end
    end
  end
  log('ext.welcome: proto ' .. tostring(msg.proto) .. ', server features: ' .. table.concat(state.serverFeatures, ','))
  for i = 1, #readyCallbacks do pcall(readyCallbacks[i], X) end
end)

X.On('ext.error', function(msg)
  log('ext.error: ' .. tostring(msg.error) .. ' ' .. tostring(msg.message or ''))
end)

-- "In the world" signal (documented in LUA-DRIVER.md section 7): the stock player unit exists,
-- i.e. X2Unit:UnitName('player') is non-empty, and the lobby stage reads 0. The unit id is part of
-- the key so a reconnect or a character switch that skips an empty reading still says hello again.
local function playerKey()
  local okName, name = pcall(function() return X2Unit and X2Unit:UnitName('player') end)
  if not okName or type(name) ~= 'string' or name == '' then return nil end
  if X2LoginCharacter and X2LoginCharacter.GetCurrentStage then
    local okStage, stage = pcall(function() return X2LoginCharacter:GetCurrentStage() end)
    if okStage and type(stage) == 'number' and stage ~= 0 then return nil end
  end
  local okId, id = pcall(function() return X2Unit:GetUnitId('player') end)
  return name .. '#' .. (okId and tostring(id) or '')
end
X._playerKey = playerKey

local HELLO_RETRY_TICKS = 20   -- ticks are ~500 ms: re-send after 10 s without ext.welcome
local HELLO_MAX_TRIES = 5

function X.Hello()
  local info = X.Info and X.Info() or {}
  local list = json.array({})
  for i = 1, #featureList do list[i] = featureList[i] end
  return X.Emit('ext.hello', {
    proto = PROTO, driver = info.driver, sc_opcode = info.sc_opcode, features = list,
  })
end

-- Called by the driver about every 500 ms from the main thread.
function AAEmuExt_Tick()
  state.ticks = state.ticks + 1
  local info = X.Info and X.Info()
  if type(info) ~= 'table' or not info.hooked or not info.send then return end
  local key = playerKey()
  if key ~= state.playerKey then
    state.playerKey = key
    state.welcomed = false
    state.helloTick = nil
    state.helloTries = 0
  end
  if not key or state.welcomed then return end
  if state.helloTick and state.ticks - state.helloTick < HELLO_RETRY_TICKS then return end
  if state.helloTries >= HELLO_MAX_TRIES then return end
  state.helloTries = state.helloTries + 1
  state.helloTick = state.ticks
  local ok, err = X.Hello()
  if not ok then log('ext.hello failed: ' .. tostring(err)) end
end

X._state = state
X._core = '1'
log('core loaded')
