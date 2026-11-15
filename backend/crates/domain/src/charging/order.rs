//! 订单展示与结算只读取购买时冻结的快照：从快照还原计价配置和展示名称。
//! 语义基准：旧仓库 internal/domain/charging/order_package.go，逐行对齐；
//! 快照读取由仓储负责，快照损坏时不能用当前模板补齐。

use crate::charging::actual::Rule;
use crate::charging::settle::Offer;
use crate::charging::spec::ChargeMode;
use serde::Deserialize;

/// 订单在创建支付意图时冻结下来的完整计费规则（旧版 OrderPackage）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrderPackage {
    /// 购买套餐及冻结价格，不能由当前模板替换。
    pub offer: Offer,
    /// 完整冻结计价方案，旧计价规则可能缺失。
    pub scheme: Option<crate::charging::scheme::Scheme>,
    /// 既有规则的计费执行方式。
    pub billing_mode: ChargeMode,
    /// 冻结规则版本。
    pub rule_version: u32,
    /// 卡累计购买记录，非刷卡订单为空。
    pub card: Option<OrderCardPurchase>,
}

/// 随单冻结的刷卡购买信息（旧版 OrderCardPurchase）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OrderCardPurchase {
    /// 累计实际购买金额，分。
    pub paid_cents: i64,
    /// 累计购买分钟，续购后可增加。
    pub purchased_minutes: u16,
    /// 冻结允许购买分钟上限。
    pub max_minutes: u16,
}

#[derive(Debug, Deserialize)]
struct FrozenSnapshot {
    #[serde(default)]
    rule: Rule,
    offer: Option<Offer>,
}

impl OrderPackage {
    /// 还原购买时的计价配置（旧版 PackageFromSnapshot）。
    /// 快照损坏、缺少套餐或套餐无效时返回 None，不能用当前模板补齐。
    pub fn from_snapshot(raw: &[u8]) -> Option<OrderPackage> {
        let frozen: FrozenSnapshot = serde_json::from_slice(raw).ok()?;
        let offer = frozen.offer?;
        if !offer.valid() {
            return None;
        }
        Some(OrderPackage {
            billing_mode: frozen.rule.spec.mode.clone(),
            rule_version: frozen.rule.version,
            scheme: frozen.rule.spec.scheme.clone(),
            offer,
            card: None,
        })
    }
}

/// 读取冻结方案名称；没有名称时返回 None，不能用套餐名替代（旧版 SelectedSchemeName）。
pub fn selected_scheme_name(selected: &OrderPackage) -> Option<String> {
    let scheme = selected.scheme.as_ref()?;
    let name = scheme.name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 读冻结的价格档名称（旧版 SelectedPackageName）。
pub fn selected_package_name(selected: &OrderPackage) -> Option<String> {
    let name = selected.offer.name.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

use serde::Serialize;
