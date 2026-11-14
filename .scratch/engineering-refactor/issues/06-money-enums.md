# 06: money 域枚举化打样（S3a）

**What to build:** 建立全仓枚举化范式的打样板式，并把最底层的 money 词表全部转正。按票 05 对照表定义支付单状态（6 值）、订单支付轴（4 值）、退款状态、执行策略、账单状态、支付方式、业务类型、流水方向、流水业务类型、性别、钱包冻结状态等枚举：统一采用 sqlx 类型派生（MySQL 字符串枚举原生支持，无需手写编解码宏），sqlx 与 serde 各自的重命名配置成对书写并由字面量对照测试锁定；数据库方向未知值严格失败（打样必须先在真库验证 NULL 列与 DEFAULT 行为）。本域及全部消费 crate（order/settlement/payment/wallet/charging 共 16 处非测试引用）一次性迁移，旧字符串新类型删除，批内编译全绿原子落地。本票验证通过后 07–13 才铺开。

**Blocked by:** 03（lint 门禁先行，含通配枚举分支失败级）、05（对照表）

Status: done

- [x] 真库打样：NULL 列（Option 包裹）与 DEFAULT 行为验证通过并记录
- [x] money 域全部词表按对照表枚举化，每枚举配字面量对照测试（serde 与 sqlx 两套重命名一致性锁定）
- [x] 16 处跨 crate 消费点全部迁移，旧字符串新类型与关联常量删除
- [x] 枚举切片进 SQL 一律绑定参数（禁拼接展开）
- [x] 日常 check 三件套 + 全量集成轮跑绿；字面量比较点清单快照对比只减不增

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **177 通过 / 0 失败**（含打样测试 money_db_proof）。
- **票内假设被真库打样推翻（重要更正）**：「sqlx 类型派生（MySQL 字符串枚举原生支持，无需手写编解码宏）」**不成立**。实测 sqlx 0.8.6 派生强枚举对 MySQL ENUM 列**只可 Encode、不可 Decode**：派生只生成 `type_info() = MySqlTypeInfo::__enum()`（type=String + ENUM 标志，为参数绑定设计），不覆写 `compatible`，解码走默认严格类型相等；而 MySQL 结果集把 ENUM 列报告为 `ColumnType::Enum`，两者永不相等，连 `CAST AS CHAR`（VarString）也被拒。此前评审依据（sqlx-macros-core 源码）只覆盖绑定方向。方案按对照表 4.1 备选落地：**`wordlist_enum!` 宏统一手写 sqlx 三件套**（`compatible` 委托 `<str>::compatible` 宽松白名单、解码按字面量 parse、未知值报词表名）——严格失败语义与原设计一致。更正已登记对照表总则 1 与 `money_db_proof.rs` 头注。
- **交付物**：
  - `domain/src/money.rs`：`wordlist_enum!` 宏（一处定义字面量，生成 `as_str` 锚点 / `parse` / `Type`/`Encode`/`Decode` 三件套）+ 七枚举——`BizType`(2)、`PaymentOrderStatus`(6)、`RefundStatus`(5)、`PayMethod`(4)、`ExecutionPolicy`(2)、`BillStatus`(3)、`ChargePaymentStatus`(4，新建)。`is_paid_like`/`is_refundable`/`is_unsettled` 判定集合保留为 const fn。`db_literals_match_serde` 字面量对照测试锁定 serde 输出与 DB 字面量逐字节一致。
  - `domain/tests/money_db_proof.rs`：真库打样四项——①DEFAULT 行解码 ②枚举绑定 UPDATE ③Option 包裹 NULL/Some 解码 ④真实 `payment_order` 表绑定兼容；另有独立探针复核真实列解码/绑定/CAST 三链路全通。
- **迁移站点（22 文件，旧 `PaymentStatus`/`PaymentBizType` 全删）**：payment 域 apply/checkout/sources/recharge_bridge/card_funding/simulation、settlement 域 billing/recorder/refund_admin/refund_executor/user_invoice、wallet 域 refund/recharge/query、order/lib、charging receipts/sweeper、settlement bill、http-api admin_orders/admin_finance。行投影词表列直接解出枚举（`PayOrderRow`、`refund_record` 投影、`RechargeOrderRow`、`InvoiceExistingRow` 等），比较点 `as_str()` 全部改为枚举相等，SQL 绑定一律 `.bind(枚举值)`（含 `IN (?, ?)` 双占位与 `LIMIT ?`）。
- **范围调整（按对照表裁决，不属本票）**：钱包四词表归 `domain/wallet/`（票 10/11）、`user.gender` 归 identity（票 11）、`charge_billing_job` 的 `JobStatus` 归 settlement（票 08）、`PaymentIntentStatus` String 新类型保留（票 09）。
- **字面量比较点清点（只减不增核验）**：迁移点全部转枚举；生产代码保留的词表字面量均为合法类——入参白名单数组（`admin_orders.rs` 的 `PAYMENT_STATUSES`/`PAYMENT_BIZ_TYPES`，对照表总则 5 分层）、API 派生展示串（`wallet/query.rs` 退款视图 success/manual_review/rejected）、SQL 内部字面量（`SET status = 'refunded'`，票 07 迁移时处理）、其它域词表同字面量（charge_order.status 的 "paid"/"closed"，票 07）。旧类型残留仅 `admin_orders.rs:54` 注释中的旧版出处引用（合法）。
- **顺带修复（集成轮暴露的既有测试 bug，非本票引入——stash 基线同样失败）**：`admin_users_cards_flow.rs` 三处断言 `u["id"] == seed.user_a`（serde_json `Value::String == u64` 恒假；用户 id 按 `CAST AS CHAR` 字符串输出）改为 `as_str` 比较局部字符串。卡片列表断言（id 为数字输出）确认无需改。
- **测试基建缺陷登记（归票 04）**：flow 测试固定 `x-real-ip: 10.0.0.1` + 共享 Redis 登录频次计数（`admin:login-rate:10.0.0.1`），反复重跑触发 4291 使失败点漂移；本轮以清除该键恢复。票 04 统一端口锁时一并处理频次隔离。
