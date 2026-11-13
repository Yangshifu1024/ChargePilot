# ChargePilot 重写方案（基线文档）

状态：已确认（2026-11-14）
旧仓库：`/Users/yangzhenbiao/Code/chargepilot`（Go 多服务，下文称「旧版」）
本仓库：`/Users/yangzhenbiao/Code/SideProjects/ChargePilot`（Rust 单体，下文称「新版」）

关键前提：

1. **项目从未部署**。没有线上数据、没有已发布的小程序包、微信 appid 仍为占位符。因此：无数据迁移任务、契约允许修剪、架构可以大胆重构。
2. **容量目标**：至少 2000 台设备同时连接（TCP），至少 1000 台设备同时充电。
3. **部署形态**：单个后端可执行程序，前后端打包在一个 Docker 镜像中。

---

## 一、旧版现状（重构的出发点）

| 维度 | 数据 |
|---|---|
| Go 后端 | 770 个 `.go` 文件、90,839 行；非测试 49,939 行、测试 40,900 行（约 45%） |
| 前端 | admin-web：React 18 + antd 5 + Vite 5（约 7,600 行）；miniprogram：Taro 4.3 weapp+h5 双端（约 4,280 行） |
| 数据库 | MySQL 8.4 三库共 89 张表（central_db 71、gateway_db 16、worker_db 2）；已核实**无任何跨库 JOIN / 跨库事务**，三库间唯一重名表是 `event_outbox` |
| 进程 | 5 个服务进程约 189 条业务路由：user(8080)、admin(8081)、central(8082)、gateway(8083 + TCP 9100)、worker(8085) |
| 服务间耦合 | 45 个内部 HTTP 端点，`X-Service-Token` 静态令牌 |
| 后台循环 | worker 三循环（1s 调度 / 10s 资金 / 1s 同步+Outbox 发布）、gateway 两循环（5s 停机重试 / 1h 清理）、admin 一循环（1h 导出清理） |
| Redis | 会话轮换（Lua）、MFA、频次限制、租锁、8 条 Redis Streams（Outbox 出口，**无任何消费者**） |
| 外部集成 | 微信登录（jscode2session）、微信支付 APIv3（预支付/回调验签/退款）、监管 webhook（未装配，`REGULATORY_*` 未接线） |
| 协议 | DC589 5.8.9：帧 `SOP(0xEE)|LEN|CMD|SESSION(6B)|DATA|SUM`，36 条平台指令；空闲心跳 60s、充电中心跳 15s；注册 A0 每 3s×10 次重试 |

旧版的问题不是「写错了」，而是**多进程架构把成本留在了数据模型与调用链上**。新版借换语言之机一次性修正，而不是逐行翻译。

## 二、方案立场：三层判定

| 层 | 判定标准 | 内容 |
|---|---|---|
| **红线层（行为不变）** | 改动会引入资金/设备事故或安全漏洞 | DC589 帧协议与指令语义；计费算法与金额取整（int64 分 + decimal 中间计算）；行锁顺序、`FOR UPDATE SKIP LOCKED`、幂等表语义；微信支付回调路径与验签；JWT 双 kind（user 15min / admin 8h）与刷新令牌轮换安全语义；bcrypt 存量口令兼容（`$2a$` 互通） |
| **重构层（本次要改）** | 进程架构残留、半成品、已知坏味道 | 见第三节 P1–P15 与第四节 G1–G7 |
| **保留层（看着可改但不值得动）** | 改动收益 < 重新验证成本 | HTTP 信封 `{code, message, data, request_id}`；admin RBAC + 数据范围 + 字段掩码（`admin_field_mask` 已核实为活功能）；telemetry 预聚合表（15min/hourly）；Taro 前端架构与页面三件套 |

## 三、重构清单 P1–P15

### 数据层

- **P1 三库按进程切分 → 单库合并**（[ADR 001](adr/001-single-database.md)）
  89 张表合并到单 database：`event_outbox` 三表合一（加 `source` 列）；删除 `worker_db` 全部 2 张表；删除 `sync_delivery_retry`。单连接池，模块→表归属用清单文档 + Rust crate 依赖方向强制。

- **P2 worker_db 调度框架是空架子 → 删除**
  `scheduled_task`/`task_execution_log` 及租约 CAS、崩溃恢复、自动暂停整套机制，处理器登记表为空，唯一种子任务 `webhook_dispatch` 代码零引用（已核实）。固定周期任务直接用 tokio task；运行观测走 metrics + admin 诊断接口。

- **P3 `snowflake_state` 全局行锁热点 → 号段模式**（[ADR 003](adr/003-segment-id.md)）
  进程内按号段分配 + 水位持久化；资金场景保留与业务同事务提交的强一致选项。bigint 有序性、防回拨语义不变。

### 异步与事件链路（最大架构修正）

- **P4 Outbox → Redis Streams 总线无消费者 → 事务性事件表 + 进程内投递**（[ADR 002](adr/002-event-driven-state-machine.md)）
  保留一张 `domain_event` 表承载 at-least-once 语义（事务内写入、失败退避）；发布从 XAdd 改为进程内异步 task 直调处理器。删除 8 条 Redis Stream 与 3 个 publisher。将来监管投递启用时在处理器加 HTTP sender（HMAC 签名与出站 SSRF 校验随需重建）。

- **P5 充电链路「双进程轮询接力」→ 状态机 + 事件驱动 + 兜底扫描**
  以订单为聚合根建显式状态机（paid → starting → charging → stopping → billing → settled → refunded），事件到达在同事务写状态 + 事件，事务提交后异步驱动下一步；兜底扫描保留但放宽间隔（约 30s），只负责把超时/遗漏状态重新推入事件流。
  **红线**：全部幂等表（start/end receipt、freeze 语义、`card_operation.event_id`、`payment_callback_idempotent`）原样保留——设备消息天生重复。

- **P6 worker 进程消失**：finance 循环 → 各域兜底扫描；sync 循环 → 事件驱动；scheduler → P2 已删。`internal/worker` 对应职责不再作为独立模块存在。

### 流程与机制塌缩

- **P7 45 个内部 HTTP 端点 + 补偿对塌缩**
  serviceclient、`X-Service-Token`、双超时客户端整层删除；`sync_delivery_retry` 表删除（进程内重试 + 命令表 `next_retry_at` 退避）；freeze/mark-processed 多跳配对在单库同事务中完成，保留幂等列但删除跨进程配对协议；清理 4 个无客户端封装的 gateway 孤儿端点与 worker 无调用方运维端点，运维触发收敛为 admin 诊断接口或 CLI 子命令。

- **P8 netguard + 监管 webhook 未装配**：随 P4 裁撤，登记为「将来启用外部投递时的必选项」。

### API 层

- **P9 契约基线：以前端实际消费为界**（[ADR 004](adr/004-contract-boundary.md)）
  路径风格保留（`/api/v1/user|admin|public/*`），微信支付回调路径逐字符冻结；信封与错误码体系保留；响应字段按前端消费审计修剪；分页风格统一；OpenAPI 重新生成作为契约测试基准。

- **P10 分页/排序/时间格式统一审计**：消除游标（`after_id`）与页码两种分页风格并存的现状。

### 部署与配置

- **P11 配置从 36 个环境变量收敛到约 15 个**：3 个 DSN → 1，5 个监听地址 → 1；`SERVICE_TOKEN`、`*_INTERNAL_URL`、Caddy 相关全部消失。
- **P12 Caddy 退役**：单端口同源服务静态与 API；`ADMIN_TRUSTED_PROXIES` 信任链删除；TLS 由外部负载均衡承担，compose 提供可选 caddy profile。设备 TCP 9100 独立 listener。
- **P13 镜像内容**：单二进制（`serve` / `migrate` / `seed` 子命令）+ `/srv/admin` 静态资源 + `/srv/h5`（publicPath 改 `/h5/`）+ 迁移 SQL；weapp 产物走微信开发者工具单独上传。

### 工程层

- **P14 测试策略重定向**
  Go 版 45% 测试代码不逐文件翻译。三层重建：① 新契约层（serde 序列化测试 + OpenAPI 快照）；② 域规则差分层（计费/退款/卡扣核心纯函数用同一批输入对旧版跑差分对照——只对资金语义做，不做全量）；③ 真库集成层（资金链路与并发/幂等场景，沿用 `TEST_MYSQL_URL` 约定）。DC589 帧层用协议文档建黄金样本。

- **P15 文档治理轻量化**：不复制旧版 check-rules 全套不变量检查；关键取舍写 ADR（001–005），表归属清单随代码维护。

## 四、容量设计 G1–G7（目标：2000 连接 / 1000 并发充电）

### 负载预算（推导依据，见 [ADR 005](adr/005-capacity-boundary.md)）

| 负载项 | 推导 | 结果 |
|---|---|---|
| 帧接收速率 | 空闲 2000÷60s + 充电 1000÷15s | ≈ 83 帧/秒（帧最大 257 字节） |
| 心跳状态更新 | 2000 台 ÷ 60s | ≈ 33 UPDATE/秒（批量缓冲） |
| 遥测明细写入 | 1000 充电 × 15s 心跳附带端口数据 | ≈ 67 INSERT/秒（批量缓冲后更低） |
| 聚合 UPSERT | 15min 桶 × 1000 端口 | ≈ 1.1/秒 |
| 订单事件峰值 | 高峰 100 台/10min 结束 | ≈ 0.17 单/秒 |
| 实时估费读 | 30% 页面在线 × 10s 轮询 | ≈ 30 QPS（走内存快照，不穿透 DB） |
| 进程内存 | 每连接 ≈ 16KB | 2000 连接 ≈ 32MB（预算上限 512MB） |

结论：挑战不是吞吐（余量三个数量级），而是连接生命周期治理、写放大控制、重连风暴、慢消费者背压、可观测验收。

### 设计约束

- **G1 连接治理**：每连接一个 tokio task；帧解码硬上限 + 读超时 90s（防半开连接）；同设备号互踢（会话表原子替换）；accept 限速默认 200 连接/秒。
- **G2 遥测批量落库管道**：帧到达先更新会话内存快照（原子，供实时读），再入有界攒批通道（4096）；批量 task 每 200ms 或满 500 条单条多值 INSERT；聚合 UPSERT 同批合并。缓冲满丢弃最旧**明细**——明细可丢、快照与聚合不可丢（分级语义，[ADR 005](adr/005-capacity-boundary.md)）。
- **G3 命令下发**：每会话有界 mpsc 队列（深度 16）；慢消费者走命令表 `next_retry_at` 退避，不无限堆积。
- **G4 重连风暴**：发布重启后 2000 台按协议 3s×10 次重试，30 秒内涌入；accept 限速削峰 + 注册批量写 DB；心跳照收（快照无 DB 压力）。**验收：重启后 60 秒内全部恢复在线**。
- **G5 实时估费走内存快照**：充电中前端轮询从会话快照组装（≈30 QPS），DB 只服务历史曲线与对账。
- **G6 观测前置**：连接数、充电数、帧速率、批量缓冲深度与丢弃计数、事件投递延迟、兜底扫描滞后、DB 批量大小——阶段 1 内置，容量验收以指标说话。
- **G7 单实例边界**：目标容量余量 10 倍以上，不预置横向扩展；设备会话有状态，多实例需会话亲和——判据与拆法记入 [ADR 005](adr/005-capacity-boundary.md)，不做代码。

## 五、目标架构

### 目录结构（Cargo workspace → 单二进制）

```text
ChargePilot/
├── backend/                      # Rust workspace → 单二进制
│   ├── migrations/               # 新的单库 schema（约 84 张表，含设计修订）
│   └── crates/
│       ├── domain/               # 纯规则（计费/金额/范围校验），零 IO、零框架
│       ├── protocol/             # 设备契约 trait（Adapter/Event/Command）
│       ├── protocol-dc589/       # 帧编解码、36 条指令、故障码语义（唯一协议实现）
│       ├── platform/             # config/jwt/totp/bcrypt/号段ID/sessiontoken/
│       │                         #   phone/httpx(信封+错误码)/metrics
│       ├── storage/              # sqlx 池 + 迁移器 + dbutil(唯一键冲突/月首日/LIKE转义)
│       ├── events/               # domain_event 表 + 进程内投递器 + 兜底扫描框架
│       ├── modules/              # 12 个业务域 crate：identity / ops / pricing / device /
│       │                         #   card / charging / order / settlement / wallet /
│       │                         #   payment / marketing / export
│       ├── http-api/             # axum：user/admin/public 路由 + 静态资源 + 鉴权中间件
│       ├── gateway-runtime/      # TCP + DC589 会话 + 遥测批量管道 + 停机重试/补偿
│       └── app/                  # 装配、生命周期、子命令(serve/migrate/seed)
├── frontend/
│   ├── admin-web/                # 迁移自旧版（base '/admin/'、BrowserRouter 保持）
│   └── miniprogram/              # 迁移自旧版（H5 publicPath 改 '/h5/'，weapp 不进镜像）
├── docker/
│   ├── Dockerfile                # 多阶段：node:24 构建两前端 → cargo-chef 构建 → debian-slim 运行时
│   └── compose.yaml              # app + mysql:8.4 + redis:8（caddy 为可选 profile）
└── docs/                         # 本方案 + ADR + 表归属清单（随阶段产出）
```

### 单进程拓扑

```text
┌──────────────────── 单一 Docker 镜像 / 单进程 ────────────────────┐
│  :8080 HTTP (axum)                                                │
│   ├─ /api/v1/user/*    C 端 API（原 user 进程 41 条路由为起点）     │
│   ├─ /api/v1/admin/*   运营后台 API（原 admin 进程 97 条为起点）     │
│   ├─ /api/v1/public/*  微信支付回调（路径冻结）                     │
│   ├─ /admin/*          admin-web 静态资源 + SPA fallback           │
│   ├─ /h5/*             miniprogram H5                              │
│   └─ /health /metrics                                             │
│  :9100 TCP  DC589 设备接入（独立 listener task）                    │
│  后台 task：各域兜底扫描 / 停机重试(5s) / 清理×3(1h) / 事件投递器    │
│  45 条内部 HTTP 端点 → 全部内联为模块间 trait 调用（不复存在）        │
└────────────────────────────────────────────────────────────────────┘
        │ 单库单连接池                    │ Redis（会话/MFA/频次/锁）
        ▼                                ▼
   MySQL 8.4                        Redis 8
```

### 事件流（改造后）

```text
支付回调 ──┐                                     兜底扫描(≈30s)
刷卡事件 ──┼→ [事务: 状态+事件] → 事件投递器 → 启动/计费/退款处理器
用户停止 ──┘            ↑                              │
设备帧(gateway) ────────┘              超时/遗漏状态重新入队 ←┘
```

进程边界消失，**模块边界不消失**：设备运行时表写权独属 gateway 模块，`charge_billing_job` 属 settlement、`card_operation` 属 card 等写入归属规则在 Rust 侧用 crate 依赖方向强制。

## 六、技术选型

| 旧版组件 | 新版选型 | 说明 |
|---|---|---|
| gin + httpapi | axum + tower | 信封、错误码表、64KiB 请求体限制做成中间件/extractor |
| GORM + 手写 SQL | sqlx（MySQL，手写 SQL） | 资金/锁/幂等必须显式 SQL；不开编译期校验宏，运行期值映射用 `query_as` |
| goroutine | tokio（task + mpsc/watch + CancellationToken） | 每循环一个受管理 task，退出 join 后再关连接池 |
| go-redis + Lua | redis crate（tokio）+ deadpool | 4 段 Lua（会话轮换/注销/租锁/频次）原样移植 |
| golang-jwt | jsonwebtoken | HS256 双 kind |
| x/crypto/bcrypt | bcrypt crate | `$2a$` 互通（本项目未部署，仅防调试数据丢失） |
| 手写 RFC 6238 | 移植（hmac + sha1 + base32，约 100 行） | 保留 ±1 步容差 |
| shopspring/decimal | rust_decimal | 重点核对 `toCents` 取整与 `StringFixed(6)` 输出一致 |
| go-pay/wechat-sdk（登录） | reqwest 直调 jscode2session / getuserphonenumber | 仅两个 HTTP 调用 |
| wechatpay-go（支付） | 自行封装 APIv3（rsa + aes-gcm + reqwest） | 请求签名、平台证书下载与验签、回调 AES-256-GCM 解密、敏感信息加密。约 3–5 天专项 |
| snowflake（DB 行锁） | 号段模式（[ADR 003](adr/003-segment-id.md)） | — |
| robfig/cron | cron crate + 校验封装 | 保留「可选秒位、拒绝 TZ=」语义（兜底扫描用固定间隔，动态 cron 需求已随 P2 删除） |
| excelize | rust_xlsxwriter | 仅导出侧 |
| caarlos0/env | envy 或手写 | required / PAYMENT_MODE / 密钥长度校验照搬 |
| goose 迁移 | 自写极简迁移器（约 50 行） | 单库按文件名序执行 + `schema_migrations` 表 |
| DC589 TCP | tokio + bytes + tokio-util Encoder/Decoder | 会话表 DashMap |
| bubbletea 模拟器 | **新增 Rust headless 批量模拟器** | `simulator-batch --devices N --charging M --speedup K`，服务容量压测与日常联调（Go TUI 模拟器一次一台，无法容量验收） |
| slog | tracing + tracing-subscriber | request_id/trace_id 贯通 |
| — | thiserror + 统一错误码 enum | 对齐 1xxx 凭证 / 2xxx 冲突 / 5xxx 故障 |

## 七、实施计划

| 阶段 | 内容 | 验收标准 | 预估 |
|---|---|---|---|
| **0 设计定稿** | 新单库 schema 设计（89 → 约 84 表：删 worker_db×2、sync_delivery_retry，outbox 三合一；逐表标注模块归属；遥测分区与清理策略）；充电状态机定义；事件层接口；前端字段消费审计；workspace 骨架 + 迁移器 | schema DDL 与状态机评审通过；ADR 001–005 定稿 | 1.5 周 |
| **1 地基** | platform + storage + events crate；号段 ID；错误码表；G6 观测指标框架；优雅停机框架 | 停机时序、事件投递 at-least-once、号段防回拨各有测试 | 1.5 周 |
| **2 身份与后台** | identity（登录/会话/手机号）+ ops（账号/角色/数据范围/字段掩码/MFA/审计/公告/仪表盘）全量 | admin 登录→MFA→权限→审计真库测试通过；C 端 development 登录可用 | 1.5 周 |
| **3 基础数据** | pricing（方案/发布/继承）+ device（站点/设备/厂商/导入/故障/告警同步）+ 后台静态服务 | 导入→站点→方案发布流程真库测试通过 | 1.5 周 |
| **4 资金主链路** | card / charging（按 P5 状态机新设计）/ order / settlement / wallet / payment；微信 APIv3 封装专项 | 资金链路真库测试 + 计费差分对照通过；幂等/行锁/事件同事务语义真库验证 | 4 周 |
| **5 长尾** | marketing（券/活动/核销）+ export（Excel/清理循环） | 契约测试对应分组全绿 | 1 周 |
| **6 设备网关** | DC589 帧层 + 指令集 + G1 连接治理 + G2 批量管道 + G3 命令队列 + G4 限速与优雅停机 + 遥测聚合 + 停机重试/补偿 | 模拟器完成注册→充电→结束闭环；黄金样本帧测试通过 | 2.5 周 |
| **6.5 压测工具** | Rust headless 批量模拟器（注册/心跳/充电/结束全流程，支持加速与并发规模参数） | 2000 台注册稳定、充电比例参数可控 | 0.5 周 |
| **7 兜底与运维收口** | 各域兜底扫描 task、admin 诊断接口、metrics 收尾、导出清理 | 故障注入（回执迟到/事件丢失/进程重启）真库测试通过 | 0.5 周 |
| **8 打包收敛** | 前端进镜像、单 compose、配置收敛到约 15 个变量、H5 publicPath | `docker compose up` 一条命令全栈可用，浏览器可用后台、H5 可下单 | 0.5 周 |
| **9 验证与切换** | 契约/OpenAPI 快照全绿、E2E 闭环、**容量验收** | 见第八节验收标准 | 1.5 周 |

合计约 **15–16 周全职当量**。阶段 2–5 与阶段 6 可部分并行。

**上线前清单（不计入本计划完成标准）**：真实微信商户联调（appid/商户号就位后的 APIv3 真实回调验签与退款）、小程序 weapp 真机发布、TLS 与域名接入。

## 八、验收标准

功能验收：

1. 新契约层全绿（serde 序列化 + OpenAPI 快照，基准为阶段 0 审计后的新 OpenAPI）；
2. 资金域差分对照通过（同输入下计费/退款/卡扣输出与旧版一致）；
3. 资金链路与并发/幂等真库集成测试通过；
4. E2E 闭环：批量模拟器完成「注册 → 扫码 → 启动 → 计量 → 停止 → 结算 → 退款」全链路；
5. 故障注入：回执迟到、事件丢失、进程重启后状态收敛。

容量验收（全部以 G6 指标实测，见 [ADR 005](adr/005-capacity-boundary.md)）：

| 指标 | 目标 |
|---|---|
| 并发连接 | 2000 台稳定在线 ≥ 30 分钟，进程内存 < 512MB |
| 并发充电 | 1000 台同时充电，遥测端到端 p99（帧到达 → 库可查）< 1s |
| 重连风暴 | 进程重启后 2000 台 60 秒内全部恢复在线 |
| 明细完整性 | 批量缓冲丢弃计数可解释，快照与聚合零丢失 |

## 九、风险清单

| 风险 | 等级 | 缓解 |
|---|---|---|
| 状态机 + 事件驱动重构偏离原行为，出现旧版没有的边界缺陷 | **高** | 状态机阶段 0 评审定稿；兜底扫描保证最终一致；阶段 7 故障注入覆盖崩溃/丢帧/迟到回执 |
| 微信支付 APIv3 自行封装的验签/解密出错导致回调丢失 | **高** | 官方测试向量 + simulation 模式验收；回调幂等表兜底重放；真实商户联调列入上线前清单 |
| decimal 取整/输出与 shopspring 语义漂移 → 计费分位差异 | 高 | 域规则差分对照 + `StringFixed(6)` 输出契约测试 |
| 单库合并后模块越界写表失去库级隔离 | 中 | crate 依赖方向强制 + repo 表名集中 + 阶段 0 表归属清单评审 |
| 契约修剪波及前端 | 中 | 字段消费审计先行；前端改动随阶段 8 一次性回归 |
| DC589 二进制细节（BCD、字节序、校验）移植出错影响真实设备 | 中 | 黄金样本帧测试 + 模拟器 E2E；协议文档 1065 行在手 |
| tokio 取消语义与 goroutine 不等价，停机丢任务/丢缓冲 | 中 | 每循环 CancellationToken + join；G2 冲刷纳入优雅停机序列并有测试 |
| Redis Lua（会话轮换保留 32 旧摘要、KEEPTTL）移植出错 → 登录态异常 | 中 | 脚本原样复制 + 集成测试覆盖轮换/注销 |
| 工期长，旧仓库演进造成目标漂移 | 中 | 本文档 + ADR 冻结基线；旧仓库只读参考，新需求一律进新版 |

## 十、决策记录

架构决策以 ADR 形式维护在 [docs/adr/](adr/README.md)：

- [ADR 001 单库合并](adr/001-single-database.md)
- [ADR 002 事件驱动与状态机](adr/002-event-driven-state-machine.md)
- [ADR 003 号段模式 ID 分配](adr/003-segment-id.md)
- [ADR 004 契约边界](adr/004-contract-boundary.md)
- [ADR 005 容量与可用性边界](adr/005-capacity-boundary.md)

新决策追加编号，不修改已接受 ADR 的决策内容（可追加「修订记录」节）。
