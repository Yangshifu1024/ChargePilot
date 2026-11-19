# 08 · 测试与门禁

> 覆盖:三层测试、`node tasks.mjs` 全部任务、真库集成、契约对账、压测模拟器。
> 规则归属:门禁「任何提交前四项全绿」与「涉及 SQL/枚举/锁/幂等的改动追加集成轮」是蓝图第七章定案;CODING_STANDARDS 两条硬规则(类型映射 / 非 NULL 测试行)在涉库改动时必读。

## 1. 常用命令速查

| 命令 | 做什么 |
|---|---|
| `node tasks.mjs check` | **提交前必跑**:fmt-check + check-sql + clippy + test + api-contract |
| `node tasks.mjs test` | 单测(workspace,不含 #[ignore] 集成) |
| `node tasks.mjs build` | cargo build --workspace |
| `node tasks.mjs test-integration [fresh]` | 真库集成(需 dev 容器);`fresh` 先 DROP+CREATE 重建开发库 |
| `node tasks.mjs check-sql` | SQL 层审计:12 域 service/ 零 SQL + gateway-runtime/device_events.rs 零 SQL |
| `node tasks.mjs fmt` / `fmt-check` | rustfmt 全仓 |
| `node tasks.mjs clippy` | workspace lints 警告即错误 + unwrap/expect 禁令 |
| `node tasks.mjs api-contract` | 前后端调用点↔路由↔必填字段三段对账(scripts/ops/check-api-contract.py) |
| `node tasks.mjs debug` | 一键开发环境:dev 容器 + 后端 `serve` + admin/H5 Vite;Ctrl-C 统一停止(`--no-admin`/`--no-h5`/`--no-infra`/`--fresh`/`--clean`) |

dev 容器:`chargepilot-dev-mysql`(127.0.0.1:3307)、`chargepilot-dev-redis`(127.0.0.1:6380),TEST_MYSQL_URL 门控集成轮。`debug` 会复用已存在的同名容器,缺省拉起后端 8080/9100、后台 5173、H5 5174,注入随机 JWT 与引导账号 `admin`/`admin-dev-pass-1234`(均可用环境变量覆盖);容器默认保留,`--clean` 退出时移除。

## 2. 三层测试(各有职责,不互相替代)

### ① 纯层(毫秒级,常跑)

- 领域规则单测:结算差分验证(settle_session)、资金快照比对、错误码表冻结断言(error::tests::code_values_are_frozen)、httpx 信封冻结码断言;
- DC589 帧测试:黄金样本与边界坏帧(见第 4 节);
- SessionRegistry 并发互踢测试。
- 现状缺口:资金路径(order/settlement/charging/card 域 src)模块级测试为 0——可提纯的判定(额度公式、幂等摘要)下沉 domain 是改进方向。

### ② 真库集成(#[ignore],TEST_MYSQL_URL 门控)

- 资金链路全事务:支付→订单→回执→计费→退款;并发/幂等/行锁;
- 解码路径**非 NULL 真实形态行**(CODING_STANDARDS 规则②:驱动真实写入端或造行重读);
- 主要入口:http-api/tests/(25 文件,路由级 oneshot 全真栈)、gateway-runtime/tests/(sink_flow、closed_loop、device_chains);
- 25 个测试二进制共享一个开发库,靠 `support/mod.rs::flow_lock`(端口 39701)全量串行。

### ③ 契约对账(纯静态,免服务)

- `api_contract.rs`:路由快照 + 信封外壳 + 错误码表冻结;
- `check-api-contract.py`:前端调用点 ↔ 后端路由 ↔ 请求必填字段三段对账(响应字段错位抓不住,靠集成流断言)。

## 3. 集成测试怎么写(http-api 为例)

```text
http-api/tests/
├── support/mod.rs   # TestAppBuilder:测试装配(走 PaymentServices::assemble 的
│                    #   Simulation 分支,与生产同源)+ flow_lock 串行
└── <域>_flow.rs     # router.clone().oneshot(req) 走完整 axum 栈:
                     #   提取器、鉴权、信封全真;128 条路由全覆盖,失败不静默跳过
```

写新集成测试的要点:

1. 用 TestAppBuilder 开关装配,不要手抄生产依赖图;
2. 涉解码的查询,夹具必须有非 NULL 真实形态行(规则②),优先驱动真实写入端;
3. 断言信封外壳与冻结码,不硬编码可变文案。

## 4. 协议测试(DC589)

- 互逆测试:build_*/parse_* 往返自洽(adapter_flow 等)——验证实现自洽,证伪力有限;
- 黄金样本:抓包帧字节硬编码对照(尤其偏移易错帧),补齐是纯增益;
- closed_loop / device_chains:真库 + 真实 TCP 会话,打通注册→支付→B7→B8→BB→计费与刷卡/告警/补单链路。

## 5. 设备模拟器

模拟器是独立 crate/二进制 `chargepilot-simulator`,不随主应用构建,也不进入 `chargepilot serve`。

### 5.1 single(单连接手工联调)

`chargepilot-simulator single --device-id <16 位板号> --target-addr 127.0.0.1:9100`,或 `node tasks.mjs simulator --device-id ...`。

指定一台设备接入:注册 → 空闲心跳 → 收到 B7 转充电遥测并回 B8 → 收到 B9 回 BA 并补 BB 结束帧(BB 回显 B7 订单号)。断线自动重连,`--duration 0`(默认)运行到 Ctrl-C;`--provision` 幂等写入 device_meta,`--speedup` 缩放心跳节拍。板号必须是 DC589 的 16 位十进制,业务侧 8 位设备号不能直接接入。

### 5.2 batch(容量验收加载源)

`chargepilot-simulator batch` 或 `node tasks.mjs simulator-batch`:headless 拉起 N 台虚拟设备跑真实协议交互。

| 参数 | 默认 | 说明 |
|---|---|---|
| --devices | 2000 | 虚拟设备数(容量目标) |
| --charging | 1000 | 并发充电数 |
| --speedup | 1.0 | 缩放间隔不改协议字节 |
| --duration | 300s | 运行时长 |
| --chaos-ratio | 0.0 | 注入迟到应答/半开挂住/重连风暴 |
| --seed | 42 | 设备号与 chaos 分派可复现 |
| --provision + --database-url | - | 预导入 device_meta(未导入设备网关拒绝) |

设备号确定性生成:前缀 5348 + 12 位序号(16 位十进制)。验收以 /metrics 指标说话,不以感觉说话(蓝图第一章)。

## 6. 改门禁的固定动作

「验收新增检查项时先扩展脚本再写规则——门禁是代码,规则引用代码,不双写」:

1. 检查逻辑写进 tasks.mjs(或 scripts/ops/ 的 python);
2. 蓝图/规则文档只引用脚本名,不复述实现;
3. 现有范例:check-sql(12 域 service/ + 网关层 device_events.rs 禁令,sink/telemetry 豁免)。
