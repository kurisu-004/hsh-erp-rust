//! prod::inspection 子模块 —— 扫码查询（装配件 → 子件 → 批次 三层树）
//!
//! 2026-10-05 新增：单只读端点
//! `GET /api/v2/prod/inspection/scan/{serial_no}`。前端扫码弹窗的数据源。
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
//! `Manager` + `Inspector`（service 内 `require_any_role`），与
//! `GET /api/v2/prod/batches/inspection` 及三个 `to-XXX` 写端点同一组。
//!
//! ## 模块结构（与 `prod::process_design` / `prod::programming` 平行）
//! - `model.rs` —— 行结构（`ScanPartRow` / `ScanAssemblyRow` / `ScanBatchRow`，
//!   `query_as!` 宏的编译期校验对象）
//! - `vo.rs` —— 出参（`ScanTreeOut` / `ScanAssemblyOut` / `ScanPartOut` /
//!   `ScanBatchOut`）
//! - `repo.rs` —— SQL 真源（`InspectionScanRepo` ZST + 5 个静态方法，2~4 条
//!   SQL 走完一次请求，无 N+1）
//! - `service.rs` —— 业务逻辑（角色守卫 + 两表回退命中 + 内存分组挂树）
//! - `handler.rs` —— HTTP 路由（只做参数提取 + `pool.acquire()` + `R::ok`）
//!
//! **刻意没有 `dto.rs`**：本端点无 query / body 入参（`serial_no` 走 path），
//! 没有任何可反序列化的入参结构，造一个空 DTO 模块只是噪音。
//!
//! ## 事务 / WS 广播 / schema
//! 纯读端点：handler `pool.acquire()` 不开事务，**不发** WS 广播（无业务流转）。
//! 零 schema 变更（无新 migration）。
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

pub mod handler;
pub mod model;
pub mod repo;
pub mod service;
pub mod vo;

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/scan/{serial_no}", get(handler::scan))
}
