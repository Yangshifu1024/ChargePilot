# ChargePilot 单镜像（票 22）：前端产物编译期嵌入 + 服务/模拟器两个独立 Rust 二进制。
# 构建上下文 = 仓库根（backend/ + frontend/）。
# 镜像拉取一律走 gh-proxy.org/docker/ 前缀（用户约定 2026-11-14）。

# ---------- 前端：admin-web(SPA) 与 miniprogram(H5) ----------
# 用 glibc 的 slim 镜像：Taro 原生 binding 锁文件只含 *-gnu 平台包，musl(alpine) 无条目。
# H5_LOGIN_MODE 决定 H5 产物登录形态（dev: 账号直登 / wechat 真实微信），须与
# 后端 LOGIN_MODE 一致（docker/.env）；默认 development 对齐开发验收栈。
FROM gh-proxy.org/docker/node:24-slim AS frontend
ARG H5_LOGIN_MODE=development
ENV TARO_APP_LOGIN_MODE=${H5_LOGIN_MODE}
WORKDIR /fe
COPY frontend/admin-web/package.json frontend/admin-web/package-lock.json ./admin-web/
COPY frontend/miniprogram/package.json frontend/miniprogram/package-lock.json frontend/miniprogram/.npmrc ./miniprogram/
RUN cd admin-web && npm ci --no-audit --no-fund \
 && cd ../miniprogram && npm ci --no-audit --no-fund
COPY frontend/admin-web ./admin-web
COPY frontend/miniprogram ./miniprogram
RUN cd admin-web && npm run build \
 && cd ../miniprogram && npm run build:h5

# ---------- Rust：cargo-chef 依赖缓存 + release 二进制 ----------
FROM gh-proxy.org/docker/rust:1-slim AS chef
RUN cargo install cargo-chef --locked
WORKDIR /build

FROM chef AS planner
COPY backend .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS backend-build
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY backend .
# 票 22：前端产物预置进叶子 crate 的嵌入目录（build.rs 检测 dist 缺失时保留预置）。
COPY --from=frontend /fe/admin-web/dist crates/assets-admin/assets/
COPY --from=frontend /fe/miniprogram/dist/h5 crates/assets-h5/assets/
# 服务与模拟器分属两个 crate/二进制：模拟器独立构建，不进入主应用。
RUN cargo build --release -p chargepilot-app -p chargepilot-simulator
# 镜像冒烟自检层（票 22）：入口命令先声明版本号，产物可用即构建失败回退。
# release 产物不在 PATH，用相对路径直呼（首次构建曾以 127 抓出此错）。
RUN ./target/release/chargepilot --version \
 && ./target/release/chargepilot-simulator --version

# ---------- 运行时：非 root + 健康检查 ----------
FROM gh-proxy.org/docker/debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --create-home --uid 10001 chargepilot
COPY --from=backend-build /build/target/release/chargepilot /usr/local/bin/chargepilot
# 现场排障工具：独立二进制，不参与 serve 进程。
COPY --from=backend-build /build/target/release/chargepilot-simulator /usr/local/bin/chargepilot-simulator
USER chargepilot
WORKDIR /home/chargepilot
# 票 22：前端产物编译期嵌入，/opt 产物目录与静态目录 ENV 退役。
ENV HTTP_ADDR=0.0.0.0:8080 \
    DEVICE_ADDR=0.0.0.0:9100 \
    EXPORT_DIR=/data/exports
EXPOSE 8080 9100
HEALTHCHECK --interval=30s --timeout=3s --start-period=20s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8080/health/live || exit 1
ENTRYPOINT ["chargepilot"]
CMD ["serve"]
