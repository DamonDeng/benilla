-- AgentCam: GM spectator toolkit for watching agent characters (up to 20 slots; the ticked,
-- named ones are on the panel and in the auto director's rotation).
-- Lua 5.0 / 1.12 API (no `#`, no string.gmatch, scripts use `this` / `arg1`).
--
-- View modes (what clicking an agent does):
--   view   : target the agent (.appear first if not nearby), then .gm setview   (its eyes)
--   follow : keep your invisible body beside it with repeated .appear          (your camera)
--   watch  : .watch <name>                                                     (server fallback)
-- Also: right-click an agent = teleport only, /ac go <name> = teleport to any player,
-- auto director (cycle agents), far camera, invisibility toggle, screenshot, info overlay.

local DEFAULT_NAMES = {} -- fill in your agents in /ac config (names are saved per account)
local MAX_AGENTS = 20
local DEFAULT_ON = 4 -- slots ticked on a fresh install
local MODES = { "view", "follow", "watch" }
local RETRY_INTERVAL = 0.5 -- seconds between target attempts after .appear
local RETRY_MAX = 12 -- give up after ~6 s
local FOLLOW_CHECK = 2.5 -- follow leash period (> APPEAR_DEBOUNCE)
local LEASH_RANGE = 3 -- CheckInteractDistance index 3 = ~10 yd: closer than this, stay put
local APPEAR_DEBOUNCE = 2 -- seconds; see Appear
local VIEW_SETTLE = 1.0 -- seconds after .appear before .gm setview, so the destination is loaded
local FOLLOW_RANGE = 4 -- CheckInteractDistance index 4 = ~28 yd (the follow range)

BINDING_HEADER_AGENTCAM = "AgentCam"
BINDING_NAME_AGENTCAM_1 = "View agent 1 (panel order)"
BINDING_NAME_AGENTCAM_2 = "View agent 2 (panel order)"
BINDING_NAME_AGENTCAM_3 = "View agent 3 (panel order)"
BINDING_NAME_AGENTCAM_4 = "View agent 4 (panel order)"
BINDING_NAME_AGENTCAM_5 = "View agent 5 (panel order)"
BINDING_NAME_AGENTCAM_NEXT = "View next agent"
BINDING_NAME_AGENTCAM_PREV = "View previous agent"
BINDING_NAME_AGENTCAM_RESET = "Back to my own view"
BINDING_NAME_AGENTCAM_MODE = "Cycle mode (view / follow / watch)"
BINDING_NAME_AGENTCAM_AUTO = "Toggle auto director"
BINDING_NAME_AGENTCAM_GOTARGET = "Teleport to target player"
BINDING_NAME_AGENTCAM_CAM = "Toggle far camera"
BINDING_NAME_AGENTCAM_INVIS = "Toggle GM invisibility"
BINDING_NAME_AGENTCAM_SHOT = "Screenshot with caption"

local db -- AgentCamDB after load
local current -- slot (1..MAX_AGENTS) of the agent being viewed, or nil
local active -- what is engaged on the server/client: "view" | "watch" | "follow" | nil
local pending -- { index, name, action, elapsed, tries } while waiting for .appear to land
local following -- { index, name, elapsed, lastAppear } while in follow mode
local invisible -- nil = not yet set this session
local autoElapsed = 0
local buttons, modeButtons = {}, {}
local frame, info, tools

local function Print(msg)
	DEFAULT_CHAT_FRAME:AddMessage("|cff33ff99AgentCam|r: " .. msg)
end

local function Cmd(line)
	SendChatMessage(line, "SAY")
end

-- True (and says so) when the addon is switched off in its settings.
local function Off()
	if db and db.enabled == false then
		Print("disabled - tick Enable in /ac config")
		return true
	end
end

local function Lower(s)
	return s and string.lower(s)
end

-- A slot counts when it is ticked and has a name: an empty ticked slot is as good as unticked.
local function IsActive(slot)
	return db.on[slot] and db.names[slot] and db.names[slot] ~= ""
end

-- The active slots in slot order: the panel's rows, the keys' and /ac N's numbering, the rotation.
local function ActiveSlots()
	local list = {}
	for slot = 1, MAX_AGENTS do
		if IsActive(slot) then table.insert(list, slot) end
	end
	return list
end

-- The slot behind panel position n, or nil.
local function NthActive(n)
	return ActiveSlots()[n]
end

-- Target `name` exactly; true when the target is now that unit.
local function TryTarget(name)
	if Lower(UnitName("target")) == Lower(name) then return true end
	TargetByName(name, true)
	return Lower(UnitName("target")) == Lower(name)
end

local function InFollowRange()
	return UnitExists("target") and CheckInteractDistance("target", FOLLOW_RANGE)
end

-- One .appear per name per APPEAR_DEBOUNCE seconds. Two back-to-back teleports to the same spot
-- (a double click, a key repeat, a click queued under the client's frozen teleport frame) land
-- in one tick, and the second, a zero-distance snap, cancels the client's destination reload:
-- the camera then stands in an unloaded world (sky and water, units, no terrain).
local lastAppearName, lastAppearAt = nil, -100
local function Appear(name)
	local now = GetTime()
	if Lower(name) == lastAppearName and now - lastAppearAt < APPEAR_DEBOUNCE then return end
	lastAppearName, lastAppearAt = Lower(name), now
	Cmd(".appear " .. name)
end

local function EnsureInvisible()
	if invisible == nil then
		Cmd(".gm visible off")
		invisible = true
	end
end

------------------------------------------------------------------------------- UI refresh

local LayoutPanel -- defined with the frame

local function UpdateButtons()
	if not frame then return end
	LayoutPanel()
	for _, b in pairs(modeButtons) do
		b:SetOn(b.mode == db.mode)
	end
	frame.autoButton:SetOn(db.auto)
	frame.camButton:SetOn(db.farCam)
	frame.invisButton:SetOn(invisible ~= false)
	local status = "mode: " .. db.mode
	if following then status = status .. " | following" end
	if db.auto then status = status .. " | auto " .. db.autoInterval .. "s" end
	frame.statusText:SetText(status)
	if AgentCamConfig and AgentCamConfig:IsShown() then AgentCam_RefreshConfig() end
end

------------------------------------------------------------------------------- engage / release

-- Undo whatever is engaged, so the next action starts from your own body and camera.
local function Release()
	pending = nil
	if active == "view" then
		Cmd(".gm resetview")
	elseif active == "watch" then
		Cmd(".unwatch")
	end
	following = nil
	active = nil
end

local function StartView(i, name)
	Cmd(".gm setview")
	active, current, pending = "view", i, nil
	UpdateButtons()
	Print("viewing " .. name)
end

-- Follow is a leash, not the client's /follow: GM mode re-templates you to faction 35, which
-- cannot cooperate with the agents, so FollowByName is refused (ERR_INVALID_FOLLOW_TARGET).
-- Every FOLLOW_CHECK seconds, when the agent is more than ~10 yd away, .appear beside it again.
-- Each hop is a few yards (< 30), so the client does not freeze to reload; the camera stays yours.
local function StartFollow(i, name)
	active, current, pending = "follow", i, nil
	following = { index = i, name = name, elapsed = 0 }
	UpdateButtons()
	Print("following " .. name .. " (free camera; click Me to stop)")
end

function AgentCam_Switch(i)
	if Off() then return end
	local name = db.names[i]
	if not name or name == "" then
		Print("slot " .. i .. " has no name; set it in /ac config")
		return
	end
	if pending and pending.index == i then return end -- already on its way there
	Release()
	EnsureInvisible()
	autoElapsed = 0
	if db.mode == "watch" then
		Cmd(".watch " .. name)
		active, current = "watch", i
		UpdateButtons()
		Print("watching " .. name)
	elseif db.mode == "follow" then
		if TryTarget(name) and InFollowRange() then
			StartFollow(i, name)
		else
			Appear(name)
			pending = { index = i, name = name, action = "follow", elapsed = 0, tries = 0 }
		end
	else
		-- Only when it is next to your body: the client can hold a stale copy of a unit thousands
		-- of yards away, and viewing it from here shows a world that was never streamed in.
		if TryTarget(name) and InFollowRange() then
			StartView(i, name)
		else
			Appear(name)
			pending = { index = i, name = name, action = "view", elapsed = 0, tries = 0 }
		end
	end
end

function AgentCam_Cycle(step)
	if Off() then return end
	local list = ActiveSlots()
	local n = table.getn(list)
	if n == 0 then
		Print("no agents ticked - pick some in /ac config")
		return
	end
	local pos = 0
	for k = 1, n do
		if list[k] == current then pos = k end
	end
	if pos == 0 and step < 0 then pos = 1 end -- from nothing, prev starts at the last
	AgentCam_Switch(list[math.mod(pos - 1 + step + n, n) + 1])
end

-- Panel position n (what Alt-n and /ac n mean).
function AgentCam_SwitchNth(n)
	if Off() then return end
	local slot = NthActive(n)
	if not slot then
		Print("no agent #" .. n .. " on the panel")
		return
	end
	AgentCam_Switch(slot)
end

function AgentCam_Reset()
	if Off() then return end
	Release()
	current = nil
	UpdateButtons()
	Print("back to my own view")
end

function AgentCam_SetMode(m)
	if Off() then return end
	if m == "gm" then m = "view" end
	if m ~= "view" and m ~= "follow" and m ~= "watch" then
		Print("modes: view, follow, watch (now " .. db.mode .. ")")
		return
	end
	local again = current
	db.mode = m
	Print("mode: " .. m)
	if again then AgentCam_Switch(again) else UpdateButtons() end
end

function AgentCam_CycleMode()
	if Off() then return end
	for k = 1, 3 do
		if MODES[k] == db.mode then
			AgentCam_SetMode(MODES[math.mod(k, 3) + 1])
			return
		end
	end
	AgentCam_SetMode("view")
end

------------------------------------------------------------------------------- teleport

-- Teleport your body to a player: agent index, any player name, or (no arg) your target.
function AgentCam_Go(who)
	if Off() then return end
	local name = who
	local n = tonumber(who or "")
	if n then
		local slot = NthActive(n)
		if not slot then
			Print("no agent #" .. n .. " on the panel")
			return
		end
		name = db.names[slot]
	elseif not who or who == "" then
		if not UnitExists("target") or not UnitIsPlayer("target") then
			Print("target a player first, or /ac go <name>")
			return
		end
		name = UnitName("target")
	end
	if following then Release() end -- the jump would break the follow anyway
	EnsureInvisible()
	Appear(name)
	Print("teleporting to " .. name)
end

------------------------------------------------------------------------------- tools

function AgentCam_ToggleAuto(secs)
	if Off() then return end
	secs = tonumber(secs or "")
	if secs then
		db.autoInterval = math.max(5, math.floor(secs))
		db.auto = true
	else
		db.auto = not db.auto
	end
	autoElapsed = 0
	UpdateButtons()
	Print(db.auto and ("auto director on, every " .. db.autoInterval .. "s (holds while the agent fights)")
		or "auto director off")
end

local function ApplyCamera()
	if db.farCam then
		SetCVar("cameraDistanceMax", "25")
		SetCVar("cameraDistanceMaxFactor", "2") -- 25 x 2 = 50 yd, the engine cap
		CameraZoomOut(50)
	else
		SetCVar("cameraDistanceMax", "15")
		SetCVar("cameraDistanceMaxFactor", "1")
	end
end

function AgentCam_ToggleCamera()
	if Off() then return end
	db.farCam = not db.farCam
	ApplyCamera()
	UpdateButtons()
	Print(db.farCam and "far camera (50 yd)" or "normal camera")
end

function AgentCam_ToggleInvis()
	if Off() then return end
	if invisible == false then
		Cmd(".gm visible off")
		invisible = true
	else
		Cmd(".gm visible on")
		invisible = false
	end
	UpdateButtons()
	Print(invisible and "you are invisible" or "you are VISIBLE to players")
end

function AgentCam_Screenshot()
	if Off() then return end
	local who = current and db.names[current] or UnitName("player")
	Print(date("%Y-%m-%d %H:%M:%S") .. " | " .. who .. " | " .. (GetZoneText() or "") ..
		" / " .. (GetMinimapZoneText() or ""))
	Screenshot()
end

------------------------------------------------------------------------------- per-frame work

local function TickPending()
	pending.elapsed = pending.elapsed + arg1
	if pending.elapsed < RETRY_INTERVAL then return end
	pending.elapsed = 0
	pending.tries = pending.tries + 1
	if pending.tries * RETRY_INTERVAL < VIEW_SETTLE then return end
	if TryTarget(pending.name) and InFollowRange() then
		if pending.action == "view" then
			StartView(pending.index, pending.name)
		else
			StartFollow(pending.index, pending.name)
		end
	elseif pending.tries >= RETRY_MAX then
		Print("could not find " .. pending.name .. " (offline?). Try /ac mode watch")
		pending = nil
	end
end

-- Keep the follow alive: re-follow when close, re-appear when the agent got away (taxi, portal).
local function TickFollow()
	following.elapsed = following.elapsed + arg1
	if following.elapsed < FOLLOW_CHECK then return end
	following.elapsed = 0
	if TryTarget(following.name) and CheckInteractDistance("target", LEASH_RANGE) then return end
	Appear(following.name)
end

local function TickAuto()
	autoElapsed = autoElapsed + arg1
	if autoElapsed < db.autoInterval then return end
	if current and Lower(UnitName("target")) == Lower(db.names[current]) and UnitAffectingCombat("target") then
		autoElapsed = db.autoInterval - 5 -- hold the shot while it fights; look again in 5 s
		return
	end
	AgentCam_Cycle(1)
end

local function Pct(cur, max)
	if not max or max == 0 then return "-" end
	return math.floor(cur / max * 100 + 0.5) .. "%"
end

local function UnitLine(unit)
	local lvl = UnitLevel(unit)
	local s = (UnitName(unit) or "?") .. "  L" .. ((lvl and lvl > 0) and lvl or "??") ..
		"  HP " .. Pct(UnitHealth(unit), UnitHealthMax(unit))
	if UnitManaMax(unit) and UnitManaMax(unit) > 0 then
		s = s .. "  MP " .. Pct(UnitMana(unit), UnitManaMax(unit))
	end
	if UnitIsDead(unit) then s = s .. "  |cffff4040DEAD|r" end
	return s
end

local infoElapsed = 0
local function TickInfo()
	infoElapsed = infoElapsed + arg1
	if infoElapsed < 0.25 then return end
	infoElapsed = 0
	local name = current and db.names[current]
	if not db.info or not name or Lower(UnitName("target")) ~= Lower(name) then
		info:Hide()
		return
	end
	local line1 = UnitLine("target")
	if UnitAffectingCombat("target") then line1 = line1 .. "  |cffff8000[combat]|r" end
	info.line1:SetText(line1)
	if UnitExists("targettarget") and not UnitIsUnit("targettarget", "target") then
		info.line2:SetText("> " .. UnitLine("targettarget"))
	else
		info.line2:SetText("|cff808080no target|r")
	end
	info:Show()
end

local function OnUpdate()
	if db.enabled == false then return end
	if pending then TickPending() end
	if following and not pending then TickFollow() end
	if db.auto then TickAuto() end
	TickInfo()
end

------------------------------------------------------------------------------- frames

local function SavePosition(f, key)
	local point, _, relPoint, x, y = f:GetPoint()
	db[key] = { point = point, relPoint = relPoint, x = x, y = y }
end

local function MakeDraggable(f, key, defPoint, defX, defY)
	f:SetMovable(true)
	f:EnableMouse(true)
	f:RegisterForDrag("LeftButton")
	f:SetScript("OnDragStart", function() this:StartMoving() end)
	f:SetScript("OnDragStop", function() this:StopMovingOrSizing(); SavePosition(this, key) end)
	local p = db[key]
	if p then
		f:SetPoint(p.point, UIParent, p.relPoint, p.x, p.y)
	else
		f:SetPoint(defPoint, UIParent, defPoint, defX, defY)
	end
end

local function Backdrop(f)
	f:SetBackdrop({
		bgFile = "Interface\\Tooltips\\UI-Tooltip-Background",
		edgeFile = "Interface\\Tooltips\\UI-Tooltip-Border",
		tile = true, tileSize = 16, edgeSize = 16,
		insets = { left = 4, right = 4, top = 4, bottom = 4 },
	})
	f:SetBackdropColor(0, 0, 0, 0.7)
end

local function Button(name, width, x, y, text, onClick)
	local b = CreateFrame("Button", name, frame, "UIPanelButtonTemplate")
	b:SetWidth(width)
	b:SetHeight(20)
	b:SetPoint("TOPLEFT", frame, "TOPLEFT", x, y)
	if text then b:SetText(text) end
	b:SetScript("OnClick", onClick)
	return b
end

local ICON = 24
-- Drawn by tools/make_icons.py (64x64 TGA). Re-run it after editing a glyph; /reload picks up a
-- changed file, a new file needs a client restart.
local ICON_DIR = "Interface\\AddOns\\AgentCam\\icons\\"
local ICONS = {}
for _, k in ipairs({ "view", "follow", "watch", "me", "go", "auto", "cam", "invis", "shot", "dismount", "settings" }) do
	ICONS[k] = ICON_DIR .. k
end

-- On: glow (and full colour for a toggle). Off: no glow (a toggle also turns grey).
local function SetOn(b, on)
	if on then b.glow:Show() else b.glow:Hide() end
	if b.toggle then
		b.icon:SetDesaturated(not on)
		if on then b.icon:SetVertexColor(1, 1, 1) else b.icon:SetVertexColor(0.6, 0.6, 0.6) end
	end
end

local function IconButton(name, x, y, texture, title, desc, key, onClick, toggle)
	local b = CreateFrame("Button", name, tools)
	b:SetWidth(ICON)
	b:SetHeight(ICON)
	b:SetPoint("TOPLEFT", tools, "TOPLEFT", x, y)
	b.icon = b:CreateTexture(nil, "ARTWORK")
	b.icon:SetAllPoints(b)
	b.icon:SetTexture(texture)
	b.glow = b:CreateTexture(nil, "OVERLAY")
	b.glow:SetAllPoints(b)
	b.glow:SetTexture("Interface\\Buttons\\CheckButtonHilight")
	b.glow:SetBlendMode("ADD")
	b.glow:Hide()
	b:SetHighlightTexture("Interface\\Buttons\\ButtonHilight-Square")
	b:GetHighlightTexture():SetBlendMode("ADD")
	b.toggle = toggle
	b.SetOn = SetOn
	b:SetScript("OnClick", onClick)
	b:SetScript("OnEnter", function()
		GameTooltip:SetOwner(this, "ANCHOR_LEFT")
		GameTooltip:SetText(title)
		GameTooltip:AddLine(desc, 1, 1, 1, 1)
		if key then GameTooltip:AddLine("Key: " .. key, 0.5, 0.8, 1) end
		GameTooltip:Show()
	end)
	b:SetScript("OnLeave", function() GameTooltip:Hide() end)
	return b
end

local function BuildFrame()
	local W, PAD, ROW = 150, 8, 22
	local inner = W - 2 * PAD

	frame = CreateFrame("Frame", "AgentCamFrame", UIParent)
	frame:SetWidth(W)
	Backdrop(frame)
	MakeDraggable(frame, "pos", "RIGHT", -40, 0)
	frame:SetScript("OnUpdate", OnUpdate)

	local title = frame:CreateFontString(nil, "OVERLAY", "GameFontNormal")
	title:SetPoint("TOP", frame, "TOP", 0, -8)
	title:SetText("AgentCam")
	local gear = CreateFrame("Button", "AgentCamGear", frame)
	gear:SetWidth(16)
	gear:SetHeight(16)
	gear:SetPoint("TOPRIGHT", frame, "TOPRIGHT", -7, -6)
	gear.icon = gear:CreateTexture(nil, "ARTWORK")
	gear.icon:SetAllPoints(gear)
	gear.icon:SetTexture(ICONS.settings)
	gear:SetHighlightTexture("Interface\\Buttons\\ButtonHilight-Square")
	gear:GetHighlightTexture():SetBlendMode("ADD")
	gear:SetScript("OnClick", function() AgentCam_ToggleConfig() end)
	gear:SetScript("OnEnter", function()
		GameTooltip:SetOwner(this, "ANCHOR_LEFT")
		GameTooltip:SetText("Settings")
		GameTooltip:AddLine("/ac config", 0.5, 0.8, 1)
		GameTooltip:Show()
	end)
	gear:SetScript("OnLeave", function() GameTooltip:Hide() end)

	-- One row per possible slot; LayoutPanel shows the active ones in order.
	for i = 1, MAX_AGENTS do
		local b = Button("AgentCamButton" .. i, inner, PAD, 0, nil, function()
			if arg1 == "RightButton" then
				if Off() then return end
				Appear(db.names[this.slot])
				Print("teleporting to " .. db.names[this.slot])
			else
				AgentCam_Switch(this.slot)
			end
		end)
		b:RegisterForClicks("LeftButtonUp", "RightButtonUp")
		buttons[i] = b
	end
	frame.emptyText = frame:CreateFontString(nil, "OVERLAY", "GameFontDisableSmall")
	frame.emptyText:SetPoint("TOP", frame, "TOP", 0, -30)
	frame.emptyText:SetText("No agents ticked.\nOpen settings (gear).")

	-- Two rows of icon buttons (hover for a tooltip), in a holder LayoutPanel moves under the list.
	tools = CreateFrame("Frame", nil, frame)
	tools:SetWidth(W)
	tools:SetHeight(2 * ICON + 8)
	local gap = (inner - 5 * ICON) / 4
	local function Col(k) return PAD + (k - 1) * (ICON + gap) end
	local y = 0
	local labels = { view = "View (its eyes)", follow = "Follow (free camera)", watch = "Watch (.watch)" }
	local descs = {
		view = "Click an agent to see through its eyes (.gm setview).",
		follow = "Click an agent to follow it: your invisible body hops beside it every few seconds; your camera stays free.",
		watch = "Click an agent to use the server's .watch.",
	}
	for k = 1, 3 do
		local m = MODES[k]
		local b = IconButton("AgentCamMode" .. k, Col(k), y, ICONS[m], labels[m], descs[m], "Alt-M cycles modes",
			function() AgentCam_SetMode(this.mode) end)
		b.mode = m
		modeButtons[k] = b
	end
	IconButton("AgentCamButtonMe", Col(4), y, ICONS.me, "Back to me", "Return the camera to your own body.",
		"Alt-0", AgentCam_Reset)
	IconButton("AgentCamGo", Col(5), y, ICONS.go, "Teleport to target",
		"Teleport to the player you have targeted. Right-click an agent, or /ac go <name>, for others.",
		"Alt-G", function() AgentCam_Go() end)
	y = y - ICON - 4
	frame.autoButton = IconButton("AgentCamAuto", Col(1), y, ICONS.auto, "Auto director",
		"Cycle through the ticked agents every few seconds; holds while one is fighting. /ac auto <secs>.",
		"Alt-A", function() AgentCam_ToggleAuto() end, true)
	frame.camButton = IconButton("AgentCamCam", Col(2), y, ICONS.cam, "Far camera",
		"Let the camera zoom out to 50 yards.", "Alt-C", AgentCam_ToggleCamera, true)
	frame.invisButton = IconButton("AgentCamInvis", Col(3), y, ICONS.invis, "GM invisibility",
		"Lit: players cannot see you (.gm visible off). Grey: you are visible.", "Alt-V", AgentCam_ToggleInvis, true)
	IconButton("AgentCamShot", Col(4), y, ICONS.shot, "Screenshot",
		"Print time, agent and zone to chat, then take a screenshot.", "Alt-S", AgentCam_Screenshot)
	IconButton("AgentCamDismount", Col(5), y, ICONS.dismount, "Dismount", "Send .dismount.", nil,
		function() Cmd(".dismount") end)

	frame.statusText = frame:CreateFontString(nil, "OVERLAY", "GameFontDisableSmall")
	frame.statusText:SetPoint("BOTTOM", frame, "BOTTOM", 0, 7)

	LayoutPanel = function()
		local list = ActiveSlots()
		local n = table.getn(list)
		local top = -26
		for i = 1, MAX_AGENTS do
			local b = buttons[i]
			local slot = list[i]
			if slot then
				b.slot = slot
				b:SetText(i .. ". " .. db.names[slot])
				b:ClearAllPoints()
				b:SetPoint("TOPLEFT", frame, "TOPLEFT", PAD, top - (i - 1) * ROW)
				if current == slot then b:LockHighlight() else b:UnlockHighlight() end
				b:Show()
			else
				b.slot = nil
				b:Hide()
			end
		end
		local listHeight = (n > 0) and n * ROW or 30
		if n > 0 then frame.emptyText:Hide() else frame.emptyText:Show() end
		tools:ClearAllPoints()
		tools:SetPoint("TOPLEFT", frame, "TOPLEFT", 0, top - listHeight - 4)
		frame:SetHeight(-top + listHeight + 4 + 2 * ICON + 4 + 22)
	end

	-- Info overlay: the viewed agent and what it is fighting.
	info = CreateFrame("Frame", "AgentCamInfo", UIParent)
	info:SetWidth(320)
	info:SetHeight(42)
	Backdrop(info)
	MakeDraggable(info, "infoPos", "TOP", 0, -120)
	info.line1 = info:CreateFontString(nil, "OVERLAY", "GameFontNormal")
	info.line1:SetPoint("TOPLEFT", info, "TOPLEFT", 10, -8)
	info.line2 = info:CreateFontString(nil, "OVERLAY", "GameFontHighlightSmall")
	info.line2:SetPoint("TOPLEFT", info.line1, "BOTTOMLEFT", 0, -4)
	info:Hide()

	if db.hidden or db.enabled == false then frame:Hide() end
	UpdateButtons()
end

------------------------------------------------------------------------------- keys & slash

local function BindDefaultKeys()
	for i = 1, 5 do SetBinding("ALT-" .. i, "AGENTCAM_" .. i) end
	SetBinding("ALT-0", "AGENTCAM_RESET")
	SetBinding("ALT-=", "AGENTCAM_NEXT")
	SetBinding("ALT--", "AGENTCAM_PREV")
	SetBinding("ALT-M", "AGENTCAM_MODE")
	SetBinding("ALT-A", "AGENTCAM_AUTO")
	SetBinding("ALT-G", "AGENTCAM_GOTARGET")
	SetBinding("ALT-C", "AGENTCAM_CAM")
	SetBinding("ALT-V", "AGENTCAM_INVIS")
	SetBinding("ALT-S", "AGENTCAM_SHOT")
	local set = GetCurrentBindingSet()
	if set ~= 1 and set ~= 2 then set = 1 end -- 1 = account-wide
	SaveBindings(set)
	Print("keys: Alt-1..5 panel agents 1-5, Alt-0 me, Alt-=/- next/prev, Alt-M mode, Alt-A auto,")
	Print("      Alt-G go to target, Alt-C camera, Alt-V invisibility, Alt-S screenshot")
end

------------------------------------------------------------------------------- settings window

local config
local MODE_LABELS = { view = "View - see through its eyes", follow = "Follow - free camera", watch = "Watch - .watch fallback" }

local function Label(parent, text, x, y, font)
	local fs = parent:CreateFontString(nil, "OVERLAY", font or "GameFontNormal")
	fs:SetPoint("TOPLEFT", parent, "TOPLEFT", x, y)
	fs:SetText(text)
	return fs
end

local function Check(name, x, y, text, onClick)
	local c = CreateFrame("CheckButton", name, config, "UICheckButtonTemplate")
	c:SetWidth(24)
	c:SetHeight(24)
	c:SetPoint("TOPLEFT", config, "TOPLEFT", x, y)
	getglobal(name .. "Text"):SetText(text)
	c:SetScript("OnClick", onClick)
	return c
end

local function GreyOut(on)
	for _, c in ipairs(config.controls) do
		c:SetAlpha(on and 1 or 0.35)
		c:EnableMouse(on)
		if c.ClearFocus then c:ClearFocus() end
		if c.Enable then if on then c:Enable() else c:Disable() end end
	end
end

function AgentCam_RefreshConfig()
	if not config then return end
	config.enable:SetChecked(db.enabled ~= false)
	GreyOut(db.enabled ~= false)
	for i = 1, MAX_AGENTS do
		local e = config.names[i]
		if not e.focused then e:SetText(db.names[i] or "") end
		config.checks[i]:SetChecked(db.on[i])
	end
	for k = 1, 3 do config.modes[k]:SetChecked(MODES[k] == db.mode) end
	config.slider:SetValue(db.autoInterval)
	config.auto:SetChecked(db.auto)
	config.info:SetChecked(db.info)
	config.cam:SetChecked(db.farCam)
	config.panel:SetChecked(not db.hidden)
end

local function SaveName()
	local i = this.index
	local text = string.gsub(this:GetText() or "", "%s", "")
	if text ~= (db.names[i] or "") then
		db.names[i] = text
		Print("agent " .. i .. " = " .. (text ~= "" and text or "(empty)"))
		UpdateButtons()
	end
end

local function BuildConfig()
	config = CreateFrame("Frame", "AgentCamConfig", UIParent)
	config:SetWidth(400)
	config:SetHeight(596)
	config:SetFrameStrata("DIALOG")
	config:SetPoint("CENTER", UIParent, "CENTER", 0, 0)
	config:SetBackdrop({
		bgFile = "Interface\\DialogFrame\\UI-DialogBox-Background",
		edgeFile = "Interface\\DialogFrame\\UI-DialogBox-Border",
		tile = true, tileSize = 32, edgeSize = 32,
		insets = { left = 11, right = 12, top = 12, bottom = 11 },
	})
	config:SetMovable(true)
	config:EnableMouse(true)
	config:RegisterForDrag("LeftButton")
	config:SetScript("OnDragStart", function() this:StartMoving() end)
	config:SetScript("OnDragStop", function() this:StopMovingOrSizing() end)
	config:SetScript("OnShow", AgentCam_RefreshConfig)
	config:Hide()
	table.insert(UISpecialFrames, "AgentCamConfig") -- Esc closes it

	local title = config:CreateFontString(nil, "OVERLAY", "GameFontHighlightLarge")
	title:SetPoint("TOP", config, "TOP", 0, -18)
	title:SetText("AgentCam Settings")
	local close = CreateFrame("Button", "AgentCamConfigClose", config, "UIPanelCloseButton")
	close:SetPoint("TOPRIGHT", config, "TOPRIGHT", -6, -6)

	-- Master switch: off releases the camera, stops the director, hides the panel and overlay,
	-- and greys out everything below.
	config.enable = Check("AgentCamConfigEnable", 22, -44, "Enable AgentCam", function()
		AgentCam_SetEnabled(this:GetChecked() and true or false)
	end)
	getglobal("AgentCamConfigEnableText"):SetFontObject(GameFontNormalLarge)

	-- Agent slots: tick to put a named agent on the panel and in the rotation.
	Label(config, "Agents (ticked + named = on the panel)", 24, -80)
	local function SetAll(on)
		for i = 1, MAX_AGENTS do db.on[i] = on end
		UpdateButtons()
		AgentCam_RefreshConfig()
		Print(on and "all agent slots ticked" or "all agent slots unticked")
	end
	local allOn = CreateFrame("Button", "AgentCamConfigAllOn", config, "UIPanelButtonTemplate")
	allOn:SetWidth(80)
	allOn:SetHeight(20)
	allOn:SetPoint("TOPLEFT", config, "TOPLEFT", 24, -100)
	allOn:SetText("Enable all")
	allOn:SetScript("OnClick", function() SetAll(true) end)
	local allOff = CreateFrame("Button", "AgentCamConfigAllOff", config, "UIPanelButtonTemplate")
	allOff:SetWidth(80)
	allOff:SetHeight(20)
	allOff:SetPoint("LEFT", allOn, "RIGHT", 6, 0)
	allOff:SetText("Disable all")
	allOff:SetScript("OnClick", function() SetAll(false) end)
	config.allButtons = { allOn, allOff }

	config.names, config.checks = {}, {}
	for i = 1, MAX_AGENTS do
		local col, row = math.floor((i - 1) / 10), math.mod(i - 1, 10)
		local x, y = 20 + col * 190, -126 - row * 24
		local c = CreateFrame("CheckButton", "AgentCamConfigOn" .. i, config, "UICheckButtonTemplate")
		c:SetWidth(22)
		c:SetHeight(22)
		c:SetPoint("TOPLEFT", config, "TOPLEFT", x, y)
		c.index = i
		c:SetScript("OnClick", function()
			db.on[this.index] = this:GetChecked() and true or false
			UpdateButtons()
		end)
		config.checks[i] = c
		Label(config, i .. ".", x + 24, y - 5, "GameFontHighlightSmall")
		local e = CreateFrame("EditBox", "AgentCamConfigName" .. i, config, "InputBoxTemplate")
		e:SetWidth(120)
		e:SetHeight(20)
		e:SetPoint("TOPLEFT", config, "TOPLEFT", x + 50, y - 1)
		e:SetAutoFocus(false)
		e:SetMaxLetters(12) -- the longest character name 1.12 allows
		e.index = i
		e:SetScript("OnEnterPressed", function() SaveName(); this:ClearFocus() end)
		e:SetScript("OnEscapePressed", function() this:SetText(db.names[this.index] or ""); this:ClearFocus() end)
		e:SetScript("OnTabPressed", function()
			local nxt = config.names[math.mod(this.index, MAX_AGENTS) + 1]
			nxt:SetFocus()
		end)
		e:SetScript("OnEditFocusGained", function() this.focused = true end)
		e:SetScript("OnEditFocusLost", function() this.focused = nil; SaveName() end)
		config.names[i] = e
	end

	-- Mode (radio)
	Label(config, "Click an agent to", 24, -374)
	config.modes = {}
	for k = 1, 3 do
		local c = Check("AgentCamConfigMode" .. k, 26, -392 - (k - 1) * 24, MODE_LABELS[MODES[k]], function()
			AgentCam_SetMode(this.mode)
			AgentCam_RefreshConfig()
		end)
		c.mode = MODES[k]
		config.modes[k] = c
	end

	-- Auto director
	config.auto = Check("AgentCamConfigAuto", 26, -470, "Auto director (cycle agents)", function()
		if (this:GetChecked() and true or false) ~= (db.auto and true or false) then AgentCam_ToggleAuto() end
	end)
	local s = CreateFrame("Slider", "AgentCamConfigInterval", config, "OptionsSliderTemplate")
	s:SetWidth(200)
	s:SetHeight(16)
	s:SetPoint("TOPLEFT", config, "TOPLEFT", 60, -508)
	s:SetMinMaxValues(5, 120)
	s:SetValueStep(5)
	getglobal("AgentCamConfigIntervalLow"):SetText("5s")
	getglobal("AgentCamConfigIntervalHigh"):SetText("120s")
	s:SetScript("OnValueChanged", function()
		local v = math.floor(this:GetValue() + 0.5)
		getglobal("AgentCamConfigIntervalText"):SetText("Switch every " .. v .. "s")
		if db.autoInterval ~= v then
			db.autoInterval = v
			UpdateButtons()
		end
	end)
	config.slider = s

	-- Display
	config.info = Check("AgentCamConfigInfo", 26, -532, "Info overlay", function()
		db.info = this:GetChecked() and true or false
	end)
	config.cam = Check("AgentCamConfigCam", 200, -532, "Far camera", function()
		if (this:GetChecked() and true or false) ~= (db.farCam and true or false) then AgentCam_ToggleCamera() end
	end)
	config.panel = Check("AgentCamConfigPanel", 26, -556, "Show panel", function()
		if this:GetChecked() then db.hidden = nil; frame:Show() else db.hidden = true; frame:Hide() end
	end)
	local keys = CreateFrame("Button", "AgentCamConfigKeys", config, "UIPanelButtonTemplate")
	keys:SetWidth(120)
	keys:SetHeight(22)
	keys:SetPoint("TOPLEFT", config, "TOPLEFT", 200, -556)
	keys:SetText("Bind Alt keys")
	keys:SetScript("OnClick", BindDefaultKeys)

	config.controls = { config.auto, config.slider, config.info, config.cam, config.panel, keys }
	for i = 1, MAX_AGENTS do
		table.insert(config.controls, config.names[i])
		table.insert(config.controls, config.checks[i])
	end
	table.insert(config.controls, config.allButtons[1])
	table.insert(config.controls, config.allButtons[2])
	for k = 1, 3 do table.insert(config.controls, config.modes[k]) end
end

-- benilla's Options window ships its AddOns tab disabled (no addon page behind it). Wire it to
-- this settings window: clicking it closes Options and opens AgentCam's.
local function HookOptionsAddOnsTab()
	local tab = getglobal("BenillaOptionsFrameAddOnsTab")
	if not tab then return end
	tab:Enable()
	tab:SetScript("OnClick", function()
		PlaySound("igMainMenuOptionCheckBoxOn")
		HideUIPanel(BenillaOptionsFrame)
		if not config then BuildConfig() end
		config:Show()
	end)
end

function AgentCam_SetEnabled(on)
	if on == (db.enabled ~= false) then return end
	if not on then
		Release()
		current = nil
		db.enabled = false
		frame:Hide()
		info:Hide()
		Print("disabled (panel hidden, keys and commands off). /ac config to turn it back on")
	else
		db.enabled = true
		if not db.hidden then frame:Show() end
		Print("enabled")
	end
	UpdateButtons()
	if config then AgentCam_RefreshConfig() end
end

function AgentCam_ToggleConfig()
	if not config then BuildConfig() end
	if config:IsShown() then config:Hide() else config:Show() end
end

local function Help()
	Print("/ac <n> | next | prev | me          -- switch to panel agent n (right-click = teleport)")
	Print("/ac mode view|follow|watch          -- eyes / free follow / .watch")
	Print("/ac go [n|Name]                     -- teleport to panel agent n, any player, or your target")
	Print("/ac auto [secs] | cam | invis | shot | info -- director, far camera, visibility, screenshot, overlay")
	Print("/ac config | enable | disable | set <slot> <Name> | all | none | list | keys | show | hide")
end

SLASH_AGENTCAM1 = "/ac"
SLASH_AGENTCAM2 = "/agentcam"
SlashCmdList["AGENTCAM"] = function(msg)
	local _, _, cmd, rest = string.find(msg or "", "^%s*(%S*)%s*(.-)%s*$")
	cmd = string.lower(cmd or "")
	if cmd == "config" or cmd == "options" then AgentCam_ToggleConfig() return end
	if cmd == "enable" or cmd == "on" then AgentCam_SetEnabled(true) return end
	if cmd == "disable" or cmd == "off" then AgentCam_SetEnabled(false) return end
	if cmd ~= "help" and Off() then return end
	local n = tonumber(cmd)
	if n then
		AgentCam_SwitchNth(n)
	elseif cmd == "next" then
		AgentCam_Cycle(1)
	elseif cmd == "prev" then
		AgentCam_Cycle(-1)
	elseif cmd == "me" or cmd == "reset" then
		AgentCam_Reset()
	elseif cmd == "mode" then
		AgentCam_SetMode(string.lower(rest))
	elseif cmd == "view" or cmd == "follow" or cmd == "watch" then
		AgentCam_SetMode(cmd)
	elseif cmd == "go" or cmd == "tp" then
		AgentCam_Go(rest)
	elseif cmd == "auto" then
		if rest == "off" then db.auto = true end
		AgentCam_ToggleAuto(rest)
	elseif cmd == "cam" then
		AgentCam_ToggleCamera()
	elseif cmd == "invis" then
		AgentCam_ToggleInvis()
	elseif cmd == "shot" then
		AgentCam_Screenshot()
	elseif cmd == "info" then
		db.info = not db.info
		Print(db.info and "info overlay on" or "info overlay off")
	elseif cmd == "set" then
		local _, _, idx, name = string.find(rest, "^(%d+)%s+(%S+)$")
		idx = tonumber(idx)
		if not idx or idx < 1 or idx > MAX_AGENTS then
			Print("usage: /ac set <slot 1-" .. MAX_AGENTS .. "> <Name>")
			return
		end
		db.names[idx] = name
		db.on[idx] = true
		UpdateButtons()
		Print("slot " .. idx .. " = " .. name .. " (ticked)")
	elseif cmd == "list" then
		for i = 1, MAX_AGENTS do
			if db.names[i] and db.names[i] ~= "" then
				Print(i .. ". [" .. (db.on[i] and "x" or " ") .. "] " .. db.names[i])
			end
		end
	elseif cmd == "all" or cmd == "none" then
		for i = 1, MAX_AGENTS do db.on[i] = (cmd == "all") end
		UpdateButtons()
		Print(cmd == "all" and "all agent slots ticked" or "all agent slots unticked")
	elseif cmd == "keys" then
		BindDefaultKeys()
	elseif cmd == "show" then
		db.hidden = nil
		frame:Show()
	elseif cmd == "hide" then
		db.hidden = true
		frame:Hide()
	elseif cmd == "" then
		if frame:IsShown() then db.hidden = true; frame:Hide() else db.hidden = nil; frame:Show() end
	else
		Help()
	end
end

local loader = CreateFrame("Frame")
loader:RegisterEvent("ADDON_LOADED")
loader:RegisterEvent("PLAYER_ENTERING_WORLD")
loader:SetScript("OnEvent", function()
	if event == "ADDON_LOADED" and arg1 == "AgentCam" then
		AgentCamDB = AgentCamDB or {}
		db = AgentCamDB
		if not db.names then
			db.names = {}
			for i = 1, table.getn(DEFAULT_NAMES) do db.names[i] = DEFAULT_NAMES[i] end
		end
		if not db.on then -- fresh, or saved before slots could be ticked
			db.on = {}
			for i = 1, MAX_AGENTS do db.on[i] = (i <= DEFAULT_ON) end
		end
		if db.mode == "gm" or not db.mode then db.mode = "view" end
		db.autoInterval = db.autoInterval or 30
		if db.info == nil then db.info = true end
		BuildFrame()
		HookOptionsAddOnsTab()
	elseif event == "PLAYER_ENTERING_WORLD" and db then
		if db.farCam then ApplyCamera() end
		this:UnregisterEvent("PLAYER_ENTERING_WORLD")
	end
end)
