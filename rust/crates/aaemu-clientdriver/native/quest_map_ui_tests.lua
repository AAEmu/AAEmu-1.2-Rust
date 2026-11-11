-- Live checks for quest_map_ui.lua. Run inside the real client's UI Lua state while a
-- character is in the world:
--   Get-Content quest_map_ui_tests.lua -Raw | aaemu-clientdriver lua --pid PID -t ui -
-- Restores the previous option value and sends the matching /questmap command at the end.
assert(AAEmuQuestMapUiInstalled ~= nil, 'quest map UI is not installed')
assert(X2:IsEnteredWorld(), 'a character must be in the world')

ToggleOptionFrame(true)
assert(optionFrame ~= nil, 'options window was not created')
local frame = optionFrame.contentWindow.pageWindow.subFrames[5]
assert(frame ~= nil and frame.aaemuQuestMapControl ~= nil,
       'Game Info row is missing')
local slider = frame.aaemuQuestMapControl
local minValue, maxValue = slider:GetMinMaxValues()
assert(minValue == 0 and maxValue == 3, 'slider must have four steps')

local original = GetOptionItemValue('QuestMapGivenDistance')
assert(original ~= nil, 'option item is missing')
slider:SetValue(3, false)
slider:Save()
assert(GetOptionItemValue('QuestMapGivenDistance') == 3, 'Unlimited was not stored')
assert(AAEmuQuestMapSlot == 3, 'Unlimited was not sent')

slider:SetValue(original, false)
slider:Save()
assert(GetOptionItemValue('QuestMapGivenDistance') == original, 'value was not restored')
ToggleOptionFrame(false)
return 'ok'
