# 重写验收报告（阶段 10）

状态：已完成（2026-11-14，全部数据为本机实测）
验收基线：阶段 09 镜像 `chargepilot/app:latest`（git `1376249` 之后含 received gauge 的重建版）+ `docker/compose.prod.yaml` 全栈 + 镜像内 `simulator-batch`

## 一、功能验收（五项）

### 1. 契约与 schema 一致
- 迁移文件 `backend/migrations/0001_init.sql` 85 张表（84 业务表 + `schema_migrations`），与 [设计稿](design/schema.md) 差异清单一致；空库建表后 `information_schema.tables` 计数 85（2026-11-14 实测开发库）。
- wire DTO serde 序列化断言随 15 个 `*_flow.rs` 集成测试全绿（145 用例 0 失败）。
- 偏差说明：阶段 10 spec 设想「OpenAPI 路由快照」，但阶段 00 实际冻结物是 schema/状态机/事件接口/字段消费审计四基准，未产出 OpenAPI 文件；本次以「表集合一致 + serde 集成全绿 + 字段消费审计表」为契约闸门。

### 2. 资金域差分对照（黄金样本）
- 样本：`backend/crates/domain/tests/golden/billing.json`（82 条：cost 22 / price_actual 8 / settle 10 / measured 10 / estimate 6 / budget 5 / cutoff 6 / decide_stop 4 / validate_spec 7 / validate_scheme 4）+ `discount.json`（38 条），由旧版 Go 计费纯函数批量产出（生成期 2026-11-13），Rust 实现逐条比对断言。
- 结果：`golden_billing` + `golden_discount` 全绿（随 workspace 145 用例）。

### 3. 资金链路与并发/幂等真库集成
- `cargo test --workspace`（TEST_MYSQL_URL 指向真实 MySQL 8.4）145 通过 0 失败；覆盖退款事务、幂等重放、行锁、`SKIP LOCKED`、唯一键仲裁（`resilience_flow` 四场景：回执迟到失败退款 / 事件丢失重放 / 重启后结束回执幂等 / 缓冲满丢最旧）。

### 4. E2E 全链路闭环
- `closed_loop`（真库）：A0 注册 → 扫码 → B7/B8 启动 → 计量 → B9/BA 停止 → BB 结束 → 计费 → 结算，全链 2.7s 通过。
- compose 生产栈冒烟（`scripts/ops/compose-smoke.sh`）18/18：管理端登录/建站/建厂商/建方案/发布/导入设备/诊断 + 用户端 development 登录/扫码查套餐/下单/模拟支付到账 + 静态与探活。

### 5. 打包与部署形态
- 单镜像多阶段构建（前端两套产物 + Rust 二进制，非 root，健康检查 `/health/live`），`docker compose up -d` 一条命令起全栈；环境变量冻结 22 个，清单外启动 WARN。

## 二、容量验收（四项，ADR 005）

**达标**。15x 加速（心跳间隔 /15，放大压力）、2000 台连接、1000 台充电遥测、1800 秒实测：

| 观测 | 结果 |
|---|---|
| 注册 | 2000/2000 成功率 100%，拒绝 0 |
| 帧 | 发送 1,571,760 / 接收 1,571,855（收发平衡） |
| 断线 | 重连 0 / 连接错误 0 / 异常注入 0 |
| app 容器内存 | 稳态 62–79 MiB（阈值 512MB，余量 6.5 倍+），采样 30 份每 60s 一份 |
| 充电台收尾 | 结束帧 1000/1000 |

复现：`docker compose -f docker/compose.prod.yaml run --rm app simulator-batch --devices 2000 --charging 1000 --speedup 15 --duration 1800 --target-addr app:9100 --database-url "…"`，内存采样 `docker stats --no-stream` 每 60s。

**达标**。口径说明：ADR 005 的「1000 台同时充电」是设备数指标；真实设备心跳周期 30s，真实遥测负载 = 1000 台 × 4 指标 / 30s ≈ 133 样本/s。本次以 1x（不加速）跑 2000 连接 / 1000 充电 300 秒，取遥测表 `ts`（帧采集时刻）→ `created_at`（落库时刻）差值直接计算端到端延迟：

| 观测 | 结果 |
|---|---|
| 端到端 p99 | **345 ms**（阈值 1s） |
| 端到端 max | 580 ms |
| 缓冲稳态深度 | 0（恒空，flush 节拍 200ms 实时消化） |
| 期间丢弃 | 0 |

复现（延迟分布 SQL）：
```sql
SELECT ROUND(AVG(latency_ms),1), MAX(latency_ms) FROM (
  SELECT TIMESTAMPDIFF(MICROSECOND, ts, created_at)/1000 AS latency_ms,
         PERCENT_RANK() OVER (ORDER BY TIMESTAMPDIFF(MICROSECOND, ts, created_at)) pr
  FROM telemetry WHERE created_at >= UTC_TIMESTAMP(3) - INTERVAL 7 MINUTE
) t WHERE pr >= 0.99;
```

**达标**。`docker compose up -d --force-recreate app` 重启（换镜像部署窗口）+ 立即起 2000 台模拟器，轮询 `gateway_online_sessions` gauge：

```
7s 在线=586 → 12s 在线=1348 → 17s 在线=1979 → 22s 在线=2000
RECOVERY_OK 全部 2000 台在线，耗时 22s（阈值 60s）
```

**达标**。三组证据：

1. **对账恒等式**（1x 窗口，app 重启后累计）：`received 135424 = flushed 135424 + dropped 0`，指标 `telemetry_buffer_{received,dropped}_total` 与明细表逐行一致。
2. **聚合零丢失**（可分离窗口）：1x 独占的 15 分钟桶——明细 1118 行 = `telemetry_aggregate_15min.count` 之和 1118，逐条相等；hourly 桶与 15min 同一聚合代码路径。
3. **超载丢样可解释**（15x 放大 30 倍压力）：生产 ~4000 样本/s > flush 能力 ~1292 样本/s，缓冲满 4096 丢最旧，`dropped` 计数以 ~2000/s 线性准确增长（净丢 = 生产 − 消费），30 分钟全程无一次计数跳变或回退——分级语义（丢明细、保快照与聚合）按 ADR 005 工作。真实负载 133 样本/s 下 flush 有 9.7 倍余量，丢弃为 0。

## 三、人工验收清单（超出本地环境，无法自动验收）

| 项 | 说明 | 前置 |
|---|---|---|
| 真实微信支付联调 | `PAYMENT_MODE=wechat_direct` 全链（下单/回调/退款）需商户号与证书 | 微信商户凭据 |
| 真机 weapp 发布 | 小程序端真机回归（登录/支付/蓝牙扫码路径） | 微信开放平台 + 真机 |
| TLS/域名接入 | `--profile edge` Caddy 证书签发与反代 | 公网域名 |
| 真实设备联调 | DC589 真机（非模拟器）协议回归 | 真机设备 |
| 长稳观测 | 7×24 小时运行内存/连接泄漏观察 | 部署环境 |

## 四、剩余风险与已知边界

- **OrbStack 容器网络空闲回收**：1x 口径容器间空闲 60s 的 TCP 连接被虚拟网络层回收（设备侧重连，服务端无感知异常）；本机直连对照实验（100 台 1x 150s，60s 心跳）零断线，证明非服务端缺陷。真实部署为设备直连/专网，不经过该层；若中间链路存在空闲超时，保持设备 30s 心跳即可。验收口径以 15x（流量活跃）30 分钟零断线为准。
- **15x 模拟放大下缓冲稳态丢样**：生产 1x 口径 133 样本/s，flush 能力 1292 样本/s（9.7 倍余量），丢弃仅发生在 30 倍超载的模拟口径；丢样只影响明细密度，快照与聚合不丢（对账见容量第 4 项）。若未来真实负载逼近 flush 能力，优化方向已归因：聚合 UPSERT 与明细同事务串行、MySQL 侧 ODKU 计算密集——可拆两级聚合为独立连接并行或对 hourly 桶节流。
- **B9 停止无回执的自动停机未实现**（旧版同样仅观测；sweeper 兜底已覆盖启动超时，停止无回执依赖设备 BB 结束帧或人工介入）。
- **微信 production 凭据未实测**（见人工清单）。
- **flush 能力实测口径**：1292 样本/s 是 compose 栈（本机 Docker MySQL 8.4）实测；生产数据库性能不同需重测。
