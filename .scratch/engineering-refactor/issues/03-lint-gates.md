# 03: 门禁骨架（S1）

**What to build:** 把低级错误挡在编译与静态检查阶段。建立工作区级 lint 表：rustc 警告即错误；clippy 的正确性/可疑/性能三组与通配枚举分支检查为失败级（持锁跨 await 属可疑组自动覆盖，不单列），全部警告为警告级，测试内解包豁免用 clippy 原生开关落地。清零存量：非测试代码约 137 处解包逐处改写或显式预期标注，产出「存量豁免清单」（逐条理由）——它同时是后续环境变量禁令豁免清单的雏形。补齐格式化配置（行宽锁定，nightly 项注释备用）。任务脚本新增 clippy 目标，日常检查升级为「格式化 + clippy + 测试」三件套；既有的前后端契约对账脚本纳入日常检查。

**Blocked by:** None (can start immediately)

Status: ready-for-agent

- [x] 工作区 lint 表生效：warnings=deny、correctness/suspicious/perf=deny、wildcard_enum_match_arm=deny、unwrap_used/expect_used=deny + 测试豁免开关
- [x] clippy 配置文件建立（含 disallowed-methods 机制的空表占位与注释说明，禁令本体由票 16 填充）
- [x] 非测试代码解包清零：逐处改写或预期标注，豁免清单文件交付（逐条理由）
- [x] 格式化配置落地（行宽 + 预留 nightly 项注释）
- [x] 任务脚本：新增 clippy 目标；check = 格式化检查 + clippy + 测试；前后端契约对账脚本纳入 check
- [x] 三件套在工作机全绿，任何新增警告即失败

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` = fmt-check + clippy（全仓 0 错误）+ 全量测试 + 前后端对账（99 调用点 / 121 路由 / 0 缺口），退出码 0。
- **门禁接线**：根 Cargo.toml `[workspace.lints]`（`rustc warnings=deny` 一并升级 clippy 警告级；clippy `all=warn(priority=-1)` + correctness/suspicious/perf/unwrap_used/expect_used/wildcard_enum_match_arm=deny），21 个 crate 全部 `[lints] workspace=true` 接线；`clippy.toml`（测试豁免开关 + disallowed-methods 占位 + `large-error-threshold=512`）；`rustfmt.toml`（max_width=100 + nightly 项注释备用）。
- **存量清零方式**（约 50 处门禁违规，其中 rustc 层 unwrap/expect 20 处生产代码）：
  - 等价改写：毒锁容错 `unwrap_or_else(|e| e.into_inner())`（ids/registry/snapshot/telemetry/adapter 会话表与快照/遥测/审计锁）、`is_some+unwrap` → let 链（charging_settle/scheme/spec）、`segments.last().unwrap()` → 局部变量与 map 比较（charging_cost）、Option 直绑（pricing publication 显式 match 分支）；
  - 不可达分支显式 `panic!`（时区偏移/当日零点/月首日/刚建账号/契约状态码/HMAC 空密钥/信号注册/黄金帧编码）；
  - 持锁跨 await（await_holding_lock，suspicious 组实战命中）：ids 测试锁改 tokio 异步锁；gateway/runtime 测试的流锁以文件级 allow 登记为**票 04 前临时债务**（统一端口锁后移除）；
  - `Result<_, ()>` → 具名单元错误（`ActivityRejected`、`FileNameRejected`，调用方零改动）；type_complexity ×13 以具名投影元组别名收编；serde_json::Value 匹配改逐变体穷尽。
- **豁免清单**（28 处 `#[allow(clippy::…)]`，全部带行内理由）：`too_many_arguments` ×9（repo 行参/帧字段/审计列即契约字段，不引入参数对象）；`wildcard_enum_match_arm` ×6（错误映射兜底分支）；`large_enum_variant` ×1（LoginOutcome 登录即序列化）；`dead_code` ×1（测试建链种子字段）；32 个 tests/ 文件 crate 级 allow（unwrap/expect/wildcard——clippy 的 tests 豁免开关只覆盖 `#[cfg(test)]`，不覆盖独立测试二进制）；另 `large-error-threshold=512`（ApiError 按契约携带字段数组与原因，处理器转响应即弃，不 Box）。
- **实施注意（后续票借鉴）**：clippy --fix 会顺手动 trybuild 夹具（行号漂移破坏 .stderr 快照）——夹具已还原，票 06 起 clippy --fix 后必须复跑 compile_fail。
