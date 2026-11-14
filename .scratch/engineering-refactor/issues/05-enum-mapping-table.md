# 05: 枚举化前置——三方对照表（S3 前置设计）

**What to build:** 产出枚举化的唯一设计输入：从迁移脚本真相源提取全部 63 个 ENUM 列，逐列建立「数据库取值 ↔ 现有 Rust 字符串形态（常量串容器/字符串新类型/裸字面量）↔ 目标域归属」三方对照表，落为设计文档。关键裁决必须显式化：支付单状态（6 值）与订单支付轴（4 值）是两个词表、现有跨表混装的支付状态类型必须拆型；支付意图状态与支付单状态分家后各有唯一属主；在线卡状态归卡域不归设备域；事件类别为无数据库约束的进程内词表。对照表同时登记每列的未知值策略（数据库方向严格失败 / 设备方向透传保留 / 入参方向参数错误）。此表是票 06–13 的直接输入。

**Blocked by:** None (can start immediately)

Status: ready-for-agent

- [x] 对照表覆盖全部 63 列，逐列登记三方信息与未知值策略，落 docs/design/
- [x] 跨表混装类型的拆型方案显式化（支付单/订单支付轴/支付意图三者的目标枚举名与归属）
- [x] 卡状态、钱包冻结状态等归属争议逐条裁决并注理由
- [x] 无数据库独立列的 JSON 持久化词表（停止原因类）单独列节：serde 透传表示约束
- [x] 对照表经与方案基准文档交叉核对无遗漏（63 列计数一致）

## Comments

- **2026-11-14 完成**：交付 [docs/design/enum-mapping.md](../../docs/design/enum-mapping.md)，63 列经程序化 diff 与迁移脚本一一对应（提取脚本逐表跟踪 CREATE TABLE 上下文）。
- **重要更正（对票 06 与方案 4.1 的输入修正）**：逐常量核实发现「跨表混装」是方案初稿示例错误而非代码现状——现有 `PaymentStatus`（6 常量）恰为 `payment_order.status` 六值、`PaymentIntentStatus`（5 常量，注释明确分立）恰为 `charge_payment_intent.status` 五值，**无需拆分存量类型**；真正缺的是 `charge_order.payment_status`（4 值）的 `ChargePaymentStatus`（新建，已并入票 06 打样批）。票 06 开工时按对照表执行。
- 关键裁决 8 条：BizType 一型三用；钱包四词表归 wallet 子树（防 money 反向依赖）；卡投递状态归 card；充电命令状态归 charging；导出词表迁 export 子树；TriggerType 更名 ActivityTriggerType；`migration_baseline` 为合法取值严格解码；生成列不建型。
- 计数口径：63 列 = 62 建型 + 1 生成列；62 列去 BizType 二列重复 = **60 个目标枚举**，其中转正/更名 20、全新 40、保留 1（EventSource）；另登记非 DB 词表（ChargeMode 等）为第二批不阻塞主线。
