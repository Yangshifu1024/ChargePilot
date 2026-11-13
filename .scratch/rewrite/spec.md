# ChargePilot Rust 单体重写 · 总规格

Status: ready-for-agent

> 本文件是 `.scratch/rewrite/` 下 11 份阶段规格的总览。事实源：[docs/rewrite-plan.md](../../docs/rewrite-plan.md)（方案基线）与 [docs/adr/](../../docs/adr/README.md)（5 篇已接受 ADR）。阶段规格只写本阶段特有内容，与总方案冲突时以总方案 + ADR 为准并显式指出。

## Problem Statement

旧版 Go 项目按五个进程切分，把成本留在了数据模型与调用链上：三个 MySQL 库按进程而非领域切分、一条没有任何消费者的 Redis Streams 事件总线、充电链路靠双进程轮询接力（六跳、秒级延迟）、为不存在的需求建的 DB 调度框架。同时项目从未部署，2000 台设备并发连接 / 1000 台并发充电的容量目标从未被验证。

## Solution

以 Rust 重写为单体：单个可执行程序（HTTP 8080 + 设备 TCP 9100 + 后台任务），前后端打包进一个 Docker 镜像；单库合并、事件驱动状态机、号段 ID、契约以前端实际消费为界；借 headless 批量模拟器把容量目标变成可量化验收。按 11 个阶段交付，每阶段独立验收。

## User Stories（全局）

1. 作为运维者，我想用一条 `docker compose up` 启动包含前端、后端、MySQL、Redis 的完整栈，所以部署不再是五个进程加反向代理的拼装。
2. 作为运维者，我想只配置约 15 个环境变量，所以不再需要维护 36 个变量与三个数据库账号。
3. 作为开发者，我想让全部业务行为测试只通过公开 HTTP API 断言，所以内部重构不会破坏测试。
4. 作为开发者，我想用 headless 批量模拟器在笔记本上打满 2000 连接 / 1000 充电，所以容量验收不依赖真机。
5. 作为维护者，我想让每条架构决策都有 ADR 记录被否掉的替代方案，所以半年后不会重新争论已解决的问题。
6. 作为运营人员，我在新版中获得与旧版等价的全部后台能力（方案、设备、订单、财务、营销、导出），所以业务不因重写而缩水。
7. 作为充电用户，我在新版中获得扫码充电全流程，且充电中的实时数据延迟从秒级降到毫秒级。
8. 作为设备（桩），我在新版中按 DC589 5.8.9 协议通信，行为与旧版完全一致，所以固件无需任何变更。

## Implementation Decisions（全局，各阶段 spec 引用不重复）

- **模块边界**：Cargo workspace；纯规则在 `domain`，设备契约在 `protocol`/`protocol-dc589`，通用能力在 `platform`，持久化原语在 `storage`，事件投递在 `events`，12 个业务域在 `modules/*`，路由在 `http-api`，设备接入在 `gateway-runtime`，装配与子命令在 `app`。
- **红线层（行为不变）**：DC589 协议语义；计费算法与金额取整（int64 分 + decimal 中间计算、`StringFixed(6)` 输出）；行锁顺序、`FOR UPDATE SKIP LOCKED`、全部幂等表语义；微信回调路径；JWT 双 kind 与刷新令牌轮换；bcrypt。
- **配置面**：约 15 个环境变量，清单在阶段 09 冻结；`PAYMENT_MODE`（disabled/simulation/wechat_direct）与 `LOGIN_MODE`（wechat/development）语义保留。
- **订单状态机**：`paid → starting → charging → stopping → billing → settled → refunded`，非法迁移在类型层拒绝（ADR 002）。
- **事件语义**：`domain_event` 表事务性写入 + 进程内投递 + 指数退避 + 约 30s 兜底扫描；处理器必须幂等（ADR 002）。
- **ID**：号段模式（默认号段 500、水位 CAS、资金场景同事务强一致选项，ADR 003）。
- **技术栈**：axum + tokio + sqlx（手写 SQL）+ redis + rust_decimal + tracing + metrics；微信 APIv3 自行封装。
- **依赖链**：00 → 01 → 02 → 03 → 04 →（05、06 可并行）→ 07 → 08 → 09 → 10；02/03 期间可并行推进 06 的帧层纯函数部分。

## Testing Decisions（接缝，全部阶段共用）

一条主缝 + 三条辅缝，除此之外不加缝：

- **主缝：公开 HTTP API 黑盒**——契约测试（serde 序列化 + OpenAPI 路由快照）与真库集成测试全部通过 axum 路由发起，断言信封 `{code, message, data, request_id}` 与状态码；内部模块结构变化不破测试。
- **辅缝 1：DC589 帧编解码纯函数**——帧字节 ↔ 解析结果黄金样本，基准是 `docs/protocols` 的协议文档（旧仓库）。
- **辅缝 2：资金纯函数差分**——计费/退款/卡扣核心纯函数，同一批输入分别跑新旧两版比对输出；旧仓库只读参考。
- **辅缝 3：进程生命周期**——真实启动 → 注入故障 → 停机，断言 `/metrics` 指标与停机时序。
- 好测试的标准：只断言外部可观察行为（HTTP 响应、库中行、指标值），不断言内部调用关系。
- 真库集成沿用 `TEST_MYSQL_URL` / `TEST_REDIS_URL` 约定，默认 `cargo test` 不依赖 Docker。

## Out of Scope（全局）

多实例横向扩展（ADR 005 判据）；真实微信商户联调、weapp 真机发布、TLS/域名接入（上线前清单）；旧仓库的任何修改。

## Further Notes

每份阶段 spec 的 Out of Scope 只列该阶段额外排除项，不重复全局排除项。阶段开工前必须先读总方案对应小节与关联 ADR。
