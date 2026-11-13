# 充电订单状态机定义

状态：待评审（阶段 00 评审基准）
类型层实现：`backend/crates/domain/src/charging_state.rs`（骨架初稿，迁移拓扑已锁定）
依据：旧版代码逐写点核实（2026-11-14，核实方法：对 `charge_order` / `charge_billing_job` / `refund_record` 全部 GORM 状态写入做全仓清点）

## 一、主状态机（charge_order.status）

DB 枚举 8 态（真相源 `migrations/0001_init.sql` 的 `charge_order` 列定义，新版逐字节保留）：

```text
pending_payment   [死值：仅列 DEFAULT，旧版代码零写入；订单创建即 paid]
paid              [活跃] —— 支付回调/刷卡扣款事务内 INSERT 即此态
charging          [活跃]
completed         [活跃]
refunding         [活跃]
refunded          [活跃终态]
cancelled         [死值：无写入点]
failed            [死值：无写入点；启动失败走 refunding 而非 failed]
```

展示轴 `business_status` 为 DB 生成列（pending_start/charging/completed），随主状态自动派生，无需代码维护。
支付轴 `payment_status`（pending/paid/refunded/partial_refunded）**不是独立状态机**：仅在「资金实际到账/退回」的事务内由 `SyncOrderPaymentStatus` 按 `payment_order` 的已退金额重算，退款**申请**不推进该轴。

### 迁移图（全部带状态谓词 + RowsAffected==1 冲突检测）

| # | from → to | 触发场景 | 旧版证据 | 新版处置 |
|---|---|---|---|---|
| 0a | 创建 → paid | 支付回调验签到账，同事务建单+价格快照+券核销 | payment_callback_repo | 事件入口；保留 |
| 0b | 创建 → paid | 刷卡余额扣款事务内建单 | card_repo | 事件入口；保留 |
| 1 | paid → charging | 设备启动成功回执（ResultCode=0） | lifecycle_repo.ActivateInTx | 类型层 `PaidOrder::activate` |
| 2 | paid → refunding | 设备拒绝启动（ResultCode 1–3），记 failure_reason | MarkStartRejectedInTx | `PaidOrder::mark_start_rejected` |
| 3 | paid →（同事务）→ refunded | 余额支付启动被拒：迁移 2 后同事务钱包退回+置 refunded | balance_refund_repo | 两迁移同事务执行 |
| 4 | charging → completed | 设备结束回执（同事务入队计费任务） | CompleteInTx | `ChargingOrder::complete` |
| 5 | completed → refunding | 计费入账后渠道余款退款登记 | RequestChannelRefundInTx | `CompletedOrder::request_channel_refund` |
| 6 | completed →（同事务）→ refunded | 余额支付计费后余款即时退回 | billing_job_repo | 两迁移同事务执行 |
| 7 | refunding → refunded | 渠道退款全额成功回执（fully=true） | CompleteChannelRefundInTx | `RefundingOrder::settle_refunded` |
| 8 | refunding → completed | 渠道退款部分成功且 `ended_at` 非空（收尾恢复） | 同上 fully=false 分支 | `RefundingOrder::restore_completed` |

**新版收紧（有意的行为修正）**：旧版 `SetStatusRefunded` 按 id 无谓词更新；新版进入 `Refunded` 一律要求 from 证据类型（`RefundingOrder::settle_refunded` 或同事务的迁移 3/6 路径），杜绝裸写。

**死值处置**：`pending_payment` / `cancelled` / `failed` 保留在 DB 枚举（避免改列定义），类型层**不提供进入方法**；评审确认后可在后续迁移中评估收窄枚举。

**旧版缺口登记（新版补齐）**：
- **paid 启动无超时**：旧版对 paid 订单无限重扫重复下发 START，设备永不回执则资金永久滞留。新版：状态机增加 paid 态超时出口（兜底扫描判定超时后走「设备拒绝启动」同款路径进 refunding），超时阈值进配置。此为对 ADR 002 的行为补充，已登记 ADR 002 修订记录。
- **停止授权的弱保护写点**：`RecordBillingCutoff` 自身无状态谓词且不在锁内（三条入口中保护最弱），新版统一为「事务内锁读 charging 校验后再写」。

## 二、伴随子状态机（保持旧版语义，随阶段 04 落地）

### 计费任务（charge_billing_job.status：pending / manual_review / done，无 failed）

| 迁移 | 场景 |
|---|---|
| 创建 → pending | 结束回执事务入队 |
| pending → pending（重试） | 依赖失败：attempts+1、generation+1、next_attempt_at=now+1min |
| pending → manual_review | 计量不足或快照无效（同事务入队人工复核） |
| manual_review → pending | 人工定价/计量二审通过后重排 |
| pending → done | 费用入账成功（费用回执+资金+余款退款+任务同事务）；同内容回执重放直接补 done |

### 退款记录（refund_record.status：pending / processing / success / failed / rejected）

| 迁移 | 场景 |
|---|---|
| 创建 → pending/automatic | 启动失败(微信)、计费余款、迟到支付、钱包充值退款；或 manual_review（人工双审） |
| 创建 → success（同事务） | 余额路径即时退款（跳过执行器） |
| pending → processing | 执行器领取（30×2^n 秒退避，n≤7） |
| processing → failed | 渠道 CLOSED 或依赖缺失（permanent）；**failed 为事实终态，无出口** |
| processing → success | 渠道 SUCCESS：回执+payment 更新+订单迁移 7+钱包结算同事务 |
| pending(manual_review) → rejected | 复核单人驳回（终态） |
| pending(manual_review) → pending(automatic) | 双人复核通过 |

**注释与代码不符点（按代码事实定案）**：旧版 `money/payment.go` 注释称 failed「可按重试策略再次发起」，但全仓无 failed 重置路径——新版文档与类型层均按「failed=终态」处理；管理端 Retry 仅对 pending/processing 提前 next_attempt_at。
**渠道退款无回调推送**：微信退款结果靠执行器轮询查单，新版保持该设计（回调路径仅支付），评审知悉。

## 三、事件驱动的迁移触发（ADR 002 落地方式）

每个迁移的触发事件与同事务写入：

| 迁移 | 触发事件（来源） | 同事务写入 |
|---|---|---|
| 0a | 微信支付回调（HTTP） | payment_order + charge_order + 价格快照 + 券核销 + domain_event |
| 0b | 刷卡请求（设备帧→事件） | 钱包扣款 + charge_order + card_charge |
| 1 | 启动回执（gateway） | charge_command 状态 + 订单迁移 + domain_event |
| 2/3 | 启动拒绝回执（gateway） | 订单迁移 + [余额路径钱包退回 / 渠道路径 refund_record + domain_event(refund_required)] |
| 4 | 结束回执（gateway） | 订单迁移 + 计量分段 + billing_job 入队 + domain_event |
| 5 | 计费完成（结算事务） | 退款登记 + domain_event |
| 7/8 | 渠道退款查单结果（执行器轮询） | refund_record + payment + 订单迁移 + domain_event |

兜底扫描接管：每个迁移定义超时（如 paid 态启动超时、charging 态无心跳、refunding 态退款超期），由约 30s 兜底扫描重新推入事件流（阶段 08 统一实现）。

## 四、类型层非法迁移证明

`crates/domain` 以 newtype per state 实现：迁移方法只定义在合法 from 类型上。`tests/compile_fail/charging_on_paid_only.rs` 用 trybuild 证明「在 charging 状态调用 `activate()`」编译失败——非法迁移在编译期即被拒绝，评审可通过 `cargo test -p chargepilot-domain` 复现。
