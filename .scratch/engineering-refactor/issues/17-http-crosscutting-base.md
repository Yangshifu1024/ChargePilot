# 17: HTTP 横切基座（S5 前半）

**What to build:** 让 HTTP 层的横切职责各有唯一归宿，为按域搬迁铺平地基（本票不移动业务文件）。① 全局依赖对象拆解为按域分组的最小句柄组（每组含数据库句柄，装配期必达、无可选字段），经 axum 子状态提取器供处理器按需取用，各域装配逻辑上移组合根；支付模式布尔位改用枚举判断。② 用户会话提取等横切函数从业务文件归入鉴权模块（否则后续门面会跨目录引用，违反域隔离验收）。③ 分页/时间/状态过滤参数提取器落地，按调用点保留双轨上界与双语义。④ 三处既有错误映射枢纽（身份错误直映、运营错误映射函数、停止错误直映）收编进统一错误映射模块，码表补齐旧版缺口并以「显式状态覆盖」支持一码多 HTTP 状态（同一业务码在不同场景配 502/409/400/503 的现状保持）。约 16 个测试装配点经票 04 的共享支撑自动收敛。

**Blocked by:** 04（支撑模块形态对齐 AppState）、16（配置全量化 + 模式枚举）

Status: done

- [x] AppState + 各域最小句柄组落地，可选字段清零，FromRef 子状态提取生效，装配逻辑在组合根
- [x] 会话提取横切函数归入鉴权模块，业务文件内不再有跨文件复用的鉴权辅助
- [x] 参数提取器落地（双轨上界、双语义按调用点保留），重复区间校验在 HTTP 层清零
- [x] 三处错误映射枢纽收编 + 码表缺口补齐 + 一码多 HTTP 状态支持（逐场景回归 502/409/400/503）
- [x] 路由快照（票 02）不变；全部集成流测试跑绿；处理器函数体仍为「提取→绑定→调服务→信封」薄壳

## Comments

- **2026-11-15 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **194 通过 / 0 失败**。
- **① 域子状态组**（`deps.rs`）：`UserSessionBase`（会话基础件五句柄）+ `WalletState`（base + WalletServices）+ `PaymentState`（base + CheckoutService + RechargeService）+ `ChargingStopState`（base + StopService），全部 `FromRef<Arc<ApiDeps>>` 派生、**组内无可选字段**。**路由注册绑定装配开关**（`build_router` 按 `wallet`/`checkout`/`charging_stop` 开关 merge 对应域路由，`simulation_payment` 布尔开模拟确认路由）：开关开启后路由才存在，域处理器对服务的取用必达——原 8 处「服务未装配」503 降级分支删除（生产 serve 全装、测试按需开，分支双向不可达，无测试断言破）。FromRef 的 expect 为装配开关约束的显式断言（allow 标注）。
- **域消费处理器迁移**（8 处，薄壳保留）：user_wallet 退款、user_payment offers/start/recharge、user_charging stop×2 改提域子状态；其余查询/快照处理器保持全局状态（只消费库连接与 snapshots 可选增强——快照缺失回退查库是行为语义，保留 Option）。
- **② 会话提取归位**：`user_guard.rs` 新建，`user_id_from` 从 device_and_user.rs 迁入并改收 `&UserSessionBase`；29 个处理器补 `State<UserSessionBase>` 第二提取；9 文件的 `ops_error` 引用迁 `errormap`。
- **③ 参数提取器**（`params.rs`）：`CursorParams::parse`（游标 + limit 1–2000，domain::limits 锚点，错误文案逐字保留）收编 admin_orders 的 `parse_process_params`；`StatusFilter::passthrough/allowed` 双语义收编 admin_misc 的 feedback 白名单（静默降级语义保留给服务层兜底调用点）。双轨上界维持 domain::page 的用户态/管理态定论。
- **④ 错误映射枢纽收编**（`errormap.rs`）：identity_error（原 lib.rs）、ops_error（原 admin_auth.rs）、stop_error（原 user_charging.rs 直映 match）三枢纽归一；**码表缺口补齐**——4291（LoginRateLimited）、3001（ChannelInteraction）、2018（QueryExpired）、5000（InternalError）入 `Code` 枚举（数值不变，编译期 const assert 锁定）；**一码多状态**——`httpx::error_with_status` 显式覆盖（3001 在 502 网关失败与 409 模拟渠道冲突两场景并存，`Code::http_status` 默认 502 主态）。响应构造（respond）迁 errormap，lib.rs re-export 保持 `crate::respond`/`crate::bearer` 引用路径不变。
- **api_contract 全量装配**：快照要求全量路由注册——测试 build() 改为各域句柄全量装配（惰性池 + 服务构造仅字段赋值不触库，探活错误信封非 404/405 即通过）。
- **支付模式枚举判断**：票 16 已将 `simulation_payment` 改为 `PaymentMode` 枚举比较派生，本票布局就绪（布尔位仅存于 ApiDeps 字段，票 19 全局依赖退役时一并处理）。