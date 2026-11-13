# ADR 002：事件驱动与订单状态机

- 状态：已接受
- 日期：2026-11-14
- 关联：[rewrite-plan.md P4/P5/P6](../rewrite-plan.md)

## 背景

旧版充电链路是「双进程轮询接力」：

- 启动：支付成功 → worker 每 1s 轮询 paid-starts（ID 游标）→ HTTP 调 gateway 下发 START → worker 轮询 start-results → HTTP 提交 central 回执。
- 结束：end-events → meter-samples → freeze → end-result → release-port → mark-processed，共 6 跳，每一跳都可能失败且需要补偿记录（`sync_delivery_retry`、freeze/mark-processed 配对）。
- 异步总线：业务事务写 outbox → worker 轮询 → `XAdd` 到 8 条 Redis Stream → **没有任何消费者**（webhook/监管投递从未装配）。每条事件经历「MySQL → 轮询 → Redis →（无人读）」，纯开销。
- `worker_db` 调度器与 Redis Streams 均为无消费者的半成品基建。

## 决策

1. 以订单为聚合根建立**显式状态机**：`paid → starting → charging → stopping → billing → settled → refunded`，状态迁移与非法迁移在类型层定义。
2. 事件到达（支付回调、设备帧、用户停止、刷卡）在**同一事务**内写状态 + `domain_event` 表；事务提交后由进程内异步投递器直调处理器驱动下一步。投递失败指数退避重试，`at-least-once` 语义由表保证。
3. **兜底扫描保留但放宽间隔（约 30s）**：只负责把超时/遗漏状态重新推入事件流——进程崩溃、投递器故障后最终一致的保障。原 worker 的 finance 批次（计费/退款/自动停机）改写为各域的兜底扫描 task。
4. 删除：8 条 Redis Stream、3 个 outbox publisher、worker 进程及其 HTTP 端点、`sync_delivery_retry`。Redis 职责收敛为会话/MFA/频次/锁。
5. **红线不变**：全部幂等表（start/end receipt、freeze 语义、`card_operation.event_id`、`payment_callback_idempotent` 等）原样保留——设备消息天生重复，事件处理器必须幂等。

## 理由

- 端到端延迟从「秒级（双进程轮询 + HTTP 跳数）」降到毫秒级（进程内直调），且轮询 CPU/DB 开销消失。
- 可靠投递语义（事务性写入 + 退避重试 + 兜底扫描）完整保留，只是去掉「Redis 中转」这一跳——该跳在此系统中从来没有下游。
- 显式状态机让非法迁移（如未充电就结算）在类型层被拒绝，替代散落在各 handler 的状态检查。

## 后果

**正面**：延迟、跳数、死基建三者同时消除；测试从「mock 两个进程的 HTTP 时序」变成「直接构造事件序列」。

**负面与对策**：
- 新状态机可能偏离旧版行为，引入旧版没有的边界缺陷（本计划最高风险）→ 状态机定义在阶段 0 评审定稿；阶段 7 故障注入（回执迟到、事件丢失、进程重启）真库验证；兜底扫描保证最终一致。
- 事件处理器必须幂等，漏写幂等会在重试/重放时重复扣款 → 处理器接入强制要求幂等表或状态条件更新，评审清单固定项。
- 将来监管投递等外部消费者出现时，需要重新引入外部投递通道 → 在事件处理器层加 HTTP sender 即可，表结构无需变更；`REGULATORY_*` 配置与出站 SSRF 校验（旧版 netguard）随需重建并先写 ADR。

## 被否掉的替代方案

- **保守方案：进程内化但保留轮询节奏，状态机二期再做**：两期总成本更高，且轮询语义（游标、批量、补偿）恰恰是最容易翻译走样的部分；一次做对并把行为差异留在测试层验证更可控。
- **直接用 tokio broadcast channel 做进程内事件，不落表**：崩溃即丢事件，兜底扫描窗口变大，放弃。
