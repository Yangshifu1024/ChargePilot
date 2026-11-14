# 18: HTTP 按域搬迁·冷区批（S5 中）

**What to build:** 把与并行修复会话冲突面小的处理器文件先搬进「域目录 × admin/user 门面」新结构，验证搬迁流水线：定价与设备混合文件拆为两域、身份与卡混合文件拆为两域、跨四域的杂项文件按域拆分（评价归订单、审计日志与多因素归运营、方案预览归定价、设备能力归设备）、营销/管理/导出/诊断/认证五个单域文件直接归位。每文件搬迁 = 移动 + 拆分 + 路由注册并入域门面，处理器逻辑零改动。修复热区（财务、订单、设备用户、结算系）不在本票。

**Blocked by:** 02（路由快照护栏）、17（横切基座）

Status: done

- [x] 上述约 10 个文件全部归位域门面结构，逐端点归属表（本票部分）交付
- [x] 拆分后的门面只引用本域服务 + 横切件（鉴权/错误/状态/提取器），无跨域 import
- [x] 路由快照与信封对照全程不变；全量集成流测试跑绿
- [x] 搬迁流水线（移动→拆分→注册→快照核对）在本批沉淀为可复述步骤，供热区批执行

## Comments

- **2026-11-15 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0（路由快照含）；真库集成轮 **194 通过 / 0 失败**。处理器逻辑零改动（原文件内容原样搬迁，仅 router 函数改名与模块头注释）。
- **新结构**：`http-api/src/views/<域>/`（每域 mod.rs + admin/user 门面）。冷区批归位 11 个原文件：
  | 原文件 | 去向 |
  |---|---|
  | admin_marketing.rs | views/marketing/admin.rs |
  | admin_management.rs | views/ops/admin.rs（management_router） |
  | admin_auth.rs | views/ops/auth.rs（auth_router） |
  | admin_diagnostics.rs | views/ops/diagnostics.rs（diagnostics_router） |
  | admin_export.rs | views/export/admin.rs |
  | admin_misc.rs（拆四域） | views/order/admin.rs（feedback 2 端点）、views/ops/audit.rs（audit-logs + MFA×3）、views/pricing/admin.rs（schemes/preview）、views/device/admin.rs（device-capabilities） |
  | user_wallet.rs | views/wallet/user.rs（router + refund_router，后者绑 wallet 开关） |
  | user_payment.rs | views/payment/user.rs（checkout_router 绑 payment 开关 + simulation_router 绑 simulation 布尔） |
  | user_charging.rs | views/charging/user.rs（router + stop_router 绑 charging_stop 开关） |
  | user_marketing.rs | views/marketing/user.rs |
  | user_ops.rs | views/ops/user.rs |
- **跨域 import 治理**：payment/user.rs 对 wallet_error（充值桥返回 WalletError）的跨域引用消除——wallet_error 迁 errormap.rs（错误映射是横切归宿，票 20 全量收编的先行站点）；views 内其余 import 仅本域模块 + platform/httpx + crate 横切件（guard/errormap/params/deps）。
- **逐端点归属表（本票部分，契约基线）**：
  - ops 域：/admin/auth/*（auth.rs）、/admin/users* 账号/角色/范围（admin.rs）、/admin/diagnostics（diagnostics.rs）、/admin/audit-logs + /admin/admin-users/{id}/mfa + /admin/auth/mfa-settings（audit.rs）、/user/ops/announcements（user.rs）
  - marketing 域：/admin/coupons*（admin.rs）、/user/marketing/*（user.rs）
  - export 域：/admin/exports*（admin.rs）
  - order 域（本票部分）：/admin/feedback*（admin.rs；admin_orders 热区批并入同文件）
  - pricing 域（本票部分）：/admin/settings/charging-schemes/preview（admin.rs；热区批并入）
  - device 域（本票部分）：/admin/settings/device-capabilities（admin.rs；热区批并入）
  - wallet 域：/user/wallet/{balance,txns,recharges,refunds} + /user/wallet/refund（user.rs）
  - payment 域：/user/scan/{offers,start} + /user/wallet/recharge（user.rs checkout_router）、/user/development/payments/confirm（simulation_router）
  - charging 域：/user/charge/*（user.rs）、/user/charging/{order_no}/stop + /user/charge/stop（stop_router）
- **搬迁流水线（沉淀，供热区批）**：①移动——原文件内容原样迁 views/<域>/<端>.rs（crate 内引用路径不变）；②拆分——混域文件按端点区块切片，各切片自带所需 use（cargo check 后 cargo fix 清未用）；③注册——lib.rs merge 改 views 路径，域开关路由（票 17 的 if-merge）同步改路径；④快照核对——`cargo test -p chargepilot-http-api --test api_contract`（路由快照）+ 全量集成轮。孤儿 doc 注释/未用辅助函数（real_ip 等）在切片后即删。
- **未迁（热区，票 19）**：admin_finance.rs、admin_orders.rs、admin_pricing_device.rs、admin_users_cards.rs、device_and_user.rs、user_auth.rs、user_settlement.rs、user_identity_cards.rs、deps.rs→state.rs、admin_guard.rs→guard.rs。