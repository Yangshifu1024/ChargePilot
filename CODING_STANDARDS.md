# 编码硬规则（Rust / sqlx）

这里只放「违反即生产回归」的硬规则，均来自真实事故。规则条目保持可执行：每条写清触发条件、核对方法与实例，不展开一般性风格讨论。

## 规则①：sqlx 类型映射必须对照迁移 SQL 的列声明

**真相源是 [backend/migrations/0001_init.sql](backend/migrations/0001_init.sql)**，不是 Rust 结构体的既有写法。`#[derive(sqlx::FromRow)]` 字段、`query_as` 元组、以及 `sqlx::query(...).bind(...)` 的绑定值类型，逐一对照列声明：

| 列声明 | 解码 / 绑定类型 |
| --- | --- |
| `tinyint unsigned` | `u8`（可空列 `Option<u8>`） |
| `int unsigned` / `int unsigned NOT NULL`（如自增 `id`） | `u32` / `u64`（按列宽与业务取值选，主键用 `u64`） |
| `bigint unsigned` | `u64`（可空列 `Option<u64>`） |
| 有符号整型（无 `unsigned` 后缀） | `i64`（小整型也统一 `i64`，不猜 `i16`/`i32`） |

**触发条件**：新写或修改 FromRow 结构体 / query_as 元组 / bind 参数时；ALTER 列类型后同步核对解码方。

**事故实例（2026-11-15）**：`device.signal_strength` 声明 `tinyint unsigned`，`DeviceRow.signal_strength` 曾写成 `Option<i64>`。sqlx 的 MySQL 类型校验只在**行值非 NULL** 时执行，开发库该列全 NULL，`node tasks.mjs check` 全绿；生产首条心跳落库后运行态查询立即 `mismatched types` 崩溃。修复见 `crates/modules/device/src/management.rs`（`Option<u8>`），守护断言见规则②的实例。

## 规则②：集成测试的解码路径必须有非 NULL 真实形态行

**触发条件**：被测查询的列在测试夹具里恒为 NULL，但生产会出现非 NULL 值——类型断裂（规则①）与 NULL 处理错误都会被 NULL 夹具掩盖。

**要求**（满足其一即可，优先前者）：

1. **驱动真实写入端**：走真实业务链路产生数据（如向 `GatewaySink` 投递心跳事件），再断言落库行与读端响应。
2. **造行重读**：直接把列 UPDATE 成生产的真实形态值（非 NULL、边界取值如 0），走完整解码路径后断言响应。

**实例**（同上事故）：

- 产生端：`crates/gateway-runtime/tests/sink_flow.rs`——心跳事件后按 `u8` 解码断言 `device.signal_strength = 21`，且 `last_heartbeat_at` / `last_seen_at` / `signal_at` 非 NULL。
- 消费端：`crates/http-api/tests/device_flow.rs`——UPDATE `signal_strength = 20` 后重读运行态列表，断言响应字段按 unsigned 单字节解码。

## 核对方法

涉及上述两类改动时，除 `node tasks.mjs check` 外必须跑 `node tasks.mjs test-integration`（真库）。规则①的断裂只有非 NULL 行能触发，规则②的夹具只在真库轮执行——单测与编译期检查都替代不了。
