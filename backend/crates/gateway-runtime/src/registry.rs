//! 进程内会话注册表：设备到活跃连接的映射与同设备互踢（G1）。

use std::collections::HashMap;
use std::sync::Arc;

use chargepilot_protocol::{BoxFut, DeviceSession};

/// 会话条目：设备到活跃连接的映射值。
struct Entry {
    session: Arc<dyn DeviceSession>,
    on_replaced: Arc<dyn Fn() + Send + Sync>,
}

/// 进程内会话注册表：同设备互踢（G1）。
/// 断线重连的设备以新连接原子替换旧连接，旧连接被关闭并标记 replaced，
/// 不产生双会话。锁粒度为整表（登记/查询频率为设备连接级，非每帧）。
#[derive(Default)]
pub struct SessionRegistry {
    inner: std::sync::Mutex<HashMap<String, Entry>>,
}

impl SessionRegistry {
    /// 空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl chargepilot_protocol::SessionRouter for SessionRegistry {
    fn attach(
        &self,
        device_id: &str,
        session: Arc<dyn DeviceSession>,
        on_replaced: Arc<dyn Fn() + Send + Sync>,
    ) -> BoxFut<bool> {
        let kicked = {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let kicked = match map.insert(
                device_id.to_string(),
                Entry {
                    session: session.clone(),
                    on_replaced: on_replaced.clone(),
                },
            ) {
                // 原子替换：先更新表再通知旧连接，旧连接 detach 时已不匹配。
                Some(old) if !Arc::ptr_eq(&old.session, &session) => {
                    (old.on_replaced)();
                    Some(old.session)
                }
                _ => None,
            };
            metrics::gauge!("gateway_online_sessions").set(map.len() as f64);
            kicked
        };
        Box::pin(async move {
            // 旧会话关闭在放锁后 await：close 只触发通知不阻塞，
            // 互踢语义（读半退出 + replaced 标记）由会话自身完成。
            match kicked {
                Some(old) => {
                    old.close().await;
                    true
                }
                None => false,
            }
        })
    }

    fn get(&self, device_id: &str) -> BoxFut<Option<Arc<dyn DeviceSession>>> {
        let found = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .map(|e| e.session.clone());
        Box::pin(async move { found })
    }

    fn detach(&self, device_id: &str, session: &Arc<dyn DeviceSession>) -> BoxFut<()> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get(device_id)
            && Arc::ptr_eq(&entry.session, session)
        {
            map.remove(device_id);
        }
        metrics::gauge!("gateway_online_sessions").set(map.len() as f64);
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chargepilot_protocol::{Command, SessionRouter as _};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeSession {
        closed: AtomicBool,
    }

    impl DeviceSession for FakeSession {
        fn send(&self, _command: Command) -> BoxFut<anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(&self) -> BoxFut<()> {
            self.closed.store(true, Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn attach_replaces_and_kicks_old_session() {
        let registry = Arc::new(SessionRegistry::new());
        let old = Arc::new(FakeSession::default());
        let new = Arc::new(FakeSession::default());
        let kicked = Arc::new(AtomicUsize::new(0));
        let flag = kicked.clone();
        let replaced_old = registry
            .attach(
                "dev-1",
                old.clone(),
                Arc::new(move || {
                    flag.fetch_add(1, Ordering::SeqCst);
                }),
            )
            .await;
        assert!(!replaced_old, "首次登记不算替换");
        // 同设备登记新连接：旧连接被关闭并回调。
        let replaced_new = registry.attach("dev-1", new.clone(), Arc::new(|| {})).await;
        assert!(replaced_new, "替换旧会话");
        assert_eq!(kicked.load(Ordering::SeqCst), 1);
        assert!(old.closed.load(Ordering::SeqCst), "旧连接被互踢关闭");
        assert!(!new.closed.load(Ordering::SeqCst));
        // 取当前会话：新连接。
        let current = registry.get("dev-1").await.unwrap();
        assert!(Arc::ptr_eq(
            &current,
            &(new.clone() as Arc<dyn DeviceSession>)
        ));
    }

    #[tokio::test]
    async fn detach_only_removes_own_session() {
        let registry = Arc::new(SessionRegistry::new());
        let first = Arc::new(FakeSession::default());
        registry
            .attach("dev-2", first.clone(), Arc::new(|| {}))
            .await;
        // 他会话 detach 不影响登记。
        let stranger = Arc::new(FakeSession::default());
        registry
            .detach("dev-2", &(stranger.clone() as Arc<dyn DeviceSession>))
            .await;
        assert!(registry.get("dev-2").await.is_some());
        // 本会话 detach 移除登记。
        registry
            .detach("dev-2", &(first.clone() as Arc<dyn DeviceSession>))
            .await;
        assert!(registry.get("dev-2").await.is_none());
    }
}
