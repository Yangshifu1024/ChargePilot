# ChargePilot

充电桩运营平台的 Rust 单体项目。架构基准、终态目录树、依赖规则与实施计划见 [docs/blueprint.md](docs/blueprint.md)——实施与蓝图冲突时，当场在其偏离登记表记一行；编码前先读 [CODING_STANDARDS.md](CODING_STANDARDS.md)。

## Agent skills

### Issue tracker

议题与规格以 markdown 文件存放在仓库 `.scratch/` 目录（本地 markdown 方式，无远程跟踪服务）。见 `docs/agents/issue-tracker.md`。

### Triage labels

五个规范 triage 角色使用默认标签字符串：`needs-triage`、`needs-info`、`ready-for-agent`、`ready-for-human`、`wontfix`。见 `docs/agents/triage-labels.md`。

### Domain docs

单上下文布局：仓库根目录一个 `GLOSSARY.md`（惰性创建），架构基准在 `docs/blueprint.md`。见 `docs/agents/domain.md`。

### Coding standards

违反即生产回归的硬规则（当前两条：sqlx 类型映射对照迁移 SQL 的列声明；集成测试解码路径必须有非 NULL 真实形态行）见根目录 [CODING_STANDARDS.md](CODING_STANDARDS.md)。写或改 FromRow / query_as / bind 前先读它。
