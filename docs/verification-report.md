# 重写验收报告（阶段 10）

状态：数据采集中（容量压测进行时，本文占位段以 `TBD` 标注）
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

### 1. 2000 连接稳定 30 分钟，内存 < 512MB
TBD（15x 加速 2000 台/1000 充电 1800s 实测：注册成功率、内存曲线、断线重连计数）

### 2. 1000 并发充电遥测端到端 p99 < 1s
TBD（口径：真实设备心跳周期 30s，15x 为模拟放大 30 倍压力；1x 口径下缓冲稳态深度 + flush 节拍推导）

### 3. 进程重启后 2000 台 60 秒内恢复在线
TBD

### 4. 缓冲丢弃计数可解释，快照/聚合零丢失
TBD（对账恒等式 `received = flushed + dropped`；聚合 `count` 总和与明细核对）

## 三、人工验收清单（超出本地环境，无法自动验收）

| 项 | 说明 | 前置 |
|---|---|---|
| 真实微信支付联调 | `PAYMENT_MODE=wechat_direct` 全链（下单/回调/退款）需商户号与证书 | 微信商户凭据 |
| 真机 weapp 发布 | 小程序端真机回归（登录/支付/蓝牙扫码路径） | 微信开放平台 + 真机 |
| TLS/域名接入 | `--profile edge` Caddy 证书签发与反代 | 公网域名 |
| 真实设备联调 | DC589 真机（非模拟器）协议回归 | 真机设备 |
| 长稳观测 | 7×24 小时运行内存/连接泄漏观察 | 部署环境 |

## 四、剩余风险与已知边界

- 15x 模拟放大下遥测缓冲稳态丢样（生产 1x 口径约 133 样本/s，flush 能力 1292 样本/s 为其 9.7 倍；丢样仅影响明细密度，聚合与快照不丢）。
- B9 停止无回执的自动停机未实现（旧版同样仅观测）。
- 微信 production 凭据未实测（见人工清单）。
