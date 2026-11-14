# 12: marketing 与 export 域枚举化（S3g）

**What to build:** 营销与导出两域词表转正：券状态、发放来源、发放状态、折扣类型（金额/比例/时长免费）、活动触发类型、导出状态。折扣类型是计费优惠计算的关键分支输入，转正后优惠计算改为 match 穷尽（编译器保证新增折扣类型时漏分支即失败）。

**Blocked by:** 06（依赖 money 词表）

Status: done

- [x] 营销五系词表与导出状态枚举化，字面量对照测试锁定
- [x] 优惠计算对折扣类型/触发类型的分派全部转 match 穷尽
- [x] 日常 check + 全量集成轮跑绿；字面量清单快照只减不增

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **186 通过 / 0 失败**（较票 11 增 2 = marketing.rs/export.rs 对照测试）。clippy 全仓 0 告警。
- **交付物**：
  - `domain/src/marketing.rs`：六 String 新类型转正——`CouponStatus`(2)、`ActivityStatus`(2)、`DiscountType`(3)、`ActivityTriggerType`(4，对照表裁决由 `TriggerType` 更名)、`GrantStatus`(3)、`GrantSource`(5)，含对照测试。
  - `domain/src/export.rs`（新建，对照表裁决自 operations 迁出）：`ExportStatus`(5)、`ExportFileFormat`(2)，含对照测试；`LEGACY_PDF` 保留为词表外常量（历史遗留清理识别）。
- **优惠计算 match 穷尽（票核心验收）**：`discount_for` 删除 `_ =>` 兜底分支，三变体穷尽分派——新增折扣类型漏分支即编译失败；`coupon.rs` 的 `valid_coupon_shape` 与活动校验 `normalize_activity` 同步穷尽化。`GrantRule.discount_type` 与 `CouponTemplate.status` 字段枚举化。黄金差分测试保持「未知类型 → NotFound」行为等价覆盖（parse 拒绝路径显式断言错误类别）。
- **历史数据兼容决策**：`ExportTaskRow.file_format` 保持 String 行模型（历史 pdf 行清理路径必须可读；DB 词表只有 csv/xlsx），受理/下载/编码分派一律经 `ExportFileFormat::parse` 白名单——对照表「历史 pdf 只为清理识别保留」落实。
- **迁移站点**：marketing activity/query/coupon（事件与规则触发类型、模板/发放状态投影与入参校验 parse 化、核销链 GrantWithRule.status/discount_type）、export service/cleanup/artifact（任务状态投影、受理校验、编码分派、mime 判定）。
- **语义收紧登记**：旧版「未知折扣类型返回 NotFound」由两处承接——DB 解码 parse 拒绝 + match 穷尽编译保证；黄金差分的 cashback 样本改为显式错误类别断言（覆盖不变）。
- **过程记录**：无绑定错位（票 10 教训生效——所有 SQL 均为简单 WHERE 或原有绑定链内改值）；golden 差分与集成轮全绿确认零语义漂移。
