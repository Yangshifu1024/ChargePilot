# 16: 配置全量化与机器禁令（S4 后半）

**What to build:** 全部环境变量收编进统一配置结构。配置结构扩展为全量分组（数据库/监听/认证/支付/微信凭据/静态资源/导出/引导账号/日志），支付模式与登录模式转为枚举型配置、非法取值启动即失败并补注入式断言测试；配置加载提供可注入的环境读取口（供测试断言默认值与校验路径）；「冻结环境变量清单」测试从常量考据改为一致性测试（配置结构实际读取的变量名集合 == 冻结清单）。随后启用 clippy 的环境变量直读禁令（disallowed-methods），预登记两类豁免：测试模块的连接变量、压测子命令的数据库地址回退。业务模块与 HTTP 层的环境变量直读清零，装配逻辑上移组合根。

**Blocked by:** 03（clippy 配置基座）

Status: done

- [x] 配置结构全量分组落地，支付/登录模式枚举化 + 非法取值启动失败补测
- [x] 可注入环境读取口落地：默认值断言测试 + 一致性测试（清单 == 实际读取集合）替代两个考据型测试
- [x] 业务模块与 HTTP 层环境变量直读清零（微信凭据组、支付/登录模式、导出目录、引导账号、静态目录全部经配置结构）
- [x] disallowed-methods 禁令启用，两类登记豁免生效，全仓仅统一配置处直读环境变量
- [x] 日常 check 三件套 + 全量集成轮跑绿

## Comments

- **2026-11-15 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **194 通过 / 0 失败**。
- **配置结构**（`platform::config::Config` 全量分组）：database_url/redis_url + 监听组（`ListenConfig{http_addr,device_addr}`）+ 认证组（`AuthConfig{jwt_secret,paid_start_timeout_secs}`）+ `payment_mode`/`login_mode` 枚举 + 微信凭据组（`WechatCreds` 七字段）+ 静态资源组（`StaticAssetsConfig{admin_dir,h5_dir}` Option）+ 导出组（`ExportConfig{dir,max_rows}`）+ 引导账号组。非核心字段缺省即空/None，与旧版 `unwrap_or_default` 语义一致（含非空值不 trim 的旧行为，测试锁定）。
- **模式枚举化**：`PaymentMode{Simulation,WechatDirect,Disabled}` / `LoginMode{Development,Wechat}`，`parse` 非法取值返回错误（启动 fail-fast）；三个枚举测试（合法取值、非法取值报错文案、默认值）注入 FakeEnv 断言。
- **可注入读取口**：`EnvSource` trait（`read`/`all`）+ 生产实现 `SystemEnv`；`Config::load(&impl EnvSource)` 为注入点、`from_env()` 委托。测试 `FakeEnv`（BTreeMap + RefCell 查询记录）驱动：默认值断言、校验路径、**一致性测试**——`Config::load` 实际读取的变量名集合 == 冻结清单 − RUST_LOG（日志层 EnvFilter 直读，清单内例外），替代原 `frozen_list_covers_core_vars` 考据测试（`system_prefixes_do_not_shadow_known_vars` 有语义保留）。
- **清单修正（漂移收编）**：`WECHAT_APIV3_KEY` 原实现直读（wechat.rs from_env）但冻结清单漏登记（每次启动打 WARN 未知变量）；本票收编进微信组与清单（22→23 项），一致性测试自动锁定。
- **直读清零明细（13 处 → 全部经 Config）**：http-api/deps.rs 6 处（PAYMENT_MODE/LOGIN_MODE/WECHAT_APPID/SECRET/NOTIFY_URL → `PaymentServices::assemble(mode, &wechat)` 与 `exchanger_of(mode, &wechat)`）；payment/wechat.rs 5 处（`WechatConfig::from_env` → `parse(WechatCredsRaw)`，PEM/证书解析留域内）；http-api/admin_export.rs 2 处（ExportConfig 经 `ApiDeps.export` 由组合根构造）；http-api/lib.rs 1 处（`build_router` 新增第 4 参数 `h5_static_dir`）；app/main.rs 5 处（EXPORT_DIR/引导账号/ADMIN_STATIC_DIR 及模式装配）。`PAYMENT_MODE` 布尔位 `simulation_payment` 由枚举比较派生（票 17 的枚举判断提前就位）。
- **clippy 禁令**：clippy.toml `disallowed-methods` 登记 `std::env::var`/`std::env::var_os`（恢复票 03 原配置基础上追加）。全仓直读点收口为：config.rs `SystemEnv`（统一入口，allow 标注）+ 三类显式豁免——①测试模块连接变量（platform ids/redisx/sessiontoken 测试、gateway-runtime 三测试、http-api support、money_db_proof）②migrate 子命令的 DATABASE_URL（运维子命令不加载全量凭据）③压测子命令 DATABASE_URL 回退。grep 复核：业务模块（modules/*）与 HTTP 层（http-api/src）零直读。
- **测试装配适配**：TestAppBuilder 新增 `with_export(dir, max_rows)`（export flow 的目录经 ApiDeps 注入，删除 `unsafe set_var("EXPORT_DIR")`——注入式断言替代进程级环境变量写取）；build_router 三处测试调用点补 `None`；三处 ApiDeps 测试构造补 `export` 字段。
- **集成轮插曲（非本票回归，测试库卫生债）**：首轮集成轮 resilience_flow 失败——真库积压 57 条**孤儿 pending 退款记录**（历次断言失败轮 panic 退出、cleanup 未执行的残留；payment_order 已不存在，永远无法执行），`ORDER BY id LIMIT 50` 的 FIFO 领取窗口被挤出导致测试记录领不到。已清理孤儿记录并全绿；**防复发项登记票 23**（集成轮入口前置清理孤儿 refund_record，或失败轮残留的例行清理）。
