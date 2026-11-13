//! 微信身份交换：jscode2session 换 openid（wechat 模式）/ development 模式本地换购。

/// 交换得到的微信身份。
#[derive(Debug, Clone)]
pub struct WeChatIdentity {
    pub openid: String,
    pub unionid: Option<String>,
}

/// 身份交换器抽象：`wechat` 走微信渠道，`development` 本地构造。
pub enum Exchanger {
    Development,
    /// 微信渠道（appid/secret 由装配注入）。
    Wechat {
        appid: String,
        secret: String,
    },
}

impl Exchanger {
    /// 用登录 code 换微信身份。
    pub async fn exchange(&self, code: &str) -> Result<WeChatIdentity, String> {
        match self {
            Exchanger::Development => exchange_development(code),
            Exchanger::Wechat { appid, secret } => exchange_wechat(appid, secret, code).await,
        }
    }
}

/// development 模式：code 必须形如 `dev:<account>`（account 为 1–64 位字母数字下划线连字符），
/// 返回独立命名空间 openid（旧版语义）。
fn exchange_development(code: &str) -> Result<WeChatIdentity, String> {
    let Some(account) = code.strip_prefix("dev:") else {
        return Err("invalid development account".into());
    };
    let ok = !account.is_empty()
        && account.len() <= 64
        && account
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        return Err("invalid development account".into());
    }
    Ok(WeChatIdentity {
        openid: format!("development:{account}"),
        unionid: None,
    })
}

/// wechat 模式：调用 jscode2session。网络/协议失败返回可记录的错误串。
async fn exchange_wechat(appid: &str, secret: &str, code: &str) -> Result<WeChatIdentity, String> {
    // 阶段 04 引入 reqwest 后替换为真实调用；当前显式未实现，避免假装已接入
    let _ = (appid, secret, code);
    Err("wechat exchange requires network client (阶段 04 接入 reqwest)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn development_namespace() {
        let e = Exchanger::Development;
        let id = e.exchange("dev:yang").await.unwrap();
        assert_eq!(id.openid, "development:yang");
        assert!(id.unionid.is_none());
        // 非法 code
        assert!(e.exchange("yang").await.is_err());
        assert!(e.exchange("dev:").await.is_err());
        assert!(e.exchange("dev:有中文字符").await.is_err());
        assert!(
            e.exchange("dev:too-long-too-long-too-long-too-long-too-long-too-long!")
                .await
                .is_err()
        );
    }
}
