//! part_file 域 repo 层（胖 trait + PG 实现 + 跨域 helper）
//!
//! ## 结构（2026-09-22 重构对齐 iam 范式）
//! - `sql.rs`：原 `repo.rs` 全文搬迁，6 个 pub 固有静态方法 + 运行时 `sqlx::query_as` /
//!   `sqlx::query` / `sqlx::query_scalar`（**不**用 `query!` 宏，本域 SQL 改动频繁），
//!   **内容零 diff**（part_file 本无 `.sqlx/query-*.json` 缓存故无哈希影响）。
//! - `mod.rs`（本文件）：对外暴露胖 trait `PartFileRepoTrait`（**13** 方法合并单
//!   trait；含 sql.rs 6 方法 trait 化 + 跨域 owner 校验 **3** + 软删 2 + cnc 配对互写
//!   paired_file_id 2），并直接 `impl PartFileRepoTrait for &mut PgConnection` ——
//!   handler/service 借 `&mut *tx` / `&mut *conn` 即可，零中间壳。
//!   ⚠️ 2026-10-16 新增第 13 个方法 `customer_owner_exists`（`owner_kind='CUSTOMER'`
//!   的存在性闸门，送货单打印模板挂在**一级客户**上），它是本域**唯一**一处读
//!   `t_customer` 的地方 —— 属既定的「只读跨域聚合」pattern，详见该方法的 doc。
//!
//! ## 为什么 trait 命名为 `PartFileRepoTrait`（带 `Trait` 后缀）
//! 域外调用方 `assembly` / `part` 共 6 处直接调 `PartFileRepo::xxx(&mut *conn, ...)`
//! 走 ZST 静态方法（`assembly/repo/mod.rs` ×3、`part/service/crud.rs` ×2、
//! `part/service/batch.rs` ×1），故 ZST 形态是对外身份的一部分，不能只留 trait。故：
//! - `part_file::repo::PartFileRepo` —— ZST struct（在 `sql.rs` 内，通过
//!   `pub use sql::PartFileRepo;` 重新导出至本模块），保留 6 个 pub 静态方法签名不变
//!   （上述跨域调用方零修改）。
//! - `part_file::repo::PartFileRepoTrait` —— 本文件新加的胖 trait（13 方法），part_file 域
//!   内部 service 用 `<R: PartFileRepoTrait>` 收。
//!
//! ⚠️ 2026-10-13 收编进 files 域时订正：原写的跨域调用方清单里含 `cnc_program`
//! （`cnc_program/service.rs` ×3）—— 该域已随本批收编为 `files::part_file::cnc` 子模块，
//! 且已改走本 trait（其自带的 `CncProgramRepoTrait` 与整个 `cnc/repo/` 目录随批删除），
//! **不再**直接调 ZST 静态方法，故不在此列。
//!
//! ## 为什么是胖 trait 而不是按表拆
//! 与 iam 范本同形（见 `iam/repo/mod.rs` §「为什么是胖 trait」）：`&mut PgConnection` 同一
//! 作用域只能借给一个 repo 实例；service 同时持有 part_file_repo + 跨域 owner 校验时只能
//! 走单借位。trait `PartFileRepoTrait` 是单借位，service 签名
//! `<R: PartFileRepoTrait>(&self, mut repo: R, ...)` 一次收下。
//!
//! ## 为什么 trait 可以直接对 `&mut PgConnection` 实现
//! `Transaction<'_, Postgres>` 与 `PoolConnection<Postgres>` 都 `DerefMut<Target = PgConnection>`，
//! 故 `&mut *tx` / `&mut *conn` 即 `&mut PgConnection`，可直接喂给 `sql::PartFileRepo::yyy`。
//! 无需任何 `PgPartFileRepo<'a>` 转发壳（与 iam 2026-09-22 删 `PgIamRepo` 同步）。
//!
//! ## 跨域 helper（4 个新方法，从 service.rs 迁来）
//! - `part_owner_exists` / `assembly_owner_exists` / `customer_owner_exists` —— 替代原
//!   service `assert_owner_exists` 内的 inline SQL
//!   `SELECT id FROM t_part/t_assembly WHERE id=$1 AND deleted_at IS NULL`
//!   （`customer_owner_exists` 是 2026-10-16 随 `DELIVERY_TEMPLATE` 一并加的第三种
//!   owner，写法是 `SELECT EXISTS(...)`、不带 `parent_id IS NULL` 判据）
//! - `soft_delete_file` —— 替代原 service `soft_delete_file` 内的 inline SQL
//!   `UPDATE t_part_file SET deleted_at = now(), version = version + 1, ... WHERE id=$1
//!   AND version=$3 AND deleted_at IS NULL`
//! - `soft_delete_active_by_part_kind` —— 替代原 service `bind_uploaded_file` 内的
//!   inline SQL（单文件 kind 的旧活跃行 soft_delete）
//! - `insert_setup_sheet_with_paired` / `set_paired_file_id` —— 给本域的
//!   `files::part_file::cnc` 子模块 `CncProgramService::upload_cnc_pair` 用：原
//!   `cnc_program::service` 内 inline `INSERT INTO t_part_file ... SETUP_SHEET
//!   ... paired_file_id = $9` 与 `UPDATE t_part_file SET paired_file_id = $2 ...`
//!   SQL 字符串零 diff 迁入 trait impl。
//!
//! ## automock
//! `#[cfg_attr(test, mockall::automock)]` 在 trait 上声明，生成 `MockPartFileRepoTrait`
//! 供 service 单测注入。part_file 域当前无内联 mod tests（service 全部走
//! `tests/files/part_file.rs` + `tests/files/cnc.rs` 集成测试守护），
//! 故未建 `part_file/service_tests/` 目录——按 conventions.md §4.1 含 IO 不强求 100%。
//!
//! ## 错误类型
//! repo trait 方法 → `sqlx::Error`（与 sql.rs 签名 1:1，`create_part_file` 检
//! SQLSTATE 23505 → 21108 BIZ_PART_FILE_DUPLICATE 在 service 层做）。

use async_trait::async_trait;
use sqlx::PgConnection;

use super::model::TPartFile;

pub mod sql;

// 重导出 sql.rs 中的 ZST struct / 表行类型 / NewPartFile，让上层继续用
// `super::repo::{PartFileRepo, NewPartFile, hash_bytes}` 这种路径不破。
pub use sql::{NewPartFile, PartFileRepo};

/// SHA-256 hex 编码（小写 64 字符）。
///
/// service 层在上传完成后用其计算 `content_sha256`，写入 `t_part_file`；
/// `uk_t_part_file_part_kind_sha` 唯一索引据此去重。
pub fn hash_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// part_file 域数据访问 trait（**13** 方法 = sql.rs 6 + 跨域 owner 校验 3 + 软删 2
/// + cnc_pair 2；⚠️ 2026-10-16 新增 `customer_owner_exists`）。
///
/// 单 trait 而非每表一个：`&mut PgConnection` 同一作用域只能借给一个 repo 实例，
/// 拆分会让 service 无法同时持有两个 repo（2026-09-22 重构定案；与 iam 同形）。
///
/// 方法签名 = `sql.rs` 固有静态方法去 executor 形参 + 跨域 helper（trait impl 一行委托
/// 到 `sql::PartFileRepo::yyy` 或 inline SQL）。`<'a>` 显式生命周期是 mockall 0.15
/// automock 在 `async_trait` 上下文的硬性要求。
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait PartFileRepoTrait: Send {
    // ── sql.rs 6 方法 trait 化 ──
    async fn create_part_file<'a>(&mut self, nf: NewPartFile<'a>) -> Result<i64, sqlx::Error>;
    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartFile>, sqlx::Error>;
    async fn get_by_part_kind<'a>(
        &mut self,
        owner_id: i64,
        kind: &'a str,
    ) -> Result<Option<TPartFile>, sqlx::Error>;
    async fn get_by_owner_kind_sha<'a>(
        &mut self,
        owner_id: i64,
        kind: &'a str,
        sha: &'a str,
    ) -> Result<Option<TPartFile>, sqlx::Error>;
    /// `exclude_kind` 是 2026-10-16 review 第 1 轮 MAJOR-3 新增的形参：把某个 kind
    /// 从结果里**排除**（与 `kind` 的「只取某个 kind」正交，两者同时生效）。
    /// ⚠️ 它的存在理由是 `kind` 形参**不能**表达这件事：调用方若只想「排除模板
    /// 行」就必须自己知道别的 kind 全集，而新增 kind 时那个全集必然过期。
    /// 本域通用列表端点传 `Some("DELIVERY_TEMPLATE")`；`print_template` 域自己
    /// 列举模板时传 `None`（它要的正是这一种 kind）。
    async fn list_with_filters<'a>(
        &mut self,
        owner_kind: &'a str,
        owner_id: Option<i64>,
        kind: Option<&'a str>,
        exclude_kind: Option<&'a str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<TPartFile>, i64), sqlx::Error>;
    async fn list_by_owner<'a>(
        &mut self,
        owner_kind: &'a str,
        owner_id: i64,
    ) -> Result<Vec<TPartFile>, sqlx::Error>;

    // ── 跨域 owner 校验（3）── 从原 service `assert_owner_exists` 迁来
    /// `SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL`。
    async fn part_owner_exists(&mut self, part_id: i64) -> Result<bool, sqlx::Error>;
    /// `SELECT id FROM t_assembly WHERE id = $1 AND deleted_at IS NULL`。
    async fn assembly_owner_exists(&mut self, assembly_id: i64) -> Result<bool, sqlx::Error>;
    /// `SELECT EXISTS(SELECT 1 FROM t_customer WHERE id = $1 AND deleted_at IS NULL)`
    /// —— 2026-10-16 新增，`owner_kind='CUSTOMER'` 的存在性闸门
    /// （送货单打印模板挂在一级客户上，由 `print_template` 域写入）。
    ///
    /// ⚠️ **只判「存在且未软删」，不判「是不是一级客户」**：`parent_id IS NULL`
    /// 这一条是 `print_template` 域自己的**唯一强制点**（绑二级客户 → 20104），
    /// 刻意不放进本文件 —— 本文件是 files 域的 owner 存在性闸门，不该知道
    /// 「客户分两级」这个别的域的业务概念。
    ///
    /// ⚠️ **本文件读 `t_customer` 是 `files` 域唯一一处跨域表引用**，属本仓既定
    /// 的「只读跨域聚合」pattern（同 `dashboard` / `statistics` / `prod::inspection`）：
    /// SQL 写在本域文件里、**不 import** 任何他域 service / repo 符号。
    async fn customer_owner_exists(&mut self, customer_id: i64) -> Result<bool, sqlx::Error>;

    // ── 软删 helper（2）── 从原 service `soft_delete_file` / `bind_uploaded_file` 迁来
    /// 单条软删（带版本号 OCC）：原 `soft_delete_file` 的 UPDATE 语句。
    /// 返回 `rows_affected`：0 ⇒ version 冲突。
    async fn soft_delete_file(
        &mut self,
        file_id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>;
    /// 按 owner + kind 软删旧活跃行：原 `bind_uploaded_file` 的 UPDATE 语句。
    /// `uk_t_part_file_single` 部分唯一约束的支撑方法。
    async fn soft_delete_active_by_part_kind<'a>(
        &mut self,
        part_id: i64,
        updated_by: i64,
        kind: &'a str,
    ) -> Result<u64, sqlx::Error>;

    // ── cnc_pair 写入 helper（2）── 从 `files::part_file::cnc::service` 迁来
    //    （SQL 字符串零 diff）
    /// `INSERT INTO t_part_file ... SETUP_SHEET ... paired_file_id = $9, ...`。
    /// `CncProgramService::upload_cnc_pair` 写 SETUP_SHEET 行专用。
    #[allow(clippy::too_many_arguments)]
    async fn insert_setup_sheet_with_paired<'a>(
        &mut self,
        id: i64,
        part_id: i64,
        file_type: &'a str,
        object_key: &'a str,
        original_filename: &'a str,
        file_size: i64,
        content_type: &'a str,
        content_sha256: Option<&'a str>,
        paired_file_id: i64,
        created_by: i64,
    ) -> Result<(), sqlx::Error>;
    /// `UPDATE t_part_file SET paired_file_id = $2, updated_at = now(), updated_by = $3
    /// WHERE id = $1 AND deleted_at IS NULL` —— `CncProgramService::upload_cnc_pair`
    /// 互写 paired_file_id 专用。
    async fn set_paired_file_id(
        &mut self,
        g_code_id: i64,
        setup_sheet_id: i64,
        updated_by: i64,
    ) -> Result<(), sqlx::Error>;
}

/// 把 `PartFileRepoTrait` 直接对 `&mut PgConnection` 实现——handler/service 借
/// `&mut *tx` 或 `&mut *conn` 即可调用 `sql::PartFileRepo::yyy`，零转发壳（与 iam
/// 2026-09-22 同步）。
///
/// trait 方法收 `&mut self`，impl 在 `&mut PgConnection` 上时 `self: &mut &mut PgConnection`，
/// 两次 deref 才得到 `PgConnection`：`*self: &mut PgConnection`，`**self: PgConnection`，
/// 故喂给 `sql::PartFileRepo::yyy` 须写 `&mut **self`（reborrow，避免 move 引用本身）。
#[async_trait]
impl PartFileRepoTrait for &mut PgConnection {
    // ── sql.rs 6 方法一行委托 ──
    async fn create_part_file<'b>(&mut self, nf: NewPartFile<'b>) -> Result<i64, sqlx::Error> {
        PartFileRepo::create_part_file(&mut **self, nf).await
    }

    async fn get_by_id(
        &mut self,
        id: i64,
        include_deleted: bool,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        PartFileRepo::get_by_id(&mut **self, id, include_deleted).await
    }

    async fn get_by_part_kind<'b>(
        &mut self,
        owner_id: i64,
        kind: &'b str,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        PartFileRepo::get_by_part_kind(&mut **self, owner_id, kind).await
    }

    async fn get_by_owner_kind_sha<'b>(
        &mut self,
        owner_id: i64,
        kind: &'b str,
        sha: &'b str,
    ) -> Result<Option<TPartFile>, sqlx::Error> {
        PartFileRepo::get_by_owner_kind_sha(&mut **self, owner_id, kind, sha).await
    }

    async fn list_with_filters<'b>(
        &mut self,
        owner_kind: &'b str,
        owner_id: Option<i64>,
        kind: Option<&'b str>,
        exclude_kind: Option<&'b str>,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<TPartFile>, i64), sqlx::Error> {
        PartFileRepo::list_with_filters(
            &mut **self,
            owner_kind,
            owner_id,
            kind,
            exclude_kind,
            limit,
            offset,
        )
        .await
    }

    async fn list_by_owner<'b>(
        &mut self,
        owner_kind: &'b str,
        owner_id: i64,
    ) -> Result<Vec<TPartFile>, sqlx::Error> {
        PartFileRepo::list_by_owner(&mut **self, owner_kind, owner_id).await
    }

    // ── 跨域 owner 校验（2）── inline SQL（迁自原 service `assert_owner_exists`）
    async fn part_owner_exists(&mut self, part_id: i64) -> Result<bool, sqlx::Error> {
        let exists: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM t_part WHERE id = $1 AND deleted_at IS NULL")
                .bind(part_id)
                .fetch_optional(&mut **self)
                .await?;
        Ok(exists.is_some())
    }

    async fn assembly_owner_exists(&mut self, assembly_id: i64) -> Result<bool, sqlx::Error> {
        let exists: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM t_assembly WHERE id = $1 AND deleted_at IS NULL")
                .bind(assembly_id)
                .fetch_optional(&mut **self)
                .await?;
        Ok(exists.is_some())
    }

    /// 2026-10-16 新增：`owner_kind='CUSTOMER'` 的存在性闸门。
    ///
    /// 写成 `SELECT EXISTS(...)` 而不是照抄上面两条的 `SELECT id … fetch_optional` ——
    /// EXISTS 返回**恒有一行**（true / false），省掉 `Option` 的存在性判断，
    /// 也让「这个 owner 至少被查过一次」在语句层就是显然的。
    ///
    /// ⚠️ 运行时 `query_scalar`（**不用 `query!` 宏**）：与本文件其余 helper 一致，
    /// 见 `repo/sql.rs` 模块 doc 的「约定」一节（本域运行时 SQL 改动频繁，
    /// 不强依赖 `.sqlx/` 离线缓存），故本条**不需要**重跑 `sqlx_prepare.sh`。
    async fn customer_owner_exists(&mut self, customer_id: i64) -> Result<bool, sqlx::Error> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM t_customer WHERE id = $1 AND deleted_at IS NULL)",
        )
        .bind(customer_id)
        .fetch_one(&mut **self)
        .await?;
        Ok(exists)
    }

    // ── 软删 helper（2）── inline SQL（迁自原 service）
    async fn soft_delete_file(
        &mut self,
        file_id: i64,
        version: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let rows_affected = sqlx::query(
            "UPDATE t_part_file \
             SET deleted_at = now(), \
                 version    = version + 1, \
                 updated_at = now(), \
                 updated_by = $2 \
             WHERE id = $1 AND version = $3 AND deleted_at IS NULL",
        )
        .bind(file_id)
        .bind(updated_by)
        .bind(version)
        .execute(&mut **self)
        .await?
        .rows_affected();
        Ok(rows_affected)
    }

    async fn soft_delete_active_by_part_kind<'b>(
        &mut self,
        part_id: i64,
        updated_by: i64,
        kind: &'b str,
    ) -> Result<u64, sqlx::Error> {
        let rows_affected = sqlx::query(
            "UPDATE t_part_file \
             SET deleted_at = now(), version = version + 1, updated_at = now(), updated_by = $2 \
             WHERE part_id = $1 AND kind = $3 AND deleted_at IS NULL",
        )
        .bind(part_id)
        .bind(updated_by)
        .bind(kind)
        .execute(&mut **self)
        .await?
        .rows_affected();
        Ok(rows_affected)
    }

    // ── cnc_pair 写入 helper（2）── inline SQL（迁自 `files::part_file::cnc::service`，
    //    零 diff）
    #[allow(clippy::too_many_arguments)]
    async fn insert_setup_sheet_with_paired<'b>(
        &mut self,
        id: i64,
        part_id: i64,
        file_type: &'b str,
        object_key: &'b str,
        original_filename: &'b str,
        file_size: i64,
        content_type: &'b str,
        content_sha256: Option<&'b str>,
        paired_file_id: i64,
        created_by: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO t_part_file \
               (id, part_id, kind, file_type, object_key, original_filename, \
                file_size, content_type, upload_status, content_sha256, \
                paired_file_id, \
                created_at, created_by, updated_at, updated_by) \
             VALUES ($1, $2, 'SETUP_SHEET', $3, $4, $5, $6, $7, 'READY', $8, \
                     $9, now(), $10, now(), $10)",
        )
        .bind(id)
        .bind(part_id)
        .bind(file_type)
        .bind(object_key)
        .bind(original_filename)
        .bind(file_size)
        .bind(content_type)
        .bind(content_sha256)
        .bind(paired_file_id)
        .bind(created_by)
        .execute(&mut **self)
        .await?;
        Ok(())
    }

    async fn set_paired_file_id(
        &mut self,
        g_code_id: i64,
        setup_sheet_id: i64,
        updated_by: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE t_part_file SET paired_file_id = $2, \
                                      updated_at = now(), updated_by = $3 \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(g_code_id)
        .bind(setup_sheet_id)
        .bind(updated_by)
        .execute(&mut **self)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_bytes_known_vector() {
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            hash_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hash_bytes_output_64_chars_lowercase_hex() {
        let s = hash_bytes(b"test");
        assert_eq!(s.len(), 64);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
