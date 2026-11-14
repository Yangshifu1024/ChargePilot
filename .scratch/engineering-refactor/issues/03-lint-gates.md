# 03: 门禁骨架（S1）

**What to build:** 把低级错误挡在编译与静态检查阶段。建立工作区级 lint 表：rustc 警告即错误；clippy 的正确性/可疑/性能三组与通配枚举分支检查为失败级（持锁跨 await 属可疑组自动覆盖，不单列），全部警告为警告级，测试内解包豁免用 clippy 原生开关落地。清零存量：非测试代码约 137 处解包逐处改写或显式预期标注，产出「存量豁免清单」（逐条理由）——它同时是后续环境变量禁令豁免清单的雏形。补齐格式化配置（行宽锁定，nightly 项注释备用）。任务脚本新增 clippy 目标，日常检查升级为「格式化 + clippy + 测试」三件套；既有的前后端契约对账脚本纳入日常检查。

**Blocked by:** None (can start immediately)

Status: ready-for-agent

- [ ] 工作区 lint 表生效：warnings=deny、correctness/suspicious/perf=deny、wildcard_enum_match_arm=deny、unwrap_used/expect_used=deny + 测试豁免开关
- [ ] clippy 配置文件建立（含 disallowed-methods 机制的空表占位与注释说明，禁令本体由票 16 填充）
- [ ] 非测试代码解包清零：逐处改写或预期标注，豁免清单文件交付（逐条理由）
- [ ] 格式化配置落地（行宽 + 预留 nightly 项注释）
- [ ] 任务脚本：新增 clippy 目标；check = 格式化检查 + clippy + 测试；前后端契约对账脚本纳入 check
- [ ] 三件套在工作机全绿，任何新增警告即失败
