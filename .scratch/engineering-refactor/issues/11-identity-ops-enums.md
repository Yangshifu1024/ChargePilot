# 11: identity 与 ops 域枚举化（S3f）

**What to build:** 身份与运营两域词表转正：账号状态（两套取值集合：C 端 active/frozen 与后台 active/disabled/locked）、公告状态（草稿/已发布/已过期）、订单评价类别等。注意两套账号状态是不同表的独立词表，不合并。

**Blocked by:** 05

Status: done

- [x] 两套账号状态分别枚举化（不合并），公告状态、评价类别转正
- [x] 字面量对照测试锁定；常量串容器删除
- [x] 日常 check + 全量集成轮跑绿；字面量清单快照只减不增

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **184 通过 / 0 失败**（较票 10 增 1 = identity.rs 对照测试）。clippy 全仓 0 告警。
- **交付物**：
  - `domain/src/identity.rs`（新建）：`UserStatus`(2) + `UserGender`(3)，含对照测试。gender 在 Rust 代码零写入（注册不含该列，DB DEFAULT 'unknown'），仅后台查询视图消费——视图行 `ChargeUserRow.gender/status` 枚举化（去 CAST 直读，serde 输出不变）。
  - `domain/src/operations.rs`：`ScopeType` String 新类型按对照表裁决更名转正 `AdminScopeType`(2)（new_data_scope 签名改枚举元组，未知类型由 DB 解码严格失败承接，归并函数删除兜底分支——旧版「未知静默忽略」语义收紧已登记）；新建 `AdminUserStatus`(3)、`AnnouncementScope`(3)、`AnnouncementStatus`(3)。
- **迁移站点**：identity lib/repo/service（User.status 枚举字段与 is_active）、ops management（软删/解锁/重置密码三处 SQL + AdminUserRow/AdminAccount/AdminRow 链枚举化 + 入参校验 parse 化——locked 为风控专用状态不可设置，旧版语义保留）、ops announcement（scope parse + INSERT/SELECT 绑定）、ops repo（登录准入 disabled 判定、种子账号构造）、ops scope（范围行投影）、跨模块 user/admin_user 状态：card admin（在线卡锁定读投影）、wallet risk（解冻恢复判定）、device fault（派单人 admin_user 校验）。
- **两套账号状态不合并**：C 端 `UserStatus`(active/frozen) 与后台 `AdminUserStatus`(active/disabled/locked) 独立建型，票文本裁决落实。
- **保留字符串（合法）**：`http-api/admin_users_cards.rs:136` 的状态筛选入参白名单（显式 400 语义，票 15 双语义定论按调用点保留）；公告审计 after 里的 scope/status 原串（审计语义）。
- **过程记录**：公告 INSERT 的 status 绑定漏插（9 占位符 8 绑定）被真库轮 503 抓出补齐——与票 08/10 同类错误，票 10 已登记「两步式替换」教训，本票为最后一处；identity 模块首次接入 domain 依赖。
