//! 支付渠道抽象与模拟渠道。
//! 渠道三模式 disabled / simulation / wechat_direct（spec 决策）；本批交付
//! simulation 与 disabled；wechat_direct 由同目录 `wechat` 模块的 APIv3 客户端实现。

use serde::{Deserialize, Serialize};

/// 渠道支付能力。
/// 实现必须可重入：同一商户单号重复请求返回等价参数。
pub trait PayProvider: Send + Sync {
    /// 以商户单号请求预支付参数；失败不产生资金，调用方可原单号重试。
    fn prepay<'a>(
        &'a self,
        request: PrepayRequest,
    ) -> futures_bounds::BoxFuture<'a, Result<PrepayParams, String>>;
}

/// dyn 兼容的 Future 别名（与 wallet::recharge 同一模式）。
pub mod futures_bounds {
    pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
}

/// 预支付请求。
#[derive(Debug, Clone)]
pub struct PrepayRequest {
    /// 商户支付单号（P 前缀）。
    pub merchant_order_no: String,
    /// 付款人微信 openid（simulation 忽略）。
    pub open_id: String,
    /// 应付金额，分。
    pub amount_cents: i64,
    /// 支付截止（UTC 微秒）。
    pub expires_at_unix_micros: i64,
}

/// 预支付参数；渠道透传给客户端唤起支付。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrepayParams {
    /// 渠道预支付会话标识（simulation 为 SIM 前缀编号）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prepay_id: String,
    /// 唤起支付所需的完整参数 JSON（微信为 SDK 签名参数）。
    #[serde(flatten)]
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl PrepayParams {
    pub fn is_empty(&self) -> bool {
        self.prepay_id.is_empty() && self.fields.is_empty()
    }
}

/// 渠道模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayMode {
    /// 未配置渠道：预支付直接失败，申请保留。
    Disabled,
    /// 模拟渠道：本地确认到账，全流程无真实商户。
    Simulation,
    /// 微信 APIv3 直连（真实商户预支付与回调）。
    WechatDirect,
}

/// 模拟渠道：返回稳定的预支付参数。
#[derive(Debug, Clone, Copy, Default)]
pub struct SimulationProvider;

impl PayProvider for SimulationProvider {
    fn prepay<'a>(
        &'a self,
        request: PrepayRequest,
    ) -> futures_bounds::BoxFuture<'a, Result<PrepayParams, String>> {
        Box::pin(async move {
            if request.amount_cents <= 0 || request.merchant_order_no.is_empty() {
                return Err("模拟渠道参数无效".to_string());
            }
            // 模拟渠道 JSON 仅含 provider 字段（前端按
            // provider==='simulation' 识别模拟渠道并用 prepay_id 前缀还原单号）。
            let mut fields = serde_json::Map::new();
            fields.insert(
                "provider".to_string(),
                serde_json::Value::String("simulation".to_string()),
            );
            Ok(PrepayParams {
                prepay_id: format!("SIM{}", request.merchant_order_no),
                fields,
            })
        })
    }
}

/// 未配置渠道：预支付恒失败。
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledProvider;

impl PayProvider for DisabledProvider {
    fn prepay<'a>(
        &'a self,
        _request: PrepayRequest,
    ) -> futures_bounds::BoxFuture<'a, Result<PrepayParams, String>> {
        Box::pin(async { Err("支付渠道未配置".to_string()) })
    }
}
