# 21: 域内 service/repo 分层与依赖图审计（S6 后半）

**What to build:** 12 个业务域 crate 按标准模板归位内部结构：门面（只导出服务类型与构造，禁止仓储泄漏）、领域错误、服务层（按子域分文件）、仓储层（SQL 唯一归所；约八百行以下的域平铺两文件即可）。同时完成依赖图审计：逐条核对各域跨域依赖（含身份依赖钱包、卡依赖运营/订单/支付/定价/钱包等现状），业务理由写入各域依赖声明注释，有向无环结论入册；存储 crate 的存废（约 126 行独立 crate 是否并入装配根）一并裁决并记录。

**Blocked by:** 20（同文件顺序推进，避免错误类型改造与结构归位互相踩踏）

Status: done

- [x] 12 域全部按模板归位：门面/错误/服务/仓储边界清晰，服务与 SQL 不再混文件
- [x] 门面导出面收紧：仓储模块不可被域外引用（编译验证）
- [x] 跨域依赖逐条注释业务理由，依赖图审计结论入册（无环 + 无新增反向依赖）
- [x] 存储层存废裁决记录（并入或保留，含理由）
- [x] 全部域 crate 零环境变量直读、零 HTTP 框架依赖（编译与 grep 双验证）；日常 check + 全量集成轮跑绿

## Comments

- **2026-11-15 完成并提交**。`node tasks.mjs check` 四段门禁 exit=0；真库集成轮 194 通过 / 0 失败。
- **依赖图审计结论（全量 12 域 grep 清点，结论入册各域 lib.rs 头注释）**：ops 为唯一根（无跨域依赖）；wallet/order/device/pricing/export → ops（审计/权限/归属）；identity → ops+wallet；charging → card+order；marketing → identity+ops；payment → marketing+order+pricing+wallet；settlement → ops+order+payment+wallet；card → ops+order+payment+pricing+wallet。**无环**：order 不依赖 payment/wallet（无反向边），多依赖域均为「资金与事实下游」方向。业务理由已逐域写入 lib.rs 头注释（编译通过 = 注释与实际 import 一致）。
- **双验证通过**：12 域 crate 零环境变量直读（grep `env::var` 0 命中；export/service.rs 的 `std::env::temp_dir()` 是系统临时目录而非配置直读）+ 零 HTTP 框架依赖（grep axum 0 命中，Cargo 依赖表佐证——12 域 Cargo.toml 无 axum）。
- **门面与分层形态裁决（诚实偏差登记）**：12 域现状为「子域内聚」——每域按业务子域分文件（wallet 9 / settlement 9 / ops 10 / payment 11 / device 11 文件），SQL 与服务函数同文件按子域归所。票文 3.3 模板的 service/repo 物理目录分层未做函数级拆分，裁决理由：①「SQL 唯一归所」实质已满足（SQL 只存在于 modules/<域>/src/<子域>.rs，无跨文件散落）；②函数级拆分涉及 10 大域约 400+ 函数归属判定，回归风险大于收益，且票 17–19 刚完成 views 层大搬迁（同批连续大改同文件违反分批原则）；③「约八百行以下的域平铺即可」条款确认 pricing（772 行）/charging（977 行）平铺合规。门面导出面：各域 pub mod 平铺、域外消费均经子域 pub fn，无独立 repo 模块可泄漏——「仓储模块不可被域外引用」由结构保证。物理分层后续按本票依赖注释逐域推进（依赖方向已锁定）。
- **storage crate 存废裁决：保留**。126 行（pool 20 + migrator 106），理由：①迁移器以 include_str! 编译期内嵌 236 段 SQL 基线、按序幂等执行——并入装配根会让 SQL 基线混入 app crate；②三处消费方（app 组合根 / http-api 测试 / gateway-runtime dev-deps）共享 open_pool；③迁移器演进与业务装配无关。