//! process_chain 域写操作（INSERT/UPDATE/DELETE）
//!
//! 签名：INSERT/UPDATE 多用 `impl PgExecutor<'_>`；同一事务内连发多条 INSERT 走
//! `&mut PgConnection`（因 `PgExecutor` 不能 move 多次）。

use sqlx::{PgExecutor, QueryBuilder};

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::prod::process_chain::model::{NewProcessChainStep, TPartProcessChain};

use super::sql::ProcessChainRepo;

impl ProcessChainRepo {
    /// INSERT 新链 header（仅做 INSERT；service 层负责 OCC 与 1:1 唯一性检查）。
    /// `id` 由 caller（service）预生成雪花。
    ///
    /// 2026-09-16 FK 翻转（migration 026）：不再写 `part_id`；归属关系由
    /// caller 同事务追加 `link_chain_to_part` 建立。
    pub async fn insert_chain<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        name: &str,
        note: Option<&str>,
        created_by: i64,
    ) -> Result<TPartProcessChain, sqlx::Error> {
        sqlx::query_as!(
            TPartProcessChain,
            r#"
            INSERT INTO t_part_process_chain (
                id, name, note, version, created_by, updated_by
            ) VALUES (
                $1, $2, $3, 0, $4, $4
            )
            RETURNING id, name, version, note,
                      created_at, created_by, updated_at, updated_by, deleted_at
            "#,
            id,
            name,
            note,
            created_by,
        )
        .fetch_one(executor)
        .await
    }

    /// 把链绑定到 part：`t_part.process_chain_id = chain_id`（2026-09-16 FK 翻转新增）。
    ///
    /// 并发守卫：`process_chain_id IS NULL` 才允许占位 —— 返回行数：
    /// - `1`：绑定成功
    /// - `0`：part 不存在 / 已软删 / 已被并发绑定（service 映射 20104 并发冲突）
    ///
    /// 另一并发面：两个 part 同时绑同一 chain → 撞 `uq_t_part_process_chain`
    /// 部分唯一索引（23505），由 service 映射 20104。
    pub async fn link_chain_to_part<'e, E: PgExecutor<'e>>(
        executor: E,
        part_id: i64,
        chain_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"
            UPDATE t_part
            SET process_chain_id = $2,
                version    = version + 1,
                updated_at = now(),
                updated_by = $3
            WHERE id = $1 AND deleted_at IS NULL AND process_chain_id IS NULL
            "#,
            part_id,
            chain_id,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 软删链 header（2026-09-16 新增：part 软删级联用）。
    /// 返回受影响行数；0 行 = 已软删 / 不存在（幂等容忍，caller 不视为错误）。
    pub async fn soft_delete_chain<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"
            UPDATE t_part_process_chain
            SET deleted_at = now(),
                version    = version + 1,
                updated_at = now(),
                updated_by = $2
            WHERE id = $1 AND deleted_at IS NULL
            "#,
            chain_id,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 解除 part 对链的引用（2026-09-16 新增：part 软删级联用）。
    ///
    /// 故意**不带** `deleted_at IS NULL` 守卫：调用场景是 part 刚被软删，
    /// 必须清掉它的 `process_chain_id` 以让出 `uq_t_part_process_chain` 槽位。
    pub async fn unlink_part_from_chain<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"
            UPDATE t_part
            SET process_chain_id = NULL,
                version    = version + 1,
                updated_at = now(),
                updated_by = $2
            WHERE process_chain_id = $1
            "#,
            chain_id,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// OCC：把 chain version 自增 + 更新 name / note，返回受影响行数；0 行 → service 转 VERSION_CONFLICT 409。
    ///
    /// upsert 整组替换语义：保留 chain id 不变（1:1 binding），更新元数据 + 自增 version。
    /// `name` / `note` 走 COALESCE 模式：传 NULL ⇒ 不修改。
    pub async fn bump_chain_version<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
        expected_version: i32,
        name: Option<&str>,
        note: Option<Option<&str>>,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error> {
        let set_note = note.is_some();
        let new_note = note.flatten();
        let r = sqlx::query!(
            r#"
            UPDATE t_part_process_chain
            SET version = version + 1,
                name     = COALESCE($3::varchar, name),
                note     = CASE WHEN $4::bool THEN $5::text ELSE note END,
                updated_at = now(),
                updated_by = $6
            WHERE id = $1 AND version = $2 AND deleted_at IS NULL
            "#,
            chain_id,
            expected_version,
            name,
            set_note,
            new_note,
            updated_by,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 软删该 chain 的所有未软删 step（同事务内，整组替换用）。
    pub async fn soft_delete_all_steps_for_chain<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            r#"
            UPDATE t_process_chain_step
            SET deleted_at = now(),
                version    = version + 1,
                updated_at = now()
            WHERE chain_id = $1 AND deleted_at IS NULL
            "#,
            chain_id,
        )
        .execute(executor)
        .await?;
        Ok(r.rows_affected())
    }

    /// 批量 INSERT steps：单条 `INSERT ... VALUES (...), (...), (...)`，
    /// 一次往返即可写完全部（防 N+1）。
    /// 空切片短路返回 0 行。
    ///
    /// 2026-09-22 D-1 重构：签名由 `&mut PgConnection` 改为 `impl PgExecutor<'_>`，
    /// 让 trait `ProcessChainRepoTrait` 可直接对本方法收编（与 shelf `bulk_insert`
    /// 同形）；SQL 字符串零 diff，`qb.build().execute(executor)` 行为不变。
    pub async fn bulk_insert_steps<'e, E: PgExecutor<'e>>(
        executor: E,
        chain_id: i64,
        rows: &[NewProcessChainStep],
        snowflake: &SnowflakeIdGenerator,
        created_by: i64,
    ) -> Result<u64, sqlx::Error> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
            "INSERT INTO t_process_chain_step (\
                id, chain_id, sort_order, process_id, estimated_minutes, note, \
                version, created_by, updated_by) ",
        );
        qb.push_values(rows.iter(), |mut b, row| {
            let id = snowflake.next_id();
            b.push_bind(id)
                .push_bind(chain_id)
                .push_bind(row.sort_order)
                .push_bind(row.process_id)
                .push_bind(row.estimated_minutes)
                .push_bind(row.note.as_deref())
                .push_bind(0_i32)
                .push_bind(created_by)
                .push_bind(created_by);
        });
        let r = qb.build().execute(executor).await?;
        Ok(r.rows_affected())
    }
}
