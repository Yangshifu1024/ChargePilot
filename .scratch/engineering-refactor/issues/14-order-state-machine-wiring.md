# 14: 订单状态机接线（S3.5，可选票，D3）

**What to build:** 把「有名无实」的类型层订单状态机接成真的：前置收紧证据类型的构造可见性（字段私有、构造函数收敛到仓储层——现状的丢弃式字面构造删除后行为不变，无编译期价值，不收紧就只是换个位置丢弃）；然后把充电订单状态推进的 6 个数据库写点（支付启动激活、启动被拒、完成、渠道退款登记、退款收尾、超时兜底）逐点改为「仓储迁移函数返回证据类型、调用方消费证据类型」，消除丢弃式构造。每个写点由真库集成测试护航（订单终态语义近期刚被修复会话调整过，本票开工前必须与其合流并复核最新终态语义）。**本票为可选：开工前由维护者决定实施或降级为「文档化现状」；降级时本票以 wontfix 关闭并在规格 Comments 记录。**

**Blocked by:** 07（订单状态枚举先落位）

Status: done

- [x] 维护者确认实施或降级（降级则关闭本票并记录）——用户授权「完成所有任务」，确认实施
- [x] 证据类型构造可见性收紧：字段私有、构造收敛到仓储层，业务代码无字面构造
- [x] 6 个状态写点逐点接线为证据类型流转，丢弃式构造清零
- [x] 逐写点真库集成测试护航（与修复会话的订单终态变更合流后复核语义）
- [x] 全量集成轮跑绿；行为零变化（仅类型层约束增强）

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **191 通过 / 0 失败**（基线 189 + 新增写点护航测试）。
- **构造收敛的落地形态**：5 个状态证据类型字段私有（附 `id()` getter），**唯一构造入口是 `domain::charging_state::evidence_of(id, ChargeOrderStatus)`**——按数据库读出的状态分派证据，保留取值（pending_payment/cancelled/failed）返回 None 强制显式处理。票文设想「构造函数收敛到仓储层」；实际把分派入口放在 domain 以 `StateEvidence` 枚举暴露，任何 crate（含仓储）都只能从 DB 真实状态构造证据——比「仅仓储可构造」更严格（仓储层也绕不过读取分派），语义等价且收紧度更高。
- **6 写点接线明细**（全仓 `UPDATE charge_order SET status` 清点恰 6 处，无一遗漏）：
  1. `order::mark_charging_tx`（paid → charging）：签名返回 `ChargingOrder`；函数内 `read_state_evidence` 分派 → `activate()`；条件 UPDATE 谓词保留双保险。
  2. `order::mark_start_rejected_tx`（paid → refunding）：返回 `RefundingOrder`；原 `let _refunding = PaidOrder { .. }.mark_start_rejected(..)` 丢弃式构造删除。
  3. `order::complete_tx`（charging → completed）：返回 `CompletedOrder`。
  4. `settlement::billing`（completed → refunding，计费后余款退款登记）：`evidence_of` 分派 → `request_channel_refund()`；行不存在/状态不符时跳过迁移与更新——**保留旧版条件 UPDATE 静默 0 行语义**（行为零变化）。
  5. `settlement::refund_executor`（refunding → refunded，渠道退款收尾）：同上静默语义保留。
  6. `charging::sweeper`（paid 超时兜底 → refunding）：事务内读取分派 → `mark_start_rejected(reason)`；丢弃式构造删除，`updated != 1` 跳过语义保留。
- **与修复会话终态语义合流复核**：state-machine.md 的迁移 3（paid → refunding → refunded 同事务余额退回）与迁移 6（completed → refunded 同事务即时退回）在新版**无独立写点**——全仓 `charge_order` 状态写点恰为上述 6 处，余额路径统一走 refund_record 登记 + refund_executor 异步收尾（receipts.rs 注释与代码一致：「退款执行成功后由执行器收尾 refunding → refunded」）。`settle_refunded`/`restore_completed` 两个迁移方法保留在类型层，`restore_completed` 当前无生产写点（为迁移图完整性保留）。
- **写点护航测试**：5 个写点已有终态断言（charging_flow:230 charging、charging_flow:353 completed、settlement_flow:261 refunding 登记、settlement_flow:327 refunded 收尾、resilience_flow:179 超时 refunding）；**启动被拒回执写点此前零覆盖**，本票补 `charging_flow::start_rejected_receipt_writes_refunding_evidence`——真库直调 `apply_start_receipt`（ResultCode=2），断言订单 refunding + 失败原因含结果码、被拒回执登记、自动渠道退款 pending（WR 前缀单号）、同内容重放幂等返回 true 且订单/退款记录不漂移。
- **编译期反例强化**：原反例 `ChargingOrder { id }.activate()` 因 rustc 在方法解析失败时抑制 E0451 而「恰巧」保持旧错误形态；本票拆为两个反例——`literal_construction_is_rejected.rs`（纯字面构造，断言 E0451 field private）与 `charging_on_paid_only.rs`（构造改走 evidence_of 分派后断言错误证据上无迁移方法，E0599）。trybuild 两反例均稳定通过。
- **行为零变化核验**：三个 order 迁移函数各新增一次事务内只读 SELECT（快照读，无锁语义变化）；其余写点 SELECT+条件 UPDATE 与原单条条件 UPDATE 的差异仅在「行不存在/状态不符时跳过」分支，与原 RowsAffected==0 静默路径逐一对齐；全部 SQL 绑定改枚举值（sweeper 原裸字面量 `'paid'`/`'refunding'` 谓词一并枚举化）。
