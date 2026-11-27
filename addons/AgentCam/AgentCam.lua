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
BINDING_NAME_AGENTCAM_INVIS = "Toggle GM mode + invisibility"
BINDING_NAME_AGENTCAM_GM = "Toggle GM mode + invisibility"
BINDING_NAME_AGENTCAM_SHOT = "Screenshot with caption"
BINDING_NAME_AGENTCAM_OBS = "Toggle the observe panel"

local db -- AgentCamDB after load
local current -- slot (1..MAX_AGENTS) of the agent being viewed, or nil
local active -- what is engaged on the server/client: "view" | "watch" | "follow" | nil
local pending -- { index, name, action, elapsed, tries } while waiting for .appear to land
local following -- { index, name, elapsed, lastAppear } while in follow mode
local keeping -- { name, mode, elapsed, resetIn } while viewing or watching: keeps the body near
local invisible -- nil = not yet set this session
local gmOn -- nil = unknown (taken as on: the spectator logs in with GM mode on)
local autoElapsed = 0
local buttons = {} -- the agent rows
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

-- Team view (/ac team): the panel lists another player's group instead of the ticked agents.
-- Its rows are virtual slots TEAM_BASE + n over team.members, so the slot-based code (current,
-- pending, the keys) works unchanged; NameOf reads either kind.
local TEAM_BASE = 100
local team -- nil, or { lead = name, members = { names } }

local Subscribe -- defined with the WOWOBS feed; UpdateButtons calls it

local function NameOf(slot)
	if not slot then return nil end
	if slot > TEAM_BASE then return team and team.members[slot - TEAM_BASE] end
	return db.names[slot]
end

-- The active slots in slot order: the panel's rows, the keys' and /ac N's numbering, the rotation.
local function ActiveSlots()
	local list = {}
	if team then
		for n = 1, table.getn(team.members) do table.insert(list, TEAM_BASE + n) end
		return list
	end
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
local UpdateAutoText -- defined with the auto director

local function UpdateButtons()
	if not frame then return end
	if Subscribe then Subscribe() end
	frame.title:SetText(team and ("|cff33ff99Team|r " .. team.lead) or "AgentCam")
	frame.teamButton:SetOn(team ~= nil)
	LayoutPanel()
	frame.autoButton:SetOn(db.auto)
	frame.camButton:SetOn(db.farCam)
	frame.gmButton:SetOn(gmOn ~= false and invisible == true)
	local status = "click name: " .. db.mode
	if following then status = status .. " | following" end
	frame.statusBase = status
	frame.statusText:SetText(status)
	UpdateAutoText(true)
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
	keeping = nil
	active = nil
end

local function StartView(i, name)
	Cmd(".gm setview")
	active, current, pending = "view", i, nil
	keeping = { name = name, mode = "view", elapsed = 0 }
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

-- Engage agent slot `i` in `mode` (view / follow / watch), or the default mode when nil.
function AgentCam_Switch(i, mode)
	if Off() then return end
	mode = mode or db.mode
	local name = NameOf(i)
	if not name or name == "" then
		Print("slot " .. i .. " has no name; set it in /ac config")
		return
	end
	if AgentCamObs_Toggle then AgentCamObs_Toggle("auto") end -- the observe panel follows this agent
	if pending and pending.index == i and pending.action == mode then return end -- on its way there
	Release()
	EnsureInvisible()
	autoElapsed = 0
	if mode == "watch" then
		Cmd(".watch " .. name)
		active, current = "watch", i
		keeping = { name = name, mode = "watch", elapsed = 0 }
		UpdateButtons()
		Print("watching " .. name)
	elseif mode == "follow" then
		if TryTarget(name) and InFollowRange() then
			StartFollow(i, name)
		else
			Appear(name)
			pending = { index = i, name = name, action = "follow", elapsed = 0, tries = 0 }
			UpdateButtons()
		end
	else
		-- Only when it is next to your body: the client can hold a stale copy of a unit thousands
		-- of yards away, and viewing it from here shows a world that was never streamed in.
		if TryTarget(name) and InFollowRange() then
			StartView(i, name)
		else
			Appear(name)
			pending = { index = i, name = name, action = "view", elapsed = 0, tries = 0 }
			UpdateButtons()
		end
	end
end

-- The slot `step` places after the current one in the panel order, or nil with nothing ticked.
local function StepSlot(step)
	local list = ActiveSlots()
	local n = table.getn(list)
	if n == 0 then return nil end
	local at = (pending and pending.index) or current -- an agent being reached counts as current
	local pos = 0
	for k = 1, n do
		if list[k] == at then pos = k end
	end
	if pos == 0 and step < 0 then pos = 1 end -- from nothing, prev starts at the last
	return list[math.mod(pos - 1 + step + n, n) + 1]
end

function AgentCam_Cycle(step)
	if Off() then return end
	local slot = StepSlot(step)
	if not slot then
		Print("no agents ticked - pick some in /ac config")
		return
	end
	AgentCam_Switch(slot)
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

-- current slot, engaged action, and the pending .appear's action and tries (for /ac state, tests).
function AgentCam_State()
	return current, active, pending and pending.action, pending and pending.tries
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
	db.mode = m -- the default for a name click; the row icons pick their own
	Print("clicking a name now does: " .. m)
	UpdateButtons()
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

-- Teleport your body beside agent slot `i` (no view change unless a follow is running).
function AgentCam_Teleport(i)
	if Off() then return end
	local name = NameOf(i)
	if not name or name == "" then return end
	if following then Release() end -- the jump would break the follow anyway
	EnsureInvisible()
	Appear(name)
	Print("teleporting to " .. name)
end

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
		name = NameOf(slot)
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

-- GM mode and invisibility as one switch: both on, or both off. A mixed or unknown state (one
-- changed by hand, or no server reply yet) counts as off, so the next click turns both on.
function AgentCam_ToggleGMInvis()
	if Off() then return end
	if gmOn ~= false and invisible == true then
		Cmd(".gm visible on") -- visibility first: it is a GM-mode command
		Cmd(".gm off")
		Print("GM mode off and visible: players see you, monsters can attack you")
	else
		Cmd(".gm on")
		Cmd(".gm visible off")
	end
end

-- Keep the two toggles true to the server: its replies arrive as a notification (UI_ERROR_MESSAGE)
-- and, for some commands, a system line, whoever sent the command.
local SERVER_STATE = {
	["GM mode is ON"] = function() gmOn = true end,
	["GM mode is OFF"] = function() gmOn = false end,
	["You are now invisible."] = function() invisible = true end,
	["You are now visible."] = function() invisible = false end,
}

-- .obs answered "There is no such command" (the server lacks it, or it needs a higher GM level
-- than the spectator's): stop sending it this session instead of repeating the error every minute.
local obsSentAt = -100
local obsUnsupported
local obsMissing = {} -- [lower name] = true: left out by the last .obs ("obs: X not online")
local function OnServerLine()
	-- the server's answers to .obs (ObsMgr): "obs: <name> not online", "obs: a, b" (subscribed)
	local _, _, gone = string.find(arg1 or "", "^obs: (%S+) not online$")
	if gone then obsMissing[Lower(gone)] = true end
	if arg1 == "There is no such command" and GetTime() - obsSentAt < 3 and not obsUnsupported then
		obsUnsupported = true
		Print("the server does not accept .obs for this account (\"There is no such command\"): no WOWOBS subscription, the bars stay empty. /ac obs retry after the server is updated")
	end
	local f = arg1 and SERVER_STATE[arg1]
	if f then
		f()
		UpdateButtons()
	end
end

-- Agent health and power, last seen. 1.12 reads a unit only through a token (target, mouseover,
-- party...), and for a player outside your group health comes as a percentage; so a row shows
-- what was seen last, dimmed once stale. Experience is the player's alone (UnitXP("player")).
local stats = {} -- [lower name] = { hp, hpMax, mp, mpMax, ptype, dead, t }
local xps = {} -- [lower name] = { xp, xpMax }: only the server's WOWOBS U records carry it
local STALE = 10 -- seconds: older readings draw dimmed
-- The class icons on the character-create sheet, 4 a row (the glue's CLASS_ICON_TCOORDS).
local CLASS_SHEET = "Interface\\Glues\\CharacterCreate\\UI-CharacterCreate-Classes"
local CLASS_TCOORDS = {
	WARRIOR = { 0, 0.25, 0, 0.25 }, MAGE = { 0.25, 0.49609375, 0, 0.25 },
	ROGUE = { 0.49609375, 0.7421875, 0, 0.25 }, DRUID = { 0.7421875, 0.98828125, 0, 0.25 },
	HUNTER = { 0, 0.25, 0.25, 0.5 }, SHAMAN = { 0.25, 0.49609375, 0.25, 0.5 },
	PRIEST = { 0.49609375, 0.7421875, 0.25, 0.5 }, WARLOCK = { 0.7421875, 0.98828125, 0.25, 0.5 },
	PALADIN = { 0, 0.25, 0.5, 0.75 },
}
local UpdateBars -- defined with the frame

-- A name the panel can show: a ticked slot's, or a member of the team being viewed.
local function Listed(key)
	for i = 1, MAX_AGENTS do
		if db.names[i] and Lower(db.names[i]) == key then return true end
	end
	if team then
		for _, m in ipairs(team.members) do
			if Lower(m) == key then return true end
		end
	end
	return false
end

local function Capture(unit)
	local n = UnitName(unit)
	if not n then return end -- matched by name against the agent slots below
	local key = Lower(n)
	if not Listed(key) then return end
	-- Only the class, for the row's icon. Health, power and experience come from the server's
	-- WOWOBS records alone (AgentCam_Feed): the old target-swap scan is gone.
	local _, class = UnitClass(unit)
	if class and db.classes[key] ~= class then
		db.classes[key] = class -- saved: the icon shows before the agent is seen again
		if UpdateBars then UpdateBars() end
	end
end

-- For AgentCamObs: the viewed agent's name, the ticked agents' names, one agent's status, and a
-- status line fed from the server's WOWOBS messages (the only source of the row bars).
-- The agent being watched, or the one an .appear is still taking us to: a click on a name moves
-- the observe panel at once, not when the hop lands.
function AgentCam_Watched()
	if pending and pending.name then return pending.name end
	return current and db and NameOf(current) or nil
end

function AgentCam_Agents()
	local t = {}
	if not db then return t end
	for _, slot in ipairs(ActiveSlots()) do table.insert(t, NameOf(slot)) end
	return t
end

function AgentCam_GetStats(name)
	local key = Lower(name)
	if not key or not db then return nil end
	return stats[key], db.classes[key]
end

function AgentCam_Feed(name, st)
	local key = Lower(name)
	if not key or not db then return end
	if not Listed(key) then return end
	if st.class then db.classes[key] = st.class end
	stats[key] = {
		hp = st.hp, hpMax = st.hpMax, mp = st.mp, mpMax = st.mpMax, ptype = st.ptype,
		dead = st.dead, offline = st.offline, t = GetTime(), obs = true,
	}
	if st.xpMax and st.xpMax > 0 then xps[key] = { xp = st.xp or 0, xpMax = st.xpMax } else xps[key] = nil end
	if UpdateBars then UpdateBars() end
end

local function OnPanelEvent()
	if event == "UI_ERROR_MESSAGE" or event == "CHAT_MSG_SYSTEM" then
		OnServerLine()
	elseif event == "PLAYER_TARGET_CHANGED" then
		Capture("target")
	elseif event == "UPDATE_MOUSEOVER_UNIT" then
		Capture("mouseover")
	end
end

------------------------------------------------------------------------------- subscription

-- The server pushes WOWOBS records only for the names a spectator subscribed with .obs (the
-- subscribed agents and their group members; see OBS_SERVER.md). AgentCam keeps that list equal
-- to what the panel shows: the ticked agents, or in team view the team's lead. A repeat of the
-- same list is not sent again; nothing is sent before the world is entered.
local entered -- PLAYER_ENTERING_WORLD has been seen this session
local subscribed -- the list the subscription stands for
local prevSent -- the last .obs argument actually sent
local resubElapsed = 0

local function WantedList()
	local names = {}
	if team then
		names = { team.lead }
	else
		for _, slot in ipairs(ActiveSlots()) do table.insert(names, NameOf(slot)) end
	end
	return table.concat(names, " ")
end

-- Send the list now: .obs off for the old one first (the server replaces it anyway; this makes
-- the drop explicit), then .obs with the new one.
local function SendSubscription(arg)
	subscribed, resubElapsed, obsMissing = arg, 0, {}
	if obsUnsupported then return end
	obsSentAt = GetTime()
	if prevSent and prevSent ~= "" then Cmd(".obs off") end
	prevSent = arg
	if arg ~= "" then Cmd(".obs " .. arg) end
end

-- /ac obs retry: try .obs again (after the server gains it).
function AgentCam_RetrySubscribe()
	obsUnsupported, subscribed, prevSent = nil, nil, nil
	if Subscribe then Subscribe() end
end

-- A settings change: the list is sent SUB_SETTLE seconds after the last change, so ticking five
-- boxes in a row sends one .obs, not five (TickResubscribe sends it).
local SUB_SETTLE = 1
local subDue -- GetTime() at which a changed list is sent
Subscribe = function()
	if not entered or db.enabled == false then return end
	if WantedList() == subscribed then subDue = nil return end
	subDue = GetTime() + SUB_SETTLE
end

-- The server keeps a subscribed character that logs off and resumes it when it returns, but a
-- name offline when .obs was sent is answered "obs: <name> not online" and left out. So while
-- the last reply left a name out, send the list again every RESUB_EVERY seconds.
local RESUB_EVERY = 60
local function TickResubscribe(dt)
	if subDue and GetTime() >= subDue then
		subDue = nil
		local arg = WantedList()
		if arg ~= subscribed then SendSubscription(arg) end
	end
	if not entered or not subscribed or subscribed == "" then return end
	resubElapsed = resubElapsed + dt
	if resubElapsed < RESUB_EVERY then return end
	resubElapsed = 0
	if obsUnsupported or not next(obsMissing) then return end
	obsSentAt = GetTime()
	obsMissing = {}
	Cmd(".obs " .. subscribed) -- the same list again: no .obs off first
end
-- Team view on `name` (no name: your target), or back to the ticked agents when already on it.
-- The rows fill from the lead's G record; until then the team is just the lead.
function AgentCam_Team(name)
	if Off() then return end
	if not name or name == "" then
		if team then name = nil
		elseif UnitExists("target") and UnitIsPlayer("target") then name = UnitName("target")
		else
			Print("target a player first, or /ac team <name>")
			return
		end
	end
	if team and (not name or Lower(name) == Lower(team.lead) or name == "off") then
		Release()
		current = nil
		team = nil
		Print("back to your agents")
	elseif name ~= "off" then
		Release()
		current = nil
		team = { lead = name, members = { name } }
		Print("team view: " .. name .. "'s group (subscribing with .obs)")
	end
	UpdateButtons()
end

-- From AgentCamObs: a G record's members. Fills the team view when it is the lead's group.
function AgentCam_Roster(name, members)
	if not team or Lower(name) ~= Lower(team.lead) then return end
	local list, seen = {}, {}
	for _, m in ipairs(members) do
		if m ~= "" and not seen[Lower(m)] then table.insert(list, m) seen[Lower(m)] = true end
	end
	if not seen[Lower(team.lead)] then table.insert(list, 1, team.lead) end
	local same = table.getn(list) == table.getn(team.members)
	for i = 1, table.getn(list) do if list[i] ~= team.members[i] then same = false end end
	if same then return end
	team.members = list
	if current and current > TEAM_BASE + table.getn(list) then Release() current = nil end
	UpdateButtons()
end

function AgentCam_Screenshot()
	if Off() then return end
	local who = current and NameOf(current) or UnitName("player")
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
		Print("could not find " .. pending.name .. " (offline?). Try its Watch icon")
		pending = nil
		UpdateButtons()
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

-- Viewing and watching move only the camera: the body stays where it was, and an agent that runs
-- out past the body's update range is lost (no more position updates). Every FOLLOW_CHECK seconds,
-- an agent farther than ~28 yd from the body brings the body to it: view hops with .appear and
-- re-aims .gm setview once the hop lands (VIEW_SETTLE); watch first asks .resetwatch, then hops.
local function TickKeep(dt)
	if keeping.resetIn then
		keeping.resetIn = keeping.resetIn - dt
		if keeping.resetIn <= 0 then
			keeping.resetIn = nil
			if TryTarget(keeping.name) then Cmd(".gm setview") end
		end
		return
	end
	keeping.elapsed = keeping.elapsed + dt
	if keeping.elapsed < FOLLOW_CHECK then return end
	keeping.elapsed = 0
	if TryTarget(keeping.name) and InFollowRange() then
		keeping.reset = nil
		return
	end
	if keeping.mode == "watch" and not keeping.reset then
		Cmd(".resetwatch")
		keeping.reset = true -- still far next time: hop
		return
	end
	keeping.reset = nil
	Appear(keeping.name)
	if keeping.mode == "view" then keeping.resetIn = VIEW_SETTLE + 0.5 end
end

local autoHolding -- the viewed agent was fighting at the last switch time
local autoTextElapsed = 0

local function TickAuto()
	autoElapsed = autoElapsed + arg1
	if autoElapsed < db.autoInterval then return end
	if current and Lower(UnitName("target")) == Lower(NameOf(current)) and UnitAffectingCombat("target") then
		autoElapsed = db.autoInterval - 5 -- hold the shot while it fights; look again in 5 s
		autoHolding = true
		return
	end
	autoHolding = nil
	AgentCam_Cycle(1)
end

-- The countdown: who is next, and in how many seconds, on the status line (5 times a second).
UpdateAutoText = function(force, dt)
	if not frame then return end
	autoTextElapsed = autoTextElapsed + (dt or 0) -- dt from OnUpdate only: in an event arg1 is not time
	if not force and autoTextElapsed < 0.2 then return end
	autoTextElapsed = 0
	local base = frame.statusBase or ""
	if not db.auto or db.enabled == false then
		frame.statusText:SetText(base)
		return
	end
	local left = math.max(0, math.ceil(db.autoInterval - autoElapsed))
	local nextSlot = StepSlot(1)
	if autoHolding and current then
		frame.statusText:SetText("|cffff8000holding:|r " .. NameOf(current) .. " is fighting")
	elseif nextSlot then
		frame.statusText:SetText("next: |cffffffff" .. NameOf(nextSlot) .. "|r in " .. left .. "s")
	else
		frame.statusText:SetText("auto: no agents ticked")
	end
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
	local name = current and NameOf(current)
	-- the observe panel shows all of this and more
	if AgentCamObs_IsShown and AgentCamObs_IsShown() then
		info:Hide()
		return
	end
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

local barsElapsed = 0
local function OnUpdate()
	if db.enabled == false then return end
	if pending then TickPending() end
	if following and not pending then TickFollow() end
	if keeping and not pending then TickKeep(arg1) end
	barsElapsed = barsElapsed + arg1
	if barsElapsed >= 1 then
		barsElapsed = 0
		if UpdateBars then UpdateBars() end
	end
	if db.auto then TickAuto() end
	TickResubscribe(arg1)
	UpdateAutoText(false, arg1)
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
for _, k in ipairs({ "view", "follow", "watch", "me", "go", "auto", "cam", "invis", "shot", "dismount", "gm", "settings", "obs", "team" }) do
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

local function IconButton(name, x, y, texture, title, desc, key, onClick, toggle, parent, size)
	parent, size = parent or tools, size or ICON
	local b = CreateFrame("Button", name, parent)
	b:SetWidth(size)
	b:SetHeight(size)
	b:SetPoint("TOPLEFT", parent, "TOPLEFT", x, y)
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
	local W, PAD, ROW, RI = 196, 8, 22, 18 -- RI: the per-row icon size
	local inner = W - 2 * PAD

	frame = CreateFrame("Frame", "AgentCamFrame", UIParent)
	frame:SetWidth(W)
	Backdrop(frame)
	MakeDraggable(frame, "pos", "RIGHT", -40, 0)
	frame:SetScript("OnUpdate", OnUpdate)
	frame:SetScript("OnEvent", OnPanelEvent)
	for _, ev in ipairs({ "UI_ERROR_MESSAGE", "CHAT_MSG_SYSTEM", "PLAYER_TARGET_CHANGED", "UPDATE_MOUSEOVER_UNIT" }) do
		frame:RegisterEvent(ev)
	end

	local title = frame:CreateFontString(nil, "OVERLAY", "GameFontNormal")
	frame.title = title
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

	-- Observe panel toggle, top left: the docked panel with the viewed agent's bars, casts,
	-- cooldowns and auras.
	local obsBtn = CreateFrame("Button", "AgentCamObsButton", frame)
	obsBtn:SetWidth(16)
	obsBtn:SetHeight(16)
	obsBtn:SetPoint("TOPLEFT", frame, "TOPLEFT", 7, -6)
	obsBtn.icon = obsBtn:CreateTexture(nil, "ARTWORK")
	obsBtn.icon:SetAllPoints(obsBtn)
	obsBtn.icon:SetTexture(ICONS.obs)
	obsBtn:SetHighlightTexture("Interface\\Buttons\\ButtonHilight-Square")
	obsBtn:GetHighlightTexture():SetBlendMode("ADD")
	obsBtn:SetScript("OnClick", function() if AgentCamObs_Toggle then AgentCamObs_Toggle() end end)
	obsBtn:SetScript("OnEnter", function()
		GameTooltip:SetOwner(this, "ANCHOR_LEFT")
		GameTooltip:SetText("Observe panel")
		GameTooltip:AddLine("The viewed agent: health, power, experience, casts, cooldowns, buffs, its group.", 1, 1, 1, 1)
		GameTooltip:AddLine("/ac obs", 0.5, 0.8, 1)
		GameTooltip:Show()
	end)
	obsBtn:SetScript("OnLeave", function() GameTooltip:Hide() end)

	-- Minimize: fold the panel to its title bar (saved), its top-left corner held where it is.
	local mini = CreateFrame("Button", "AgentCamMinimize", frame)
	mini:SetWidth(16)
	mini:SetHeight(16)
	mini:SetPoint("RIGHT", gear, "LEFT", -2, 0)
	mini:SetHighlightTexture("Interface\\Buttons\\UI-PlusButton-Hilight")
	mini:GetHighlightTexture():SetBlendMode("ADD")
	mini:SetScript("OnClick", function() AgentCam_ToggleCollapsed() end)
	mini:SetScript("OnEnter", function()
		GameTooltip:SetOwner(this, "ANCHOR_LEFT")
		GameTooltip:SetText(db.collapsed and "Expand" or "Minimize")
		GameTooltip:AddLine("/ac min", 0.5, 0.8, 1)
		GameTooltip:Show()
	end)
	mini:SetScript("OnLeave", function() GameTooltip:Hide() end)
	frame.mini = mini

	-- One row per possible slot; LayoutPanel shows the active ones in order. A row is the name
	-- (left click: the default mode from settings) and four actions aimed at that agent alone.
	local ROW_ACTIONS = {
		{ "view", ICONS.view, "View", "See through its eyes (.gm setview)." },
		{ "follow", ICONS.follow, "Follow", "Follow it with your invisible body; your camera stays free." },
		{ "watch", ICONS.watch, "Watch", "The server's .watch on it." },
		{ "go", ICONS.go, "Teleport", "Teleport beside it; your camera stays yours." },
	}
	local nameW = inner - table.getn(ROW_ACTIONS) * (RI + 2)
	for i = 1, MAX_AGENTS do
		local row = CreateFrame("Frame", "AgentCamRow" .. i, frame)
		row:SetWidth(inner)
		row:SetHeight(20)
		-- The name: flat text on a faint strip (no red button), the quest log's highlight on hover
		-- and, locked, on the agent being viewed.
		local b = CreateFrame("Button", "AgentCamButton" .. i, row)
		b:SetWidth(nameW)
		b:SetHeight(20)
		b:SetPoint("TOPLEFT", row, "TOPLEFT", 0, 0)
		b:SetScript("OnClick", function() AgentCam_Switch(this:GetParent().slot) end)
		local strip = b:CreateTexture(nil, "BACKGROUND")
		strip:SetAllPoints(b)
		strip:SetTexture(1, 1, 1, 0.06)
		local label = b:CreateFontString("AgentCamButton" .. i .. "Text", "OVERLAY", "GameFontNormal")
		label:SetPoint("LEFT", b, "LEFT", 4, 0)
		label:SetPoint("RIGHT", b, "RIGHT", -2, 0)
		label:SetJustifyH("LEFT")
		label:SetHeight(14)
		b:SetFontString(label)
		b:SetHighlightTexture("Interface\\QuestFrame\\UI-QuestTitleHighlight")
		b:GetHighlightTexture():SetBlendMode("ADD")
		row.name = b
		b:SetScript("OnEnter", function()
			local slot = this:GetParent().slot
			local st = slot and stats[Lower(NameOf(slot))]
			GameTooltip:SetOwner(this, "ANCHOR_LEFT")
			GameTooltip:SetText(NameOf(slot) or "?")
			if st then
				local power = ({ [0] = "Mana", [1] = "Rage", [3] = "Energy" })[st.ptype] or "Power"
				GameTooltip:AddLine("HP " .. st.hp .. " / " .. st.hpMax .. (st.dead and "  (dead)" or "") .. (st.offline and "  (offline)" or ""), 0.4, 1, 0.4)
				if st.mpMax and st.mpMax > 0 then
					GameTooltip:AddLine(power .. " " .. st.mp .. " / " .. st.mpMax, 0.5, 0.7, 1)
				end
				local x = xps[Lower(NameOf(slot))]
				if x then
					GameTooltip:AddLine(string.format("XP %d / %d (%d%%)", x.xp, x.xpMax, math.floor(100 * x.xp / x.xpMax)), 0.75, 0.5, 1)
				end
				GameTooltip:AddLine("seen " .. math.floor(GetTime() - st.t) .. "s ago", 0.7, 0.7, 0.7)
			else
				GameTooltip:AddLine("no server data yet (WOWOBS)", 0.7, 0.7, 0.7)
			end
			GameTooltip:Show()
		end)
		b:SetScript("OnLeave", function() GameTooltip:Hide() end)
		local function Bar(h)
			local bar = CreateFrame("StatusBar", nil, row)
			bar:SetHeight(h)
			bar:SetStatusBarTexture("Interface\\TargetingFrame\\UI-StatusBar")
			bar:SetMinMaxValues(0, 1)
			local bg = bar:CreateTexture(nil, "BACKGROUND")
			bg:SetAllPoints(bar)
			bg:SetTexture(0.15, 0.15, 0.15, 0.8)
			-- the value on the bar, small and outlined so it reads over any colour
			bar.text = bar:CreateFontString(nil, "OVERLAY")
			bar.text:SetFont("Fonts\\ARIALN.TTF", h + 1, "OUTLINE")
			bar.text:SetPoint("CENTER", bar, "CENTER", 0, 0)
			return bar
		end
		-- health over power over experience (the last only with server data)
		row.hp, row.mp, row.xp = Bar(9), Bar(7), Bar(3)
		row.xp.text:Hide()
		row.xp:SetStatusBarColor(0.6, 0.3, 0.9)
		row.class = row:CreateTexture(nil, "ARTWORK")
		row.class:SetWidth(18)
		row.class:SetHeight(18)
		row.icons = {}
		for k, a in ipairs(ROW_ACTIONS) do
			local action = a[1]
			local ib = IconButton("AgentCamRow" .. i .. "_" .. action, nameW + 2 + (k - 1) * (RI + 2), -1, a[2], a[3],
				a[4], nil, function()
					local slot = this:GetParent().slot
					if this.action == "go" then AgentCam_Teleport(slot) else AgentCam_Switch(slot, this.action) end
				end, nil, row, RI)
			ib.action = action
			row.icons[action] = ib
		end
		buttons[i] = row
	end
	frame.emptyText = frame:CreateFontString(nil, "OVERLAY", "GameFontDisableSmall")
	frame.emptyText:SetPoint("TOP", frame, "TOP", 0, -30)
	frame.emptyText:SetText("No agents ticked.\nOpen settings (gear).")

	-- One row of tools under the list (hover for a tooltip).
	tools = CreateFrame("Frame", nil, frame)
	tools:SetWidth(W)
	tools:SetHeight(ICON + 4)
	local gap = (inner - 7 * ICON) / 6
	local function Col(k) return PAD + (k - 1) * (ICON + gap) end
	IconButton("AgentCamButtonMe", Col(1), 0, ICONS.me, "Back to me", "Return the camera to your own body.",
		"Alt-0", AgentCam_Reset)
	frame.autoButton = IconButton("AgentCamAuto", Col(2), 0, ICONS.auto, "Auto director",
		"Cycle through the ticked agents every few seconds (in the default mode); holds while one is fighting. /ac auto <secs>.",
		"Alt-A", function() AgentCam_ToggleAuto() end, true)
	frame.camButton = IconButton("AgentCamCam", Col(3), 0, ICONS.cam, "Far camera",
		"Let the camera zoom out to 50 yards.", "Alt-C", AgentCam_ToggleCamera, true)
	frame.gmButton = IconButton("AgentCamGM", Col(4), 0, ICONS.gm, "GM mode + invisibility",
		"Lit: GM mode on and invisible (.gm on, .gm visible off). Grey: both off, a visible normal player that monsters can attack. One click switches both.",
		"Alt-H / Alt-V", AgentCam_ToggleGMInvis, true)
	frame.teamButton = IconButton("AgentCamTeam", Col(5), 0, ICONS.team, "Team view",
		"Target a player and click: the list shows that player's group (subscribed with .obs), each member's bars from the server. Click again: back to your agents. /ac team <name>.",
		nil, function() AgentCam_Team() end, true)
	IconButton("AgentCamShot", Col(6), 0, ICONS.shot, "Screenshot",
		"Print time, agent and zone to chat, then take a screenshot.", "Alt-S", AgentCam_Screenshot)
	IconButton("AgentCamDismount", Col(7), 0, ICONS.dismount, "Dismount", "Send .dismount.", nil,
		function() Cmd(".dismount") end)

	-- How many listed agents the server is feeding (a U record in the last STALE seconds):
	-- 0 means no WOWOBS data, so the bars stay empty (not subscribed, or the server not sending).
	frame.dataText = frame:CreateFontString(nil, "OVERLAY", "GameFontDisableSmall")
	frame.dataText:SetPoint("BOTTOMLEFT", frame, "BOTTOMLEFT", 10, 7)
	frame.statusText = frame:CreateFontString(nil, "OVERLAY", "GameFontDisableSmall")
	frame.statusText:SetPoint("BOTTOM", frame, "BOTTOM", 0, 7)

	-- Width: fitted to the longest name, or the width the grip was dragged to (db.width); never
	-- narrower than the tool row. A hidden string measures the names in the buttons' font.
	local MIN_W = 2 * PAD + 7 * ICON + 6 * 2
	local ICONS_W = table.getn(ROW_ACTIONS) * (RI + 2)
	local BAR_W = 96
	local CLASS_W = 20
	local function BarsW() return (db.bars ~= false) and (CLASS_W + BAR_W + 3) or 0 end
	local measure = frame:CreateFontString(nil, "OVERLAY", "GameFontNormal")
	measure:Hide()
	local function FitWidth(list)
		local widest = 0
		for i = 1, table.getn(list) do
			measure:SetText(i .. ". " .. NameOf(list[i]))
			widest = math.max(widest, measure:GetStringWidth())
		end
		return math.max(MIN_W, math.ceil(2 * PAD + widest + 8 + 2 + BarsW() + ICONS_W))
	end
	-- Rows, name buttons and their icons follow the panel's width.
	local function LayoutRows(w)
		local bw = BarsW()
		local nw = w - 2 * PAD - ICONS_W - bw
		for i = 1, MAX_AGENTS do
			local row = buttons[i]
			row:SetWidth(w - 2 * PAD)
			row.name:SetWidth(nw)
			-- Two bars right of the name: health over power.
			row.hp:ClearAllPoints()
			row.mp:ClearAllPoints()
			row.class:ClearAllPoints()
			if bw > 0 then
				row.class:SetPoint("TOPLEFT", row, "TOPLEFT", nw + 2, -1)
				row.class:Show()
				row.hp:SetPoint("TOPLEFT", row, "TOPLEFT", nw + 2 + CLASS_W, -1)
				row.hp:SetWidth(BAR_W)
				row.mp:SetPoint("TOPLEFT", row.hp, "BOTTOMLEFT", 0, -1)
				row.mp:SetWidth(BAR_W)
				row.xp:ClearAllPoints()
				row.xp:SetPoint("TOPLEFT", row.mp, "BOTTOMLEFT", 0, 0)
				row.xp:SetWidth(BAR_W)
				row.hp:Show()
				row.mp:Show()
			else
				row.class:Hide()
				row.hp:Hide()
				row.mp:Hide()
				row.xp:Hide()
			end
			local k = 0
			for _, a in ipairs(ROW_ACTIONS) do
				local ib = row.icons[a[1]]
				ib:ClearAllPoints()
				ib:SetPoint("TOPLEFT", row, "TOPLEFT", nw + 2 + bw + k * (RI + 2), -1)
				k = k + 1
			end
		end
		tools:SetWidth(w)
	end

	frame:SetResizable(true)
	frame:SetMinResize(MIN_W, 28)
	frame:SetMaxResize(800, 2000)
	frame:SetScript("OnSizeChanged", function()
		if this.sizing then LayoutRows(this:GetWidth()) end
	end)
	-- The grip: three dots down the bottom-right corner. Drag: width. Right click: fit to names.
	local grip = CreateFrame("Button", "AgentCamSizer", frame)
	grip:SetWidth(12)
	grip:SetHeight(12)
	grip:SetPoint("BOTTOMRIGHT", frame, "BOTTOMRIGHT", -4, 4)
	for d = 0, 2 do
		local dot = grip:CreateTexture(nil, "OVERLAY")
		dot:SetTexture(0.85, 0.75, 0.45, 0.9)
		dot:SetWidth(2)
		dot:SetHeight(2)
		dot:SetPoint("BOTTOMRIGHT", grip, "BOTTOMRIGHT", -d * 4, d * 4 - 0)
		local dot2 = grip:CreateTexture(nil, "OVERLAY")
		dot2:SetTexture(0.85, 0.75, 0.45, 0.9)
		dot2:SetWidth(2)
		dot2:SetHeight(2)
		dot2:SetPoint("BOTTOMRIGHT", grip, "BOTTOMRIGHT", -d * 4, 0)
	end
	grip:RegisterForClicks("LeftButtonUp", "RightButtonUp")
	grip:SetScript("OnMouseDown", function()
		if arg1 ~= "LeftButton" then return end
		local left, top = frame:GetLeft(), frame:GetTop()
		frame:ClearAllPoints()
		frame:SetPoint("TOPLEFT", UIParent, "BOTTOMLEFT", left, top) -- grow to the right only
		frame.sizing = true
		frame:StartSizing("RIGHT")
	end)
	grip:SetScript("OnMouseUp", function()
		if not frame.sizing then return end
		frame:StopMovingOrSizing()
		frame.sizing = nil
		db.width = math.floor(frame:GetWidth() + 0.5)
		db.pos = { point = "TOPLEFT", relPoint = "BOTTOMLEFT", x = frame:GetLeft(), y = frame:GetTop() }
		UpdateButtons()
	end)
	grip:SetScript("OnClick", function()
		if arg1 == "RightButton" then
			db.width = nil
			Print("panel width: fitted to the names")
			UpdateButtons()
		end
	end)
	grip:SetScript("OnEnter", function()
		GameTooltip:SetOwner(this, "ANCHOR_RIGHT")
		GameTooltip:SetText("Resize")
		GameTooltip:AddLine("Drag to set the width; right-click to fit the names.", 1, 1, 1, 1)
		GameTooltip:Show()
	end)
	grip:SetScript("OnLeave", function() GameTooltip:Hide() end)
	frame.grip = grip

	local POWER_COLOR = { [0] = { 0.2, 0.45, 1 }, [1] = { 0.9, 0.15, 0.15 }, [3] = { 1, 0.85, 0.1 } }
	UpdateBars = function()
		if db.bars == false then return end
		local now = GetTime()
		local live, total = 0, 0
		for i = 1, MAX_AGENTS do
			local row = buttons[i]
			if row.slot then
				total = total + 1
				local s0 = stats[Lower(NameOf(row.slot))]
				if s0 and now - s0.t <= STALE then live = live + 1 end
			end
			local key = row.slot and Lower(NameOf(row.slot))
			local st = key and stats[key]
			local tc = key and CLASS_TCOORDS[db.classes[key] or ""]
			if tc then
				row.class:SetTexture(CLASS_SHEET)
				row.class:SetTexCoord(tc[1], tc[2], tc[3], tc[4])
			else
				row.class:SetTexture("Interface\\Icons\\INV_Misc_QuestionMark")
				row.class:SetTexCoord(0.07, 0.93, 0.07, 0.93)
			end
			row.class:SetAlpha((st and now - st.t <= STALE) and 1 or 0.5)
			if st then
				row.hp:SetValue(st.hpMax > 0 and st.hp / st.hpMax or 0)
				if st.dead then row.hp:SetStatusBarColor(0.5, 0.5, 0.5) else row.hp:SetStatusBarColor(0.1, 0.85, 0.2) end
				local c = POWER_COLOR[st.ptype] or POWER_COLOR[0]
				row.mp:SetStatusBarColor(c[1], c[2], c[3])
				row.mp:SetValue((st.mpMax and st.mpMax > 0) and st.mp / st.mpMax or 0)
				row.hp.text:SetText(st.offline and "offline" or st.dead and "dead" or (st.hp .. " / " .. st.hpMax))
				row.mp.text:SetText((st.mpMax and st.mpMax > 0) and (st.mp .. " / " .. st.mpMax) or "")
				-- dimmed when stale or offline (the server's last U before it unsubscribes the agent)
				local a = (st.offline or now - st.t > STALE) and 0.45 or 1
				row.hp:SetAlpha(a)
				row.mp:SetAlpha(a)
				row.xp:SetAlpha(a)
			else
				row.hp:SetValue(0)
				row.mp:SetValue(0)
				row.hp.text:SetText("|cff808080no data|r")
				row.mp.text:SetText("")
				row.hp:SetAlpha(0.45)
				row.mp:SetAlpha(0.45)
			end
			local x = key and xps[key]
			if x then
				row.xp:SetValue(x.xp / x.xpMax)
				row.xp:Show()
			else
				row.xp:Hide()
			end
		end
		if total > 0 then
			frame.dataText:SetText((live > 0 and "|cff40ff40" or "|cffff4040") .. "data " .. live .. "/" .. total .. "|r")
		else
			frame.dataText:SetText("")
		end
	end

	LayoutPanel = function()
		local up = db.collapsed and "Interface\\Buttons\\UI-PlusButton" or "Interface\\Buttons\\UI-MinusButton"
		frame.mini:SetNormalTexture(up .. "-Up")
		frame.mini:SetPushedTexture(up .. "-Down")
		if db.collapsed then
			for i = 1, MAX_AGENTS do buttons[i]:Hide() end
			frame.emptyText:Hide()
			tools:Hide()
			frame.statusText:Hide()
			frame.grip:Hide()
			frame:SetHeight(28)
			return
		end
		tools:Show()
		frame.statusText:Show()
		frame.grip:Show()
		local list = ActiveSlots()
		if not frame.sizing then
			-- a width dragged before the bars grew must not cut the names: never under the fit
			local w = db.width and math.max(FitWidth(list), db.width) or FitWidth(list)
			frame:SetWidth(w)
			LayoutRows(w)
		end
		UpdateBars()
		local n = table.getn(list)
		local top = -26
		for i = 1, MAX_AGENTS do
			local b = buttons[i]
			local slot = list[i]
			if slot then
				b.slot = slot
				b.name:SetText(i .. ". " .. NameOf(slot))
				b:ClearAllPoints()
				b:SetPoint("TOPLEFT", frame, "TOPLEFT", PAD, top - (i - 1) * ROW)
				if current == slot then b.name:LockHighlight() else b.name:UnlockHighlight() end
				for action, ib in pairs(b.icons) do
					-- Lit: engaged on this agent, or on its way there (.appear sent, waiting to land).
					ib:SetOn((current == slot and active == action)
						or (pending ~= nil and pending.index == slot and pending.action == action))
				end
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
		frame:SetHeight(-top + listHeight + 4 + ICON + 4 + 22)
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

function AgentCam_Collapsed()
	return db and db.collapsed and true or false
end

function AgentCam_ToggleCollapsed()
	if not frame then return end
	-- Re-anchor at the current top-left first, so the height change grows/shrinks downward.
	local left, top = frame:GetLeft(), frame:GetTop()
	if left and top then
		frame:ClearAllPoints()
		frame:SetPoint("TOPLEFT", UIParent, "BOTTOMLEFT", left, top)
		db.pos = { point = "TOPLEFT", relPoint = "BOTTOMLEFT", x = left, y = top }
	end
	db.collapsed = not db.collapsed or nil
	if db.collapsed and AgentCamObs then AgentCamObs:Hide() end -- folds with it; reopens on unfold
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
	SetBinding("ALT-H", "AGENTCAM_GM")
	SetBinding("ALT-S", "AGENTCAM_SHOT")
	local set = GetCurrentBindingSet()
	if set ~= 1 and set ~= 2 then set = 1 end -- 1 = account-wide
	SaveBindings(set)
	Print("keys: Alt-1..5 panel agents 1-5, Alt-0 me, Alt-=/- next/prev, Alt-M mode, Alt-A auto,")
	Print("      Alt-G go to target, Alt-C camera, Alt-V invisibility, Alt-H GM mode, Alt-S screenshot")
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
	config.obsBg:SetValue(AgentCamObs_GetBg and math.floor(AgentCamObs_GetBg() * 100 + 0.5) or 75)
	config.bars:SetChecked(db.bars ~= false)
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
	config:SetHeight(668)
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
	Label(config, "Clicking a name (and Alt-1..5, auto director) does", 24, -374)
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
	config.bars = Check("AgentCamConfigBars", 26, -580, "Status bars (HP / power) by the names", function()
		db.bars = this:GetChecked() and true or false
		UpdateButtons()
	end)
	-- Observe panel background opacity, 0 (fully transparent, no border) to 100 %
	local bs = CreateFrame("Slider", "AgentCamConfigObsBg", config, "OptionsSliderTemplate")
	bs:SetWidth(200)
	bs:SetHeight(16)
	bs:SetPoint("TOPLEFT", config, "TOPLEFT", 60, -622)
	bs:SetMinMaxValues(0, 100)
	bs:SetValueStep(5)
	getglobal("AgentCamConfigObsBgLow"):SetText("0%")
	getglobal("AgentCamConfigObsBgHigh"):SetText("100%")
	bs:SetScript("OnValueChanged", function()
		local v = math.floor(this:GetValue() + 0.5)
		getglobal("AgentCamConfigObsBgText"):SetText("Observe panel background " .. v .. "%" .. (v == 0 and " (transparent)" or ""))
		if AgentCamObs_SetBg and AgentCamObs_GetBg and math.floor(AgentCamObs_GetBg() * 100 + 0.5) ~= v then
			AgentCamObs_SetBg(v / 100)
		end
	end)
	config.obsBg = bs
	config.panel = Check("AgentCamConfigPanel", 26, -556, "Show panel", function()
		if this:GetChecked() then db.hidden = nil; frame:Show() else db.hidden = true; frame:Hide() end
	end)
	local keys = CreateFrame("Button", "AgentCamConfigKeys", config, "UIPanelButtonTemplate")
	keys:SetWidth(120)
	keys:SetHeight(22)
	keys:SetPoint("TOPLEFT", config, "TOPLEFT", 200, -556)
	keys:SetText("Bind Alt keys")
	keys:SetScript("OnClick", BindDefaultKeys)

	config.controls = { config.auto, config.slider, config.info, config.cam, config.panel, config.bars, config.obsBg, keys }
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
	Print("/ac auto [secs] | cam | gm | autogm | shot | info -- director, far camera, GM mode + invisibility (autogm: on at login), screenshot, overlay")
	Print("/ac obs [Name|auto|dock|bg 0-100] -- observe panel: cast, casts, cooldowns, auras; drag it anywhere, dock puts it back, bg sets its background opacity")
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
	elseif cmd == "invis" or cmd == "gm" then
		AgentCam_ToggleGMInvis()
	elseif cmd == "team" then
		AgentCam_Team(rest)
	elseif cmd == "autogm" then
		db.autoGM = (db.autoGM == false) and true or false
		Print(db.autoGM and "on login: GM mode on and invisible" or "on login: GM mode and visibility left as they are")
	elseif cmd == "shot" then
		AgentCam_Screenshot()
	elseif cmd == "obs" then
		if AgentCamObs_Toggle then AgentCamObs_Toggle(rest) end
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
	elseif cmd == "min" or cmd == "minimize" then
		AgentCam_ToggleCollapsed()
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
		db.classes = db.classes or {} -- [lower name] = class token, as last seen
		if db.info == nil then db.info = true end
		BuildFrame()
		HookOptionsAddOnsTab()
	elseif event == "PLAYER_ENTERING_WORLD" and db then
		if db.farCam then ApplyCamera() end
		-- The spectator logs in with GM mode on but visible: switch both on (the toolbar switch's
		-- "on" state) unless that is turned off (/ac autogm). The server's replies set the icon.
		if db.autoGM ~= false and db.enabled ~= false then
			Cmd(".gm on")
			Cmd(".gm visible off")
		end
		entered = true
		Subscribe()
		this:UnregisterEvent("PLAYER_ENTERING_WORLD")
	end
end)
