//! `files::part_file::preview` 的数据访问层（`t_file_preview` 表，全域独占）
//!
//! ## 为什么用 `query!` 宏而不是运行时 `sqlx::query`
//!
//! 与 `part_file::repo::sql`（**刻意不用** `query!` 宏，理由是「本域 SQL 改动频繁，
//! 不强依赖 `.sqlx/` 离线缓存」）相反，本文件**用** `query!` 宏：
//!
//! - `t_file_preview` 是**新建**表、生命周期与 `t_part_file` 的改动无关，SQL 不会
//!   随旧文件域代码漂移；
//! - 本表的读写形状高度固定（一条 SELECT + 五个状态迁移 UPDATE），编译期列名校验
//!   收益大而成本小 —— 写错列名/漏列在 `cargo check` 就红，不必等运行到那条路径。
//!
//! 代价是本文件有 `.sqlx/query-*.json` 缓存条目，改 SQL 必须重跑
//! `./scripts/sqlx_prepare.sh`（与 `CLAUDE.md`「常用命令」一节同款约定）。
//!
//! ## 状态迁移五个 UPDATE 的形状
//!
//! `mark_submitted` / `mark_ready` / `mark_failed` / `touch_poll` / `reset_to_pending`
//! 五个方法都是「当前状态 → 下一个状态」的单向迁移，**都带 `state` 谓词**
//! （`mark_*` 限定 `state = 'PENDING'`，`reset_to_pending` 限定 `state <> 'PENDING'`）。
//!
//! 这**不是**乐观锁（本表刻意没有 `version` 列，见建表注释），而是**幂等重放保护**：
//! 并发下两个请求都判了 PENDING 并都去写同一个 job，后到者必须被谓词挡住，否则会把
//! READY 覆盖回 PENDING、让已成功的预览重新排队一次（CI 按页计费 = 重复花钱）。

use chrono::NaiveDateTime;

use crate::modules::files::part_file::preview::model::TFilePreview;

/// 预览任务的三种状态（与 migration 的 `state` 列一一对应；本仓无 DB ENUM）。
///
/// ⚠️ **唯一真源**：service / dto 的状态判定一律读这里的常量，不要在别处再写一份
/// 字符串字面量 —— 两份字面量分叉时，「DB 里的 FAILED」会走到「未知 → 当进行中」
/// 的兜底分支，表现为预览永远转圈而不是报错。
pub mod state {
    /// 已登记、CI 正在渲染（或尚未提交成功）。
    pub const PENDING: &str = "PENDING";
    /// CI 终态成功，`page_count > 0`，可直接返回 `pages`。
    pub const READY: &str = "READY";
    /// CI 终态失败；**不终止**，下次调用自愈重启为 `PENDING`（见 service 模块 doc）。
    pub const FAILED: &str = "FAILED";
}

/// `t_file_preview` 的全部读写（**全部静态方法**，无 trait）。
///
/// 本层刻意不引胖 trait：`t_file_preview` 只有 `preview` 子模块一个写方、一个读方。
/// 对照 `part_file::repo::PartFileRepoTrait` 存在的理由（「`&mut PgConnection` 同一
/// 作用域只能借给一个 repo 实例，service 需要同时持有跨域 helper」）在本表不成立
/// —— 预览状态机整条链只需要一个连接。
pub struct PreviewRepo;

impl PreviewRepo {
    /// 按 `preview_seed` 取预览任务（软删闸门）。
    ///
    /// 未命中返回 `None` —— 调用方据此走「首次登记」分支。
    ///
    /// ## ⚠️⚠️ `preview_seed` 的 `char(64)` **空白填充陷阱**（读出来一定带尾随空格）
    ///
    /// 建表用 `preview_seed char(64)`（定长 64，sha256 hex 的全宽；本列只存前 32
    /// 位，取 64 是给「将来改存完整 sha256」留位）。PostgreSQL 的 `bpchar` 会在
    /// **写入时右侧填充空格到定长**，读出来恒是 64 字符 —— `char(64)` 存 32 位种子
    /// 会被填成 `'<32位hex>' + 32 个空格`。
    ///
    /// `WHERE preview_seed = $1` **不受影响**（`bpchar` 比较语义会忽略尾随空格），
    /// 所以唯一索引与查重都是对的。但**任何把这个值拼进 URL 的地方都会带上空格** ——
    /// 实测症状是 `.../previews/aaaa…aaaa␣␣␣␣/p1.jpg` 这种带空格的 URL，
    /// 前端拿到就是 404，而日志里看着「几乎对」。
    ///
    /// 故本方法**在此处统一 `trim_end()`** —— 它是本表**唯一**的读出口，收口在这里
    /// 才不会让下一个调用方忘记（新增读路径时也必须走这里，不要自己写 SQL 读
    /// `preview_seed`）。
    pub async fn find_by_seed<'e, E>(
        executor: E,
        seed: &str,
    ) -> Result<Option<TFilePreview>, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let mut row = sqlx::query_as!(
            TFilePreview,
            r#"
            SELECT id, preview_seed, src_object_key, ci_job_id, state, page_count,
                   error_message, next_poll_at
            FROM public.t_file_preview
            WHERE preview_seed = $1 AND deleted_at IS NULL
            "#,
            seed,
        )
        .fetch_optional(executor)
        .await?;
        if let Some(r) = row.as_mut() {
            r.preview_seed = r.preview_seed.trim_end().to_string();
        }
        Ok(row)
    }

    /// 首次登记一条 `PENDING` 任务，**`ON CONFLICT DO NOTHING`**。
    ///
    /// 返回 `true` = 本调用是**登记者**（可以提交 CI job）；`false` = 已被别的请求
    /// 登记过（撞 `uk_t_file_preview_seed`），调用方必须走「已有 PENDING」分支。
    ///
    /// ⚠️ **这就是并发不重复提交 CI job 的全部机制** —— 判据是唯一索引，不是应用层
    /// 的「先查再插」（那有 TOCTOU 窗口：两个请求都查到 None、都去提交 job）。
    /// CI 按页计费，重复提交是真金白银的损失，所以这一层必须交给 DB 的唯一索引裁决。
    ///
    /// ⚠️ `next_poll_at` 置调用方给的时刻（**不是 NULL**）：登记者紧接着就提交 job，
    /// 此刻 `next_poll_at` 只起「万一提交失败，N 秒后允许重试」的作用；对**后到者**
    /// 则立刻生效（它读到的那行若 `ci_job_id IS NULL`，走已有分支返回 PENDING，
    /// 不会去提交）。
    pub async fn insert_pending<'e, E>(
        executor: E,
        id: i64,
        seed: &str,
        src_object_key: &str,
        next_poll_at: NaiveDateTime,
        created_by: i64,
    ) -> Result<bool, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query!(
            r#"
            INSERT INTO public.t_file_preview
                (id, preview_seed, src_object_key, state, next_poll_at,
                 created_at, created_by, updated_at, updated_by)
            VALUES ($1, $2, $3, 'PENDING', $4, now(), $5, now(), $5)
            ON CONFLICT (preview_seed) WHERE deleted_at IS NULL DO NOTHING
            "#,
            id,
            seed,
            src_object_key,
            next_poll_at,
            created_by,
        )
        .execute(executor)
        .await?
        .rows_affected();
        Ok(rows == 1)
    }

    /// `PENDING` → 已提交：`ci_job_id` 落库 + `next_poll_at` 推进到下一轮可轮询时刻。
    ///
    /// 带 `state = 'PENDING'` 谓词（理由见模块 doc）：并发下若已被推到 READY/FAILED，
    /// 0 行是**正确结果**，调用方不据此判错。
    pub async fn mark_submitted<'e, E>(
        executor: E,
        id: i64,
        ci_job_id: &str,
        next_poll_at: NaiveDateTime,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query!(
            r#"
            UPDATE public.t_file_preview
            SET ci_job_id = $2, next_poll_at = $3, updated_at = now(), updated_by = $4
            WHERE id = $1 AND state = 'PENDING' AND deleted_at IS NULL
            "#,
            id,
            ci_job_id,
            next_poll_at,
            updated_by,
        )
        .execute(executor)
        .await?
        .rows_affected();
        Ok(rows)
    }

    /// `PENDING` → `READY`：`page_count` 落库、清 `next_poll_at`（终态不再轮询）。
    pub async fn mark_ready<'e, E>(
        executor: E,
        id: i64,
        page_count: i32,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query!(
            r#"
            UPDATE public.t_file_preview
            SET state = 'READY', page_count = $2, error_message = NULL,
                next_poll_at = NULL, updated_at = now(), updated_by = $3
            WHERE id = $1 AND state = 'PENDING' AND deleted_at IS NULL
            "#,
            id,
            page_count,
            updated_by,
        )
        .execute(executor)
        .await?
        .rows_affected();
        Ok(rows)
    }

    /// `PENDING` → `FAILED`：落 `error_message`、清 `next_poll_at`。
    ///
    /// `message` 由 service 截断到 `ERROR_MESSAGE_MAX_CHARS`（列宽 varchar(500)），
    /// 本方法不重复截断 —— 唯一的写入点就是 service。
    pub async fn mark_failed<'e, E>(
        executor: E,
        id: i64,
        message: &str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query!(
            r#"
            UPDATE public.t_file_preview
            SET state = 'FAILED', error_message = $2, next_poll_at = NULL,
                updated_at = now(), updated_by = $3
            WHERE id = $1 AND state = 'PENDING' AND deleted_at IS NULL
            "#,
            id,
            message,
            updated_by,
        )
        .execute(executor)
        .await?
        .rows_affected();
        Ok(rows)
    }

    /// `PENDING` 且仍在跑：只推进 `next_poll_at`（退避），不改任何状态。
    pub async fn touch_poll<'e, E>(
        executor: E,
        id: i64,
        next_poll_at: NaiveDateTime,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query!(
            r#"
            UPDATE public.t_file_preview
            SET next_poll_at = $2, updated_at = now(), updated_by = $3
            WHERE id = $1 AND state = 'PENDING' AND deleted_at IS NULL
            "#,
            id,
            next_poll_at,
            updated_by,
        )
        .execute(executor)
        .await?
        .rows_affected();
        Ok(rows)
    }

    /// `FAILED` → 重新登记为 `PENDING`：清 job id / 页数 / 错误、`next_poll_at`
    /// 置 `now()`（允许立刻重试）。
    ///
    /// ⚠️ **自愈路径**：坏数据（CI 曾失败）不该让这份预览**永久**卡在失败态 ——
    /// 用户再点一次预览就重新排队。
    ///
    /// ⚠️ **谓词是 `state = 'FAILED'`，刻意不是 `state <> 'PENDING'`**：后者会把
    /// `READY` 一并重置，而 `READY` 是「已收敛」的终态，重置它等于把一个本该零
    /// IO 返回的请求变成一次重复的 CI 提交（CI 按页计费 = 重复花钱）。
    pub async fn reset_to_pending<'e, E>(
        executor: E,
        id: i64,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query!(
            r#"
            UPDATE public.t_file_preview
            SET state = 'PENDING', ci_job_id = NULL, page_count = 0,
                error_message = NULL, next_poll_at = now(), updated_at = now(), updated_by = $2
            WHERE id = $1 AND state = 'FAILED' AND deleted_at IS NULL
            "#,
            id,
            updated_by,
        )
        .execute(executor)
        .await?
        .rows_affected();
        Ok(rows)
    }
}
