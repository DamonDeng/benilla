# WOWOBS: server → AgentCam observe data

AgentCam's observe panel (`/ac obs`) shows what an agent is doing. The 1.12 client cannot read
another player's cooldowns, auras with time left, or a group it is not in, so the server sends
them to the spectator as addon messages. Without them the panel falls back to what the combat log
shows nearby (casts and auras, no cooldowns or time left).

## Transport

- An addon message, `LANG_ADDON`, text `WOWOBS<TAB><record>`. The client fires `CHAT_MSG_ADDON`
  with `arg1 = "WOWOBS"`, `arg2 = <record>`. Any chat type the client delivers works (whisper to
  the spectator is the natural one); `/wop` from the WowObsProbe addon reports which arrive.
- Fallback: a system line `[WOWOBS] <record>` (`CHAT_MSG_SYSTEM`) is read the same way.
- One record per message, at most 240 bytes (`/wop` delivered 245 of a 250-byte line). Fields are separated by `;`, lists by `,`, list
  item parts by `:`. Names, spell names and zone names must not contain these.
- Who gets them: the GM(s) watching (`.watch`) or spectating; sending them for every agent in
  the workshop is fine too, the client keeps a record per name.

## Records

All times are milliseconds. `name` is the agent the record is about.

| Record | Fields | When |
|---|---|---|
| `U` unit | `U;name;CLASS;level;hp;hpMax;power;powerMax;powerType;zone;flags;target;combo;xp;xpMax` | every 1 s, and on change |
| `G` group | `G;name;kind;leader;part/total;member,member,...` | on change, and every 10 s |
| `C` cast start | `C;name;spellId;spell;icon;target;castMs` | a cast starts (`castMs` 0: instant) |
| `S` cast done | `S;name;spellId;spell;icon;target` | a spell goes off |
| `F` cast failed | `F;name;spellId;reason` | interrupted / failed |
| `D` cooldown | `D;name;spellId;spell;icon;remainMs;durationMs` | a cooldown starts; `remainMs` 0 when it ends early |
| `A` auras | `A;name;kind;[part/total;]spellId:icon:remainMs:stacks[:name],...` | on change (the whole list of that kind) |

- `CLASS`: the English token, `MAGE`, `WARRIOR`, ...
- `powerType`: 0 mana, 1 rage, 2 focus, 3 energy.
- `flags`: any of `d` dead, `g` ghost, `c` in combat, `o` offline; empty for none.
- `target`: the unit's current target name, or empty.
- `combo`: combo points on its target, 0-5 (optional; send it for rogues and druids, omit or leave
  empty for the rest). The panel draws five pips under the power bar when it is present.
- `xp`, `xpMax`: experience into the level and the level's total (optional; omit or leave empty at
  the level cap). The panel draws an experience bar under the power bar when `xpMax` is above 0.
- `kind` of `G`: `P` party, `R` raid, `S` solo (the server sends `G;name;S;name;1/1;name`); `-`
  is read as not grouped too. A long raid roster is split
  into parts `1/3`, `2/3`, `3/3`; the client shows it when the last part arrives. Send a `U` for
  each member too, so the group shows their health.
- `kind` of `A`: `B` buffs, `D` debuffs. `remainMs` 0 means no end time. Up to 16 shown per kind.
  One message holds about five auras, so split a longer list into parts `1/n` .. `n/n` (the
  client shows it when the last part arrives); without the part field (or with `1/1`) the
  record is the whole list, and `A;name;B;1/1;` (or `A;name;B;`) clears it. The optional `name` part is the aura's name, for the tooltip.
- `icon`: the icon's file name under `Interface\Icons` (`Spell_Frost_FrostBolt02`), from
  `SpellIcon.dbc` via the spell's `SpellIconID`. No 1.12 client API maps a spell id to its icon.
  Empty is allowed: the client reuses an icon it learned for the same spell name, else shows a
  question mark.

## Examples

```
U;Linyeshone;MAGE;23;800;1000;500;1200;0;Elwynn Forest;c;Kobold Miner
U;Linyeshtwo;ROGUE;23;700;900;60;100;3;Elwynn Forest;c;Kobold Miner;3;12000;17700
G;Linyeshone;P;Linyeshone;1/1;Linyeshone,Linyeshtwo,Linyeshthree
C;Linyeshone;116;Frostbolt;Spell_Frost_FrostBolt02;Kobold Miner;2500
S;Linyeshone;116;Frostbolt;Spell_Frost_FrostBolt02;Kobold Miner
D;Linyeshone;122;Frost Nova;Spell_Frost_FrostNova;25000;25000
A;Linyeshone;B;1459:Spell_Holy_MagicalSentry:1700000:0,168:Spell_Frost_FrostArmor02:0:0
```

Test without a server: `/run AgentCamObs_Record("U;Linyeshone;MAGE;23;800;1000;500;1200;0;Elwynn Forest;c;")`.
