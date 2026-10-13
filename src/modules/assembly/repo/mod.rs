//! assembly 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 重构）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，11 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `AssemblyRepoTrait`（**14 方法**合并单 trait；
//!   含 t_assembly CRUD 9 + sync hook helper 2 + 跨域 helper 3），并直接
//!   `impl AssemblyRepoTrait for &mut PgConnection`——handler/service 借 `&mut *tx` /
//!   `&mut *conn` 即可，零中间壳。
//!
//! ## 为什么 trait 命名为 `AssemblyRepoTrait` 而非 `AssemblyRepo`
//! delivery_note 域 service/{crud,inner,scan}.rs 都 `use crate::modules::assembly::repo::AssemblyRepo;`
//! 然后 `AssemblyRepo::list_by_ids / get_by_id / get_by_serial(&mut *conn, ...)`
//! 走 ZST 静态方法。**该 3 文件本次不在本任务范围**（属于 Group D-5/D-6/E），故本任务不能
//! 破坏 `assembly::repo::AssemblyRepo` 作为 ZST 的对外身份。
//!
//! 解法：
//! - `assembly::repo::AssemblyRepo` —— ZST struct（在 `sql.rs` 内，通过 `pub use sql::AssemblyRepo;`
//!   重新导出至本模块），保留 11 个 t_assembly 静态方法签名不变（cross-module 调用方零修改）。
//! - `assembly::repo::AssemblyRepoTrait` —— 本文件新加的胖 trait，assembly 域内部 service 用
//!   `<R: AssemblyRepoTrait>` 收。
//!
//! 2026-10-13 PR1b：跨域 helper 5 → **3**（删 `part_file_get_by_owner_kind_sha`
//! CAS 去重查 / `part_file_create` INSERT，两者的唯一调用方是被删的
//! `POST /assemblies/{id}/files` 上传端点）。剩下 3 个跨域 helper 全是
//! `t_customer` / `t_part` / `t_part_batch` 的只读 + 写，`t_part_file` 只剩
//! `list_part_files_by_owner`（详情 `files[]` 用）。
//!
//! ## 为什么是胖 trait 而不是按实体拆 trait
//! `&mut PgConnection` 同一作用域只能借给一个 repo 实例；如果按实体拆，service 同时使用
//! 时无法表达「同连接两次借用」。胖 trait `AssemblyRepoTrait` 是单借位，service 签名
//! `<R: AssemblyRepoTrait>(&self, mut repo: R, ...)` 一次收下（by-value；生产路径
//! `R = &mut PgConnection`，单测 `R = MockAssemblyRepoTrait`），方法体内全部 `repo.xxx()`
//! 都走同一连接。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::AssemblyRepo::yyy`
//! 和其他静态方法（PartRepo / PartFileRepo）。无需任何 `PgAssemblyRepo<'a>` 转发壳（与
//! iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockAssemblyRepoTrait` 供
//! service 单测注入。方法签名里的 `<'a>` 显式生命周期是 mockall 0.15 + async_trait
//! 的硬性要求。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1）。
//!
//! ## 跨域 helper 设计
//! assembly service 在创建 / 更新 / 详情 / 同步钩子中需要查 `t_customer`
//! （L1 展开 + parent_id 校验 + serial_prefix 派发）、`t_part`（子件列表 + 父件级联 +
//! 套数缩放 + 子件创建）、`t_part_file`（**只剩列表** —— 文件写入 2026-10-13 PR1b
//! 起全部归 `files::part_file`）、`t_part_batch`（current_batch_id 派生）。
//! 这些跨域 SQL 通过本 trait 收口：
//! - trait 方法体 = `PartRepo::xxx(&mut **self, ...)` / `PartFileRepo::xxx(&mut **self, ...)`
//!   或直接 sqlx::query_as 单条 SQL；
//! - service 不再持有 `&mut PgConnection`（除了 trait 借出的隐式连接），无法跨域调静态方法。
//!
//! 这一收口对 D-6（part 重构）和未来 part_file 重构均为零影响——service 仅面向
//! `AssemblyRepoTrait`，不直接依赖 part / part_file 域的 repo 路径。

use async_trait::async_trait;
use sqlx::PgConnection;

use super::model::TAssembly;
use crate::modules::files::part_file::model::TPartFile;
use crate::modules::files::part_file::repo::PartFileRepo;
use crate::modules::part::model::TPart;
use crate::modules::part::repo::PartRepo;
use crate::modules::part::repo::part::{ChildInheritFields, NewPartCreate};
use crate::modules::prod::batch::repo::{NewInitialBatch, PartBatchRepo};
use crate::shared::error::AppError;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct / 表行类型 / INSERT UPDATE 入参，让上层继续用
// `super::repo::{AssemblyRepo, NewAssembly, AssemblyUpdate, AssemblyListFilters, TAssembly}`
// 这种路径不破（cross-module 调用方 delivery_note 都依赖 `AssemblyRepo` ZST）。
pub use sql::{AssemblyListFilters, AssemblyRepo, AssemblyUpdate, NewAssembly};

/// assembly 域数据访问 trait（20 方法 = t_assembly CRUD 9 + sync helper 2 + 跨域 helper 9）。
///
/// 2026-09-25 补 3 端点（api-drift-fix）：新增跨域 helper
///（`create_simple_child_part` / `create_initial_part_batch` / `get_part_by_id`）。
/// `fetch_customer_names_map` 在第 1 轮 review 被标记为死代码，已删除（2026-09-25）。
///
/// 2026-10-17（客户树无限层级化）三条客户相关方法的形态变更：
/// - `lookup_customer_names` / `fetch_customer_names_by_ids` **删除**，取名统一走
///   `customer_paths_by_ids`（委托 `shared::customer::paths_of`）；
/// - `expand_customer_l2_ids` → `subtree_ids`，委托 `shared::customer::subtree_ids`。
///
/// 单 trait 而非每实体一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 重构定案；与 iam / shelf 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参。`<'a>` 显式生命周期是 mockall
/// 0.15 automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait AssemblyRepoTrait: Send {
    // ── t_assembly reads (3) ──
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TAssembly>, sqlx::Error>;
    async fn list_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
        include_deleted: bool,
    ) -> Result<Vec<TAssembly>, sqlx::Error>;
    async fn get_by_serial<'a>(
        &mut self,
        serial_no: &'a str,
        include_deleted: bool,
    ) -> Result<Option<TAssembly>, sqlx::Error>;

    // ── t_assembly writes (3) ──
    async fn insert<'a>(&mut self, new: NewAssembly<'a>) -> Result<i64, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn update_partial<'a>(
        &mut self,
        id: i64,
        expected_version: i32,
        upd: AssemblyUpdate<'a>,
    ) -> Result<u64, sqlx::Error>;
    async fn soft_delete(
        &mut self,
        id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── t_assembly status transitions (1) ──
    async fn cancel(&mut self, id: i64, current_user_id: i64) -> Result<u64, sqlx::Error>;

    // ── t_assembly list / count (2) ──
    #[allow(clippy::too_many_arguments)]
    async fn list_with_filters<'a>(
        &mut self,
        customer_ids: &'a [i64],
        status: Option<&'a str>,
        statuses: &'a [String],
        is_urgent: Option<bool>,
        keyword: Option<&'a str>,
        sort_by: Option<&'a str>,
        sort_dir: Option<&'a str>,
        limit: i64,
        offset: i64,
        include_deleted: bool,
    ) -> Result<Vec<TAssembly>, sqlx::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn count_with_filters<'a>(
        &mut self,
        customer_ids: &'a [i64],
        status: Option<&'a str>,
        statuses: &'a [String],
        is_urgent: Option<bool>,
        keyword: Option<&'a str>,
        include_deleted: bool,
    ) -> Result<i64, sqlx::Error>;

    // ── sync hook helpers (2) ──
    async fn aggregate_children_status(
        &mut self,
        assembly_id: i64,
    ) -> Result<Vec<String>, sqlx::Error>;
    async fn update_status_if_not_terminal<'a>(
        &mut self,
        id: i64,
        expected_version: i32,
        new_status: &'a str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    /// 2026-10-01 新增：终态清空 `serial_no`（父装配件无事件表，故不归档；
    /// 详见 `AssemblyRepo::clear_serial_no_if_terminal` 的 doc）。
    async fn clear_serial_no_if_terminal(
        &mut self,
        id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;

    // ── 跨域 helper（5）── 委托 part / part_file / t_customer / t_part_batch 的 SQL ──
    //
    // 设计动机：assembly service 在多处需要查 / 写跨域表（part 子件 + part_file CAS +
    // customer L1 展开 + part_batch current_batch_id 派生）。如果 service 仍直接持有
    // `&mut PgConnection` 调 `PartRepo::xxx(&mut *conn, ...)`，就破坏「`&mut PgConnection`
    // 同一作用域只能借给一个 repo 实例」的胖 trait 原则（service 同时持 repo: R 借位 +
    // 隐式连接，会触发重复借用）。
    //
    // 解法：所有跨域 SQL 通过本 trait 收口，impl 一行委托到对应 ZST 静态方法。

    /// part 子件列表（`PartRepo::list_by_assembly_id`）。
    async fn list_parts_by_assembly_id(
        &mut self,
        assembly_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error>;

    /// part 子件创建（`PartRepo::insert_child_for_assembly`，11 个形参）。
    #[allow(clippy::too_many_arguments)]
    async fn insert_part_child_for_assembly<'a>(
        &mut self,
        id: i64,
        customer_id: i64,
        assembly_id: i64,
        serial_no: &'a str,
        name: &'a str,
        drawing_no: Option<&'a str>,
        quantity: i32,
        planned_delivery_date: Option<chrono::NaiveDate>,
        unit_price: Option<rust_decimal::Decimal>,
        total_price: Option<rust_decimal::Decimal>,
        inherit: ChildInheritFields<'a>,
        current_user_id: i64,
        initial_batch_id: i64,
    ) -> Result<(), sqlx::Error>;

    /// 父件级联覆盖到所有未软删子件（`PartRepo::cascade_sync_from_assembly`，10 形参）。
    #[allow(clippy::too_many_arguments)]
    async fn cascade_sync_from_assembly<'a>(
        &mut self,
        assembly_id: i64,
        request_date: chrono::NaiveDate,
        applicant_name: &'a str,
        order_no: Option<&'a str>,
        system_delivery_date: Option<chrono::NaiveDate>,
        planned_delivery_date: chrono::NaiveDate,
        is_urgent: bool,
        note: Option<&'a str>,
        customer_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;

    /// 子件套数缩放（`PartRepo::scale_children_quantity`，4 形参）。
    async fn scale_children_quantity(
        &mut self,
        assembly_id: i64,
        old_qty: i32,
        new_qty: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;

    /// part_file 列表（`PartFileRepo::list_by_owner`）。
    ///
    /// ⚠️ 2026-10-13 PR1b：同组另两个 helper（`part_file_get_by_owner_kind_sha`
    /// CAS 去重查 / `part_file_create` INSERT）随 `POST /assemblies/{id}/files`
    /// 端点下线一并删除 —— 本 trait 自此只剩这一个 part_file 跨域 helper，
    /// 唯一调用方是 `service/crud.rs` 的 `GET /assemblies/{id}` 详情（`files[]`）。
    async fn list_part_files_by_owner<'a>(
        &mut self,
        owner_kind: &'a str,
        owner_id: i64,
    ) -> Result<Vec<TPartFile>, sqlx::Error>;

    /// assembly 行 quantity（用于 §3.3 缩放触发判断）；`None` 表示不存在。
    async fn fetch_assembly_quantity(
        &mut self,
        assembly_id: i64,
    ) -> Result<Option<i32>, sqlx::Error>;

    /// 子件 current_batch_id（最近一条活跃 batch）；空输入返回空 HashMap。
    async fn fetch_current_batch_ids_for_parts<'a>(
        &mut self,
        part_ids: &'a [i64],
    ) -> Result<std::collections::HashMap<i64, Option<i64>>, sqlx::Error>;

    /// customer.parent_id（None=不存在；Some(None)=L1；Some(Some(pid))=L2）。
    async fn fetch_customer_parent_id(
        &mut self,
        customer_id: i64,
    ) -> Result<Option<Option<i64>>, sqlx::Error>;

    /// 客户筛选展开：`[customer_id]` + 其任意深度的全部后代（**纯子树**）。
    ///
    /// 2026-10-17 起委托 [`crate::shared::customer::subtree_ids`]（全仓唯一的客户树
    /// 遍历设施），本域不再自写递归 CTE。语义变更：旧的 `expand_customer_l2_ids`
    /// 对「非根节点」只返回**自身**（两层时代的假设：非 L1 必然是叶子），在三级以上
    /// 的树上会把选中的节点的下级整批漏掉。
    async fn subtree_ids(&mut self, customer_id: i64) -> Result<Vec<i64>, AppError>;

    /// 建单派序列号用的 L1 客户 `serial_prefix` 首字符。委托
    /// `shared::serial::prefix_for_customer`（与 part 域同一实现，全仓只一份）。
    async fn serial_prefix_for_customer(
        &mut self,
        customer_id: i64,
    ) -> Result<char, crate::shared::error::AppError>;

    /// 是否存在**直接**子节点（未软删）。委托
    /// [`crate::shared::customer::has_children`]。
    ///
    /// ⚠️ 它分不出「叶子」与「客户不存在」（两种情况都返 `false`），故调用方必须
    /// 另配一个存在性判据（本域是 `fetch_customer_parent_id` 返回 `None`）。
    async fn has_children(&mut self, customer_id: i64) -> Result<bool, sqlx::Error>;

    /// 业务单据只能挂**叶子**客户的闸门（2026-10-17 新增；建单与改客户端点共用）。
    ///
    /// 有未软删子节点 → `20104 BIZ_INVALID_VALUE`，消息带客户路径与下级节点数。
    /// 收在 repo 层是因为 service 方法泛型 `R: AssemblyRepoTrait`（拿不到
    /// `&mut PgConnection`）。2026-10-17 起方法体委托
    /// [`crate::shared::customer::require_leaf_customer`]（原先是本域 service 里
    /// 两段各抄一遍的内联实现）。
    ///
    /// ⚠️ 同样**不分「客户不存在」**：调用方仍须各自先跑
    /// `fetch_customer_parent_id` 的存在性闸门。
    async fn require_leaf_customer(&mut self, customer_id: i64) -> Result<(), AppError>;

    /// 客户三列（`root_name` / `own_name` / `path`）批量查（防 N+1，空 ids 短路）。
    ///
    /// 2026-10-17 新增，取代 `fetch_customer_names_by_ids`（后者只取 `name` +
    /// `parent_id`，父名要调用方自己再上跳一层查一次，三层树上必错）。委托
    /// [`crate::shared::customer::paths_of`]。
    async fn customer_paths_by_ids(
        &mut self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, crate::shared::customer::CustomerPath>, AppError>;

    /// 子件挂送货单预检（PR-2 后改 JOIN t_part_batch 查）。
    async fn has_active_shipment_for_assembly(
        &mut self,
        assembly_id: i64,
    ) -> Result<bool, sqlx::Error>;

    // ── 跨域 helper（追加：2026-09-25 补 3 端点）──

    /// 单 part 子件创建（无 serial 派生；用于
    /// `POST /api/v2/assemblies/{id}/children`）。委托 `PartRepo::create_part`。
    #[allow(clippy::too_many_arguments)]
    async fn create_simple_child_part<'a>(
        &mut self,
        new: NewPartCreate<'a>,
    ) -> Result<i64, sqlx::Error>;

    /// 配套初始 `t_part_batch`（batch_no=1 / status='PENDING' / location=NULL）。
    /// 委托 `PartBatchRepo::create_initial_batch`。
    async fn create_initial_part_batch<'a>(
        &mut self,
        nf: NewInitialBatch<'a>,
    ) -> Result<i64, sqlx::Error>;

    /// 读回单条 part（按 id，include_deleted 旗标）—— 给 add_assembly_child
    /// service 在 INSERT 后做 read-back 用（与 PartService::create_part 同形）。
    async fn get_part_by_id(
        &mut self,
        part_id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error>;

    /// 反查 part 的 assembly_id（None = part 不存在；Some(None) = part 无父；Some(Some) = 有父）。
    /// 用于 `sync_from_part_change_by_id_inner` 短路判断。
    async fn fetch_part_assembly_id(
        &mut self,
        part_id: i64,
    ) -> Result<Option<Option<i64>>, sqlx::Error>;

    /// 序列号派发（`crate::shared::serial::acquire` 的 trait 包装）。
    /// service 不直接调 serial::acquire（后者要 `&mut PgConnection`），故经 trait 收口。
    async fn acquire_serial(
        &mut self,
        prefix: char,
    ) -> Result<String, crate::shared::error::AppError>;
}

/// 把 `AssemblyRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借 `&mut *tx` 或
/// `&mut *conn` 即可调用 `sql::AssemblyRepo::yyy`，零转发壳（与 iam 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::AssemblyRepo::yyy` / `PartRepo::xxx` 等静态方法须写 `&mut **self`
///（reborrow，避免 move 引用本身）。
///
/// 2026-09-22 决策：list_with_filters / count_with_filters 在 trait 层把 `AssemblyListFilters`
/// 拆成扁平形参（避免 `AssemblyListFilters<'a>` 形参跨 trait 边界传递的隐式生命周期绑定）；
/// impl 内一行重新组装 `AssemblyListFilters` 喂给 `sql::AssemblyRepo::list_with_filters`。
/// 这样 trait 方法签名 ≤ 9 个形参（避免 clippy::too_many_arguments 再次命中）。
#[async_trait]
impl AssemblyRepoTrait for &mut PgConnection {
    // ── t_assembly reads (3) ──
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TAssembly>, sqlx::Error> {
        AssemblyRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn list_by_ids<'b>(
        &mut self,
        ids: &'b [i64],
        include_deleted: bool,
    ) -> Result<Vec<TAssembly>, sqlx::Error> {
        AssemblyRepo::list_by_ids(&mut **self, ids, include_deleted).await
    }

    async fn get_by_serial<'b>(
        &mut self,
        serial_no: &'b str,
        include_deleted: bool,
    ) -> Result<Option<TAssembly>, sqlx::Error> {
        AssemblyRepo::get_by_serial(&mut **self, serial_no, include_deleted).await
    }

    // ── t_assembly writes (3) ──
    async fn insert<'b>(&mut self, new: NewAssembly<'b>) -> Result<i64, sqlx::Error> {
        AssemblyRepo::insert(&mut **self, new).await
    }

    async fn update_partial<'b>(
        &mut self,
        id: i64,
        expected_version: i32,
        upd: AssemblyUpdate<'b>,
    ) -> Result<u64, sqlx::Error> {
        AssemblyRepo::update_partial(&mut **self, id, expected_version, upd).await
    }

    async fn soft_delete(
        &mut self,
        id: i64,
        expected_version: i32,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
        AssemblyRepo::soft_delete(&mut **self, id, expected_version, current_user_id).await
    }

    // ── t_assembly status transitions (1) ──
    async fn cancel(&mut self, id: i64, current_user_id: i64) -> Result<u64, sqlx::Error> {
        AssemblyRepo::cancel(&mut **self, id, current_user_id).await
    }

    // ── t_assembly list / count (2) ── 一行委托 sql::AssemblyRepo ─────
    async fn list_with_filters<'b>(
        &mut self,
        customer_ids: &'b [i64],
        status: Option<&'b str>,
        statuses: &'b [String],
        is_urgent: Option<bool>,
        keyword: Option<&'b str>,
        sort_by: Option<&'b str>,
        sort_dir: Option<&'b str>,
        limit: i64,
        offset: i64,
        include_deleted: bool,
    ) -> Result<Vec<TAssembly>, sqlx::Error> {
        let filters = AssemblyListFilters {
            customer_ids,
            status,
            statuses,
            is_urgent,
            keyword,
            // 2026-09-30 新增：trait 层 list_with_filters 不带日期过滤形态（trait
            // 形参上限已达 10），固定 None 维持旧行为。仅 com::union_list 端点
            // 直接构造 AssemblyListFilters 时传实际日期值。
            planned_delivery_date_from: None,
            planned_delivery_date_to: None,
            // 2026-09-30 新增：trait 层不暴露 10 字段（4 文本 + 4 日期 + 2
            // IS NULL）形态（trait 形参上限已达 10），固定 None 维持旧行为。
            // com::union_list 端点直接构造 AssemblyListFilters 时传实际值。
            drawing_no_pat: None,
            name_pat: None,
            order_no_pat: None,
            serial_no_pat: None,
            request_date_from: None,
            request_date_to: None,
            system_delivery_date_from: None,
            system_delivery_date_to: None,
            order_no_is_null: None,
            system_delivery_date_is_null: None,
            sort_by,
            sort_dir,
            limit,
            offset,
            include_deleted,
        };
        AssemblyRepo::list_with_filters(&mut **self, &filters).await
    }

    async fn count_with_filters<'b>(
        &mut self,
        customer_ids: &'b [i64],
        status: Option<&'b str>,
        statuses: &'b [String],
        is_urgent: Option<bool>,
        keyword: Option<&'b str>,
        include_deleted: bool,
    ) -> Result<i64, sqlx::Error> {
        // sort_by/sort_dir/limit/offset 仅 list 用；count 不读，故给 None / 0 占位。
        let filters = AssemblyListFilters {
            customer_ids,
            status,
            statuses,
            is_urgent,
            keyword,
            // 2026-09-30 新增：与 list_with_filters trait 方法同理，固定 None。
            planned_delivery_date_from: None,
            planned_delivery_date_to: None,
            // 2026-09-30 新增：10 字段 trait 层不暴露，固定 None 维持旧行为。
            drawing_no_pat: None,
            name_pat: None,
            order_no_pat: None,
            serial_no_pat: None,
            request_date_from: None,
            request_date_to: None,
            system_delivery_date_from: None,
            system_delivery_date_to: None,
            order_no_is_null: None,
            system_delivery_date_is_null: None,
            sort_by: None,
            sort_dir: None,
            limit: 0,
            offset: 0,
            include_deleted,
        };
        AssemblyRepo::count_with_filters(&mut **self, &filters).await
    }

    // ── sync hook helpers (2) ── 一行委托 sql::AssemblyRepo ─────────
    async fn aggregate_children_status(
        &mut self,
        assembly_id: i64,
    ) -> Result<Vec<String>, sqlx::Error> {
        AssemblyRepo::aggregate_children_status(&mut **self, assembly_id).await
    }

    async fn update_status_if_not_terminal<'b>(
        &mut self,
        id: i64,
        expected_version: i32,
        new_status: &'b str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        AssemblyRepo::update_status_if_not_terminal(
            &mut **self,
            id,
            expected_version,
            new_status,
            updated_by,
        )
        .await
    }

    async fn clear_serial_no_if_terminal(
        &mut self,
        id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        AssemblyRepo::clear_serial_no_if_terminal(&mut **self, id, updated_by).await
    }

    // ── 跨域 helper（5）── 一行委托 PartRepo / PartFileRepo / inline sqlx ──
    async fn list_parts_by_assembly_id(
        &mut self,
        assembly_id: i64,
        include_deleted: bool,
    ) -> Result<Vec<TPart>, sqlx::Error> {
        PartRepo::list_by_assembly_id(&mut **self, assembly_id, include_deleted).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_part_child_for_assembly<'b>(
        &mut self,
        id: i64,
        customer_id: i64,
        assembly_id: i64,
        serial_no: &'b str,
        name: &'b str,
        drawing_no: Option<&'b str>,
        quantity: i32,
        planned_delivery_date: Option<chrono::NaiveDate>,
        unit_price: Option<rust_decimal::Decimal>,
        total_price: Option<rust_decimal::Decimal>,
        inherit: ChildInheritFields<'b>,
        current_user_id: i64,
        initial_batch_id: i64,
    ) -> Result<(), sqlx::Error> {
        PartRepo::insert_child_for_assembly(
            &mut **self,
            id,
            customer_id,
            assembly_id,
            serial_no,
            name,
            drawing_no,
            quantity,
            planned_delivery_date,
            unit_price,
            total_price,
            inherit,
            current_user_id,
            initial_batch_id,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn cascade_sync_from_assembly<'b>(
        &mut self,
        assembly_id: i64,
        request_date: chrono::NaiveDate,
        applicant_name: &'b str,
        order_no: Option<&'b str>,
        system_delivery_date: Option<chrono::NaiveDate>,
        planned_delivery_date: chrono::NaiveDate,
        is_urgent: bool,
        note: Option<&'b str>,
        customer_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::cascade_sync_from_assembly(
            &mut **self,
            assembly_id,
            request_date,
            applicant_name,
            order_no,
            system_delivery_date,
            planned_delivery_date,
            is_urgent,
            note,
            customer_id,
            updated_by,
        )
        .await
    }

    async fn scale_children_quantity(
        &mut self,
        assembly_id: i64,
        old_qty: i32,
        new_qty: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        PartRepo::scale_children_quantity(&mut **self, assembly_id, old_qty, new_qty, updated_by)
            .await
    }

    async fn list_part_files_by_owner<'b>(
        &mut self,
        owner_kind: &'b str,
        owner_id: i64,
    ) -> Result<Vec<TPartFile>, sqlx::Error> {
        PartFileRepo::list_by_owner(&mut **self, owner_kind, owner_id).await
    }

    async fn fetch_assembly_quantity(
        &mut self,
        assembly_id: i64,
    ) -> Result<Option<i32>, sqlx::Error> {
        let row: Option<(i32,)> =
            sqlx::query_as("SELECT quantity FROM t_assembly WHERE id = $1 AND deleted_at IS NULL")
                .bind(assembly_id)
                .fetch_optional(&mut **self)
                .await?;
        Ok(row.map(|(q,)| q))
    }

    async fn fetch_current_batch_ids_for_parts<'b>(
        &mut self,
        part_ids: &'b [i64],
    ) -> Result<std::collections::HashMap<i64, Option<i64>>, sqlx::Error> {
        let mut out: std::collections::HashMap<i64, Option<i64>> = std::collections::HashMap::new();
        if part_ids.is_empty() {
            return Ok(out);
        }
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT DISTINCT ON (part_id) part_id, id \
             FROM t_part_batch \
             WHERE part_id = ANY($1) AND deleted_at IS NULL \
             ORDER BY part_id, batch_no DESC",
        )
        .bind(part_ids)
        .fetch_all(&mut **self)
        .await?;
        for (pid, bid) in rows {
            out.insert(pid, Some(bid));
        }
        for pid in part_ids {
            out.entry(*pid).or_insert(None);
        }
        Ok(out)
    }

    async fn fetch_customer_parent_id(
        &mut self,
        customer_id: i64,
    ) -> Result<Option<Option<i64>>, sqlx::Error> {
        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL")
                .bind(customer_id)
                .fetch_optional(&mut **self)
                .await?;
        Ok(row.map(|(p,)| p))
    }

    async fn subtree_ids(&mut self, customer_id: i64) -> Result<Vec<i64>, AppError> {
        crate::shared::customer::subtree_ids(&mut **self, customer_id).await
    }

    async fn serial_prefix_for_customer(
        &mut self,
        customer_id: i64,
    ) -> Result<char, crate::shared::error::AppError> {
        crate::shared::serial::prefix_for_customer(&mut **self, customer_id).await
    }

    async fn has_children(&mut self, customer_id: i64) -> Result<bool, sqlx::Error> {
        crate::shared::customer::has_children(&mut **self, customer_id).await
    }

    async fn require_leaf_customer(&mut self, customer_id: i64) -> Result<(), AppError> {
        crate::shared::customer::require_leaf_customer(&mut **self, customer_id, "装配件").await
    }

    async fn customer_paths_by_ids(
        &mut self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, crate::shared::customer::CustomerPath>, AppError>
    {
        crate::shared::customer::paths_of(&mut **self, ids).await
    }

    async fn has_active_shipment_for_assembly(
        &mut self,
        assembly_id: i64,
    ) -> Result<bool, sqlx::Error> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT 1::bigint \
             FROM t_part_batch pb \
             JOIN t_part p ON p.id = pb.part_id \
             WHERE p.assembly_id = $1 \
               AND p.deleted_at IS NULL \
               AND pb.deleted_at IS NULL \
               AND pb.delivery_note_id IS NOT NULL \
             LIMIT 1",
        )
        .bind(assembly_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row.is_some())
    }

    async fn fetch_part_assembly_id(
        &mut self,
        part_id: i64,
    ) -> Result<Option<Option<i64>>, sqlx::Error> {
        let row: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT assembly_id FROM t_part WHERE id = $1 AND deleted_at IS NULL")
                .bind(part_id)
                .fetch_optional(&mut **self)
                .await?;
        Ok(row.map(|(a,)| a))
    }

    async fn acquire_serial(
        &mut self,
        prefix: char,
    ) -> Result<String, crate::shared::error::AppError> {
        crate::shared::serial::acquire(&mut **self, prefix).await
    }

    // ── 跨域 helper（追加：2026-09-25 补 3 端点）──────────────────────────

    async fn create_simple_child_part<'b>(
        &mut self,
        new: NewPartCreate<'b>,
    ) -> Result<i64, sqlx::Error> {
        PartRepo::create_part(&mut **self, new).await
    }

    async fn create_initial_part_batch<'b>(
        &mut self,
        nf: NewInitialBatch<'b>,
    ) -> Result<i64, sqlx::Error> {
        PartBatchRepo::create_initial_batch(&mut **self, nf).await
    }

    async fn get_part_by_id(
        &mut self,
        part_id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPart>, sqlx::Error> {
        PartRepo::get_by_id(&mut **self, part_id, include_deleted).await
    }
}
