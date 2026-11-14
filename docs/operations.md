# 运行与数据维护（阶段 09 冻结）

## 一条命令起全栈

```bash
cd docker
cp .env.example .env        # 填写密码与 JWT_SECRET（≥32 字节）
docker compose -f compose.prod.yaml --env-file .env up -d --build
# 可选边缘 TLS：加 --profile edge，并把 docker/Caddyfile 里的 <域名> 换成实际域名
```

- 应用：`http://<主机>:8080`（后台 `/admin/`，H5 `/h5/`，探活 `/health/live|ready`，指标 `/metrics`）
- 设备接入：`<主机>:9100`（TCP，DC589）
- 迁移：`serve` 启动时自动执行（幂等）；单独升级用 `docker compose ... run --rm app chargepilot migrate`
- 模拟器随镜像交付（现场排障）：`docker compose ... run --rm app chargepilot simulator-batch --target-addr app:9100 --devices 10 ...`

## 环境变量冻结清单（22 个，超出清单启动时 WARN）

| 组 | 变量 |
| --- | --- |
| 核心 | `DATABASE_URL` `REDIS_URL` `HTTP_ADDR` `DEVICE_ADDR` `JWT_SECRET` `PAID_START_TIMEOUT_SECS` |
| 静态产物与导出 | `ADMIN_STATIC_DIR` `H5_STATIC_DIR` `EXPORT_DIR` `EXPORT_MAX_ROWS` |
| 引导账号 | `ADMIN_BOOTSTRAP_USER` `ADMIN_BOOTSTRAP_PASSWORD` |
| 微信凭据组 | `WECHAT_APPID` `WECHAT_SECRET` `WECHAT_MCH_ID` `WECHAT_MCH_SERIAL` `WECHAT_PRIVATE_KEY_PEM` `WECHAT_PLATFORM_CERTS` `WECHAT_NOTIFY_URL` |
| 运行模式与日志 | `PAYMENT_MODE` `LOGIN_MODE` `RUST_LOG` |

- `PAYMENT_MODE`：`simulation`（默认，模拟渠道，无需微信凭据）/
  `wechat_direct`（生产，需填齐微信凭据组）/ `disabled`（只读兜底）。
- `LOGIN_MODE`：`development`（默认，`dev:<openid>` 登录）或 `wechat`（生产）。
  生产两者都置生产取值并填齐微信凭据组。

## 数据卷与备份

| 卷 | 内容 | 备份 |
| --- | --- | --- |
| `mysql-data` | 全部业务数据 | `docker exec <mysql> mysqldump -uroot -p... --single-transaction chargepilot > backup.sql` |
| `redis-data` | 会话/MFA/频次（可丢失重建） | 无需备份 |
| `exports` | 导出文件 | 直接拷目录 |

恢复：空卷起栈后 `docker exec -i <mysql> mysql -uroot -p... chargepilot < backup.sql`。

## 镜像说明

- 多阶段：node:24-alpine 构建两套前端 → rust:1-slim（cargo-chef 缓存）→ debian:bookworm-slim 运行时
- 镜像内仅：一个二进制（`chargepilot`，含 serve/migrate/simulator-batch 子命令）、两套静态产物；非 root（uid 10001）
- 镜像拉取一律走 `gh-proxy.org/docker/` 前缀（2026-11-14 约定）
