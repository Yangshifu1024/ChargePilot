//! 可导出资源注册表：
//! 只有登记过的资源允许导出；账单包含全局财务数据，只允许全局范围。

use chargepilot_domain::operations::DataScope;

/// 导出列：中文表头 + 数据库列名（顺序即文件列顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportColumn {
    /// 文件里的中文表头。
    pub header: &'static str,
    /// 数据库列名，也是脱敏规则匹配的字段名。
    pub column: &'static str,
}

/// 站点维度的过滤方式：来源表有的直接拼列，
/// charge_order 不带站点列，经设备表子查询过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StationFilter {
    /// 该资源不受站点范围约束。
    None,
    /// 直接拼来源表的站点列（stations 用 id，devices 用 station_id）。
    Column(&'static str),
    /// 经 device_meta 子查询（orders 的 device_id 关联站点）。
    ViaDevice,
}

/// 资源定义：权限、来源表、列清单与数据范围过滤。
#[derive(Debug, Clone, Copy)]
pub struct ExportSpec {
    /// 导出该资源所需的读权限。
    pub permission: &'static str,
    /// 数据来源表名（代码登记，不接受 HTTP 表名）。
    pub table: &'static str,
    /// 导出列清单。
    pub columns: &'static [ExportColumn],
    /// 站点维度过滤方式。
    pub station_filter: StationFilter,
    /// 厂商维度过滤（仅 devices，直接拼 vendor_id）。
    pub scope_vendors: bool,
}

const ORDER_COLUMNS: &[ExportColumn] = &[
    ExportColumn {
        header: "订单号",
        column: "order_no",
    },
    ExportColumn {
        header: "用户",
        column: "user_id",
    },
    ExportColumn {
        header: "设备",
        column: "device_id",
    },
    ExportColumn {
        header: "端口",
        column: "port_no",
    },
    ExportColumn {
        header: "状态",
        column: "status",
    },
    ExportColumn {
        header: "开始时间",
        column: "started_at",
    },
    ExportColumn {
        header: "结束时间",
        column: "ended_at",
    },
    ExportColumn {
        header: "电费(分)",
        column: "electric_cents",
    },
    ExportColumn {
        header: "服务费(分)",
        column: "service_cents",
    },
    ExportColumn {
        header: "合计(分)",
        column: "total_cents",
    },
];

const STATION_COLUMNS: &[ExportColumn] = &[
    // 站点 ID 仅用于范围筛选，文件只包含名称、地址和状态。
    ExportColumn {
        header: "名称",
        column: "name",
    },
    ExportColumn {
        header: "地址",
        column: "address",
    },
    ExportColumn {
        header: "状态",
        column: "status",
    },
];

const DEVICE_COLUMNS: &[ExportColumn] = &[
    ExportColumn {
        header: "设备号",
        column: "device_id",
    },
    ExportColumn {
        header: "型号",
        column: "model",
    },
    ExportColumn {
        header: "站点",
        column: "station_id",
    },
    ExportColumn {
        header: "厂商",
        column: "vendor_id",
    },
    ExportColumn {
        header: "状态",
        column: "status",
    },
];

const BILL_COLUMNS: &[ExportColumn] = &[
    ExportColumn {
        header: "账单号",
        column: "bill_no",
    },
    ExportColumn {
        header: "订单ID",
        column: "charge_order_id",
    },
    ExportColumn {
        header: "用户ID",
        column: "user_id",
    },
    ExportColumn {
        header: "设备号",
        column: "device_id",
    },
    ExportColumn {
        header: "电费(分)",
        column: "electric_cents",
    },
    ExportColumn {
        header: "服务费(分)",
        column: "service_cents",
    },
    ExportColumn {
        header: "合计(分)",
        column: "total_cents",
    },
    ExportColumn {
        header: "退款(分)",
        column: "refund_cents",
    },
    ExportColumn {
        header: "状态",
        column: "status",
    },
    ExportColumn {
        header: "开具时间",
        column: "issued_at",
    },
];

/// 按资源名取登记信息；false 表示该资源不可导出。
#[must_use]
pub fn export_resource(name: &str) -> Option<ExportSpec> {
    match name {
        "orders" => Some(ExportSpec {
            permission: "order.read",
            table: "charge_order",
            columns: ORDER_COLUMNS,
            station_filter: StationFilter::ViaDevice,
            scope_vendors: false,
        }),
        "stations" => Some(ExportSpec {
            permission: "station.read",
            table: "station",
            columns: STATION_COLUMNS,
            station_filter: StationFilter::Column("id"),
            scope_vendors: false,
        }),
        "devices" => Some(ExportSpec {
            permission: "device.read",
            table: "device_meta",
            columns: DEVICE_COLUMNS,
            // 设备导出与设备列表采用站点、厂商两维交集；仅厂商范围不得退化为全库。
            station_filter: StationFilter::Column("station_id"),
            scope_vendors: true,
        }),
        "bills" => Some(ExportSpec {
            permission: "finance.read",
            table: "charge_bill",
            columns: BILL_COLUMNS,
            station_filter: StationFilter::None,
            scope_vendors: false,
        }),
        _ => None,
    }
}

/// 全部已登记资源名。
#[must_use]
pub fn export_resource_names() -> &'static [&'static str] {
    &["orders", "stations", "devices", "bills"]
}

/// 范围过滤的占位符清单与绑定（调用方负责 IN 列表逐个 bind）。
fn in_placeholders(values: &[u64], binds: &mut Vec<i64>) -> String {
    binds.extend(values.iter().map(|v| *v as i64));
    values.iter().map(|_| "?").collect::<Vec<_>>().join(",")
}

/// 资源范围过滤 SQL。
/// 追加到 WHERE 的片段与绑定值；账单无范围过滤。
/// 修正：charge_order 不带站点列，
/// 报未知列，这里改为经 device_meta 子查询过滤。
#[must_use]
pub fn scope_filter(spec: &ExportSpec, scope: &DataScope) -> (String, Vec<i64>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<i64> = Vec::new();
    // 受限范围的站点集合；空集合表示该维度无限制。
    let stations_restricted = !scope.unrestricted && !scope.station_ids.is_empty();
    if stations_restricted {
        let placeholders = in_placeholders(&scope.station_ids, &mut binds);
        match spec.station_filter {
            StationFilter::None => {}
            StationFilter::Column(column) => {
                clauses.push(format!("`{column}` IN ({placeholders})"));
            }
            StationFilter::ViaDevice => {
                clauses.push(format!(
                    "`device_id` IN (SELECT `device_id` FROM `device_meta` \
                     WHERE `station_id` IN ({placeholders}))"
                ));
            }
        }
    }
    // 设备导出的厂商维度（与站点交集）。
    let vendors_restricted = !scope.unrestricted && !scope.vendor_ids.is_empty();
    if spec.scope_vendors && vendors_restricted {
        let placeholders = in_placeholders(&scope.vendor_ids, &mut binds);
        clauses.push(format!("`vendor_id` IN ({placeholders})"));
    }
    (clauses.join(" AND "), binds)
}

/// 记录实际执行的范围：非设备资源不携带厂商维度；
/// 内存范围不携带单条范围类型。
#[must_use]
pub fn effective_scope(resource: &str, scope: &DataScope) -> DataScope {
    let mut scope = scope.clone();
    if resource != "devices" {
        scope.vendor_ids = vec![];
    }
    scope.unrestricted =
        scope.unrestricted || (scope.station_ids.is_empty() && scope.vendor_ids.is_empty());
    scope.scope_type = String::new();
    scope.scope_id = 0;
    scope
}

/// 当前范围是否覆盖文件保存时的全部范围；扩大权限可下载，收窄须拒绝
///。
#[must_use]
pub fn contains_scope(current: &DataScope, saved: &DataScope) -> bool {
    if current.unrestricted {
        return true;
    }
    if saved.unrestricted {
        return false;
    }
    let covered = |now: &[u64], before: &[u64]| -> bool {
        if now.is_empty() {
            return true;
        }
        if before.is_empty() {
            return false;
        }
        before.iter().all(|id| now.contains(id))
    };
    covered(&current.station_ids, &saved.station_ids)
        && covered(&current.vendor_ids, &saved.vendor_ids)
}
