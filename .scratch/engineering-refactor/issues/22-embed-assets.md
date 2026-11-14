# 22: 静态资源嵌入与镜像冒烟（S7，D2）

**What to build:** 前端双产物（管理端 SPA 与用户端 H5）从磁盘挂载改为编译期嵌入：各建一个依赖面最小的独立叶子 crate（嵌入宏展开昂贵，隔离后产物不变则不重编；不依赖平台 crate，SPA 回退所需值由装配根传参），允许本地无产物时编译（占位缺失容忍），SPA 回退逻辑随迁。两个静态目录环境变量退役、配置结构同步收编。镜像构建保留依赖缓存三段式，补二进制版本号自检冒烟层（入口命令需先声明版本号）。部署形态变更（镜像不再支持外挂产物目录）落 ADR。

**Blocked by:** 19（路由汇总层的静态挂载段重排后）

Status: done

- [x] 双资产叶子 crate 嵌入生效（缺失容忍 + 最小依赖面），SPA 回退回归通过（现有 SPA 回退测试保留）
- [x] 静态目录环境变量退役，配置一致性测试同步更新
- [x] 入口命令版本号声明 + 镜像构建冒烟自检层
- [x] 部署形态 ADR 落 docs/adr/
- [x] 镜像本地构建 + 冒烟 + 健康检查全通；前端产物变更不触发业务代码重编（增量验证）

## Comments

### 2026-11-15（实施完成）

**实施内容**

- 双叶子 crate：`crates/assets-admin`（admin-web dist）与 `crates/assets-h5`（miniprogram H5 dist），`rust-embed 8.14.0` 嵌入 `folder="assets"`，各暴露 `get/is_empty/index`；不依赖 platform，零业务依赖面。
- build.rs 产物准备：仓库内 `frontend/*/dist` 存在则复制进 `assets/`（复制前清空重建），缺失时保留现有 assets 并 `create_dir_all` 兜底（本地无产物 = 空目录 = 嵌入空集合）；`cargo:rerun-if-changed` 仅指向 dist。
- 嵌入路由（http-api/lib.rs）：产物非空时挂载 `/admin`（`/admin`、`/admin/`、`/admin/{*rest}` 三条）与 `/h5`（同构三条）；MIME 按扩展名猜测；未命中静态文件回落 index.html（SPA history 路由），H5 同样回落。
- 配置退役：`ADMIN_STATIC_DIR`/`H5_STATIC_DIR` 出配置结构与 KNOWN_ENV_VARS 冻结清单（23→21），一致性测试通过；SpaFallback 磁盘目录形态整段删除。
- 入口版本号：clap `#[command(version)]`，`chargepilot --version` 输出 `chargepilot 0.1.0`。
- Dockerfile：backend-build 阶段 `COPY --from=frontend` 预置两处 assets/ + `RUN ./target/release/chargepilot --version` 冒烟层；runtime 删 `/opt` 产物 COPY 与静态目录 ENV。
- ADR：[006-static-embed.md](../../docs/adr/006-static-embed.md)（部署形态变更：镜像不再支持外挂产物目录），README 索引同步。

**关键裁决**

1. rust-embed 8.x 无 `allow-missing` feature（提票时的假设不成立）→ 改 build.rs 复制方案：folder 恒存在（build.rs 兜底建目录），产物有无由内容表达，`is_empty()` 判空后决定路由挂载。
2. axum 0.8 通配 `{*rest}` 不匹配空尾：`/admin/`（尾斜杠根）落到 fallback 404——镜像实测抓出后补 `/admin`、`/admin/`（及 `/h5` 同构）显式根路由。
3. build.rs 豁免 `disallowed-methods`/`expect_used`：`CARGO_MANIFEST_DIR` 是 cargo 注入的构建环境变量，不属于票 16 业务进程 env 禁令管辖；`expect` 失败即构建环境损坏应中止构建。
4. 冒烟层首次运行即抓住 `chargepilot` 不在 PATH（exit 127）→ 改 `./target/release/chargepilot` 相对路径；自检层的价值已自证。

**验证证据**

- `cargo fmt --all` + `cargo clippy --workspace --all-targets` 全绿；`node tasks.mjs check` 四段 exit=0（契约核对：前端调用点 99 个、后端路由 121 个、缺口 0）。
- 真库集成轮 `node tasks.mjs test-integration`：**194 通过 / 0 失败**（基线不劣化）。
- 镜像：`docker build -f backend/Dockerfile -t chargepilot:ticket22 .` 成功；冒烟层输出 `chargepilot 0.1.0`。
- 容器探活（连开发库）：`/health/live`、`/health/ready` 200；`/admin`、`/admin/`、`/admin/login`、`/admin/devices` 全 200（index.html 深链回落）；`/admin/assets/index-*.js` 直出 288KB（`application/javascript`，vite `base:'/admin/'` 资源自洽）；`/h5/`、`/h5/index.html` 200；`/metrics` 200。缺 `DATABASE_URL`/`JWT_SECRET` 时进程启动即 fail-fast（正确行为）。
- 增量验证（验收项「产物变更不触发业务代码重编」）：产物未变二次构建 `Finished in 18.41s` 零重编；产物内容变更后仅 `Compiling chargepilot-assets-admin` 一个 crate，业务 crate 不重编。
- SPA 回退测试 `admin_spa_fallback` 双分支（空集合 404 / 产物存在 200）通过。

**偏差**

无。注：集成轮跑在根路由修复提交前的代码上，根路由修复仅触及静态路由注册段（无产物形态该段不挂载），不影响任何 DB 语义。
