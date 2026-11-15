# 03 · 设备接入链路(protocol → gateway-runtime → events)

> 覆盖:TCP 设备接入的契约层、DC589 协议实现、接入运行时、事件投递。
> 规则归属:协议两层结构与 36 条指令/帧格式/故障码语义是冻结红线(F5);容量预算 G1–G4 见 gateway-runtime/src/lib.rs 总纲。

## 1. 两层协议结构(F5,结构冻结)

```text
crates/protocol/            # 契约层(416 行,唯一会合点):trait 与中性类型
crates/protocol-dc589/      # 实现层(约 2900 行,唯一协议实现):帧编解码 + 适配器
```

规则:**协议码值、帧结构、校验算法一律封闭在实现 crate**;业务与运行时只见中性契约。新协议 = 新增实现 crate,不改根契约。

### protocol 契约层(src/lib.rs,单文件)

| trait | 职责 |
|---|---|
| `Sink` | `register(Registration)` / `record(Event)`——适配器把事件交还运行时;**持久化成功后才向设备确认帧** |
| `SessionRecorder` | `record_session(SessionRecord)`——连接结束后的会话审计(可选能力) |
| `DeviceSession` | `send(Command)` / `close()`——向单设备连接串行下发命令 |
| `SessionRouter` | `attach/get/detach`——设备到会话映射与同设备二次连接顶替(互踢) |
| `Adapter` | `name()` / `serve_conn(stream, sink, recorder, router)`——每种协议一个实现 |

核心类型:

- `EventType`(12 值):Heartbeat / StartResult / StopResult / ChargeEnd / Fault / RemoteResult / ConfigResult / Telemetry / TimeSync / CardSwipe / CardBalanceQuery / LocalStart;`as_str()` 与 `format!("{k:?}")` 逐字一致(兼容存量数据)。
- `CommandKind`(5 值):Start / Stop / Reboot / CardDenied / CardBalance。
- `Event`:已解码事件的完整字段面(含 `success` 中性成败判定——厂商应答码由适配器翻译,不进运行时)。
- `event_payload` 读视图载荷(3 种):`CommandResultPayload`(B8/BA)、`ChargeEndPayload`(BB)、`CardEventPayload`(B6 刷卡 / D0 余额查询 / C0 故障 / B4 本地启动)。

### protocol-dc589 实现层(src/)

| 文件 | 职责 |
|---|---|
| frame.rs | 二进制分帧:`EE \| LEN \| CMD \| SESSION(6B) \| DATA \| SUM(XOR)` |
| adapter.rs | DC589 适配器:注册握手、逐帧分派、会话互踢、连接审计;持久化成功才确认帧 |
| device.rs | 设备方向编解码(主板发送帧 A0/B4/B8/BA/BB/C2/C6/D0;解析平台下发帧 A1/B7/B9/A6/E0) |
| commands.rs | 平台侧 36 条指令编解码(注册/心跳设置/校时/启停/结束与故障确认/复位) |
| measurements.rs | 计量帧编解码(心跳 A4、结束订单 BB、充电分档 C2);Wh→mWh 统一 |
| cards.rs | 在线卡帧(刷卡 B6、拒绝 BD、余额查询 D0、应答 D1) |
| config.rs | 本地参数表(C3/C4/C5/C6)与功率控制(E0/E1) |
| fault.rs | C0 故障码 → 中性指标名与严重度(运行时只见 "smoke"/"high_temperature" 等) |
| consumer.rs | 消费类型码词表(consumer_type 字段,业务只引用具名常量) |

字段单位换算(Wh↔mWh、分↔0.1 元、温度偏移)在本 crate 边界完成。

## 2. gateway-runtime 运行时(src/)

```text
TCP :9100 → listener.rs(DeviceListener,accept 限速削峰)
  → registry.rs(SessionRegistry,同设备二次连接顶替,锁内原子替换)
  → protocol-dc589::adapter(逐帧解码为中性 Event)
  → sink.rs(GatewaySink,实现 Sink trait)
       ├─ 注册信息 → 设备上线处理
       ├─ 事件落库:device_event + domain_event 同事务(at-least-once 根基)
       ├─ 高频遥测 → telemetry.rs(TelemetryBuffer 有界缓冲 → 批量落库 → 15min/小时聚合)
       └─ 快照更新 → snapshot.rs(SnapshotStore,内存毫秒级,只承载显示与估算)

下发方向:charging 域 CommandSink trait ← command_sink.rs(GatewayCommandSink)
          经 SessionRegistry 找到 DeviceSession 串行下发;设备离线即失败,
          由事件层退避重试 / dispatcher 标 rejected / sweeper 兜底退款

事件消费:sink 落库的 domain_event(kind=device_event)
  → events runtime → device_events.rs(DeviceEventDispatcher)
      B8/BA/BB 回执 → charging 域原语(locate_pending_start / ack_stop_command / find_unreported_stop)
      B6 刷卡      → card::SwipeService 准入与授权/拒绝帧
      C0 故障      → device::alert::record_fault 告警生成
      B4 本地启动  → 平台侧补单(卡余额消费)
```

| 文件 | 职责 |
|---|---|
| lib.rs | 总纲:分层与会话路由→协议适配器→事件 sink→命令队列(有界 mpsc + 失败退避);业务码值零协议字面量 |
| listener.rs | accept 循环 + 受管理停机(G1 顺序:停 accept → 关会话 → 冲刷遥测) |
| registry.rs | 会话注册表:互踢原子替换 + ptr_eq 防 self-detach(162 行,自带测试) |
| sink.rs | GatewaySink:注册/事件落库/快照更新/遥测分发;写 device_session、device_event 表(蓝图豁免的运行时存储) |
| snapshot.rs | 快照存储:帧到达先更新内存,缺失回退查库;不承载资金事实 |
| telemetry.rs | 遥测管道:buffer → flush → 两级聚合;批量 200ms 或 500 条触发;缓冲满丢最旧明细(ADR 005);写 telemetry 表 |
| command_sink.rs | GatewayCommandSink:charging 域命令下发的真实实现 |
| device_events.rs | DeviceEventDispatcher:回执/刷卡/故障/本地启动的传输编排;**本文件零 SQL**(check-sql 门禁把守),业务查询已下沉 charging/device 域 repo |

## 3. events 事件层(跨域粘合剂)

```text
写入:业务事务内 chargepilot_events::enqueue::in_tx(conn, source, kind, event_id, payload)
     ——事件与业务写入同事务提交,at-least-once 的根基
投递:runtime.rs——1s 轮询到期事件 → 按 EventKind 路由到 Registry 注册的 handler
     → 成功标 published / 失败退避重排(1 << min(retry, 8) 秒)/ 死信 dead
兜底:各域 sweeper 推滞者(如启动超时扫描),见 09-troubleshooting.md
```

| 项 | 值 |
|---|---|
| EventSource | User / Admin / Gateway |
| EventKind(8 值) | charge_ended / charge_settled / charge_payment_confirmed / charge_payment_late / charge_start_failed / charge_start_timeout / refund_completed / device_event |
| EventDeliveryStatus | pending / published / failed / dead |
| 注册的 handler(app::serve) | charge_payment_confirmed → charging::StartDispatcher;device_event → DeviceEventDispatcher |
| 幂等要求 | EventDispatcher 实现必须幂等(at-least-once 语义,重投是常态不是异常) |

`event_id` 格式串由各调用点自拼(如 `S{command_id 前 32 位}`、`WR{...}` 退款号),是唯一的隐式约定。

## 4. 改设备链路时的注意点

1. **帧格式与指令表是红线**(F5):改协议行为先确认不属于冻结面;新增指令在 commands.rs/measurements.rs 落码,中性化后经 Event/Command 契约暴露。
2. **协议翻译归实现层**:应答码 → success 中性判定、故障码 → 中性指标名,都在 protocol-dc589 完成;运行时与业务不出现协议字面量。
3. **确认帧前先持久化**:adapter 的「持久化成功才 ACK」顺序不能反,否则崩溃窗口丢事件。
4. **遥测分级丢弃是有意设计**(ADR 005):缓冲满丢最旧明细,不为遥测牺牲业务事件。
5. 测试:黄金样本帧与 closed_loop 集成见 [08-testing-gates.md](08-testing-gates.md);容量加载用 `app` 的 simulator-batch 子命令。
