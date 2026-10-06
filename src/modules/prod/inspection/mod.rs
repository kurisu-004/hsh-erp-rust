//! prod::inspection 子模块 —— 待品检域（队列列表读 + 扫码查询）
//!
//! 前端「待品检」页的**两个**数据源在本域收敛：
//! - `GET /api/v2/prod/inspection/queue` —— 队列列表（表头筛选 + 服务端排序 + 分页）
//! - `GET /api/v2/prod/inspection/scan/{serial_no}` —— 扫码弹窗的三层树
//!
//! 两个端点读的是同一批数据（`status='INSPECTION'` 的活跃批次）、同一组角色，
//! 故同域；队列读自 `prod::batch` 迁入后，本域**零跨域依赖**（护栏见
//! [`crate::shared::domain_guard`] 与本文件末尾的 `mod tests`）。
//!
//! 2026-10-05 新增：扫码端点 `GET /api/v2/prod/inspection/scan/{serial_no}`。
//!
//! 2026-10-07 队列读迁入：⚠️ **破坏性路由变更** —— 队列读的原路径
//! `GET /api/v2/prod/batches/inspection` **已下线且无 alias**（404），新路径是
//! `GET /api/v2/prod/inspection/queue`。**出参 JSON 逐字不变**（字段名、
//! `items[*]` 恰好 13 个 key、计数 `total` / `limit` / `offset` 是 JSON **string**
//! 而非 number），前端 Zod schema 无需改动；**入参 query string 亦逐字不变**
//! （`drawing_no` / `name` / `serial_no` / `customer_id` / `system_delivery_date_from|to`
//! / `sort_by` / `sort_dir` / `limit` / `offset`）。前端配套改动在**前端仓**
//! （`~/Code/hsh-erp/frontend`）由独立任务负责，共 3 类落点：
//! - 1 处 URL 字面量：队列读的 api 封装 `listInspectionBatches` 与其行 / 入参类型
//!   并入既有的 `src/api/inspection.ts`（该模块此前已承载扫码端点），URL 字面量
//!   `/prod/batches/inspection` → `/prod/inspection/queue`；
//! - 1 处单测路径断言：`src/api/parts/__tests__/routes.spec.ts` 里那条
//!   `expect(...).toBe('/prod/batches/inspection')` 迁到新建的
//!   `src/api/__tests__/inspection.contract.spec.ts`（该 spec 里扫码端点 URL 是
//!   独立的 Q4 断言）；
//! - 6 个文件的注释引用旧路径（`src/composables/queries/{keys,schemas}.ts` /
//!   `src/types/inspection.ts` / `src/views/inspection` 下的
//!   `inspectionColumnDefs.ts` 与 `composables/{inspectionSchema,useInspectionQueueQuery}.ts`），
//!   纯文案、不影响行为。
//!
//! 本次后端提交不含前端改动。
//!
//! ## 扫码端点（`GET /scan/{serial_no}`）
//!
//! ## 为什么在 prod 域另起端点
//! part 域既有 `GET /api/v2/parts/by-serial/{serial_no}`（28 列 `PartDetailOut`）
//! 与 `GET /api/v2/parts/by-serial/{serial_no}/part-batches`（单 part 上下文），
//! 两者都无法表达「装配件 + 全部子件 + 全部批次」这棵树：前者不展开子件，后者
//! 不返回装配件节点。本端点的谓词（先 part 后 assembly 的两表回退）、字段集
//! （9~11 列窄投影）与形状（两层 `children`）都与既有端点不同，另起一个比给
//! part 域塞 `with_children` / `expand_assembly` 之类的开关更干净。
//!
//! **part 域一行未改**：旧端点保留兼容，新增调用方走本域。
//!
//! ## 命中口径（先 `t_part` 再 `t_assembly`）
//! 1. `t_part.serial_no` 命中 → `hit_kind = "PART"`
//! 2. 未命中再查 `t_assembly.serial_no` → 命中 → `hit_kind = "ASSEMBLY"`
//! 3. 都未命中 → `20101 BIZ_PART_NOT_FOUND`（HTTP 404）
//!
//! 扫到**子件**时：`assembly` 有值，`children` 是该装配件的**全部**子件；
//! 扫到**装配件**时返回的是**同一棵树**（`hit_kind` 不同，`children` 完全一致）。
//!
//! ## 两条必须记住的口径
//!
//! ### 1. `process_name` 对 `INSPECTION` / `DELIVERED` 批次恒为 `null`
//! 这**不是 bug**，是「出池必须把 `current_process_id` 置 NULL」这条不变式的
//! **正确**结果：所有进 `INSPECTION` 的写点（`BatchService::scan_inspect` /
//! `BatchService::receive_from_outsource_to_inspection` /
//! `BatchService::complete_repair` / `mark_batch_inspected`）都按出池清该列；
//! `DELIVERED` 更进一步 —— 进 `READY_TO_SHIP` 的边只有
//! `INSPECTION → READY_TO_SHIP`，故也必经 `INSPECTION`、同样恒 NULL。
//! 前端在这两个状态下**不要**渲染工序标签。
//!
//! 取值列是 `current_process_id`（migration 004 确立的工序归属权威列），**不是**
//! `current_process_step_id`。取舍：后者只在首次定位工序时写、之后永不推进，
//! 多工序链工单上会停在第一步，用它渲染「当前工序」会显示过时信息；代价就是上条
//! 那两个状态恒为 `null`（可接受 —— 这两个状态本就不在生产流中）。其余展示类
//! 列表（`GET /parts/{id}/batches`、待品检队列、返修列表）仍走 step 派生。
//! ⚠️ 本端点是「展示类列表一律走 step 派生」这条分工的**唯一有意例外**，已登记在
//! `prod::batch::model.rs` 模块 doc 的读取方分工清单第 4 条（改动那份清单前请先
//! 对照本节）。
//!
//! ### 2. 本端点**读全部批次，不按状态过滤**
//! 含 `COMPLETED` / `CANCELLED` 等终态批次，与 `GET /api/v2/parts/{id}/batches`
//! 同口径（该端点同样是「无 status 过滤」的展示类列表）。理由：扫码弹窗要回答
//! 「这批货总共分了几批、每批现在什么状态」，砍掉终态就答不了；而**状态闸门在
//! 前端** —— 按 `ScanBatchOut::status` 决定「送检 / 指定工序」等按钮的显隐，
//! 后端不去重、不改写、不代做决策。
//!
//! ## `is_scanned` 的由来（本端点唯一一处内存派生字段）
//! `t_part_batch` **没有序列号列**，批次与扫码串之间没有可 join 的关系，命中关系
//! 只能由 service 在内存里比对 `batch.part_id == 命中 part.id` 得出。
//! 结果：装配件树里**只有**被扫中那个子件的批次为 `true`；扫装配件条码时
//! `hit_kind = "ASSEMBLY"` 且无命中零件，故全部批次 `is_scanned = false`
//! （前端此时可把整树当「已定位到装配件」整体高亮）。
//!
//! ## 版本号分工（前端最容易踩的一处）
//! | 字段 | 来源列 | 用途 |
//! |---|---|---|
//! | `ScanPartOut::version` | `t_part.version` | **仅展示** |
//! | `ScanBatchOut::version` | `t_part_batch.version` | `to-ship` / `to-process` / `to-inspection` 的 **OCC 锚** |
//!
//! 批次 id + 批次 version 是一对，回传时**不许**拿零件 version 顶替。
//!
//! ## 软删闸门
//! part / assembly / batch 三处的软删行一律不返回（另含 `t_customer` 软删时客户名
//! 退化为 `null`，不影响节点返回）；`LEFT JOIN` 进来的 `t_process` / `t_shelf` /
//! `t_worker` / `t_outsource_company` 四张表**不加**闸门（与 `prod::batch::repo`
//! 既有写法一致，展示用附加信息照常显示最后的样子）。
//!
//! ## 角色
//! `Manager` + `Inspector`（service 内 `require_any_role`），两个读端点共用
//! `service.rs` 的 `READ_ROLES`，与 `prod::batch` 三个 `to-XXX` 写端点同一组。
//!
//! ## 队列端点（`GET /queue`）
//! 2026-10-07 自 `prod::batch` 迁入。口径如下（迁前逐字保留，判定依据见 `repo.rs`）：
//!
//! - **判据写死** `pb.status = 'INSPECTION'` + `pb` / `p` 双软删闸门；本端点
//!   **不接** `statuses` 参数（要按其它状态筛请走 `/repair` / `/repairing`）。
//! - **3-JOIN 窄投影**：`t_part` + `t_customer`（L2）+ `t_customer` 自连（L1）。
//!   不 JOIN holder 三表 / 工序 / 送货单 —— 待品检页不渲染那些列。
//! - **13 字段**：表头 7 个数据列 + 3 个写端点锚点（`batch_id` / `version` /
//!   `part_id`）+ `customer_id` / `is_urgent` + 两个客户名。
//! - **表头筛选**：图号 / 名称 / 序列号各一个独立 ILIKE（`%…%`）。service 层
//!   trim + 空串→None，并**拒绝** `%` / `_` / `\`（40001 —— 防 `%…%` 被 PG 当通配符
//!   放大成全表扫描；注入面由 repo 的 `push_bind` 参数化保证，与该校验无关）。
//! - **`customer_id`**：单值 → `crate::shared::customer::expand_customer_id`
//!   展开为 L1 + 全部 L2 ids 进 `= ANY($n)`。该函数在 `shared`（公共设施，不是域），
//!   故本域引用它**不**违反零跨域依赖。
//! - **分页**：`limit ∈ [1, 200]`（默认 200）、`offset ≥ 0`（默认 0）；`total`
//!   与 `items` 共用同一个 WHERE 拼装器，恒等于**过滤后**的条数。
//! - **排序**：白名单映射在 service 层完成（表头 7 列；缺省 / 非法 `sort_by` 退化为
//!   系统交期，方向只认 `DESC`、其余退化为 `ASC`），非法值**不报错**。SQL 侧
//!   `ORDER BY {col} {dir} NULLS LAST, pb.id ASC`（`pb.id` 是翻页稳定性的兜底键）。
//!
//! ⚠️ `l1_customer_name` 的派生口径**与返修列表不同**（`c.parent_id IS NOT NULL`
//! → `pc.name.or(c.name)`，否则 `c.name`；返修那条不回落 `c.name`）—— 两条 SQL 的
//! 分叉是有意的，但**只有本域侧写了登记**（见 `repo.rs`），返修侧当前没有对应注释。
//!
//! ## 模块结构（平级单文件，与 `prod::process_design` / `prod::programming` 平行）
//! 本域**两个端点共一层文件**（不是「一端点一目录」）：端点之间的耦合只有「共用
//! 角色白名单 / 共用批次表」这一层，按层切文件比按端点切目录更贴近全仓形态，且
//! 两个 repo / 两个 service 的**类型名**自带 `InspectionScan` / `InspectionQueue`
//! 前缀区分（方法名则是 `scan` 与 `list_queue`），不会混。
//! - `dto.rs` —— 入参（仅队列读的 `InspectionQueueQuery`，Query string；
//!   **扫码端点无入参**，`serial_no` 走 path）
//! - `model.rs` —— 行结构：`ScanPartRow` / `ScanAssemblyRow` / `ScanBatchRow`
//!   （`query_as!` 宏的编译期校验对象）+ `InspectionQueueRow`（`QueryBuilder`
//!   动态 SQL，故手写 `FromRow`）
//! - `vo.rs` —— 出参：`ScanTreeOut` / `ScanAssemblyOut` / `ScanPartOut` /
//!   `ScanBatchOut` + `InspectionQueueItemOut` / `InspectionQueueListOut`
//! - `repo.rs` —— SQL 真源，**两个 ZST**：`InspectionScanRepo`（5 个静态方法，
//!   2~4 条 SQL 走完一次扫码请求，无 N+1）+ `InspectionQueueRepo`（list / count，
//!   共用私有 WHERE 拼装器）
//! - `service.rs` —— 业务逻辑：`InspectionScanService`（角色守卫 + 两表回退命中 +
//!   内存分组挂树）+ `InspectionQueueService`（角色守卫 + limit/offset clamp +
//!   **排序白名单映射** + ILIKE 通配符拒绝 + row→vo 投影）
//! - `handler.rs` —— HTTP 路由 2 条（只做参数提取 + `pool.acquire()` + `R::ok`）
//!
//! ## 事务 / WS 广播 / schema
//! 两个端点都是纯读：handler `pool.acquire()` 不开事务，**不发** WS 广播（无业务
//! 流转）。零 schema 变更（无新 migration）。
//!
//! ⚠️ **已知风险登记（不修，跟随全仓读端点惯例）**：`pool.acquire()` 拿到的连接
//! 在 READ COMMITTED 下每条语句各看一个快照，装配件分支的 2~4 条语句**不保证同一
//! 快照**。理论上的可撕裂场景：4 条语句之间被扫中的那个子件被软删 → `children`
//! 里没有自己刚扫的码，且全树 `is_scanned = false`、**无任何错误提示**。发生概率
//! 极低（要求软删与扫码在毫秒级重叠），且全仓读端点都是这个形态（读端点不开事务
//! 是 CLAUDE.md 的约定），故本次不改；真要消除只能给读端点开 REPEATABLE READ 快照
//! 事务，属跨域惯例改动。

use std::sync::Arc;

use axum::{Router, routing::get};

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod vo;

pub fn router() -> Router<Arc<AppState>> {
    // ⚠️ 两条路由段数不同（`/queue` 1 段静态、`/scan/{serial_no}` 2 段），
    // matchit 无同段位争用，注册顺序无关。
    Router::new()
        .route("/queue", get(handler::queue))
        .route("/scan/{serial_no}", get(handler::scan))
}

#[cfg(test)]
mod tests {
    //! 域隔离护栏：把「待品检域不依赖其它域」从口头约定变成 CI 强制。
    //!
    //! 探测器实现（剥注释、根段 + 域路径前缀匹配、元测试）见
    //! [`crate::shared::domain_guard`]，本域只负责传参 + 域专属指引。

    use std::path::Path;

    use crate::shared::domain_guard::assert_no_foreign_domain;

    /// 待品检域只允许 `crate::` 下的 `auth` / `infra` / `shared` / `state`
    /// 与本域自身；代码区里出现任何其它域的路径即失败（`prod` 下的兄弟域同样是
    /// 别的域，如 `prod::batch`）。
    #[test]
    fn inspection_domain_depends_on_no_other_domain() {
        assert_no_foreign_domain(
            "prod::inspection",
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modules/prod/inspection"),
            "需要别的域的数据时，正确做法是像 statistics / admin 那样在本域 SQL 里只读聚合\
             （本域要读的 t_part / t_part_batch / t_assembly / t_customer 等表，SQL 真源见 \
             repo.rs；客户 L1 展开用 crate::shared::customer::expand_customer_id，shared 是\
             公共设施不是域），而不是 import 别人的 service / repo。",
        );
    }
}
