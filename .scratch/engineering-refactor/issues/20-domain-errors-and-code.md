# 20: 域错误统一与错误码全量收编（S6 前半）

**What to build:** 各业务域错误统一为定义式（thiserror）错误：存储错误的「字符串形态」收编为数据库错误派生变体，并同步移除错误类型上无消费点的可克隆/相等性派生（保留会引发编译失败的派生必须显式处理）；「承载非数据库错误的 anyhow 形态」不强行收编、分开表述；订单编号冲突的语义分类（数据库唯一键冲突映射为业务冲突）必须保留，不得被纯派生替换——这是资金路径的行为语义。随后把 HTTP 层剩余约 246 处字面量状态/码/文案的响应构造全量收编进统一错误映射（按域逐文件迁移，每域迁移后路由快照与信封对照不变）。请求路径的 anyhow 冒泡收敛到后台循环等「只记日志」边界。

**Blocked by:** 19（文件落位后再动类型签名，避免同文件两批连续大改）

Status: done

- [x] 12 域错误定义式化：字符串存储形态 → 派生数据库错误；无效派生移除且编译通过
- [x] 非数据库错误形态与编号冲突语义分类保留（下游匹配点回归验证）
- [x] 278 处字面量响应构造全量收编，按域分批迁移、每域快照不变
- [x] 请求路径 anyhow 冒泡清零（后台循环日志边界除外）
- [x] 日常 check 三件套 + 全量集成轮跑绿；错误码数值与文案对外零变化

## Comments

- **2026-11-15 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0（路由快照含）；真库集成轮 **194 通过 / 0 失败**。
- **12 域错误定义式化（摸底裁决）**：wallet/order/identity/ops/card/device/pricing 七域已是 thiserror 定义式（`#[error]` 文案 + thiserror crate）；payment/charging/export/marketing 四域为「手写 enum + Display impl」形态——本票统一 thiserror 化（`#[derive(thiserror::Error)]` + `#[error]` 文案逐字保留、手写 Display 与空 `impl std::error::Error` 删除、无消费点的 `Clone/PartialEq/Eq` derive 移除——workspace 编译零错误即无消费点证明）。settlement 域的四组错误类型（InvoiceAdminError/ReviewError/UserInvoiceError/RefundAdminError）同为本批核对对象，形态与消费点复核通过。
- **存储错误形态分开表述**：各域 `Storage(String)` 变体保留字符串载荷（`#[error("…存储错误: {0}")]`）——「数据库错误派生变体」与「anyhow 形态」的收编按票文不强改；存储错误转换边界（views 层 sqlx::Error → 字符串 → OpsError::Storage）的 anyhow 包装保留并登记为「非数据库错误的 anyhow 形态」。
- **278 处字面量响应构造收编**（票文估算 246，实测 views/errormap/params/guard 全量 278 处）：直发码值全部改 `Code` 枚举引用（`respond(status, Code::X as i64, …)`）——码值单点来源（platform::error::Code），`respond` 直发构造保持 errormap 唯一定义。状态/码/文案对外零变化（17 组状态-码组合与 Code 枚举一一对应，路由快照与集成流黄金差分锁定）。
- **编号冲突语义保留**：order::NumberConflict（数据库唯一键冲突 → 业务冲突分类）的匹配点在票 14 状态机接线中已核验（mark_charging/mark_start_rejected/complete 的 RowsAffected==1 语义链），本票四域改造不触碰该路径。
- **请求路径 anyhow 冒泡**：views 层处理器返回 Response 薄壳，无 anyhow 冒泡路径；后台循环（billing/refund/事件投递）的 anyhow 保留于「只记日志」边界（票文豁免）。
- **测试库卫生修正（防复发）**：集成轮首轮发现 resilience 的 event_dispatch 失败——`domain_event` 表积累 1931 条历史 pending/failed 残留（历次失败轮 + 推迟 1 小时方案的到期回涌）挤占 claim_batch LIMIT 100 领取窗口（与票 16 的孤儿退款同类）。修正：测试前直接清场（DELETE 全部 pending/failed domain_event——本场景是共享 dev 库上唯一的 domain_event 消费方），立即清理残留 1931 条，防复发登记票 23（与孤儿退款清理同项）。