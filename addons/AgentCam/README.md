# AgentCam

A 1.12 addon for the workshop's spectator (GM) account: switch the camera between agent
characters, follow one with a free camera, rotate through them, and jump to any player. It uses
only the spectator commands the server grants (`.gm on/off`, `.gm visible/setview/resetview`, `.watch`,
`.unwatch`, `.appear`, `.dismount`) and the stock 1.12 Lua API, so it runs on benilla as any
addon does. It is not part of benilla itself: 1.12.1 has no such addon.

## Install

benilla loads addons from `AddOns/` in its state folder (`BENILLA_HOME`, default
`benilla-config/` at the repo root). Link this folder there:

```bash
mkdir -p "${BENILLA_HOME:-benilla-config}/AddOns"
ln -sfn "$PWD/addons/AgentCam" "${BENILLA_HOME:-benilla-config}/AddOns/AgentCam"
```

Windows (PowerShell, from the repo root): `New-Item -ItemType Junction -Path
"$env:BENILLA_HOME\AddOns\AgentCam" -Target "$PWD\addons\AgentCam"`.

Restart the client after adding the folder or an icon; `/reload` picks up edits to existing files.

## Use

- The panel (right edge, draggable) lists the ticked agents, one row each: the name, then four
  icons aimed at that agent alone: View, Follow, Watch, Teleport. Clicking the name does the
  default mode from the settings (also what Alt-1..5 and the auto director use). The lit icon
  shows what is engaged on which agent. Under the list: back to me, auto director, far camera,
  GM mode + invisibility (one switch: `.gm on` + `.gm visible off`, or both off), screenshot,
  dismount; the GM icon follows the server's replies, whoever sent the command. The gear opens the settings.
- Settings: `/ac config`, the gear, or the Options window's AddOns tab. Up to 20 agent slots: a
  slot counts when it is ticked **and** named; Enable all / Disable all tick every slot. Four
  are ticked on a fresh install.
- The actions:
  - **View**: `.appear` beside it if it is not near, then `.gm setview`: its eyes.
  - **Follow**: keep your invisible body within about 10 yards of it by re-`.appear`-ing every
    2.5 s; the camera stays yours. (The client's own `/follow` is refused: GM mode re-templates
    you to faction 35, which cannot cooperate with the agents.)
  - **Watch**: the server's `.watch`.
- Tools: auto director (cycles the ticked agents, holds while one fights), far camera (50 yd),
  GM mode + invisibility, screenshot with a chat caption, dismount, an info overlay with the viewed
  agent's health and target (hidden while the observe panel is open).
- Keys: `/ac keys` binds Alt-1..5 (panel agents 1-5), Alt-0 (back to me), Alt-=/Alt--
  (next/previous), Alt-M (mode), Alt-A (auto), Alt-G (teleport to target), Alt-C (camera),
  Alt-H or Alt-V (GM mode + invisibility), Alt-S (screenshot). They are also under AgentCam in Keybindings.
- Observe panel: what the viewed agent is doing, one compact line per kind: combo points (rogues,
  druids), the cast bar (icon, target, seconds left), the last casts (a queue, newest first: a repeated spell moves to
  the front with its count, each leaves 30 s after its last cast; hover: spell, target, how
  long ago), cooldowns with a sweep, buffs and debuffs, each with its time under the icon as the
  stock buff bar shows it. A kind with nothing to show takes no room. Its
  name, health, power and experience are its row in the AgentCam panel. It starts docked beside
  the AgentCam panel (left, or right near the screen's left edge); drag it to move it on its own,
  `/ac obs dock` docks it again; it folds and hides with the AgentCam panel. the settings' "Observe panel
  background" slider or `/ac obs bg <0-100>` set its background opacity (0: no background or
  border). Clicking an agent's
  name opens it on that agent; the bars icon at the panel's top left and `/ac obs` toggle it
  (`/ac obs <Name>` pins one agent, `/ac obs auto` follows the viewed agent again).
- Data: the server's `WOWOBS` addon messages ([OBS_PROTOCOL.md](OBS_PROTOCOL.md),
  [OBS_SERVER.md](OBS_SERVER.md)) are the only source of the row bars and the cooldowns. Until
  the server sends them, casts and auras come from the combat log nearby and the bars stay empty.
- `/ac help` lists the commands; `/ac disable` / `/ac enable` turn the whole addon off and on.

## Icons

`AgentCamSpellIcons.lua` maps spell names to icons and aura durations for the combat-log
fallback (the log names spells, never their icon or how long an aura lasts, and 1.12 has no API
for either). A logged name that is not a spell there is ignored. Regenerate it from the client's data with
`WOW_DATA=<Data> cargo run -p benilla-formats --example spell_icon_names > addons/AgentCam/AgentCamSpellIcons.lua`.


`icons/*.tga` (64x64, 32-bit with alpha) are drawn by `tools/make_icons.py`, pure Python with no
dependencies. Each glyph is one `g_<name>()` function and its tile colours are in `ICONS`:

```bash
python3 -I addons/AgentCam/tools/make_icons.py --preview /tmp/agentcam_icons.png
```

## Testing without a server

```bash
cargo build --profile play -p benilla-app --example addon_harness
WOW_DATA=<your 1.12.1 Data folder> target/play/examples/addon_harness addons --probe AgentCam \
  --eval 'AgentCam_Switch(1); return "ok"' --tick 5
```
