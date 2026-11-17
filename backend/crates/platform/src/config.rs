//! 环境变量配置：统一加载与校验（必填项 / 取值 / 密钥长度）。
//!
//! 全部应用环境变量经 [`Config`] 分组加载（数据库/监听/认证/支付/
//! 登录/微信凭据/导出/引导账号等）；支付与登录模式为枚举——非法取值启动即失败。
//! 通过可注入的 [`EnvSource`] 读取环境，便于测试断言默认值、校验路径，以及
//! 「实际读取集合 == 冻结清单」的一致性。
//! 清单外变量启动时打 WARN，不静默忽略。

/// 进程级配置（自环境变量组装）。
#[derive(Debug, Clone)]
pub struct Config {
    /// 数据库组：单库 DSN（ADR 001）。
    pub database_url: String,
    /// 数据库组：Redis 连接（会话/MFA/频次/锁）。
    pub redis_url: String,
    /// 监听组。
    pub listen: ListenConfig,
    /// 认证组。
    pub auth: AuthConfig,
    /// 支付渠道模式（PAYMENT_MODE，枚举型——非法取值启动失败）。
    pub payment_mode: PaymentMode,
    /// 登录模式（LOGIN_MODE，枚举型——非法取值启动失败）。
    pub login_mode: LoginMode,
    /// 微信凭据组（production 必填；development/simulation 模式可缺省）。
    pub wechat: WechatCreds,
    /// 导出组。
    pub export: ExportConfig,
    /// 引导账号组（user 为空则跳过引导）。
    pub bootstrap: BootstrapConfig,
}

/// 监听组。
#[derive(Debug, Clone)]
pub struct ListenConfig {
    /// HTTP 监听地址。
    pub http_addr: String,
    /// 设备 TCP 监听地址（启动时校验 host:port 格式）。
    pub device_addr: String,
}

/// 认证组。
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// JWT 签名密钥（≥32 字节）。
    pub jwt_secret: String,
    /// 订单 paid 态等待设备启动的最长秒数（见 docs/design/state-machine.md）。
    pub paid_start_timeout_secs: i64,
}

/// 支付渠道模式（环境变量 `PAYMENT_MODE` 的三个合法取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentMode {
    /// 模拟渠道（开发/测试默认）：确认端点开放，无真实出站。
    Simulation,
    /// 微信直连（真实商户联调）。
    WechatDirect,
    /// 支付禁用（下单拒绝）。
    Disabled,
}

impl PaymentMode {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "simulation" => Ok(Self::Simulation),
            "wechat_direct" => Ok(Self::WechatDirect),
            "disabled" => Ok(Self::Disabled),
            other => anyhow::bail!(
                "PAYMENT_MODE 取值非法: {other}（可选 simulation / wechat_direct / disabled）"
            ),
        }
    }
}

/// 登录模式（环境变量 `LOGIN_MODE` 的两个合法取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMode {
    /// 开发登录（dev:<account> 伪码换 token）。
    Development,
    /// 微信 code 换 session（jscode2session 出站）。
    Wechat,
}

impl LoginMode {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "development" => Ok(Self::Development),
            "wechat" => Ok(Self::Wechat),
            other => anyhow::bail!("LOGIN_MODE 取值非法: {other}（可选 wechat / development）"),
        }
    }
}

/// 微信凭据组（原始字符串；PEM/证书解析在支付域装配层完成）。
#[derive(Debug, Clone, Default)]
pub struct WechatCreds {
    pub appid: String,
    pub secret: String,
    pub mch_id: String,
    pub mch_serial: String,
    pub private_key_pem: String,
    /// 平台证书（序列号,PEM;... 多条分号分隔）。
    pub platform_certs: String,
    /// 微信支付 APIv3 密钥（32 字节）。
    pub api_v3_key: String,
    pub notify_url: String,
}

/// 导出组（与 `chargepilot_mod_export::ExportConfig` 对齐；max_rows 非正数默认 50000）。
#[derive(Debug, Clone)]
pub struct ExportConfig {
    pub dir: String,
    pub max_rows: i64,
}

/// 引导账号组。
#[derive(Debug, Clone, Default)]
pub struct BootstrapConfig {
    pub user: String,
    pub password: String,
}

/// 可注入的环境读取口：`read` 取单变量、`all` 供未知变量告警。
/// 生产实现 [`SystemEnv`]；测试注入假环境以断言默认值/校验路径/读取集合。
pub trait EnvSource {
    fn read(&self, name: &str) -> Option<String>;
    fn all(&self) -> Vec<(String, String)>;
}

/// 生产实现：直读进程环境（全仓唯一的环境变量直读点，clippy 禁令豁免于此）。
pub struct SystemEnv;

impl EnvSource for SystemEnv {
    #[allow(clippy::disallowed_methods)] // 统一配置入口：全仓唯一允许直读环境变量
    fn read(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    #[allow(clippy::disallowed_methods)] // 统一配置入口：全仓唯一允许直读环境变量
    fn all(&self) -> Vec<(String, String)> {
        std::env::vars().collect()
    }
}

impl Config {
    /// 从进程环境加载并校验（组合根唯一入口）。任何校验失败即返回错误。
    pub fn from_env() -> anyhow::Result<Self> {
        Self::load(&SystemEnv)
    }

    /// 经 [`EnvSource`] 加载并校验；测试可注入假环境。
    pub fn load(src: &impl EnvSource) -> anyhow::Result<Self> {
        warn_unknown_env_vars(&src.all()[..]);
        let database_url = require(src, "DATABASE_URL")?;
        let redis_url = require(src, "REDIS_URL")?;
        let listen = ListenConfig {
            http_addr: optional(src, "HTTP_ADDR", "0.0.0.0:8080"),
            device_addr: optional(src, "DEVICE_ADDR", "0.0.0.0:9100"),
        };
        let paid_start_timeout_secs: i64 = optional(src, "PAID_START_TIMEOUT_SECS", "300")
            .parse()
            .map_err(|_| anyhow::anyhow!("PAID_START_TIMEOUT_SECS 不是合法整数"))?;
        let auth = AuthConfig {
            jwt_secret: require(src, "JWT_SECRET")?,
            paid_start_timeout_secs,
        };
        let payment_mode = PaymentMode::parse(&optional(src, "PAYMENT_MODE", "simulation"))?;
        let login_mode = LoginMode::parse(&optional(src, "LOGIN_MODE", "development"))?;
        let wechat = WechatCreds {
            appid: optional(src, "WECHAT_APPID", ""),
            secret: optional(src, "WECHAT_SECRET", ""),
            mch_id: optional(src, "WECHAT_MCH_ID", ""),
            mch_serial: optional(src, "WECHAT_MCH_SERIAL", ""),
            private_key_pem: optional(src, "WECHAT_PRIVATE_KEY_PEM", ""),
            platform_certs: optional(src, "WECHAT_PLATFORM_CERTS", ""),
            api_v3_key: optional(src, "WECHAT_APIV3_KEY", ""),
            notify_url: optional(src, "WECHAT_NOTIFY_URL", ""),
        };
        let export = ExportConfig {
            dir: optional(src, "EXPORT_DIR", ""),
            max_rows: optional(src, "EXPORT_MAX_ROWS", "").parse().unwrap_or(0),
        };
        let bootstrap = BootstrapConfig {
            user: optional(src, "ADMIN_BOOTSTRAP_USER", ""),
            password: optional(src, "ADMIN_BOOTSTRAP_PASSWORD", ""),
        };
        Self::validate(
            &database_url,
            &redis_url,
            &listen,
            &auth,
            &export.max_rows.to_string(),
        )?;
        Ok(Self {
            database_url,
            redis_url,
            listen,
            auth,
            payment_mode,
            login_mode,
            wechat,
            export,
            bootstrap,
        })
    }

    /// 纯校验（与环境解耦）。
    fn validate(
        database_url: &str,
        redis_url: &str,
        listen: &ListenConfig,
        auth: &AuthConfig,
        _max_rows_raw: &str,
    ) -> anyhow::Result<()> {
        if database_url.trim().is_empty() {
            anyhow::bail!("DATABASE_URL 不能为空");
        }
        if redis_url.trim().is_empty() {
            anyhow::bail!("REDIS_URL 不能为空");
        }
        if auth.jwt_secret.len() < 32 {
            anyhow::bail!("JWT_SECRET 至少 32 字节（当前 {}）", auth.jwt_secret.len());
        }
        for (name, addr) in [
            ("HTTP_ADDR", &listen.http_addr),
            ("DEVICE_ADDR", &listen.device_addr),
        ] {
            if addr.parse::<std::net::SocketAddr>().is_err() {
                anyhow::bail!("{name} 不是合法监听地址: {addr}");
            }
        }
        Ok(())
    }
}

/// 应用可识别的环境变量冻结清单。
/// **一致性测试锁定**：`Config::load` 实际读取的变量名集合（含 RUST_LOG 的
/// 日志层直读）必须与本清单相等——新增/删除变量两处同步，漂移即测试失败。
pub const KNOWN_ENV_VARS: &[&str] = &[
    // 核心
    "DATABASE_URL",
    "REDIS_URL",
    "HTTP_ADDR",
    "DEVICE_ADDR",
    "JWT_SECRET",
    "PAID_START_TIMEOUT_SECS",
    // 静态产物（编译期内嵌）与导出
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
    "WECHAT_APIV3_KEY",
    "WECHAT_NOTIFY_URL",
    // 运行模式与日志
    "PAYMENT_MODE",
    "LOGIN_MODE",
    "RUST_LOG",
];

/// 宿主/容器运行时会注入的系统变量（与配置无关，不告警）。
const SYSTEM_ENV_PREFIXES: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_",
    "TMPDIR",
    "USER",
    "SHELL",
    "TERM",
    "PWD",
    "SHLVL",
    "TZ",
    "HOSTNAME",
    "SSH_",
    "XPC_",
    "__CF",
    "LS_COLORS",
    "COLORFGBG",
    "EDITOR",
    "RUST_BACKTRACE",
    "MYSQL_",
    "GATEWAY_",
    "WINDIR",
    "COMSPEC",
    "PROGRAMFILES",
    "ProgramFiles",
    "HOMEDRIVE",
    "HOMEPATH",
    "SYSTEMROOT",
    "SystemRoot",
    "SYSTEMDRIVE",
    "TEMP",
    "TMP",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "PUBLIC",
    "DriverData",
    "OS",
    "PROCESSOR_",
    "NUMBER_OF_",
    "PATHEXT",
    "PROMPT",
    "SESSIONNAME",
    "USERDOMAIN",
    "USERNAME",
    "COMPUTERNAME",
    "COMMONPROGRAM",
    "CommonProgram",
    "POWERSHELL",
    "EXEPATH",
    "HOMESHARE",
    "LOGONSERVER",
    "ONEHEADER",
];

/// 清单外且非系统前缀的变量名集合（用于启动告警，行为测试覆盖其语义）。
fn unknown_env_var_names(all: &[(String, String)]) -> Vec<&str> {
    all.iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| {
            !KNOWN_ENV_VARS.contains(name)
                && !SYSTEM_ENV_PREFIXES
                    .iter()
                    .any(|p| name.starts_with(p) || name == p)
        })
        .collect()
}

/// 对清单外变量打 WARN（不阻断启动：容器运行时变量难以穷举，漂移靠日志可查）。
fn warn_unknown_env_vars(all: &[(String, String)]) {
    for name in unknown_env_var_names(all) {
        tracing::warn!(
            var = %name,
            "未在冻结清单内的环境变量（若为配置项请核对拼写与 docs/operations.md）"
        );
    }
}

fn require(src: &impl EnvSource, name: &str) -> anyhow::Result<String> {
    match src.read(name) {
        Some(v) if !v.trim().is_empty() => Ok(v),
        Some(_) => anyhow::bail!("{name} 不能为空"),
        None => anyhow::bail!("缺少必需环境变量 {name}"),
    }
}

fn optional(src: &impl EnvSource, name: &str, default: &str) -> String {
    match src.read(name) {
        Some(v) if !v.trim().is_empty() => v,
        _ => default.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    const DB: &str = "mysql://u:p@127.0.0.1:3307/db";
    const REDIS: &str = "redis://127.0.0.1:6380";
    const SECRET_OK: &str = "0123456789abcdef0123456789abcdef";

    /// 测试用假环境：记录被查询的变量名供一致性测试。
    struct FakeEnv {
        vars: BTreeMap<String, String>,
        queried: RefCell<Vec<String>>,
    }

    impl FakeEnv {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Self {
                vars: pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
                queried: RefCell::new(Vec::new()),
            }
        }
    }

    impl EnvSource for FakeEnv {
        fn read(&self, name: &str) -> Option<String> {
            self.queried.borrow_mut().push(name.to_string());
            self.vars.get(name).cloned()
        }

        fn all(&self) -> Vec<(String, String)> {
            self.vars
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }
    }

    fn minimal_env() -> Vec<(&'static str, &'static str)> {
        vec![
            ("DATABASE_URL", DB),
            ("REDIS_URL", REDIS),
            ("JWT_SECRET", SECRET_OK),
        ]
    }

    #[test]
    fn valid_config_assembles_with_defaults() {
        let env = FakeEnv::new(&minimal_env());
        let cfg = Config::load(&env).unwrap();
        assert_eq!(cfg.listen.http_addr, "0.0.0.0:8080");
        assert_eq!(cfg.listen.device_addr, "0.0.0.0:9100");
        assert_eq!(cfg.auth.paid_start_timeout_secs, 300);
        // 未配置时默认：simulation / development。
        assert_eq!(cfg.payment_mode, PaymentMode::Simulation);
        assert_eq!(cfg.login_mode, LoginMode::Development);
        // 引导账号缺省为空（开发形态）。
        assert!(cfg.bootstrap.user.is_empty());
        // 导出缺省：空目录 + max_rows 0（service 层回退默认 50000）。
        assert_eq!(cfg.export.max_rows, 0);
    }

    #[test]
    fn payment_mode_values_and_fail_fast() {
        for (raw, want) in [
            ("simulation", PaymentMode::Simulation),
            ("wechat_direct", PaymentMode::WechatDirect),
            ("disabled", PaymentMode::Disabled),
        ] {
            let mut pairs = minimal_env();
            pairs.push(("PAYMENT_MODE", raw));
            let cfg = Config::load(&FakeEnv::new(&pairs)).unwrap();
            assert_eq!(cfg.payment_mode, want, "{raw} 应解析成功");
        }
        let mut pairs = minimal_env();
        pairs.push(("PAYMENT_MODE", "alipay"));
        let e = Config::load(&FakeEnv::new(&pairs)).err().unwrap();
        assert!(e.to_string().contains("PAYMENT_MODE 取值非法"), "{e}");
    }

    #[test]
    fn login_mode_values_and_fail_fast() {
        for (raw, want) in [
            ("development", LoginMode::Development),
            ("wechat", LoginMode::Wechat),
        ] {
            let mut pairs = minimal_env();
            pairs.push(("LOGIN_MODE", raw));
            let cfg = Config::load(&FakeEnv::new(&pairs)).unwrap();
            assert_eq!(cfg.login_mode, want, "{raw} 应解析成功");
        }
        let mut pairs = minimal_env();
        pairs.push(("LOGIN_MODE", "sms"));
        let e = Config::load(&FakeEnv::new(&pairs)).err().unwrap();
        assert!(e.to_string().contains("LOGIN_MODE 取值非法"), "{e}");
    }

    #[test]
    fn short_jwt_secret_rejected() {
        let mut pairs = minimal_env();
        pairs[2] = ("JWT_SECRET", "short");
        let e = Config::load(&FakeEnv::new(&pairs)).err().unwrap();
        assert!(e.to_string().contains("32 字节"));
    }

    #[test]
    fn empty_database_url_rejected() {
        let mut pairs = minimal_env();
        pairs[0] = ("DATABASE_URL", "");
        let e = Config::load(&FakeEnv::new(&pairs)).err().unwrap();
        assert!(e.to_string().contains("DATABASE_URL"));
    }

    #[test]
    fn missing_required_rejected() {
        let pairs = vec![("REDIS_URL", REDIS), ("JWT_SECRET", SECRET_OK)];
        let e = Config::load(&FakeEnv::new(&pairs)).err().unwrap();
        assert!(e.to_string().contains("缺少必需环境变量 DATABASE_URL"));
    }

    #[test]
    fn bad_listen_addr_rejected() {
        let mut pairs = minimal_env();
        pairs.push(("HTTP_ADDR", "not-an-addr"));
        let e = Config::load(&FakeEnv::new(&pairs)).err().unwrap();
        assert!(e.to_string().contains("HTTP_ADDR"));
    }

    #[test]
    fn wechat_group_passes_through() {
        let mut pairs = minimal_env();
        pairs.push(("WECHAT_APPID", "wx123"));
        let cfg = Config::load(&FakeEnv::new(&pairs)).unwrap();
        assert_eq!(cfg.wechat.appid, "wx123");
        // ADMIN_STATIC_DIR 已从清单移除，配置加载不再读取。
        pairs.push(("ADMIN_STATIC_DIR", " /srv/admin "));
        let env = FakeEnv::new(&pairs);
        Config::load(&env).unwrap();
        assert!(
            !env.queried.borrow().iter().any(|v| v == "ADMIN_STATIC_DIR"),
            "静态目录变量不再被配置读取"
        );
    }

    /// `Config::load` 实际读取的变量名集合须等于冻结清单（不含 `RUST_LOG`，
    /// 日志层单独直读）。
    #[test]
    fn frozen_list_equals_actually_read_set() {
        let env = FakeEnv::new(&minimal_env());
        Config::load(&env).unwrap();
        let mut read: Vec<String> = env.queried.borrow().clone();
        read.sort();
        let mut frozen: Vec<String> = KNOWN_ENV_VARS
            .iter()
            .filter(|v| **v != "RUST_LOG")
            .map(|v| (*v).to_string())
            .collect();
        frozen.sort();
        assert_eq!(read, frozen, "配置读取集合必须与冻结清单一致（漂移即失败）");
    }

    /// 清单外变量不阻断启动；`unknown_env_var_names` 只包含清单外且非系统前缀项。
    #[test]
    fn unknown_env_vars_warn_without_blocking() {
        let mut pairs = minimal_env();
        pairs.push(("MY_CUSTOM_SETTING", "x"));
        pairs.push(("PATH", "/usr/bin"));
        let env = FakeEnv::new(&pairs);
        Config::load(&env).expect("清单外变量不应阻断启动");
        let binding = env.all();
        let warned = super::unknown_env_var_names(&binding);
        assert!(
            warned.contains(&"MY_CUSTOM_SETTING"),
            "清单外自定义变量应告警: {warned:?}"
        );
        assert!(
            !warned.contains(&"PATH"),
            "系统前缀变量不应告警: {warned:?}"
        );
        assert!(
            !warned.contains(&"DATABASE_URL"),
            "清单内变量不应告警: {warned:?}"
        );
    }
}
