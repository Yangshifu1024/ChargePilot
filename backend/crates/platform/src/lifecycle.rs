//! 受管理后台任务框架：统一取消与等待，保证「先任务后连接池」的停机顺序。
//!
//! 停机顺序：先等受管后台任务全部退出，再关闭数据库、Redis 与连接池。

use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// 取消令牌：watch(bool) 的薄封装。
#[derive(Clone)]
pub struct CancelToken {
    rx: watch::Receiver<bool>,
}

impl CancelToken {
    /// 从既有 watch 通道构造（测试/自定义装配用）。
    pub fn from_watch(rx: tokio::sync::watch::Receiver<bool>) -> Self {
        Self { rx }
    }

    /// 是否已收到停机取消信号。
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// 取消时立即返回；用于 `tokio::select!` 分支。
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// 周期性等待：休眠或取消，先到者赢（后台循环的标准节拍）。
    pub async fn sleep_or_cancelled(&self, d: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(d) => {}
            _ = self.cancelled() => {}
        }
    }
}

/// 受管理任务集合：spawn 的任务在 shutdown 时全部收到取消并 join。
#[derive(Default)]
pub struct TaskManager {
    tx: Option<watch::Sender<bool>>,
    tasks: Vec<(String, JoinHandle<()>)>,
}

impl TaskManager {
    /// 创建任务管理器（初始未取消）。
    pub fn new() -> Self {
        let (tx, _) = watch::channel(false);
        Self {
            tx: Some(tx),
            tasks: Vec::new(),
        }
    }

    /// 当前取消令牌（spawn 的任务持它响应停机）。
    pub fn token(&self) -> CancelToken {
        CancelToken {
            rx: match self.tx.as_ref() {
                Some(tx) => tx.subscribe(),
                None => panic!("TaskManager 已关闭后再取 token()"),
            },
        }
    }

    /// 登记一个受管理任务（名字用于停机超时报告与日志）。
    pub fn spawn<F>(&mut self, name: impl Into<String>, fut: F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let name = name.into();
        let task_name = name.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = fut.await {
                tracing::error!(task = %task_name, error = %e, "后台任务异常退出");
            }
        });
        self.tasks.push((name, handle));
    }

    /// 停机：广播取消 → 逐个 join。返回超时未退出的任务名。
    pub async fn shutdown(mut self, timeout: Duration) -> Vec<String> {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(true);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut stuck: Vec<String> = Vec::new();
        for (name, handle) in self.tasks.drain(..) {
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remain, handle).await {
                Ok(_) => tracing::info!(task = %name, "后台任务已退出"),
                Err(_) => {
                    tracing::error!(task = %name, "后台任务停机超时");
                    stuck.push(name);
                }
            }
        }
        stuck
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_waits_for_tasks_then_times_out() {
        let mut tm = TaskManager::new();
        let token = tm.token();
        tm.spawn("quick", async move {
            token.sleep_or_cancelled(Duration::from_millis(10)).await;
            Ok(())
        });
        let token2 = tm.token();
        tm.spawn("stuck", async move {
            // 无视取消，模拟卡死
            tokio::time::sleep(Duration::from_secs(5)).await;
            let _ = token2;
            Ok(())
        });
        let started = std::time::Instant::now();
        let stuck = tm.shutdown(Duration::from_millis(100)).await;
        assert_eq!(stuck, vec!["stuck"]);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
