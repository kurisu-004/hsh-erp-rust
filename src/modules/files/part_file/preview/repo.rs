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
//! ## 状态迁移 UPDATE 的形状
//!
//! `mark_submitted` / `mark_ready` / `mark_failed` / `touch_poll` / `reset_to_pending`
//! 五个方法都是「当前状态 → 下一个状态」的单向迁移，**都带 `state` 谓词**
//! （`mark_*` / `touch_poll` 限定 `state = 'PENDING'`，`reset_to_pending` 限定
//! `state = 'FAILED'` —— ⚠️ **不是** `state <> 'PENDING'`，理由见该方法的 doc）。
//!
//! 这**不是**乐观锁（本表刻意没有 `version` 列，见建表注释），而是**幂等重放保护**：
//! 并发下两个请求都判了 PENDING 并都去写同一个 job，后到者必须被谓词挡住，否则会把
//! READY 覆盖回 PENDING、让已成功的预览重新排队一次（CI 按页计费 = 重复花钱）。
//!
//! ### ⚠️ 第六个方法 `claim_submit`：**条件 UPDATE 认领**，不是「先查再插」
//!
//! 并发不重复提交 CI job 有**两道闸**，**各管一段**（2026-10-13 review 第 1 轮
//! MAJOR-1 补齐）：唯一索引 `uk_t_file_preview_seed` 只闸**首次登记**；
//! 「行已存在但 `ci_job_id IS NULL`」的**重试**与**自愈**两条路径靠
//! [`PreviewRepo::claim_submit`] 的条件 UPDATE 认领。
//!
//! 为什么不复用 `mark_submitted` 做认领：认领要在**打 python 之前**占位，而
//! `mark_submitted` 的输入是 python 返回的真 job id，顺序反了。

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
    /// 此刻 `next_poll_at` 只起「万一提交失败，N 秒后允许重试」的作用。⚠️ 注意本表
    /// **零 `DELETE`**：登记冲突方是 `service` 里 `row = None` 直接返 PENDING，
    /// **它根本不读那一行**（review 第 1 轮 MINOR-9 订正：此处曾写成「对后到者
    /// 则立刻生效（它读到的那行…）」，那句描述的是一个本仓库没有的读路径）。
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
    ///
    /// ⚠️ **刻意不动 `ci_job_id`** —— 它此时要么是真 job id（3e 分支）要么已被
    /// [`PreviewRepo::release_claim`] 清空。
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
    /// 这条谓词是「READY 绝不重置 / 绝不重打 python」那条**省钱契约的唯一实现点**。
    ///
    /// ## ⚠️ 返回的 `rows_affected` **必须**被调用方判（2026-10-13 review 第 1 轮）
    ///
    /// **0 行不是错误**，而是「并发下别人已经把这行推进走了」（它可能已重置成
    /// PENDING 并提交了新 job，也可能已经 READY）。此前 `service.rs` **丢弃**了这个
    /// 返回值、无条件把内存行改成 `PENDING / ci_job_id = None` 并继续往下走 ⇒ 那个
    /// 调用方会**再提交一个 CI job**，与赢家那个并存（按页计费 = 双份钱，且先写的
    /// job id 被覆盖成孤儿）。
    ///
    /// 正确处置：0 行 ⇒ 重新 `find_by_seed` 读回真实状态，按它走，**绝不自己提交**。
    /// 见 [`crate::modules::files::part_file::preview::service::PreviewService::get_preview`]
    /// 的 3b 分支。
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

    /// **条件 UPDATE 认领**：在**打 python 之前**抢占「我来提交」这件事。
    ///
    /// 返回 `true` = 本调用抢到了，**只有此时才允许** POST `/doc_jobs`；
    /// `false` = 别人已认领（租约未过）/ 行已不是 PENDING，调用方**必须放弃提交**。
    ///
    /// ## 为什么需要它（唯一索引帮不上忙的地方）
    ///
    /// `uk_t_file_preview_seed` 只在**首次登记**时裁决「谁该建这一行」。但下面两条
    /// 路径是**行已经存在**的，它们各自都能让两个请求同时认为「该我提交」：
    ///
    /// - **路径 A（提交失败后的重试）**：行 = `PENDING / ci_job_id IS NULL /
    ///   next_poll_at = T`。T 到点后两个请求同时读到「due + NULL」；
    /// - **路径 B（FAILED 自愈）**：见 [`PreviewRepo::reset_to_pending`] 的 0 行处置。
    ///
    /// 「先查再打 python」是典型的 TOCTOU：查与打之间没有闸。标准写法是**用一条
    /// 带谓词的 UPDATE 做认领** —— 只有 `rows_affected == 1` 的那个调用方才继续。
    ///
    /// ## 谓词：NULL **或** 已过期的认领 token
    ///
    /// ```sql
    /// WHERE … state = 'PENDING'
    ///   AND (ci_job_id IS NULL
    ///        OR (ci_job_id LIKE '__claiming__:%' AND next_poll_at <= $6))
    /// ```
    ///
    /// - `ci_job_id IS NULL`：**首次**认领；
    /// - 认领 token + **租约已过期**：认领者进程在中途挂了（token 永远留着），由
    ///   下一次调用接管。**不能省** —— 否则一次进程崩溃就让这份预览永远卡在
    ///   PENDING、永远没人再提交；
    /// - 认领 token + **租约未过期**：**抢不走**。⚠️ 这一支是本方法全部价值所在：
    ///   若谓词只写 `ci_job_id LIKE '__claiming__:%'`（不判租约），第二个请求会把
    ///   第一个请求**正在提交中**的认领抢走，两个都 POST ⇒ 重复计费 —— 正是本方法
    ///   要防的那个失败模式换个形式重现。
    ///
    /// ## ⚠️⚠️「租约到期」比较**必须用 bind 的应用时钟**（`$6`），**不得**写 SQL 的
    /// `now()`
    ///
    /// 本仓的两套时钟是两个东西：`infra::clock::now_naive()` 是应用侧
    /// **Asia/Shanghai 的 naive** 时间戳，而 `next_poll_at` 列由
    /// `mark_submitted` / 本方法写入、同样来自应用时钟。⚠️ **若改用 SQL 的
    /// `now()`**，它返回的是**会话时区**的 `timestamptz`：测试库 / 生产库 /
    /// 不同部署的 `TimeZone` 设置未必一致（本地是 `Asia/Shanghai`，容器里可能是
    /// `UTC`），差 8 小时 ⇒ 租约恒判「未过期」⇒ **过期认领永远接管不了**（进程崩
    /// 了就永久卡死）。且这种偏差**不报错**，只在某个部署环境的某个时刻显形。
    ///
    /// 这与 CLAUDE.md「窗口下界取形参 `today`，SQL 里绝不写 `CURRENT_DATE`」是
    /// 同一条纪律的另一个应用面。
    ///
    /// ## ⚠️ 占位 token 被 python 返回的真 job id 覆盖的时序
    ///
    /// 认领（写 token + 租约）→ POST python → `mark_submitted`（写真 job id +
    /// 退回正常退避戳）。两步之间若进程挂掉，行停在「token 占位」态；租约到期后
    /// 由下一次调用接管重提。
    pub async fn claim_submit<'e, E>(
        executor: E,
        id: i64,
        claim_token: &str,
        lease_until: NaiveDateTime,
        now: NaiveDateTime,
        updated_by: i64,
    ) -> Result<bool, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query!(
            r#"
            UPDATE public.t_file_preview
            SET ci_job_id = $2, next_poll_at = $3, updated_at = now(), updated_by = $4
            WHERE id = $1 AND state = 'PENDING' AND deleted_at IS NULL
              AND (ci_job_id IS NULL
                   OR (ci_job_id LIKE $5 AND next_poll_at <= $6))
            "#,
            id,
            claim_token,
            lease_until,
            updated_by,
            // ⚠️ 与 service 侧 `CLAIM_TOKEN_PREFIX` 同源，禁止在此处再写一份字面量
            format!("{}%", super::service::CLAIM_TOKEN_PREFIX),
            now,
        )
        .execute(executor)
        .await?
        .rows_affected();
        Ok(rows == 1)
    }

    /// **归还认领**：`ci_job_id` 从占位 token 清回 `NULL`（提交 python 失败时调用）。
    ///
    /// ⚠️ **刻意不复用 `touch_poll`**：那个方法的另一个调用点（service 3e 的
    /// Running 分支）**必须保留真 job id**，两处对 `ci_job_id` 的语义完全相反，
    /// 合并成一个方法等于给「提交失败」路径留一个「顺手把 job id 也清了」的暗坑。
    ///
    /// 谓词带 `ci_job_id = $2`（**只**清自己写的那枚 token）：并发下若已被别人
    /// 认领 / 提交成功，本方法 0 行是**正确结果**，绝不误清别人的真 job id。
    pub async fn release_claim<'e, E>(
        executor: E,
        id: i64,
        claim_token: &str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query!(
            r#"
            UPDATE public.t_file_preview
            SET ci_job_id = NULL, updated_at = now(), updated_by = $3
            WHERE id = $1 AND state = 'PENDING' AND deleted_at IS NULL AND ci_job_id = $2
            "#,
            id,
            claim_token,
            updated_by,
        )
        .execute(executor)
        .await?
        .rows_affected();
        Ok(rows)
    }
}
