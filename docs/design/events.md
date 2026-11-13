# 事件层接口定义

状态：待评审（阶段 00 评审基准；运行时实现在阶段 01）
代码契约：`backend/crates/events/src/lib.rs`（trait 与词表已锁定，编译即契约）
依据：[ADR 002](../adr/002-event-driven-state-machine.md)、重写方案 P4/P5

## 一、表结构

`domain_event`（见 [schema.md](schema.md) 合并产物）：`id, event_id(uk), source(user|admin|gateway), kind, payload_json, status(pending|published|failed), retry_count, last_error, scheduled_at, published_at, created_at`；索引 `idx_status_sched(status, scheduled_at)`（投递扫描）、`idx_source_kind(source, kind, created_at)`（排查）。

## 二、核心语义

1. **事务性写入**：事件与业务写入在同一事务提交（`enqueue::in_tx`），提交前对外不可见——这是 at-least-once 的根基。
2. **进程内投递**：提交后由运行时 task 扫描 `status='pending' AND scheduled_at<=NOW()`，按 `kind` 路由到 `EventDispatcher` 实现，**不经任何网络中转**（替代旧版 outbox→Redis Streams）。
3. **失败退避**：投递失败置 `failed` 并重排 `scheduled_at = now + 1 << min(retry, 8)` 秒（`retry_count` 上限 8，退避上限 256 秒），退回扫描队列；连续失败只告警不丢弃。
4. **完成标记**：处理器成功后置 `published`，记 `published_at`；`published` 行保留供追溯，由周期清理任务按保留期删除（阶段 08）。
5. **处理器幂等**：处理器必须以业务幂等表或状态谓词自保证（重放不重复扣款），这是处理器接入的评审固定项。
6. **兜底接管**：投递器自身故障（如进程重启）后，`pending`/`failed` 行由重启后的扫描自然恢复；无需额外恢复流程。

## 三、类型契约（骨架已锁定）

```text
EventSource  { User, Admin, Gateway }          → domain_event.source 词表
kind 词表    charge_ended / refund_required / refund_succeeded / charge_settled /
             charge_payment_confirmed / wallet_recharge_settled / device_event
EnqueuedEvent { id, event_id, source, kind, payload, retry_count }
EventDispatcher::dispatch(&EnqueuedEvent) -> Future<Result<()>>   // 失败即退避重排
enqueue::in_tx(&mut MySqlConnection, source, kind, &Value)        // 业务事务内入队
```

kind 词表首批沿用旧版流名（语义不变），随各域落地增补；增补即改词表常量并补评审说明。

## 四、与旧版的差异（评审要点）

| 旧版 | 新版 |
|---|---|
| 三张 outbox 表（按进程分） | 一张 `domain_event`（`source` 区分产生域） |
| worker 进程轮询 + `XAdd` 到 8 条 Redis Stream | 进程内 task 扫描 + 直调处理器 |
| 信封 JSON（event_id/stream/时间戳冗余包装） | 直接载荷 `payload_json` |
| 无消费者（webhook/监管未装配） | 每类 kind 必须登记处理器（编译/启动期校验，未登记 kind 启动报错） |

## 五、阶段 01 实现清单（此处只登记，不展开）

投递运行时（扫描批次、退避重排、published 标记）、kind→处理器登记表与启动校验、`/metrics` 投递延迟与队列深度指标、停机时的在途事件等待。
