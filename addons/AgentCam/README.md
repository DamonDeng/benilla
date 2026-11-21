# AgentCam

A 1.12 addon for the workshop's spectator (GM) account: switch the camera between agent
characters, follow one with a free camera, rotate through them, and jump to any player. It uses
only the spectator commands the server grants (`.gm visible/setview/resetview`, `.watch`,
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

- The panel (right edge, draggable) lists the ticked agents. Left-click one to switch to it,
  right-click to teleport beside it only. The gear opens the settings.
- Settings: `/ac config`, the gear, or the Options window's AddOns tab. Up to 20 agent slots: a
  slot counts when it is ticked **and** named; Enable all / Disable all tick every slot. Four
  are ticked on a fresh install.
- Modes (what clicking an agent does):
  - **View**: `.appear` beside it if it is not near, then `.gm setview`: its eyes.
  - **Follow**: keep your invisible body within about 10 yards of it by re-`.appear`-ing every
    2.5 s; the camera stays yours. (The client's own `/follow` is refused: GM mode re-templates
    you to faction 35, which cannot cooperate with the agents.)
  - **Watch**: the server's `.watch`.
- Tools: auto director (cycles the ticked agents, holds while one fights), far camera (50 yd),
  GM invisibility, screenshot with a chat caption, dismount, an info overlay with the viewed
  agent's health and target.
- Keys: `/ac keys` binds Alt-1..5 (panel agents 1-5), Alt-0 (back to me), Alt-=/Alt--
  (next/previous), Alt-M (mode), Alt-A (auto), Alt-G (teleport to target), Alt-C (camera),
  Alt-V (invisibility), Alt-S (screenshot). They are also under AgentCam in Keybindings.
- `/ac help` lists the commands; `/ac disable` / `/ac enable` turn the whole addon off and on.

## Icons

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
