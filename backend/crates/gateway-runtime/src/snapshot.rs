//! 设备实时快照（G2 实时读数据源）：帧到达先更新内存快照（毫秒级），
//! 查询读快照；快照缺失时回退查库（设备重启后首次查询）。
//!
//! 快照只承载显示与估算（电量/功率/端口状态），不承载资金事实——
//! 计费与结算仍以数据库与冻结快照为准。

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// 单端口实时快照。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PortSnapshot {
    /// 端口号（从 1 开始）。
    pub port_no: u8,
    /// 设备上报状态码（0 空闲 / 1 充电 / 3 输出故障 / 4 粘连）。
    pub reported_status: Option<u8>,
    /// 已充时长（秒）。
    pub charged_seconds: u32,
    /// 已充电量（mWh）。
    pub charged_mwh: u32,
    /// 瞬时功率（0.1 W）。
    pub power_deci_watts: u32,
    /// 剩余授权时长（秒）。
    pub remaining_secs: u32,
}

/// 设备级实时快照。
#[derive(Debug, Clone, Default)]
pub struct DeviceSnapshot {
    /// 设备编号。
    pub device_id: String,
    /// 是否在线（有活跃会话）。
    pub online: bool,
    /// 信号强度。
    pub signal: u8,
    /// 整机状态位。
    pub device_status: u8,
    /// 输入电压（V）。
    pub voltage_v: u16,
    /// 温度（摄氏）。
    pub temperature_c: i16,
    /// 最近心跳时刻。
    pub last_heartbeat_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 端口快照（按端口号升序）。
    pub ports: Vec<PortSnapshot>,
}

impl DeviceSnapshot {
    /// 取端口快照。
    #[must_use]
    pub fn port(&self, port_no: u8) -> Option<&PortSnapshot> {
        self.ports.iter().find(|p| p.port_no == port_no)
    }

    /// 是否有端口在充电。
    #[must_use]
    pub fn any_charging(&self) -> bool {
        self.ports
            .iter()
            .any(|p| p.power_deci_watts > 0 || p.charged_seconds > 0)
    }
}

/// 进程内快照表。
#[derive(Default)]
pub struct SnapshotStore {
    map: RwLock<HashMap<String, DeviceSnapshot>>,
    /// 快照缺失回退查库的次数（可观测）。
    db_fallbacks: AtomicU64,
}

impl SnapshotStore {
    /// 空表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 更新心跳快照（帧到达先更新，实时读数据源）；
    /// 参数即帧字段契约（信号/状态/电压/温度/端口态），不引入参数对象。
    #[allow(clippy::too_many_arguments)]
    pub fn apply_heartbeat(
        &self,
        device_id: &str,
        signal: u8,
        device_status: u8,
        voltage_v: u16,
        temperature_c: i16,
        has_port_status: bool,
        port_states: &[u8],
        charging: &[chargepilot_protocol::PortTelemetry],
    ) {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        let snap = map
            .entry(device_id.to_string())
            .or_insert_with(|| DeviceSnapshot {
                device_id: device_id.to_string(),
                online: true,
                ..DeviceSnapshot::default()
            });
        snap.online = true;
        snap.signal = signal;
        snap.last_heartbeat_at = Some(chrono::Utc::now());
        if has_port_status {
            snap.device_status = device_status;
            snap.voltage_v = voltage_v;
            snap.temperature_c = temperature_c;
            if snap.ports.len() < port_states.len() {
                snap.ports
                    .resize(port_states.len(), PortSnapshot::default());
            }
            for (idx, state) in port_states.iter().enumerate() {
                snap.ports[idx].port_no = (idx + 1) as u8;
                snap.ports[idx].reported_status = Some(*state);
                // 非充电端口清零遥测（状态切换后的陈旧读数不残留）。
                if *state != 1 {
                    let port = &mut snap.ports[idx];
                    port.power_deci_watts = 0;
                    port.charged_seconds = 0;
                    port.charged_mwh = 0;
                    port.remaining_secs = 0;
                }
            }
            for port in charging {
                let idx = usize::from(port.port) - 1;
                if idx < snap.ports.len() {
                    let slot = &mut snap.ports[idx];
                    slot.port_no = port.port;
                    slot.remaining_secs = port.remaining_secs;
                    slot.charged_seconds = port.charged_seconds;
                    slot.charged_mwh = port.charged_mwh;
                    slot.power_deci_watts = port.power_deci_watts;
                }
            }
        }
    }

    /// 更新单端口遥测（C2 分档上报等独立遥测帧）。
    pub fn apply_port_telemetry(
        &self,
        device_id: &str,
        port: &chargepilot_protocol::PortTelemetry,
    ) {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        let snap = map
            .entry(device_id.to_string())
            .or_insert_with(|| DeviceSnapshot {
                device_id: device_id.to_string(),
                online: true,
                ..DeviceSnapshot::default()
            });
        if snap.ports.len() < usize::from(port.port) {
            snap.ports
                .resize(usize::from(port.port), PortSnapshot::default());
        }
        let slot = &mut snap.ports[usize::from(port.port) - 1];
        slot.port_no = port.port;
        slot.remaining_secs = port.remaining_secs;
        slot.power_deci_watts = port.power_deci_watts;
    }

    /// 设备离线：快照标记下线（保留最后遥测供显示回退）。
    pub fn mark_offline(&self, device_id: &str) {
        if let Some(snap) = self
            .map
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(device_id)
        {
            snap.online = false;
        }
    }

    /// 读快照；缺失时经 loader 回退查库并缓存。
    pub async fn get_or_load<F, Fut>(&self, device_id: &str, loader: F) -> Option<DeviceSnapshot>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Option<DeviceSnapshot>>,
    {
        if let Some(snap) = self
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
        {
            return Some(snap.clone());
        }
        // 快照缺失回退查库（设备重启后首次查询）。
        self.db_fallbacks.fetch_add(1, Ordering::Relaxed);
        let loaded = loader(device_id.to_string()).await;
        if let Some(snap) = &loaded {
            self.map
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(device_id.to_string(), snap.clone());
        }
        loaded
    }

    /// 回退查库计数。
    #[must_use]
    pub fn db_fallbacks(&self) -> u64 {
        self.db_fallbacks.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn heartbeat_updates_and_clears_stale_ports() {
        let store = SnapshotStore::new();
        store.apply_heartbeat(
            "dev-1",
            20,
            0,
            220,
            25,
            true,
            &[0, 1],
            &[chargepilot_protocol::PortTelemetry {
                port: 2,
                remaining_secs: 600,
                charged_seconds: 100,
                charged_mwh: 50_000,
                power_deci_watts: 1200,
                ..Default::default()
            }],
        );
        let snap = store
            .get_or_load("dev-1", |_| async { None })
            .await
            .unwrap();
        assert!(snap.online);
        assert_eq!(snap.signal, 20);
        assert_eq!(snap.ports.len(), 2);
        assert_eq!(snap.port(2).unwrap().power_deci_watts, 1200);
        assert!(snap.any_charging());
        // 空闲后的心跳清零陈旧遥测。
        store.apply_heartbeat("dev-1", 20, 0, 220, 25, true, &[0, 0], &[]);
        let snap = store
            .get_or_load("dev-1", |_| async { None })
            .await
            .unwrap();
        assert!(!snap.any_charging());
        assert_eq!(snap.port(2).unwrap().charged_seconds, 0);
        // 离线标记。
        store.mark_offline("dev-1");
        let snap = store
            .get_or_load("dev-1", |_| async { None })
            .await
            .unwrap();
        assert!(!snap.online);
    }

    #[tokio::test]
    async fn fallback_loader_caches_once() {
        let store = SnapshotStore::new();
        let loads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = loads.clone();
        let snapshot = store
            .get_or_load("dev-2", |id| async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some(DeviceSnapshot {
                    device_id: id,
                    online: false,
                    ..DeviceSnapshot::default()
                })
            })
            .await
            .unwrap();
        assert_eq!(snapshot.device_id, "dev-2");
        assert_eq!(loads.load(std::sync::atomic::Ordering::Relaxed), 1);
        // 二次查询走缓存，不再回退。
        let _ = store.get_or_load("dev-2", |_| async { None }).await;
        assert_eq!(loads.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(store.db_fallbacks(), 1);
    }
}
