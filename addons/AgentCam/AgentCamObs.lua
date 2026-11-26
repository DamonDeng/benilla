-- AgentCamObs: the observe panel (/ac obs). What the viewed agent is doing: its state and target,
-- its party or raid, the spells it casts, its cooldowns, buffs and debuffs.
--
-- Two sources, best first:
--   1. The server's WOWOBS addon messages (CHAT_MSG_ADDON, prefix "WOWOBS"; see OBS_PROTOCOL.md):
--      exact numbers, cooldowns, auras with time left, the whole group.
--   2. The 1.12 combat log, when no server data has come for an agent: casts and auras the client
--      can see nearby, without cooldowns or time left; the group is the ticked agent list, with
--      the status AgentCam's own scan reads.
-- Lua 5.0 (1.12): no '#', varargs as arg, this/event/arg1 globals.

local PREFIX = "WOWOBS"
local SERVER_FRESH = 5 -- seconds a server record keeps an agent on source 1
local MAX_CASTS = 8
local MAX_CDS = 10
local MAX_AURAS = 16
local MAX_MEMBERS = 40
local QUESTION = "Interface\\Icons\\INV_Misc_QuestionMark"

local obs = {} -- [lower name] = record, see Get
local pinned -- lower name pinned with /ac obs <name>, or nil to follow the viewed agent
local panel
local odb -- AgentCamDB.obs

local function Lower(s) return s and string.lower(s) end

local function Get(name)
	local key = Lower(name)
	local o = obs[key]
	if not o then
		o = { name = name, casts = {}, cds = {}, buffs = {}, debuffs = {}, seen = -100, server = -100 }
		obs[key] = o
	end
	return o
end

local function Split(s, sep)
	local t, start = {}, 1
	while true do
		local i = string.find(s, sep, start, true)
		if not i then
			table.insert(t, string.sub(s, start))
			return t
		end
		table.insert(t, string.sub(s, start, i - 1))
		start = i + 1
	end
end

local function Num(s) return tonumber(s or "") or 0 end

-- An icon name ("Spell_Holy_Heal") or a full path, else the spell's icon learned earlier.
local function Icon(icon, spell)
	if icon and icon ~= "" then
		if not string.find(icon, "\\", 1, true) then icon = "Interface\\Icons\\" .. icon end
		if spell and spell ~= "" and odb then odb.icons[spell] = icon end
		return icon
	end
	return (spell and odb and odb.icons[spell]) or QUESTION
end

local function AddCast(o, c)
	table.insert(o.casts, 1, c)
	while table.getn(o.casts) > MAX_CASTS do table.remove(o.casts) end
end

------------------------------------------------------------------------------- server records

local groupParts = {} -- [lower name] = { members so far } while a multi-part G record arrives

local HANDLERS = {}

-- U;name;CLASS;level;hp;hpMax;power;powerMax;powerType;zone;flags;target
HANDLERS.U = function(f)
	local o = Get(f[2])
	o.class = f[3] ~= "" and string.upper(f[3]) or o.class
	o.level = Num(f[4])
	o.hp, o.hpMax, o.mp, o.mpMax, o.ptype = Num(f[5]), Num(f[6]), Num(f[7]), Num(f[8]), Num(f[9])
	o.zone = f[10]
	local flags = f[11] or ""
	o.dead = string.find(flags, "d", 1, true) ~= nil
	o.ghost = string.find(flags, "g", 1, true) ~= nil
	o.combat = string.find(flags, "c", 1, true) ~= nil
	o.offline = string.find(flags, "o", 1, true) ~= nil
	o.target = f[12] ~= "" and f[12] or nil
	if AgentCam_Feed then
		AgentCam_Feed(o.name, { class = o.class, hp = o.hp, hpMax = o.hpMax, mp = o.mp, mpMax = o.mpMax,
			ptype = o.ptype, dead = o.dead or o.ghost })
	end
end

-- G;name;P|R|-;leader;part/total;member,member,...
HANDLERS.G = function(f)
	local o = Get(f[2])
	local key = Lower(f[2])
	local _, _, part, total = string.find(f[5] or "", "^(%d+)/(%d+)$")
	part, total = tonumber(part) or 1, tonumber(total) or 1
	if part == 1 then groupParts[key] = {} end
	local acc = groupParts[key] or {}
	groupParts[key] = acc
	if f[6] and f[6] ~= "" then
		for _, m in ipairs(Split(f[6], ",")) do table.insert(acc, m) end
	end
	if part >= total then
		o.group = { kind = f[3], leader = f[4] ~= "" and f[4] or nil, members = acc }
		groupParts[key] = nil
	end
end

-- C;name;spellId;spell;icon;target;castMs  (castMs 0: instant, already done)
HANDLERS.C = function(f)
	local o = Get(f[2])
	local ms = Num(f[7])
	local c = { id = Num(f[3]), spell = f[4], icon = Icon(f[5], f[4]), target = f[6] ~= "" and f[6] or nil, t = GetTime() }
	if ms > 0 then
		c.state, c.ends = "casting", GetTime() + ms / 1000
		o.casting = c
	else
		c.state = "done"
	end
	AddCast(o, c)
end

-- S;name;spellId;spell;icon;target
HANDLERS.S = function(f)
	local o = Get(f[2])
	local id = Num(f[3])
	if o.casting and o.casting.id == id then
		o.casting.state, o.casting.t = "done", GetTime()
		o.casting = nil
		return
	end
	AddCast(o, { id = id, spell = f[4], icon = Icon(f[5], f[4]), target = f[6] ~= "" and f[6] or nil, t = GetTime(), state = "done" })
end

-- F;name;spellId;reason
HANDLERS.F = function(f)
	local o = Get(f[2])
	local id = Num(f[3])
	for _, c in ipairs(o.casts) do
		if c.id == id and c.state == "casting" then
			c.state, c.reason = "failed", f[4]
			break
		end
	end
	if o.casting and o.casting.id == id then o.casting = nil end
end

-- D;name;spellId;spell;icon;remainMs;durationMs  (remainMs 0: ready again)
HANDLERS.D = function(f)
	local o = Get(f[2])
	local id = Num(f[3])
	local remain, dur = Num(f[6]) / 1000, Num(f[7]) / 1000
	if remain <= 0 then
		o.cds[id] = nil
		return
	end
	if dur < remain then dur = remain end
	o.cds[id] = { spell = f[4], icon = Icon(f[5], f[4]), start = GetTime() + remain - dur, dur = dur }
end

-- A;name;B|D;spellId:icon:remainMs:stacks,...  (the whole list of that kind; remainMs 0: no end)
HANDLERS.A = function(f)
	local o = Get(f[2])
	local list = {}
	if f[4] and f[4] ~= "" then
		for _, item in ipairs(Split(f[4], ",")) do
			local p = Split(item, ":")
			local remain = Num(p[3]) / 1000
			table.insert(list, { id = Num(p[1]), icon = Icon(p[2]), ends = remain > 0 and GetTime() + remain or nil, stacks = Num(p[4]) })
		end
	end
	if f[3] == "D" then o.debuffs = list else o.buffs = list end
	o.auraByLog = nil
end

local function OnRecord(text)
	local f = Split(text, ";")
	local h = HANDLERS[f[1]]
	if not h or not f[2] or f[2] == "" then return end
	h(f)
	local o = Get(f[2])
	o.seen, o.server = GetTime(), GetTime()
end

------------------------------------------------------------------------------- combat log fallback

-- A GlobalStrings format ("%s casts %s on %s.") as a Lua pattern with captures.
local function Pattern(fmt)
	if not fmt then return nil end
	local p = string.gsub(fmt, "([%(%)%.%[%]%*%+%-%?%^%$])", "%%%1")
	p = string.gsub(p, "%%%d%$s", "(.+)") -- positional forms of other locales
	p = string.gsub(p, "%%%d%$d", "(%%d+)")
	p = string.gsub(p, "%%s", "(.+)")
	p = string.gsub(p, "%%d", "(%%d+)")
	return "^" .. p .. "$"
end

-- { GlobalStrings key, what, the capture order of unit, spell, target }
local LOG_LINES = {
	{ "SPELLCASTOTHERSTART", "start", 1, 2 },
	{ "SPELLPERFORMOTHERSTART", "start", 1, 2 },
	{ "SPELLCASTGOOTHERTARGETTED", "done", 1, 2, 3 },
	{ "SPELLCASTGOOTHER", "done", 1, 2 },
	{ "SPELLPERFORMGOOTHERTARGETTED", "done", 1, 2, 3 },
	{ "SPELLPERFORMGOOTHER", "done", 1, 2 },
	{ "SPELLLOGCRITOTHEROTHER", "done", 1, 2, 3 },
	{ "SPELLLOGOTHEROTHER", "done", 1, 2, 3 },
	{ "SPELLLOGCRITOTHERSELF", "done", 1, 2 },
	{ "SPELLLOGOTHERSELF", "done", 1, 2 },
	{ "HEALEDCRITOTHEROTHER", "done", 1, 2, 3 },
	{ "HEALEDOTHEROTHER", "done", 1, 2, 3 },
	{ "HEALEDCRITOTHERSELF", "done", 1, 2 },
	{ "HEALEDOTHERSELF", "done", 1, 2 },
	{ "AURAADDEDOTHERHELPFUL", "buff", 1, 2 },
	{ "AURAADDEDOTHERHARMFUL", "debuff", 1, 2 },
	{ "AURAREMOVEDOTHER", "fade", 2, 1 },
}
local compiled

local function Compile()
	compiled = {}
	for _, l in ipairs(LOG_LINES) do
		local p = Pattern(getglobal(l[1]))
		if p then table.insert(compiled, { p = p, what = l[2], u = l[3], s = l[4], t = l[5] }) end
	end
end

local function IsAgent(name)
	local key = Lower(name)
	if obs[key] then return true end
	if AgentCam_Agents then
		for _, n in ipairs(AgentCam_Agents()) do
			if Lower(n) == key then return true end
		end
	end
	return false
end

local function SetAura(list, spell, on)
	for i = table.getn(list), 1, -1 do
		if list[i].spell == spell then table.remove(list, i) end
	end
	if on and table.getn(list) < MAX_AURAS then
		table.insert(list, { spell = spell, icon = Icon(nil, spell) })
	end
end

local function OnLogLine(msg)
	if not compiled then Compile() end
	for _, c in ipairs(compiled) do
		local cap = { string.find(msg, c.p) }
		if cap[1] then
			local unit, spell = cap[c.u + 2], cap[c.s + 2]
			local target = c.t and cap[c.t + 2] or nil
			if not unit or not IsAgent(unit) then return end
			local o = Get(unit)
			if GetTime() - o.server < SERVER_FRESH then return end -- the server says it better
			o.seen = GetTime()
			if c.what == "start" then
				local cast = { spell = spell, icon = Icon(nil, spell), t = GetTime(), state = "casting" }
				o.casting = cast
				AddCast(o, cast)
			elseif c.what == "done" then
				local last = o.casts[1]
				if o.casting and o.casting.spell == spell then
					o.casting.state, o.casting.t, o.casting.target = "done", GetTime(), target or o.casting.target
					o.casting = nil
				elseif not (last and last.spell == spell and GetTime() - last.t < 0.5) then
					-- one cast can log twice (casts X, X hits Y): keep one row
					AddCast(o, { spell = spell, icon = Icon(nil, spell), target = target, t = GetTime(), state = "done" })
				elseif target and not last.target then
					last.target = target
				end
			elseif c.what == "buff" then
				SetAura(o.buffs, spell, true); o.auraByLog = true
			elseif c.what == "debuff" then
				SetAura(o.debuffs, spell, true); o.auraByLog = true
			elseif c.what == "fade" then
				SetAura(o.buffs, spell, false); SetAura(o.debuffs, spell, false)
			end
			return
		end
	end
end

------------------------------------------------------------------------------- panel

local W = 300
local CLASS_SHEET = "Interface\\Glues\\CharacterCreate\\UI-CharacterCreate-Classes"
local CLASS_TCOORDS = {
	WARRIOR = { 0, 0.25, 0, 0.25 }, MAGE = { 0.25, 0.49609375, 0, 0.25 },
	ROGUE = { 0.49609375, 0.7421875, 0, 0.25 }, DRUID = { 0.7421875, 0.98828125, 0, 0.25 },
	HUNTER = { 0, 0.25, 0.25, 0.5 }, SHAMAN = { 0.25, 0.49609375, 0.25, 0.5 },
	PRIEST = { 0.49609375, 0.7421875, 0.25, 0.5 }, WARLOCK = { 0.7421875, 0.98828125, 0.25, 0.5 },
	PALADIN = { 0, 0.25, 0.5, 0.75 },
}
local POWER_COLORS = { [0] = { 0, 0, 1 }, [1] = { 1, 0, 0 }, [2] = { 1, 0.5, 0.25 }, [3] = { 1, 1, 0 } }

local function ClassColor(class)
	local c = class and RAID_CLASS_COLORS and RAID_CLASS_COLORS[class]
	if c then return c.r, c.g, c.b end
	return 0.8, 0.8, 0.8
end

local function Bar(parent, w, h)
	local b = CreateFrame("StatusBar", nil, parent)
	b:SetWidth(w)
	b:SetHeight(h)
	b:SetStatusBarTexture("Interface\\TargetingFrame\\UI-StatusBar")
	b:SetMinMaxValues(0, 1)
	b.bg = b:CreateTexture(nil, "BACKGROUND")
	b.bg:SetAllPoints(b)
	b.bg:SetTexture(0, 0, 0, 0.5)
	b.text = b:CreateFontString(nil, "OVERLAY", "GameFontHighlightSmall")
	b.text:SetPoint("CENTER", b, "CENTER", 0, 0)
	return b
end

local function SetBar(b, v, max, r, g, bl, text)
	b:SetMinMaxValues(0, (max and max > 0) and max or 1)
	b:SetValue(v or 0)
	b:SetStatusBarColor(r, g, bl)
	b.text:SetText(text or "")
end

local function Label(parent, text)
	local l = parent:CreateFontString(nil, "OVERLAY", "GameFontNormalSmall")
	l:SetText(text)
	return l
end

-- A row of icon buttons with a cooldown sweep and a corner text; tooltips from button.tip.
local function IconRow(parent, n, size, name)
	local row = {}
	for i = 1, n do
		local b = CreateFrame("Button", name .. i, parent)
		b:SetWidth(size)
		b:SetHeight(size)
		b.icon = b:CreateTexture(nil, "ARTWORK")
		b.icon:SetAllPoints(b)
		b.cd = CreateFrame("Model", name .. i .. "Cooldown", b, "CooldownFrameTemplate")
		b.cd:SetAllPoints(b)
		b.text = b:CreateFontString(nil, "OVERLAY", "NumberFontNormalSmall")
		b.text:SetPoint("BOTTOMRIGHT", b, "BOTTOMRIGHT", 1, 1)
		b:SetScript("OnEnter", function()
			if not this.tip then return end
			GameTooltip:SetOwner(this, "ANCHOR_BOTTOMRIGHT")
			GameTooltip:SetText(this.tip[1])
			for k = 2, table.getn(this.tip) do GameTooltip:AddLine(this.tip[k], 1, 1, 1) end
			GameTooltip:Show()
		end)
		b:SetScript("OnLeave", function() GameTooltip:Hide() end)
		b:Hide()
		row[i] = b
	end
	return row
end

local function Place(row, parent, y, size, gap, perLine)
	for i, b in ipairs(row) do
		local col, line = math.mod(i - 1, perLine), math.floor((i - 1) / perLine)
		b:ClearAllPoints()
		b:SetPoint("TOPLEFT", parent, "TOPLEFT", 10 + col * (size + gap), y - line * (size + gap))
	end
end

local function Build()
	panel = CreateFrame("Frame", "AgentCamObs", UIParent)
	panel:SetWidth(W)
	panel:SetHeight(400)
	panel:SetFrameStrata("MEDIUM")
	panel:SetBackdrop({
		bgFile = "Interface\\Tooltips\\UI-Tooltip-Background",
		edgeFile = "Interface\\Tooltips\\UI-Tooltip-Border",
		tile = true, tileSize = 16, edgeSize = 12,
		insets = { left = 3, right = 3, top = 3, bottom = 3 },
	})
	panel:SetBackdropColor(0, 0, 0, 0.75)
	panel:SetMovable(true)
	panel:EnableMouse(true)
	panel:RegisterForDrag("LeftButton")
	panel:SetScript("OnDragStart", function() this:StartMoving() end)
	panel:SetScript("OnDragStop", function()
		this:StopMovingOrSizing()
		local p, _, rp, x, y = this:GetPoint()
		odb.pos = { p, rp, x, y }
	end)
	if odb.pos then
		panel:SetPoint(odb.pos[1], UIParent, odb.pos[2], odb.pos[3], odb.pos[4])
	else
		panel:SetPoint("TOPRIGHT", UIParent, "TOPRIGHT", -220, -200)
	end

	local close = CreateFrame("Button", "AgentCamObsClose", panel, "UIPanelCloseButton")
	close:SetWidth(24)
	close:SetHeight(24)
	close:SetPoint("TOPRIGHT", panel, "TOPRIGHT", 0, 0)
	close:SetScript("OnClick", function() AgentCamObs_Toggle("hide") end)

	local y = -8
	panel.classIcon = panel:CreateTexture(nil, "ARTWORK")
	panel.classIcon:SetWidth(28)
	panel.classIcon:SetHeight(28)
	panel.classIcon:SetTexture(CLASS_SHEET)
	panel.classIcon:SetPoint("TOPLEFT", panel, "TOPLEFT", 8, y)
	panel.title = panel:CreateFontString(nil, "OVERLAY", "GameFontNormal")
	panel.title:SetPoint("TOPLEFT", panel.classIcon, "TOPRIGHT", 6, -1)
	panel.title:SetJustifyH("LEFT")
	panel.sub = panel:CreateFontString(nil, "OVERLAY", "GameFontHighlightSmall")
	panel.sub:SetPoint("TOPLEFT", panel.title, "BOTTOMLEFT", 0, -2)
	panel.sub:SetJustifyH("LEFT")
	y = y - 32
	panel.hp = Bar(panel, W - 20, 12)
	panel.hp:SetPoint("TOPLEFT", panel, "TOPLEFT", 10, y)
	y = y - 13
	panel.mp = Bar(panel, W - 20, 8)
	panel.mp:SetPoint("TOPLEFT", panel, "TOPLEFT", 10, y)
	y = y - 12
	panel.cast = Bar(panel, W - 20, 12)
	panel.cast:SetPoint("TOPLEFT", panel, "TOPLEFT", 10, y)
	panel.castY = y

	panel.groupLabel = Label(panel, "Group")
	panel.members = {}
	for i = 1, MAX_MEMBERS do
		local m = Bar(panel, 54, 12)
		m.text:SetFontObject(GameFontHighlightSmall)
		m.text:ClearAllPoints()
		m.text:SetPoint("LEFT", m, "LEFT", 2, 0)
		m.text:SetWidth(52)
		m.text:SetJustifyH("LEFT")
		m:Hide()
		panel.members[i] = m
	end

	panel.castLabel = Label(panel, "Recent casts")
	panel.casts = IconRow(panel, MAX_CASTS, 26, "AgentCamObsCast")
	panel.cdLabel = Label(panel, "Cooldowns")
	panel.cds = IconRow(panel, MAX_CDS, 24, "AgentCamObsCD")
	panel.buffLabel = Label(panel, "Buffs")
	panel.buffs = IconRow(panel, MAX_AURAS, 18, "AgentCamObsBuff")
	panel.debuffLabel = Label(panel, "Debuffs")
	panel.debuffs = IconRow(panel, MAX_AURAS, 18, "AgentCamObsDebuff")
	panel.source = panel:CreateFontString(nil, "OVERLAY", "GameFontDisableSmall")
	panel.source:SetJustifyH("LEFT")
	panel.empty = panel:CreateFontString(nil, "OVERLAY", "GameFontDisableSmall")
	panel.empty:SetText("none")
	panel.emptyCd = panel:CreateFontString(nil, "OVERLAY", "GameFontDisableSmall")
	panel.emptyCd:SetText("none")
	panel:Hide()
end

-- The agent the panel shows: the pinned one, the viewed one, the target when it is an agent,
-- else the first ticked agent.
local function Subject()
	if pinned then return Get(pinned) end
	local n = AgentCam_Watched and AgentCam_Watched()
	if not n then
		local t = UnitName("target")
		if t and IsAgent(t) then n = t end
	end
	if not n and AgentCam_Agents then n = AgentCam_Agents()[1] end
	return n and Get(n) or nil
end

-- Status for a member: the server's record, else AgentCam's scan.
local function MemberStatus(name)
	local o = obs[Lower(name)]
	if o and o.hpMax then return o.hp, o.hpMax, o.class, o.dead or o.ghost, o.offline end
	local st, class = nil, nil
	if AgentCam_GetStats then st, class = AgentCam_GetStats(name) end
	if st then return st.hp, st.hpMax, class, st.dead end
	return nil, nil, class
end

local function Age(t)
	local s = math.floor(GetTime() - t)
	if s < 60 then return s .. "s" end
	return math.floor(s / 60) .. "m"
end

local function FillAuras(row, list, isDebuff)
	local n = 0
	for i, b in ipairs(row) do
		local a = list[i]
		if a then
			n = i
			b.icon:SetTexture(a.icon or QUESTION)
			local left = a.ends and a.ends - GetTime()
			b.text:SetText((a.stacks and a.stacks > 1) and a.stacks or (left and left > 0 and left < 60 and math.floor(left) or ""))
			b.tip = { a.spell or ("Spell " .. (a.id or "?")) }
			if left and left > 0 then table.insert(b.tip, "Time left: " .. math.floor(left) .. "s") end
			if isDebuff then b.icon:SetVertexColor(1, 0.7, 0.7) else b.icon:SetVertexColor(1, 1, 1) end
			b:Show()
		else
			b:Hide()
		end
	end
	return n
end

local function Refresh()
	if not panel or not panel:IsShown() then return end
	local o = Subject()
	if not o then
		panel.title:SetText("No agent")
		panel.sub:SetText("Tick an agent in /ac config")
		return
	end
	local hp, hpMax, class = MemberStatus(o.name)
	class = o.class or class
	local r, g, b = ClassColor(class)
	panel.title:SetText(o.name)
	panel.title:SetTextColor(r, g, b)
	local tc = CLASS_TCOORDS[class or ""]
	if tc then
		panel.classIcon:SetTexCoord(tc[1], tc[2], tc[3], tc[4]); panel.classIcon:Show()
	else
		panel.classIcon:Hide()
	end
	local sub = {}
	if o.level and o.level > 0 then table.insert(sub, "Lv " .. o.level) end
	if o.zone and o.zone ~= "" then table.insert(sub, o.zone) end
	if o.dead then table.insert(sub, "|cffff4040dead|r") elseif o.ghost then table.insert(sub, "|cffaaaaaaghost|r") end
	if o.combat then table.insert(sub, "|cffff8000in combat|r") end
	if o.offline then table.insert(sub, "|cff808080offline|r") end
	if o.target then table.insert(sub, "-> " .. o.target) end
	panel.sub:SetText(table.concat(sub, "  "))

	-- health, power, cast
	if hpMax and hpMax > 0 then
		SetBar(panel.hp, hp, hpMax, 0.1, 0.8, 0.1, hp .. " / " .. hpMax)
	else
		SetBar(panel.hp, 0, 1, 0.3, 0.3, 0.3, "health unknown")
	end
	local st = AgentCam_GetStats and AgentCam_GetStats(o.name)
	local mp, mpMax, ptype = o.mp, o.mpMax, o.ptype
	if not mpMax and st then mp, mpMax, ptype = st.mp, st.mpMax, st.ptype end
	local pc = POWER_COLORS[ptype or 0] or POWER_COLORS[0]
	SetBar(panel.mp, mp or 0, mpMax or 1, pc[1], pc[2], pc[3], mpMax and mpMax > 0 and (mp .. " / " .. mpMax) or "")
	local c = o.casting
	if c and c.ends and c.ends < GetTime() - 1 then o.casting, c = nil, nil end
	if c then
		local total = c.ends and (c.ends - c.t) or 0
		local done = GetTime() - c.t
		SetBar(panel.cast, total > 0 and math.min(done, total) or 1, total > 0 and total or 1, 1, 0.7, 0,
			c.spell .. (c.target and (" -> " .. c.target) or ""))
	else
		SetBar(panel.cast, 0, 1, 1, 0.7, 0, "|cff808080not casting|r")
	end

	local y = panel.castY - 18

	-- group: the server's roster, else the ticked agents
	local members, head
	if o.group and o.group.kind ~= "-" then
		members = o.group.members
		head = (o.group.kind == "R" and "Raid " or "Party ") .. table.getn(members)
		if o.group.leader then head = head .. "  leader " .. o.group.leader end
	elseif o.group then
		members, head = { o.name }, "Not in a group"
	else
		members = AgentCam_Agents and AgentCam_Agents() or {}
		head = "Agents " .. table.getn(members) .. " (no group data from the server)"
	end
	panel.groupLabel:ClearAllPoints()
	panel.groupLabel:SetPoint("TOPLEFT", panel, "TOPLEFT", 10, y)
	panel.groupLabel:SetText(head)
	y = y - 14
	local perLine = 5
	local cellW = math.floor((W - 20 - (perLine - 1) * 2) / perLine)
	local shown = 0
	for i, m in ipairs(panel.members) do
		local name = members[i]
		if name then
			shown = i
			local mhp, mmax, mclass, mdead, moff = MemberStatus(name)
			local cr, cg, cb = ClassColor(mclass)
			m:SetWidth(cellW)
			m.text:SetWidth(cellW - 4)
			m:ClearAllPoints()
			m:SetPoint("TOPLEFT", panel, "TOPLEFT", 10 + math.mod(i - 1, perLine) * (cellW + 2), y - math.floor((i - 1) / perLine) * 14)
			local label = name
			if Lower(name) == Lower(o.name) then label = "> " .. name end
			if mdead then label = label .. " x" elseif moff then label = label .. " off" end
			if mmax and mmax > 0 then
				SetBar(m, mhp, mmax, cr, cg, cb, label)
			else
				SetBar(m, 0, 1, cr, cg, cb, label)
			end
			m:Show()
		else
			m:Hide()
		end
	end
	y = y - math.ceil(shown / perLine) * 14 - 6

	-- recent casts
	panel.castLabel:ClearAllPoints()
	panel.castLabel:SetPoint("TOPLEFT", panel, "TOPLEFT", 10, y)
	y = y - 14
	Place(panel.casts, panel, y, 26, 4, MAX_CASTS)
	local nc = 0
	for i, bt in ipairs(panel.casts) do
		local cs = o.casts[i]
		if cs then
			nc = i
			bt.icon:SetTexture(cs.icon or QUESTION)
			bt.icon:SetVertexColor(1, 1, 1)
			if cs.state == "failed" then bt.icon:SetVertexColor(1, 0.3, 0.3) end
			bt.text:SetText(Age(cs.t))
			bt.tip = { cs.spell or ("Spell " .. (cs.id or "?")) }
			if cs.target then table.insert(bt.tip, "on " .. cs.target) end
			table.insert(bt.tip, (cs.state == "casting" and "casting, " or cs.state == "failed" and ("failed" .. (cs.reason and (" (" .. cs.reason .. ")") or "") .. ", ") or "") .. Age(cs.t) .. " ago")
			bt:Show()
		else
			bt:Hide()
		end
	end
	panel.empty:ClearAllPoints()
	panel.empty:SetPoint("TOPLEFT", panel, "TOPLEFT", 12, y - 6)
	if nc == 0 then panel.empty:Show() else panel.empty:Hide() end
	y = y - 26 - 8

	-- cooldowns, soonest ready first
	panel.cdLabel:ClearAllPoints()
	panel.cdLabel:SetPoint("TOPLEFT", panel, "TOPLEFT", 10, y)
	y = y - 14
	Place(panel.cds, panel, y, 24, 4, MAX_CDS)
	local cds = {}
	for id, cd in pairs(o.cds) do
		if cd.start + cd.dur <= GetTime() then
			o.cds[id] = nil
		else
			table.insert(cds, cd)
		end
	end
	table.sort(cds, function(a, b2) return a.start + a.dur < b2.start + b2.dur end)
	for i, bt in ipairs(panel.cds) do
		local cd = cds[i]
		if cd then
			bt.icon:SetTexture(cd.icon or QUESTION)
			if bt.cdStart ~= cd.start then
				CooldownFrame_SetTimer(bt.cd, cd.start, cd.dur, 1)
				bt.cdStart = cd.start
			end
			local left = cd.start + cd.dur - GetTime()
			bt.text:SetText(left >= 60 and (math.floor(left / 60) .. "m") or math.floor(left))
			bt.tip = { cd.spell or "?", "Ready in " .. math.floor(left) .. "s" }
			bt:Show()
		else
			bt.cdStart = nil
			bt:Hide()
		end
	end
	panel.emptyCd:ClearAllPoints()
	panel.emptyCd:SetPoint("TOPLEFT", panel, "TOPLEFT", 12, y - 6)
	if table.getn(cds) == 0 then
		panel.emptyCd:SetText(GetTime() - o.server < SERVER_FRESH and "none" or "needs server data")
		panel.emptyCd:Show()
	else
		panel.emptyCd:Hide()
	end
	y = y - 24 - 8

	-- buffs, debuffs
	panel.buffLabel:ClearAllPoints()
	panel.buffLabel:SetPoint("TOPLEFT", panel, "TOPLEFT", 10, y)
	y = y - 14
	Place(panel.buffs, panel, y, 18, 2, 14)
	local nb = FillAuras(panel.buffs, o.buffs, false)
	y = y - math.max(1, math.ceil(nb / 14)) * 20 - 4
	panel.debuffLabel:ClearAllPoints()
	panel.debuffLabel:SetPoint("TOPLEFT", panel, "TOPLEFT", 10, y)
	y = y - 14
	Place(panel.debuffs, panel, y, 18, 2, 14)
	local nd = FillAuras(panel.debuffs, o.debuffs, true)
	y = y - math.max(1, math.ceil(nd / 14)) * 20 - 4

	panel.source:ClearAllPoints()
	panel.source:SetPoint("TOPLEFT", panel, "TOPLEFT", 10, y)
	if GetTime() - o.server < SERVER_FRESH then
		panel.source:SetText("Source: server (WOWOBS)" .. (pinned and "  pinned" or ""))
	else
		panel.source:SetText("Source: combat log nearby, no cooldowns" .. (pinned and "  pinned" or ""))
	end
	panel:SetHeight(-y + 18)
end

function AgentCamObs_Toggle(arg)
	if not panel then return end
	arg = arg or ""
	local low = string.lower(arg)
	if low == "hide" or low == "off" then
		panel:Hide()
		odb.shown = nil
		return
	end
	if low == "auto" then
		pinned = nil
	elseif arg ~= "" and low ~= "show" and low ~= "on" then
		pinned = Lower(arg)
		Get(arg)
	end
	if low == "" and panel:IsShown() then
		panel:Hide()
		odb.shown = nil
		return
	end
	panel:Show()
	odb.shown = true
	Refresh()
end

-- For tests and other addons: feed one WOWOBS record as if the server had sent it.
function AgentCamObs_Record(text)
	OnRecord(text)
	Refresh()
end

function AgentCamObs_Get(name)
	return obs[Lower(name)]
end

------------------------------------------------------------------------------- events

local LOG_EVENTS = {
	"CHAT_MSG_SPELL_PARTY_DAMAGE", "CHAT_MSG_SPELL_PARTY_BUFF",
	"CHAT_MSG_SPELL_FRIENDLYPLAYER_DAMAGE", "CHAT_MSG_SPELL_FRIENDLYPLAYER_BUFF",
	"CHAT_MSG_SPELL_HOSTILEPLAYER_DAMAGE", "CHAT_MSG_SPELL_HOSTILEPLAYER_BUFF",
	"CHAT_MSG_SPELL_PERIODIC_PARTY_BUFFS", "CHAT_MSG_SPELL_PERIODIC_PARTY_DAMAGE",
	"CHAT_MSG_SPELL_PERIODIC_FRIENDLYPLAYER_BUFFS", "CHAT_MSG_SPELL_PERIODIC_FRIENDLYPLAYER_DAMAGE",
	"CHAT_MSG_SPELL_PERIODIC_HOSTILEPLAYER_BUFFS", "CHAT_MSG_SPELL_PERIODIC_HOSTILEPLAYER_DAMAGE",
	"CHAT_MSG_SPELL_AURA_GONE_PARTY", "CHAT_MSG_SPELL_AURA_GONE_OTHER",
	"CHAT_MSG_SPELL_CREATURE_VS_PARTY_DAMAGE", "CHAT_MSG_SPELL_CREATURE_VS_CREATURE_DAMAGE",
}

local ev = CreateFrame("Frame")
ev:RegisterEvent("ADDON_LOADED")
ev:RegisterEvent("CHAT_MSG_ADDON")
ev:RegisterEvent("CHAT_MSG_SYSTEM")
for _, e in ipairs(LOG_EVENTS) do ev:RegisterEvent(e) end
ev:SetScript("OnEvent", function()
	if event == "ADDON_LOADED" then
		if arg1 ~= "AgentCam" then return end
		AgentCamDB = AgentCamDB or {}
		AgentCamDB.obs = AgentCamDB.obs or {}
		odb = AgentCamDB.obs
		odb.icons = odb.icons or {}
		Build()
		if odb.shown then panel:Show() end
	elseif not odb then
		return
	elseif event == "CHAT_MSG_ADDON" then
		if arg1 == PREFIX and arg2 then OnRecord(arg2) end
	elseif event == "CHAT_MSG_SYSTEM" then
		-- the probe's fallback channel: a system line "[WOWOBS] <record>"
		local _, _, rec = string.find(arg1 or "", "^%[WOWOBS%]%s*(.+)$")
		if rec then OnRecord(rec) end
	elseif arg1 then
		OnLogLine(arg1)
	end
end)

local elapsed = 0
ev:SetScript("OnUpdate", function()
	elapsed = elapsed + arg1
	if elapsed < 0.2 then return end
	elapsed = 0
	Refresh()
end)
