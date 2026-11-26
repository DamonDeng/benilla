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
	if spell and odb and odb.icons[spell] then return odb.icons[spell] end
	-- the combat log names spells only: the table generated from Spell.dbc (AgentCamSpellIcons.lua)
	local file = spell and AgentCam_SpellIcons and AgentCam_SpellIcons[spell]
	if file then return "Interface\\Icons\\" .. file end
	return QUESTION
end

local function AddCast(o, c)
	table.insert(o.casts, 1, c)
	while table.getn(o.casts) > MAX_CASTS do table.remove(o.casts) end
end

------------------------------------------------------------------------------- server records

local groupParts = {} -- [lower name] = { members so far } while a multi-part G record arrives

local HANDLERS = {}

-- U;name;CLASS;level;hp;hpMax;power;powerMax;powerType;zone;flags;target[;combo;xp;xpMax]
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
	-- optional tail, absent from older servers
	o.combo = f[13] and f[13] ~= "" and Num(f[13]) or nil
	o.xp = f[14] and f[14] ~= "" and Num(f[14]) or nil
	o.xpMax = f[15] and f[15] ~= "" and Num(f[15]) or nil
	if AgentCam_Feed then
		AgentCam_Feed(o.name, { class = o.class, hp = o.hp, hpMax = o.hpMax, mp = o.mp, mpMax = o.mpMax,
			ptype = o.ptype, dead = o.dead or o.ghost, offline = o.offline, xp = o.xp, xpMax = o.xpMax })
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

-- A;name;B|D;[part/total;]spellId:icon:remainMs:stacks,...  (the whole list of that kind;
-- remainMs 0: no end). One message holds about five auras, so a longer list comes in parts
-- 1/n .. n/n, shown when the last arrives; without the part field the record is the whole list.
local auraParts = {} -- [lower name .. kind] = { auras so far }
HANDLERS.A = function(f)
	local o = Get(f[2])
	local items = f[4]
	local _, _, part, total = string.find(f[4] or "", "^(%d+)/(%d+)$")
	part, total = tonumber(part), tonumber(total)
	local key = Lower(f[2]) .. (f[3] or "")
	local list
	if part then
		items = f[5]
		if part == 1 then auraParts[key] = {} end
		list = auraParts[key] or {}
		auraParts[key] = list
	else
		list = {}
	end
	if items and items ~= "" then
		for _, item in ipairs(Split(items, ",")) do
			local p = Split(item, ":")
			local remain = Num(p[3]) / 1000
			-- optional 5th part: the aura's name, for the tooltip and the icon table when icon is empty
			local nm = p[5] ~= "" and p[5] or nil
			table.insert(list, { id = Num(p[1]), spell = nm, icon = Icon(p[2], nm), ends = remain > 0 and GetTime() + remain or nil, stacks = Num(p[4]) })
		end
	end
	if part and part < total then return end -- wait for the rest
	auraParts[key] = nil
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

-- Compact, one line per kind: a small label, then the icons with their time under them as the
-- stock buff bar does. A kind with nothing to show takes no room, and the panel's height follows.
local W = 240
local PAD = 8
local LABEL_W = 34 -- the label column; icons start after it
local CAST_ICON = 20
local SIZES = { casts = 22, cds = 22, buffs = 20, debuffs = 20 }
local GAP = 3
local TIME_H = 10 -- the time line under an icon

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
	b.text:SetPoint("LEFT", b, "LEFT", 4, 0)
	b.text:SetPoint("RIGHT", b, "RIGHT", -30, 0)
	b.text:SetJustifyH("LEFT")
	b.text:SetHeight(12)
	b.right = b:CreateFontString(nil, "OVERLAY", "GameFontHighlightSmall")
	b.right:SetPoint("RIGHT", b, "RIGHT", -3, 0)
	return b
end

local function SetBar(b, v, max, r, g, bl, text, right)
	b:SetMinMaxValues(0, (max and max > 0) and max or 1)
	b:SetValue(v or 0)
	b:SetStatusBarColor(r, g, bl)
	b.text:SetText(text or "")
	b.right:SetText(right or "")
end

-- Seconds as the stock buff bar abbreviates them: 12, 9m, 2h.
local function Short(s)
	s = math.floor(s)
	if s < 60 then return tostring(s) end
	if s < 3600 then return math.floor(s / 60) .. "m" end
	return math.floor(s / 3600) .. "h"
end

-- A row of icon buttons with a cooldown sweep, a stack count in the corner and a time under it;
-- tooltips from button.tip.
local function IconRow(parent, n, size, name)
	local row = {}
	for i = 1, n do
		local b = CreateFrame("Button", name .. i, parent)
		b:SetWidth(size)
		b:SetHeight(size)
		b.icon = b:CreateTexture(nil, "ARTWORK")
		b.icon:SetAllPoints(b)
		-- the stock debuff border (BuffFrame.xml's overlay crop), tinted like DebuffTypeColor
		b.border = b:CreateTexture(nil, "OVERLAY")
		b.border:SetTexture("Interface\\Buttons\\UI-Debuff-Overlays")
		b.border:SetTexCoord(0.296875, 0.5703125, 0, 0.515625)
		b.border:SetPoint("TOPLEFT", b, "TOPLEFT", -1, 1)
		b.border:SetPoint("BOTTOMRIGHT", b, "BOTTOMRIGHT", 1, -1)
		b.border:Hide()
		b.cd = CreateFrame("Model", name .. i .. "Cooldown", b, "CooldownFrameTemplate")
		b.cd:SetAllPoints(b)
		b.text = b:CreateFontString(nil, "OVERLAY", "NumberFontNormalSmall")
		b.text:SetPoint("BOTTOMRIGHT", b, "BOTTOMRIGHT", 1, 1)
		b.time = b:CreateFontString(nil, "OVERLAY", "GameFontHighlightSmall")
		b.time:SetPoint("TOP", b, "BOTTOM", 0, -1)
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

-- One section at y: its label and its first `n` icons, wrapped to as many lines as they need.
-- Returns the y below it; nothing shown and y unchanged when n is 0.
local function Section(label, row, size, n, y)
	local perLine = math.max(1, math.floor((W - PAD - LABEL_W + GAP) / (size + GAP)))
	if n == 0 then
		label:Hide()
		for _, b in ipairs(row) do b:Hide() end
		return y
	end
	label:ClearAllPoints()
	label:SetPoint("TOPLEFT", panel, "TOPLEFT", PAD, y - math.floor((size - 10) / 2))
	label:Show()
	local lineH = size + TIME_H + 3
	for i, b in ipairs(row) do
		if i <= n then
			local col, line = math.mod(i - 1, perLine), math.floor((i - 1) / perLine)
			b:ClearAllPoints()
			b:SetPoint("TOPLEFT", panel, "TOPLEFT", PAD + LABEL_W + col * (size + GAP), y - line * lineH)
			b:Show()
		else
			b:Hide()
		end
	end
	return y - math.ceil(n / perLine) * lineH - 2
end

local function Build()
	-- A child of the AgentCam panel: it hides, folds and scales with it. It starts docked beside it
	-- (Dock); dragged, it stays where it was dropped (odb.pos), and /ac obs dock puts it back.
	panel = CreateFrame("Frame", "AgentCamObs", AgentCamFrame or UIParent)
	panel:SetWidth(W)
	panel:SetHeight(40)
	panel:SetFrameStrata("MEDIUM")
	panel:SetBackdrop({
		bgFile = "Interface\\Tooltips\\UI-Tooltip-Background",
		edgeFile = "Interface\\Tooltips\\UI-Tooltip-Border",
		tile = true, tileSize = 16, edgeSize = 12,
		insets = { left = 3, right = 3, top = 3, bottom = 3 },
	})
	panel.ApplyBg = function()
		-- odb.bg: background opacity 0..1 (settings slider, /ac obs bg <0-100>); 0 also drops the border
		local a = odb.bg or 0.75
		panel:SetBackdropColor(0, 0, 0, a)
		panel:SetBackdropBorderColor(1, 1, 1, a > 0 and 1 or 0)
	end
	panel.ApplyBg()
	panel:SetMovable(true)
	panel:EnableMouse(true)
	panel:RegisterForDrag("LeftButton")
	panel:SetScript("OnDragStart", function() this:StartMoving() end)
	panel:SetScript("OnDragStop", function()
		this:StopMovingOrSizing()
		-- saved as a top-left corner on UIParent, in this frame's own scale
		odb.pos = { this:GetLeft(), this:GetTop() }
		this:ClearAllPoints()
		this:SetPoint("TOPLEFT", UIParent, "BOTTOMLEFT", odb.pos[1], odb.pos[2])
		this.side = "free"
	end)
	if type(odb.pos) ~= "table" or type(odb.pos[1]) ~= "number" then odb.pos = nil end -- older layouts

	local close = CreateFrame("Button", "AgentCamObsClose", panel, "UIPanelCloseButton")
	close:SetWidth(20)
	close:SetHeight(20)
	close:SetPoint("TOPRIGHT", panel, "TOPRIGHT", 1, 1)
	close:SetScript("OnClick", function() AgentCamObs_Toggle("hide") end)

	-- Only what the agent is doing. Its name, health, power and experience are its row in the
	-- AgentCam panel. Combo points (server data only) sit above the cast bar.
	panel.combo = {}
	panel.pipW = math.floor((W - 2 * PAD - 20 - 4 * 3) / 5)
	for i = 1, 5 do
		local t = panel:CreateTexture(nil, "ARTWORK")
		t:SetWidth(panel.pipW)
		t:SetHeight(4)
		t:SetTexture(0.2, 0.2, 0.2, 0.8)
		t:Hide()
		panel.combo[i] = t
	end
	panel.castIcon = panel:CreateTexture(nil, "ARTWORK")
	panel.castIcon:SetWidth(CAST_ICON)
	panel.castIcon:SetHeight(CAST_ICON)
	panel.cast = Bar(panel, W - 2 * PAD - CAST_ICON - 3 - 18, 14) -- clear of the close button

	local function L(text)
		local l = panel:CreateFontString(nil, "OVERLAY", "GameFontNormalSmall")
		l:SetText(text)
		l:SetWidth(LABEL_W - 2)
		l:SetJustifyH("LEFT")
		return l
	end
	panel.castLabel, panel.casts = L("Cast"), IconRow(panel, MAX_CASTS, SIZES.casts, "AgentCamObsCast")
	panel.cdLabel, panel.cds = L("CD"), IconRow(panel, MAX_CDS, SIZES.cds, "AgentCamObsCD")
	panel.buffLabel, panel.buffs = L("Buff"), IconRow(panel, MAX_AURAS, SIZES.buffs, "AgentCamObsBuff")
	panel.debuffLabel, panel.debuffs = L("Debuff"), IconRow(panel, MAX_AURAS, SIZES.debuffs, "AgentCamObsDebuff")
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

-- The agent's class: the server's record, else what AgentCam learned (for the combo pips).
local function ClassOf(o)
	if o.class then return o.class end
	local _, class = nil, nil
	if AgentCam_GetStats then _, class = AgentCam_GetStats(o.name) end
	return class
end

local function FillAuras(row, list, isDebuff)
	local n = 0
	for i, b in ipairs(row) do
		local a = list[i]
		if a then
			n = i
			b.icon:SetTexture(a.icon or QUESTION)
			local left = a.ends and a.ends - GetTime()
			b.text:SetText((a.stacks and a.stacks > 1) and a.stacks or "")
			b.time:SetText((left and left > 0) and Short(left) or "")
			b.tip = { a.spell or ("Spell " .. (a.id or "?")) }
			if left and left > 0 then table.insert(b.tip, "Time left: " .. Short(left)) end
			if isDebuff then
				-- no dispel type reaches us, so the stock "none" colour (BuffFrame.lua's DebuffTypeColor)
				local c = DebuffTypeColor and DebuffTypeColor["none"] or { r = 0.8, g = 0, b = 0 }
				b.border:SetVertexColor(c.r, c.g, c.b)
				b.border:Show()
			else
				b.border:Hide()
			end
		end
	end
	return n
end

local function Refresh()
	if not panel or not panel:IsShown() then return end
	local o = Subject()
	local y = -PAD
	if not o then
		panel.castIcon:Hide()
		panel.cast:ClearAllPoints()
		panel.cast:SetPoint("TOPLEFT", panel, "TOPLEFT", PAD, y)
		SetBar(panel.cast, 0, 1, 1, 0.7, 0, "|cff808080no agent: tick one in /ac config|r")
		panel:SetHeight(PAD * 2 + 14)
		return
	end
	local class = ClassOf(o)

	-- combo points for a rogue or a druid (cat form), once the server reports them
	local cp = o.combo
	local pips = cp and (class == "ROGUE" or class == "DRUID" or cp > 0)
	for i, t in ipairs(panel.combo) do
		if pips then
			t:ClearAllPoints()
			t:SetPoint("TOPLEFT", panel, "TOPLEFT", PAD + (i - 1) * (panel.pipW + 3), y)
			if i <= cp then t:SetTexture(1, 0.85, 0.1, 1) else t:SetTexture(0.2, 0.2, 0.2, 0.8) end
			t:Show()
		else
			t:Hide()
		end
	end
	if pips then y = y - 7 end

	-- the cast bar: icon, spell -> target, seconds left
	local c = o.casting
	if c and c.ends and c.ends < GetTime() - 1 then o.casting, c = nil, nil end
	panel.castIcon:ClearAllPoints()
	panel.castIcon:SetPoint("TOPLEFT", panel, "TOPLEFT", PAD, y)
	panel.cast:ClearAllPoints()
	panel.cast:SetPoint("TOPLEFT", panel, "TOPLEFT", PAD + CAST_ICON + 3, y - (CAST_ICON - 14) / 2)
	if c then
		panel.castIcon:SetTexture(c.icon or QUESTION)
		panel.castIcon:SetAlpha(1)
		local total = c.ends and (c.ends - c.t) or 0
		local done = GetTime() - c.t
		local left = total > 0 and math.max(0, total - done) or nil
		SetBar(panel.cast, total > 0 and math.min(done, total) or 1, total > 0 and total or 1, 1, 0.7, 0,
			c.spell .. (c.target and (" > " .. c.target) or ""), left and string.format("%.1f", left) or nil)
	else
		panel.castIcon:SetTexture("Interface\\Icons\\INV_Misc_PocketWatch_01")
		panel.castIcon:SetAlpha(0.3)
		SetBar(panel.cast, 0, 1, 1, 0.7, 0, "|cff808080not casting|r")
	end
	panel.castIcon:Show()
	y = y - CAST_ICON - 6

	-- recent casts, newest first; the time under each is how long ago
	local nc = 0
	for i, bt in ipairs(panel.casts) do
		local cs = o.casts[i]
		if cs then
			nc = i
			bt.icon:SetTexture(cs.icon or QUESTION)
			bt.icon:SetVertexColor(1, 1, 1)
			if cs.state == "failed" then bt.icon:SetVertexColor(1, 0.3, 0.3) end
			bt.text:SetText("")
			bt.time:SetText(Short(GetTime() - cs.t))
			bt.tip = { cs.spell or ("Spell " .. (cs.id or "?")) }
			if cs.target then table.insert(bt.tip, "on " .. cs.target) end
			table.insert(bt.tip, (cs.state == "casting" and "casting, " or cs.state == "failed" and ("failed" .. (cs.reason and (" (" .. cs.reason .. ")") or "") .. ", ") or "") .. math.floor(GetTime() - cs.t) .. "s ago")
		end
	end
	y = Section(panel.castLabel, panel.casts, SIZES.casts, nc, y)

	-- cooldowns, soonest ready first
	local cds = {}
	for id, cd in pairs(o.cds) do
		if cd.start + cd.dur <= GetTime() then
			o.cds[id] = nil
		else
			table.insert(cds, cd)
		end
	end
	table.sort(cds, function(a, b2) return a.start + a.dur < b2.start + b2.dur end)
	local ncd = math.min(table.getn(cds), MAX_CDS)
	for i, bt in ipairs(panel.cds) do
		local cd = cds[i]
		if cd then
			bt.icon:SetTexture(cd.icon or QUESTION)
			if bt.cdStart ~= cd.start then
				CooldownFrame_SetTimer(bt.cd, cd.start, cd.dur, 1)
				bt.cdStart = cd.start
			end
			local left = cd.start + cd.dur - GetTime()
			bt.text:SetText("")
			bt.time:SetText(Short(left))
			bt.tip = { cd.spell or "?", "Ready in " .. Short(left) }
		else
			bt.cdStart = nil
		end
	end
	y = Section(panel.cdLabel, panel.cds, SIZES.cds, ncd, y)

	-- buffs, debuffs
	y = Section(panel.buffLabel, panel.buffs, SIZES.buffs, FillAuras(panel.buffs, o.buffs, false), y)
	y = Section(panel.debuffLabel, panel.debuffs, SIZES.debuffs, FillAuras(panel.debuffs, o.debuffs, true), y)

	panel:SetHeight(-y + PAD - 2)
end

-- Beside the AgentCam panel: on its left, or its right when there is no room on the left.
local function Dock()
	local f = AgentCamFrame
	if not f or panel:GetParent() ~= f then return end
	if odb.pos then
		-- moved by hand: where it was dropped
		if panel.side ~= "free" then
			panel.side = "free"
			panel:ClearAllPoints()
			panel:SetPoint("TOPLEFT", UIParent, "BOTTOMLEFT", odb.pos[1], odb.pos[2])
		end
		return
	end
	local left = f:GetLeft()
	local side = (left and left < W + 10) and "RIGHT" or "LEFT"
	if panel.side == side then return end
	panel.side = side
	panel:ClearAllPoints()
	if side == "LEFT" then
		panel:SetPoint("TOPRIGHT", f, "TOPLEFT", -2, 0)
	else
		panel:SetPoint("TOPLEFT", f, "TOPRIGHT", 2, 0)
	end
end

-- Wanted (odb.shown) and the AgentCam panel unfolded: a folded panel folds this one away too.
local function Visible()
	return odb and odb.shown and not (AgentCam_Collapsed and AgentCam_Collapsed())
end

-- Background opacity 0..1, for the settings window.
function AgentCamObs_SetBg(a)
	if not panel then return end
	odb.bg = a
	panel.ApplyBg()
end

function AgentCamObs_GetBg()
	return odb and (odb.bg or 0.75) or 0.75
end

function AgentCamObs_IsShown()
	return panel ~= nil and panel:IsVisible() and true or false
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
	local _, _, bg = string.find(low, "^bg%s*(%d+)$")
	if bg then
		odb.bg = math.max(0, math.min(100, tonumber(bg))) / 100
		panel.ApplyBg()
		return
	end
	if low == "dock" then
		odb.pos, panel.side = nil, nil -- back beside the AgentCam panel
		low, arg = "show", "show"
	end
	if low == "auto" then
		pinned = nil
	elseif arg ~= "" and low ~= "show" and low ~= "on" then
		pinned = Lower(arg)
		Get(arg)
	end
	if low == "" and odb.shown then
		panel:Hide()
		odb.shown = nil
		return
	end
	odb.shown = true
	if Visible() then
		Dock()
		panel:Show()
		Refresh()
	end
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
		if odb.shown == nil then odb.shown = true end -- on by default, docked to the AgentCam panel
		if Visible() then Dock() panel:Show() end
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
	if not panel then return end
	if Visible() then
		Dock()
		if not panel:IsShown() then panel:Show() end
	elseif panel:IsShown() then
		panel:Hide()
	end
	Refresh()
end)
