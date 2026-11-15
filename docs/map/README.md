# ChargePilot 功能地图

这是代码库的**导航地图**:回答「某个功能住在哪些文件、沿着什么路径运转」。地图只做导航,不定规则——架构规则以 [docs/blueprint.md](../blueprint.md) 为唯一基准,编码硬规则以 [CODING_STANDARDS.md](../../CODING_STANDARDS.md) 为准,表结构以 [backend/migrations/0001_init.sql](../../backend/migrations/0001_init.sql) 为准。

**维护约定**:地图与代码冲突时,以代码为准,并顺手修正地图(与蓝图偏离登记表同理)。每份分图头部标注覆盖范围与核对入口。

## 分图索引

| 分图 | 覆盖范围 |
|---|---|
| [01-http-api.md](01-http-api.md) | HTTP 门面层结构(lib/state/errormap/guard/params/views)+ **全部 139 个端点按域分组清单** |
| [02-charging-flow.md](02-charging-flow.md) | 充电核心链路:下单支付 → 启动命令 → 回执推进 → 遥测 → 停止 → 结束 → 计费 → 结算 → 退款(跨 crate 全时序) |
| [03-device-link.md](03-device-link.md) | 设备接入:protocol 契约、protocol-dc589 实现、gateway-runtime 各模块、事件投递 |
| [04-funds-flow.md](04-funds-flow.md) | 资金链路:钱包 / 支付意图 / 退款执行 / 发票 / 风控,锁序与幂等清单 |
| [05-admin-domains.md](05-admin-domains.md) | 支撑域:ops(后台账号/权限/审计/MFA/公告)、identity(C 端身份)、device(台账/告警/工单)、pricing、marketing、card、export |
| [06-domain-wordlist.md](06-domain-wordlist.md) | domain crate:词表宏、界限值、分页校验、decode 三方向、各域枚举清单 |
| [07-platform-infra.md](07-platform-infra.md) | platform 12 模块、events 事件层、app 组合根与装配顺序 |
| [08-testing-gates.md](08-testing-gates.md) | 测试三层、门禁清单(tasks.mjs 全任务)、真库集成、批量模拟器 |
| [09-troubleshooting.md](09-troubleshooting.md) | 排障索引:一条链路出问题时的查证路径(兜底扫描/诊断端点/日志要点) |

## crate 布局与依赖方向

```text
backend/crates/
├── domain/                # 纯规则 + 词表枚举 + 值对象(零框架)     → 06
├── platform/              # 无业务语义横切能力(12 模块)           → 07
├── storage/               # 连接池 + 迁移器                       → 07
├── protocol/              # 设备契约 trait(结构冻结 F5)           → 03
├── protocol-dc589/        # DC589 协议唯一实现                    → 03
├── events/                # domain_event 事务内入队 + 异步投递     → 07
├── gateway-runtime/       # TCP 接入运行时(会话/遥测/事件)        → 03
├── modules/<12 域>/       # 业务域 crate(lib/error/service/repo)  → 02 04 05
├── http-api/              # 聚合 API 薄层(views/<域>/ 双门面)     → 01
├── assets-admin/ h5/      # rust-embed 前端叶子 crate             → 07
└── app/                   # 组合根(装配 + serve/migrate/simulator)→ 07
```

依赖规则(编译器强制,详见蓝图第四章):`domain` 零依赖;`modules/<域>` → domain + platform + events + 显式列出的其它 modules(Cargo.toml 即依赖清单,跨域依赖须有业务理由注释);`http-api` → 用到的 modules + platform + gateway-runtime(仅 SnapshotStore,偏离登记 #1);`app` → 一切。

## 按任务找图

| 你要做的事 | 先读 |
|---|---|
| 加/改一个 HTTP 端点 | 01(路由与门面结构、条件装配、errormap 映射) |
| 改充电启动/停止/回执逻辑 | 02(链路时序)+ 03(帧与事件入口) |
| 改设备命令下发 / 会话 / 遥测 | 03 |
| 改支付 / 钱包 / 退款 / 发票 | 04(资金纪律:锁序、幂等表是红线) |
| 改后台账号 / 权限 / 审计 / MFA | 05(ops 节)+ 01(鉴权守卫) |
| 新增一个状态词表枚举 | 06(宏范式与 sqlx 三件套,编码前必读 CODING_STANDARDS) |
| 加环境变量 / 改装配 | 07(Config 冻结清单 F9、组合根顺序) |
| 跑测试 / 扩门禁 | 08 |
| 线上链路卡住 / 想看兜底扫描状态 | 09 |

## 一图速览:三层测试与两条主干

- **两条主干链路**:①设备帧 → gateway-runtime → events → 域用例(上行);②HTTP → views 门面 → 域 service → repo → MySQL(下行)。资金语义只在域内事务里推进。
- **事件是跨域粘合剂**:业务事务内 `enqueue::in_tx` 写 `domain_event`(at-least-once),runtime 1s 轮询按 `EventKind` 路由到注册的 handler,失败指数退避,兜底扫描推滞者。
