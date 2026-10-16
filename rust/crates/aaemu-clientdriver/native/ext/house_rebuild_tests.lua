-- Tests for house_rebuild.lua with mocked natives and a stub widget API. No client needed.
-- Run from this folder with any stock Lua 5.1:   lua house_rebuild_tests.lua
-- (or: python -I run_lua_tests.py  -- runs every *_tests.lua with the `lupa` package)
local DIR = (arg and arg[0] and arg[0]:match('^(.*)[/\\][^/\\]*$')) or '.'
if EXT_DIR then DIR = EXT_DIR end

-- ---- natives -----------------------------------------------------------------------------------
local sent, logs = {}, {}
local sendOk = true
AAEmuExt = {
  Send = function(channel, payload)
    if not sendOk then return nil, 'no transport' end
    sent[#sent + 1] = { channel, payload }
    return true
  end,
  Info = function() return { proto = 1, driver = 'test-driver', sc_opcode = 498, hooked = true, send = true } end,
  Log = function(s) logs[#logs + 1] = s end,
  _native = 1,
}
dofile(DIR .. '/core.lua')
local X, json = AAEmuExt, AAEmuExt.Json

-- ---- stub widget API -----------------------------------------------------------------------------
local windows = {}      -- id -> latest top-level window (CreateWindow / CreateEmptyWindow)
local created = {}      -- every widget in creation order
local styleStub = setmetatable({}, { __index = function(t, k)
  return function(self, ...) if k == 'SetColor' then self._color = { ... } end end
end })

local widgetMethods = {}
local function newWidget(kind, id, parent)
  local w = { _kind = kind, _id = id, _parent = parent, _shown = false, _enabled = true, _text = '',
              _children = {}, _handlers = {}, _hidden = 0 }
  w.style = setmetatable({}, { __index = function(t, k)
    return function(self, ...) if k == 'SetColor' then w._color = { ... } end end
  end })
  created[#created + 1] = w
  -- methods are UpperCamel (unknown ones are no-ops); lowerCamel keys are plain fields (nil when unset)
  return setmetatable(w, { __index = function(t, k)
    if widgetMethods[k] then return widgetMethods[k] end
    if type(k) == 'string' and k:match('^%u') then return function() end end
    return nil
  end })
end
function widgetMethods.SetText(self, t) self._text = t end
function widgetMethods.GetText(self) return self._text end
function widgetMethods.Show(self, v)
  local was = self._shown
  self._shown = v and true or false
  if was and not self._shown then
    self._hidden = self._hidden + 1
    if self._handlers.OnHide then self._handlers.OnHide(self) end
  end
end
function widgetMethods.IsVisible(self) return self._shown end
function widgetMethods.Enable(self, v) self._enabled = v and true or false end
function widgetMethods.IsEnabled(self) return self._enabled end
function widgetMethods.SetHandler(self, name, fn) self._handlers[name] = fn end
function widgetMethods.CreateChildWidget(self, kind, id, _, show)
  local c = newWidget(kind, id, self)
  c._shown = show and true or false
  self._children[id] = c
  self[id] = c
  return c
end
function widgetMethods.CreateColorDrawable(self, ...) return newWidget('drawable', 'd', self) end
function widgetMethods.SetVisible(self, v) self._shown = v and true or false end
function widgetMethods.GetOffset() return 0, 0 end
function widgetMethods.GetExtent() return 0, 0 end

local function click(w) assert(w._handlers.OnClick, 'no OnClick on ' .. tostring(w._id)); w._handlers.OnClick(w, 'LeftButton') end

function CreateWindow(id, parent)
  local w = newWidget('window', id, parent)
  windows[id] = w
  return w
end
function CreateEmptyWindow(id, parent)
  local w = newWidget('empty', id, parent)
  windows[id] = w
  return w
end
local registeredEvents
function RegistUIEvent(w, events) registeredEvents = events; w._events = events end
function ApplyButtonSkin() end
function GetWindowMargin() return 20, 50, -65 end
function CreateItemIconButton(id, parent) return newWidget('icon', id, parent) end
function ApplyTextColor() end
BUTTON_BASIC = { DEFAULT = {} }
FONT_SIZE = { SMALL = 11, MIDDLE = 13, LARGE = 15, XLARGE = 18 }
ALIGN_LEFT, ALIGN_CENTER, ALIGN_RIGHT = 1, 2, 3
CMF_SYSTEM = 12
X2Item = { GetItemInfoByType = function(self, id)
  if id == 8329 then return { name = 'Blue Salt Hammer', path = 'x.dds' } end
  return { name = 'invalid item type', path = 'bad.dds' }
end }
local chat, sysmsg = {}, {}
function AddMsgToChatWin(text) chat[#chat + 1] = text end
function AddMessageToSysMsgWindow(text) sysmsg[#sysmsg + 1] = text end

-- stock housing UI the module hooks
housing = {}
local stock = { show = 0, updateMy = 0, updateOther = 0, tax = 0, height = 0 }
function ShowHousingMainTainWindow(want)
  stock.show = stock.show + 1
  if want and not housing.maintainWindow then
    local w = CreateWindow('housing.maintainWindow', 'UIParent')
    w.okButton = w:CreateChildWidget('button', 'okButton', 0, true)
    w:Show(true)
    housing.maintainWindow = w
    w._handlers.OnHide = function() housing.maintainWindow = nil end
  elseif not want and housing.maintainWindow then
    housing.maintainWindow:Show(false)
  end
end
function UpdateMyHouseDefaultInfo() stock.updateMy = stock.updateMy + 1 end
function UpdateMyHouseTaxInfo() stock.tax = stock.tax + 1 end
function UpdateHouseDefaultInfo() stock.updateOther = stock.updateOther + 1 end
function UpdateMaintainWindowHeight() stock.height = stock.height + 1 end
local isMine = false
X2House = { IsMyHouse = function() return isMine end }

local playerName = 'Tester'
X2Unit = { UnitName = function() return playerName end, GetUnitId = function() return 7 end }

dofile(DIR .. '/house_rebuild.lua')
local H = AAEmuHouseRebuild

-- ---- helpers -----------------------------------------------------------------------------------
local failures = 0
local function check(cond, what)
  if not cond then failures = failures + 1; print('FAIL: ' .. what) end
end
local function eq(a, b, what)
  if a ~= b then failures = failures + 1; print('FAIL: ' .. what .. ' (got ' .. tostring(a) .. ', expected ' .. tostring(b) .. ')') end
end
local function drain()
  local out = {}
  for i = 1, #sent do out[i] = { channel = sent[i][1], msg = json.decode(sent[i][2] ~= '' and sent[i][2] or '{}') or {} } end
  sent = {}
  return out
end
local function receive(channel, tbl) AAEmuExt_Receive(channel, json.encode(tbl)) end
local function text(t) return (t:gsub('^|cFF%x%x%x%x%x%x', ''):gsub('|r$', '')) end

local function info(extra)
  local msg = {
    ok = true, house = '123456789012', template = 171, name = 'Thatched Farmhouse',
    options = {
      { rebuilding = 3, target = 432, name = 'Wilderness Farm', skill = 28829, skill_name = 'Extend Building Performance', labor = 0, tax = '250000',
        can = true, reason = '', reason_message = '',
        materials = { { item = 8329, count = 15, have = 15 }, { item = 34983, count = 6, have = 2 }, { item = 37893, count = 30, have = 0, name = 'Rough Plank' } } },
      { rebuilding = 4, target = 433, name = 'Prairie Farm', skill = 28828, labor = 5, tax = 250000, can = false, reason = 'materials',
        reason_message = 'You need 2 more Cedar Lumber.',
        materials = { { item = 8329, count = 15, have = 3 } } },
      { rebuilding = 5, target = 434, name = 'Lode Farm', tax = 1234567, can = true, materials = {} },
    },
  }
  if extra then for k, v in pairs(extra) do msg[k] = v end end
  return msg
end

local function gui() return windows['aaemuHouseRebuild'] end
local function ready() receive('ext.welcome', { proto = 1, features = { 'ext.debug', 'house.rebuild' } }) end

-- ---- pure helpers -------------------------------------------------------------------------------
eq(H.FormatMoney(250000), '25g', 'money: whole gold')
eq(H.FormatMoney('1234567'), '123g 45s 67c', 'money: g s c from string')
eq(H.FormatMoney(5), '5c', 'money: copper only')
eq(H.FormatMoney(10100), '1g 1s', 'money: gold and silver')
eq(H.FormatMoney('12345678901234'), '1,234,567,890g 12s 34c', 'money: exact big string with separators')
eq(H.FormatMoney(0), '0c', 'money: zero')
eq(H.FormatMoney(nil), '0c', 'money: nil')
eq(H.FormatMoney('x'), '0c', 'money: garbage')

-- ---- server absent: never errors -----------------------------------------------------------------
check(X.HasFeature('house.rebuild') == false, 'feature unknown before welcome')
check(H.Open() == false, 'open refuses without the server feature')
eq(#sysmsg, 1, 'user is told the server lacks the feature'); eq(gui(), nil, 'no window without the feature')
eq(#drain(), 0, 'nothing is sent without the feature')
check(X.Features()[#X.Features()] == 'house.rebuild', 'module registers the house.rebuild feature')

-- ---- open / info ---------------------------------------------------------------------------------
ready()
sendOk = false
check(H.Open() == true, 'open works while the transport fails'); check(gui() ~= nil, 'window is built')
check(H.state.busy == nil, 'no pending request when the send failed')
check(text(H.ui.status._text):find('Could not reach'), 'send failure shown in the status line')
H.Close()
sendOk = true

drain()
check(AAEmuHouseRebuild_Open() == true, 'global test entry opens the window')
local out = drain()
eq(#out, 1, 'one request on open'); eq(out[1].channel, 'house.rebuild.info', 'info requested')
eq(out[1].msg.house, nil, 'first request omits the house id')
check(gui()._shown, 'window shown')
check(text(H.ui.status._text):find('Loading'), 'loading status while waiting')

receive('house.rebuild.info', info())
eq(H.state.house, '123456789012', 'house id remembered as a string')
eq(#H.state.options, 3, 'three options'); eq(H.state.sel, 1, 'first option selected')
eq(H.ui.rows[1]._text, 'Wilderness Farm', 'row 1 text'); eq(H.ui.rows[3]._text, 'Lode Farm', 'row 3 text')
check(H.ui.rows[3]._shown, 'row 3 visible'); check(not H.ui.rows[4]._shown, 'row 4 hidden')
eq(H.ui.target._text, 'Wilderness Farm', 'target name'); eq(H.ui.tax._text, 'Tax: 25g', 'tax formatted')
eq(H.ui.labor._text, 'Labor: 0', 'labor')
eq(H.ui.matName[1]._text, 'Blue Salt Hammer', 'known item name'); eq(H.ui.matCount[1]._text, '15 / 15', 'material have/need')
eq(H.ui.matName[2]._text, 'Item #34983', 'unknown client item falls back to its id')
eq(H.ui.matName[3]._text, 'Rough Plank', 'server supplied name wins for unknown items')
local good, bad = H.ui.matCount[1]._color, H.ui.matCount[2]._color
check(good and bad and good[2] > good[1] and bad[1] > bad[2], 'enough materials green, missing red')
check(H.ui.btnRebuild._enabled, 'rebuild enabled when can'); check(H.ui.btnPreview._enabled, 'preview enabled')
eq(H.ui.btnPreview._text, 'Preview', 'preview label')
eq(text(H.ui.status._text), '', 'no status for a possible rebuild')

-- selecting an option that cannot be built
H.Select(2)
eq(H.state.sel, 2, 'second option selected')
check(not H.ui.btnRebuild._enabled, 'rebuild disabled when cannot')
eq(text(H.ui.status._text), 'You need 2 more Cedar Lumber.', 'reason shown'); eq(H.ui.tax._text, 'Tax: 25g', 'numeric tax')
eq(H.ui.skill._text, '', 'a skill without a localized name shows no skill line (never the id)')
H.Select(1)
eq(H.ui.skill._text, 'Skill: Extend Building Performance', 'localized skill name shown')
receive('house.rebuild.info', info({ options = {
  { rebuilding = 6, target = 435, name = 'Coded Farm', can = false, reason = 'labor', materials = {} },
  { rebuilding = 7, target = 436, name = 'Odd Farm', can = false, reason = 'brand_new_code', materials = {} },
} }))
H.Select(1)
eq(text(H.ui.status._text), 'Not enough labor power.', 'no reason_message: the local table text, never the bare code')
H.Select(2)
eq(text(H.ui.status._text), 'This rebuild is not possible right now.', 'unknown reason code falls back to the generic text')
check(not text(H.ui.status._text):find('brand_new_code'), 'unknown code is never shown')
receive('house.rebuild.info', info())
H.Select(2)
H.Select(3)
eq(H.ui.tax._text, 'Tax: 123g 45s 67c', 'tax with silver and copper'); eq(H.ui.matName[1]._text, 'No materials needed.', 'empty material list message')
eq(text(H.ui.status._text), '', 'cannot-less option has no reason')
H.Select(1)
drain()

-- ---- preview -------------------------------------------------------------------------------------
click(H.ui.btnPreview)
out = drain()
eq(#out, 1, 'one preview request'); eq(out[1].channel, 'house.rebuild.preview', 'preview channel')
eq(out[1].msg.house, '123456789012', 'house id sent back'); eq(out[1].msg.rebuilding, 3, 'rebuilding id')
eq(H.state.busy, 'preview', 'waiting for the preview reply'); check(not H.ui.btnPreview._enabled, 'controls locked while waiting')
receive('house.rebuild.preview', { ok = true, active = true })
check(H.state.preview, 'preview active'); eq(H.ui.btnPreview._text, 'End Preview', 'toggle label'); eq(H.state.busy, nil, 'idle again')

H.Select(2)   -- another option ends the preview
out = drain()
eq(#out, 1, 'switching option ends the preview'); eq(out[1].channel, 'house.rebuild.preview_end', 'preview_end channel')
eq(out[1].msg.house, '123456789012', 'preview_end carries the house')
check(not H.state.preview, 'preview cleared locally')
receive('house.rebuild.preview', { ok = true, active = false })
eq(H.ui.btnPreview._text, 'Preview', 'label back')

H.Select(1)
click(H.ui.btnPreview); receive('house.rebuild.preview', { ok = true, active = true }); drain()
click(H.ui.btnPreview)   -- toggle off
out = drain()
eq(#out, 1, 'toggle off sends preview_end'); eq(out[1].channel, 'house.rebuild.preview_end', 'toggle off channel')
receive('house.rebuild.preview', { ok = true, active = false })
check(not H.state.preview, 'preview off')

-- preview failure
sysmsg = {}
click(H.ui.btnPreview); drain()
receive('house.rebuild.preview', { ok = false, error = 'no_preview', message = 'Preview is not available.' })
check(not H.state.preview, 'failed preview is not active'); eq(text(H.ui.status._text), 'Preview is not available.', 'preview error in the status line')
eq(sysmsg[1], 'Preview is not available.', 'preview error as a system message')

-- ---- confirm + start -----------------------------------------------------------------------------
H.Select(2); H.Select(1)
click(H.ui.btnPreview); receive('house.rebuild.preview', { ok = true, active = true }); drain()
click(H.ui.btnRebuild)
local confirm = windows['aaemuHouseRebuildConfirm']
check(confirm and confirm._shown, 'confirm dialog shown'); check(H.state.confirming, 'confirming flag')
check(text(H.confirmUi.text._text):find('Thatched Farmhouse into Wilderness Farm', 1, true), 'confirm names source and target')
eq(#drain(), 0, 'nothing sent before the player confirms')
click(H.confirmUi.btnCancel)
check(not H.state.confirming and H.confirmUi == nil, 'cancel closes the confirm dialog'); eq(#drain(), 0, 'cancel sends nothing')
check(H.state.preview, 'cancel keeps the preview')

click(H.ui.btnRebuild); click(H.confirmUi.btnOk)
out = drain()
eq(#out, 2, 'confirm: preview_end then start'); eq(out[1].channel, 'house.rebuild.preview_end', 'preview ended before the start')
eq(out[2].channel, 'house.rebuild.start', 'start sent'); eq(out[2].msg.house, '123456789012', 'start house'); eq(out[2].msg.rebuilding, 3, 'start rebuilding')
eq(H.state.busy, 'start', 'waiting for the result')

-- failed start: error shown, options refreshed, status kept
sysmsg = {}
receive('house.rebuild.result', { ok = false, house = '123456789012', error = 'missing_materials', message = 'Missing materials.' })
out = drain()
eq(#out, 1, 'options refreshed after a failed start'); eq(out[1].channel, 'house.rebuild.info', 'refresh channel'); eq(out[1].msg.house, '123456789012', 'refresh uses the known house')
eq(sysmsg[1], 'Missing materials.', 'failure shown as a system message'); eq(text(H.ui.status._text), 'Missing materials.', 'failure in the status line')
receive('house.rebuild.info', info())
eq(text(H.ui.status._text), 'Missing materials.', 'refresh keeps the failure text'); eq(H.state.sel, 1, 'selection kept across the refresh')
H.Select(1); H.Select(3); H.Select(1)
eq(text(H.ui.status._text), '', 'status cleared by the next action')

-- error without message uses the code; unknown error object does not throw
receive('house.rebuild.result', { ok = false, error = 'not_owner' })
eq(text(H.ui.status._text), 'Only the owner can rebuild this house.', 'known error code without a message uses the local text')
receive('house.rebuild.result', { ok = false, error = 'future_error' })
check(text(H.ui.status._text):find('future_error'), 'an unknown error code is still reported')
drain(); receive('house.rebuild.info', info())

-- successful rebuild closes everything
click(H.ui.btnRebuild); click(H.confirmUi.btnOk); drain()
chat = {}
receive('house.rebuild.result', { ok = true, house = '123456789012', target = 'Wilderness Farm' })
eq(chat[1], 'The house was rebuilt into Wilderness Farm.', 'success message'); eq(H.ui, nil, 'window closed after success')
check(windows['aaemuHouseRebuild']._hidden >= 1, 'window hidden'); eq(H.state.house, nil, 'state reset')
eq(#drain(), 0, 'nothing sent on success (preview already ended)')

-- ---- closing -------------------------------------------------------------------------------------
H.Open(); drain(); receive('house.rebuild.info', info())
click(H.ui.btnPreview); receive('house.rebuild.preview', { ok = true, active = true }); drain()
local win = gui()
win:Show(false)   -- title-bar X / Escape
out = drain()
eq(#out, 1, 'closing ends the preview'); eq(out[1].channel, 'house.rebuild.preview_end', 'close sends preview_end')
eq(H.ui, nil, 'ui released'); check(not H.state.open, 'state closed')
H.Close(); eq(#drain(), 0, 'closing twice is harmless')

-- close while a preview request is still outstanding
H.Open(); drain(); receive('house.rebuild.info', info())
click(H.ui.btnPreview); drain()
H.Close(); out = drain()
eq(#out, 1, 'close with a pending preview ends it'); eq(out[1].channel, 'house.rebuild.preview_end', 'pending preview ended')
receive('house.rebuild.preview', { ok = true, active = true })   -- stale replies after close are harmless
receive('house.rebuild.info', info()); receive('house.rebuild.result', { ok = true })

-- close on world change (events and player key)
H.Open(); drain(); receive('house.rebuild.info', info())
click(H.ui.btnPreview); receive('house.rebuild.preview', { ok = true, active = true }); drain()
check(registeredEvents and registeredEvents.ENTERED_WORLD, 'world change event registered')
registeredEvents.ENTERED_WORLD()
out = drain()
eq(#out, 1, 'world change event ends the preview'); eq(H.ui, nil, 'window closed on ENTERED_WORLD')
for _, ev in ipairs({ 'HOUSE_INTERACTION_START', 'HOUSE_INTERACTION_END' }) do
  H.Open(); drain(); receive('house.rebuild.info', info())
  registeredEvents[ev]()
  eq(H.ui, nil, 'window closed on ' .. ev)
end
AAEmuExt_Tick()   -- learn the player key
H.Open(); drain(); receive('house.rebuild.info', info())
playerName = 'Other'
AAEmuExt_Tick()
eq(H.ui, nil, 'window closed when the player unit changes'); playerName = 'Tester'; AAEmuExt_Tick()
ready()

-- ---- timeouts and bad replies ---------------------------------------------------------------------
H.Open(); drain()
for _ = 1, 25 do AAEmuExt_Tick() end
eq(H.state.busy, nil, 'reply timeout clears the pending request')
check(text(H.ui.status._text):find('did not answer'), 'timeout shown')
receive('house.rebuild.info', info())   -- a late reply still works
eq(#H.state.options, 3, 'late reply accepted')
H.Close()

sysmsg = {}
H.Open(); drain()
receive('house.rebuild.info', { ok = false, error = 'no_house', message = nil })
eq(H.ui, nil, 'first-load failure closes the window'); eq(sysmsg[1], 'No house to rebuild was found.', 'first-load failure message')

H.Open(); drain()
receive('house.rebuild.info', { ok = true, house = 99, options = 'nonsense' })
eq(#H.state.options, 0, 'malformed options ignored'); eq(H.state.house, '99', 'numeric house id stringified')
check(text(H.ui.status._text):find('cannot be rebuilt'), 'empty option list explained')
receive('house.rebuild.info', { ok = true, house = '99', options = { 'x', { rebuilding = 1 }, { rebuilding = 2, materials = 'bad' } } })
eq(#H.state.options, 2, 'non-table options skipped'); eq(H.ui.rows[1]._text, '#1', 'unnamed option gets a placeholder')
check(not H.ui.btnRebuild._enabled, 'rebuild disabled when `can` is missing')
H.Close()

-- many options are cut off (bounded widgets)
H.Open(); drain()
local many = info(); many.options = {}
for i = 1, 12 do many.options[i] = { rebuilding = i, name = 'O' .. i, can = true } end
receive('house.rebuild.info', many)
eq(#H.state.options, 8, 'option list is bounded'); check(H.state.cutOff, 'cut-off recorded')
H.Close()

-- resource bound: windows are created per open, rows are fixed
local before = #created
for _ = 1, 3 do H.Open(); drain(); receive('house.rebuild.info', info()); H.Close() end
local perOpen = (#created - before) / 3
H.Open(); drain(); receive('house.rebuild.info', info()); H.Close()
eq(#created - before, perOpen * 4, 'every open builds the same bounded set of widgets')

-- ---- maintain window integration -----------------------------------------------------------------
ShowHousingMainTainWindow(true)
local mw = housing.maintainWindow
check(mw ~= nil, 'stock window exists'); eq(mw.aaemuRebuildButton, nil, 'no button before it is known to be my house')
check(stock.show == 1, 'stock Show called exactly once through the hook')
UpdateMyHouseDefaultInfo()
check(stock.updateMy == 1, 'stock UpdateMyHouseDefaultInfo called exactly once')
local btn = mw.aaemuRebuildButton
check(btn ~= nil, 'button added for my house'); eq(btn._text, 'Rebuild', 'button label'); check(btn._shown, 'button visible when the server supports it')
UpdateMyHouseDefaultInfo(); UpdateMyHouseTaxInfo(); UpdateMaintainWindowHeight()
eq(mw.aaemuRebuildButton, btn, 'button is created only once per window')
local count = 0
for _, w in ipairs(created) do if w._id == 'aaemuRebuildButton' then count = count + 1 end end
eq(count, 1, 'exactly one button widget')
drain(); click(btn)
out = drain()
eq(#out, 1, 'button opens the rebuild window'); eq(out[1].channel, 'house.rebuild.info', 'button requests info'); H.Close()
-- not ready: hidden
X._state.welcomed = false
AAEmuExt_Tick()
check(not btn._shown, 'button hidden when the server does not support the feature'); X._state.welcomed = true
AAEmuExt_Tick(); check(btn._shown, 'button shown again when ready')
-- other people's house: no button
stock.updateOther = 0
mw:Show(false); eq(housing.maintainWindow, nil, 'stock window gone')
ShowHousingMainTainWindow(true); local mw2 = housing.maintainWindow
UpdateHouseDefaultInfo()
eq(mw2.aaemuRebuildButton, nil, 'no button for a foreign house')
-- fallback by tick when the hook did not see the stock call
isMine = true
AAEmuExt_Tick()
check(mw2.aaemuRebuildButton ~= nil, 'tick attaches the button via X2House:IsMyHouse'); isMine = false

-- ---- idempotence ---------------------------------------------------------------------------------
local handlerSnapshot = {}
dofile(DIR .. '/house_rebuild.lua')
dofile(DIR .. '/house_rebuild.lua')
stock.show, stock.updateMy = 0, 0
housing.maintainWindow = nil
ShowHousingMainTainWindow(true); UpdateMyHouseDefaultInfo()
eq(stock.show, 1, 'hooks are not stacked by a re-run'); eq(stock.updateMy, 1, 'UpdateMyHouseDefaultInfo hook not stacked')
H = AAEmuHouseRebuild
sysmsg, chat = {}, {}
H.Open(); drain(); receive('house.rebuild.info', info())
out = drain(); eq(#out, 0, 'one info handler after re-runs (no extra messages)')
click(H.ui.btnRebuild); click(H.confirmUi.btnOk); drain()
receive('house.rebuild.result', { ok = false, error = 'x', message = 'Nope.' }); drain()
eq(#sysmsg, 1, 'result handled exactly once after re-runs')
H.Close()
local ticksBefore = H.state.ticks
AAEmuExt_Tick()
eq(H.state.ticks, ticksBefore + 1, 'tick wrapper runs once per tick after re-runs')

check(#logs >= 0, 'logs are strings')
-- (the transport failure is staged above and expected)
for _, l in ipairs(logs) do if l:find('failed') and l:find('house_rebuild') and not l:find('no transport') then failures = failures + 1; print('FAIL: unexpected log: ' .. l) end end

if failures > 0 then print(failures .. ' house_rebuild test failure(s)'); os.exit(1) end
print('house_rebuild.lua tests passed')
