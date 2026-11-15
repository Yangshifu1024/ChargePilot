# 06 · domain 词表与类型纪律

> 覆盖:`backend/crates/domain/`(纯规则 + 词表枚举 + 值对象,零框架依赖——不出现 axum/sqlx/redis 类型)。
> 规则归属:类型系统纪律(蓝图第六章)与 decode 三方向(F10)是定案;sqlx 类型映射硬规则见 CODING_STANDARDS.md——**编码前必读**。

## 1. wordlist_enum! 宏(wordlist.rs)

一切有限状态词表用 `wordlist_enum!` 定义,禁止裸字符串比较。宏生成三件套:

- `as_str()`——与 migrations/0001_init.sql 的 ENUM 字面量**逐字节一致**;
- `parse()`——字符串转枚举;
- `compatible()`——sqlx 0.8.6 兼容判定(派生强枚举的 `type_info()` 与 MySQL 结果集报告的 `ColumnType::Enum` 永不相等,须委托 `<str>::compatible`;这是宏**手写 sqlx 三件套**的原因)。

**decode 三方向(F10)**:

| 方向 | 行为 |
|---|---|
| DB → Rust | 严格失败——未知值即 schema 漂移报警 |
| 设备/外部上报 | 保留原值透传(协议翻译在 protocol-dc589 边界) |
| API 入参 | 解析失败 400 |

**新增词表的动作**:落在所属域文件 + 宏生成 + 对照 0001_init.sql 列声明;跨域共用的资金词表放 domain/money。

## 2. 顶层配套

| 文件 | 职责 |
|---|---|
| limits.rs | 业务界限值单一真相源,每条常量注明旧版出处;分页双轨(C 端 1..=100 / 后台更大上界)是有意设计 |
| page.rs | 分页参数校验唯一入口(PageRequest);http-api 的 params.rs 双轨提取器内部调它,**不要在 handler 手写界值** |
| telemetry.rs | 遥测类型(与 gateway-runtime 遥测管道配套) |
| money/ | 资金词表与工具:PaymentOrderStatus、RefundStatus、PayMethod、BizType 等 + 复核校验与重放比对 |

## 3. 各域词表枚举清单(按子目录)

| 域子目录 | 代表枚举 |
|---|---|
| charging/(10 文件) | ChargeCommandStatus、ChargeStopCommandStatus、BillingError、ChargeExecutor、ServerBasis、Channel |
| money/ | PaymentOrderStatus、ChargePaymentStatus、RefundStatus、PayMethod、ExecutionPolicy、BillStatus、BizType |
| wallet/ | AccountStatus、FreezeStatus、TxnBizType、TxnDirection |
| card/ | CardStatus、CardOperationKind、CardOperationStatus、CardDeliveryStatus |
| order/ | ChargeOrderStatus、FeedbackStatus、FeedbackCategory |
| payment/ | PaymentIntentStatus(与 money::PaymentOrderStatus 分立——共享 initiated/paid 字面量但含义不同,勿合并) |
| settlement/ | BillingJobStatus、InvoiceType、ManualFeeReviewStatus、MeterReviewStatus、InvoiceRequestReviewStatus、InvoiceAdminReviewStatus |
| operations/ | AdminUserStatus、AnnouncementScope、AnnouncementStatus、AdminScopeType;结构 DataScope、FieldMask |
| marketing/ | CouponStatus、DiscountType、ActivityStatus、ActivityTriggerType、GrantSource、GrantStatus |
| device/(词表最多) | DeviceStatus、DevicePortStatus、StationStatus、VendorProtocol、VendorStatus、AlertSeverity、AlertStatus、FaultType、FaultReportSource、FaultReportStatus、FaultReportEventType、DeviceImportStatus、DeviceMetaStatus、SessionProtocol |
| pricing/ | PricingTemplateStatus、PricingRuleStatus、PricingChannel |
| export/ | ExportStatus |
| identity/ | UserStatus、UserGender |

**词表纪律实例**:`money::BizType`(charge / wallet_recharge)与钱包流水 `wallet::TxnBizType` 是两组不同词表,禁止并集——它们语义不同(支付单业务类型 vs 流水业务类型)。

## 4. 纯规则函数的落位惯例

- 结算计算:`domain::charging::settle_session`(纯函数,被 settlement::billing 与差分验证测试共用);
- 退款草稿:`domain::money::RefundDraft`(含月首换算等工具);
- 分段比对:`domain::charging::actual::same_segments`(结束回执幂等比对)。

可提纯的判定应尽量放 domain(零 IO,可毫秒级单测)——这是资金语义快速回归的落点。

## 5. 改动词表的固定动作

1. 确认 0001_init.sql 的 ENUM 定义已同步(表结构真相源);改 ENUM 是 DDL,走迁移(F2);
2. 域文件内 `wordlist_enum!` 落型;
3. `grep` 全消费点迁移(编译器穷尽分派会逼出 match 缺臂);
4. `node tasks.mjs check`(api-contract 会核对错误码与字段)+ 涉 decode 路径加集成非 NULL 行(CODING_STANDARDS 规则②)。
