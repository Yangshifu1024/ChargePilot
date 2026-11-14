# 13: events 事件类别枚举化（S3h）

**What to build:** 事件 crate 的类别词表由常量串容器转为进程内枚举：类别列在数据库侧是无约束字符串，枚举仅约束进程内（注册表路由、入队校验），载荷侧保持透传。同文件的事件来源枚举已是正确范式，本票把二者对齐。处理器注册表的键从字符串改用枚举匹配。

**Blocked by:** 05

Status: done

- [x] 事件类别枚举化，处理器注册表键改枚举匹配
- [x] 载荷透传路径确认零变化（数据库列仍为字符串写入原字面量）
- [x] 与事件来源枚举的范式一致性核对（同一 crate 两种词表同款写法）
- [x] 日常 check + 全量集成轮跑绿

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **186 通过 / 0 失败**。clippy 全仓 0 告警。
- **交付物**（`crates/events`）：
  - `EventKind` 进程内枚举（七类别，`as_str` 锚点 + `parse` 返回 Option——kind 列为无约束 VARCHAR，未知类别保持「无已登记处理器」路径，与旧行为一致）；旧 `kind::` 常量模块保留为兼容入口（委托枚举 `as_str`）。
  - `EventDeliveryStatus`(3)：DB ENUM 词表，sqlx 三件套**手写**（与 domain `wordlist_enum!` 同款打样范式；events 是 domain 外独立 crate，宏保持 `pub(crate)` 不跨 crate 导出——波及 domain 全部调用点的 `#[macro_export]` 改法被否决并登记）。
- **注册表键改枚举匹配**：`Registry.handlers: HashMap<EventKind, _>`，`register(EventKind, handler)`；`get(&str)` 内 parse 后路由。登记点 app/main.rs 两处、入队点 gateway sink 与集成测试全部转枚举。
- **入队校验**：`enqueue::in_tx` 的 kind 参数改 `EventKind`（编译期约束入队侧只能发已知类别）；`repo.rs` 三处 status SQL 字面量（claim IN / published / failed）改绑定。
- **载荷透传零变化**：payload JSON 与 kind 字面量写入路径逐字节不变；`EventSource` 保留（serde lowercase 范式正例 + FromStr），同 crate 两种词表写法一致性核对完成——进程内词表（EventKind）与 DB 词表（EventDeliveryStatus）职责分离且注释登记。
- **附带基建**：events crate 首次接入 `chargepilot-domain` 依赖（无运行时使用，为后续词表宏跨 crate 复用预留；本票词表手写未动用）。
