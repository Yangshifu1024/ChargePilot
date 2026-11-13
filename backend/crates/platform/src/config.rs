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

    #[test]
    fn missing_required_fails() {
        // 环境变量进程级共享：并行测试下仅断言错误信息包含变量名
        let r = Config::from_env();
        match r {
            Err(e) => assert!(
                e.to_string().contains("DATABASE_URL")
                    || e.to_string().contains("REDIS_URL")
                    || e.to_string().contains("JWT_SECRET")
            ),
            Ok(_) => panic!("缺环境变量时应失败"),
        }
    }
}
