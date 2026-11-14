# ChargePilot 工程结构重构方案（二期）

状态：已评审修订（2026-11-14，五视角子代理评审后 v2）
前序：[rewrite-plan.md](rewrite-plan.md)（一期：Go 多服务 → Rust 单体，十阶段已完成）是本文的上游基准；一期「红线层」全部继续有效——DC589 帧协议、计费算法与金额语义、行锁/幂等、JWT 语义、HTTP 信封 `{code, message, data, request_id}` 与错误码数值、DB ENUM 字面量均**不变**。
本文范围：一期解决「换语言、塌进程」，二期解决「结构看不懂、类型不严、门禁缺失、测试不可信」四个工程问题。**不改任何对外行为与数据库语义。**

并行会话提示：本仓库另有修复会话在推进（近期提交集中在 `http-api` 参数修复、`user_settlement` 新增与订单终态收尾）。文中文件与行号是 2026-11-14 快照；任何批次实施前须以最新 `main` 重跑证据核对（第八节 S0，含批次前 `git log --oneline -20` 例行核对）。

---

## 一、现状诊断：为什么「看不出是 MVC 还是 DDD」

### Q1 组织结构是「三不像」

当前 `backend/crates/` 共 21 个 crate（约 5.2 万行 src + 1.44 万行集成测试），三种组织范式各占一角：

| 部分 | 形态 | 规模 | 问题 |
|---|---|---|---|
| `http-api` | **水平 MVC 的 V+C**：全部 admin/user HTTP 处理器集中一个 crate，按 `admin_xxx.rs` / `user_xxx.rs` 横切命名 | src 21 文件约 5 965 行（含修复会话新增的 `user_settlement.rs`）；tests 约 1.27 万行 | 文件名与业务域不对应：`admin_finance.rs`（992 行）横跨 wallet＋settlement＋ops 三域；`device_and_user.rs` 混设备与用户站点；`admin_pricing_device.rs` 混定价与设备；`admin_misc.rs` 横跨 order/ops/pricing/device 四域 |
| `modules/*`（12 个） | **垂直域 crate**，但内部无分层 | 约 22 800 行（仍在随修复增长） | service 与 SQL 混在同一文件（如 `wallet/refund.rs`、`settlement/refund_executor.rs` 内联全部 SQL）；单文件 200–967 行；`order` 只有 3 个文件，`device` 有 10 个，粒度无指引 |
| `domain` | **薄领域层**，14 文件平铺 | 约 5 100 行 | 只覆盖 charging 系（8 文件）/money/marketing/operations/card/payment_intent；identity/device/ops 等 8 个域没有规则层；文件按前缀分组（`charging_*.rs`）而不是子模块树 |

三个部分各说各话：看 `http-api` 是 MVC（一层 Controller 壳），看 `modules` 是垂直切片，看 `domain` 是贫血共享层。**评审者无从判断「一个改动该落在哪」**。

附属结构问题：

- **Q1a `deps.rs` 上帝对象**：`ApiDeps`（`http-api/src/deps.rs:8-24`）把全仓服务句柄装进一个 `Clone` 结构体，含 4 个 `Option<Arc<...>>` 字段（「字段存在即可，None 时尚未装配」——装配完整性不成立，测试里 `checkout: None` 也能跑）。装配逻辑（`WalletServices::build`、`PaymentServices::from_env`）又住在 http-api 里而不是组合根 `app`。
- **Q1b `storage` crate 只有约 126 行**（pool + migrator），独立成 crate 的收益只剩「名字好找」。
- **Q1c `app/main.rs` 约 281 行**：既有装配又有环境变量直读（`ADMIN_BOOTSTRAP_USER`、`EXPORT_DIR`、`ADMIN_STATIC_DIR`），组合根与配置加载混在一起；`app/src/simulator_batch.rs`（约 575 行 headless 压测子命令）也挂在 app 内，定位未登记。
- **Q1d 无用依赖**：`http-api/Cargo.toml` 生产依赖声明 `chargepilot-gateway-runtime`，src 中零引用。

### Q2 状态词表字符串化（该用枚举的地方全是字符串）

数据库单库 63 个 ENUM 列（真相源 `migrations/0001_init.sql`），Rust 侧却分三种退化写法：

1. **常量串容器**（unit struct + `&'static str` 常量）：
   - `modules/order/src/lib.rs:14-25`：`pub struct OrderStatus;` + 8 个 `pub const X: &'static str`；
   - `modules/settlement/src/billing.rs`：`pub struct JobStatus;` 同款；
   - `crates/events/src/lib.rs`：事件 `kind` 词表 7 个 `pub const CHARGE_ENDED: &str`（同文件 `EventSource` 却是真 enum——正反例同框）。
2. **String 新类型**（newtype over String，伪装成类型其实是字符串）：
   - `domain/src/money.rs`：`PaymentStatus(pub String)` 等 7 个。**注意其中 `PaymentStatus` 的 6 个常量横跨两张表的 ENUM 列**（`payment_order.status` 6 值 initiated/paid/closed/refunded/partial_refunded/failed 与 `charge_order.payment_status` 4 值 pending/paid/refunded/partial_refunded 的取值混装，`money.rs:29` 注释自己就要求区分两者）——枚举化必须先拆型。
3. **裸字面量**：`domain/src/charging_settle.rs` 里 `paid.mode == "duration"` / `"energy"` / `"amount"` 出现 8 处；`Settlement.executor` 用裸 `"server".to_string()`；`StopReason(pub String)` 挂 14 个常量串（`charging_settle.rs:15-33`，无 DB 独立列，持久化在三处 JSON）。

量化：全仓（不含测试）`== "…"` 字符串相等比较约 90 处（modules 73 + domain 14 + http-api 3）；`match x.as_str()` 21 处。**编译器对状态取值零约束**。

同仓已有正确范式可循：`domain/src/charging_state.rs`（类型层状态机骨架，迁移方法 + trybuild 反例）与 `events::EventSource`（serde enum）。但 `charging_state` 在业务代码中只有 2 处调用（`modules/order/src/lib.rs:307`、`modules/charging/src/sweeper.rs:41`），且均为 `let _refunding = …` 丢弃式令牌构造——字段全 pub、无构造约束，删除后行为不变，只证明「类型层可表达」（Q10）。

### Q3 魔法数散布

业务界限值以内联字面量重复出现（旧版多为内联、无具名常量）：

| 字面量 | 出现处（示例） | 含义 |
|---|---|---|
| `4320` | `charging_settle.rs:66,160`、`charging_spec.rs:423,428`、`charging_scheme.rs:151-232`（≥6 处） | 最大充电分钟数（旧版 offer.go/spec.go/device.go/capabilities.go/scheme.go 多处内联） |
| `(1000..=65000)` | `charging_settle.rs:164`（`charging_scheme.rs:251` 为 `1..=65` kWh 等价形式） | 电量套餐 Wh 区间 |
| `(12..=72)` | `ops/repo.rs:310`、`ops/management.rs:135,435` | 后台口令长度 |
| `(1..=100_000)` vs `(1..=1_000_000)` | `marketing/activity.rs:618`、`marketing/query.rs:155` vs `ops/management.rs:21`、`export/service.rs:309` | 分页上界——**经旧仓库核实是旧版有意双轨**：`DefaultPageLimit = 100_000`（C 端，旧 `httpapi/gin.go:104`）与 `MaxAdminPageLimit = 1_000_000`（后台，`admin/request.go:23` 注释明确分别配置）。不是漂移 |
| `(100..=5_000_000)` | `wallet/recharge.rs:90` | 充值金额区间（分）；注意退款下界是 1，两者不同 |
| `(1..=2000)` | `http-api/src/admin_orders.rs:385` | 游标分页 limit |
| 其他 | `charging_scheme.rs` 的 `60..=4320` 下界、`10800`、`500` 等 | 浮动秒数、功率上限等（收编时逐一对照旧版 `scheme.go:82` 同函数清单） |

状态过滤另存在**两种语义**：`admin_management.rs:85-87` 白名单外静默降级 None，`marketing/query.rs:158-162` 非法值返回 400——同为旧版既有行为，收编时保留差异。

### Q4 配置分裂

`platform/src/config.rs` 建立了 22 个变量的「冻结清单」并带未知变量告警，但清单外的读取路径绕过了它：

- `http-api/src/deps.rs:60-120`：`PAYMENT_MODE`、`LOGIN_MODE`、`WECHAT_APPID/SECRET/NOTIFY_URL` 直读；
- `modules/payment/src/wechat.rs:36-40`：商户 5 个凭据直读（**业务模块持有配置读取**）；
- `http-api/src/admin_export.rs:18-19`：`EXPORT_DIR/MAX_ROWS` 直读；
- `app/src/main.rs`：`ADMIN_BOOTSTRAP_*`、`EXPORT_DIR`、`ADMIN_STATIC_DIR` 直读。

后果：`Config` 只管 6 个核心变量，「哪些变量影响哪些行为」要全仓 grep。另外 `platform` 内 `ids.rs`/`redisx.rs`/`sessiontoken.rs` 测试模块直读 `TEST_*`、`app/simulator_batch.rs` 直读 `DATABASE_URL`（CLI 回退）——收编时这两类属合法豁免。

### Q5 错误体系不统一

- 各域自造错误且风格不一：`Storage(String)`（order、settlement×4、payment×3、wallet、marketing×3、export、charging×2）与 `Storage(anyhow::Error)`（identity、ops、card、feedback——承载非 sqlx 错误）两种形态；`OrderError` 的手写 `From<sqlx::Error>` 还承载 MySQL 23000 → `NumberConflict` 的语义分类（`order/src/lib.rs:72-82`），被 card/payment/charging 消费；
- 契约错误码一部分收编进 `platform::Code`（`error.rs:9-42`，带冻结数值测试），但映射枢纽有三处：`lib.rs:174-187` 的 `identity_error`（裸 `respond(401, 1001, …)`）、`admin_auth.rs:32-57` 的 `ops_error()`（被 ≥8 个文件引用）、`user_charging.rs:43-51` 的 StopError 直映；`crate::respond(` 全 crate 约 246 处调用、码值散落在各 handler 分支；
- **3001 是「一码多 HTTP 状态」**：Rust 侧配 502 也配 409（`user_payment.rs:135,188`），旧 Go 侧 502/503/400/409 四态并存——`Code::http_status()` 的一对一设计收编不了它；
- `anyhow` 在服务层冒泡（如 `refund_executor::run_once`）。

### Q6 工程门禁缺失

- `tasks.mjs` 的 `check` = `fmt-check` + `cargo test`。**没有 clippy**；无 `[workspace.lints]`、`clippy.toml`、`rustfmt.toml`、`deny.toml`；非测试代码 `.unwrap()` 实测约 137 处；
- 无 CI（仓库当前无远端）；
- `PAYMENT_MODE` 等取值校验靠运行时报错，无静态约束。

### Q7 测试「看似在跑，实际没跑」

21 个集成测试文件（约 1.34 万行，含 gateway-runtime 2 个）开头 `env_or_skip("TEST_MYSQL_URL")`，变量缺失时静默 `return`——**Cargo 报告为 ok（pass），不是 skipped**。`tasks.mjs check` 不注入任何变量，即日常 `check` 全绿时，资金/幂等/设备闭环主缝实际执行率为 0。量化（2026-11-14 审计快照）：

| 维度 | 总量 | 平时实际执行 | 占比 |
|---|---|---|---|
| 测试代码行 | ≈17 284 | ≈2 760 | 16% |
| 集成测试行 | ≈14 400 | ≈1 030 | **7%** |

这 38 个被跳过的测试只在带 `TEST_MYSQL_URL` 的手动验证中跑过一次（`docs/verification-report.md:18`）。内嵌测试中也有同款盲区：`platform/src/sessiontoken.rs:129-189` 的 Lua 轮换脚本回归测试（红线级测试）同样静默跳过。

### Q8 伪有效测试与重复（审计结论 + 复核）

- **E 类（no-op）**：`platform/src/config.rs:270-285` `defaults_applied`——前半 `let _ = Config::from_env();` 无断言；后半断言的「默认值」是自己传进去的输入，恒真。
- **D 类（常量考据）**：`config.rs:292-313` 两个冻结清单测试——断言 `KNOWN_ENV_VARS.len() == 22` 等，抓不住任何行为回归；唯一消费者 `warn_unknown_env_vars` 无行为测试。
- **F 类（同路径重复）**：`http-api/tests/settlement_admin_flow.rs:421-525` 场景 A 与 `admin_billing_flow.rs:690-1020` 完全同路径且后者是严格超集（逐断言核实：转人工 job 状态、提案-双审-重排队-16 分重算、幂等重放、工单联动、权限分支全部在超集内）——场景 B 的 `resolve_manual_amount` 全仓独有须保留。
- **断言缺失**：`admin_auth_flow.rs:205-206,221` 两处 `let _ = …` 丢弃应断言的响应（无 Authorization 的 refresh、轮换出的新令牌；注意「旧令牌 401」断言在 211-212 已存在）。
- **helper 重复**：21 个测试文件各自复制 `env_or_skip`/`prometheus_handle`/`rand_suffix`/`req`/`build()` 装配，合计约 4 885 行；锁形态分裂——9 文件端口锁（`TcpListener::bind("127.0.0.1:39701")`）、9 文件 `static FLOW_LOCK: Mutex<()>`、2 文件（device/wallet）两种都有，Mutex 组与端口锁组之间对共享 dev 库无互斥。
- **明确不删**：`platform/error.rs` 错误码冻结测试、`storage/migrator.rs` 84 表计数哨兵、`money.rs::status_sets` 与 `operations.rs::vocabularies`（断言谓词函数行为，且是 S3 枚举化的迁移锚点）、protocol-dc589 码值断言、全部黄金/差分/编译期测试、全部资金/幂等/并发主缝测试。内嵌测试 101 个逐一扫描无第二个可删 no-op。

可精简合计约 2 700–3 200 行（16–18%），其中「删测试」只有约 60 行——**本仓测试的问题不在多写，而在没在跑与装配重复**。

### Q9 静态资源与镜像（现状可用，但有版本错位隐患）

`backend/Dockerfile` 已是规范三段式（node 构建双前端 → cargo-chef 编译 → bookworm-slim 运行时），静态资源以 `ADMIN_STATIC_DIR`/`H5_STATIC_DIR` 磁盘挂载 + 自写 SPA fallback（`http-api/src/lib.rs:64-139`）服务。隐患：二进制与静态文件版本错位可能；`H5_STATIC_DIR` 在 `lib.rs:87` 绕过 Config 直读。前端双产物合计估算仅 3–5MB（对比 openobserve 嵌入的 50MB 量级小一个数量级），嵌入成本可控。

### Q10 类型层状态机有名无实

`domain/src/charging_state.rs` 骨架完整（每态 newtype + 6 个迁移方法 + trybuild 反例——注意反例夹具仅 1 个，只锁定一种非法迁移），但业务侧只有 2 处丢弃式构造。`charge_order` 状态 UPDATE 写点实为 **6 处**（`order/src/lib.rs:279,312,341`、`settlement/billing.rs:298`、`refund_executor.rs:156`、`charging/sweeper.rs:46`）；state-machine.md 迁移 3/6（余额路径同事务直达 refunded）尚无独立写点。**状态机文档、类型骨架、实际写点三者脱节。**

---

## 二、调研结论（GitHub 大型 Rust 项目，`gh api` 实证）

两轮调研共覆盖 12 个项目（全部实际读取 workspace 定义、目录树与关键源文件），评审代理对关键论断做了二次抽查（证据见 §2.3）。

### 2.1 代码组织（9 项目）

| 项目 | crate 数 | 组织模式 |
|---|---|---|
| lemmy（14.7k★） | 41 | 按域垂直拆到极致（db_views 每实体一 crate）；api crate 按资源分目录 |
| rauthy（1.4k★） | 14 | 唯一纯水平分层（api/service/data 三层 crate），但层内文件按资源一一对应 |
| meilisearch（60k★） | 22 | 胖主 crate（HTTP+编排 10 万行）+ 引擎核心 + 共享 types crate |
| surrealdb（33k★） | 52 | 超大内核 crate + 按技术关注点细拆外围 |
| vector（22.9k★） | 28+ | 反例警示：29 万行主 crate + 薄基础设施 lib/；横切 crate 膨胀是坏味道 |
| quickwit（11.8k★） | 34 | 按功能域一 crate；HTTP 集中在专用 serve crate **内部按 API 域分目录**（`search_api/`…），handler 函数体薄但文件可大（rest_handler.rs 1087 行） |
| iggy（5.3k★） | 35 | HTTP 是 server crate 内部模块；HTTP/TCP 复用独立 protocol DTO crate |
| synctv（2.5k★） | 22 | 核心一个大 crate 内部分 models/repository/service 三层目录；外围按接入协议垂直拆；**admin 与 user 是资源目录下两个薄门面文件**（admin.rs 2810 行也是「薄」——薄指函数体不看行数） |
| rustical（0.6k★） | 10 | 协议域垂直拆；存储 trait 与实现分 crate |

**归纳**：垂直域拆分是绝对主流（8/9）；「crate 边界的动机是依赖方向与复用，不是行数」；跨域聚合的 HTTP API 独立成薄 API **层**是 5/9 主流（其中独立成 **crate** 的为 4/9，meilisearch/iggy 是 crate 内模块）；没有任何项目按「admin 一棵树、user 一棵树」组织。本仓 21 crate 处于合理区间（对照 rauthy 9 万行 14 crate、lemmy 6.4 万行 41 crate），**问题不在 crate 数量，在 `http-api` 的横切组织与域内无分层**。

### 2.2 工程化（构建/门禁/测试）

- **Lint**：主流是 `[workspace.lints]` + `clippy.toml` 双轨。iggy 用一行 `rustc warnings = "deny"` 封底（且无 clippy 分节）；vector 用 `pedantic = warn` 渐进 + `clippy.toml` 的 `disallowed-methods` 落团队禁令 + `allow-unwrap-in-tests = true`；lemmy 的 30+ 项 deny 是「存量清零后」姿态不宜照抄。`await_holding_lock` 属 clippy **suspicious 组**——本方案 `suspicious = deny` 已自动覆盖 tokio 后台循环持锁跨 await 的检查。
- **CI**：Swatinem/rust-cache 事实标准；vector 经验是 **build 与 clippy 的 fingerprint 不同，缓存须分别预热**；surrealdb 为 clippy 单独开降级 profile（本仓规模小一个数量级，记为「拖慢再开」预案，不预引入）。
- **测试**：外部数据库一律「CI services 容器 + env URL」，六仓库 grep testcontainers 零命中——本仓 `test-integration` 模式方向正确，缺显式入口。**nextest 对本仓有硬前提**：iggy 官方注释明言「nextest 每测试独立进程，`#[serial]` 与进程内 Mutex 失效」——本仓 13 个测试文件用 `static FLOW_LOCK` 做资金/频次互斥，未改造前引入 nextest 等于给资金测试开互扰口子。
- **前端产物**：5/5 同构单体（quickwit、iggy、openobserve、warpgate、trailbase）选 **rust-embed 编译期嵌入**；openobserve 关键经验是**叶子 crate 隔离且依赖面小而稳定**（「依赖的任何东西都会把 50MB embed 再拖一遍 LLVM，config 曾是依赖导致每次改配置重付 embed 成本」——所以 assets crate 不依赖 platform，配置值由 app 传参）；iggy 用 `#[allow_missing = true]`；SPA fallback 均为 30 行手写 handler。
- **镜像**：quickwit/iggy 与本仓同为「node stage + rust stage 单镜像」；可补 lemmy 的 chef debug/release 双模式（与 CI 同批缓引入）与 `RUN ["binary","--version"]` 冒烟层。
- **任务编排**：本仓 `tasks.mjs` 等价于 xtask，保留扩展即可。

### 2.3 调研证据索引

lemmy：`Cargo.toml`、`crates/api/api/src/`、`crates/db_views/post/src/`；rauthy：`Cargo.toml`（`members=["src/*"]`）、`src/{api,service,data,api_types}/`；meilisearch：`crates/meilisearch/src/routes/`、`crates/xtask/`；vector：`Cargo.toml`（workspace.lints）、`clippy.toml`、`deny.toml`、`.github/workflows/warm-cache.yml`；quickwit：`quickwit-serve/src/{ui_handler.rs,search_api/rest_handler.rs}`、`quickwit-integration-tests/`、`.github/workflows/ci.yml`；iggy：`justfile`、`Cargo.toml`、`.config/nextest.toml`、`core/server/src/http/web.rs`；synctv：`synctv-api-http/src/http/{admin,room}.rs`、`synctv-core/src/`；rustical：`crates/{store,store_sqlite}/`；openobserve：`src/web/src/lib.rs`；surrealdb：`Cargo.toml`（clippy-release profile）。评审抽查另核对本仓本地 sqlx 0.8.6 源码（derive 行为）与旧 Go 仓库常量出处。

---

## 三、目标架构

### 3.1 一句话定位（回答「是 MVC 还是 DDD」）

**垂直域为主轴的混合式**：12 个业务域 crate 是第一公民（domain 词表＋域内 service/repo 分层），HTTP 是唯一的横切薄层（聚合 API crate 内部按域分目录），共享 crate 只剩五类（domain/platform/storage/protocol 系/装配）。每个改动有唯一落点判断规则：

> 先问业务域 → 落 `modules/<域>/`；再问是不是纯规则/词表 → 落 `domain`；只有 HTTP 信封、路由、鉴权提取留在 `http-api`。

这与 Go 旧版「业务实现按 use case 位于 internal/、HTTP 适配位于 internal/http/」的边界一脉相承，crate 化后由编译器强制。

### 3.2 目标目录树（改动部分加粗）

```text
backend/crates/
├── domain/                      # 纯规则 + 词表枚举 + 值对象（零框架）
│   └── src/
│       ├── charging/            # 现 charging_*.rs 8 文件归入子模块树
│       ├── money/               # 现 money.rs 拆分：状态枚举 + 金额工具（含 wallet 冻结状态等钱包侧词表）
│       ├── card/                # 现 card.rs（在线卡词表 + CanExtend 刷卡准入）
│       ├── payment/             # 现 payment_intent.rs（支付意图规则 + 业务单号格式）
│       ├── marketing/  operations/  identity/  device/   # 按 4.1 批次表逐步建立
│       └── limits.rs            # 全局界限值常量（逐条注释旧版出处，见 4.2）
├── platform/                    # 无业务语义通用能力（新增 PageRequest 双轨校验、from_env_with）
├── storage/                     # pool + migrator（存废见开放问题 6）
├── protocol/  protocol-dc589/   # 设备契约（调研确认结构正确，不动）
├── events/                      # 事件 crate：EventKind 常量串 → enum（见 4.1 S3h）
├── gateway-runtime/             # TCP 设备接入（结构不动；依赖 domain/platform 为生产依赖，order/payment/protocol-dc589/storage 为 dev-deps，无循环）
├── modules/<域>/                # 12 个域 crate，内部模板见 3.3
├── http-api/                    # 聚合 API 薄层，src/（21 文件）重排：
│   └── src/
│       ├── lib.rs               # 路由汇总 + 中间件 + 健康检查（现职责不变）
│       ├── state.rs             # AppState + 各域最小 Handles（含 db）+ FromRef 子状态（替代 deps.rs）
│       ├── error.rs             # 域错误 → Code 映射（收编三处枢纽；Code 增加状态覆盖能力见 4.4）
│       ├── guard.rs             # AdminGuard / 用户会话提取（横切；user_id_from_pub 等归此）
│       ├── extract.rs           # 分页/时间/状态过滤参数提取器（双轨参数化，见 4.2）
│       └── views/<域>/          # 每域一个目录：admin.rs + user.rs 两个薄门面（映射表见 5.2）
├── assets-admin/  assets-h5/    # 【决策 D2】rust-embed 叶子 crate（allow_missing=true + SPA fallback；不依赖 platform，配置由 app 传参）
└── app/                         # 组合根：Config 全量加载 → 装配各域 → 路由汇总 → 生命周期
    └── simulator_batch.rs       # headless 批量模拟器子命令（保留 app 内，定位登记于本文档）
```

### 3.3 域 crate 内部标准模板

```text
modules/<域>/src/
├── lib.rs        # 门面：pub use service 公共类型与构造函数；禁止 pub use repo 内部
├── error.rs      # DomainError（thiserror；sqlx::Error 处理见 4.4 的两种形态与语义分类保留）
├── service/      # 业务用例（按子域一文件：refund.rs、risk.rs…）
└── repo/         # 表访问（可见性 crate 内；SQL 只出现在这里）
```

粒度指引：域 crate src < 约 800 行可平铺 `service.rs` + `repo.rs`（如 order）；超过则按子域分文件。**不强制每层独立 crate**（调研结论：层是 crate 内的目录）。

依赖规则（编译器强制 + 文档约束）：

```text
domain → 无
platform/storage/protocol* → 无（或仅彼此明确列出的）
events → platform
modules/<域> → domain + platform + events（后两者按需）+ 显式列出的其它 modules（Cargo.toml 即清单）
http-api → 其实际用到的 modules + platform    # 唯一允许依赖业务域的横切 crate；现声明的 gateway-runtime 依赖为冗余，随 S5 删除
gateway-runtime → protocol* + events + domain + platform + 其用到的 modules
app → 一切（组合根，唯一读 env 的地方经由 platform::Config）
```

禁止：`modules/*` 互相依赖只为拿一个 DTO（先想是否该进 `domain`）；`http-api` 出现业务 SQL；模块读环境变量（见 4.3 的机器禁令）。现行跨域依赖（identity→wallet、card→ops/order/payment/pricing/wallet、settlement→payment 等）经评审逐条核对为有向无环，S6 依赖图审计时逐条写入 Cargo.toml 注释说明业务理由。

### 3.4 决策点（已经评审修订）

| # | 决策 | 方案与理由 | 备选 |
|---|---|---|---|
| **D1** | `http-api` 归宿 | **保留单一薄 API crate，内部按 `views/<域>/{admin,user}.rs` 重排**（评审确认正确：现状已是薄层——全 crate 业务 SQL 仅 health 探活与诊断计数 2 处非业务写路径；真正跨域文件仅 3 个且每端点可唯一归属；`admin_orders.rs` 全文件只调 order::admin_query 一个服务面，handler 层天然是聚合/信封职责） | 备选 A：handler 下沉各域 crate（12 个 crate 引入 axum 的代价真实、收益不存在）。留作远期演进 |
| **D2** | 静态资源嵌入 | **新建 `assets-admin`/`assets-h5` 两个 rust-embed 叶子 crate**（`allow_missing=true`；SPA fallback 随迁；**不依赖 platform**，SPA 所需值由 app 传参——openobserve 经验）。理由：5/5 同构项目选嵌入；本仓产物 3–5MB 远小于 openobserve 的 50MB，重编成本可控；消除版本错位。代价：部署形态变化（需 ADR 记录） | 维持磁盘挂载（仅收编 Config） |
| **D3** | `charging_state` 接线 | **修改策略后纳入，作为 S3b 之后的可选批 S3.5**：① 前置收紧证据 newtype 的构造可见性（字段私有、构造函数收敛到 repo——现状 `let _x = PaidOrder{..}` 丢弃式构造删除后行为不变，无编译期价值）；② 范围按实际 **6 个写点**（非 9）逐点真库测试。若不愿做构造收紧，则诚实降级为「仅文档化现状」 | 降级为文档化 |
| **D4** | 枚举 decode 未知值策略 | **DB→Rust 严格失败**（由 `#[derive(sqlx::Type)]` 的 decode 原生实现，未知值报错，评审已核对本地 sqlx 0.8.6 源码）；**设备/外部上报方向保留原值**（`StopReason::Other(String)`，serde 用 **untagged 或自定义实现**保持 JSON 逐字节透传——勿套 rename_all，否则 `{"other":"x"}` 破坏 meter_json 幂等往返）；**API 入参方向**解析失败返回 400（与现行为一致，现状就是 400/1005） | 宽松 Unknown 方案无采纳必要（项目未上线，无「先迁移代码后迁移 DDL」窗口） |

---

## 四、类型系统与配置改造

### 4.1 枚举化词表（核心批次）

统一范式（替代常量串容器与 String 新类型）：

```rust
/// 支付单状态（payment_order.status，6 值；与 charge_order.payment_status 是两个词表，严禁混装）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(rename_all = "snake_case")]     // sqlx 与 serde 是两套独立配置，各写一遍
#[serde(rename_all = "snake_case")]
pub enum PaymentOrderStatus { Initiated, Paid, Closed, Refunded, PartialRefunded, Failed }
```

关键修正（评审核实结论）：

1. **sqlx 绑定不需要手写 Encode/Decode 宏**——`#[derive(sqlx::Type)]` 对 unit-only enum 在 MySQL feature 下原生生成 `Type/Encode/Decode`（按 `&str` 编解码、未知值报错）。现状 `.bind(RefundStatus::PENDING)` 绑定的其实是 `&'static str` 关联常量，从无枚举绑定先例。S3a 打样只需验证 NULL 列（`Option<T>`）与 DEFAULT 行为；
2. **先拆跨列混装类型**：`PaymentStatus`（6 常量横跨两表）拆为 `PaymentOrderStatus`（6 值）与 `ChargePaymentStatus`（4 值）；`PaymentIntentStatus` 与 `PaymentOrderStatus` 的关系在对照表中显式裁决。**S3 前置设计物：产出「63 列 ENUM ↔ 现有 String 新类型/常量串 ↔ 域归属」三方对照表**（半天到一天设计工作，已计入工作量）；
3. wire 契约零变化：每枚举配「字面量对照测试」，同时锁 serde 与 sqlx 两套 rename_all 的一致性；
4. 设备上报类（`StopReason`）无 DB 列、持久化在三处 JSON，serde 用 untagged 保持透传，`Other(String)` 保留；`StopPlan.reason`（中文文案轴）是另一个东西，勿并入；
5. DB 词表 enum 与「API 入参白名单」是两层：入参侧（如 `admin_orders.rs` 的状态过滤集合、派生表达式产出的 `none/processing` 非枚举值）用判定集合建模，不强行共用 DB 枚举；
6. 枚举切片进 SQL 一律绑定参数（Go 旧版「单元素 IN 列表」教训）；
7. DB→Rust 未知值报错即 schema 漂移报警，配合 CI 抓「代码与迁移不同步」。

分域批次表（按唯一属主归属，S3a→S3h 顺序即依赖序）：

| 批 | 域 | 枚举（代表；完整清单以 S3 前置对照表为准） | 说明 |
|---|---|---|---|
| S3a | money | PaymentOrderStatus、ChargePaymentStatus（charge_order.payment_status 4 值）、RefundStatus、ExecutionPolicy、BillStatus、PayMethod、PaymentBizType、LedgerDirection(in/out)、LedgerBizType、Gender、WalletFreezeStatus(frozen/unfrozen) | 打样批：验证 `#[derive(sqlx::Type)]` 的 NULL/DEFAULT 行为 |
| S3b | order | OrderStatus(8 态)、ChargePaymentStatus 若未在 S3a 覆盖则在此、ChargeKind(start/extend) | 与 D3（S3.5）同写点，先枚举后接线 |
| S3c | settlement | MeterReviewStatus、InvoiceReviewStatus(两套)、RefundRecordStatus(pending/processing/success/failed/rejected) | |
| S3d | payment | PaymentIntentStatus(intent 列 5 值，与 PaymentOrderStatus 分家后唯一属主于此)、通道 biz_type | |
| S3e | device | DeviceStatus、FaultStatus、FaultType、ReportSource、AlertStatus、StationStatus | CardStatus(active/lost/disabled/unbound) 归 **card 域**（S3e 一并出，属主 card） |
| S3f | identity/ops | AccountStatus(两套)、AnnouncementStatus(draft/published/expired)、EventCategory | |
| S3g | marketing/export | CouponStatus、GrantSource、GrantStatus、DiscountType、ActivityTriggerType、ExportStatus | |
| S3h | events | EventKind 常量串 → enum（kind 列为 varchar 无 DB 约束，enum 仅进程内） | |

每批验收：该域 `match` 穷尽（编译器强制）；**「字面量比较点清单快照」批前后对比只减不增**（排除 `#[cfg(test)]` 模块与 serde 锚点——`grep "== *\""` 原始计数不可机器执行，验收对象是清单文件）。

### 4.2 魔法数收编

- `domain/src/limits.rs`：`MAX_CHARGE_MINUTES = 4320`（注释「旧版多处内联」）、`ENERGY_PACKAGE_WH: 1_000..=65_000`、`PASSWORD_LEN: 12..=72`、`RECHARGE_AMOUNT_CENTS: 100..=5_000_000`（与退款下界 1 分开两条）、`CURSOR_LIMIT: 1..=2000`、方案浮动 `60..=4320`/`10800`/`500` 等，逐条注释旧版出处；
- **分页双轨定论（评审已核旧版）**：C 端 `1..=100_000`、后台 `1..=1_000_000` 是旧版有意设计（`DefaultPageLimit` vs `MaxAdminPageLimit`），**维持双轨**。`platform` 新增 `PageRequest::with_bounds(page, page_size, 上界)` 参数化校验器，两个调用形态 `PageRequest::user()` / `PageRequest::admin()`，替换 4+ 处重复区间；状态过滤的两种语义（静默降级 vs 400）同为旧版既有行为，提取器按调用点声明语义，**不顺手统一**（红线：不改对外行为）。

### 4.3 配置集中（含机器禁令）

- `Config` 扩展为全量分组结构（`db`/`listen`/`auth`/`payment`（`PaymentMode` enum）/`wechat`/`static_assets`（D2 通过后消失）/`export`/`bootstrap`/`log`），`from_env` 一次加载，`app` 装配时分发；`from_env_with(getenv)` 注入式测试口；
- `KNOWN_ENV_VARS` 冻结清单测试改为**一致性测试**（断言「Config 读取的变量名集合 == KNOWN_ENV_VARS」）——**前提是 S4 全量 Config 落地**（现状 Config 只读 6 个变量时集合相等不成立），该测试与 `from_env_with` 重构是同一次改动；
- `clippy.toml` 加 `disallowed-methods`: `std::env::var`/`var_os` → 提示走 `platform::config`；**预写两类豁免**：测试模块的 `TEST_MYSQL_URL`/`TEST_REDIS_URL`（platform 内嵌测试）与 `app/simulator_batch.rs` 的 `DATABASE_URL` CLI 回退，否则 S4 验收第一天就红；
- `PAYMENT_MODE`/`LOGIN_MODE` 变 `PaymentMode`/`LoginMode` enum，非法取值在 `Config::from_env` 即失败（deps.rs 的 `anyhow::bail!` 上移），并补 fail-fast 断言测试。

### 4.4 错误体系

- 各域 `error.rs`：thiserror 化。**两种 Storage 形态分开处理**：`Storage(String)` → `#[from] sqlx::Error`（同步删除错误类型上的 `Clone/PartialEq/Eq` derive——评审核实无消费点，但必须列为改造项，否则编译失败）；`Storage(anyhow::Error)` 形态（identity/ops/card/feedback，承载非 sqlx 错误）保留 anyhow 或改 `#[source]`，不强行收编；
- **语义分类保留**：`OrderError` 的手写 `From<sqlx::Error>`（MySQL 23000 → `NumberConflict`，被 card/payment/charging 消费）不得被纯 `#[from]` 静默替换；
- `Code` 收编 3001 等：**`ApiError` 增加显式 HTTP 状态覆盖字段**（或映射函数按场景指定状态），解决「一码多状态」（3001 配 502/409/400/503 四态）；`respond()` 保留为信封底层；
- 映射收编范围分两步：**S5 只做三处枢纽收编**（`identity_error`、`ops_error()`、StopError 直映）+ `Code` 补齐缺口；**246 处 `respond(` 字面量调用的全量收编单列为 S6 内的子任务**（工作量差一个数量级，评审已定量）；
- 服务层 `anyhow` 收敛到后台循环等「只记日志」边界，请求路径不冒泡。

### 4.5 `ApiDeps` 消解

```rust
// http-api/src/state.rs（S5 落地）
pub struct AppState {
    pub db: MySqlPool,
    pub cfg: Arc<Config>,
    pub wallet: wallet::Handles,   // 最小 Handles 由 S5 一并定义，内含 db + 已有服务句柄
    pub payment: payment::Handles,
    pub charging: charging::Handles,
    // …每域一组，装配期全部就绪，无 Option
}
impl FromRef<AppState> for wallet::Handles { … }
```

**顺序裁决（评审指出 S5/S6 矛盾）**：现状 12 个模块没有任何 `Handles` 类型，handler 普遍直传 `&deps.db`。裁决：**S5 定义各域「最小 Handles」（含 `db` 字段），S6 做域内分层归位时不改 Handles 形状**，避免两批之间返工。`FromRef` 用手写 `impl`（无需 axum `macros` feature，当前 workspace 只有 `features=["json"]`；若用 `#[derive(FromRef)]` 则需补 feature，二选一在 S5 开工时定）。`simulation_payment: bool` → `PaymentMode` enum。`WalletServices::build`/`PaymentServices::from_env` 上移 `app`。

---

## 五、HTTP 层重排（D1 细化）

### 5.1 薄层职责清单

`lib.rs`（路由汇总/健康检查/静态回落）、`state.rs`、`error.rs`、`guard.rs`、`extract.rs` 五件套 + `views/<域>/`。处理器只做：鉴权提取 → 入参绑定（含枚举解析）→ 调域服务 → 信封输出。业务 SQL 继续禁止（现仅 health 探活与诊断计数两处非业务例外，保持）。**「薄」指 handler 函数体，不看文件行数**（quickwit 1087 行、synctv 2810 行的薄层文件先例）。

### 5.2 迁移映射表（现 21 个 src 文件 → views/<域>/）

| 现文件 | 拆分去向 |
|---|---|
| `admin_finance.rs`（992 行，跨 wallet+settlement+ops） | `views/wallet/admin.rs`（风控 3 端点）＋ `views/settlement/admin.rs`（发票/计量核实/人工退款 9 端点）；ops 侧引用经 `ops_error()` 收编后归 `views/ops/` |
| `admin_orders.rs` | **整体** `views/order/admin.rs`（process/timeline/payment-orders 均直查 `order::admin_query`，无代理层，拆去 charging 会制造人为跨域 import） |
| `admin_pricing_device.rs` | `views/pricing/admin.rs` ＋ `views/device/admin.rs` |
| `admin_users_cards.rs` | `views/identity/admin.rs` ＋ `views/card/admin.rs` |
| `admin_misc.rs`（跨四域） | `views/order/admin.rs`（feedback）、`views/ops/admin.rs`（audit-logs/MFA）、`views/pricing/admin.rs`（charging-schemes/preview）、`views/device/admin.rs`（device-capabilities） |
| `admin_marketing.rs` / `admin_management.rs` / `admin_export.rs` / `admin_diagnostics.rs`（归 ops）/ `admin_auth.rs` | 对应 `views/<域>/admin.rs` |
| `device_and_user.rs` | `views/device/{admin,user}.rs`；其中 **`user_id_from_pub` 等用户会话提取函数归 `guard.rs`**（否则 views/wallet/user.rs 等门面将跨目录引用，违反验收条款） |
| `user_wallet.rs` / `user_charging.rs` / `user_auth.rs` / `user_marketing.rs` / `user_ops.rs` | 对应 `views/<域>/user.rs` |
| `user_payment.rs` | `views/payment/user.rs`（扫码/下单/模拟确认；**注意：微信支付回调 HTTP 端点当前不存在**——验签解密能力在 `modules/payment/src/wechat.rs:181` 但无路由接线，`/api/v1/public/*` 前缀未出现。S0 必须核实接线状态并登记为独立红线项：若后续会话补接，该端点落 `views/payment/public.rs` 且注册顺序单独核验） |
| `user_settlement.rs`（修复会话新增，243 行） | `views/settlement/user.rs` |
| `deps.rs` | 拆解：`state.rs` ＋ 装配逻辑上移 `app` |
| `admin_guard.rs` | `guard.rs` |

验收交付物：**逐端点归属表**（每条路由 → views 文件 → 域，一次生成作为契约基线）；`views/<域>/` 只 import 本域 + platform/httpx + 本 crate 横切件（guard/error/state/extract）；路由快照不变；S5 按修复冷热区排序搬迁（`admin_pricing_device`/`device_and_user`/`admin_auth`/settlement 系为热区排最后）。

对 tests 主缝的冲击（评审补充）：16 个测试文件手工构造 `Arc::new(ApiDeps{…})`，全部随 S2 的 support `build()` 收敛；`admin_orders_flow.rs:44-49` 存在绕过 build_router 直联 merge 的装配（注释已过时），路由快照须覆盖直联路径或将其改回 build_router。**S2 设计 support 时必须预知 S5 的 AppState 形态（builder 模式），否则 S2 产出在 S5 作废。**

---

## 六、工程门禁与构建产物

### 6.1 Lint / 格式化

- `[workspace.lints.rust] warnings = "deny"`；
- `[workspace.lints.clippy]`：`correctness/suspicious/perf = deny`（`await_holding_lock` 已被 suspicious 组覆盖，勿单列），`all = warn`，`unwrap_used/expect_used = deny` + `clippy.toml` 的 `allow-unwrap-in-tests = true`（vector 先例，一行落地测试豁免）；
- **`wildcard_enum_match_arm = deny`**（restriction 组需单列；护住 S3「编译器强制穷尽」的收益，防 `_ =>` 静默绕过）；
- `clippy.toml`：`disallowed-methods`（`std::env::var`，含 4.3 的两类豁免说明）；
- `rustfmt.toml`：`max_width = 100`，nightly 项注释备用；
- 存量清零：非测试代码约 137 处 `unwrap()` 逐处判断改写或 `#[expect]`，**「存量豁免清单（逐条理由）」列为 S1 显式产出**；pedantic 不启用（记为远期）；`protocol-dc589` 与金额模块可模块级启用 cast 系 lint（记远期）。

### 6.2 任务与 CI

- `tasks.mjs` 扩展：`clippy`；`check` = fmt-check + clippy + test；`test-integration`（注入 `TEST_MYSQL_URL`/`TEST_REDIS_URL`，要求 `db-up`/`redis-up` 先行；**提供每轮库重建或按前缀清理选项**——dev 库残留会累积并干扰频次窗口类断言）；
- 全量集成单轮粗估 5–15 分钟，S3×8 + S4..S8 约 13 轮 ≈ 2–3 小时纯验证，可接受；
- **nextest：现阶段不引入**（硬前提：13 个测试文件的进程内 `FLOW_LOCK` 在 nextest 独立进程模型下全部失效；须先改造为 test-groups 或统一端口锁。S2 把锁统一为端口锁后再评估）；
- **CI：缓引入**（仓库当前无远端）；建远端后：job1 fmt+clippy+test（rust-cache，build 与 clippy 分别预热），job2 test-integration（`services:` 容器 + env URL，不用 testcontainers）；deny.toml + cargo-deny 随 CI 缓引入；
- surrealdb 式 clippy 降级 profile：不预引入（本仓规模小一个数量级），拖慢再开。

### 6.3 契约测试层（新增，护住后续所有批次）

- **路由清单快照**：axum 0.8 无路由枚举 API（评审核对 `Router` 公共方法仅 `has_routes()` 等布尔判断）。落地方式：**手写 `ROUTES: &[(method, path)]` 清单常量 + 探测式断言**（对每条发 oneshot 请求：已知 path 错 method → 405，未知 path → 404 信封——fallback 是 `not_found` 信封函数，区分度成立；契约测试传 `admin_static_dir: None` 无 SPA 干扰）。清单与 18 个 `router()` 函数人工对齐一次，S5 重排导致不符即测试失败；
- **信封序列化对照**：代表 DTO 清单从 5.2 映射表按域生成（每 views/<域> 的 admin.rs/user.rs 各 ≥1 个），字段基线提炼自现有集成测试已逐字段断言的集合，清单注释钉在 contract 测试头部；
- **枚举字面量对照**（见 4.1）；
- **与 `scripts/ops/check-api-contract.py` 分工**：该脚本（修复会话活跃维护中）管「前端调用 ↔ 后端路由」对账，纳入 S1 门禁本地跑；本节快照管「路由集合与信封形状不变」，双机制不重叠。
- 三者放 `http-api/tests/contract/`，纯 serde/oneshot 无外部 IO，日常 `check` 真跑。

### 6.4 镜像与静态资源（D2）

- `Dockerfile` 保留 cargo-chef 三段式，补 `RUN ["chargepilot","--version"]` 冒烟层（`#[command(version)]` 需补）；lemmy 式 debug/release 双模式与 CI 同批缓引入；
- D2 通过后：前端产物 `COPY --from=frontend` 进 Rust 构建上下文，由 `assets-*` 叶子 crate 嵌入（**依赖清单最小化、不依赖 platform**）；运行时镜像不再携带静态目录；实施时写 ADR 记录部署形态变更。

---

## 七、测试治理（按审计结论 + 评审复核执行）

### 7.1 运行门槛（最高优先，S0 即做）

- `tasks.mjs test-integration` 显式入口（对齐「编排供容器、测试认 URL」主流）；
- **跳过可见性用 `#[ignore]` 静态标记全部真库测试 + `cargo test -- --include-ignored` 跑全量**（评审纠正：`eprintln!` 跳过计数在 cargo test 默认输出下被吞——libtest 捕获线程输出、测试 ok 时丢弃，且 21 个文件是独立进程无法聚合；`#[ignore]` 让汇总行恒显 `N passed; M ignored`，一眼看出没跑）；
- `env_or_skip` 移入共享 support 后，**test-integration 轮中 URL 已给但连接失败应判失败**（区分「跳过」与「失败」，现状两者都静默绿）；
- 内嵌门控测试一并治理：`platform/src/sessiontoken.rs:129-189` 的 Lua 轮换脚本回归测试（红线级）改 `#[ignore]` 纳入 ignored 治理（其余 3 个 env 门控内嵌测试中，redisx ping 删除、ids/sessiontoken 并入上条）；
- `docs/operations.md` 补记两层检查的覆盖边界。

### 7.2 删除/裁剪/加固清单（每条经二次独立复核，实施时以最新代码再复核）

| 动作 | 对象 | 依据与前提 |
|---|---|---|
| 删除 | `platform/src/config.rs` `defaults_applied`（no-op+恒真） | 审计 + 复核一致；字段映射已被 `valid_config_assembles` 覆盖 |
| 删除 | `config.rs` `frozen_list_covers_core_vars`、`system_prefixes_do_not_shadow_known_vars` | 考据型；**前提：S4 全量 Config 落地后以一致性测试 + `warn_unknown_env_vars` 行为测试替代**（缺前提不删） |
| 删除 | `platform/src/redisx.rs` `ping_real_redis` | 13 个真库流把真实 redis_url 装进 ApiDeps，覆盖强于一次 PING |
| 裁剪 | `settlement_admin_flow.rs:421-525` 场景 A（保留场景 B 与 `resolve_manual_amount`） | 与 `admin_billing_flow.rs:690-1020` 同路径且后者严格超集（逐断言复核成立）；裁剪后补 7.2 末条 billing 判定单测 |
| 合并 | `charging_state.rs` 两个运行期测试 → 合并为 `all_transitions_are_expressible` 并**补上从未被调用的迁移 6 `restore_completed`** | 评审纠正：trybuild 反例仅 1 个，「拓扑已锁定」不成立，正例锚点应保留补全而非注释化 |
| 加固 | `config.rs`：`from_env_with(getenv)` 注入式默认值断言 + 与一致性测试同 PR | 替代被删 E/D 类 |
| 加固 | `admin_auth_flow.rs:205-206`（无 Authorization 的 refresh 响应补断言）、`:221`（轮换出的新令牌补消费验证） | 评审纠正：「旧令牌 401」在 211-212 已存在，勿重复补 |
| 加固 | `warn_unknown_env_vars`：收集逻辑提为纯函数（返回 Vec）后直测四条语义 | 接住被删 freeze 测试的守护语义 |
| 补测 | `settlement/billing.rs` 分段守恒判定提为纯函数直测（正常/缺失/不一致/倒序四类输入） | 该资金关键判定现内嵌在 300 行 `process_job` 且 452 行零内嵌测试，只靠 0 执行率的真库流 |
| 补测 | PAYMENT_MODE/LOGIN_MODE 非法取值 fail-fast 断言（S4 落地后） | 现 `anyhow::bail!` 运行时兜底无测试 |
| 去重 | 21 文件 helper → `tests/support/mod.rs`（builder 模式：`TestApp::new(db,redis)` 默认全 None，`.with_wallet()/.with_checkout()/.with_charging_stop()` 按需开启） | 复核确认依赖差异只在运行时装配、无需 testutil feature；**锁统一为端口锁**（9 文件 Mutex 与端口锁并存、互不互斥）并论证种子隔离边界，锁语义变更写入 S2 验收；support 设计预知 S5 AppState 形态 |

红线：资金/并发/幂等/协议黄金样本一律不动；删除合计约 60 行 + 裁剪 115 行；替代覆盖不存在即撤回该条。

---

## 八、实施批次

顺序原则：先机器可验的低风险（门禁）→ 类型系统 → 结构重排 → 部署形态 → 测试收尾。每批一个 PR；**回滚单位为「级联组」**——S3a（money）被 16 个非测试文件跨 5 个 crate 引用，单独 revert 必然级联，PR 描述须登记「本批被哪些后续批依赖」，回滚按逆序级联组执行。每批验收 = `node tasks.mjs check` + `test-integration` 全绿 + 契约快照不变；**每批开工前例行 `git log --oneline -20` + `git status` 与本批文件清单对照冲突面**（修复会话的万字级提交 f0ca2d8 说明 modules 与 http-api 都已是热点）。

| 批 | 内容 | 验收要点 | 估时 |
|---|---|---|---|
| **S0** 基线 | 合流后重跑诊断证据；建 `test-integration` 入口并跑绿一轮；`#[ignore]` 静态标记落地；生成契约快照（手写路由清单 + 信封 DTO 清单）；**核实微信支付回调端点接线状态并登记红线对象**；`check-api-contract.py` 纳入基线门禁 | 全量测试（含 38 个真库流）通过基线记录 | 1 天 |
| **S1** 门禁 | workspace.lints + clippy 清零（含 137 处 unwrap 逐处处理）+ clippy.toml/rustfmt.toml + tasks.mjs 扩展 + `check-api-contract.py` 接入 | `check` 三件套全绿；产出 unwrap/env 两份存量豁免清单 | 1–1.5 天 |
| **S2** 测试基建 | tests/support/ 去重（builder 模式 + 端口锁统一 + 连接失败判失败） | 测试行数 −约 3 000；断言零变动、锁语义变更单独论证；support 形态对齐 S5 AppState | 0.5–1 天 |
| **S3** 枚举化 | **前置：63 列三方对照表（拆 PaymentStatus 跨表混装）**；S3a→S3h 八子批；`#[derive(sqlx::Type)]` 范式；每批含字面量对照测试 | 字面量比较点清单只减不增；契约快照不变；S3a 打样验证 NULL/DEFAULT | 4–5 天 |
| **S3.5**（可选，D3） | charging_state 接线：构造可见性收紧 + 6 写点逐点接线 | 逐写点真库测试；丢弃式构造清零 | 1 天 |
| **S4** 常量与配置 | limits.rs + PageRequest 双轨 + Config 全量化 + env 直读清除 + `disallowed-methods` 启用（含豁免）+ 模式 enum fail-fast 补测 | `env::var` 全仓仅 platform::config + 两类登记豁免；分页双轨结论入档 | 0.5–1 天 |
| **S5** HTTP 重排 | 5.2 映射表执行（21 文件）+ state.rs（最小 Handles 含 db）/guard.rs（收编 user_id_from_pub）/extract.rs（双轨）+ **三处错误映射枢纽收编 + Code 补缺口**（246 处全量收编不在此批） | 逐端点归属表交付；路由快照不变；views 只引本域；16 个测试装配点经 support 收敛 | 2–3 天 |
| **S6** 域内模板 | 各域 service/repo 分层归位 + lib.rs 门面 + **错误类型 thiserror 化（含删 Clone/PartialEq derive、246 处 respond 全量收编）** + 依赖图审计入册 | 域 crate 零 env/零 axum；门面无 repo 泄漏；错误语义分类（NumberConflict）保留验证 | 3–5 天 |
| **S7** 静态资源 | D2 通过则 assets 叶子 crate + Dockerfile 冒烟层 + ADR；否则仅收编 Config | 镜像冒烟 + SPA fallback 回归（admin_spa_fallback 测试保留） | 0.5 天 |
| **S8** 收尾 | 7.2 删除/裁剪/加固/补测清单销项（其中 frozen_list 删除依赖 S4 已完成） | 审计清单逐条销项 | 0.5 天 |

总量约 **15–19 天**（v1 估算 8.5–11.5 天经评审修正：S3 前置对照表、S5 错误码范围定量、S6 体量与类型改造、S1 存量清零为四大隐性工作量）。

---

## 九、风险与对策

1. **与修复会话冲突**：`http-api` 与 `modules/settlement` 都是修复热区（当前工作区就有未提交的 `user_settlement.rs`）。S0 合流是硬前置；S3/S5/S6 每批开工前例行核对冲突面；S5 按冷热区排序搬迁。
2. **枚举化触碰 wire 契约**：三重护栏——字面量对照测试 + 路由快照 + 真库流测试（`test-integration` 每批必跑）。S3a 先打样验证 sqlx derive 的 NULL/DEFAULT 行为再铺开。
3. **S3a 词表设计错误风险**：前置三方对照表把跨列混装类型（PaymentStatus）显式拆型后才动代码；对照表本身作为 S3 交付物入 docs。
4. **微信回调端点缺口**：红线「回调路径逐字符冻结」当前无核验对象。S0 核实登记；若修复会话补接端点，S5 之前完成接线核验。
5. **clippy 清零引入行为改动**：S1 只允许 `#[allow]`/`#[expect]` 标注或纯等价改写；顺手重构推迟到对应批次。
6. **测试删除风险**：所有删除项引用审计 + 二次复核证据；「替代覆盖不存在即撤回」写进批次验收。
7. **D2 部署形态变更**：写 ADR；镜像冒烟层兜底。

## 十、开放问题（余项）

1. ~~D1~~ 已评审确认：保留薄 API crate。
2. ~~D2~~ 方案确认嵌入，待实施时 ADR 记录部署形态变更。
3. ~~D3~~ 已修订：修改后纳入 S3.5（构造收紧 + 6 写点），可选批。
4. ~~D4~~ 已确认：严格失败 + untagged 透传 + 入参 400。
5. **CI 时机**：仓库当前无远端。S1 的本地门禁（clippy 进 check）先行；建远端后按 6.2 启用。
6. **`storage` 存废**（126 行独立 crate 是否并入 app）：S6 依赖图审计时一并裁决，倾向并入。
7. **`simulator_batch.rs` 定位**：暂保留 app 内（headless 压测子命令），S3.5/S6 不动；若后续模拟器能力扩张再议独立部署单元。

## 附录 A：评审记录（v2 修订依据）

五视角子代理评审（2026-11-14，与本文同日）：架构、Rust 类型系统、迁移风险、测试治理、调研对照。全部 P0/P1 已合入本文；要点追溯：

- 架构评审：P1-1 映射表基数（21 文件口径）与 `user_settlement.rs` 缺失 → 5.2 补全；P1-2 domain 树缺 card/payment 落点 → 3.2 补全；P1-3 Handles 顺序矛盾 → 4.5 裁决；P1-4 分页双轨冲突 → 4.2 定论；P1-5 tests 主缝冲击 → 5.2/S2 联动；P1-6 `user_id_from_pub` → guard.rs。
- 类型系统评审：P1-1 `#[derive(sqlx::Type)]` 原生支持（删手写宏计划）→ 4.1；P1-2 PaymentStatus 跨表混装 → 4.1 前置对照表；P1-3 3001 一码多态 → 4.4 状态覆盖字段；P1-4 分页双轨旧版有意 → 4.2；P1-5 OrderError 语义分类与 derive 冲突 → 4.4；P1-6 StopReason untagged → D4/4.1。
- 迁移风险评审：P0-1 词表自相矛盾（与类型系统 P1-2 同源）→ S3 前置对照表；P0-2 微信回调端点不存在 → S0 核实项；P1-1 revert 级联 → 第八节回滚单位；P1-2 错误码 246 处定量 → 4.4/S5/S6 分工；P1-3/4 工作量修正 → 批次表；P1-5 修复会话协调机制 → 第八节例行核对。
- 测试治理评审：P1-1 eprintln 不可见 → 7.1 改 `#[ignore]`；P1-2 sessiontoken 内嵌盲区 → 7.1；P1-3 路由枚举无 API → 6.3 改手写清单 + 探测断言；P2-1 charging_state 合并补迁移 6；P2-2 锁形态分裂 → S2 统一端口锁；P2-3 frozen_list 删除的 S4 前提；P2-4 admin_auth_flow 加固内容修正；P2-5 补测三项 → 7.2。
- 调研对照评审：P1 nextest × 13 文件全局锁 → 6.2 不引入判定；P2 wildcard_enum_match_arm、allow-unwrap-in-tests、await_holding_lock 已覆盖说明、assets 叶子不依赖 platform、薄层不看行数、5/9 层与 crate 口径区分。

## 附录 B：证据速查

- 组织结构：`backend/Cargo.toml`（21 成员）；`http-api/src/lib.rs`、`deps.rs`；`modules/order/src/lib.rs:14-25`；`domain/src/money.rs:29-39`；`app/src/main.rs`
- 字符串词表：`grep -rn '== *"' crates --include=*.rs`（modules 73 / domain 14 / http-api 3）；`match .*as_str()`（21 处）；`domain/src/charging_settle.rs:157-176`
- 魔法数与双轨：`domain/src/charging_settle.rs:66,160-165`；`ops/management.rs:21-24`；`marketing/query.rs:155`；旧版 `internal/platform/httpapi/gin.go:104-105`、`internal/http/admin/request.go:23`
- 配置：`platform/src/config.rs:81-109`（冻结清单）；`modules/payment/src/wechat.rs:36-40`；`http-api/src/deps.rs:60-120`
- 错误：`platform/src/error.rs:44-65`；`http-api/src/admin_auth.rs:32-57`（ops_error）；`user_payment.rs:135,188`（3001 多态）；`order/src/lib.rs:72-82`（NumberConflict 语义）
- 测试：`http-api/tests/*`（19 门控文件 + gateway-runtime 2）；`platform/src/config.rs:270-313`；`settlement_admin_flow.rs:421-595`；`sessiontoken.rs:129-189`
- 状态机：`domain/src/charging_state.rs`；6 写点 `order/src/lib.rs:279,312,341`、`settlement/billing.rs:298`、`refund_executor.rs:156`、`charging/sweeper.rs:46`
- ENUM 真相源：`migrations/0001_init.sql`（63 列；`payment_order.status` 283 行 6 值、`charge_order.payment_status` 206 行 4 值）

## 附录 C：S0 证据复核记录（票 01，2026-11-14，commit 007d2d8）

按票 01 要求在修复会话合流（`007d2d8`，+17 用户端端点）后重跑诊断证据，偏差如下（快照基线为本文初版 `b4a732f`）：

| 项 | 快照 | 实测 | 偏差与处置 |
|---|---|---|---|
| 字符串相等比较（modules / domain） | 73 / 14 | 73 / 14 | 一致 |
| 字符串相等比较（http-api） | 3 | 19 | +16：新端点文件延续既有写法，随 S3/S5 批次一并处理 |
| `match …as_str()` | 21 | 13 | −8：修复提交消化了部分分支；继续按批次清零 |
| 非测试 unwrap | 约 137 | 137 | 一致 |
| 模块/HTTP 层 env 直读 | 11 | 14 | +3：新增端点文件随 S4 清零 |
| 门控测试文件 | 21 + 2 内嵌 | **25 + 2 内嵌**（+redisx 待删项） | +4：`device_chains`、`user_charging_query`、`user_identity_cards`、`user_settlement` 为修复会话新增；已随票 01 一并纳入忽略治理 |
| 微信回调端点 | 未接线 | 未接线（81 admin + 41 user 路由，无 public 前缀） | 与快照一致；登记见规格 Comments |

结论：偏差全部来自修复会话的既有写法延续，不改变方案任何批次的方向与估时；票 06 的打样输入（money 词表、跨表混装）无变化。
