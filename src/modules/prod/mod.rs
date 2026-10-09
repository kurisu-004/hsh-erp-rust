//! prod 域（生产调度：工人 / 工种 / 工序 / 工艺链 / 生产队列 / 待下发批次）
//!
//! 2026-09-19 prod 模块聚合：把 worker + work_type + process + process_chain +
//! queue 五个支撑域平移至 `prod` 下，URL 一并迁移到 `/api/v2/prod/*`。
//!
//! 路由风格保持 com 容器模式：各子域各自定义独立 `router()`，prod 模块做 nest。
//!
//! 2026-09-29 新增 `prod::batch` 子模块（车间 PENDING 批次列表 + 一次 / 多次 / 自动
//! 下发 4 端点，URL 挂 `/api/v2/prod/batches/*`），复用既有 `t_shelf_process` 解析货架，
//! 零 schema 变更。
//!
//! 2026-09-30 prod 域 9 端点重构（worker-pool + batches 合并）：
//! - worker-pool → pool 路径收敛（2026-09-30）：原 `/worker-pool` +
//!   `/admin/worker-pool` 双 nest 合并为单一 `/pool` nest。**该路径已于
//!   2026-10-08 再硬切为 `/queue`**，见下。
//! - batches 端点合并：原 `/batches/dispatch`（单条）+ `/batches/bulk-dispatch`（批量）
//!   合并为单一 bulk-only `/batches/dispatch`，原 `/bulk-dispatch` 路径 404。
//!
//! 2026-10-01 新增 `prod::programming` 子模块（待编程一览，1 端点，URL 挂
//! `/api/v2/prod/programming/pending`）：它是「待编程一览」页的**唯一**数据源
//! （part 域同名端点已于 2026-10-07 下线）。谓词按 part 状态闸门
//! （`p.status IN ('PENDING','IN_PROCESS','PROGRAMMING')`，约束全部规则）+ 三规则
//! 并集（part 级去重）：① `p.status = 'PROGRAMMING'`（历史 PROGRAMMING 状态仍
//! 允许消化）② 工单工艺链含 `is_cnc` 工序 ③ 批次 `current_process_id` 指向
//! `is_cnc` 工序（migration 004 确立的唯一权威列）。
//!
//! 2026-10-08 `prod::worker_pool` 更名 `prod::queue`（URL `/pool` → `/queue`，
//! **无 alias**）：域职责从「工人候选池」扩到「工序队列」（候选池 + 工人持有 +
//! 发放 / 召回 / 移动 / 自动分配），`pool` 这个名字只覆盖了第一块。同 commit 从
//! `prod::batch` 吸收 4 个端点（`pending` / `dispatch` / `auto-dispatch` 三个下发流
//! + `recall` 召回），因为它们的消费方是队列页而非批次详情页。
//!
//! 2026-10-02 新增 `prod::shelf_process` 子模块（货架 ↔ 工序映射 `t_shelf_process`，
//! 3 端点，URL 挂 `/api/v2/prod/shelf-processes/*`）：自当时的 shelf 域整体搬入。
//! 域规约依据「货架自身包括账号部分和工序映射部分，应拆分到 iam 域和 prod 域」——
//! 账号部分**消除**（`ShelfOut.account_count` 字段与 `count_accounts_by_shelf` 一并
//! 删除，绑定真源本来就在 iam），工序映射**搬进 prod**（`t_shelf_process` 关联的是
//! prod 域实体 `t_process`）。旧路径 `GET|POST /api/v2/shelves/{id}/processes` 与
//! `GET /api/v2/shelves/processes` 404（**无 alias**，沿 2026-09-19 prod 聚合先例），
//! 请求 / 响应契约逐字不变。跨域依赖方向是 prod → `iam::shelf`（只读
//! `ShelfRepo::get_by_id`）—— 货架实体本身已于 2026-10-10 迁入 iam 域。
//!
//! `/prod/batches/{batch_id}/recall-to-pending` 一律 404。
//!
//! 2026-10-05 新增 `prod::process_design` 子模块（制定工序页零件列表，1 端点，URL 挂
//! `/api/v2/prod/process-design/parts`）：前端「制定工序」页从 part 域
//! `GET /api/v2/parts?status=PENDING` 切过来（前端**不传 `limit`**）。part 域 `GET /parts` 在
//! service 层硬置 `part_only: true`，repo 据此在 SQL 里加 `AND assembly_id IS NULL`
//! 守卫，把**装配件的子零件全部排除**；本页需要所有还没定工序的零件，故新端点
//! **刻意不加**该守卫（子件行 `assembly_id` 有值，照常返回供前端标注归属）。
//! 谓词（软删 + `PENDING` 状态闸门）与字段集（7 字段最小集）均与旧端点不同，沿
//! 2026-10-01 `prod::programming` 的先例下沉到 prod 域。part 域旧端点**保留兼容、
//! 一行未改**。
//!
//! 2026-10-05 新增 `prod::inspection` 子模块（扫码查询，1 端点，URL 挂
//! `/api/v2/prod/inspection/scan/{serial_no}`）：返回「装配件（可空）→ 全部子件
//! → 全部批次」三层树，供前端扫码弹窗一次取全。命中口径是**先查 `t_part.serial_no`、
//! 未命中再查 `t_assembly.serial_no`**（子件码与父件码值域不同形，两表各自对活跃
//! 行有唯一索引），都未命中返 20101 / HTTP 404。扫子件与扫父装配件返回的是
//! **同一棵树**（`hit_kind` 区分）。角色 Manager + Inspector，与三个 `to-XXX`
//! 写端点同组。零 schema 变更。
//! part 域 `GET /parts/by-serial/{serial_no}` **保留兼容、一行未改**。
//!
//! 2026-10-07 待品检队列读（`GET /api/v2/prod/inspection/queue`，出参 13 字段
//! 分页列表）自 `prod::batch` 迁入本域 —— **破坏性路由变更**，旧路径
//! `GET /api/v2/prod/batches/inspection` 已下线且**无 alias**，请求 / 响应契约逐字
//! 不变。理由：前端「待品检」页只有这两个数据源，迁后同域收敛成一页一域，且本域
//! **零跨域依赖**（已被域隔离护栏覆盖，见 `inspection/mod.rs` 的 `mod tests`）。
//!
//! `t_part_batch`（批次）是生产执行单元，**归 prod 域**：它的 repo / model /
//! `shared::batch::status` 状态写入口与批次路由（`worker-scan` / `pick-up` / `to-*` /
//! `complete` / `split` / `cancel` / `scan-inspect` / 2 条集合读）整体在本域
//! `prod::batch`，URL 挂 `/api/v2/prod/batches/*`。下发流（`pending` / `dispatch` /
//! `auto-dispatch`）与召回已剥离到 `prod::queue`（2026-10-08）。
//!
//! part 域只留「多批次动作 + 非批次动作」：`POST /parts/{part_id}/cancel`（翻转该
//! part 全部活跃批次）、`POST /parts/{part_id}/force-complete`（全部非 CANCELLED
//! 批次）、`POST /parts/{part_id}/soft-delete`、`GET /parts/{part_id}/batches`，
//! 以及全部 CRUD / 文件 / Excel 工具 / 各类 list 端点。
//!
//! assembly **域本体**仍是核心实体、其 10 条 CRUD / 状态机端点不进 prod（决策不变，
//! URL 仍在 `/api/v2/assemblies/*`，由 `modules::v2_router` 的顶层 nest 承载）。
//! **唯一的例外**是 2026-10-11 新增的强制完成逃生端点
//! `POST /api/v2/prod/assemblies/{assembly_id}/force-complete`：它的操作对象是
//! 「子件 + 批次的生产执行状态」，与批次 / 队列动作同属生产链路语义，故挂
//! `/api/v2/prod/assemblies/*`（见本文件 `router()` 末尾的 nest）。这只是端点级
//! 的归属裁决，不构成「assembly 进 prod」的域归属变更。
//!
//! URL 硬切换（无 alias）：前端配套 PR 锁步迁移。端点全貌以本文件末尾的
//! `router()` 聚合为准。

use std::sync::Arc;

use axum::Router;

use crate::modules::assembly;
use crate::state::AppState;

pub mod batch;
pub mod inspection;
pub mod process;
pub mod process_chain;
pub mod process_design;
pub mod programming;
pub mod queue;
pub mod scan;
pub mod shelf_process;
pub mod work_type;
pub mod worker;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/workers", worker::router())
        .nest("/work-types", work_type::router())
        .nest("/processes", process::router())
        .nest("/process-chains", process_chain::router())
        // 2026-10-08：/pool → /queue 硬切（无 alias），并从 prod::batch 吸收
        // 下发流 + 召回 4 个端点（原挂在 /batches/pending 等路径）。
        .nest("/queue", queue::router())
        // 2026-09-29 新增：prod::batch（PENDING 批次 + 下发）
        .nest("/batches", batch::router())
        // 2026-10-01 新增：prod::programming（待编程一览，part 状态闸门 + 三规则并集口径）
        .nest("/programming", programming::router())
        // 2026-10-05 新增：prod::process_design（制定工序页零件列表，含装配件子件）
        .nest("/process-design", process_design::router())
        // 2026-10-05 新增：prod::inspection（扫码查询：装配件 → 子件 → 批次 三层树）
        .nest("/inspection", inspection::router())
        // 2026-10-10 新增：prod::scan（报工台：扫工牌 + 取件 / 放回列表 +
        // worker-scan + pick-up 共 5 端点，自 prod::worker / part / prod::batch
        // 三域硬切迁入，**无 alias**）
        .nest("/scan", scan::router())
        // 2026-10-02 新增：prod::shelf_process（货架 ↔ 工序映射，3 端点，自 shelf 域硬切）
        .nest("/shelf-processes", shelf_process::router())
        // 2026-10-11 新增：装配件级强制完成逃生端点（1 端点，自 assembly 域带过来）。
        // 见 [`assembly::force_complete_router`] 的 doc —— 装配件域**本体**仍是核心
        // 实体、不进 prod（决策不变），只有这条生产链路收口的端点归 `/prod` 前缀。
        .nest("/assemblies", assembly::force_complete_router())
}

/// 全模块共用的批次拆分工厂（挂载点 `/api/v2/batches`，见 `modules::v2_router`）。
///
/// 2026-10-09 新增。拆批有三个前端消费方（生产队列看板 / 外协看板 / 零件详情页），
/// 按「目标域按前端消费方判定」的规约它属多域共用，因此**不进** `/prod` 前缀而是
/// 单开一条顶层 nest。⚠️ 这使 `prod::batch` 有**两处挂载**：本 `router()` 里的
/// `/prod/batches/*`（域内 19 条）与这里的 `/batches/*`（本条）。
pub fn split_router() -> Router<Arc<AppState>> {
    batch::handler::split_router()
}
