# 07: order 域枚举化（S3b）

**What to build:** 订单域词表转正：充电订单 8 态主状态、充电类型（启动/续充）等，沿用票 06 的打样板式。注意订单主状态与领域层类型状态机（每态新类型 + 迁移方法）是并存的两个机制——数据库往返用枚举，进程内推进的证据类型留给票 14，本票不消费证据类型、保持现状（防实施者自行发挥）。常量串容器（订单状态词表、进行中状态集合）删除，比较全部转 match 穷尽。

**Blocked by:** 06（依赖 money 词表）

Status: done

- [x] 订单 8 态与充电类型枚举化，字面量对照测试锁定
- [x] 订单状态常量串容器与进行中状态集合删除；与订单支付轴词表的关系按对照表落位
- [x] 类型状态机文件保持现状不动（接线归票 14）
- [x] 日常 check + 全量集成轮跑绿；字面量清单快照只减不增

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **179 通过 / 0 失败**（较票 06 增 2 = order.rs 新增测试）。clippy 全仓 0 告警。
- **范围核对**：票文本所列「充电类型（启动/续充）」经 schema 核实为 `card_operation.kind`（card 域，票 10）——`charge_order` 无 `charge_type` 列，本票以对照表 order 节为准落地三型。
- **交付物**：
  - `domain/src/wordlist.rs`：`wordlist_enum!` 宏自 money.rs 拆出为全域共享（宏内路径绝对化 `$crate::`/`::serde::`，调用方零依赖）；对照表总则 1 的手写范式结论随宏落位。
  - `domain/src/order.rs`：`ChargeOrderStatus`（8 态，`is_ongoing` 判定集合收编旧 `ONGOING_STATUSES`）+ `FeedbackCategory`(3) + `FeedbackStatus`(3)，含 `db_literals_match_serde` 字面量对照测试与 ongoing 集合测试。
- **删除**：`modules/order` 的 `OrderStatus` 常量串容器与 `ONGOING_STATUSES: [&str; 2]`。
- **迁移站点**：order lib（8 处绑定）、charging dispatch/stop/receipts（4 处 + `LifecycleView.status`/`OrderRecord.status`/`OrderViewRow.status` 投影枚举化）、payment checkout（进行中集合 2 处）、settlement refund_admin/refund_executor（准入 matches、订单状态投影、`SET status = ? WHERE status = ?` 绑定）、http-api user_charging（`is_ongoing()`）、order user_query/admin_query/feedback（反馈流转全枚举化、`feedback.category` 提交校验转 `parse` 后按枚举绑定写入）。
- **保持现状（按票约定）**：`domain/charging_state` 类型状态机零改动（接线归票 14）；`business_status` 生成列不建型；`OrderViewRow.refund_status`（SQL 派生展示值）与后台状态筛选入参（`status: &str` 允许空串）按对照表总则 5 保留字符串。
- **字面量清单核验**：订单/反馈词表的行投影比较点全部转枚举；生产代码残留同字面量仅 `device_import.status`（"completed"，device 域词表，票 10）。JSON 契约零变化（serde snake_case 输出与原字面量逐字节一致）。
- **过程记录**：迁移中一次错误替换曾给 refund_executor 的 `UPDATE charge_order` 擅自加写 `payment_status` 列（改变行为）——已在提交前发现并还原为纯词表迁移（仅 `SET status = ? WHERE status = ?` 绑枚举）。
