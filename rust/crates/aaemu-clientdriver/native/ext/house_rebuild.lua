-- Housing rebuild UI (feature "house.rebuild"), ArcheAge 1.2 UI script state, Lua 5.1.
-- Spec: rust/docs/client/CLIENT-EXTENSIONS.md, rust/docs/client/backports/HOUSING.md (10.x flow:
-- maintain window -> "Rebuild" -> rebuild_housing.lua window). Server messages:
--   emit house.rebuild.info {house?}            -> house.rebuild.info {ok, house, template, name, options = {
--        {rebuilding, target, name, skill, skill_name?, labor, materials = {{item, count, have}}, tax, can, reason,
--         reason_message}}, blocked?, blocked_message?, error?}
--   The UI shows `skill_name` (the 1.2 client knows no text for the rebuild skills; no name = no skill
--   line, never the id) and `reason_message` (else the local L.reasons[code] table, else L.cannot).
--   emit house.rebuild.preview {house, rebuilding} / house.rebuild.preview_end {house}
--                                               -> house.rebuild.preview {ok, active, error?}
--   emit house.rebuild.start {house, rebuilding} -> house.rebuild.result {ok, house, target, error?, message?}
-- The 1.2 UI cannot read the house id: the first request omits `house` (the server picks the nearest
-- manageable house) and the id of the reply is used for every later call. Ids that may exceed
-- 16,777,216 stay strings. The server renders the 3D preview; this module only toggles it and always
-- ends it when the window closes, another option is picked or the rebuild is confirmed.
--
-- Entry points: a "Rebuild" button added to the stock house maintain window (housing.maintainWindow,
-- hooked through the stock Update*/Show globals plus the extension tick) and the global
-- AAEmuHouseRebuild_Open() for testing. Re-running this file is safe: logic lives in the persistent
-- global table AAEmuHouseRebuild, windows are rebuilt on every open (EnableHidingIsRemove).

local type, tostring, tonumber, pairs, ipairs, pcall, select = type, tostring, tonumber, pairs, ipairs, pcall, select
local string, table, math = string, table, math

local X = AAEmuExt
if type(X) ~= 'table' or type(X.Module) ~= 'function' then return end

local H = AAEmuHouseRebuild
if type(H) ~= 'table' then H = {}; AAEmuHouseRebuild = H end

local FEATURE = 'house.rebuild'
local WINDOW_ID = 'aaemuHouseRebuild'
local CONFIRM_ID = 'aaemuHouseRebuildConfirm'
local EVENTS_ID = 'aaemuHouseRebuildEvents'
local BUTTON_ID = 'aaemuRebuildButton'
local MAX_OPTIONS = 8          -- rows in the list (more options are cut off, logged once)
local MAX_MATERIALS = 6        -- material rows (as in the 10.x window)
local REPLY_TIMEOUT_TICKS = 20 -- ticks are ~500 ms: give up waiting for a server reply after 10 s

---------------------------------------------------------------------------------------------
-- Strings (single table so the module can be localised later)
---------------------------------------------------------------------------------------------
H.L = {
  button        = 'Rebuild',
  title         = 'Rebuild House',
  loading       = 'Loading rebuild options...',
  waiting       = 'Waiting for the server...',
  noOptions     = 'This house cannot be rebuilt.',
  houseLabel    = 'House: %s',
  unknownHouse  = 'House',
  skill         = 'Skill: %s',
  labor         = 'Labor: %s',
  tax           = 'Tax: %s',
  materials     = 'Materials (have / need)',
  noMaterials   = 'No materials needed.',
  moreMaterials = '(+%d more)',
  itemFallback  = 'Item #%s',
  preview       = 'Preview',
  endPreview    = 'End Preview',
  rebuild       = 'Rebuild',
  close         = 'Close',
  ok            = 'OK',
  cancel        = 'Cancel',
  confirmTitle  = 'Confirm Rebuild',
  confirmText   = 'Rebuild %s into %s?\n\nThe materials are consumed and this cannot be undone.',
  notSupported  = 'This server does not support rebuilding houses.',
  noServer      = 'Could not reach the server.',
  timeout       = 'The server did not answer.',
  noHouse       = 'No house to rebuild was found.',
  unknownError  = 'The rebuild failed (%s).',
  success       = 'The house was rebuilt into %s.',
  successShort  = 'The house was rebuilt.',
  cannot        = 'This rebuild is not possible right now.',
  -- fallback texts for the server's reason / error codes (the server normally sends the message)
  reasons = {
    no_house         = 'No house found.',
    not_owner        = 'Only the owner can rebuild this house.',
    not_finished     = 'The house must be finished first.',
    for_sale         = 'A house that is for sale cannot be rebuilt.',
    tax_due          = 'Pay the house tax first.',
    in_siege         = 'The house cannot be rebuilt during a siege.',
    not_rebuildable  = 'This house cannot be rebuilt.',
    unknown_rebuilding = 'This rebuilding is not available for this house.',
    no_target        = 'The target building is not available.',
    too_far          = 'You are too far from the house.',
    labor            = 'Not enough labor power.',
    materials        = 'Not enough materials.',
    not_available    = 'This server does not support rebuilding houses.',
  },
  gold          = 'g',
  silver        = 's',
  copper        = 'c',
}
local L = H.L

local function log(text)
  if X.Log then pcall(X.Log, 'house_rebuild: ' .. tostring(text)) end
end

---------------------------------------------------------------------------------------------
-- Pure helpers (also used by the tests)
---------------------------------------------------------------------------------------------

-- Copper amount (number or digit string, exact for strings) -> "12g 3s 4c".
function H.FormatMoney(value)
  local s
  if type(value) == 'number' then
    if value ~= value or value < 0 then value = 0 end
    s = string.format('%.0f', math.floor(value))
  elseif type(value) == 'string' and string.find(value, '^%d+$') then
    s = string.gsub(value, '^0+', '')
  else
    s = '0'
  end
  if s == '' then s = '0' end
  local padded = string.rep('0', math.max(0, 5 - string.len(s))) .. s
  local gold = string.sub(padded, 1, -5)
  local silver = tonumber(string.sub(padded, -4, -3)) or 0
  local copper = tonumber(string.sub(padded, -2)) or 0
  gold = string.gsub(gold, '^0+', '')
  local parts = {}
  if gold ~= '' then
    -- thousands separators
    local g = gold
    local grouped = ''
    while string.len(g) > 3 do
      grouped = ',' .. string.sub(g, -3) .. grouped
      g = string.sub(g, 1, -4)
    end
    parts[#parts + 1] = g .. grouped .. L.gold
  end
  if silver > 0 then parts[#parts + 1] = silver .. L.silver end
  if copper > 0 or #parts == 0 then parts[#parts + 1] = copper .. L.copper end
  return table.concat(parts, ' ')
end

local function idString(v)
  if v == nil then return nil end
  if type(v) == 'number' then return string.format('%.0f', v) end
  return tostring(v)
end

-- Human text for a reason / error code: the server's message, else the local table, else nil.
local function codeText(code, message)
  if type(message) == 'string' and message ~= '' then return message end
  if type(code) == 'string' and code ~= '' and type(L.reasons) == 'table' then return L.reasons[code] end
  return nil
end

local function errorText(msg)
  if type(msg) ~= 'table' then return L.unknownError:format('?') end
  local code = msg.error
  if type(msg.message) == 'string' and msg.message ~= '' then return msg.message end
  if code == 'no_house' or code == 'house_not_found' then return L.noHouse end
  local known = codeText(code, nil)
  if known then return known end
  if type(code) == 'string' and code ~= '' then return L.unknownError:format(code) end
  return L.unknownError:format('?')
end

local function itemName(item, fallbackName)
  local info
  if X2Item and X2Item.GetItemInfoByType and type(item) == 'number' then
    local ok, r = pcall(X2Item.GetItemInfoByType, X2Item, item)
    if ok and type(r) == 'table' then info = r end
  end
  local name = info and info.name
  -- the 1.2 client does not know backported 10.x items: it answers a placeholder
  if type(name) == 'string' and name ~= '' and name ~= 'invalid item type' then return name, info end
  if type(fallbackName) == 'string' and fallbackName ~= '' then return fallbackName, nil end
  return L.itemFallback:format(idString(item) or '?'), nil
end

---------------------------------------------------------------------------------------------
-- State machine
---------------------------------------------------------------------------------------------
local S = H.state
if type(S) ~= 'table' then S = {}; H.state = S end

local function resetState()
  S.open = false
  S.house = nil          -- id remembered from house.rebuild.info
  S.houseName = nil
  S.template = nil
  S.options = {}
  S.sel = nil            -- index into S.options
  S.preview = false      -- server confirmed an active preview
  S.previewFor = nil     -- rebuilding id the preview shows
  S.busy = nil           -- nil | 'info' | 'preview' | 'start'
  S.busySince = 0
  S.confirming = false
  S.status = nil         -- { text, kind = 'info' | 'error' | 'warn' }
  S.cutOff = false
  S.keepStatus = nil
end
if S.options == nil then resetState() end
S.ticks = S.ticks or 0

local function ready()
  if type(X.Ready) == 'function' and not X.Ready() then return false end
  if type(X.HasFeature) == 'function' and not X.HasFeature(FEATURE) then return false end
  return true
end
H.Ready = ready

local function selected()
  return S.sel and S.options[S.sel] or nil
end

-- Sends a message; returns true or false (and shows the error).
local function send(channel, tbl)
  local ok, a, b = pcall(X.Emit, channel, tbl)
  if ok and a then return true end
  log('send ' .. channel .. ' failed: ' .. tostring(ok and b or a))
  return false
end

local function setBusy(kind)
  S.busy = kind
  S.busySince = S.ticks
end

-- chat-window system message and the red system notice
function H.NotifyInfo(text)
  if AddMsgToChatWin and CMF_SYSTEM then pcall(AddMsgToChatWin, text, CMF_SYSTEM) end
end
function H.NotifyError(text)
  if AddMessageToSysMsgWindow then pcall(AddMessageToSysMsgWindow, text) end
  if AddMsgToChatWin and CMF_SYSTEM then pcall(AddMsgToChatWin, text, CMF_SYSTEM) end
end

local function setStatus(text, kind)
  if text == nil or text == '' then S.status = nil else S.status = { text = text, kind = kind or 'info' } end
end

local render -- forward

local function endPreview(reason)
  if S.house and (S.preview or S.busy == 'preview') then
    send('house.rebuild.preview_end', { house = S.house })
  end
  S.preview = false
  S.previewFor = nil
  if S.busy == 'preview' then S.busy = nil end
end

-- Closes the window and ends any preview. Safe to call repeatedly and when nothing is open.
function H.Close()
  local was = S.open or S.house ~= nil
  local ui = H.ui
  if S.confirming then H.CloseConfirm() end
  if S.open or S.preview then endPreview('close') end
  S.open = false
  if ui and ui.win then
    H.ui = nil
    pcall(ui.win.Show, ui.win, false)
  end
  if was then resetState() end
end

local function requestInfo(withHouse)
  if not send('house.rebuild.info', withHouse and S.house and { house = S.house } or {}) then
    setStatus(L.noServer, 'error')
    S.busy = nil
    return false
  end
  setBusy('info')
  return true
end

function H.Open()
  if not ready() then
    H.NotifyError(L.notSupported)
    return false
  end
  if S.open then H.Close() end
  resetState()
  S.open = true
  setStatus(L.loading, 'info')
  if not H.BuildWindow() then
    resetState()
    return false
  end
  requestInfo(false)
  render()
  return true
end

function H.Select(index)
  if index == S.sel or not S.options[index] then return end
  -- another option: end a running preview (the user starts it again explicitly)
  if S.preview or S.busy == 'preview' then endPreview('select') end
  S.sel = index
  if not S.busy then setStatus(nil) end
  render()
end

function H.TogglePreview()
  local opt = selected()
  if not opt or not S.house or S.busy then return end
  if S.preview then
    if send('house.rebuild.preview_end', { house = S.house }) then
      S.preview = false
      S.previewFor = nil
      setBusy('preview')
    else
      setStatus(L.noServer, 'error')
    end
  else
    if send('house.rebuild.preview', { house = S.house, rebuilding = opt.rebuilding }) then
      S.previewFor = opt.rebuilding
      setBusy('preview')
      setStatus(L.waiting, 'info')
    else
      setStatus(L.noServer, 'error')
    end
  end
  render()
end

function H.AskRebuild()
  local opt = selected()
  if not opt or not S.house or S.busy or not opt.can then return end
  S.confirming = true
  H.BuildConfirm(opt)
  render()
end

function H.CloseConfirm()
  local ui = H.confirmUi
  S.confirming = false
  H.confirmUi = nil
  if ui and ui.win then pcall(ui.win.Show, ui.win, false) end
  render()
end

function H.ConfirmRebuild()
  local opt = selected()
  local house = S.house
  H.CloseConfirm()
  if not opt or not house or S.busy or not opt.can then return end
  if S.preview or S.busy == 'preview' then endPreview('confirm') end
  if send('house.rebuild.start', { house = house, rebuilding = opt.rebuilding }) then
    setBusy('start')
    setStatus(L.waiting, 'info')
  else
    setStatus(L.noServer, 'error')
  end
  render()
end

-- ---- server replies -------------------------------------------------------------------------

function H.OnInfo(msg)
  if not S.open then return end
  if S.busy == 'info' then S.busy = nil end
  if msg.ok == false then
    local text = errorText(msg)
    H.NotifyError(text)
    if not S.house then H.Close(); return end
    setStatus(text, 'error')
    render()
    return
  end
  local keepKey = selected() and selected().rebuilding
  if msg.house ~= nil then S.house = idString(msg.house) end
  S.houseName = type(msg.name) == 'string' and msg.name or nil
  S.template = msg.template
  S.options = {}
  S.cutOff = false
  if type(msg.options) == 'table' then
    for i = 1, #msg.options do
      local o = msg.options[i]
      if type(o) == 'table' then
        if #S.options >= MAX_OPTIONS then S.cutOff = true; break end
        S.options[#S.options + 1] = o
      end
    end
  end
  if S.cutOff then log('more than ' .. MAX_OPTIONS .. ' rebuild options, extra options are not shown') end
  S.sel = nil
  for i = 1, #S.options do
    if keepKey ~= nil and S.options[i].rebuilding == keepKey then S.sel = i end
  end
  if not S.sel and #S.options > 0 then S.sel = 1 end
  if S.preview and not (S.sel and S.options[S.sel].rebuilding == S.previewFor) then
    endPreview('refresh')
  end
  if #S.options == 0 then setStatus(L.noOptions, 'warn')
  elseif S.keepStatus then S.keepStatus = nil   -- a failed start explains itself until the next action
  else setStatus(nil) end
  render()
end

function H.OnPreview(msg)
  if S.busy == 'preview' then S.busy = nil end
  if msg.ok == false then
    S.preview = false
    S.previewFor = nil
    if S.open then
      local text = errorText(msg)
      H.NotifyError(text)
      setStatus(text, 'error')
    end
  else
    S.preview = msg.active == true
    if not S.preview then S.previewFor = nil end
    if S.open then setStatus(nil) end
  end
  render()
end

function H.OnResult(msg)
  if S.busy == 'start' then S.busy = nil end
  if msg.ok == false then
    local text = errorText(msg)
    H.NotifyError(text)
    if S.open then
      setStatus(text, 'error')
      -- materials / eligibility may have changed: refresh the options (keeping the error visible)
      if S.house and requestInfo(true) then S.keepStatus = true end
    end
    render()
    return
  end
  local target = type(msg.target) == 'string' and msg.target or nil
  local text = (type(msg.message) == 'string' and msg.message ~= '') and msg.message
    or (target and L.success:format(target)) or L.successShort
  H.NotifyInfo(text)
  H.Close()
end

---------------------------------------------------------------------------------------------
-- Widgets
---------------------------------------------------------------------------------------------
local COLOR = {
  text   = { 0.25, 0.17, 0.07, 1 },
  title  = { 0.34, 0.24, 0.09, 1 },
  good   = { 0.04, 0.61, 0.14, 1 },
  bad    = { 0.76, 0.24, 0.21, 1 },
  warn   = { 0.75, 0.45, 0.05, 1 },
  select = { 0.55, 0.32, 0.05, 0.30 },
}

local function paint(widget, c)
  if widget and widget.style and widget.style.SetColor then pcall(widget.style.SetColor, widget.style, c[1], c[2], c[3], c[4]) end
end

local function label(parent, id, x, y, w, h, size, color, align)
  local lb = parent:CreateChildWidget('label', id, 0, true)
  lb:SetExtent(w, h)
  lb:AddAnchor('TOPLEFT', parent, x, y)
  if lb.style then
    if lb.style.SetFontSize then lb.style:SetFontSize(size or (FONT_SIZE and FONT_SIZE.MIDDLE) or 13) end
    if lb.style.SetAlign then lb.style:SetAlign(align or ALIGN_LEFT or 0) end
    if lb.style.SetEllipsis then pcall(lb.style.SetEllipsis, lb.style, true) end
  end
  paint(lb, color or COLOR.text)
  lb:SetText('')
  return lb
end

-- Wrapping text: a 'textbox' widget. It takes colour from inline |cFFRRGGBB escapes (a label would
-- print them literally and a textbox ignores style:SetColor).
local HEX = { text = '3F2B12', title = '573C17', good = '0B9C24', bad = 'C23D36', warn = 'BF730D' }

local function textbox(parent, id, x, y, w, h)
  local tb = parent:CreateChildWidget('textbox', id, 0, true)
  tb:SetExtent(w, h)
  tb:AddAnchor('TOPLEFT', parent, x, y)
  if tb.SetAutoWordwrap then pcall(tb.SetAutoWordwrap, tb, true) end
  tb:SetText('')
  return tb
end

local function setRich(tb, text, hexKey)
  if not tb then return end
  if text == nil or text == '' then tb:SetText(''); return end
  local safe = string.gsub(tostring(text), '|', '/')   -- no stray colour escapes from server text
  tb:SetText('|cFF' .. (HEX[hexKey] or HEX.text) .. safe .. '|r')
end

local function button(parent, id, text, x, y, w, h, anchor, click)
  local b = parent:CreateChildWidget('button', id, 0, true)
  if ApplyButtonSkin and BUTTON_BASIC then pcall(ApplyButtonSkin, b, BUTTON_BASIC.DEFAULT) end
  b:SetText(text)
  b:SetExtent(w, h)
  b:AddAnchor(anchor or 'TOPLEFT', parent, x, y)
  b:SetHandler('OnClick', function() click() end)
  return b
end

local WIN_W, WIN_H = 640, 530
local MARGIN = 34              -- the parchment frame has ~26 px borders
local LEFT_X, LEFT_W, ROW_H, ROW_Y = MARGIN, 190, 30, 84
local RIGHT_X = 244
local RIGHT_W = WIN_W - RIGHT_X - MARGIN

function H.BuildWindow()
  local ok, ui = pcall(function()
    local win = CreateWindow(WINDOW_ID, 'UIParent')
    win:SetExtent(WIN_W, WIN_H)
    local sideMargin = 20
    if GetWindowMargin then sideMargin = (GetWindowMargin()) or sideMargin end
    win:AddAnchor('RIGHT', 'UIParent', -sideMargin, 0)
    win:SetTitle(L.title)
    win:EnableHidingIsRemove(true)
    if win.SetCloseOnEscape then win:SetCloseOnEscape(true) end
    local ui = { win = win, rows = {}, matIcon = {}, matName = {}, matCount = {} }

    ui.house = label(win, 'house', LEFT_X, 52, WIN_W - 2 * MARGIN, 20, FONT_SIZE and FONT_SIZE.LARGE, COLOR.title)

    for i = 1, MAX_OPTIONS do
      local idx = i
      local row = button(win, 'opt' .. i, '', LEFT_X, ROW_Y + (i - 1) * ROW_H, LEFT_W, ROW_H - 2, 'TOPLEFT',
        function() H.Select(idx) end)
      row:Show(false)
      if row.CreateColorDrawable then
        local c = COLOR.select
        local mark = row:CreateColorDrawable(c[1], c[2], c[3], c[4], 'background')
        mark:AddAnchor('TOPLEFT', row, 0, 0)
        mark:AddAnchor('BOTTOMRIGHT', row, 0, 0)
        mark:SetVisible(false)
        row.aaemuMark = mark
      end
      ui.rows[i] = row
    end

    ui.target = label(win, 'target', RIGHT_X, 84, RIGHT_W, 24, FONT_SIZE and FONT_SIZE.XLARGE, COLOR.title)
    ui.skill = label(win, 'skill', RIGHT_X, 114, RIGHT_W, 20)
    ui.labor = label(win, 'labor', RIGHT_X, 136, RIGHT_W, 20)
    ui.tax = label(win, 'tax', RIGHT_X, 158, RIGHT_W, 20)
    ui.matTitle = label(win, 'matTitle', RIGHT_X, 190, RIGHT_W, 20, FONT_SIZE and FONT_SIZE.MIDDLE, COLOR.title)
    for i = 1, MAX_MATERIALS do
      local y = 214 + (i - 1) * 34
      if CreateItemIconButton then
        local icon = CreateItemIconButton('aaemuRebuildMat' .. i, win)
        icon:SetExtent(30, 30)
        icon:AddAnchor('TOPLEFT', win, RIGHT_X, y)
        if icon.Init then pcall(icon.Init, icon) end
        icon:Show(false)
        ui.matIcon[i] = icon
      end
      ui.matName[i] = label(win, 'matName' .. i, RIGHT_X + 38, y + 5, RIGHT_W - 38 - 110, 20)
      ui.matCount[i] = label(win, 'matCount' .. i, RIGHT_X + RIGHT_W - 108, y + 5, 108, 20, nil, COLOR.text, ALIGN_RIGHT)
    end
    ui.more = label(win, 'more', RIGHT_X, 214 + MAX_MATERIALS * 34, RIGHT_W, 20)

    ui.status = textbox(win, 'status', LEFT_X, WIN_H - 106, WIN_W - 2 * MARGIN, 52)

    local by = WIN_H - 52
    ui.btnPreview = button(win, 'btnPreview', L.preview, WIN_W - MARGIN - 3 * 100 - 2 * 8, by, 100, 33, 'TOPLEFT',
      function() H.TogglePreview() end)
    ui.btnRebuild = button(win, 'btnRebuild', L.rebuild, WIN_W - MARGIN - 2 * 100 - 8, by, 100, 33, 'TOPLEFT',
      function() H.AskRebuild() end)
    ui.btnClose = button(win, 'btnClose', L.close, WIN_W - MARGIN - 100, by, 100, 33, 'TOPLEFT',
      function() H.Close() end)

    win:SetHandler('OnHide', function() H.OnWindowHidden(win) end)
    win:Show(true)
    return ui
  end)
  if not ok then
    log('window build failed: ' .. tostring(ui))
    H.ui = nil
    return false
  end
  H.ui = ui
  return true
end

-- Window hidden by the title-bar X / Escape (H.Close hides it itself and clears H.ui first).
function H.OnWindowHidden(win)
  if H.ui and H.ui.win == win then
    H.ui = nil
    H.Close()
  end
end

function H.BuildConfirm(opt)
  if H.confirmUi then H.CloseConfirm(); S.confirming = true end
  local ok, ui = pcall(function()
    local win = CreateWindow(CONFIRM_ID, 'UIParent')
    win:SetExtent(400, 220)
    win:AddAnchor('CENTER', 'UIParent', 0, 0)
    win:SetTitle(L.confirmTitle)
    win:EnableHidingIsRemove(true)
    if win.SetCloseOnEscape then win:SetCloseOnEscape(true) end
    local ui = { win = win }
    local text = textbox(win, 'text', MARGIN, 64, 400 - 2 * MARGIN, 100)
    local houseName = S.houseName or L.unknownHouse
    local targetName = (type(opt.name) == 'string' and opt.name) or (type(opt.target) == 'string' and opt.target) or '?'
    setRich(text, L.confirmText:format(houseName, targetName), 'text')
    ui.text = text
    ui.btnOk = button(win, 'btnOk', L.ok, 400 - MARGIN - 2 * 100 - 8, 220 - 54, 100, 33, 'TOPLEFT', function() H.ConfirmRebuild() end)
    ui.btnCancel = button(win, 'btnCancel', L.cancel, 400 - MARGIN - 100, 220 - 54, 100, 33, 'TOPLEFT', function() H.CloseConfirm() end)
    win:SetHandler('OnHide', function()
      if H.confirmUi and H.confirmUi.win == win then H.confirmUi = nil; S.confirming = false end
    end)
    win:Show(true)
    if win.Raise then win:Raise() end
    return ui
  end)
  if not ok then
    log('confirm build failed: ' .. tostring(ui))
    S.confirming = false
    return
  end
  H.confirmUi = ui
end

local function setText(widget, text)
  if widget then widget:SetText(text or '') end
end

local function show(widget, visible)
  if widget then widget:Show(visible and true or false) end
end

local function enable(widget, on)
  if widget and widget.Enable then widget:Enable(on and true or false) end
end

render = function()
  local ui = H.ui
  if not ui then return end
  local ok, err = pcall(function()
    setText(ui.house, L.houseLabel:format(S.houseName or L.unknownHouse))
    local idle = not S.busy and not S.confirming

    for i = 1, MAX_OPTIONS do
      local row, opt = ui.rows[i], S.options[i]
      if opt then
        setText(row, (type(opt.name) == 'string' and opt.name) or (type(opt.target) == 'string' and opt.target) or ('#' .. i))
        show(row, true)
        enable(row, idle)
        paint(row, i == S.sel and COLOR.title or COLOR.text)
        if row.aaemuMark then row.aaemuMark:SetVisible(i == S.sel) end
      else
        show(row, false)
      end
    end

    local opt = selected()
    if opt then
      setText(ui.target, (type(opt.name) == 'string' and opt.name) or (type(opt.target) == 'string' and opt.target) or '')
      -- only a localized name; the bare skill id means nothing to the player (the client lacks its text)
      if type(opt.skill_name) == 'string' and opt.skill_name ~= '' then setText(ui.skill, L.skill:format(opt.skill_name)) else setText(ui.skill, '') end
      if opt.labor ~= nil then setText(ui.labor, L.labor:format(tostring(opt.labor))) else setText(ui.labor, '') end
      if opt.tax ~= nil then setText(ui.tax, L.tax:format(H.FormatMoney(opt.tax))) else setText(ui.tax, '') end
      setText(ui.matTitle, L.materials)
      local mats = type(opt.materials) == 'table' and opt.materials or {}
      local shown = 0
      for i = 1, MAX_MATERIALS do
        local m = mats[i]
        if type(m) == 'table' then
          shown = shown + 1
          local need, have = tonumber(m.count) or 0, tonumber(m.have) or 0
          local name, info = itemName(m.item, m.name)
          setText(ui.matName[i], name)
          setText(ui.matCount[i], string.format('%d / %d', have, need))
          paint(ui.matCount[i], have >= need and COLOR.good or COLOR.bad)
          local icon = ui.matIcon[i]
          if icon then
            if info and icon.SetItemInfo then pcall(icon.SetItemInfo, icon, info) end
            show(icon, info ~= nil)
          end
        else
          setText(ui.matName[i], '')
          setText(ui.matCount[i], '')
          show(ui.matIcon[i], false)
        end
      end
      if #mats == 0 then setText(ui.matName[1], L.noMaterials) end
      setText(ui.more, #mats > MAX_MATERIALS and L.moreMaterials:format(#mats - MAX_MATERIALS) or '')
    else
      setText(ui.target, '')
      setText(ui.skill, '')
      setText(ui.labor, '')
      setText(ui.tax, '')
      setText(ui.matTitle, '')
      setText(ui.more, '')
      for i = 1, MAX_MATERIALS do
        setText(ui.matName[i], '')
        setText(ui.matCount[i], '')
        show(ui.matIcon[i], false)
      end
    end

    -- status line: an explicit message wins, then why the option cannot be used
    local text, kind
    if S.status then
      text, kind = S.status.text, S.status.kind
    elseif opt and opt.can == false then
      text, kind = codeText(opt.reason, opt.reason_message) or L.cannot, 'warn'
    end
    setRich(ui.status, text, kind == 'error' and 'bad' or kind == 'warn' and 'warn' or 'text')

    setText(ui.btnPreview, S.preview and L.endPreview or L.preview)
    enable(ui.btnPreview, opt ~= nil and S.house ~= nil and not S.busy and not S.confirming)
    enable(ui.btnRebuild, opt ~= nil and S.house ~= nil and idle and opt.can == true)
    enable(ui.btnClose, true)
  end)
  if not ok then log('render failed: ' .. tostring(err)) end
end
H.Render = render

---------------------------------------------------------------------------------------------
-- Maintain window integration
---------------------------------------------------------------------------------------------
local function maintainWindow()
  if type(housing) == 'table' and type(housing.maintainWindow) == 'table' then return housing.maintainWindow end
  return nil
end

local function isMyHouse(w)
  if w.aaemuMine then return true end
  if X2House and X2House.IsMyHouse then
    local ok, r = pcall(X2House.IsMyHouse, X2House)
    return ok and r == true
  end
  return false
end

-- Adds (once per window object) the Rebuild button next to Confirm and keeps its visibility current.
function H.AttachButton()
  local w = maintainWindow()
  if not w or not w.CreateChildWidget then return end
  local mine = isMyHouse(w)
  local btn = w.aaemuRebuildButton
  if not btn then
    if not mine then return end
    local ok, created = pcall(function()
      local b = w:CreateChildWidget('button', BUTTON_ID, 0, true)
      if ApplyButtonSkin and BUTTON_BASIC then ApplyButtonSkin(b, BUTTON_BASIC.DEFAULT) end
      b:SetText(L.button)
      b:SetExtent(84, 33)
      if type(w.okButton) == 'table' then
        b:AddAnchor('RIGHT', w.okButton, 'LEFT', -6, 0)
      else
        b:AddAnchor('BOTTOMRIGHT', w, -120, -20)
      end
      b:SetHandler('OnClick', function() H.Open() end)
      return b
    end)
    if not ok then log('maintain window button failed: ' .. tostring(created)); return end
    btn = created
    w.aaemuRebuildButton = btn
  end
  local want = mine and ready()
  if btn.aaemuShown ~= want then
    btn.aaemuShown = want
    pcall(btn.Show, btn, want)
  end
end

-- Wraps a stock global once; `after` runs after the stock function (errors contained).
H.orig = H.orig or {}
H.wrappers = H.wrappers or {}
local function hook(name, after)
  local cur = _G[name]
  if type(cur) ~= 'function' then return end
  if H.wrappers[name] == cur then return end   -- already ours
  H.orig[name] = cur
  local function wrapper(...)
    local a, b, c, d = H.orig[name](...)
    local ok, err = pcall(function()
      if after then after() end
      H.AttachButton()
    end)
    if not ok then log('hook ' .. name .. ' failed: ' .. tostring(err)) end
    return a, b, c, d
  end
  H.wrappers[name] = wrapper
  _G[name] = wrapper
end

function H.InstallHooks()
  local function mine() local w = maintainWindow(); if w then w.aaemuMine = true end end
  hook('ShowHousingMainTainWindow')
  hook('UpdateMyHouseDefaultInfo', mine)
  hook('UpdateMyHouseTaxInfo', mine)
  hook('UpdateMaintainWindowHeight')
  hook('UpdateHouseDefaultInfo')   -- other people's houses: no button, but re-checks stale state
end

-- UI events: close on world change / leaving the house interaction (as the 10.x window does).
function H.InstallEvents()
  if H.eventsWindow or not CreateEmptyWindow or not RegistUIEvent then return end
  local ok, err = pcall(function()
    local w = CreateEmptyWindow(EVENTS_ID, 'UIParent')
    local events = {}
    local function close() H.Close() end
    events.ENTERED_WORLD = close
    events.HOUSE_INTERACTION_START = function() H.Close() end
    events.HOUSE_INTERACTION_END = close
    w:SetHandler('OnEvent', function(this, event, ...)
      local f = events[event]
      if f then local ok2, e2 = pcall(f, ...); if not ok2 then log('event ' .. tostring(event) .. ': ' .. tostring(e2)) end end
    end)
    RegistUIEvent(w, events)
    H.eventsWindow = w
  end)
  if not ok then log('events failed: ' .. tostring(err)) end
end

-- Periodic work (~500 ms): reply timeout, world change, late button attach.
function H.Tick()
  S.ticks = S.ticks + 1
  if not H.eventsWindow then H.InstallEvents() end
  if S.busy and S.ticks - S.busySince > REPLY_TIMEOUT_TICKS then
    local kind = S.busy
    S.busy = nil
    if kind == 'preview' then S.preview = false; S.previewFor = nil end
    if S.open then setStatus(L.timeout, 'error'); render() end
  end
  if X._playerKey then
    local key = X._playerKey()
    if H.playerKey ~= nil and key ~= H.playerKey and (S.open or S.house) then H.Close() end
    H.playerKey = key
  end
  H.InstallHooks()   -- cheap; re-wraps if the stock functions were replaced
  if maintainWindow() then H.AttachButton() end
end

if H.tickWrapper ~= AAEmuExt_Tick and type(AAEmuExt_Tick) == 'function' then
  H.origTick = AAEmuExt_Tick
  local function tick(...)
    H.origTick(...)
    local ok, err = pcall(H.Tick)
    if not ok then log('tick failed: ' .. tostring(err)) end
  end
  H.tickWrapper = tick
  AAEmuExt_Tick = tick
end

function AAEmuHouseRebuild_Open() return H.Open() end

---------------------------------------------------------------------------------------------
-- Module registration (channel handlers dispatch through H so a re-run replaces the logic)
---------------------------------------------------------------------------------------------
X.Module{
  name = 'house_rebuild',
  features = { FEATURE },
  init = function(ext)
    ext.On('house.rebuild.info', function(msg) H.OnInfo(msg) end)
    ext.On('house.rebuild.preview', function(msg) H.OnPreview(msg) end)
    ext.On('house.rebuild.result', function(msg) H.OnResult(msg) end)
  end,
}

pcall(H.InstallHooks)
pcall(H.InstallEvents)
H.loaded = true
