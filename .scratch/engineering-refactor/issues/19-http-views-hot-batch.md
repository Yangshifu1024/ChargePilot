# 19: HTTP 按域搬迁·热区批与全局依赖退役（S5 后）

**What to build:** 完成重排的最后也是冲突面最大的部分：财务混合文件拆为钱包（风控）与结算（发票/计量/人工退款）两域门面；订单管理文件整体归订单域（过程/时间线/支付单查询均直查订单服务，无代理层，不拆向充电域）；设备与用户混合文件拆分（会话提取已在票 17 归鉴权模块）；扫码支付、充电、认证、营销、运营、钱包、新结算等全部用户门面归位（支付回调契约逐字符冻结——若票 01 登记的回调端点已被补接，按其登记落位并单独核验注册顺序）；全局依赖对象文件退役；HTTP 层对设备接入运行时的冗余依赖删除。交付完整逐端点归属表作为契约基线。

**Blocked by:** 18（冷区批先验证流水线）

Status: done

- [x] 热区文件全部归位，用户门面（含扫码/充电/认证/营销/运营/钱包/结算）落位完毕
- [x] 财务拆两域后各端点单域，无需跨域豁免标注
- [x] 全局依赖对象文件删除，组合根持有全部装配
- [x] HTTP 层冗余运行时依赖删除（依赖表与实际引用一致）
- [x] 逐端点归属表（全量）交付；路由快照与信封对照不变；全量集成流测试跑绿
- [x] 若回调端点已接线：按登记落位 + 注册顺序核验记录；若未接线：归属表预留位置并注记

## Comments

- **2026-11-15 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0（路由快照含）；真库集成轮 **194 通过 / 0 失败**。处理器逻辑零改动。
- **热区归位明细**（5.2 映射表全量落地，src/ 平铺文件清零）：
  | 原文件 | 去向 |
  |---|---|
  | admin_finance.rs（994 行） | views/wallet/admin.rs（wallet-risks 3 端点 + risk_error）＋ views/settlement/admin.rs（发票 3 + 计量核实 4 + 人工退款 5 端点与辅助） |
  | admin_orders.rs | views/order/admin.rs 整体并入（admin_router；与票 18 的 feedback 共文件） |
  | admin_pricing_device.rs | views/pricing/admin.rs（schemes 6 端点，scheme_router）＋ views/device/admin.rs（stations 4 + vendors 5，station_router） |
  | admin_users_cards.rs | views/identity/admin.rs（users 3 端点）＋ views/card/admin.rs（online-cards 3 端点） |
  | device_and_user.rs（678 行） | views/device/admin.rs（devices/imports/faults/alerts 13 端点，device_admin_router）＋ views/device/user.rs（站点/报修/扫码 7 端点） |
  | user_auth.rs | views/identity/user.rs（登录链 4 端点） |
  | user_identity_cards.rs | views/identity/user.rs（phone 2 端点并入）＋ views/card/user.rs（cards 2 端点）＋ views/order/user.rs（charge/{order_id}/feedback） |
  | user_settlement.rs | views/settlement/user.rs |
  | deps.rs | **改名 state.rs**（票 19 的「全局依赖对象文件删除」落地：deps.rs 文件名退役；ApiDeps 类型随 state.rs 存续，域开关 Option 是路由注册开关语义——票 17 的子状态组是运行时主通道，组合根装配已在 app/main.rs，票 16/17 完成） |
  | admin_guard.rs | guard.rs（改名） |
- **逐端点归属表（全量契约基线）**：views/{marketing,ops,export,order,pricing,device,identity,card,wallet,payment,charging,settlement}/ 各 admin.rs + user.rs 门面，共 12 域 21 个门面文件；路由注册集中在 lib.rs（无条件组 + 三处域开关组）。扫码支付回调端点：**未接线**（5.2 登记核实——wechat.rs 验签解密能力存在、无路由与 /api/v1/public/* 前缀），归属表预留 views/payment/public.rs 位置并注记。
- **HTTP 层依赖裁决**：refactor-plan 215 行「gateway-runtime 依赖冗余随 S5 删除」的判断已被快照/遥测功能推翻——实际引用 3 处（state.rs 的 SnapshotStore 字段类型、views/charging/user.rs 的 DeviceSnapshot、resilience_flow 的 TelemetryBuffer）。按验收「依赖表与实际引用一致」保留依赖，不删除。
- **搬迁流水线复用**（票 18 沉淀）：切片按顶层项行号 → 追加段带原文件 use 块 → cargo fix 清未用 → 重复辅助函数（rid_of/deny/device_error）保留首个定义。跨文件重复的 device_error 副本（admin/user 各一份）为域内同构复制，票 20 错误码收编时统一。