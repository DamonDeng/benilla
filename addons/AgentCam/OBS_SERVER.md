# 服务端需求：观察者实时数据推送（WOWOBS）

给 workshop cmangos 1.12.1 服务端维护者。目标：观察者（SEC_SPECTATOR 账号）在 AgentCam 里实时看到
agent（自己的或其他参与者的）的血/蓝/怒气/能量、经验、连击点、当前读条、最近施法、技能冷却、Buff/Debuff，以及队友状态。

客户端（AgentCam 插件）已经实现并测试了接收与显示；服务端只需按本文**推送数据**。字段级的协议定义见
[OBS_PROTOCOL.md](OBS_PROTOCOL.md)，本文描述功能、时机和验收。

## 1. 背景：为什么需要服务端

1.12 客户端的 Lua API 读不到别人的技能冷却、带剩余时间的光环、不在同队的队伍成员和经验值。没有服务端数据
时，AgentCam 只能靠“选中扫描”读附近 agent 的血蓝、靠战斗日志猜施法和光环，没有冷却、没有经验，离远了就没有。

## 2. 功能需求

### F1 订阅

- 新增 GM 命令（观察者权限可用）：
  - `.obs <名字> [名字…]`：订阅这些角色（建议上限 10 个），重复执行即替换订阅列表。
  - `.obs off`：取消订阅。
  - 无参 `.obs`：回显当前订阅列表（系统消息）。
- 自动取消订阅：
  - **观察者下线**：清除他的全部订阅。
  - **被订阅的角色下线**：服务端自动把它从所有观察者的订阅列表里移除，移除前给每个订阅者发最后一条
    带离线标志的 `U`（`flags` 含 `o`），再发一条系统消息 `obs: Xxx went offline, unsubscribed`。
    之后不再推送它的任何记录；它重新上线也不自动恢复，需要观察者重新 `.obs`。
  - 只是队伍成员（未被直接订阅）下线时：发一条带 `o` 的 `U`，并在下一条 `G` 里反映队伍变化即可。
- 被订阅的角色**及其队伍成员**的数据推送给该观察者（队友的 `U` 记录用于显示队友血蓝）。
- 权限：观察者可以订阅**任意在线角色**，包括其他参与者的 agent，不限于自己队伍；同一角色可被多个观察者同时订阅。
  角色不在线时回一条系统消息（如 `obs: Xxx not online`），上线后不自动补订。

> 过渡方案（可选）：在 `.obs` 完成前，对执行了 `.watch X` 的观察者推送 X 和 X 队伍的数据。

### F2 传输

- 包：`SMSG_MESSAGECHAT`，`Language = LANG_ADDON (0xFFFFFFFF)`，聊天类型 **`CHAT_MSG_WHISPER`**，
  发送者 guid 填被描述的 agent，直接发到观察者的 `WorldSession`（不经过组队/公会频道）。
- 正文：`WOWOBS<TAB><记录>`，例如 `WOWOBS\tU;Linyeshone;ROGUE;...`。
- **单条 ≤ 240 字节**（`.watchprobe` 实测 250 字节的行只到了 245）。
- 名字、技能名、区域名里不能出现 `;` `,` `:`（出现时替换成空格）。
- 已验证：`.watchprobe` 的 26 行全部到达，20 条连发无丢失；WHISPER 在客户端显示为 `UNKNOWN` 频道，
  AgentCam 只认前缀，不受影响。

### F3 记录与触发时机

| 记录 | 内容 | 触发 | 频率上限 |
|---|---|---|---|
| `U` | 职业、等级、血、能量（值/上限/类型）、区域、标志（死亡/鬼魂/战斗/离线）、目标名、**连击点、经验、本级经验** | 定时；血或能量变化超过 5%、进出战斗、死亡、换目标时立即 | 每角色每秒 2 条 |
| `C` | 开始施法：技能 id、名、图标、目标、读条毫秒（瞬发为 0） | 施法开始（`Spell::SendSpellStart` 附近） | 事件 |
| `S` | 施法成功 | 施法生效（`Spell::SendSpellGo` 附近） | 事件 |
| `F` | 施法失败/打断：技能 id、原因 | 打断、取消、失败（`Spell::cancel` / 失败的 `SendCastResult`） | 事件 |
| `D` | 冷却开始：技能 id、名、图标、剩余毫秒、总时长；提前结束发剩余 0 | 进入冷却（`Player::AddSpellCooldown` 附近）、冷却被重置 | 事件；只发总时长 ≥ 2 秒的，不发公共冷却 |
| `A` | 某一类（Buff 或 Debuff）的**完整列表**：id、图标、剩余毫秒、层数、名（可选） | 光环增加/移除/层数变化/刷新时长；同一 tick 多次变化合并为一次 | 每角色每类每秒 2 次 |
| `G` | 队伍类型、队长、成员名列表 | 进组、退组、换队长、转团队 | 变化时 + 每 10 秒 |

- 订阅开始时立即发一轮全量：每个相关角色的 `U`、`A`（B 和 D）、`G`，以及正在进行的冷却 `D`。
- 字段细节：
  - `U` 的最后三个字段是这次新增的：`;连击点;经验;本级经验`。非盗贼/德鲁伊连击点留空，满级经验留空。
    连击点是该角色对**当前目标**的连击点。
  - 图标填 `SpellIcon.dbc` 的文件名（不带路径）。可以留空，客户端会按技能名查自带的对照表。
  - `A` 列表一条大约放得下 5 个光环，超过就拆成 `1/n … n/n` 多段（客户端收到最后一段才显示）；
    列表为空发 `A;名字;B;`。
  - 时间一律毫秒。

示例：
```
U;Linyeshtwo;ROGUE;23;700;900;60;100;3;Elwynn Forest;c;Kobold Miner;3;12000;17700
G;Linyeshone;P;Linyeshone;1/1;Linyeshone,Linyeshtwo,Linyeshthree
C;Linyeshone;116;Frostbolt;Spell_Frost_FrostBolt02;Kobold Miner;2500
S;Linyeshone;116;Frostbolt;Spell_Frost_FrostBolt02;Kobold Miner
F;Linyeshone;116;Interrupted
D;Linyeshone;122;Frost Nova;Spell_Frost_FrostNova;25000;25000
A;Linyeshone;B;1/2;1459:Spell_Holy_MagicalSentry:1700000:0:Arcane Intellect,168:Spell_Frost_FrostArmor02:0:0:Frost Armor
A;Linyeshone;B;2/2;10157:Spell_Holy_ArcaneIntellect:1700000:0:Arcane Intellect
A;Linyeshone;D;
```

### F4 性能与安全

- 只推给订阅者，不全服广播；没有订阅者的角色不产生任何开销。
- 估算：5 个 agent，平时约每秒 5–10 条，战斗峰值每秒 30–50 条，单条 ≤ 240 字节，约 10 KB/s 以内。
- 推送内容只包含被订阅角色及其队伍成员的战斗状态（第 F3 节所列），不含背包、金币、聊天等；不提供任何写操作。

## 3. 验收

在观察者客户端（装好 AgentCam）里：

1. `.obs Linyeshone Linyeshtwo` 后，右侧面板的 Cooldowns 不再显示 “needs server data”。
2. 左侧列表：每行血条、能量条、经验条实时变化；agent 离观察者很远时依然更新（客户端已不再用选中扫描读血蓝，
   这些条**只**来自 `U` 记录）。
3. 盗贼在右侧面板读条上方显示 5 格连击点。
4. agent 读条时出现读条进度；施法后进入“Recent casts”；被打断时图标变红。
5. 放了有冷却的技能后，“Cooldowns”出现图标和转圈，倒计时与实际一致（误差 < 1 秒）。
6. Buff/Debuff 与 agent 身上一致，含剩余时间和层数；超过 5 个光环时也完整显示。
7. `.obs off` 后不再收到数据：左侧条变暗（10 秒无更新），冷却栏 5 秒后回到 “needs server data”。
8. 被订阅的 agent 下线：观察者收到 `obs: Xxx went offline, unsubscribed`，之后不再收到它的记录；
   它重新上线后需要重新 `.obs` 才有数据。

服务端开发时可以不开客户端界面，先用 `/run AgentCamObs_Record("U;...")` 手动灌一条记录确认显示；
`WowObsProbe` 插件的 `/wop` 可复查通道。
