# 数据库 ENUM 词表对照表（S3 枚举化设计输入）

状态：已评审（票 05 交付物，2026-11-14）
真相源：[backend/migrations/0001_init.sql](../../backend/migrations/0001_init.sql)（63 个 ENUM 列，本文计数与其一致）
依据：[refactor-plan.md](../refactor-plan.md) 4.1 节与附录 A 评审记录；票 [05](../../.scratch/engineering-refactor/issues/05-enum-mapping-table.md)
上游更正：方案初稿示例曾把 `charge_order.payment_status`（4 值）误注释为 `payment_order.status`。经逐常量核实，**现有代码三型均已分立且忠实**：`PaymentStatus`（6 常量）恰为 `payment_order.status` 六值，`PaymentIntentStatus`（5 常量，文档注释明确分立）恰为 `charge_payment_intent.status` 五值。**无需拆分存量类型；真正缺的是 `charge_order.payment_status`（4 值）的专有类型**。票 06 的「拆型」工作据此修正为「补型 + 全量转正」。

## 一、总则（S3a–S3h 统一范式）

1. **目标范式**：每列一个 Rust 枚举，`#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]` + `#[serde(rename_all = "snake_case")]`，**sqlx 三件套（`Type`/`Encode`/`Decode`）由 `wordlist_enum!` 宏统一手写生成**（money 域见 `domain/src/money.rs`）。每枚举配「字面量对照测试」锁 serde 输出与 DB ENUM 字面量逐字节一致。
   **派生更正（票 06 真库打样结论，2026-11-14）**：`#[derive(sqlx::Type)]` 强枚举在 MySQL 下**只可用 Encode，不可用 Decode**——派生只生成 `type_info() = MySqlTypeInfo::__enum()`（type=String + ENUM 标志，为参数绑定设计），不覆写 `compatible`，解码走默认严格类型相等，而 MySQL 结果集把 ENUM 列报告为 `ColumnType::Enum`，两者永不相等，连 `CAST AS CHAR` 也被拒。此前评审依据（sqlx-macros-core 源码）只覆盖了绑定方向，未验证解码方向。手写宏的兼容判定委托 `<str>::compatible`（接受 Enum 列且非 BINARY），解码按字面量 parse，未知值即 schema 漂移报警——严格失败语义与原设计一致。真库打样证据：`domain/tests/money_db_proof.rs`（DEFAULT 行解码、枚举绑定、`Option` 包裹 NULL/Some、真实表 COUNT 四项）。
2. **未知值策略（三分支）**：数据库→Rust 方向**严格失败**（手写 decode 的 parse 原生行为，未知值即 schema 漂移报警）；设备/外部上报方向**透传保留**（仅适用于第四节 JSON 词表，63 列中无此前提）；API 入参方向解析失败返回参数错误（现状即 400/1005）。生成列（`charge_order.business_status`、`charge_payment_intent.active_port_code`）**不建 Rust 类型**，由数据库派生。
3. **共享词表规则**：同一词表服务多列时一型多用（如 `BizType` 三列）；不同表的同义/同值词表各自建型（如两套 `active/disabled` 与 `active/frozen` 不合并；`user.status` 与 `wallet_account.status` 同值不同义）。
4. **词表归属**：一律落 `domain/<域>/` 子树，写点 crate 按依赖规则引用 domain。由此对方案 3.2 目录树补五个子树：`order/`、`settlement/`、`wallet/`、`pricing/`、`export/`。
5. **入参白名单与 DB 词表分两层**：API 过滤参数的取值集合（含派生表达式的 `none/processing` 等非枚举值）用判定集合建模，不与 DB 枚举共用类型。
6. 枚举切片进 SQL 一律绑定参数；现有「绑定关联常量（实为 `&'static str`）」的写法全部替换为绑定枚举值。

## 二、拆型与归属裁决

| 裁决 | 结论 | 理由 |
|---|---|---|
| 支付单/意图/订单支付轴三型 | `PaymentOrderStatus`（6 值）、`PaymentIntentStatus`（5 值）、`ChargePaymentStatus`（4 值，**新建**）三型分立 | 现有 `PaymentStatus`/`PaymentIntentStatus` 已分立忠实（见文首更正）；订单支付轴仅存于 `http-api` 入参白名单，无类型 |
| `BizType`（charge/wallet_recharge） | 一型三用（`payment_order`/`refund_record`/`invoice_request` 三列） | 三列取值集合逐字节相同且语义同一（资金业务面向） |
| 钱包侧词表归 wallet 不归 money | `WalletAccountStatus`、`WalletFreezeStatus`、`WalletTxnDirection`、`WalletTxnBizType` 落 `domain/wallet/` | 消费点全在钱包域；money 依赖钱包词表无需求，避免反向依赖 |
| 卡投递状态归 card | `CardEventDeliveryStatus` 落 `domain/card/`（写点在网关运行时，经 domain 引用） | 表语义属卡域；与 `CardOperationStatus` 同域 |
| 充电命令状态归 charging | `ChargeCommandStatus`、`ChargeStopCommandStatus` 落 `domain/charging/` | 写点（命令通道与停止服务）均在充电域 |
| 导出词表迁出 operations | `ExportStatus`、`ExportFileFormat` 随 S3g 迁 `domain/export/` | 与 `modules/export` 域对齐；`operations` 保留后台账号/范围/公告词表 |
| `TriggerType` 更名 `ActivityTriggerType` | 转正时更名 | 现名过泛，表语义是活动触发类型 |
| 设备故障事件含 `migration_baseline` | 枚举含该变体，严格解码 | 它是合法 DB 取值（迁移种子写入），非未知值 |

## 三、63 列对照总表

「现有形态」缩写：新类型=String 新类型；容器=常量串容器；字面量=裸字符串比较；SQL=仅 SQL 字面量无 Rust 类型；enum=已是枚举。策略列 D=严格失败（全部 62 个建型词表默认 D，无透传例外）。

### money 词表（8 列 → 6 型；`invoice_request.biz_type` 为 BizType 第三列，按词表而非域归组）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| payment_order.status | initiated,paid,closed,refunded,partial_refunded,failed | 新类型 `PaymentStatus`（6 常量，忠实） | `PaymentOrderStatus`（转正更名，避免与订单支付轴混淆） |
| payment_order.biz_type | charge,wallet_recharge | 新类型 `PaymentBizType` | `BizType`（一型三用） |
| payment_order.pay_method | wechat,balance,mixed,coupon | 新类型 `PayMethod` | `PayMethod` |
| refund_record.status | pending,processing,success,failed,rejected | 新类型 `RefundStatus` | `RefundStatus` |
| refund_record.biz_type | 同上 | 同上 | `BizType` |
| refund_record.execution_policy | manual_review,automatic | 新类型 `ExecutionPolicy` | `ExecutionPolicy` |
| charge_bill.status | issued,settled,voided | 新类型 `BillStatus` | `BillStatus` |
| invoice_request.biz_type | charge,wallet_recharge | 字面量 | `BizType`（第三列） |

### order 词表（含 feedback，5 列 → 4 型 + 1 生成列）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| charge_order.status | pending_payment,paid,charging,completed,cancelled,failed,refunding,refunded | 容器 `OrderStatus`（modules 内） | `ChargeOrderStatus`（迁 domain/order/） |
| charge_order.payment_status | pending,paid,refunded,partial_refunded | 字面量（仅 http-api 入参白名单） | `ChargePaymentStatus`（**新建**） |
| charge_order.business_status | pending_start,charging,completed | 生成列 | **不建型**（数据库派生） |
| feedback.category | rating,complaint,suggestion | 字面量 | `FeedbackCategory` |
| feedback.status | pending,processed,closed | 字面量 | `FeedbackStatus` |

### card 域（4 列 → 4 型）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| card_operation.kind | start,extend | 容器 `CardOperationKind` | `CardOperationKind` |
| card_operation.status | confirming,confirmed,failed | 容器 `CardOperationStatus` | `CardOperationStatus` |
| online_card.status | active,lost,disabled,unbound | 容器 `CardStatus` | `CardStatus` |
| card_event_delivery.status | pending,done | 字面量 | `CardDeliveryStatus` |

### settlement 词表（6 列 → 6 型）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| charge_billing_job.status | pending,manual_review,done | 容器 `JobStatus`（modules 内） | `BillingJobStatus` |
| charge_meter_review.status | awaiting_second,approved,rejected | 字面量 | `MeterReviewStatus` |
| invoice_request.invoice_type | normal,vat_special | 字面量 | `InvoiceType` |
| invoice_request.review_status | pending,approved,rejected,issued | 字面量 | `InvoiceRequestReviewStatus` |
| invoice_admin_review.review_status | awaiting_second,approved,rejected | 字面量 | `InvoiceAdminReviewStatus` |
| manual_fee_review.status | pending,resolved | 字面量 | `ManualFeeReviewStatus` |

### payment 域（1 列）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| charge_payment_intent.status | initiated,paid,expired,closed,refund_required | 新类型 `PaymentIntentStatus`（5 常量，忠实） | `PaymentIntentStatus`（迁 domain/payment/） |

### wallet 域（4 列 → 4 型）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| wallet_account.status | active,frozen | 字面量 | `WalletAccountStatus` |
| risk_freeze_log.status | frozen,unfrozen | 字面量 | `WalletFreezeStatus` |
| wallet_txn.direction | in,out | 字面量 | `WalletTxnDirection` |
| wallet_txn.biz_type | recharge,pay,refund,gift,freeze,unfreeze,adjust | 字面量 | `WalletTxnBizType` |

### identity 域（2 列）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| user.status | active,frozen | 字面量 | `UserStatus` |
| user.gender | unknown,male,female | 字面量 | `UserGender` |

### ops 词表（6 列 → 6 型，其中 1 型已存在）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| admin_user.status | active,disabled,locked | 字面量 | `AdminUserStatus` |
| admin_data_scope.scope_type | station,vendor | 新类型 `ScopeType` | `AdminScopeType`（更名） |
| announcement.scope | global,station,city | 字面量 | `AnnouncementScope` |
| announcement.status | draft,published,expired | 字面量 | `AnnouncementStatus` |
| domain_event.status | pending,published,failed | 字面量 | `EventDeliveryStatus` |
| domain_event.source | user,admin,gateway | **enum** `EventSource` | 保留（范式正例） |

### device 词表（13 列 → 13 型）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| device_fault_report.report_source | user,inspect,monitor | 字面量 | `FaultReportSource` |
| device_fault_report.fault_type | mechanical,electrical,communication,display,other | 字面量 | `FaultType` |
| device_fault_report.status | open,dispatched,fixed,closed | 字面量 | `FaultReportStatus` |
| device_fault_report_event.event_type | reported,dispatched,reassigned,fixed,closed,migration_baseline | 字面量（migration_baseline 仅 SQL） | `FaultReportEventType` |
| alert_event.severity | warning,critical,fatal | 字面量 | `AlertSeverity` |
| alert_event.status | active,acknowledged,resolved,auto_resolved | 字面量 | `AlertStatus` |
| device_import.status | pending,completed,failed | 字面量 | `DeviceImportStatus` |
| device_meta.status | enabled,disabled,retired,fault | 字面量 | `DeviceMetaStatus` |
| device.status | enabled,disabled,retired | 字面量 | `DeviceStatus` |
| device_port.status | idle,charging,full,fault,disabled | 字面量（full 仅 SQL） | `DevicePortStatus` |
| station.status | active,disabled,construction | 字面量 | `StationStatus` |
| device_session.protocol | tcp,mqtt | 字面量 | `SessionProtocol` |
| vendor.protocol | tcp,mqtt,hybrid | 字面量 | `VendorProtocol` |
| vendor.status | enabled,disabled | 字面量 | `VendorStatus` |

### charging 域（2 列）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| charge_command.status | pending,sent,acked,stopping,rejected | 字面量 | `ChargeCommandStatus` |
| charge_stop_command.status | pending,sent,acked,rejected | 字面量 | `ChargeStopCommandStatus` |

### marketing 域（6 列 → 6 型）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| coupon.discount_type | amount,percentage,time_free | 新类型 `DiscountType` | `DiscountType` |
| coupon.status | active,disabled | 新类型 `CouponStatus` | `CouponStatus` |
| coupon_activity_rule.trigger_type | first_recharge,invite_reward,threshold_redeem,holiday | 新类型 `TriggerType` | `ActivityTriggerType`（更名） |
| coupon_activity_rule.status | active,disabled | 新类型 `ActivityStatus` | `ActivityStatus` |
| coupon_grant.grant_source | register,activity,invite,manual,invite_reward | 新类型 `GrantSource` | `GrantSource` |
| coupon_grant.status | unused,used,expired | 新类型 `GrantStatus` | `GrantStatus` |

### pricing 域（3 列）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| pricing_rule.status | active,disabled | 字面量 | `PricingRuleStatus` |
| pricing_rule.channel | default,temp,card | 字面量 | `PricingChannel` |
| pricing_template.status | active,disabled | 字面量 | `PricingTemplateStatus` |

### export 域（2 列）

| 列 | 取值 | 现有形态 | 目标枚举 |
|---|---|---|---|
| export_task.file_format | csv,xlsx | 新类型 `ExportFileFormat` | `ExportFileFormat`（迁 domain/export/） |
| export_task.status | pending,running,completed,failed,expired | 新类型 `ExportStatus` | `ExportStatus`（迁 domain/export/） |

**实施完成（2026-11-14，S3 批次票 06–13 + 收尾）**：60 个目标枚举全部落地——money 7（票 06，含新建 ChargePaymentStatus）、order 3+支付轴接线（票 07）、settlement 6（票 08）、payment_intent 1（票 09）、device 14+card 4（票 10）、identity 2+ops 4（票 11）、marketing 6+export 2（票 12）、events 2（票 13）、wallet 4 + pricing 3 + charging 2（拆票遗漏，随收尾补齐落 domain/wallet.rs、pricing.rs、charging_command.rs）。所有枚举配字面量对照测试；DB 词表 sqlx 三件套手写（见总则 1 更正）。收尾补记：wallet 四词表自 modules/wallet/lib.rs 迁入并补 sqlx 接线，行读取的静默回退（unknown→Active）改严格失败；旧版「未知范围类型静默忽略」「未知折扣类型返回 NotFound」语义由词表穷尽承接（登记于票 11/12 Comments）。

**计数核对**：63 列 = 62 个建型列 + 1 个生成列（`business_status`）；62 列中 `BizType` 一型三列（payment_order/refund_record/invoice_request），故目标枚举 **60 个**，其中 20 个已有 String 新类型/容器形态转正（money 6 + marketing 6 + operations 3 + card 3 + payment_intent 1 + order 1，扣除 `BizType`/`PaymentOrderStatus` 等更名合并后见批次表）、40 个全新；`EventSource` 已是枚举保留不动（计入 60 内为「保留」）。

## 四、JSON 持久化词表（无 DB 独立列，透传约束）

| 词表 | 现状 | 持久化位置 | 约束 |
|---|---|---|---|
| `StopReason`（14 常量 + 未知值） | 新类型 (pub String) | `charge_end_receipt.meter_json`（serde 往返后按 `PartialEq` 幂等比对）、`device_event.payload_json`、`SessionActual` | 转正为枚举时 serde 用 **untagged 或自定义实现**保持 JSON 逐字节透传（未知值序列化为原字符串而非 `{"other":…}`）；`PartialEq` 语义保留 |
| `StopPlan.reason` | 中文文案字段（「达到最长时长」等） | 同上（非词表轴） | 不并入任何枚举，保持自由文本 |

## 五、非 DB 词表附注（进程内，不属本表 63 列，第二批转正）

`ChargeMode`（充电模式，含协议码映射）、`ServiceBasis`、`TierPriceBasis`（计费规则词表）、事件 `kind` 词表（varchar 无约束，票 13 单独处理）。它们是 String 新类型但服务于纯规则层，转正策略（含 DC589 协议码互转）在票 06 打样后另行小批处理，不阻塞 63 列主线。

## 六、批次映射（与工票对应）

| 工票 | 覆盖本表节 | 新建枚举数 |
|---|---|---|
| 06 money 打样 | money 域 8 列 + `ChargePaymentStatus`（提前到打样批，因其与支付单状态同源易混） | 7 |
| 07 order | order 2 型 + feedback 2 型 | 4 |
| 08 settlement | 6 型 | 6 |
| 09 payment | 1 型（迁移现有） | 1 |
| 10 device+card | device 13 + card 4 | 17 |
| 11 identity+ops | 2 + 6（含 EventSource 保留） | 7 |
| 12 marketing+export | 6 + 2（迁移动作） | 8 |
| 13 events | domain_event.status + kind 进程内词表 | 2 |
