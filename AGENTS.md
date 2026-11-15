# ChargePilot

充电桩运营平台的 Rust 单体重写项目（替代 Go 多服务旧仓库）。整体方案、实施计划与验收标准见 [docs/rewrite-plan.md](docs/rewrite-plan.md)；架构决策见 [docs/adr/](docs/adr/README.md)。

## Agent skills

### Issue tracker

议题与规格以 markdown 文件存放在仓库 `.scratch/` 目录（本地 markdown 方式，无远程跟踪服务）。见 `docs/agents/issue-tracker.md`。

### Triage labels

五个规范 triage 角色使用默认标签字符串：`needs-triage`、`needs-info`、`ready-for-agent`、`ready-for-human`、`wontfix`。见 `docs/agents/triage-labels.md`。

### Domain docs

单上下文布局：仓库根目录一个 `GLOSSARY.md`，架构决策记录位于 `docs/adr/`。见 `docs/agents/domain.md`。

### Coding standards

违反即生产回归的硬规则（当前两条：sqlx 类型映射对照迁移 SQL 的列声明；集成测试解码路径必须有非 NULL 真实形态行）见根目录 [CODING_STANDARDS.md](CODING_STANDARDS.md)。写或改 FromRow / query_as / bind 前先读它。
