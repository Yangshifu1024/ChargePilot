# 技术债收口：魔法数与魔法字符串

- 登记日期：2026-11-16
- 状态：待开工（未拆票；开工时按 `.scratch/` 议题约定拆票，建议 slug：`magic-value-cleanup`）
- 来源：全仓扫描——后端 313 个 `.rs` 的生产代码（排除 `tests/` 与 `#[cfg(test)]`）、前端 admin-web 与小程序源码（含 `.controller.js`）、`docker/`、`scripts/`、`tasks.mjs`。关键行号已抽查核实。
- 扫描基线：commit `2eac326`（main）。

## 背景与目标

项目已有两处「单一真相源」：`domain/src/wordlist.rs`（`wordlist_enum!` 词表宏）与 `domain/src/limits.rs`（业务界限值）。本次扫描盘点了**绕过这两处**散落在各处的内联字面量。目标不是消灭一切字面量，而是：

1. 修掉清单中暴露的疑似 bug 与口径漂移；
2. 让状态词表、权限码、界限值回归单一真相源；
3. 把协议码值收回 `protocol-dc589`，兑现「业务码值零协议字面量」的 crate 自述红线；
4. 合并同值多点、多份复制的常量与换算。

**本文档是排查依据与任务总账**；每批任务完成后在对应小节打勾并注记提交号。

## 任务批次

### 批次 0：修 bug（先行，独立可交付）

| 项 | 位置 | 问题 |
|---|---|---|
| [ ] | `http-api/src/views/ops/diagnostics.rs:58` | SQL 内联 `status = 'processing'`，但 `ExportStatus` 词表只有 `running`（`domain/export/mod.rs:9`）——导出进行中计数恒为 0 |
| [ ] | `payment/src/repo/card_funding.rs:139` | SQL 内联 `IN ('paid','starting','charging','stopping')`，`starting`/`stopping` 不在 `ChargeOrderStatus` 八值词表（`domain/order/mod.rs:12-21`）；同逻辑 `repo/checkout.rs:74-81` 绑定枚举只查 `paid`/`charging`，两处口径漂移 |
| [ ] | `platform/src/metrics.rs:19,21` | `telemetry_buffer_dropped_total` 同名注册 gauge + counter 双类型；`telemetry_buffer_received_total`（`gateway-runtime/src/telemetry.rs:274` 实际写入）从未 describe |

### 批次 1：词表收口（状态字面量 → wordlist_enum）

- [ ] 套餐模式 `"amount"/"duration"/"energy"`：跨 `domain/charging/scheme.rs`（20+ 处）、`settle.rs`、`payment/mod.rs`、`card/mod.rs` 内联比较，无词表无具名常量——**最典型的词表绕过**，建议新建 `wordlist_enum!`。
- [ ] SQL 内联状态字面量改绑定枚举，热点：`'active'` 15 处（`ops/repo/mod.rs`、`card/repo/admin.rs`、`marketing/repo/*` 等）、`'charge'` 6 处（biz_type）、`'in'/'out'`（`wallet/repo/query.rs`、`credit.rs`）、`'unused'/'used'/'expired'`（marketing 5 处）、`'wallet_recharge'` 3 处。
- [ ] 服务层状态白名单绕过词表：`card/service/admin.rs:118,127`（卡状态）、`pricing/service/templates.rs:188`（模板）、`settlement/service/user_invoice.rs:40`（开票资格）、`device/service/fault.rs:96`（工单）、`management.rs:53,153,172`（台账）、`station.rs:55,176`（站点）、`import.rs:129`/`vendor.rs:67,144`（厂商）。
- [ ] `EventSource` 字面量映射双份手工维护（`events/src/enqueue.rs:17-21` 与 `repo.rs:44-48`）收敛到 `as_str()/parse()`。
- [ ] 告警严重度 `"fatal"/"critical"/"warning"` 双份并存（`protocol-dc589/fault.rs:22-24` 与 `domain/device/mod.rs:51-55`），合并一份。
- [ ] 订单事件类型/事件源裸字符串（`"charge_started"`/`"charge_ended"`/`"payment_confirmed"` + `"gateway"`/`"payment_callback"`）与 `chargepilot_events::EventKind` 枚举并存两套命名，统一到枚举。
- [ ] views 层重抄词表的 6 组数组（`http-api/src/views/order/admin.rs:126-150` 的 `ORDER_STATUSES`/`BUSINESS_STATUSES`/`PAYMENT_STATUSES`/`START_SOURCES`/`PAYMENT_BIZ_TYPES`/`PAY_METHODS`、`charging/user.rs:166-175` 的 8 值白名单）改为引用 domain 词表派生。

### 批次 2：协议码值归位

- [ ] `domain/payment/mod.rs:63-76` 的 `charge_mode 0/1/4` 裸写，改引用 `protocol-dc589/commands.rs:29-36` 具名 `ChargeMode`。
- [ ] `gateway-runtime/src/device_events.rs:293-294`（consumer_type `0/1/4`，`consumer.rs` 具名常量未用）、`:319`（端口 `0xFF` 哨兵）、`snapshot.rs:117`（端口态 `1`）——与 `gateway-runtime/src/lib.rs:5`「业务码值零协议字面量」声明冲突。
- [ ] `"dc589"` 协议名 7 处内联（`gateway-runtime/sink.rs:48,320`（后者写死在 SQL 里）、`device` 模块 6 处）统一改 adapter `name()`。
- [ ] `commands.rs:180,184-188` 消费类型 `2`/`3` 裸写（`consumer.rs` 已有 `SCAN`/`ONLINE_CARD` 未引用）。

### 批次 3：权限码与角色名收口（改用 `ops/src/perms.rs` 常量）

- [ ] `"user.read"`（card/admin.rs:44、identity/admin_query.rs:128）、`"online_card.manage"`、`"feedback.reply"`、`"order.refund.create"`（views/order/admin.rs:382）、`"audit.read"`（diagnostics.rs:78）。
- [ ] `"finance.read"/"export.create"/"admin_user.read"/"order.read"/"station.read"/"device.read"`（export/service/tasks.rs 7 处、export/registry.rs:172-194）。
- [ ] `"finance.wallet_risk.review"/"...release"`（wallet/service/risk.rs 5 处）；SQL 内联 `'fault.resolve'`（device/repo/fault.rs:210）。
- [ ] RETIRED 权限码前缀 8 条硬编码（ops/repo/mod.rs:468-477）提为常量。
- [ ] 角色 `'customer_finance'`/`'customer_cs'`（wallet/service/risk.rs 4 处）提为常量；前端权限码（`Devices.tsx:103-133`、`Orders.tsx:49-50` 等 15+ 处）建前端常量模块。

### 批次 4：limits.rs 归口（旁路副本改引具名常量）

- [ ] 分页双轨旁路：`http-api/views/charging/user.rs:155,158`、`identity/admin.rs:123,128`、`device/user.rs:39` 改引 `limits::USER_PAGE_MAX/ADMIN_PAGE_MAX/PAGE_SIZE_MAX`（`params.rs` 已正确引用，为对照样板）。
- [ ] 退款上限 `5_000_000`（wallet/service/refund.rs:103）改引 limits 具名上界。
- [ ] 能力 JSON `"max_minutes": 4320`（device/repo/capabilities.rs:27）改引 `MAX_CHARGE_MINUTES`。
- [ ] 散落各 crate 的具名常量归口 limits.rs：心跳 60/15/3（protocol-dc589/config.rs:10-14）、`MAX_RETRY=10`（events/repo.rs:65）、JWT/会话 TTL（platform/jwt.rs:27-28、identity/session.rs:8、ops/session.rs:7）。

### 批次 5：单号前缀 / 键名构造器收敛

- [ ] 单号前缀族 `P`/`C`/`WR`/`RC`/`CP`/`INV`/`RF`/`FEE-R`/`B`/`BILL`/`BILL-I`/`EXP`/`S`/`T`/`SIM`/`SIMH5`/`SIMTX` + 截断长度 32/16/20 收敛到 `platform::ids` 单号构造器（`WR` 同构逻辑已复制 5 处）。
- [ ] Redis 键前缀（`"admin:login-rate:{ip}"`、`"admin:mfa:{token}"`、`"admin:mfa-settings:*:{id}"` 等，ops/service/session.rs 与 mfa.rs）提为常量。
- [ ] `"device-event-{key}"` 幂等键格式（gateway-runtime/sink.rs:338）。
- [ ] 前端 localStorage 键建常量模块：`'cp_admin'` 读取样板重复 10 处；`'cp_refresh'`（admin）与 `'cp_refresh_token'`（小程序）命名不一致需统一决策。

### 批次 6：重复实现合并

- [ ] SQLSTATE `"23000"` 唯一键冲突判定 6 处内联（card/swipe.rs:566、identity/error.rs:29、order/error.rs:84、export/tasks.rs:315、charging/dispatch.rs:74、payment/checkout.rs:366）。
- [ ] UUID 校验（长度 36、连字符位 `8|13|18|23`）后端 3 处 + 前端 `newRequestId()` 手写 3 处。
- [ ] 手机号 11 位校验后端 2 处（identity/service/phone.rs:41-43、repo/admin_query.rs:111-113）+ 前端 8 处正则重复。
- [ ] 前端 `money()` 分转元 6 份各自实现、`/100` 换算 40+ 处——建统一换算模块。
- [ ] `['completed','refunding','refunded']` 可开票三元组前端 7 处重复。
- [ ] HTTP 状态码内联改 `StatusCode::` 常量（errormap.rs 全文件 + views 下 80+ 处 400）。
- [ ] `*1000`（Wh↔mWh）9 处、`*60`（分↔秒）11 处、`8*3600`（UTC+8）3 份并存——评估收敛为具名换算函数。

### 批次 7：配置与脚本默认值

- [ ] `docker/.env` 弱默认凭据（`smoke-pass-1`、`smoke-jwt-secret-…`、`smoke-admin-1`）与危险默认 `PAYMENT_MODE=simulation`、`LOGIN_MODE=development`（:12-13）——`.env` 不入库、`.env.example` 改占位符。
- [ ] 前端登录模式默认值 3 处重复（miniprogram/config/index.ts:21-22、runtime/session.js:3、runtime/platform.ts:40）。
- [ ] `payment/repo/simulation.rs:43` 对预支付 JSON 做 `"simulation"` 子串匹配判定渠道，改结构化判定。

## 附录：全量魔法值清单

> 路径约定：后端省略 `backend/crates/` 前缀，前端省略 `frontend/` 前缀。同一值多处出现已合并为一行。已排除具名常量定义点、用户文案、路由/表名/列名、平凡数字。

### A. 后端协议常量（protocol-dc589）

| 魔法值 | 位置 | 含义 |
|---|---|---|
| `0xEE` / `8` | protocol-dc589/src/frame.rs:11,13 | 帧起始字节 / 帧头固定长度（已具名，定义点） |
| `255` | frame.rs:77；device.rs:89 | LEN u8 上限 / 心跳载荷容量（内联 2 处） |
| `6`（会话宽） | frame.rs:103-104；adapter.rs:320；commands.rs:111；cards.rs:45,53,71；config.rs:239；device.rs:53,83,126,138,180,196,212 | 六字节帧会话字段宽，13 处内联 |
| `0xA0` / `45` | commands.rs:80；device.rs:45 | 注册命令字 / 45 字节载荷 |
| `0xA1` / `7` | commands.rs:101,245 | 注册回复命令字 / 载荷 |
| `0xA5` + `vec![1]` | commands.rs:106 | 心跳确认 |
| `0xA6` / `0xA7` | commands.rs:135,143；adapter.rs:606,609 | 心跳周期下发 / 心跳设置应答 |
| `0xA9` + 6 字节 BCD | commands.rs:111-113 | 校时应答 |
| `0xA2` + `data[0]=1` | commands.rs:209-210 | 主板复位 |
| `0xB7` / `19` | commands.rs:176,195；device.rs:233 | 启动命令 / 19 字节载荷 |
| `2` / `3`（消费类型） | commands.rs:180,184-188；device.rs:239-240 | 扫码 / 在线卡码值裸写 |
| `0xB9` / `0xB8` / `0xBA` | commands.rs:203,224；device.rs:135；adapter.rs:512,514,523 | 停止命令 / 启动、停止应答（多处内联） |
| `0x10` | adapter.rs:526 | BA 停止成功码 |
| `0xBC` / `0xC1` | commands.rs:235,240 | 结束订单确认 / 故障确认 |
| `0xB5` + `vec![0, port]` | adapter.rs:584 | 本地启动确认（内联构造，未进 commands.rs） |
| `0xA4` / `17` / `22` / `23` | adapter.rs:457；measurements.rs:32,54,62；device.rs:78,89 | 心跳短格式 17 字节与扩展载荷偏移 |
| `1`（端口状态=充电） | adapter.rs:473；snapshot.rs:117 | 端口状态码泄漏进 gateway-runtime |
| `0xB6` / `10`、`0xD0` / `5` | cards.rs:21,37；adapter.rs:483,492 | 刷卡上报 / 卡余额查询命令字与载荷 |
| `0xD1` + 7 字节、`0xBD` + 9 字节 | cards.rs:53-58,71-76 | 余额应答 / 刷卡拒绝 |
| `0/1`（D1/BD flag） | cards.rs:54,72 | 卡有效 / 无效标志码 |
| `65535` / `/10` | cards.rs:86 | 分→0.1 元显示换算与 u16 上限 |
| `0xBB` / `0xAA` / `0xCC` | fault.rs:47,52,57 | 整机故障码：烟雾 / 高温 / 输入电压异常 |
| `0xFF` | fault.rs:44；commands.rs:238；adapter.rs:612-631；config.rs:63,111,225 | 整机端口哨兵 / 解析失败哨兵 / 参数关闭哨兵 |
| `<=2`（恢复阈值） | fault.rs:92 | 端口状态 ≤2 视为告警恢复 |
| `60`/`15`/`3`（心跳） | config.rs:10,12,14 | 空闲 60s / 充电 15s 心跳、3 次缺失判掉线（具名但未归口 limits.rs） |
| ConfigTable 默认组 | config.rs:51-63 | 协议默认参数表 `0/4/240/240/100/[100..500]/…/1800/300/0xFF`（裸数字组） |
| `999`、`2500`、`is_multiple_of(10)` | config.rs:99,102,105 | 投币刷卡上限 / 25 元扣费上限 / 10 分倍数 |
| `50..=100`、`120..=10800`、`10..=500`、`5..=3600` | config.rs:111-120 | 温度保护 / 浮充时间 / 浮充功率 / 拔除检查合法区间 |
| `0..13`（C4 拒绝码） | config.rs:147-163 | 设备参数拒绝码→字段名映射 |
| `4`（音量）、`100`（首档折扣）、`8888u16`（面板密码） | config.rs:174,184,192,225 | 平台固定下发值 |
| `0xC3`/`0xC6`/`0xC4`/`0xC5` | config.rs:200,231,239,244 | 参数表下发 / 上报 / 应答 / 读取命令字 |
| `+50/-50`（温度偏置） | device.rs:92；measurements.rs:60 | 温度编码偏置 |
| `64`（端口上限）、`13`（计量块）、`59`/`%60` | device.rs:85,89,105-119；measurements.rs:62,68-69,80-81 | 单帧端口数 / 单端口计量块 13 字节 / 秒余数校验 |
| `44`（BB 载荷）、`9`（C2 载荷）、`36`（B4 载荷） | device.rs:167,192,204；measurements.rs:159,225；adapter.rs:565 | 结束订单 / 分档上报 / 本地启动载荷长度 |
| B4/B7 字段偏移 | adapter.rs:573-580；commands.rs:180-194；device.rs:236-247 | 订单 BCD、启动时间、卡号、余额等帧内偏移 |
| `8 * 3600`（UTC+8） | protocol-dc589/src/lib.rs:27；domain/charging/spec.rs:12 | 协议民用时区 / 计费时区，两 crate 重复内联 |
| `%100`、BCD 编码、`2000+` | lib.rs:113,121,141 | BCD 两位年 / 编码 / 世纪基准 |
| `0x0f`/`0x40`/`0x3f`/`0x80` | adapter.rs:659-660；platform/src/ids.rs:137-138 | UUID v4 版本位手工构造（两处重复实现） |

### B. 后端魔法数 · 超时 / 间隔

| 魔法值 | 位置 | 含义 |
|---|---|---|
| `Duration::from_secs(10)` | storage/src/pool.rs:16 | 连接池 acquire 超时 |
| `100` / `Duration::from_secs(1)` | events/src/runtime.rs:43,46 | 事件领取批次 / 轮询节拍 |
| `250` / `1i64 << retry.min(8)` / `MAX_RETRY=10` | events/src/repo.rs:83,96,89,65 | last_error 截断 / 指数退避（上限 8 ≈ 256s）/ 重试上限 |
| `15s`×2、`30s`、`5`（分钟）、`1h`、`15s`（停机） | app/src/main.rs:171,188,200,204,209,297 | 计费 / 退款节拍、兜底扫描、启动超时阈值、导出清理、停机宽限 |
| `2/32/16`、`500`、`50` | app/src/main.rs:51,66,72,73,159,183 | 连接池 / Redis 池、ID 号段步长、执行器批大小 |
| `Duration::days(7)` ×3 | domain/charging/actual.rs:106；cost.rs:117；measured.rs:62 | 计量/计费时间跨度 7 天上限 |
| `Duration::minutes(5)` | domain/charging/actual.rs:108 | 上报时长超出墙钟容差 |
| `minutes(2)` / `-minutes(1)` | device/src/service/scan.rs:61 | 端口在线窗口 |
| `INTERVAL 1 HOUR` | device/src/repo/user_station.rs:136 | 心跳 1 小时内视为在线（SQL 内联） |
| `INTERVAL 1 MICROSECOND` | device/src/repo/management.rs:181 | 乐观锁 updated_at 最小步长 |
| `Duration::minutes(5)` | payment/src/service/checkout.rs:287 | 支付意图冻结有效期（与 card_funding.rs:63 SQL `INTERVAL 5 MINUTE` 双份定义） |
| `Duration::minutes(30)` | payment/src/service/recharge_bridge.rs:67 | 充值支付单有效期 |
| `Duration::minutes(5)` + `count >= 3` | wallet/src/service/refund.rs:148,150 | 退款频次风控窗口 |
| `30s` / 未来 `1s` | card/src/service/swipe.rs:123-124 | 刷卡事件时效 |
| `60`（Lua 内）/ `Duration::seconds(30)` ×2 | ops/src/service/mfa.rs:29,140-142；ops/src/repo/mod.rs:155-157 | MFA 频次窗口 / TOTP 步长与 ±1 步窗口（两处独立实现） |
| `INTERVAL 30 MINUTE` | ops/src/repo/mod.rs:102 | 登录连败锁定 |
| `Duration::hours(1)` / `24h` | ops/src/service/announcement.rs:41；export/src/service/tasks.rs:295 | 公告回填窗口 / 导出文件过期 |
| `INTERVAL 2 HOUR` | http-api/src/views/ops/diagnostics.rs:53 | 诊断：充电超 2 小时 |
| `24h`/`3 年`/`3600`/`900`/`30min/2h/24h` | http-api/src/views/charging/user.rs:339-342,404,407；order/src/repo/user_query.rs:617 | 曲线窗口映射、3 年保留期、小时/15 分钟桶宽 |
| `Duration::from_millis(300)` | gateway-runtime/src/listener.rs:89 | 停机在途帧落库宽限 |
| `900i64` / `3600i64` / `chunks_mut(2000)` | gateway-runtime/src/telemetry.rs:192,198,207 | 聚合桶步长（裸写）/ UPSERT 分片 |
| `300s`（`"300"` 字符串默认）、`8080`/`9100` 端口 | platform/src/config.rs:156-159 | paid 态启动超时默认 / 默认监听地址 |

### C. 后端魔法数 · 容量 / 长度 / 数量限制

| 魔法值 | 位置 | 含义 |
|---|---|---|
| `LIMIT 500` / `LIMIT 200` / `LIMIT 100` | card/src/repo/admin.rs:23；device/src/repo/alert.rs:148；device/src/repo/import.rs:253 | 列表固定上限（SQL 内联） |
| `120` / `1_000_000u64` / `accept_rate 200` | storage/src/migrator.rs:65-66；gateway-runtime/src/listener.rs:37,48 | SQL 截断 / 秒→微秒 / accept 限速 |
| `4096`/`500`/`200ms`/`4000`（具名）、`BUFFER_CAPACITY/2` | gateway-runtime/src/telemetry.rs:15-22,286 | 遥测缓冲参数 / 积压过半阈值 |
| `128`（X-Request-ID）、`32`（JWT 密钥）、`22`/`43`（sid/secret） | platform/src/httpx.rs:43；config.rs:220；sessiontoken.rs:20-21,33 | 平台长度契约 |
| `16`/`32` 字节随机、`>32` 保留摘要、`[0u8;20]` 密钥、`div_euclid(30)`、`0..=2` | platform/src/sessiontoken.rs:20-21,81；totp.rs:14,35,36,54 | TOTP/会话参数（RFC 6238） |
| `bcrypt cost 10` | platform/src/password.rs:5 | 哈希成本因子内联 |
| `1440` ×5、`48`、`8`（阶梯档）×2、`9990`、`600` | domain/charging/spec.rs:339-393,336,370；settle.rs:78；scheme.rs:130 | 全天分钟数 / 时段上限 / 档数 / 功率上界 / 默认时长 |
| `99`（套餐数）、`64`/`255`（文本）、`rule.id*100+p.id`、`kwh*1000` | domain/charging/scheme.rs:148,145,211,328,336 | 套餐上限 / ID 组合编码 / kWh 换算 |
| `Decimal::new(1,3)`（0.001 kWh）、`Div 16`/`StringFixed 6` | domain/charging/spec.rs:45,56,67 | 计量下界 / 精度契约 |
| `(8..=32)`、`>64`、`>2000`、`>9`、`>20`、`>32`、`>128`、`2`（默认端口） | device/src/repo/capabilities.rs:13；service/fault.rs:30-42；import.rs:35-75,212；management.rs:32-39；station.rs:46-58；vendor.rs:34-37 | 设备编号 / 描述 / 图片 / 标签 / 站名等字段上限、默认端口数 |
| `-180..=180`、`-90..=90`、`1.0..=50.0`、`6371`、`ROUND(…,3)` | device/src/service/station.rs:49-52；repo/user_station.rs:40-52 | 经纬度域 / 搜索半径 / 地球半径 |
| `>64`/`>2000`/`>5`/`>512` | order/src/service/feedback.rs:129-170；http-api/src/views/charging/user.rs:76,479 | 订单号 / 正文 / 图片 / URL 上限（4 处重复内联） |
| `(1..=5)`、`>500`/`>1000`、`1_000_000`、`>255`、`>128`、`10080`、`64KB`、`(1000..=9999)` | order/src/service/feedback.rs:139；settlement/src/service/meter_review.rs:173,432-436；refund_admin.rs:53；user_invoice.rs:83-85；http-api/src/views/settlement/admin.rs:165-879 | 评分 / 理由 / 金额 / 分段 / 请求体 / 年份等上限 |
| `500`（理由）、`>2048`、`(1..=64)`、`>64` | http-api/src/views/device/user.rs:39,142,154；payment/user.rs:120；order/admin.rs:284-285 | 扫码原文 / 端口编码 / 单号上限 |
| `3..=64`、`>64/128`、`>32`、`>128`、`<=255` | ops/src/service/management.rs:85-97,163-171,441-453 | 账号/角色字段长度（create/update 双轨内联） |
| `password>72`、`code>16`、`code==6`、`enrollment==43`、`random_token(16/24/32)` | http-api/src/views/ops/auth.rs:69,98；ops/src/service/mfa.rs:148-152,187,239；session.rs:132 | 密码 / 验证码 / MFA 参数 |
| `p<100.0`、`round_dp(2)`、`<=1440`、`>87_600`、`valid_text(128)` | marketing/src/service/coupon.rs:108-154,151,226 | 券折扣 / 免时长 / 有效期 / 名称上限 |
| `50_000`、`truncate(255)`、`<=32`、`64KB` | export/src/service/tasks.rs:82,560；artifact.rs:18,270 | 导出行数 / 截断 / 任务号段 / 校验文件上限 |
| `port_id<=64`、`event_id>128`、`digest>=32`、`.take(16)` | card/src/service/swipe.rs:70,83；wallet/src/repo/credit.rs:52,84；wallet/src/service/refund.rs:221 | 刷卡 / 入账 / 退款号参数 |
| `>255`（退款理由）、`openid>64` | wallet/src/service/refund.rs:104；identity/src/repo/user.rs:35 | 文本 / openid 上限 |
| `phone 11` 位、`code==6`、UUID `36` 与 `8\|13\|18\|23`、`api_v3==32`、`nonce==12` | identity/src/service/phone.rs:41-43；identity/src/repo/admin_query.rs:111-113；export/src/service/tasks.rs:102,107；settlement/src/service/user_invoice.rs:47-54；http-api/src/views/settlement/admin.rs:859-866；payment/src/service/wechat.rs:57,146 | 手机号（2 处重复）/ UUID（3 处重复）/ 微信密钥与 nonce |

### D. 后端魔法数 · 单位换算

| 魔法值 | 位置 | 含义 |
|---|---|---|
| `*1000` / `/1000`（Wh↔mWh） | protocol-dc589/src/device.rs:121-122,177,210；measurements.rs:91-92,186；charging/receipts.rs:286-287；settle.rs:269,400；measured.rs:89,93 | 共 9 处内联 |
| `*60` / `/60`（分↔秒） | protocol-dc589 9 处；charging/settle.rs:244；marketing/mod.rs:150 | 共 11 处内联 |
| `10000`（万分比 1 倍）、`Decimal::from(100)` | domain/charging/scheme.rs:136,166；marketing/mod.rs:138 | 渠道系数 / 百分比换算 |
| `65535→/10`（分→0.1 元）、`*10`（W↔0.1W） | protocol-dc589/src/cards.rs:86；config.rs:177-214 | ×6 处 |
| `Duration::hours(8)`（UTC+8） | ops/src/repo/dashboard.rs:33,38 | 北京时间换算硬编码（与 protocol/domain 的 `8*3600` 三份并存） |
| `6371`（地球半径） | device/src/repo/user_station.rs:50 | SQL 内球面距离 |
| `1_000_000u64`（秒→微秒） | gateway-runtime/src/listener.rs:48 | 令牌桶换算 |
| `rule_id*100+package_id`（offer 编码） | card/src/service/swipe.rs:464；payment/src/repo/card_funding.rs:81 | ×100 组合键两处（后者套餐 ID 硬编码 1） |
| `{:020}`（20 位定宽） | settlement/src/service/refund_executor.rs:111,124；billing.rs:200,330；bill.rs:74,132 | 退款 / 账单号编码（6 处） |
| `60_000_000_000i64`、`Decimal::from(60)`/`(1000)` | domain/charging/cost.rs:173,212,239-276 | 纳秒→分钟 / 分钟→小时 / Wh→kWh（7 处） |
| `used/=60`、`amount*used/limit` | domain/charging/settle.rs:244,248 | 秒→分钟折算计费 |

### E. 后端魔法字符串 · 状态字面量（SQL / 服务层绕过词表）

| 魔法值 | 位置 | 说明 |
|---|---|---|
| `'active'`（后台账号） | ops/src/repo/mod.rs:101,120,412,507,539；ops/src/repo/mfa.rs:36 | 6 处内联 |
| `'active'`（用户 / 卡 / 券 / 站点 / 厂商） | card/src/repo/admin.rs:36,49,86；marketing/src/repo/coupon.rs:130,332；query.rs:94,141；activity.rs:80,114；device/src/service/station.rs:55,176；service/import.rs:129；vendor.rs:67,144 | 各域启用态均内联 |
| `'enabled'/'disabled'/'retired'/'fault'` | device/src/service/management.rs:53,153,172 | 台账状态 3 处，绕过 `DeviceMetaStatus` 词表 |
| `'idle'`、`'open'/'dispatched'/'fixed'/'closed'` | device/src/service/scan.rs:71；payment/src/repo/scan.rs:54；device/src/service/fault.rs:96 | 端口 / 工单状态白名单绕过词表 |
| `'unused'/'used'/'expired'`、`'manual'`、`'activity','invite_reward'` | marketing/src/repo/query.rs:95；coupon.rs:172-174,235；activity.rs:461；identity/src/repo/admin_query.rs:397 | 券状态与发放来源 |
| `'unbound'` | card/src/repo/user_card.rs:14,29 | 解绑态 |
| `'pending'`（三张表）、`'pending','running'`、`'completed','failed'` | order/src/repo/feedback.rs:217；export/src/repo/tasks.rs:188,223,243,273；settlement/src/repo/billing.rs:33；marketing/src/repo/activity.rs:197 | 初始态 / 竞争窗口 / 终态判定 |
| `'paid','charging'`、`'charging'`、`'sent'`、`'failed'` | order/src/repo/user_query.rs:124；ops/src/repo/dashboard.rs:43；diagnostics.rs:42,47,31,52 | 进行中 / 在充 / 已下发 / 失败计数 |
| `'charge'`（biz_type） | charging/src/repo/mod.rs:32；wallet/src/repo/recharge_history.rs:57；marketing/src/repo/coupon.rs:383,430；order/src/repo/user_query.rs:413；admin_query.rs:752 | 6 处内联，绕过 `BizType` 词表 |
| `'wallet_recharge'` | wallet/src/repo/recharge_history.rs:71,83；marketing/src/repo/activity.rs:207-208 | 绕过词表 |
| `'in'/'out'`、`'in','recharge'` | wallet/src/repo/query.rs:63-71；credit.rs:92 | 资金方向，绕过 `TxnDirection` 词表 |
| `'charge'/'balance'/'paid'`（INSERT） | payment/src/repo/card_funding.rs:20,62-63 | 刷卡首购 / 意图 INSERT 全内联（含 charge_mode=4、`'0.000'`、5 分钟过期） |
| `'initiated'/'paid'`、`status='active'`、`!= "enabled"` | payment/src/repo/card_funding.rs:139；checkout.rs:93,113,151 | 见批次 0；用户 / 设备状态内联 |
| `'refunded'/'partial_refunded'/'paid'/'pending'`（SQL CASE） | wallet/src/repo/recharge_history.rs:52-55；order/src/repo/admin_query.rs:37-40 | 派生展示状态整套内联 |
| `"pending"/"success"/"failed"/"needs_review"/"processing"/"manual_review"/"rejected"` | wallet/src/repo/query.rs:329,337-352 | 退款进度 7 值裸组合 |
| `'automatic'`、`"manual_review"/"accepted"`、`"reviewed"` | settlement/src/repo/refund_executor.rs:17,31；wallet/src/service/refund.rs:154,166；risk.rs:94 | 执行策略 / 回执状态 |
| `'wallet_refund_frequency'`、`'wallet'` | wallet/src/repo/refund.rs:67；risk.rs:125；service/risk.rs:283 | 风控规则名 3 处内联 |
| `"fatal"/"critical"/"warning"` | protocol-dc589/src/fault.rs:22-24 | 告警严重度，与 `domain/device/mod.rs:51-55` 枚举双份并存 |
| `"user"/"admin"/"gateway"` 事件源映射 ×2 | events/src/enqueue.rs:17-21；repo.rs:44-48 | `EventSource` 枚举映射两份手工维护 |
| `"none"/"energy"/"minute_power"/"minute"/"session"` | domain/charging/spec.rs:171 | ServiceBasis 保留词表内联比较 |
| `"calculated"/"confirming"/"rejected"/"meter_review"`、`"start_failed"` 等 | domain/charging/scheme.rs:401-586 | 预览状态 / 场景名（JSON 契约值） |
| `"server"`（executor）、`"budget_exhausted"` | domain/charging/scheme.rs:581；settle.rs:228,456 | spec.rs:86 有锚点未引用 |
| `"payment"/"balance"/"card"/"wechat"` | http-api/src/views/order/admin.rs:147,150 | 支付方式词表数组重抄 domain |
| `ORDER_STATUSES` 等 6 组数组 | http-api/src/views/order/admin.rs:126-150；charging/user.rs:166-175 | views 层重抄 domain 词表（8+3+4+3+2+2 值） |
| `"active"/"lost"/"disabled"/"unbound"`、`"active"/"disabled"` | card/src/service/admin.rs:118,127；pricing/src/service/templates.rs:188 | 卡 / 模板状态白名单绕过词表 |
| `"completed"/"refunding"/"refunded"` | settlement/src/service/user_invoice.rs:40 | 开票资格判断绕过词表 |
| `"published"`（审计 JSON） | ops/src/service/announcement.rs:94 | repo 用枚举、审计旁路内联 |
| `'customer_admin'` | ops/src/repo/mod.rs:399,506 | 角色码内联 |
| `"user"/"admin"`（JWT kind） | platform/src/jwt.rs:68-70 | 词表锚点（具名，列出备查） |
| `"simulation"/"wechat_direct"/"disabled"/"development"/"wechat"` | platform/src/config.rs:64-66,85-87,166-167 | 配置取值词表（解析锚点，未走 wordlist_enum） |

### F. 后端魔法字符串 · 权限码 / 角色名

| 魔法值 | 位置 |
|---|---|
| `"user.read"` | card/src/service/admin.rs:44；identity/src/repo/admin_query.rs:128 |
| `"online_card.manage"` | card/src/service/admin.rs:36 |
| `"feedback.reply"` | order/src/service/feedback.rs:47 |
| `"finance.read"` / `"export.create"` / `"admin_user.read"` | export/src/service/tasks.rs:174,220,236,285,434,584,592 |
| `"order.read"/"station.read"/"device.read"/"finance.read"` | export/src/registry.rs:172-194 |
| `"order.refund.create"` | http-api/src/views/order/admin.rs:382 |
| `"audit.read"` | http-api/src/views/ops/diagnostics.rs:78 |
| `"finance.wallet_risk.review"/"...release"` | wallet/src/service/risk.rs:89,111,116,154,246 |
| `'fault.resolve'`（SQL 内联） | device/src/repo/fault.rs:210 |
| RETIRED 前缀组 8 条 | ops/src/repo/mod.rs:468-477 |
| `'customer_finance'`/`'customer_cs'` 角色 | wallet/src/service/risk.rs:107,112,150,242 |

### G. 后端魔法字符串 · 单号前缀 / 键名 / 存储键

| 魔法值 | 位置 | 说明 |
|---|---|---|
| `"P{}"`（支付单）、`"C"+时间戳+端口`（充电单） | payment/src/service/apply.rs:266,355；recharge_bridge.rs:66；order/src/repo/lifecycle.rs:148；domain/payment/mod.rs:121-123 | 前缀族 |
| `"WR{}"` + 截断 32 | charging/service/receipts.rs:165；sweeper.rs:51；payment/service/apply.rs:392；wallet/service/refund.rs:230 | 5 处同构复制 |
| `"RC{}"`、`"CP{operation_id}"`、`"INV{}"`、`"RF{id:020}"`、`"FEE-R{id:020}"`、`"B{id:020}"`、`"BILL{}"`/`"BILL-I{}"`、`"PENDING-{}"`、`"EXP{}"`、`"S{}"`/`"T{}"`、`"SIM{}"`/`"SIMH5{}"`/`"SIMTX{}"` | wallet/repo/credit.rs:84；card/service/swipe.rs:313,578；settlement/service/user_invoice.rs:111；refund_executor.rs:111,124；billing.rs:200,330；bill.rs:74,132；marketing/service/coupon.rs:537；export/service/tasks.rs:279；charging/service/receipts.rs:124；sweeper.rs:59；payment/service/channel.rs:84；simulation.rs:27,49 | 单号 / 事件号前缀全量清单 |
| `"ART_"`、`"RT_"`、`"admin:session:"`、`"user:session:"`、`"admin:login-rate:{ip}"`、`"admin:mfa:{token}"`、`"admin:mfa-settings:*:{id}"` | ops/src/service/session.rs:14-15,111,139,152,190；identity/src/service/session.rs:11-12；ops/src/service/mfa.rs:32,36,40 | 刷新令牌前缀与 Redis 键 |
| `"device-event-{key}"`、`"dc589\|{}\|{}\|{}\|{millis}\|{}"` | gateway-runtime/src/sink.rs:338,48 | 事件幂等键格式 |
| `"chargepilot-exports"`、`"{task_no}.scope.json"`、`".partial"`、`"v1"` | export/src/service/tasks.rs:91；artifact.rs:49,141-217 | 导出目录 / 校验文件布局 |
| `"dev:"`、`"development:{account}"`、`"ChargePilot"`、`"inviter"` | identity/src/service/wechat.rs:33,43；ops/src/service/mfa.rs:241,436；marketing/service/activity.rs:122,186,196 | 开发登录命名空间 / otpauth issuer |
| `"x-request-id"`（读+写）、`"cache-control"/"no-store"`、`"Bearer "`、`"attachment; filename=\"{}\""` | platform/src/httpx.rs:40,59,61；http-api/src/errormap.rs:20,42-46；views/export/admin.rs:168-169 | header 名与值 |
| `"index.html"` | http-api/src/lib.rs:129；assets-*/src/lib.rs:30 | SPA 回退文件名 |
| `"\r\n"`、`replace('"',"\"\"")`、`ESCAPE '!'` | export/src/artifact.rs:75,86,90；ops/src/repo/audit_query.rs:28-33,47,87 | CSV / LIKE 转义约定 |
| `"smoke"/"high_temperature"/"device_fault"/"port_fault_{port}"` | protocol-dc589/src/fault.rs:48-65,80,84 | 告警指标名（跨模块聚合键） |

### H. 后端魔法字符串 · 事件名 / 审计 action / 单据类型词

| 魔法值 | 位置 |
|---|---|
| `"charge_started"`/`"charge_ended"` + 源 `"gateway"` | charging/src/service/receipts.rs:147-148,316-317 |
| `"payment_confirmed"` + 源 `"payment_callback"` | payment/src/service/apply.rs:360-361 |
| `"card_start_confirming"`/`"card_extend_confirmed"` | card/src/service/swipe.rs:600,333 |
| `"refund_completed"`/`"billing"`、`"manual_settlement"` | settlement/src/service/refund_executor.rs:129-130；bill.rs:138；meter_review.rs:488 |
| `"login"`/`"login_mfa"`、`"mfa_enable"/"mfa_disable"/"mfa_enrol"`、`"admin_user"`、`'ops','diagnostics','system'` | ops/src/repo/mod.rs:126,164,278,302；ops/src/service/mfa.rs:315-507 |
| `"bind"` + 状态作 action | card/src/service/admin.rs:102,132 |
| `"coupon_activity"`、`"announcement"`、`"pricing_template"`、`"export_task"` 等 create/update | marketing/service/activity.rs:311-382；ops/service/announcement.rs:98-100；pricing/service/templates.rs:77-199；export/service/tasks.rs:347-350 |
| device 域审计 `"ack"/"dispatch"/"import"/"update_status"/…` | device/src/service/alert.rs:120-122；fault.rs:178-256；import.rs:225-227；management.rs:130-184；station.rs:101-152；vendor.rs:99-166 |
| `"orders"/"stations"/"devices"/"bills"` 导出资源名 | export/src/registry.rs:170-196；tasks.rs:258-602 |
| `"last_30min"/"last_2h"/"last_24h"`、`"hourly"`、`"15min"` | http-api/src/views/charging/user.rs:340-342,407 |
| `"度"/"小时"`（费用单位） | domain/charging/cost.rs:180,240,268 |
| `"在线刷卡start"` | card/src/service/swipe.rs:579 |

### I. 后端魔法字符串 · 错误码 / HTTP 状态码 / 协议杂项

| 魔法值 | 位置 | 说明 |
|---|---|---|
| `1001`、`2009`、`1005` 直发 | http-api/src/errormap.rs:98,473,479 | 未走 `Code` 枚举（恰等于 Unauthorized/Conflict/BadRequest 值） |
| `"60"`（Retry-After） | errormap.rs:122,685 | 字符串字面量 2 处重复 |
| `400/401/403/404/409/410/422/423/429/500/502/503` | errormap.rs 全文件（400 约 20 处、404 约 20 处、503 约 40 处）；views 下 80+ 处 400、多处 503；export/admin.rs:173 | HTTP 状态码全项目内联，无 `StatusCode::` 常量引用 |
| `code: 0` + `"ok"` | platform/src/httpx.rs:28-29 | 信封成功契约 |
| `"23000"`（SQLSTATE 唯一键冲突） | card/swipe.rs:566；identity/error.rs:29；order/error.rs:84；export/tasks.rs:315；charging/dispatch.rs:74；payment/checkout.rs:366 | 6 处重复内联 |
| `result_code==0`、`0..=3`、`=0,=1` | charging/src/service/receipts.rs:66-67；repo/receipts.rs:90 | 启动回执结果码（协议值） |
| `"WECHATPAY2-SHA256-RSA2048"`、`"CNY"`、`"/v3/pay/transactions/jsapi"`、`status!=200`、`json!("RSA")` | payment/src/service/wechat.rs:106,236,306,315-349 | 微信渠道常量 |
| `"application/vnd.openxml…sheet"`、`"text/csv; charset=utf-8"`、mime 表 16 项 | export/src/service/tasks.rs:665-667；http-api/src/lib.rs:98-118 | MIME |
| `"SELECT 1"`/`"PING"`、`READ COMMITTED` 前置句 | http-api/src/lib.rs:204-208；card/src/repo/mod.rs:14（各域 repo 重复） | 探针 / 隔离级别 |
| `"+00:00"`、`"utf8mb4"`/`"utf8mb4_unicode_ci"` | storage/src/pool.rs:11-13 | 会话时区 / 字符集 |

### J. 前端 · 换算 / 轮询 / 限制

| 魔法值 | 位置 | 说明 |
|---|---|---|
| `/100`（分→元） | admin-web 21+ 处（Dashboard.tsx:29、Coupons.tsx:95、Refunds.tsx:63、schemes/model.ts:99、pricing/model.ts:121 等）；小程序 16+ 处（utils/order.js:4、bill.js:2、wallet.controller.js:17 等） | `money()` 在 6 个文件各自实现，共 40+ 处 |
| `*100`（元→分） | ManualRefund.tsx:27；MeterReviews.tsx:29；SchemeEditor.tsx:50-194；pricing/model.ts:122,298；recharge.controller.js:40；refund.controller.js:5 | 含 `padEnd(2,'0')` 手工补位 |
| `/1000`（Wh→度）、`*1000`（km→m）、`/60`、`%3600`、`exp*1000`、`86400000`/`*60000`、`1440` | OrderPackageDetails.tsx:35；scheme.js:8；index.controller.js:8；presentation.ts:82-107；session.ts:26；SchemeEditor.tsx:65；schemes/model.ts:22-70 | 各类单位换算散落 |
| `10000`（bp=1.0 倍） | schemes/model.ts:33；pricing/model.ts:214-215,248-249,464；compose-smoke.sh:38 | 5 处重复 |
| `5000` 轮询、`60_000`、`15_000` | Orders.tsx:74；OrderDetailsDrawer.tsx:186；charging.controller.js:37,47；Devices.tsx:92；OrderPowerCurve.tsx:71 | 轮询间隔散落（钳制 `3000..30000` 内联） |
| `15_000` ×2、`30_000`、`250` ×3、`1000` | api/client.ts:17,32；api/session.ts:83；DeviceCreate.tsx:184,197；OrderStationSelect.tsx:18；Exports.tsx:116 | 超时 / 防抖重复定义 |
| `page_size=100` ×4、`50`、`20` ×6、`3`、`100` | Casework.tsx:144,190；AppliedScheme.tsx:28；OrderStationSelect.tsx:28；DeviceCreate.tsx:60,84；小程序各列表 controller；invoice/apply.controller.js:34 | 分页条数散落 |
| 充值 `100..=5_000_000`、退款 `<=100000000` | recharge.controller.js:42；refund.controller.js:5 | 金额限制（后端 limits.rs 的镜像副本） |
| `4320`/`65`/`60`/`600` | schemes/model.ts:33-34,85-90 | 后端方案校验值的前端镜像 |
| `2048`、`255`、`{8,32}`、`128`、user_id 正则 `[1-9]\d{0,19}` ×8 | scan.js:2,13；deviceCsv.ts:32-43；session.js:36；chargeUserID.ts:5 等 | 各类长度 / 格式校验（8 处重复） |
| `MAX_USER_ID='18446744073709551615'` | chargeUserID.ts:1 | uint64 上限字符串 |
| `/^\d{6}$/`、`/^1[3-9]\d{9}$/`、`radii:[1,5,10,20,50]` | Security.tsx:14；phone.controller.js:10；nearby.controller.js:4 | 格式 / 半径选项 |

### K. 前端 · 状态映射表与裸字符串

| 魔法值 | 位置 |
|---|---|
| 订单状态 9 值映射、退款 4 值映射 | miniprogram/src/utils/order.js:2-3 |
| `portStatuses = { 0:空闲, 1:充电, 3:输出故障, 4:粘连 }` | admin-web/src/utils/deviceTelemetry.ts:1 |
| 反馈 / 报修 / 故障类型 / 类别映射 | Casework.tsx:54-58；dev/fault.controller.js:5 |
| 告警级别 / 状态 / 指标映射（9 指标 + `port_fault_` 前缀） | Alerts.tsx:11-20 |
| 导出资源 / 任务状态映射 | Exports.tsx:26-35 |
| 发票 5 值、计量核实 5 值、退款 5 值映射 | Billing.tsx:35；MeterReviews.tsx:13；Refunds.tsx:14 |
| 订单业务 / 支付状态 + 颜色映射 | orders/presentation.ts:1-16；OrderDetailsDrawer.tsx:56-84 |
| 支付单类型 / 方式映射（`wechat`/`balance` 重复 3 处） | PaymentOrders.tsx:29-35；ChargeUserRecharges.tsx:24 |
| 计费算法 5 值、站点状态 3 值（重复 2 处） | OrderPackageDetails.tsx:12-18；StationWorkspace.tsx:14-18；Stations.tsx:89,122 |
| 券类型 / 触发类型映射 | Coupons.tsx:111-165 |
| 充值单 6 值 + 终态数组 ×2、券 3 值、发票 5 值 | recharge.controller.js:3,24,53；coupons/my.controller.js:3-5；invoice/my.controller.js:3 |
| `['completed','refunding','refunded']` 三元组 | charging.tsx:49；detail.tsx:56,76,89；detail.controller.js:48,64；invoice/apply.controller.js:36,56（7 处重复） |
| `statusValues` 筛选枚举（订单 9 值、流水方向 3 值） | history.controller.js:6；txns.controller.js:6 |
| 端口 / 设备 / 套餐模式映射 | scan.js:5-6；scheme.js:3,11-15 |
| 微信 errMsg 约定（`requestPayment:fail cancel` 等） | scan.controller.js:29；runtime/platform.ts:27-43；utils/payment.js:4 |
| `[400,409,422]`、`[401,403]`、`code!==0` | ManualRefund.tsx:41；refund.controller.js:52；client.ts:33,64；session.js:84 |
| `'customer_finance'` 角色、权限码 15+ 处（`station.read` ×5 等） | Billing.tsx:121；Devices.tsx:103-133；Orders.tsx:49-50；ChargingSchemes.tsx:29-40 等 |

### L. 前端 · 存储键 / 锁名

| 魔法值 | 位置 |
|---|---|
| `'cp_token'` ×8、`'cp_refresh'`、`'cp_admin'` ×10 | admin-web：App.tsx:24；api/client.ts:53；api/session.ts:38-89；Exports.tsx:52,104；MainLayout.tsx:66 等（`cp_admin` 读取样板重复 10 处） |
| `'cp_session_epoch'`、`'cp-admin-session'`、`'cp-session'`、`` `cp_manual_refund_${…}` ``、`` `cp_coupon_grant:${…}` `` | api/session.ts:32,71,74；client.ts:22,27；ManualRefund.tsx:14；Coupons.tsx:54 |
| `'cp_token'`、`'cp_refresh_token'`、`'cp_user'`、`'cp_development_account'`、`'tester-1'` | miniprogram：runtime/session.js:8-25；development.tsx:8；platform.ts:26,41 |
| `` 'cp_wallet_recharge_'+user_id ``、`` 'cp_wallet_refund_'+user_id `` | recharge.controller.js:21；refund.controller.js:27 |
| `newRequestId()` UUID v4 手写 ×3 | utils/payment.js:7-9；recharge.controller.js:5；refund.controller.js:6 |

### M. docker / 脚本 / 构建

| 魔法值 | 位置 | 说明 |
|---|---|---|
| `MYSQL_PASSWORD=smoke-pass-1`、`JWT_SECRET=smoke-jwt-secret-…`、`ADMIN_BOOTSTRAP_PASSWORD=smoke-admin-1` | docker/.env:3-10 | 生产 compose 弱默认凭据 |
| `PAYMENT_MODE=simulation`、`LOGIN_MODE=development` | docker/.env:12-13（.env.example 同） | 危险默认 |
| 端口 `3306/6379/8080/9100/80/443`、镜像 tag `mysql:8.4`/`redis:8-alpine`/`caddy:2` | docker/compose.prod.yaml:16-69；tasks.mjs:148,155 | 端口与版本内联（tasks.mjs 重复） |
| `TEST_MYSQL_URL`（3307 + 明文密码）、`TEST_REDIS_URL`（6380）、`-proot`、`-pchargepilot` | tasks.mjs:111-129 | 测试连接串与密码内联 |
| 清场 SQL 状态字面量 `('pending','processing')` 等 | tasks.mjs:131-133 | |
| `mysql:3307`/`redis:6380`、`root` 密码 | tasks.mjs:146-155 | dev 容器默认 |
| `http://127.0.0.1:8080` 默认、站点坐标、方案/设备内联数值、`sent\|pending\|…` 分支 | scripts/ops/compose-smoke.sh:6,29,38,49,83 | 冒烟脚本 |
| `port 5173/5174`、`'/admin/'`/`'/h5/'`、`'http://localhost:8081'`、`designWidth 750` | admin-web/vite.config.ts:7,27,35；miniprogram/config/index.ts:6-7,28,33 | dev 配置 |
| `'/api/v1'`、`'wechat'` 登录模式默认（3 处重复） | miniprogram/config/index.ts:21-22；runtime/session.js:3；runtime/platform.ts:40 | |

## 验收口径

- 每批改动后 `cargo build && cargo clippy` 通过、相关集成测试通过（后端测试基线见 `docs/map/08-testing-gates.md`）。
- 词表改动须核对 `migrations/0001_init.sql` 列声明逐字节一致（`CODING_STANDARDS.md` 硬规则）。
- 批次 0 的三处修复各配一条能失败的回归测试再修。
