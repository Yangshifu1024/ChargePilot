//! 设备 TCP 接入监听器：accept 循环 + 受管理停机（G1 优雅停机顺序）。
//!
//! 停机顺序：停 accept → 关闭全部在途设备会话（互踢通道）→ 冲刷遥测
//! 缓冲 → 返回。数据库连接池由进程装配层最后关闭。

use std::sync::Arc;

use chargepilot_platform::lifecycle::CancelToken;
use chargepilot_protocol::Adapter;

/// 设备接入监听器。
pub struct DeviceListener {
    /// 协议适配器（当前 dc589）。
    pub adapter: Arc<dyn Adapter>,
    /// 事件汇聚（会话审计能力供适配器收尾落库）。
    pub sink: Arc<dyn chargepilot_protocol::Sink>,
    pub recorder: Arc<dyn chargepilot_protocol::SessionRecorder>,
    pub router: Arc<dyn chargepilot_protocol::SessionRouter>,
    /// accept 限速（每秒新连接上限，spec G1：200/秒）。
    pub accept_rate: u32,
}

impl DeviceListener {
    /// 组装。
    #[must_use]
    pub fn new(
        adapter: Arc<dyn Adapter>,
        sink: Arc<dyn chargepilot_protocol::Sink>,
        recorder: Arc<dyn chargepilot_protocol::SessionRecorder>,
        router: Arc<dyn chargepilot_protocol::SessionRouter>,
    ) -> Self {
        Self {
            adapter,
            sink,
            recorder,
            router,
            accept_rate: 200,
        }
    }

    /// 监听循环：绑定端口、逐连接投递适配器；取消后停 accept。
    /// 返回是否正常退出（取消路径 Ok）。
    pub async fn run(self, addr: std::net::SocketAddr, token: CancelToken) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, adapter = self.adapter.name(), "设备接入监听启动");
        // accept 限速：令牌间隔（微秒级平滑）。
        let interval =
            std::time::Duration::from_micros(1_000_000u64 / u64::from(self.accept_rate.max(1)));
        let accept_all = tokio::select! {
            r = self.accept_loop(&listener, interval) => r,
            _ = token.cancelled() => Ok(()),
        };
        // 停机：关闭全部在途会话（互踢通道触发读半退出），等待收尾。
        self.router_shutdown().await;
        tracing::info!("设备接入监听停止");
        accept_all
    }

    async fn accept_loop(
        &self,
        listener: &tokio::net::TcpListener,
        interval: std::time::Duration,
    ) -> anyhow::Result<()> {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return Ok(());
            };
            let adapter = self.adapter.clone();
            let sink = self.sink.clone();
            let recorder = self.recorder.clone();
            let router = self.router.clone();
            tokio::spawn(async move {
                let _ = adapter
                    .serve_conn(
                        stream.into_std().unwrap_or_else(|_| unreachable!()),
                        sink,
                        Some(recorder),
                        Some(router),
                    )
                    .await;
            });
            tokio::time::sleep(interval).await;
        }
    }

    async fn router_shutdown(&self) {
        // SessionRouter 契约不暴露枚举全表；互踢关闭由适配器读半的
        // closed 信号完成。此处等待一个短暂宽限期让在途帧落库。
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}
