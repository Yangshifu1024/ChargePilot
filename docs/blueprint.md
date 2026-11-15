# ChargePilot 架构蓝图

本文档是本项目**唯一**的架构与实施基准：现状从代码核实，目标只看本文，不引用任何外部项目。规则只有两条：

1. **已定事实（第二章）不可再议**——改动它们等于架构变更，须先改本章再动代码；
2. **偏离必须登记（第九章）**——实施中放弃本文任何条目，当场在登记表写一行（原条款、决定、理由、日期）。静默漂移视为违例。

其他文档分工：[CODING_STANDARDS.md](../CODING_STANDARDS.md) 是编码硬规则（编码前必读）；[operations.md](operations.md) 是运行与数据维护手册；`docs/agents/` 是代理工作流约定；表结构真相源是 `backend/migrations/0001_init.sql`。

---

## 一、项目定位

充电桩运营平台。单二进制 Rust 服务：axum HTTP（`:8080`，后台 `/admin/`、用户端 `/h5/`、探活与指标）+ 设备 TCP 接入（`:9100`，DC589 协议），MySQL 8 单库。前端两套：`frontend/admin-web/`（React 运营后台）、`frontend/miniprogram/`（Taro，H5 与微信小程序）。

容量目标：**2000 台长连接 / 1000 并发充电**。支撑手段已内建：连接治理（同设备二次连接顶替）、心跳/遥测快照 + 有界缓冲批量落库、命令队列与退避、accept 限速削峰、15 分钟预聚合。容量验收以指标说话（`/metrics`），不以感觉说话。

## 二、已定架构事实

| # | 事实 | 真相源 |
|---|---|---|
| F1 | Cargo workspace 单仓：`backend/crates/` 下 11 个顶层 crate + `modules/` 12 个域 crate（card / charging / device / export / identity / marketing / ops / order / payment / pricing / settlement / wallet） | `backend/Cargo.toml` |
| F2 | 单库单连接池。所有 DDL 变更改 `migrations/0001_init.sql`（迁移幂等，`serve` 启动时自动执行） | `backend/migrations/` |
| F3 | 事件：`domain_event` 表在业务事务内写入（at-least-once），进程内异步投递，各域兜底扫描推滞者；`EventKind` 是枚举，禁止常量串 | `crates/events/` |
| F4 | ID 分配：号段模式（进程内分配 + 水位持久化防回拨）；资金场景保留与业务同事务提交的强一致选项 | `crates/platform/src/ids.rs` |
| F5 | 设备契约两层：`crates/protocol/`（trait 与 wire 类型，294 行，唯一会合点）+ `crates/protocol-dc589/`（唯一协议实现，2861 行，零业务依赖）。**帧格式、36 条指令、故障码语义是冻结红线**；新协议 = 新增实现 crate，不改根契约 | `crates/protocol*` |
| F6 | HTTP 信封 `{code, message, data, request_id}` 与错误码表冻结；分页双轨上界（C 端 / 后台各自 `limits` 值）由 `domain/page.rs` 唯一校验 | `crates/platform/src/httpx.rs`、`crates/domain/src/page.rs` |
| F7 | 资金纪律：金额 int64 分 + decimal 中间计算；幂等表（启动/结束回执、冻结、支付回调、`card_operation.event_id`）原样保留；行锁顺序不重排 | 各域 repo + `CODING_STANDARDS.md` |
| F8 | 静态资源 rust-embed 进二进制（`assets-admin` / `assets-h5` 叶子 crate，SPA fallback）；前端产物由镜像构建阶段生成 | `crates/assets-*/` |
| F9 | 部署：单 compose 栈，应用容器暴露 8080/9100；22 个环境变量冻结清单，超出启动时 WARN | `docker/`、`docs/operations.md` |
| F10 | 枚举 decode 三方向：DB→Rust 严格失败（未知值即 schema 漂移报警）；设备/外部上报方向保留原值透传；API 入参方向解析失败 400 | `crates/domain/src/wordlist.rs` |

## 三、目标目录树（终态）

```text
backend/crates/
├── domain/                      # 纯规则 + 词表枚举 + 值对象（零框架：不出现 axum/sqlx/redis 类型）
│   └── src/
│       ├── charging/            # charging_{actual,command,cost,measured,order,scheme,settle,spec,state} 归入子树
│       ├── money/               # money.rs 按内容拆：支付/退款词表 + 复核校验与重放比对工具
│       ├── card/  payment/      # card.rs、payment_intent.rs 各归子目录
│       ├── marketing/  operations/  identity/  device/
│       ├── order/  pricing/  settlement/  wallet/  export/   # 各域文件同样归子目录
│       ├── limits.rs            # 全局界限值常量（逐条注释来源）
│       ├── page.rs              # 分页双轨校验（唯一入口）
│       └── wordlist.rs          # wordlist_enum! 宏：全域词表枚举的生成范式（sqlx 三件套手写，见第六章）
├── platform/                    # 无业务语义通用能力：config/error/httpx/ids/jwt/lifecycle/metrics/
│   └──                          #   password/redisx/sessiontoken/sweeperx/totp
├── storage/                     # 连接池 + 迁移器
├── protocol/  protocol-dc589/   # 设备契约与唯一实现（F5，结构冻结）
├── events/                      # lib / enqueue / repo / runtime：EventKind 枚举 + 事务内入队
├── gateway-runtime/             # 设备接入运行时：listener / registry / sink / snapshot /
│   └──                          #   telemetry / command_sink / device_events
├── modules/<域>/                # 12 个域 crate，内部模板见第五章
├── http-api/                    # 聚合 API 薄层，src/：
│   ├── lib.rs                   # 路由汇总 + 中间件 + 健康检查
│   ├── state.rs                 # AppState + 各域最小 Handles + FromRef 子状态
│   ├── errormap.rs              # 域错误 → Code 映射（唯一枢纽）
│   ├── guard.rs  user_guard.rs  # 后台守卫（AdminGuard/MFA/实时权限）与用户会话提取，各一文件
│   ├── params.rs                # 分页/时间/状态过滤参数提取器（双轨参数化）
│   └── views/<域>/              # 12 域 × {admin.rs, user.rs, mod.rs}：每域两个薄门面
├── assets-admin/  assets-h5/    # rust-embed 叶子 crate（F8）
└── app/                         # 组合根：Config 全量加载 → 装配 → 路由汇总 → 生命周期
    ├── main.rs                  #   serve / migrate / seed 子命令
    └── simulator_batch.rs       #   headless 批量模拟器（压测与现场排障）
```

## 四、crate 依赖规则（编译器强制）

```text
domain                → 无
platform / storage    → 无
protocol*             → protocol（实现 crate 只依赖契约）
events                → platform
modules/<域>          → domain + platform + events（后两者按需）+ 显式列出的其它 modules
                        （Cargo.toml 即依赖清单；跨域依赖须有业务理由注释）
gateway-runtime       → protocol* + events + domain + platform + storage + 其用到的 modules
http-api              → 其实际用到的 modules + platform
                        + gateway-runtime（仅 SnapshotStore 快照读取，登记例 #1）
app                   → 一切（组合根，唯一读 env 的地方经由 platform::Config）
```

禁止：`modules/*` 互赖只为拿一个 DTO（先问是否该进 domain）；http-api 出现业务 SQL；任何模块读环境变量。新增跨域依赖在 `Cargo.toml` 注释写明业务理由。

## 五、域 crate 内部模板

```text
modules/<域>/src/
├── lib.rs        # 门面：pub use 公共类型与构造函数；禁止 pub use repo 内部
├── error.rs      # DomainError（thiserror）；sqlx::Error 的两种处理形态语义固定
├── service/      # 业务用例（按子域一文件：refund.rs、risk.rs…）
└── repo/         # 表访问（SQL 只出现在这里；可见性 crate 内）
```

- **粒度指引**：域 crate src 总量 < 约 800 行可平铺 `service.rs + repo.rs`；超过则按子域分文件。不强制每层独立 crate——层是 crate 内的目录。
- **域职责注记（order，2026-11-15 评审定案）**：order 写侧为跨域编排者（payment 创建、charging 回执、settlement 退款）提供存储原语，生命周期推进的证据链（读状态→构证据→条件 UPDATE）封装在原语内部（`mark_charging_tx` 范式）；order 自身 service/ 只承载 feedback 用例——写侧不设编排层是有意形态，评审不应以「缺 service 层」为由重提。
- **SQL 归属是硬规则**：`sqlx::query`/`QueryBuilder` 只允许出现在 `repo/`（或平铺形态的 `repo.rs`）与 gateway-runtime 的运行时存储内。审计方法：`grep -rn "sqlx::query" crates/modules/<域>/src/service/` 必须为空。
- 迁移策略见第八章批次 3。

## 六、类型系统纪律

1. **词表枚举**：一切有限状态词表用 `wordlist_enum!` 宏定义。宏手写 sqlx 三件套的原因：sqlx 0.8.6 派生强枚举的 `type_info()` 与 MySQL 结果集报告的 `ColumnType::Enum` 永不相等，兼容判定须委托 `<str>::compatible`。新增词表 = 落在所属域文件 + 宏生成，禁止裸字符串比较。
2. **sqlx 类型映射**：FromRow / query_as / bind 类型逐一对照 `migrations/0001_init.sql` 列声明（unsigned→u8/u32/u64、有符号统一 i64）；集成测试解码路径必须有非 NULL 真实形态行。细节与事故实例见 [CODING_STANDARDS.md](../CODING_STANDARDS.md)——**编码前必读**。
3. **decode 三方向**：见 F10。
4. **wire 契约**：请求/响应字段显式命名；`rename` 语义对齐调用方；响应载荷字段变更必须同步核对前端调用点（门禁 7.3 的字段对账会抓必填缺口，但抓不住响应字段错位——响应字段靠集成流断言）。

## 七、测试与门禁

三层测试，各有明确职责，不互相替代：

| 层 | 内容 | 入口 |
|---|---|---|
| ① 纯层 | 领域规则单测、DC589 黄金样本帧（协议文档字节硬编码）、帧边界与坏帧 | `node tasks.mjs test` |
| ② 真库集成 | 资金链路、并发/幂等/行锁、状态机流转、解码路径非 NULL 行；`TEST_MYSQL_URL` 门控 | `node tasks.mjs test-integration`（需 dev 容器） |
| ③ 契约对账 | 路由快照 + 信封 + 错误码（`api_contract.rs`）；前后端调用点↔路由↔请求字段三段对账（`check-api-contract.py`） | `node tasks.mjs check` 内含 |

静态门禁：`node tasks.mjs check` = fmt-check + clippy（workspace lints 警告即错误 + unwrap/expect 禁令）+ test + api-contract。**任何提交前四项全绿**；涉及 SQL/枚举/锁/幂等的改动追加 ②。

验收新增检查项时先扩展脚本再写规则——门禁是代码，规则引用代码，不双写。

## 八、实施计划（现状 → 终态）

现状与终态的已知差距四项，按批次执行；每批次完成即提交，全程 `check` + `test-integration` 兜底，纯移动批次额外要求零行为变化。

| 批次 | 内容 | 验收 |
|---|---|---|
| **1. domain 归树** | 25 文件归子目录（charging 9 文件、money 拆分、card/payment/marketing/operations/identity/device/order/pricing/settlement/wallet/export 各归子树）；纯 `mod` 重排 | 全绿 + `git diff` 无逻辑变更 |
| **2. 域 error.rs 铺设** | 12 域各建 `error.rs`，集中现有错误类型（thiserror），域内 `pub use` 收口 | 全绿；http-api errormap 引用路径不变 |
| **3. SQL 抽 repo** | 按域分批把用例文件中的 `sqlx::query` 抽到 `repo/`：只读域先行（order/pricing/device/export）→ marketing/identity/ops → 资金域最后（card/charging/payment/settlement/wallet） | 每批 `grep` 审计（第五章）+ 真库集成全绿 |
| **4. 依赖与文档收口** | 确认依赖规则第四章逐 crate 落实；AGENTS.md 指向本文；登记表启用 | 全绿 + 本文第九章有首批评审记录 |

预估：批次 1 半天；批次 2 一天；批次 3 视域规模 3–5 天（资金域放最后并加密集成测试）；批次 4 半天。

### 实施记录（2026-11-15 全部完成）

| 批次 | 提交 | 结果 |
|---|---|---|
| 1 | `04002f0` | 25 文件归树，门面 re-export 保路径 |
| 2 | `ec799e2` | 12 域 31 个错误类型集中 error.rs |
| 3a/3b/3c | `b6cf8eb` `a72aa67` `e152c65` `3ddb3fc` `336c462` `06c0395` `c338fe0` `7368831` | 12 域全部落 service/repo 模板，493 处 SQL 收敛 repo/，service 层零 SQL |
| 4 | `ac618ca` | SQL 审计成为 `node tasks.mjs check` 常驻门禁（check-sql）；Cargo.toml 跨域依赖逐条补业务理由；反查删除 export→order 死依赖 |

实施期间修复的真库回归一个：`ops` 域 `mfa_secret_version` 按 schema `bigint unsigned` 由 `i64` 改 `u64`（违反类型纪律①，落域模板时被真库集成测试暴露）。每批次均以 `check` 全绿 + fresh 重建后 `test-integration` 77 套件零失败验收；纯移动批次核对锁序、幂等判定、条件更新 RowsAffected 语义逐字节保持。

## 九、偏离登记表

机制：实施与本文冲突时，**当场**在此追加一行（编号、原条款、决定、理由、日期），经确认后生效。登记 ≠ 追认：理由必须回答「为什么原条款不该执行」，回答不了的就去执行原条款。

| # | 原条款 | 决定 | 理由 | 日期 |
|---|---|---|---|---|
| 1 | （原计划）http-api 对 gateway-runtime 的依赖判冗余、应删除 | **保留**，收窄为「仅 SnapshotStore 快照读取」 | C 端充电页需要实时快照；http-api 已定为聚合薄层，聚合层读聚合所需的运行时状态符合 D1 语义。若将来快照查询需跨进程/多协议，再抽桥 | 2026-11-15 |
| 2 | （原计划）gateway-runtime 对 order/payment 等 modules 仅 dev-deps | **按第四章生产依赖执行** | 充电链路直调域服务面 + 事件层并存是已实施形态；dev-deps 设想的前提（状态推进全走事件层）未成立 | 2026-11-15 |
