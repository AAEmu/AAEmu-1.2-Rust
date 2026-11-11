-- "Quest Details Map Display" (Options > Game Settings > Game Info).
-- 1.2 has no equivalent client option: the injected driver adds the row, stores its value in
-- the client's own option storage (OL_CHARACTER) and asks the server to widen the given-quest
-- markers with a level-0 chat command. Tooltip and steps follow the ArcheAge Classic 3.5
-- option (option_map_given_quest_distance, 100/200/300/unlimited).
if not X2Option or not RegisterOptionItem or not CreateGameInfoOptionFrame or
   not CreateOptionSubFrame or not UIParent or OL_CHARACTER == nil then
  return nil
end

local installed = AAEmuQuestMapUiInstalled
if type(installed) == 'table' and installed.create == CreateGameInfoOptionFrame then
  return 'ready'
end

local OPTION_ID = 'QuestMapGivenDistance'
local LABELS = { '100 m', '200 m', '300 m', 'Unlimited' }
local METERS = { [0] = 100, [1] = 200, [2] = 300, [3] = 0 }
local TOOLTIP = 'Sets the range for which quest targets will be displayed on the map.\n' ..
                'Selecting at least 200m also displays the location of Event NPCs.'

local function sendRange(slot)
  local value = math.floor(tonumber(slot) or 0)
  if value < 0 or value > 3 then value = 0 end
  AAEmuQuestMapSlot = value
  local entered, inWorld = pcall(function() return X2:IsEnteredWorld() end)
  if not entered or not inWorld or X2Chat == nil or CHAT_SAY == nil then return end
  local meters = METERS[value]
  pcall(function() X2Chat:SendChatMessage(CHAT_SAY, '/questmap ' .. tostring(meters), '') end)
end

RegisterOptionItem({
  { id = OPTION_ID, default = 0, saveLevel = OL_CHARACTER }
})

local function addRow(frame)
  if frame == nil or frame.aaemuQuestMapControl ~= nil then return end
  local ok, slider = pcall(function()
    return frame:InsertNewOption('sliderbar', 'Quest Details Map Display', LABELS,
                                 OPTION_ID, sendRange)
  end)
  if not ok or slider == nil then return end
  frame.aaemuQuestMapControl = slider
  slider:SetMinMaxValues(0, #LABELS - 1)
  slider:SetValueStep(1)
  slider:SetPageStep(1)
  slider:SetTooltipText(TOOLTIP)
  if ResetScroll ~= nil then ResetScroll(frame, frame.totalHeight) end
  local baseInit = slider.Init
  if baseInit ~= nil then
    function slider:Init()
      baseInit(self)
      sendRange(GetOptionItemValue(OPTION_ID) or 0)
    end
  end
end

local originalCreate = CreateGameInfoOptionFrame
function CreateGameInfoOptionFrame(parent, subFrameIndex)
  local frame = originalCreate(parent, subFrameIndex)
  addRow(frame)
  return frame
end

-- An options window created before this script ran (e.g. a reload while it was open).
if optionFrame ~= nil and optionFrame.contentWindow ~= nil and
   optionFrame.contentWindow.pageWindow ~= nil then
  local subFrames = optionFrame.contentWindow.pageWindow.subFrames
  if subFrames ~= nil and subFrames[5] ~= nil then addRow(subFrames[5]) end
end

-- Re-send the stored range after each world entry (the server keeps only live state).
if AAEmuQuestMapProbe == nil then
  local probe = CreateEmptyWindow('aaemuQuestMapProbe', 'UIParent')
  probe:Show(false)
  probe:RegisterEvent('ENTERED_WORLD')
  probe:SetHandler('OnEvent', function(self, event, ...)
    if event == 'ENTERED_WORLD' then
      sendRange(GetOptionItemValue(OPTION_ID) or 0)
    end
  end)
  AAEmuQuestMapProbe = probe
end

-- The script may be installed after ENTERED_WORLD (login -> world transition): sync now.
local entered, inWorld = pcall(function() return X2:IsEnteredWorld() end)
if entered and inWorld then
  sendRange(GetOptionItemValue(OPTION_ID) or 0)
end

AAEmuQuestMapUiInstalled = { create = CreateGameInfoOptionFrame }
return 'ready'
