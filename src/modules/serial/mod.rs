//! `serial` 域 —— 一键释放「终态工单仍占用的序列号」，**2026-10-19 新建**
//!
//! 需求（用户原话）：「当前还有残留已完成的的装配件但占用序列号的情况，创建一个 api
//! 用于一键释放已完成工单的序列号」。经与用户确认范围：**装配件 + 零件 / 子件两张表
//! 都清**（`t_assembly.serial_no` + `t_part.serial_no`）。
//!
//! ## 域范围与端点（1 条，挂在 `/api/v2/serials`，由 [`router`] 桥接）
//!
//! | # | Method | Path | 守卫 | 说明 |
//! |---|---|---|---|---|
//! | 1 | `POST` | `/release-terminal` | `require_role(Manager)` | 释放全部「已完成 / 已取消且仍持号」的工单序列号；`{"dry_run": true}` 预览 → 二次确认 → `false` 实跑；返**计数 + 明细** |
//!
//! ## 为什么**独立成域**（端点级归属裁决）
//!
//! 候选集横跨 `t_assembly`（装配件域本体）与 `t_part`（part 域本体）**两张表**，而
//! 序列号释放这件事**不属于任何一侧**：它既不是装配件 CRUD、也不是零件 CRUD，更不是
//! 生产批次流转（`prod::batch`）。塞进任一域都会让那个域的「表边界」被打破（对照
//! `docs/api/queue.md` 记的「候选池判据的两列一致 / 货架 JOIN 不一致」那类跨表读，
//! 本端点的跨表**写**性质更重）。
//!
//! 顶层单开 `/serials` nest 的**先例**：`src/modules/mod.rs::v2_router()` 里的
//! `.nest("/batches", prod::split_router())` —— `prod::batch` 域的跨域共用端点
//! （批次拆分被生产队列 / 外协看板 / 零件详情三处调用）就单开了顶层前缀，
//! `src/modules/prod/mod.rs` 的 `split_router` doc 解释了理由。本端点同理：
//! **它服务的是「两张表的一个共同状态」，不是某一域的资源**。
//!
//! ## URL 与硬切记录
//!
//! 本域**全新**，**无任何旧路径、无 alias**。前缀 `/serials`：顶层 nest、单段、
//! kebab-case，与 `/mail-digest` / `/print-templates` / `/outsource-queue` 同款。
//!
//! ## ⚠️ 本域**一行写 SQL 都不写**
//!
//! 候选 SELECT 之外，清列动作全部复用仓库既有的两个原语（装配件：
//! `AssemblyRepo::clear_serial_no_if_terminal`；零件：`shared::batch::status::release_part_serial_no`，
//! 它**先写 `t_part_event` 归档、再清列**）。理由与逐条分叉点见 `service.rs` 模块 doc ——
//! 与 `docs/api/batch.md` 里「写 `t_part_batch.status` 只能走 `apply_batch_status_change`」
//! 是同一条纪律。
//!
//! ## 域隔离护栏：**不适用**（2026-10-19 复核过）
//!
//! 本域对别域的**唯一**代码引用是 `service.rs` 里一行
//! `use crate::modules::assembly::repo::AssemblyRepo;`（装配件侧清列原语）——
//! 护栏（`shared::domain_guard::assert_no_foreign_domain`）若整域装上会当场红。
//!
//! 为什么不装：那一行**就是本域的立域前提**，与上一节「一行写 SQL 都不写」同源 ——
//! 清 `t_assembly.serial_no` 必须走 `AssemblyRepo::clear_serial_no_if_terminal`，
//! 自写 UPDATE 会立刻分叉出「归档丢失 / `version` 不递增 / 守卫口径漂移」三处。
//! 护栏的规则（「不 import 别域的 service / repo」）在这里恰好把**正确**的写法也拦了，
//! 与 `prod::scan`（转发型域）、`mail_digest`（内容域 → 通道域）同款豁免，两处都在
//! 各自 `mod.rs` 与 `docs/api/*.md` 复核登记。
//!
//! ⚠️ 零件侧那个原语 `shared::batch::status::release_part_serial_no` **不构成**跨域
//! 引用：`crate::shared` 是非域公共设施，该护栏按设计就不挡它（同 `crate::infra` /
//! `crate::auth` / `crate::state`）。
//!
//! ## 结构（**转发式**，见 CLAUDE.md「路由声明规约」第 9 条）
//!
//! `mod.rs` 只放 `pub fn router()`，路由表在 `handler::router()`。
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `dto.rs` | 入参 / 出参 + `RELEASE_DETAIL_LIMIT`（明细上限）与 `kind` 字面量常量 |
//! | `repo.rs` | 2 条 `COUNT` + 2 条候选 SELECT（`FOR UPDATE` 由形参控制），**唯一持有守卫谓词真源** |
//! | `service.rs` | 释放用例（dry_run / apply 两分支）+ 计数口径 + 明细拼装 |
//! | `handler.rs` | 1 个端点 + `ROUTES` + `router()`；事务边界、WS 广播 |
//!
//! **权威路由清单是 `handler::ROUTES`（1 条）**，lib 单测
//! `handler::tests::routes_declared_in_router` 用
//! `crate::modules::files::route_guard::routes_in_router_source` 逐条比对 `router()`
//! 源码（复用该共用比对器，**不**复制一份）。
//!
//! ## 完整契约
//!
//! [`docs/api/serial.md`](../../../docs/api/serial.md) —— 端点表 / 出参逐字段 /
//! 计数与明细解耦口径 / 并发口径（含**锁序**）/ 移除记录 / WS 关系 / 表依赖与前端配套 /
//! 已知偏差登记。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub mod dto;
pub mod handler;
pub mod repo;
pub mod service;

/// 本域 router 工厂（转发式：路由表在 [`handler::router`]）。
///
/// 挂载点：`src/modules/mod.rs::v2_router()` 的 `.nest("/serials", …)`
/// —— 顶层 nest、单段、kebab-case。
pub fn router() -> Router<Arc<AppState>> {
    handler::router()
}
