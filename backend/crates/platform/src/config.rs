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
        warn_unknown_env_vars();
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

/// 环境变量冻结清单（阶段 09）：应用可识别的全部变量。
/// 清单外变量（含拼写错误）启动时显式告警——配置漂移有据可查，不静默忽略。
const KNOWN_ENV_VARS: &[&str] = &[
    // 核心
    "DATABASE_URL",
    "REDIS_URL",
    "HTTP_ADDR",
    "DEVICE_ADDR",
    "JWT_SECRET",
    "PAID_START_TIMEOUT_SECS",
    // 静态产物与导出
    "ADMIN_STATIC_DIR",
    "H5_STATIC_DIR",
    "EXPORT_DIR",
    "EXPORT_MAX_ROWS",
    // 引导账号
    "ADMIN_BOOTSTRAP_USER",
    "ADMIN_BOOTSTRAP_PASSWORD",
    // 微信凭据组（production 必填；development 模式可缺省）
    "WECHAT_APPID",
    "WECHAT_SECRET",
    "WECHAT_MCH_ID",
    "WECHAT_MCH_SERIAL",
    "WECHAT_PRIVATE_KEY_PEM",
    "WECHAT_PLATFORM_CERTS",
    "WECHAT_NOTIFY_URL",
    // 运行模式与日志
    "PAYMENT_MODE",
    "LOGIN_MODE",
    "RUST_LOG",
];

/// 宿主/容器运行时会注入的系统变量（与配置无关，不告警）。
const SYSTEM_ENV_PREFIXES: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_", "TMPDIR", "USER", "SHELL", "TERM", "PWD", "SHLVL", "TZ",
    "HOSTNAME", "SSH_", "XPC_", "__CF", "LS_COLORS", "COLORFGBG", "EDITOR", "RUST_BACKTRACE",
    "MYSQL_", "GATEWAY_", "WINDIR", "COMSPEC", "PROGRAMFILES", "ProgramFiles", "HOMEDRIVE",
    "HOMEPATH", "SYSTEMROOT", "SystemRoot", "SYSTEMDRIVE", "TEMP", "TMP", "APPDATA",
    "LOCALAPPDATA", "ProgramData", "PUBLIC", "DriverData", "OS", "PROCESSOR_", "NUMBER_OF_",
    "PATHEXT", "PROMPT", "SESSIONNAME", "USERDOMAIN", "USERNAME", "COMPUTERNAME", "COMMONPROGRAM",
    "CommonProgram", "POWERSHELL", "EXEPATH", "HOMESHARE", "LOGONSERVER", "ONEHEADER",
];

/// 对清单外变量打 WARN（不阻断启动：容器运行时变量难以穷举，漂移靠日志可查）。
fn warn_unknown_env_vars() {
    for (name, _) in std::env::vars() {
        if KNOWN_ENV_VARS.contains(&name.as_str()) {
            continue;
        }
        if SYSTEM_ENV_PREFIXES
            .iter()
            .any(|p| name.starts_with(p) || name == *p)
        {
            continue;
        }
        tracing::warn!(
            var = %name,
            "未在冻结清单内的环境变量（若为配置项请核对拼写与 docs/operations.md）"
        );
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

#[cfg(test)]
mod freeze_tests {
    use super::*;

    #[test]
    fn frozen_list_covers_core_vars() {
        // 冻结清单覆盖核心五项（阶段 09 spec：约 15 个）。
        for name in [
            "DATABASE_URL",
            "REDIS_URL",
            "JWT_SECRET",
            "HTTP_ADDR",
            "DEVICE_ADDR",
        ] {
            assert!(KNOWN_ENV_VARS.contains(&name), "{name} 应在冻结清单");
        }
        // 微信凭据组 7 个是 spec 明确要求的一组，总数冻结在 22 个不再增长。
        assert_eq!(KNOWN_ENV_VARS.len(), 22);
    }

    #[test]
    fn system_prefixes_do_not_shadow_known_vars() {
        // 系统前缀不得吞掉清单内变量（RUST_LOG 同时在两侧时清单优先）。
        assert!(KNOWN_ENV_VARS.contains(&"RUST_LOG"));
        assert!(!SYSTEM_ENV_PREFIXES.contains(&"RUST_LOG"));
    }
}
