# 09: payment 域枚举化（S3d）

**What to build:** 支付域词表转正：支付意图状态（与票 06 拆分出的支付单状态分家后，唯一属主落位本域）与通道业务类型等，沿用打样板式。支付意图列上的派生生成列（待支付意图占用端口约束依赖的状态值）仅数据库侧承担，不建 Rust 类型。

**Blocked by:** 06、07

Status: done

- [x] 支付意图状态枚举化（与支付单状态无共享类型），字面量对照测试锁定
- [x] 通道业务类型等剩余支付词表转正
- [x] 派生列相关逻辑确认零代码维护（数据库生成列承担）
- [x] 日常 check + 全量集成轮跑绿；字面量清单快照只减不增

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **181 通过 / 0 失败**（较票 08 增 1 = payment_intent.rs 对照测试）。clippy 全仓 0 告警。
- **交付物**：`domain/src/payment_intent.rs` 的 `IntentStatus` 常量容器转正为 `wordlist_enum!` 枚举 `PaymentIntentStatus`（对照表裁决名，五值全——initiated/paid/expired/closed/refund_required，取值语义移入枚举文档行内注释）；`db_literals_match_serde` 对照测试。money.rs 中零消费的 `PaymentIntentStatus(String)` String 新类型删除（对照表「存量忠实、无需拆分」的原类型自此由枚举承担）。
- **迁移站点**：payment checkout（冻结意图锁读绑定 ×3、过期扫描 UPDATE 绑定 ×2）、payment apply（迟到意图 query_scalar 投影改枚举、迟到退款登记绑定 ×2）、charging receipts/sweeper（迟到退款 UPDATE 绑定 ×4）。
- **生成列确认**：`charge_payment_intent.active_port_code`（端口占用约束依赖的派生列）无 Rust 类型、无代码维护，数据库生成列承担——对照表总则 2 与枚举文档双重登记。
- **通道业务类型核对**：对照表 payment 域仅 `charge_payment_intent.status` 一列；「通道业务类型」无对应 DB 词表列（渠道分支由 `BizType` 承担，票 06 已迁），无需新增类型。
- **残留核验**：`IntentStatus` 旧名全仓清零（仅 `PaymentIntentStatus` 枚举）；`marketing`/`export` 的 `known()` 为其它域 String 新类型（票 12 范围）。
