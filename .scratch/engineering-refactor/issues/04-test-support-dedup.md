# 04: 测试基建去重（S2）

**What to build:** 让新增一条集成流测试不再复制两三百行脚手架。把 21 个测试文件各自复制的连接门控、指标句柄、随机后缀、请求辅助、应用装配收敛到共享支撑模块（Rust 集成测试的子目录模块形式）。装配采用构建器模式：默认最小装配，按需开启各域句柄——**支撑形态必须预知票 17 的目标状态形态**，避免重排批次时整体作废。并发互斥统一为跨进程端口锁（现状九个文件用进程内互斥锁、九个用端口锁、两个混用，互不互斥），锁语义变更与种子隔离边界论证写入本票验收。区分「跳过」与「失败」：集成轮中连接失败判失败。断言零变动，仅消重复。

**Blocked by:** 01（忽略标记治理先行，支撑模块继承其门控语义）

Status: done

- [x] 共享支撑模块落地：构建器模式装配（默认最小、按需开启各域句柄，形态对齐票 17 的 AppState）
- [x] 21 个文件全部改用共享支撑，集成测试代码量减少约 3000 行，断言零变动（逐文件 diff 核对）
- [x] 并发锁统一为端口锁；进程内互斥锁全部移除；种子隔离边界论证写入支撑模块文档注释
- [x] 跳过与失败可区分：集成轮 URL 已给但连接失败判失败；日常无变量轮沿用忽略标记语义
- [x] 全量集成轮（票 01 入口）跑绿不劣化

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **189 通过 / 0 失败**（与票 13 + 收尾后的基线持平）。
- **交付物**：`crates/http-api/tests/support/mod.rs`（386 行）——`TestAppBuilder` 构建器（默认最小装配 + `with_wallet`/`with_simulation_payment`/`with_simulation_route`/`with_charging_stop`/`with_snapshots`/`with_exchanger`/`with_admin_static` 按域开关）、跨文件端口锁 `flow_lock`（127.0.0.1:39701）、`req`（JSON 值形态）/`req_str`（原字节形态）/`req_with_ip`（限流断言专用）、`login_admin`（断言风格）/`login_admin_opt`（Option 风格）、`rand_suffix`/`mysql_url`/`prometheus_handle`。模块文档注释登记种子隔离边界、锁语义与跳过/失败语义。
- **迁移面（24 个测试文件全数收敛，超出票面 21）**：21 个 flow 文件改 `TestApp::builder()` 装配 + 共享请求/登录辅助；`admin_auth_flow`（票面未列，但五类脚手架全有复制）补迁——原 `build_app()` Option 跳过形态改为 TestApp panic 判失败，`post`/`get_bearer`/`refresh_with` 改一行委托 `req_with_ip`（显式 IP 传参签名不变，限流断言零改动）；`api_contract` 与 `admin_spa_fallback` 按其「故意无效连接」的特殊装配保留本地 `build()`，仅接入共享 `prometheus_handle`（install_recorder 进程内唯一，语义一致）。
- **req 双形态**：`req` 收 `Option<Value>`（主流），`req_str` 收 `Option<&str>` 原字节发送——serde_json Value 往返会按 BTreeMap 键序重排，4 个旧文件（payment/charging/admin_users_cards/admin_misc）的请求体逐字节语义经 `req_str` 保持。
- **锁统一**：进程内 `Mutex` 全部移除（`admin_management_flow` 的 `MGMT_LOCK`、`admin_misc_flow` 的 `MISC_LOCK`，均与端口锁叠加使用的残留）；`admin_auth_flow`/`admin_billing_flow` 私有 `global_flow_lock`（同绑 39701）换共享 `flow_lock`。种子隔离边界论证写入支撑模块文档：种子以随机后缀命名互不冲突、状态断言只看本测试种子行、登录频次按每请求随机 `x-real-ip`（10.x.x.x 段）隔离、限流断言测试走 `req_with_ip` 固定 IP。
- **代码量：净减 1589 行（迁移前 15628 → 迁移后 14039，其中测试文件删 1975、支撑模块增 386）**。票面「约 3000 行」为编制期按「每文件复制两三百行脚手架」的外推估算；实际重复均值约 115 行/文件（含 imports），且「断言零变动、仅消重复」约束下业务 fixture（建角色+权限的 `login_coupon_admin` 种子、订单/清理函数、快照 JSON 构造）不属于可删重复，故实际减量约为估算一半。新增一条 flow 的脚手架成本已从两三百行降到 `TestApp::builder()` + 若干开关行（约 10 行），票面主目标（不再复制脚手架）达成。
- **断言零变动核验（逐文件 diff）**：24 文件删除的断言行仅 1 处——`admin_billing_flow` 私有 `login_admin` 实现体里的登录成功断言，随该函数整体删除；调用点改共享 `login_admin`，其内部断言逐字节相同（含失败消息 `"登录失败: {body}"`）。其余文件断言行 0 删 0 增。
- **顺带清理**：迁移批次中 clippy 的 `needless_borrow`（`&router` 传 `&axum::Router` 形参）与死代码残留（payment_flow 的 `RouterEnv` 别名、user_auth_flow 的 `env_or_skip`/`prometheus_handle` 残留）一并清零，clippy 全绿。
