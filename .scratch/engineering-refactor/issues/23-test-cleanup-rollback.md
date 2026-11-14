# 23: 测试收尾销项（S8）

**What to build:** 执行测试治理清单的收尾批次，全部对象经审计与二次独立复核、实施时以最新代码再复核，替代覆盖不存在即撤回该条：① 删除——配置默认值的无断言/恒真测试（字段映射已被正路径测试覆盖）、两个环境变量清单考据测试（**前提：票 16 的一致性测试与告警行为测试已落地**，缺前提不删）、真实缓存单次探活测试（13 个真库流以更强方式覆盖连接性）；② 裁剪——与财务流严格超集重复的计量核实场景（保留全仓独有的人工金额分支，裁剪后由票 23 补测承接判定覆盖）；③ 合并——状态机两个运行期测试合并且补上从未被调用的迁移方法调用（编译期反例仅覆盖一种非法迁移，「拓扑已锁定」不成立，正例锚点必须补全；若票 14 已实施则随其销项）；④ 加固——登录流测试中被丢弃的两处响应断言补全（旧令牌失效断言已存在勿重复）。

**Blocked by:** 16（考据型测试删除的前提）、21（域结构归位后再动测试，避免冲突）

Status: done

- [x] 删除三项销项（各附审计证据与替代覆盖路径；替代不存在即撤回）
- [x] 裁剪项销项：计量核实重复场景移除，独有人工金额分支保留
- [x] 状态机测试合并 + 补迁移方法调用（或注记已由票 14 销项）
- [x] 登录流两处丢弃断言补全（无凭据刷新 → 401；轮换新令牌消费验证）
- [x] 补测承接确认：未知变量告警行为测试（票 16）、计费分段守恒纯函数直测（若票 08 未随域落地则本票补）、模式非法取值 fail-fast（票 16）
- [x] 全量集成轮跑绿；测试代码量与守护能力的对照记录（删除约 175 行 vs 新增覆盖点）追加到基准文档

## Comments

### 2026-11-15（实施完成）

**逐条销项（对象经 7.2 清单 + 最新代码二次复核）**

1. **删除·配置考据**：`config.rs` `system_prefixes_do_not_shadow_known_vars` 删除（断言清单字面含 RUST_LOG 的恒真考据）。替代覆盖：`frozen_list_equals_actually_read_set` 一致性测试——系统前缀吞掉清单变量的情形表现为实际读取集合缺项而失败，行为守护强于字面断言。`defaults_applied` 与 `frozen_list_covers_core_vars` 已随票 16 删除（git 6474abb 核实）。
2. **删除·缓存探活**：`redisx.rs` `ping_real_redis` 删除。替代覆盖：真库集成轮的会话/频次/租锁流（user/admin 登录、MFA、踢线）全部真实经过 deadpool 连接池的取用与命令往返，连接故障即整轮红。
3. **裁剪·计量核实场景 A**：`settlement_admin_flow.rs` 场景 A（转人工→提案→双审→重排队→重算入账）删除——`admin_billing_flow.rs` `meter_review_list_propose_decide_flow` 同路径严格超集（转人工 40 行 / 首审冲突 801 行 / 批准重排队 260 行 / 重算入账 271 行，逐断言复核成立）。全仓独有 `resolve_manual_amount`（40+10 人工金额入账 + 超限 Conflict）保留于场景 B。
4. **状态机**：票 14 已实施——`charging_state.rs` 已有三个运行期正例测试（mainline / start_rejected / reserved_none），本票按 7.2 评审纠正补齐**迁移 6 `restore_completed`（refunding→completed）的运行期锚点**（该方法运行期零调用，正例表达可达性由测试锁定），两项合并销项注记。
5. **加固·登录流**：`admin_auth_flow.rs` 两处丢弃断言补全——①无凭据 refresh 的 `let _ = (status, body)` 改为断言 401 + code 1001（处理器已核实：bearer 缺失直接 401）；②`let _ = new_refresh` 改为链式消费验证（new_refresh 再刷 → 200 且再下发新令牌）。旧令牌 401 断言已存在，未重复。
6. **补测承接**：①未知变量告警——`warn_unknown_env_vars` 收集逻辑提纯为 `unknown_env_var_names`（返回 Vec）后直测三段语义（清单外应告警 / 系统前缀不告警 / 清单内不告警）+ `Config::load` 不阻断；②计费分段守恒——域落批次已把判定提取为 `charging_settle::settle_session` 纯函数（billing.rs 仅调用 + 转人工），82 条 golden case 直测（含 MeterReview/InvalidPricing 全错误分支），原「内嵌 300 行 process_job」前提已被先行批次消除，注记销项不重做；③PAYMENT_MODE/LOGIN_MODE fail-fast 随票 16 落地（`*_values_and_fail_fast` 两测试）。
7. **防复发清理（承接票 16/20 移交）**：`tasks.mjs test-integration` 入口加前置清场——孤儿退款记录（LEFT JOIN payment_order 缺失且 pending/processing）、残留 domain_event（pending/failed）、admin 登录限流键，防止历史残留挤占 refund 领取 LIMIT 与事件 claim 窗口。

**代码量对照（追加至 docs/refactor-plan.md 7.2 红线节）**

实际删除 152 行 / 新增 79 行（净 -73；预估 175 行含 scenario 缩排归并的统计口径差）。新增覆盖点：迁移 6 正例锚点、未知变量告警三段语义直测、登录流两处断言、集成轮入口防复发清场——守护能力不减反增（考据型字面断言全部换为行为断言）。

**验证证据**

- `cargo clippy --workspace --all-targets` 全绿；`node tasks.mjs check` 四段 exit=0。
- 真库集成轮（含新前置清场入口首跑）：**193 通过 / 0 失败**（基线 194 − 本票删除的 `ping_real_redis` 探活测试 1 项，0 失败；守护能力不减）。
- `cargo test -p chargepilot-platform` 25 passed（含新 `unknown_env_vars_warn_without_blocking`）；`cargo test -p chargepilot-domain` 36 passed（含补锚点后的 mainline）。

**偏差**

无。状态机项按票面「若票 14 已实施则随其销项」注记，并按 7.2 评审纠正补齐迁移 6 锚点（票 14 只覆盖了 5 条迁移链路，restore_completed 此前零调用）。
