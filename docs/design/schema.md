# 新单库 Schema 设计稿

状态：待评审（阶段 00 评审基准）
DDL 事实源：[backend/migrations/0001_init.sql](../../backend/migrations/0001_init.sql)（单文件基线，由旧版三库迁移合并改造生成）
依据：[ADR 001 单库合并](../adr/001-single-database.md)、[ADR 002 事件驱动](../adr/002-event-driven-state-machine.md)、重写方案 P1/P2/P7

## 一、相对旧版的差异清单

旧版三库 89 张表（central_db 71、gateway_db 16、worker_db 2）→ 新版单库 **84 张表**：

| 变更 | 对象 | 说明 |
|---|---|---|
| **删除** | `scheduled_task`、`task_execution_log` | worker_db 调度框架空架子，含 `webhook_dispatch` 种子行（P2） |
| **删除** | `sync_delivery_retry` | 跨进程投递退避，由进程内事件退避 + 命令表退避列取代（P7） |
| **合并** | `central_db.event_outbox` + `central_db.admin_event_outbox` + `gateway_db.event_outbox` → **`domain_event`** | 三张同构 outbox 合一。变化：加 `source` 列（user/admin/gateway 产生域）；`stream` 改名 `kind`（事件类别，处理器路由用）；`envelope_json` 改为 `payload_json`（进程内投递不再需要跨进程信封）；删除 `stream_message_id` 列及其索引（XAdd 缺口反查随 Redis Streams 退役）；新增 `idx_source_kind` 索引 |
| **加列** | `charge_command`、`charge_stop_command` | 各加 `next_retry_at datetime(3) NULL` 与 `KEY idx_retry (status, next_retry_at)`：命令下发失败/无确认的进程内退避重扫（P7，替代 sync_delivery_retry） |
| **并入** | 旧 `0002_invoice_active_unique.sql` | `invoice_request` 建表内直接带 `UNIQUE KEY uk_invoice_active (biz_type, biz_id)`（基线直接改 CREATE 的旧版约定） |
| **保留** | 其余 80 张表 | 列定义逐字节沿用旧版 DDL（含 telemetry 等表月分区、`card_event_delivery` 等表的 `utf8mb4_0900_ai_ci` 混排规则）；三库间本无跨库 JOIN/事务，合并无语义影响 |

保留表中的既有种子数据完整迁移：`snowflake_state` 水位行（1,0,0）、`permission` 全量权限行、角色与系统默认配置。

## 二、84 张表模块归属清单

写权归属即模块边界：**每张表只有一个模块拥有写权**，跨模块只读。评审通过后此清单作为 repo 层代码评审的对照基准。

### 基础设施（2）

| 表 | 归属 | 说明 |
|---|---|---|
| `domain_event` | `events` crate | 领域事件出站表（合并产物） |
| `snowflake_state` | `platform`（号段分配器） | 水位表，仅号段分配器读写 |

### 业务域（69）

| 归属模块 | 表 | 数 |
|---|---|---|
| `identity` | user, user_login_identity | 2 |
| `ops` | admin_user, admin_data_scope, admin_field_mask, role, permission, role_permission, announcement, audit_log | 8 |
| `device` | station, device_meta, device_import, device_import_identity, device_fault_report, device_fault_report_event, alert_event, **vendor** | 8 |
| `pricing` | pricing_template, pricing_rule, pricing_publication | 3 |
| `card` | online_card, online_card_audit, card_charge, card_operation | 4 |
| `order` | charge_order, charge_order_pricing, feedback | 3 |
| `charging` | active_port_charge, charge_prepay, charge_event_log, charge_start_receipt, charge_end_receipt, charge_fee_receipt, charge_billing_cutoff, charge_manual_settlement | 8 |
| `settlement` | charge_bill, charge_bill_read, charge_billing_job, charge_meter_review, fee_calculation, fee_delivery, fee_receipt, manual_fee_review, manual_refund_request, refund_record, refund_rejection, refund_review, refund_success_receipt, invoice_request, invoice_admin_review | 15 |
| `wallet` | wallet_account, wallet_txn, wallet_recharge_request, wallet_refund_request, wallet_refund_part, wallet_risk_freeze_link, wallet_risk_release, wallet_risk_review, risk_freeze_log | 9 |
| `payment` | payment_order, payment_callback_idempotent, **charge_payment_intent** | 3 |
| `marketing` | coupon, coupon_activity_rule, coupon_grant, coupon_grant_request, coupon_redemption | 5 |
| `export` | export_task | 1 |

### 设备运行时（13，`gateway-runtime` 独占写）

card_event_delivery, charge_command, charge_end_delivery, charge_process, charge_stop_command, device, device_event, device_port, device_provision, device_session, telemetry, telemetry_aggregate_15min, telemetry_aggregate_hourly

合计：2 + 69 + 13 = **84**。

## 三、归属评审点（评审时逐条定案）

1. **`vendor` 归 device 域**（上表已按此标注）：厂商 CRUD 是后台业务功能（device 域路由），gateway-runtime 仅按 `vendor_id` 只读引用；旧版由 gateway/provision 事务读写，新版开通流程由 device 域受理 + gateway-runtime 执行 TCP 同步（阶段 03/06 桥接）。
2. **`charge_payment_intent` 归 payment 域**：支付意图是支付域概念，充电只是场景；旧版 `payment_intent_repo` 亦在 payment。若评审倾向 charging，改归属不影响 DDL。
3. **`charge_order` / `charge_order_pricing` 归 order 域**：沿用旧版表模型归属（order/order_schema）；charging 生命周期子包经 order 模块的锁读与状态推进能力访问，不直写。
4. **`alert_event` 归 device 域**：告警产生与恢复判定来自设备事件（device 域同步流程）；后台认领路由属 ops 表现层，经 device 域服务调用，不直接写表。
5. **`charge_bill_read`（读模型表）归 settlement**：账单读投影由 settlement 维护，`ops.dashboard` 只读。
6. **列 COMMENT 中的旧库名引用**（3 处：「冗余自 gateway_db」「冗余自 admin_db」「冗余自 central_db.charge_order」）：单库后语义失效，评审时决定改写文案或保留历史注记。

## 四、验证方式

- `backend/migrations/0001_init.sql` 共 84 个 `CREATE TABLE`、无重名、无 DROP 残留（转换脚本已校验）。
- 迁移器对空库执行后的表集合与本文清单一致的比对脚本随骨架交付（`node tasks.mjs` 任务，见骨架 spec 验收）。
- DDL 可执行性：待阶段 00 验收时在真实 MySQL 8.4 上跑迁移器确认（本机暂无 docker/MySQL 实例，见验收指引）。
