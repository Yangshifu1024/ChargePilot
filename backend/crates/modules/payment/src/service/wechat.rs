//! 微信支付 APIv3 自研封装。
//! 覆盖：请求签名（SHA256withRSA + Authorization 头）、平台证书验签、
//! 回调资源 AES-256-GCM 解密、敏感信息 RSA-OAEP 加密、退款请求构造。
//! HTTP 外呼经 [`HttpExecutor`] 窄接口注入：单元测试用内存桩，
//! 进程装配按 PAYMENT_MODE 选择 simulation 或 wechat_direct（真实商户联调
//! 真实联调前以官方测试向量与桩流程验收）。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};

/// 微信 APIv3 客户端凭据与密钥（进程启动时从配置加载）。
#[derive(Debug, Clone)]
pub struct WechatConfig {
    /// 商户号。
    pub mch_id: String,
    /// 商户 API 证书序列号。
    pub mch_serial_no: String,
    /// 商户 API 私钥（PEM 解析后的 PKCS#8）。
    pub private_key: RsaPrivateKey,
    /// 平台证书（序列号 → 公钥），验签与敏感信息加密用；轮换期可多把。
    pub platform_certs: Vec<(String, RsaPublicKey)>,
    /// APIv3 密钥（32 字节）。
    pub api_v3_key: [u8; 32],
}

/// 微信凭据原始字符串组。
#[derive(Debug, Clone, Default)]
pub struct WechatCredsRaw {
    pub mch_id: String,
    pub mch_serial_no: String,
    pub key_pem: String,
    /// 平台证书（序列号,PEM;... 多条分号分隔）。
    pub certs_raw: String,
    pub api_v3: String,
}

impl WechatConfig {
    /// 解析凭据原始字符串（来源为统一配置的微信组）。缺项返回错误，
    /// 由调用方决定降级模式。
    pub fn parse(raw: WechatCredsRaw) -> anyhow::Result<Self> {
        let WechatCredsRaw {
            mch_id,
            mch_serial_no,
            key_pem,
            certs_raw,
            api_v3,
        } = raw;
        if mch_id.is_empty()
            || mch_serial_no.is_empty()
            || key_pem.is_empty()
            || certs_raw.is_empty()
            || api_v3.len() != 32
        {
            anyhow::bail!("微信支付凭据缺失或 APIv3 密钥长度不为 32");
        }
        use rsa::pkcs8::DecodePrivateKey;
        let private_key = RsaPrivateKey::from_pkcs8_pem(&key_pem)
            .map_err(|e| anyhow::anyhow!("商户私钥解析失败: {e}"))?;
        let mut platform_certs = Vec::new();
        for entry in certs_raw.split(';') {
            let Some((serial, pem)) = entry.split_once(',') else {
                anyhow::bail!("平台证书配置格式须为 序列号,PEM；多条以分号分隔");
            };
            use rsa::pkcs8::DecodePublicKey;
            let key = RsaPublicKey::from_public_key_pem(pem)
                .map_err(|e| anyhow::anyhow!("平台证书公钥解析失败: {e}"))?;
            platform_certs.push((serial.trim().to_string(), key));
        }
        let mut api_v3_key = [0u8; 32];
        api_v3_key.copy_from_slice(api_v3.as_bytes());
        Ok(Self {
            mch_id,
            mch_serial_no,
            private_key,
            platform_certs,
            api_v3_key,
        })
    }
}

/// APIv3 请求签名（官方规范）：签名串 = 方法\nURL路径\n时间戳\n随机串\n报文\n，
/// SHA256withRSA 后 Base64。GET 的报文部分为空串。
pub fn sign_request(config: &WechatConfig, message: &str) -> anyhow::Result<String> {
    let signing_key = SigningKey::<Sha256>::new(config.private_key.clone());
    let signature = signing_key.sign(message.as_bytes());
    Ok(base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()))
}

/// 构造 Authorization 头值（官方 WECHATPAY2-SHA256-RSA2048 格式）。
pub fn authorization_header(
    config: &WechatConfig,
    method: &str,
    url_path: &str,
    timestamp: i64,
    nonce: &str,
    body: &str,
) -> anyhow::Result<String> {
    let message = format!("{method}\n{url_path}\n{timestamp}\n{nonce}\n{body}\n");
    let signature = sign_request(config, &message)?;
    Ok(format!(
        "WECHATPAY2-SHA256-RSA2048 mchid=\"{}\",nonce_str=\"{}\",signature=\"{}\",timestamp=\"{}\",serial_no=\"{}\"",
        config.mch_id, nonce, signature, timestamp, config.mch_serial_no
    ))
}

/// 平台证书验签（应答 Wechatpay-Signature 头与通知签名）。
/// 签名串 = 时间戳\n随机串\n报文\n；按证书序列号选公钥。
pub fn verify_platform_signature(
    config: &WechatConfig,
    serial: &str,
    timestamp: &str,
    nonce: &str,
    body: &str,
    signature_b64: &str,
) -> anyhow::Result<bool> {
    let Some((_, key)) = config.platform_certs.iter().find(|(s, _)| s == serial) else {
        anyhow::bail!("未知平台证书序列号: {serial}");
    };
    let message = format!("{timestamp}\n{nonce}\n{body}\n");
    let signature = base64::engine::general_purpose::STANDARD
        .decode(signature_b64)
        .ok()
        .and_then(|b| Signature::try_from(&b[..]).ok());
    let Some(signature) = signature else {
        return Ok(false);
    };
    let verifying_key = VerifyingKey::<Sha256>::new(key.clone());
    Ok(verifying_key.verify(message.as_bytes(), &signature).is_ok())
}

/// 回调资源解密（官方 AES-256-GCM）：key=APIv3 密钥，nonce 12 字节，
/// associated_data 原样传入，ciphertext 为 Base64（末 16 字节为 GCM 标签）。
pub fn decrypt_callback_resource(
    config: &WechatConfig,
    nonce: &str,
    associated_data: &str,
    ciphertext_b64: &str,
) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new((&config.api_v3_key).into());
    let nonce_bytes = nonce.as_bytes();
    if nonce_bytes.len() != 12 {
        anyhow::bail!("回调 nonce 长度须为 12");
    }
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(ciphertext_b64)
        .map_err(|e| anyhow::anyhow!("回调解文 Base64 解码失败: {e}"))?;
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            aes_gcm::aead::Payload {
                msg: &ciphertext,
                aad: associated_data.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("回调资源解密失败（密钥或数据不匹配）"))
}

/// 敏感信息加密（官方规范：平台证书公钥 RSA-OAEP，输出 Base64）。
pub fn encrypt_sensitive(
    config: &WechatConfig,
    serial: &str,
    plaintext: &str,
) -> anyhow::Result<String> {
    let Some((_, key)) = config.platform_certs.iter().find(|(s, _)| s == serial) else {
        anyhow::bail!("未知平台证书序列号: {serial}");
    };
    use rsa::Oaep;
    use rsa::sha2::Sha256;
    let padded = key
        .encrypt(
            &mut rsa::rand_core::OsRng,
            Oaep::new::<Sha256>(),
            plaintext.as_bytes(),
        )
        .map_err(|e| anyhow::anyhow!("敏感信息加密失败: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(padded))
}

/// 回调通知签名字段（官方 Wechatpay-Timestamp/Nonce/Signature/Serial 头集合）。
#[derive(Debug, Clone)]
pub struct CallbackHeaders {
    pub timestamp: String,
    pub nonce: String,
    pub signature: String,
    pub serial: String,
}

/// 验证并解密一条支付/退款回调：先验签，后 AES-GCM 解密 resource。
/// 返回解密后的资源 JSON 字节（含 out_trade_no/transaction_id 等字段）。
pub fn verify_and_decrypt_callback(
    config: &WechatConfig,
    headers: &CallbackHeaders,
    body: &str,
) -> anyhow::Result<Vec<u8>> {
    if !verify_platform_signature(
        config,
        &headers.serial,
        &headers.timestamp,
        &headers.nonce,
        body,
        &headers.signature,
    )? {
        anyhow::bail!("回调验签失败");
    }
    let payload: serde_json::Value =
        serde_json::from_str(body).map_err(|e| anyhow::anyhow!("回调报文不是合法 JSON: {e}"))?;
    let resource = &payload["resource"];
    let nonce = resource["nonce"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("回调缺少 resource.nonce"))?;
    let ciphertext = resource["ciphertext"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("回调缺少 resource.ciphertext"))?;
    let associated = resource["associated_data"].as_str().unwrap_or("");
    decrypt_callback_resource(config, nonce, associated, ciphertext)
}

/// 渠道退款请求构造（POST /v3/refund/domestic/refunds 的报文体）。
/// out_request_no 为幂等键（对应新版 refund_record.refund_no）。
pub fn build_refund_body(
    transaction_id: &str,
    out_request_no: &str,
    refund_cents: i64,
    total_cents: i64,
    reason: &str,
    notify_url: &str,
) -> serde_json::Value {
    serde_json::json!({
        "transaction_id": transaction_id,
        "out_request_no": out_request_no,
        "amount": {"refund": refund_cents, "total": total_cents, "currency": "CNY"},
        "reason": reason,
        "notify_url": notify_url,
    })
}

/// HTTP 执行器窄接口：wechat_direct 模式的真实外呼点。
/// 返回 (状态码, 响应体)；超时与网络错误映射为 Err（渠道未知结果，
/// 调用方按批次语义重查，不得直接判定失败）。
/// 外呼应答 Future（状态码 + 响应体文本；错误以 String 描述，见 trait 契约）。
pub type WechatHttpFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(u16, String), String>> + Send + 'a>>;

pub trait HttpExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        authorization: &'a str,
        body: &'a str,
    ) -> WechatHttpFuture<'a>;
}

/// 真实外呼执行器：显式未实现。
/// 签名、请求体与响应解析已就绪并有测试覆盖；接通 TLS 外呼（reqwest）
/// 属上线前清单，接入前 wechat_direct 模式的预支付会明确失败而非假装成功。
pub struct WechatHttpExecutor;

impl HttpExecutor for WechatHttpExecutor {
    fn execute<'a>(
        &'a self,
        _method: &'a str,
        _url: &'a str,
        _authorization: &'a str,
        _body: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(u16, String), String>> + Send + 'a>,
    > {
        Box::pin(async {
            Err(
                "微信 APIv3 真实外呼未接入（上线前清单）：签名与解析已就绪，待 TLS 客户端接通"
                    .to_string(),
            )
        })
    }
}

/// 微信直连支付提供者（PayProvider 的 wechat_direct 实现）。
/// 下单走 JSAPI /v3/pay/transactions/jsapi；响应 prepay_id 组装小程序拉起参数。
pub struct WechatDirectProvider {
    pub config: WechatConfig,
    /// 小程序 AppID（商户级常量，随配置注入）。
    pub appid: String,
    pub http: std::sync::Arc<dyn HttpExecutor>,
    pub notify_url: String,
}

impl super::channel::PayProvider for WechatDirectProvider {
    fn prepay<'a>(
        &'a self,
        request: super::channel::PrepayRequest,
    ) -> super::channel::futures_bounds::BoxFuture<'a, Result<super::channel::PrepayParams, String>>
    {
        Box::pin(async move {
            let body = serde_json::json!({
                "appid": self.appid,
                "mchid": self.config.mch_id,
                "description": "充电服务",
                "out_trade_no": request.merchant_order_no,
                "notify_url": self.notify_url,
                "amount": {"total": request.amount_cents, "currency": "CNY"},
                "payer": {"openid": request.open_id},
            })
            .to_string();
            let timestamp = chrono::Utc::now().timestamp();
            let nonce = super::checkout::new_uuid_v4().replace('-', "");
            let authorization = authorization_header(
                &self.config,
                "POST",
                "/v3/pay/transactions/jsapi",
                timestamp,
                &nonce,
                &body,
            )
            .map_err(|e| e.to_string())?;
            let (status, resp) = self
                .http
                .execute("POST", "/v3/pay/transactions/jsapi", &authorization, &body)
                .await
                .map_err(|e| format!("微信下单网络错误: {e}"))?;
            if status != 200 {
                return Err(format!("微信下单失败（HTTP {status}）: {resp}"));
            }
            let parsed: serde_json::Value =
                serde_json::from_str(&resp).map_err(|e| format!("微信下单响应解析失败: {e}"))?;
            let prepay_id = parsed["prepay_id"]
                .as_str()
                .ok_or_else(|| "微信下单响应缺少 prepay_id".to_string())?;
            // 小程序拉起参数（官方二次签名串：appId\n时间戳\n随机串\npackage\n）。
            let pay_sign_message = format!(
                "{}\n{}\n{}\nprepay_id={prepay_id}\n",
                self.appid, timestamp, nonce
            );
            let pay_sign =
                sign_request(&self.config, &pay_sign_message).map_err(|e| e.to_string())?;
            let mut fields = serde_json::Map::new();
            fields.insert("appId".into(), serde_json::json!(self.appid));
            fields.insert("timeStamp".into(), serde_json::json!(timestamp.to_string()));
            fields.insert("nonceStr".into(), serde_json::json!(nonce));
            fields.insert(
                "package".into(),
                serde_json::json!(format!("prepay_id={prepay_id}")),
            );
            fields.insert("signType".into(), serde_json::json!("RSA"));
            fields.insert("paySign".into(), serde_json::json!(pay_sign));
            Ok(super::channel::PrepayParams {
                prepay_id: prepay_id.to_string(),
                fields,
            })
        })
    }
}
