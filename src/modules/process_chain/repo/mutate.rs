//! process_chain 域写操作（INSERT/UPDATE/DELETE）
//!
//! 签名：INSERT/UPDATE 多用 `impl PgExecutor<'_>`；同一事务内连发多条 INSERT 走
//! `&mut PgConnection`（因 `PgExecutor` 不能 move 多次）。

use sqlx::{PgConnection, PgExecutor, QueryBuilder};

use crate::infra::snowflake::SnowflakeIdGenerator;
use crate::modules::process_chain::model::{NewProcessChainStep, TPartProcessChain};

use super::ProcessChainRepo;

impl ProcessChainRepo {
    /// INSERT 新链 header（仅做 INSERT；service 层负责 OCC 与 1:1 唯一性检查）。
    /// `id` 由 caller（service）预生成雪花。
    pub async fn insert_chain<'e, E: PgExecutor<'e>>(
        executor: E,
        id: i64,
        part_id: i64,
        name: &str,
        note: Option<&str>,
        created_by: i64,
    ) -> Result<TPartProcessChain, sqlx::Error> {
        sqlx::query_as!(
            TPartProcessChain,
            r#"
            INSERT INTO t_part_process_chain (
                id, part_id, name, note, version, created_by, updated_by
            ) VALUES (
                $1, $2, $3, $4, 0, $5, $5
            )
            RETURNING id, part_id, name, version, note,
                      created_at, created_by, updated_at, updated_by, deleted_at
            "#,
            id,
            part_id,
            name,
            note,
            created_by,
        )
        .fetch_one(executor)
        .await
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
    /// 函数签名收 `&mut PgConnection`（非 `impl PgExecutor<'_>`），因为批量
    /// INSERT 通常与 `soft_delete_all_steps_for_chain` 在同一事务内连发。
    pub async fn bulk_insert_steps(
        conn: &mut PgConnection,
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
        let r = qb.build().execute(&mut *conn).await?;
        Ok(r.rows_affected())
    }
}