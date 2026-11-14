# 08: settlement 域枚举化（S3c）

**What to build:** 结算域三系审核词表转正：计量核实状态、发票审核状态（两套取值集合）、退款记录状态（含处理中/成功/失败/拒绝），沿用打样板式。结算的计费任务状态常量串容器一并删除。

**Blocked by:** 06、07（依赖 money 与 order 词表）

Status: done

- [x] 计量/发票（两套）/退款记录状态枚举化，字面量对照测试锁定
- [x] 计费任务状态常量串容器删除
- [x] 日常 check + 全量集成轮跑绿；字面量清单快照只减不增

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **180 通过 / 0 失败**（较票 07 增 1 = settlement.rs 对照测试）。clippy 全仓 0 告警。
- **交付物**：`domain/src/settlement.rs` 六枚举——`BillingJobStatus`(3)、`MeterReviewStatus`(3)、`InvoiceType`(2)、`InvoiceRequestReviewStatus`(4)、`InvoiceAdminReviewStatus`(3)、`ManualFeeReviewStatus`(2)，含 `db_literals_match_serde` 字面量对照测试。
- **删除**：`modules/settlement/billing.rs` 的 `JobStatus` 常量串容器（`billing.rs` 内以 `as JobStatus` 别名引用域枚举，meter_review 的三处跨模块 `crate::billing::JobStatus` 引用改 domain 导入；`defer_job` 参数 `&str` → `BillingJobStatus`）。
- **迁移站点**：billing.rs（任务领取/推进/人工出口绑定 + manual_fee_review 的 SET/WHERE 绑定 + 计量修正 approved SELECT）、meter_review.rs（三处任务锁读投影、活跃提案 `IN (?, ?)`、提案 INSERT、二审 UPDATE ×2、幂等与准入比较、`MeterReviewRawRow`/`MeterReviewRow.status` 投影枚举化）、invoice_admin.rs（申请单与双审进度投影、驳回/首审/二审/开票四段 INSERT/UPDATE 全绑定，`ON DUPLICATE KEY UPDATE` 改 `VALUES()` 引用绑定参数）、user_invoice.rs（`invoice_type` 入参一次 parse 存档复用于重放比对与 INSERT、biz_type/review_status 字面量绑定）、http-api/admin_diagnostics.rs（billing_job pending 计数改绑定）。
- **保留字符串（合法，对照表总则 5）**：HTTP 层 invoice_type 前置校验（`user_settlement.rs`，与用例内 parse 校验分层）、审计动作与队列返回值（`"awaiting_second"`/`"rejected"` 作为响应/审计语义串）、`ManualFeeReviewRow.status`（Serialize 视图行，无比较点，后续视图统一时再收）、`domain_event`/`charge_command` 的同字面量（events/charging 域，票 13/10）。
- **过程记录**：迁移中两处枚举绑定插错占位符位置（提案 INSERT 的 status 绑到 charge_order_id 位、approved SELECT 顺序颠倒），被真库集成轮逐次抓出（503 与「修正分段应可入账」失败），已修正并把全部改动过的绑定链按列序逐一核对；教训已吸收——改绑定链必须先看完整 bind 链与 VALUES 列序再插入。
