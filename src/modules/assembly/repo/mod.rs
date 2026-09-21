//! assembly 域 repo 层（SQL 真源 + 胖 trait + PG 实现）
//!
//! ## 结构（2026-09-22 重构）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，11 个 pub 固有静态方法 + sqlx `query!` 宏，
//!   **内容零 diff**（`.sqlx/query-*.json` 哈希不变）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `AssemblyRepoTrait`（16 方法合并单 trait；
//!   含 t_assembly CRUD 9 + sync hook helper 2 + 跨域 helper 5），并直接
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
//!   `<R: AssemblyRepoTrait>` 收。trait 方法数 16（t_assembly 11 + 跨域 helper 5）。
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
//! assembly service 在创建 / 更新 / 详情 / 文件上传 / 同步钩子中需要查 `t_customer`
//! （L1 展开 + parent_id 校验 + serial_prefix 派发）、`t_part`（子件列表 + 父件级联 +
//! 套数缩放 + 子件创建）、`t_part_file`（CAS 去重 + 列表 + INSERT）、`t_part_batch`
//! （current_batch_id 派生）。这些跨域 SQL 通过本 trait 收口：
//! - trait 方法体 = `PartRepo::xxx(&mut **self, ...)` / `PartFileRepo::xxx(&mut **self, ...)`
//!   或直接 sqlx::query_as 单条 SQL；
//! - service 不再持有 `&mut PgConnection`（除了 trait 借出的隐式连接），无法跨域调静态方法。
//!
//! 这一收口对 D-6（part 重构）和未来 part_file 重构均为零影响——service 仅面向
//! `AssemblyRepoTrait`，不直接依赖 part / part_file 域的 repo 路径。

use async_trait::async_trait;
use sqlx::PgConnection;

use super::model::TAssembly;
use crate::modules::part::model::TPart;
use crate::modules::part::repo::part::ChildInheritFields;
use crate::modules::part::repo::PartRepo;
use crate::modules::part_file::model::TPartFile;
use crate::modules::part_file::repo::{NewPartFile, PartFileRepo};

pub mod sql;

// 重导出 sql.rs 中的 ZST struct / 表行类型 / INSERT UPDATE 入参，让上层继续用
// `super::repo::{AssemblyRepo, NewAssembly, AssemblyUpdate, AssemblyListFilters, TAssembly}`
// 这种路径不破（cross-module 调用方 delivery_note 都依赖 `AssemblyRepo` ZST）。
pub use sql::{AssemblyListFilters, AssemblyRepo, AssemblyUpdate, NewAssembly};

/// assembly 域数据访问 trait（16 方法 = t_assembly CRUD 9 + sync helper 2 + 跨域 helper 5）。
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
    async fn insert<'a>(
        &mut self,
        new: NewAssembly<'a>,
    ) -> Result<i64, sqlx::Error>;
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
    async fn cancel(
        &mut self,
        id: i64,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error>;

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

    /// part_file CAS 去重查（`PartFileRepo::get_by_owner_kind_sha`）。
    async fn part_file_get_by_owner_kind_sha<'a>(
        &mut self,
        owner_id: i64,
        kind: &'a str,
        sha: &'a str,
    ) -> Result<Option<TPartFile>, sqlx::Error>;

    /// part_file INSERT（`PartFileRepo::create_part_file`）。
    async fn part_file_create<'a>(
        &mut self,
        nf: NewPartFile<'a>,
    ) -> Result<i64, sqlx::Error>;

    /// part_file 列表（`PartFileRepo::list_by_owner`）。
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

    /// L1 → L2 子节点 + 自身展开（recursive CTE）；若入参是 L2 直接返回 `[customer_id]`。
    async fn expand_customer_l2_ids(
        &mut self,
        customer_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error>;

    /// customer.serial_prefix（用于 PDF 上传序列号派发）。
    async fn fetch_customer_serial_prefix(
        &mut self,
        customer_id: i64,
    ) -> Result<Option<String>, sqlx::Error>;

    /// customer L1 id（`COALESCE(parent_id, id)`，把 L2 叶子转回 L1）。
    async fn fetch_customer_l1_id(
        &mut self,
        customer_id: i64,
    ) -> Result<Option<i64>, sqlx::Error>;

    /// customer name + parent_id 批量查（防 N+1，`ids` 为空返回空 HashMap）。
    async fn fetch_customer_names_by_ids<'a>(
        &mut self,
        ids: &'a [i64],
    ) -> Result<std::collections::HashMap<i64, (String, Option<i64>)>, sqlx::Error>;

    /// 子件挂送货单预检（PR-2 后改 JOIN t_part_batch 查）。
    async fn has_active_shipment_for_assembly(
        &mut self,
        assembly_id: i64,
    ) -> Result<bool, sqlx::Error>;

    /// 反查 part 的 assembly_id（None = part 不存在；Some(None) = part 无父；Some(Some) = 有父）。
    /// 用于 `sync_from_part_change_inner` 短路判断。
    async fn fetch_part_assembly_id(
        &mut self,
        part_id: i64,
    ) -> Result<Option<Option<i64>>, sqlx::Error>;

    /// 批量反查 part 的 assembly_id（去重 + 仅返回 IS NOT NULL）。
    /// 用于 `sync_from_part_changes` 一次性拿到全部需 sync 的父装配件。
    async fn fetch_distinct_assembly_ids_by_part_ids<'a>(
        &mut self,
        part_ids: &'a [i64],
    ) -> Result<Vec<i64>, sqlx::Error>;

    /// 序列号派发（`crate::shared::serial::acquire` 的 trait 包装）。
    /// service 不直接调 serial::acquire（后者要 `&mut PgConnection`），故经 trait 收口。
    async fn acquire_serial(&mut self, prefix: char) -> Result<String, crate::shared::error::AppError>;
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
    async fn insert<'b>(
        &mut self,
        new: NewAssembly<'b>,
    ) -> Result<i64, sqlx::Error> {
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
    async fn cancel(
        &mut self,
        id: i64,
        current_user_id: i64,
    ) -> Result<u64, sqlx::Error> {
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

    async fn part_file_get_by_owner_kind_sha<'b>(
        &mut self,
        owner_id: i64,
        kind: &'b str,
        sha: &'b str,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        PartFileRepo::get_by_owner_kind_sha(&mut **self, owner_id, kind, sha).await
    }

    async fn part_file_create<'b>(
        &mut self,
        nf: NewPartFile<'b>,
    ) -> Result<i64, sqlx::Error> {
        PartFileRepo::create_part_file(&mut **self, nf).await
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
        let row: Option<(i32,)> = sqlx::query_as(
            "SELECT quantity FROM t_assembly WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(assembly_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row.map(|(q,)| q))
    }

    async fn fetch_current_batch_ids_for_parts<'b>(
        &mut self,
        part_ids: &'b [i64],
    ) -> Result<std::collections::HashMap<i64, Option<i64>>, sqlx::Error> {
        let mut out: std::collections::HashMap<i64, Option<i64>> =
            std::collections::HashMap::new();
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
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(customer_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row.map(|(p,)| p))
    }

    async fn expand_customer_l2_ids(
        &mut self,
        customer_id: i64,
    ) -> Result<Vec<i64>, sqlx::Error> {
        // 先看自身是不是 L1（parent_id IS NULL） → 是：收集自身 + 所有 L2 子节点；否：仅自身
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT parent_id FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(customer_id)
        .fetch_optional(&mut **self)
        .await?;
        match row {
            Some((None,)) => {
                // L1：递归取所有 L2（用 recursive CTE 一次拿齐）
                let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
                    "WITH RECURSIVE subtree AS (SELECT id FROM t_customer WHERE id = ",
                );
                qb.push_bind(customer_id);
                qb.push(
                    " AND deleted_at IS NULL UNION ALL SELECT c.id FROM t_customer c \
                     INNER JOIN subtree s ON c.parent_id = s.id WHERE c.deleted_at IS NULL) \
                     SELECT id FROM subtree",
                );
                let ids: Vec<(i64,)> = qb.build_query_as().fetch_all(&mut **self).await?;
                Ok(ids.into_iter().map(|(i,)| i).collect())
            }
            _ => Ok(vec![customer_id]),
        }
    }

    async fn fetch_customer_serial_prefix(
        &mut self,
        customer_id: i64,
    ) -> Result<Option<String>, sqlx::Error> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT serial_prefix FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(customer_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row.and_then(|(p,)| p))
    }

    async fn fetch_customer_l1_id(
        &mut self,
        customer_id: i64,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT COALESCE(parent_id, id) FROM t_customer WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(customer_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row.map(|(p,)| p))
    }

    async fn fetch_customer_names_by_ids<'b>(
        &mut self,
        ids: &'b [i64],
    ) -> Result<std::collections::HashMap<i64, (String, Option<i64>)>, sqlx::Error> {
        let mut out: std::collections::HashMap<i64, (String, Option<i64>)> =
            std::collections::HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let mut qb: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
            "SELECT id, name, parent_id FROM t_customer WHERE deleted_at IS NULL AND id IN (",
        );
        let mut sep = qb.separated(", ");
        for id in ids {
            sep.push_bind(*id);
        }
        qb.push(")");
        let rows: Vec<(i64, String, Option<i64>)> =
            qb.build_query_as().fetch_all(&mut **self).await?;
        for (i, n, p) in rows {
            out.insert(i, (n, p));
        }
        Ok(out)
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
        let row: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT assembly_id FROM t_part WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .fetch_optional(&mut **self)
        .await?;
        Ok(row.map(|(a,)| a))
    }

    async fn fetch_distinct_assembly_ids_by_part_ids<'b>(
        &mut self,
        part_ids: &'b [i64],
    ) -> Result<Vec<i64>, sqlx::Error> {
        if part_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(Option<i64>,)> = sqlx::query_as(
            "SELECT DISTINCT assembly_id FROM t_part \
             WHERE id = ANY($1) AND assembly_id IS NOT NULL \
               AND deleted_at IS NULL",
        )
        .bind(part_ids)
        .fetch_all(&mut **self)
        .await?;
        Ok(rows.into_iter().filter_map(|(a,)| a).collect())
    }

    async fn acquire_serial(
        &mut self,
        prefix: char,
    ) -> Result<String, crate::shared::error::AppError> {
        crate::shared::serial::acquire(&mut **self, prefix).await
    }
}
