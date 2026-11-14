# ADR 006：前端产物编译期嵌入（镜像不再支持外挂产物目录）

- 状态：已接受（票 22，2026-11-15）
- 裁决：管理端 SPA 与用户端 H5 前端产物从「磁盘挂载（`ADMIN_STATIC_DIR`/`H5_STATIC_DIR`）」改为「编译期嵌入二进制」——`crates/assets-admin` 与 `crates/assets-h5` 两个独立叶子 crate，`rust-embed` 嵌入 + SPA 回退随迁。

## 背景

旧形态在镜像构建后将前端产物 COPY 到 `/opt/chargepilot/{admin,h5}`，运行时经两个环境变量磁盘挂载；本地开发无产物时路由不挂载。问题：产物与二进制版本可能错位（镜像层分离）；部署编排需要额外挂载约定。

## 决策

1. **嵌入为唯一形态**：`build_router` 按嵌入产物挂载静态路由（产物缺失→空集合→不挂载，404 可见即诊断）；`ADMIN_STATIC_DIR`/`H5_STATIC_DIR` 环境变量退役（配置冻结清单 23→21）。
2. **叶子 crate 隔离**：嵌入宏展开昂贵，独立 crate 使产物变更只重编该 crate 与最终链接；两 crate 不依赖 platform，SPA 回退所需常量由装配根/门面自带。
3. **build.rs 产物准备**：仓库内 `frontend/*/dist` 存在则复制进 crate 的 `assets/`（rust-embed 嵌入 folder 恒存在）；镜像构建在 backend-build 阶段先 `COPY --from=frontend` 预置 `assets/`（build.rs 检测 dist 缺失时保留预置内容）。
4. **冒烟自检**：入口命令声明版本号（clap `version`），镜像构建末尾 `RUN chargepilot --version` 独立层自检。

## 影响

- 镜像**不再支持外挂产物目录**（无挂载点约定）；产物更新 = 重新构建镜像。
- 前端产物变更只触发 assets crate 与链接重编，业务代码不重编（增量构建验证：build.rs `cargo:rerun-if-changed` 仅指向 dist）。
- 本地开发无前端产物时编译通过（空 assets），`/admin`、`/h5` 返回 404——与旧行为（目录缺失不挂载）一致。
