# 05 · 支撑域(ops / identity / device / pricing / marketing / card / export)

> 覆盖:充电与资金主链路之外的七个域 crate。统一内部模板:`lib.rs`(门面)+ `error.rs`(thiserror)+ `service/`(业务用例)+ `repo/`(SQL 只在此)。
> HTTP 端点清单见 [01-http-api.md](01-http-api.md);本文讲每个域管什么、怎么组织。

## ops — 后台运营(依赖图唯一根,零跨域依赖)

管:后台账号、角色权限、审计、MFA、公告、仪表盘。表:admin_user、role、role_permission、audit_log、announcement(读侧另有 admin_data_scope、admin_field_mask)。

| service 文件 | 职责 |
|---|---|
| service.rs | 后台会话编排——登录(密码 → 可选 MFA 挑战)→ 签发;刷新;注销;**实时权限每次请求从库读**(JWT admin kind 8 小时,内带 sid) |
| session.rs | 后台会话(`ART_`/`admin:session:` 前缀,7 天 + Lua 轮换保留旧摘要)、MFA 挑战(5 分钟)、登录频次(60 秒窗口)的 Redis 存取 |
| management.rs | 账号与角色管理——锁序「角色行→操作者→目标行」;最后管理员保护;越权校验锁内复核 |
| mfa.rs | MFA 本人设置(enrol/confirm/disable)+ 管理端重置;登记材料只存 5 分钟缓存 |
| announcement.rs | 公告发布(创建即 published,无草稿)与 C 端查询(global 生效窗口内) |

顶级模块 `perms.rs`:后台权限码词表,profile.permissions 的单一真相源(http-api 各 handler 的 deny 检查引用它)。

跨域消费点:settlement/wallet/card 等域写审计时调用 `ops` 的审计原语(`append_audit_tx`、`load_profile_tx`)。

## identity — C 端身份

管:登录即建户(与零余额钱包同事务)、会话、手机号。表:user(写入权)。

| service 文件 | 职责 |
|---|---|
| service.rs | 会话服务编排——登录/刷新/注销/profile;错误到 HTTP 的映射在 errormap |
| session.rs | C 端会话存储(Redis `{uid, openid, digest, previous[]}`,7 天绝对寿命 + Lua 轮换保留 32 旧摘要) |
| phone.rs | 手机号绑定/解绑——development 模式收明文、wechat 模式只收授权 code;uk_phone 仲裁并发 |
| wechat.rs | 微信身份交换——jscode2session 换 openid;development 模式本地换购(`dev:` 前缀) |

登录方式由 `LoginMode` 配置枚举(platform::config)决定;C 端令牌 JWT user kind 15 分钟。

## device — 站点与设备台账

管:站点、设备、厂商、批量导入、故障工单、告警。表:station、device、device_port、device_meta、vendor、device_provision、device_import、device_import_identity、alert_event、device_fault_report、device_fault_report_event。
注意:设备**运行时**表(device_session、telemetry 等)写权属 gateway-runtime,本域只写台账表。

| service 文件 | 职责 |
|---|---|
| station.rs | 站点 CRUD(校验 → 事务内创建或锁行更新 → 审计) |
| management.rs | 设备台账——列表/详情/资料更新(乐观锁)/启停(enabled|disabled 可写子集);运行态合并(心跳/信号/端口) |
| vendor.rs | 厂商管理——编码不可变、有设备时协议不可改、当前仅支持 dc589+TCP |
| import.rs | 批量导入——受理(幂等登记 + 身份核对 + 兼容检查)→ 执行(provision + 台账落库)同事务塌缩 |
| scan.rs | 匿名扫码解析——端口编码 → 设备端口列表 → 中央运营状态补可用性;不创建支付或会话 |
| fault.rs | 故障工单——用户提交/查询 + 后台派单/结案(open→dispatched→fixed→closed) |
| alert.rs | 告警——故障事件 → 告警生成/合并/恢复判定(心跳水位防乱序重放);设备事件链同步入口(record_fault / resolve_recovered_for_heartbeat) |

repo 另有 `user_station.rs`(C 端站点详情聚合)与 `capabilities.rs`(设备能力清单)。

## pricing — 计费方案

管:三层模型(模板 → 发布规则 → 生效匹配)。表:pricing_template、pricing_rule、pricing_publication。

| service 文件 | 职责 |
|---|---|
| templates.rs | 模板 CRUD——单事务「锁模板行→版本核对→写入→审计」,更新版本 +1 |
| publication.rs | 发布与继承——发布幂等(request_id + payload_hash + 同操作者);站点行锁串行化;停旧插新(version=expected+1);继承 = 删除设备自有覆盖 |

试算:`POST /admin/settings/charging-schemes/preview`;真正的结算计算在 `domain::charging::settle_session`(纯函数,见 02 阶段五)。

## marketing — 优惠券

管:券模板、人工/活动发放与核销。表:coupon、coupon_activity_rule、coupon_grant、coupon_grant_request、coupon_redemption。

| service 文件 | 职责 |
|---|---|
| coupon.rs | 模板与人工发放——权限核对、校验与发放编排(锁序:请求回执→用户行→券模板行→发放行计数) |
| activity.rs | 活动规则管理与事件触发的幂等发放——发放号 = sha256(规则+用户+事件键) 前 16 字节 hex,重试不产生新券 |

核销发生在下单事务内(payment::checkout / card::swipe 读 coupon_redemption);本域不直接改订单。

## card — 在线卡

管:在线卡绑定、刷卡扣款、续购与操作幂等。表:online_card、card_operation、card_charge、online_card_audit、card_event_delivery。
锁序:卡 → 钱包 → 会话 → 订单。刷卡准入是设备刷卡事件的**同步应答**(拒绝统一 409/2009 语义),见 03 链路图 B6。

| service 文件 | 职责 |
|---|---|
| swipe.rs | 刷卡准入与扣款——幂等恢复(uk_device_event)→ 30 秒时效 → 端口解析 → 设备条件 → 事务内扣款 + 订单/意图/快照写入 + 出站事件;首购出站 charge_payment_confirmed |
| admin.rs | 后台在线卡管理——列表、核验绑定、状态变更(状态写入与审计同事务) |
| user_card.rs | 用户侧——列表、挂失(lost)/解绑(unbound);解绑即从用户视角消失 |

设备链专用门面:`balance_facts`(余额事实,供拒绝帧显示)、`card_charge_port_release_tx`、`confirm_start_operation_tx`、`fail_start_operation_tx`(充电回执事务内调用)。

## export — 批量导出

管:导出任务、文件生成与发布、下载校验、到期清理。表:export_task。

| 模块 | 职责 |
|---|---|
| service/tasks.rs | 受理(同事务登记 + **锁定权限核对**(auth_version 变化即凭据失效)+ 数据范围/脱敏 FOR SHARE 读取)、列表/详情、下载核对 |
| service/cleanup.rs | 到期清理(受管理循环,每小时,每批 ≤1000,标 expired) |
| artifact.rs | 文件生成与发布(先写 .partial 再改名);下载核对任务、范围、脱敏与 SHA-256 |
| registry.rs | 可导出资源注册表——只有登记过的资源允许导出;账单只允许全局范围 |

新增可导出资源:在 registry.rs 登记 ExportSpec(列清单 + 数据范围约束),不新写文件管线。

## 快速对照:谁在什么时候被谁调用

- 设备事件链(异步):device::alert(故障/恢复)、card::swipe(刷卡)、charging::receipts(回执)——入口都在 gateway-runtime::device_events。
- HTTP 门面(同步):每域两个薄门面 views/<域>/{admin,user},见 01。
- 事件出站(事务内):identity(无)、card::swipe(charge_payment_confirmed)、charging::receipts(charge_start_failed/charge_ended 同事务)、settlement::billing(charge_settled)、settlement::refund_executor(refund_completed)。
