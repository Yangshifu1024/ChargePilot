# 阶段 00 · 设计定稿与工作区骨架

Status: ready-for-agent

## Problem Statement

开工没有可评审的基准：旧版 89 张表的 schema 不能照搬（要删 worker_db 两张表与投递退避表、合并三张 outbox 表、加命令退避字段）；订单状态机只存在于方案文字中，没有可执行的定义；前端到底消费了哪些响应字段没有审计结论，契约修剪无从谈起；Cargo workspace、迁移器、任务脚本都不存在。

## Solution

一次性产出四份评审基准（新单库 schema 设计稿、订单状态机定义、事件层接口定义、前端字段消费审计表）与可运行的 workspace 骨架（含单库迁移器）。评审通过即冻结，成为后续全部阶段的依据。

## User Stories

1. 作为开发者，我想拿到标注了模块归属的完整 DDL 设计稿，所以每张表在哪个模块写是评审过的而不是约定俗成的。
2. 作为开发者，我想让订单状态机的合法迁移与非法迁移在类型层可表达，所以非法流转在编译期就被拒绝。
3. 作为开发者，我想运行一条命令就在空库上建出全部表并记录迁移版本，所以环境搭建不依赖手工 SQL。
4. 作为前端维护者，我想拿到字段消费审计表，所以知道契约修剪会动到哪些页面。
5. 作为评审者，我想看到状态机定义里每个状态的事件来源与超时出口，所以能逐条对照旧版行为评审。
6. 作为开发者，我想有一个能跑 `cargo build` 与 `cargo test` 的空 workspace 和任务脚本入口，所以后续阶段不用重复搭骨架。

## Implementation Decisions

- **Schema 设计稿**（覆盖 [ADR 001](../../../docs/adr/001-single-database.md)）：以旧版三库 DDL 为列定义起点；删除 `scheduled_task`、`task_execution_log`、`sync_delivery_retry`；三张 outbox 合一（加 `source` 列）；命令表加 `next_retry_at`；遥测明细按月分区并给出清理策略；每张表标注归属模块（设备台账/开通属 device 域，连接会话/遥测/命令属 gateway-runtime 域——此归属是本次设计要显式定案的点）；约 84 张表。
- **状态机定义**（覆盖 [ADR 002](../../../docs/adr/002-event-driven-state-machine.md)）：状态 `paid → starting → charging → stopping → billing → settled → refunded`；每个迁移写明触发事件、同事务写入、失败出口（超时/拒绝/回滚）与兜底扫描接管条件。
- **事件层接口**：`domain_event` 表结构、投递器 trait、退避策略（`1 << min(retry, 8)` 秒）、兜底扫描框架签名。
- **字段消费审计**：扫描两个前端源码对每个后端响应字段的读取点，产出《字段消费审计表》：零消费字段清单（候选删除）、保留白名单（零消费但语义必要，逐条注明理由）、分页风格不一致清单。
- **Workspace 骨架**：总览 spec「Implementation Decisions」列出的全部 crate 建出空壳；`app` 提供 `serve` / `migrate` 子命令位；任务脚本入口（构建/测试/集成测试）就位；仓库完成 `git init` 与首个提交。
- **迁移器**：按文件名序执行、`schema_migrations` 记录版本、失败即停；单库。

## Testing Decisions

- 迁移器对空库执行后，表集合与设计稿一致（脚本比对，挂主缝之外作为工具自检）。
- 状态机：以类型系统表达非法迁移为例证（一条故意无法编译的反例测试）。
- 骨架：`cargo build`、空测试运行、迁移器幂等（重复执行跳过已应用版本）。
- 设计稿与审计表本身靠**人评审**验收，不写自动化测试。

## Out of Scope

任何业务模块的行为实现；Redis 依赖引入（阶段 1）；性能工作。

## Further Notes

本阶段验收 = 四份基准评审通过 + 骨架可运行。评审不通过则修订重评，不带病进入阶段 1。
