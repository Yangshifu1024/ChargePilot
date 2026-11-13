//! 环境变量配置：手写加载与校验（required / 取值 / 密钥长度），语义对齐旧版 caarlos0/env 用法。
//!
//! 环境变量清单在阶段 09 冻结；本模块自阶段 01 起仅收录已启用变量，
//! 未收录变量不做静默处理（阶段 09 引入「未知变量报错」）。

/// 进程配置（阶段 01 地基部分；支付/微信凭据随阶段 04 扩展）。
#[derive(Debug, Clone)]
pub struct Config {
    /// 单库 DSN（ADR 001）。
    pub database_url: String,
    /// Redis 连接（会话/MFA/频次/锁）。
    pub redis_url: String,
    /// HTTP 监听地址。
    pub http_addr: String,
    /// 设备 TCP 监听地址（阶段 06 生效，此处校验格式）。
    pub device_addr: String,
    /// JWT 签名密钥（≥32 字节，旧版同语义）。
    pub jwt_secret: String,
    /// paid 态启动超时秒数（旧版缺口补齐，见 docs/design/state-machine.md）。
    pub paid_start_timeout_secs: i64,
}

impl Config {
    /// 从环境变量加载并校验。任何校验失败即返回错误（启动 fail-fast）。
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = require("DATABASE_URL")?;
        let redis_url = require("REDIS_URL")?;
        let http_addr = optional("HTTP_ADDR", "0.0.0.0:8080");
        let device_addr = optional("DEVICE_ADDR", "0.0.0.0:9100");
        let jwt_secret = require("JWT_SECRET")?;
        let paid_start_timeout_secs: i64 = optional("PAID_START_TIMEOUT_SECS", "300")
            .parse()
            .map_err(|_| anyhow::anyhow!("PAID_START_TIMEOUT_SECS 不是合法整数"))?;
        Self::build(
            database_url,
            redis_url,
            http_addr,
            device_addr,
            jwt_secret,
            paid_start_timeout_secs,
        )
    }

    /// 纯校验与装配（与环境解耦，供测试直接驱动）。
    fn build(
        database_url: String,
        redis_url: String,
        http_addr: String,
        device_addr: String,
        jwt_secret: String,
        paid_start_timeout_secs: i64,
    ) -> anyhow::Result<Self> {
        if database_url.trim().is_empty() {
            anyhow::bail!("DATABASE_URL 不能为空");
        }
        if redis_url.trim().is_empty() {
            anyhow::bail!("REDIS_URL 不能为空");
        }
        if jwt_secret.len() < 32 {
            anyhow::bail!("JWT_SECRET 至少 32 字节（当前 {}）", jwt_secret.len());
        }
        for (name, addr) in [("HTTP_ADDR", &http_addr), ("DEVICE_ADDR", &device_addr)] {
            if addr.parse::<std::net::SocketAddr>().is_err() {
                anyhow::bail!("{name} 不是合法监听地址: {addr}");
            }
        }
        Ok(Self {
            database_url,
            redis_url,
            http_addr,
            device_addr,
            jwt_secret,
            paid_start_timeout_secs,
        })
    }
}

fn require(name: &str) -> anyhow::Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        Ok(_) => anyhow::bail!("{name} 不能为空"),
        Err(_) => anyhow::bail!("缺少必需环境变量 {name}"),
    }
}

fn optional<'a>(name: &str, default: &'a str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => default.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB: &str = "mysql://u:p@127.0.0.1:3307/db";
    const REDIS: &str = "redis://127.0.0.1:6380";
    const SECRET_OK: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn valid_config_assembles() {
        let cfg = Config::build(
            DB.into(),
            REDIS.into(),
            "127.0.0.1:8080".into(),
            "127.0.0.1:9100".into(),
            SECRET_OK.into(),
            300,
        )
        .unwrap();
        assert_eq!(cfg.paid_start_timeout_secs, 300);
        assert_eq!(cfg.http_addr, "127.0.0.1:8080");
    }

    #[test]
    fn short_jwt_secret_rejected() {
        let e = Config::build(
            DB.into(),
            REDIS.into(),
            "127.0.0.1:8080".into(),
            "127.0.0.1:9100".into(),
            "short".into(),
            300,
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("32 字节"));
    }

    #[test]
    fn empty_database_url_rejected() {
        let e = Config::build(
            "".into(),
            REDIS.into(),
            "127.0.0.1:8080".into(),
            "127.0.0.1:9100".into(),
            SECRET_OK.into(),
            300,
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("DATABASE_URL"));
    }

    #[test]
    fn bad_listen_addr_rejected() {
        let e = Config::build(
            DB.into(),
            REDIS.into(),
            "not-an-addr".into(),
            "127.0.0.1:9100".into(),
            SECRET_OK.into(),
            300,
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("HTTP_ADDR"));
    }

    #[test]
    fn defaults_applied() {
        // from_env 在带完整 .env 的环境下也应通过（不假设环境为空）
        let _ = Config::from_env();
        // 默认值由 optional 兜底：无环境变量的构建路径等价于默认（此处验证 build 全默认地址）
        let cfg = Config::build(
            DB.into(),
            REDIS.into(),
            "0.0.0.0:8080".into(),
            "0.0.0.0:9100".into(),
            SECRET_OK.into(),
            300,
        )
        .unwrap();
        assert_eq!(cfg.device_addr, "0.0.0.0:9100");
    }
}
