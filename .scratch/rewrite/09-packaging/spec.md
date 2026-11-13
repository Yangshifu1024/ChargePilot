# 阶段 09 · 打包收敛与单镜像交付

Status: ready-for-agent

## Problem Statement

后端仍以开发形态运行：前端未进镜像、compose 还是旧版多进程拓扑、环境变量未收敛、H5 未被服务——「前后端打包在一个 Docker 镜像」的目标尚未成立。

## Solution

交付生产形态的单镜像与单 compose：多阶段构建（前端两套产物 + Rust 二进制），`docker compose up` 一条命令起全栈，环境变量冻结在约 15 个。

## User Stories

1. 作为运维者，我想用一条 `docker compose up` 启动 app + MySQL + Redis 完整栈，所以交付物就是一个 compose 文件加一份 env。
2. 作为运维者，我想让镜像里只包含一个二进制、两套前端静态产物与迁移 SQL，所以镜像可审计、无构建残留。
3. 作为运维者，我想让迁移在启动路径上可控（`serve` 自动先迁移或显式 `migrate` 子命令），所以升级流程明确。
4. 作为运营人员，我想浏览器打开 `/admin/` 即用后台（history 路由刷新不 404），所以部署后无需额外 Web 服务器。
5. 作为充电用户，我想浏览器打开 `/h5/` 即用 H5 端完成充电全流程，所以 H5 正式可服务。
6. 作为运维者，我想 TLS 由外部负载均衡承担（compose 提供可选 caddy profile 兜底），所以内层无需证书管理。
7. 作为开发者，我想环境变量清单冻结（约 15 个）并写入运行手册，所以配置漂移有据可查。
8. 作为运维者，我想镜像以非 root 运行且健康检查就绪，所以编排探针可用。

## Implementation Decisions

- Dockerfile 多阶段：node:24 构建两套前端（admin-web `base '/admin/'` 不变；miniprogram H5 `publicPath` 改 `/h5/`，hash 路由无需 fallback）→ cargo-chef 缓存构建 → debian-slim 运行时；weapp 产物不进镜像。
- 路由收敛：8080 单端口承载 `/api/v1/user|admin|public` + `/admin/*` + `/h5/*` + `/health` + `/metrics`；9100 独立暴露给设备。
- 环境变量冻结清单（约 15 个）：数据库 DSN、Redis URL、监听地址、JWT_SECRET、微信凭据组、PAYMENT_MODE、LOGIN_MODE、引导账号组、导出目录、默认费率 JSON——超出清单的变量一律在启动时报错提醒而非静默忽略。
- compose：app + mysql:8.4 + redis:8；caddy 为可选 profile；设备端口 9100 映射；数据卷与备份说明进运行手册。
- **镜像加速约定**（用户提供，2026-11-14）：所有容器镜像经 gh-proxy 前缀拉取，写作 `gh-proxy.org/docker/<原始镜像>`（如 `gh-proxy.org/docker/mysql:8.4`）；该平台不支持 daemon.json 的 registry-mirrors 方式，一律用镜像名前缀，Dockerfile 的 FROM 与 compose 的 image 全部遵守。
- 前端请求封装改动点：admin-web 与 H5 的 `VITE_API_BASE` / `TARO_APP_API_BASE` 同源化；契约修剪（字段消费审计表执行结果）在本阶段随前端一次性回归。
- `simulator-batch` 随镜像交付（排障用），默认不自动运行。

## Testing Decisions

- 主缝冒烟：compose 起全栈后跑一条关键路径（管理员登录→建方案→导入设备；用户 development 登录→下单→充电→结算）。
- 静态资源：`/admin/` 与 `/h5/` 的资源可达性与 fallback；H5 全流程浏览器手工回归清单。
- 镜像：非 root、体积记录、健康检查通过、重启后数据卷持久。
- 前端回归以审计表为清单逐项核对。

## Out of Scope

TLS 证书签发与域名接入；weapp 发布；CI/CD 流水线（本地脚本已覆盖构建验证，流水线按需另立 spec）。

## Further Notes

本阶段完成后系统即具备「可部署」形态；容量与全量验收在阶段 10 关闸。
