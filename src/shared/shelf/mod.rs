//! 跨域货架设施层（2026-10-10 新增）
//!
//! 抽取动机：自动选架（`pick_least_loaded`）与货架负载聚合（`LOAD_AGGREGATE_SQL`）
//! 是**任何**碰「把批次落到某个架上」的域都要用的公共语义 —— 逐域自持会让
//! `prod::batch` / `prod::queue` / `outsource` 三域各写一份排序与聚合，两份漂移
//! 之后没有任何东西会报警（与 `shared::batch` 的抽取动机同形，都是「判序 / 口径
//! 是全仓一份的公共语义」）。
//!
//! | 文件 | 内容 | 依赖 |
//! |---|---|---|
//! | [`load`] | `ShelfLoad` 行结构 + `LOAD_AGGREGATE_SQL`（`t_part_batch` 按 holder 聚合）+ `load_ratio` | 仅 sqlx |
//! | [`select`] | `pick_least_loaded`（按负载选架）+ `shelf_scope_for`（`CurrentUser` → scope） | `auth` + [`load`] |
//! | [`pool_priority`] | `POOL_PRIORITY_ORDER_SQL`（候选池取件的 4 级优先级 ORDER BY 片段） | 无（纯常量） |
//!
//! ## 边界登记：`shared::shelf` **零域依赖**
//!
//! 本层只允许 import `crate::auth` / `crate::infra` / `crate::shared` /
//! `crate::state`，**不得 import 任何 `crate::modules::<域>`**。表数据一律自己写
//! SQL 在本层聚合 —— 这是本仓对「跨域只读聚合」的既定 pattern（`dashboard` /
//! `statistics` / `prod::queue::board` 三个模块都零域依赖）。
//!
//! 与 `shared::batch` 的对照很说明问题：`shared::batch` 之所以破例依赖 4 个域，
//! 是因为「三层状态派生」的实现天然跨 part / assembly / batch 且无域可归属（它的
//! 模块 doc 有完整论证）。选架没有这个性质 —— 「某个架上现在有多少件」是一条
//! `t_part_batch` 的 `GROUP BY`，把它写在域里反而制造了「谁拥有这条聚合」的
//! 无谓争议。故本层**坚持自己写 SQL**，即便代价是同一张 `t_shelf` 表在
//! `modules::shelf::repo::sql.rs` 里另有一份查询。
//!
//! ## 不放什么
//!
//! - 货架 **CRUD**（`t_shelf` 的增删改查、软删、`count_in_use_parts` 引用计数）
//!   —— 归 `shelf` 域，本层只提供「读负载 + 选架」两项；
//! - 货架 ↔ 工序**映射**的写侧（`t_shelf_process`）—— 归 `prod::shelf_process`
//!   域。选架的 SQL 自己 `EXISTS` 查那张表（只读，且需要 `deleted_at IS NULL`
//!   闸门与 `t_shelf` 的可用性谓词在**同一条** SQL 里生效），不 import 该域 repo；
//! - picker 端点（`GET /shelves/for-return` / `for-inspection`）—— 2026-10-10
//!   随自动选架一并下线（移除记录见 `docs/api/shelves.md`）。

pub mod load;
pub mod pool_priority;
pub mod select;

pub use load::{LOAD_AGGREGATE_SQL, ShelfLoad};
pub use pool_priority::POOL_PRIORITY_ORDER_SQL;
pub use select::{pick_least_loaded, shelf_scope_for};
