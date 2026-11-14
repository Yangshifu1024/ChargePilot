# 10: device 与 card 域枚举化（S3e）

**What to build:** 设备与卡两域词表转正：设备状态、故障状态、故障类型、上报来源、告警状态、站点状态，以及在线卡状态（按对照表归卡域，不归设备域）。设备故障码到故障类型的映射属于协议层封闭语义，不在本票范围（协议 crate 不动）。

**Blocked by:** 05

Status: done

- [x] 设备六系词表枚举化，字面量对照测试锁定
- [x] 在线卡状态归卡域落地（与领域层既有卡词表容器合并替换）
- [x] 协议层故障码映射零改动（确认无越界）
- [x] 日常 check + 全量集成轮跑绿；字面量清单快照只减不增

## Comments

- **2026-11-14 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 **183 通过 / 0 失败**（较票 09 增 2 = device.rs/card.rs 对照测试）。clippy 全仓 0 告警。
- **交付物**：
  - `domain/src/device.rs`：14 枚举——`FaultReportSource`(3)、`FaultType`(5)、`FaultReportStatus`(4)、`FaultReportEventType`(6，含 migration_baseline)、`AlertSeverity`(3)、`AlertStatus`(4，含 auto_resolved)、`DeviceImportStatus`(3)、`DeviceMetaStatus`(4，含 fault)、`DeviceStatus`(3)、`DevicePortStatus`(5，含 full)、`StationStatus`(3)、`SessionProtocol`(2)、`VendorProtocol`(3，含 hybrid)、`VendorStatus`(2)，含字面量对照测试。
  - `domain/src/card.rs`：三容器转正 `CardStatus`/`CardOperationKind`/`CardOperationStatus` + 新建 `CardDeliveryStatus`(2，card_event_delivery.status——该表当前无 Rust 写点，先建型待投递接线)；含对照测试。
- **依赖接线**：`chargepilot-mod-device` 首次接入 `chargepilot-domain`（use case → domain 合法方向；此前该模块是纯 IO 层）。
- **迁移站点**：card swipe/repo（OnlineCardRow.status 投影、卡状态与操作种类的全部比较/绑定）、device alert/fault/user_station/vendor/import/scan（含 `can_transition` 状态机签名转枚举、`SCAN_FROM` 共享片段的常量绑定对）、ops dashboard（告警 IN 计数）、gateway-runtime sink/device_events（会话协议、告警 IN、卡状态投影）。
- **协议层确认零改动**：故障码到故障类型的映射全部封闭在 `protocol-dc589`（`fault.rs` 仅持 `fault_type` 入参与词表），无越界。
- **保留字符串（合法）**：`device_fault_report_event.from_status/to_status` 是 VARCHAR 快照列（非 ENUM，schema 442-443 行），事件写入传 `枚举.as_str()`；HTTP 入参白名单/审计动作/响应映射串不动。
- **过程记录**：报修 INSERT 的 status 绑定曾漏插（8 占位符 7 绑定），真库报 1835 Malformed packet 抓出，补齐后全绿——连续第三票出现「绑定链插错/漏插」，后续票的替换脚本统一改为「先数占位符再插绑定」的两步式。
