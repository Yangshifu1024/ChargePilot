//! C 端充电路由：用户停止充电（路径对齐旧版 /charging/{order_no}/stop）。
//! 授权与计费截止冻结在服务层事务内；停止命令下沉经 CommandSink
//! （阶段 06 前为 simulation 实现）。

use crate::deps::ApiDeps;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use chargepilot_platform::httpx;
use serde_json::json;
use std::sync::Arc;

pub fn router() -> axum::Router<Arc<ApiDeps>> {
    axum::Router::new().route(
        "/api/v1/user/charging/{order_no}/stop",
        axum::routing::post(user_stop),
    )
}

/// 用户停止充电：归属校验 + 计费截止冻结 + 停止命令下发。
async fn user_stop(
    State(deps): State<Arc<ApiDeps>>,
    headers: HeaderMap,
    Path(order_no): Path<String>,
) -> Response {
    let rid = httpx::request_id_from(&headers);
    let Some(stop) = &deps.charging_stop else {
        return crate::respond(503, 5003, "充电服务未装配", &rid);
    };
    let uid = match crate::device_and_user::user_id_from_pub(&deps, &headers).await {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    match stop.stop(&order_no, uid).await {
        Ok(active) => httpx::ok(
            json!({
                "charge_order_id": active.charge_order_id,
                "order_no": active.order_no,
                "status": "stopping",
            }),
            &rid,
        ),
        Err(chargepilot_mod_charging::StopError::NotFound) => {
            crate::respond(404, 1004, "订单不存在或不可停止", &rid)
        }
        Err(chargepilot_mod_charging::StopError::PortIdentity) => {
            crate::respond(409, 2009, "端口身份缺失", &rid)
        }
        Err(chargepilot_mod_charging::StopError::SendFailed(m)) => {
            tracing::warn!(error = %m, "停止命令下发失败");
            crate::respond(502, 3001, "停止命令下发失败，请重试", &rid)
        }
        Err(e) => {
            tracing::warn!(error = %e, "停止请求处理失败");
            crate::respond(503, 5003, "停止请求暂不可用", &rid)
        }
    }
}
