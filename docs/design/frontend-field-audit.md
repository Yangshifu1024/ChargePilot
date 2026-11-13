# 前端响应字段消费审计

> 阶段 00 评审基准 · 生成方法：OpenAPI + 前端源码 grep 核对 · 日期 2026-11-14

审计对象：`chargepilot` 仓库后端对外 HTTP 端点的响应 DTO 字段与两个前端（`frontend/admin-web` React 运营后台、`frontend/miniprogram` Taro 用户端）的实际消费情况。目的：配合 Rust 重写，识别从未被前端读取的响应字段，给出候选删除、保留白名单与格式统一建议。

---

## 1. 审计范围与方法

**响应字段真相源**（两层）：

1. `backend/internal/http/user/user.openapi.json`（40 个端点、72 个 schema）与 `backend/internal/http/auth/admin.openapi.json`（89 个端点、88 个 schema）。user 规范完整；admin 规范有 41 个端点仅有响应描述、缺 schema。
2. 对上述 41 个 admin 端点，从 Go 源码逐个追溯：路由注册（`internal/http/admin/<业务族>/api.go`）→ handler 函数体（AST 提取）→ `httpapi.OK(c, X)` 参数 → use case 服务方法返回类型 → wire 结构体 json tag（或 `[]map[string]any` 的 SELECT 列清单）。共补充 285 条字段记录、20 个 wire 类型。

**消费判定方法**：对每个响应字段 json 名（snake_case 原名），在 `frontend/admin-web/src` 与 `frontend/miniprogram/src` 全部 148 个源文件（ts/tsx/js/jsx）做词边界全文匹配。命中分两档：

- **已消费**：任一文件中作为标识符出现（解构、点号访问、字符串键、模板绑定均算）；
- **仅类型声明**：命中行匹配 `name?: <类型>` 声明模式（TS interface/type 成员行），值从未被读取；
- **零消费**：全仓 grep 无任何命中。

统一响应包装 `{code, message, data, request_id, trace_id}` 由前端请求层统一解包（admin-web `src/api/client.ts` 的 `ApiEnvelope`，读 code/message/data/request_id；miniprogram `src/runtime/session.js`，读 code/message/data），不逐端点重复计入业务字段。

**边界声明**：

- 按字段名匹配意味着跨端点同名字段共享命中结果；「零消费」判定（无命中）不受此影响，无假阳性；「已消费」不逐条归因到端点。
- 未检测运行时动态构造的键访问（如 `row[dynamicKey]`）；审计中抽查了 Dashboard、Exports、WalletRisks、Casework、订单详情、扫码结果等页面，未发现此类消费。
- OpenAPI 有 schema 的端点抽查了 devices 族，与 Go 源码 `device.Device` json tag 一致；其余按规范为准。

## 2. 总体统计

| 指标 | 数值 |
| --- | --- |
| 端点总数 | **129**（user 40，admin 89；admin 含 1 个文件流下载端点） |
| 响应 DTO/schema 总数 | OpenAPI 被 2xx 响应引用的 schema **116** 个 + Go 源码补充 wire 类型 **18** 个 ≈ **134**（另有 14 处 `gin.H` 内联结构未单计） |
| 响应字段记录（端点 × 字段，含嵌套展开） | **1296** |
| 唯一 json 字段名 | **340** |
| 零消费字段记录 | **102 条（7.9%）** |
| 零消费唯一字段名 | **54 个（15.9%）**＝全仓无命中 41 ＋ 仅类型声明 13 |
| 涉及零消费字段的端点 | 52 个 |

## 3. 清单 A：零消费字段清单（候选删除）

证据列说明：`无命中`＝全仓 grep 无任何命中；`仅类型声明`＝只出现在 TS 类型声明行（括号内为命中位置）。标记列：`候选删除`＝无明确保留理由；`待评审`＝疑似预留展示、回执语义或涉及第二消费者，删除前需人工确认。

### A-1 admin 端（30 个字段名）

| 字段名 | 端点 | DTO | 证据 | 标记 |
| --- | --- | --- | --- | --- |
| `attempts` | GET/POST `/admin/device-imports`、POST `/{import_id}/retry` | ImportJob | 仅类型声明（`admin-web/src/pages/Refunds.tsx:11` 的命中属退款任务 `task.attempts`，非本字段） | 候选删除 |
| `biz_id` | GET `/admin/billing/invoices` | AdminInvoiceRow | 仅类型声明（`Billing.tsx:19`） | 待评审（关联业务单据 ID） |
| `gender` | GET `/admin/users` | ChargeUserRow | 仅类型声明（`pages/chargeUsers/ChargeUserProfileDrawer.tsx:19`） | 待评审（用户档案展示） |
| `locked_until` | GET `/admin/admin-users` | AdminUserRow | 仅类型声明（`Users.tsx:25`） | 待评审（账号锁定状态展示） |
| `requested_by` | GET `/admin/exports`、GET `/{id}` | ExportTaskRow | 仅类型声明（`Exports.tsx:17`） | 待评审（发起人展示） |
| `user_visible` | GET `/admin/device-fault-reports/{id}/history` | FaultEventRow | 仅类型声明（`Casework.tsx:48`） | 候选删除（内部可见性过滤标记） |
| `wallet_active` | POST `/admin/billing/wallet-risks/{request_id}/release` | 解冻响应 map | 仅类型声明（`WalletRisks.tsx:17`） | 待评审（解冻后钱包状态回执） |
| `deleted_at` | GET `/admin/device-fault-reports` | FaultReportRow（`SELECT f.*` 带出） | 无命中 | 候选删除（内部软删除列，建议 SELECT 显式排除） |
| `enabled_at` | GET/POST/PUT `/admin/vendors`、GET `/admin/vendor-options` | Vendor | 无命中 | 候选删除 |
| `filter_json` | GET `/admin/exports`、GET `/{id}` | ExportTaskRow | 无命中 | 候选删除（筛选条件原文，调试用途） |
| `freeze_id` | GET `/admin/billing/wallet-risks` | WalletRiskRow | 无命中 | 候选删除（内部冻结关联 ID） |
| `review_json` | GET `/admin/billing/wallet-risks` | WalletRiskRow | 无命中 | 候选删除（审核回执 JSON 快照副本，审计已在独立表） |
| `release_json` | GET `/admin/billing/wallet-risks` | WalletRiskRow | 无命中 | 候选删除（同上） |
| `trigger_rule` | GET `/admin/billing/wallet-risks` | WalletRiskRow | 无命中 | 待评审（风控触发规则，运营展示可能） |
| `granted` | GET/POST/PUT `/admin/coupon-activities` | ActivityRule | 无命中 | 待评审 |
| `original` | GET `/admin/billing/meter-reviews` | MeterReview.reviews[] | 无命中 | 待评审（原始值对照，计量核实展示可能） |
| `resolved_at` | GET `/admin/device-fault-reports` | FaultReportRow | 无命中 | 待评审（处理完成时间，工单展示常见需求） |
| `replied_at` | GET `/admin/feedback` | FeedbackRow | 无命中 | 待评审（回复时间） |
| `replied_by` | GET `/admin/feedback` | FeedbackRow | 无命中 | 待评审（回复人） |
| `today_completed_orders` | GET `/admin/dashboard` | Metrics | 无命中 | 待评审（Dashboard 实际展示 today_orders / today_charging_users / today_settled_cents / active_alerts / total_users） |
| `today_order_users` | GET `/admin/dashboard` | Metrics | 无命中 | 待评审（同上） |
| `permissions_locked` | PUT `/admin/roles/{id}` | RoleUpdateResult | 无命中 | 候选删除（行内 `is_builtin` 已表达锁定语义） |
| `charge_mode` | GET `/admin/devices`、GET/PUT `/admin/devices/{id}` | Device | 无命中 | 待评审（设备能力展示；同族 reports_energy / reports_segmented_power 已消费） |
| `bound_at` | GET `/admin/online-cards` | OnlineCardRow | 无命中 | 待评审（绑卡时间） |
| `deleted` | DELETE `/admin/admin-users/{id}`、DELETE `/admin/roles/{id}` | 删除回执 | 无命中 | 待评审（删除确认回执，前端删后直接刷新列表） |
| `unlocked` | POST `/admin/admin-users/{id}/unlock` | 解锁回执 | 无命中 | 待评审（解锁确认回执） |
| `revoked` | POST `/admin/auth/logout` | Revoked | 无命中 | 待评审（注销回执） |
| `scheduled` | POST `/admin/billing/refunds/{refund_no}/retry` | 重试回执 | 无命中 | 待评审（重试受理回执） |
| `remaining_kwh` | GET `/admin/orders/{id}/process` | ChargeProcessPoint | 仅类型声明（`admin-web/src/pages/orders/powerCurve.ts:6`） | 待评审（充电进度展示） |
| `remaining_seconds` | GET `/admin/orders/{id}/process` | ChargeProcessPoint | 仅类型声明（`powerCurve.ts:8`） | 待评审（同上） |

### A-2 user 端（17 个字段名）

| 字段名 | 端点 | DTO | 证据 | 标记 |
| --- | --- | --- | --- | --- |
| `logged_out` | POST `/api/v1/user/auth/logout` | LogoutResult | 无命中 | 待评审（幂等注销回执） |
| `command_id` | POST `/api/v1/user/charge/stop` | ChargeStopResult | 无命中 | 待评审（停机命令关联 ID，证据链） |
| `stopped` | POST `/api/v1/user/charge/stop` | ChargeStopResult | 无命中 | 待评审（停止受理回执） |
| `device_name` | POST `/api/v1/user/scan/resolve`、`/scan/port` | ScanResolveResult | 无命中 | 待评审（扫码结果页实际展示 device_id / device_status） |
| `intent_id` | POST `/api/v1/user/scan/start` | ScanStartResult | 无命中 | 待评审（支付意图关联 ID） |
| `coupon_grant_id` | POST `/api/v1/user/scan/start`；POST `/admin/coupons/{id}/grants` | ScanStartResult / GrantResult | 无命中 | 待评审（发放记录主键，幂等凭据） |
| `discount_cents` | GET `/user/charge/history`、`/{order_no}`、`/ongoing/snapshot`；POST `/scan/start` | ChargeOrderRow / ChargeOrderDetail / ChargeSnapshot / ScanStartResult | 无命中 | 待评审（优惠金额；订单详情页确认未展示，涉及用户侧资费透明） |
| `shortfall_cents` | GET `/user/charge/{order_no}`、`/ongoing/snapshot` | ChargeOrderDetail / ChargeSnapshot | 无命中 | 待评审（余额不足缺口，充电中提示预留） |
| `tariff` | GET `/user/charge/{order_no}`、`/ongoing/snapshot` | ChargeOrderDetail / ChargeSnapshot | 无命中 | 待评审（冻结资费文案） |
| `billing_cutoff` | GET `/user/charge/{order_no}`、`/ongoing/snapshot` | ChargeOrderDetail / ChargeSnapshot | 无命中 | 待评审（计费截止冻结证据） |
| `telemetry_at` | GET `/user/charge/ongoing/snapshot` | ChargeSnapshot | 无命中 | 待评审（遥测新鲜度展示） |
| `boundary_approximate` | GET `/user/charge/{order_no}/curve` | ChargeHistoricalCurveResponse | 无命中 | 待评审（曲线端点近似标志） |
| `available_offer_count` | GET `/user/station/{id}` | StationDetailResult | 无命中 | 待评审（站点可用优惠角标预留） |
| `usable` | GET `/user/coupon/my` | MyCouponRow | 无命中 | 待评审（后端已推导的可用标志；前端现以 expired_at/status 自行推导，存在口径漂移风险，建议消费或删除二选一） |
| `phone_masked` | POST `/user/phone/bind` | PhoneBindResult | 无命中 | 待评审（绑定成功回执掩码号；前端也可自行掩码） |
| `is_new_user` | POST `/user/auth/login` | LoginResult | 仅类型声明（`miniprogram/src/runtime/models.ts:10`） | 待评审（新用户引导流程标志） |
| `coupon_unused_count` | GET `/user/profile` | ProfileResult | 仅类型声明（`runtime/models.ts:20`） | 待评审（我的页面券角标） |

## 4. 清单 B：保留白名单

零消费但建议保留的字段，共 7 个。判定依据：存在前端之外的第二消费者、对外协议契约或明确的前端缺陷（应消费而未消费）。

| 字段名 | 端点 | 保留理由 | 依据位置 |
| --- | --- | --- | --- |
| `trace_id` | 全部端点的统一响应包装 | 统一响应包装契约的组成部分（`code/message/data/request_id/trace_id`），服务间链路排障标识；Rust 重写若改动包装契约将影响所有端点 | `httpapi` 统一响应中间件；admin-web `ApiEnvelope` 已声明 request_id 但未读 trace_id |
| `access_token` | POST `/admin/auth/login`、`/auth/refresh`、`/auth/mfa` | 与 `token` 同值的兼容别名（源码注释「同一访问签名」），为兼容既有客户端保留；与 `token`（已消费）构成双字段契约 | `backend/internal/ops/session.go:101-102` |
| `jwt` | POST `/user/auth/login`、`/auth/refresh` | OpenAPI 明确注释「与 token 同值，为兼容既有客户端保留」；miniprogram 类型声明存在（`runtime/models.ts:4`）但值未读 | `user.openapi.json` LoginResult 描述 |
| `jwt_expires_in` | POST `/user/auth/login`、`/auth/refresh` | 同上配套的过期秒数字段，兼容契约一部分 | 同上 |
| `appId` | POST `/user/scan/start`（PaymentParams） | 微信小程序调起支付参数集（appId/timeStamp/nonceStr/package/signType/paySign）的标准组成；小程序运行时自动携带 appId 故前端未显式读取，但后端回执需保持渠道协议完整 | `backend/internal/payment/channel_wechat.go:39`；`miniprogram/src/utils/payment.js` 校验其余 5 个字段 |
| `file_format` | GET `/admin/exports`、GET `/{id}` | **前端缺陷线索，应消费而未消费**：`Exports.tsx:111` 下载名硬编码 `row.task_no + '.csv'`，导出 xlsx 时下载文件名错误。Rust 重写时此字段必须保留且前端应改用 | `admin-web/src/pages/Exports.tsx:111` |
| `formats` | GET `/admin/exports/resources` | 可选格式清单（csv/xlsx），前端在创建导出表单中硬编码格式选项；应改为消费该清单动态渲染。同上属应消费未消费 | `admin-web/src/pages/Exports.tsx` 创建表单 |

## 5. 清单 C：分页/排序/时间格式不一致清单

### C-1 分页参数风格

| 风格 | 端点范围 | 参数 | 响应包装 |
| --- | --- | --- | --- |
| 页码式 | user 全部 10 个列表端点（wallet/txns、wallet/recharges、wallet/refunds、station/nearby、coupon/my、announcement/list、invoice/my、device/fault-reports、charge/history、bills） | `page` + `page_size`（nearby 另有 longitude/latitude/radius_km） | `items/total/page/page_size`（+端点专属字段，如 wallet/txns 附带 `user_id`） |
| 页码式 | admin 全部列表端点（统一经 `ReadAdminPage`） | `page` + `page_size` + `keyword` + `status`（状态须落在各端点白名单） | `persist.Page<T>`：`items/total/page/page_size/permissions(omitempty)` |
| 游标式 | 仅 admin `GET /orders/{id}/process`（充电过程点位轮询） | `after_id` + `limit` | 点位数组（非分页包装） |

差异点与统一建议：

1. **页大小上限不一致**：admin 上限 `MaxAdminPageLimit = 1,000,000`（`httpapi/gin.go:105`），C 端使用独立的较小默认（`DefaultPageLimit`，默认页大小 20）。百万级上限会使「分页」退化为全量拉取，Rust 重写建议统一为两者一致的合理上限（如 200），全量场景走导出。
2. **游标与页码并存**：仅充电过程点位用 `after_id/limit`，其余全部页码。轮询型只增数据（点位、遥测）适合保留游标；管理列表保持页码。建议将风格差异收敛为「管理列表＝页码、时序轮询＝游标」并在 API 规范中写明，避免第三种风格出现。
3. **响应包装字段增删不齐**：`persist.Page` 的 `permissions` 为 omitempty（admin 站点等端点返回，其余省略）；user 的 `WalletTxnPageResponse` 额外带 `user_id`。建议 Rust 重写统一为 `items/total/page/page_size` 核心四字段，权限等扩展字段移入独立键或统一返回。
4. **排序**：各列表均为后端固定排序（`id DESC` 或 `created_at DESC`），无排序请求参数，前端不可自定义——现状一致，无迁移负担；若 Rust 重写增加排序参数应作为新契约评审。

### C-2 时间字段格式

主流格式：`time.Time` 经 JSON 序列化为 **RFC3339（UTC，`string: date-time`）**，绝大多数 `*_at` 字段属此类。不一致点如下：

| 字段 | 端点/DTO | 实际格式 | 问题与建议 |
| --- | --- | --- | --- |
| `created_at`、`completed_at`、`expires_at` | GET `/admin/exports`、`/{id}`（ExportTaskRow，Go 类型 string/*string） | MySQL `datetime(3)` 驱动原样文本 `YYYY-MM-DD HH:MM:SS.mmm` | 与 RFC3339 不一致（无 T/Z）。Rust 重写建议统一输出 RFC3339 |
| 发票/退款审核列表时间列 | GET `/admin/billing/invoices`、`/billing/refunds`（`refund_review_list_repo.go:49` 注释「保留数据库时间文本」） | MySQL datetime 原样文本 | 同上 |
| `cutoff_at` | GET `/user/charge/{order_no}`、`/ongoing/snapshot` | 「首次冻结截止的原数据库时刻文本」 | 同上；且该字段本身在零消费清单（A-2），可一并处理 |
| `last_login_at`、`last_order_at` | GET `/admin/users`（ChargeUserRow，*string） | RFC3339 文本（Go 侧显式 `time.RFC3339` 格式化） | 格式值一致但**类型异构**（string 而非 date-time 声明），OpenAPI 亦未标 format；建议统一为 date-time 类型 |
| `locked_until` | GET `/admin/admin/users`（AdminUserRow，*string） | 文本（未标 format） | 同上；该字段同时在零消费清单 |
| `timeStamp` | POST `/user/scan/start`（PaymentParams） | 渠道签名的**秒级时间戳文本** | 微信支付协议固有格式，保留，不参与统一 |
| `jwt_expires_in` | user 登录/刷新 | 相对**秒数**（非时间戳） | 协议语义不同，保留 |
| dashboard 趋势 `day` | GET `/admin/dashboard` | `DATE_FORMAT(..., '%Y-%m-%d')` 且 SQL 中硬编码 `+8小时` 时区偏移 | 时区硬编码在后端 SQL，Rust 重写应显式以配置注入或约定 UTC 日期键 |
| `warranty_until`、`expected_updated_at`/`updated_at` | admin Device 族 | RFC3339，但语义为「资料版本号」（UTC 毫秒比较的乐观锁） | 建议文档明示该字段的双重语义（时间＋版本），避免重写时误当纯时间处理 |

统一建议：**对外响应时间一律 RFC3339 UTC（date-time）**；渠道协议字段（timeStamp）与相对时长（jwt_expires_in）显式豁免；数据库文本格式字段（导出、审核列表、cutoff_at）在 Rust 重写时收口为 RFC3339，前端（admin-web `utils/time.ts`、miniprogram 各 utils）需同步适配两种解析路径的现状。

## 6. 附录：证据与复现方法

- 字段提取：解析两份 OpenAPI（展开 `$ref`/`oneOf`/嵌套对象至 5 层），user 40 端点全量、admin 48 端点取自规范 schema；admin 其余 41 端点经 Go AST 提取 handler 后逐个追溯 wire 类型与 SELECT 列。
- 消费核对：对 340 个唯一字段名在两前端 src 全文做 `\b<name>\b` 匹配，命中行含文件、行号与原文；类型声明行启发式：`/^\s*[A-Za-z_][A-Za-z0-9_]*\??\s*:\s*[^=()]*$/` 且含类型关键字。
- 关键抽查记录：`Dashboard.tsx` 指标消费（today_orders 等 5 项）；`Exports.tsx:111` 硬编码 `.csv` 下载名；miniprogram 订单详情页（`pages/charge/detail.controller.js`）读取 order_no/amount_cents/hold_expires_at/bucket_start/device_id 等，未读 discount_cents/tariff/billing_cutoff/shortfall_cents；`runtime/session.js:84` 统一解包 code/message/data。
- 本表为阶段 00 评审基准；后端或前端改动后应重新生成，不随业务代码维护更新。
