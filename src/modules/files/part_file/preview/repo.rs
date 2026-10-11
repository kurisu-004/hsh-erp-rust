//! `files::part_file::preview` 的数据访问层（`t_file_preview` 表，全域独占）
//!
//! ## 为什么用 `query!` 宏而不是运行时 `sqlx::query`
//!
//! 与 `part_file::repo::sql`（**刻意不用** `query!` 宏，理由是「本域 SQL 改动频繁，
//! 不强依赖 `.sqlx/` 离线缓存」）相反，本文件**用** `query!` 宏：
//!
//! - `t_file_preview` 是**新建**表、生命周期与 `t_part_file` 的改动无关，SQL 不会
//!   随旧文件域代码漂移；
//! - 本表的读写形状高度固定（一条 SELECT + 若干条状态迁移 UPDATE），编译期列名校验
//!   收益大而成本小 —— 写错列名/漏列在 `cargo check` 就红，不必等运行到那条路径。
//!
//! 代价是本文件有 `.sqlx/query-*.json` 缓存条目，改 SQL 必须重跑
//! `./scripts/sqlx_prepare.sh`（与 `CLAUDE.md`「常用命令」一节同款约定）。
//!
//! ## 本表读写方法的**唯一真源**是 [`ALL_METHODS`]
//!
//! 模块 doc 与别处的「N 个静态方法」一律引用该常量，不要手写数字 —— 2026-10-13
//! review 第 2 轮 N-1：这个数字**连错两轮**（6 → 8 → 实际 9），因为它是纯手工维护
//! 的散文，加方法时没有编译期抓手。改为引用常量后，加/删方法而不改本表会由
//! `tests::all_methods_matches_impl` 当场红。
//!
//! ## ⚠️ 审计列 `updated_at` 用 SQL `now()`，节流列 `next_poll_at` **必须** bind
//! 应用时钟（2026-10-13 review 第 2 轮 N-7：登记既有模式，勿「顺手统一」）
//!
//! 本文件全部写方法的 `SET updated_at = now()` 走的是 **DB 会话时钟**。这是
//! **全仓既有模式**、且 `updated_at` 只做审计、**不进任何谓词** ⇒ 换成应用时钟
//! 对功能零影响，本轮也不改。
//!
//! ⚠️ 但 `next_poll_at` **恰恰相反，必须 bind `infra::clock::now_naive()`**：它进
//! `claim_submit` 的租约判据（`next_poll_at <= $6`）。DB 会话时区与应用时钟是
//! **两个不同的时钟**（本地 `Asia/Shanghai`、容器里可能 `UTC`，差 8 小时）⇒
//! 用 `now()` 会让「过期认领」恒判未过期，认领者崩溃后这份预览**永远**没人接管。
//! 事故实录：第 1 轮的第一版就踩了这个坑（`expired_claim_is_taken_over` 直接红）。
//!
//! **判据很简单：进谓词 / 参与业务比较的时间列一律 bind；纯审计列可留 `now()`。**
//! 与 `CLAUDE.md`「SQL 里绝不写 `CURRENT_DATE`」是同一条纪律。
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
//! ### ⚠️ `claim_submit`：**条件 UPDATE 认领**，不是「先查再插」
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

/// 本表全部静态方法的**名字清单**（唯一真源，见模块 doc）。
///
/// ⚠️ **必须与 `impl PreviewRepo` 里的 `pub async fn` 逐条一致** ——
/// `tests::all_methods_matches_impl` 扫 `impl` 块做双向比对，少写 / 多写 / 改名
/// 都当场红。散文里出现的方法个数一律引用 [`ALL_METHODS`].len()，不要手写。
pub const ALL_METHODS: &[&str] = &[
    "find_by_seed",
    "insert_pending",
    "mark_submitted",
    "mark_ready",
    "mark_failed",
    "touch_poll",
    "reset_to_pending",
    "claim_submit",
    "release_claim",
];

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
    /// ## 谓词带 `AND ci_job_id = $5`（**认领归属**，2026-10-13 review 第 2 轮 N-5）
    ///
    /// ⚠️ 只判 `state = 'PENDING'` 时，「租约过期 → 被接管 → 抢先 `mark_submitted`」
    /// 这条路是敞开的：原认领者 POST 飞行超过 [`super::service::CLAIM_LEASE_SECONDS`]
    /// （R1 场景）时，它的 `mark_submitted` 会**覆盖接管者刚写的 job id** ⇒ 库里留下
    /// 一个**不属于任何在飞请求**的孤儿 job，而接管者的那个 job 成了没人轮询的孤儿。
    ///
    /// `ci_job_id = $5`（传调用方**自己那枚**认领 token）把「只有认领者能写自己的
    /// 结果」这条约束**下沉到 SQL**：接管发生后，先完成者的写入得 0 行，是**正确
    /// 结果**（与「已被推到 READY/FAILED」同类），调用方不据此判错。
    ///
    /// ⚠️ 这样「绝不重复计费」就不必再依赖「超时 < 租约」这条**配置纪律** ——
    /// 即使两者配错，代价上限也只是**多跑一个 job**（已经被租约过期这条闸允许了），
    /// 而不再是「静默覆盖别人的 job id」。
    pub async fn mark_submitted<'e, E>(
        executor: E,
        id: i64,
        ci_job_id: &str,
        next_poll_at: NaiveDateTime,
        claim_token: &str,
        updated_by: i64,
    ) -> Result<u64, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let rows = sqlx::query!(
            r#"
            UPDATE public.t_file_preview
            SET ci_job_id = $2, next_poll_at = $3, updated_at = now(), updated_by = $4
            WHERE id = $1 AND state = 'PENDING' AND deleted_at IS NULL AND ci_job_id = $5
            "#,
            id,
            ci_job_id,
            next_poll_at,
            updated_by,
            claim_token,
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
    ///        OR (starts_with(ci_job_id, '__claiming__:') AND next_poll_at <= $6))
    /// ```
    ///
    /// - `ci_job_id IS NULL`：**首次**认领；
    /// - 认领 token + **租约已过期**：认领者进程在中途挂了（token 永远留着），由
    ///   下一次调用接管。**不能省** —— 否则一次进程崩溃就让这份预览永远卡在
    ///   PENDING、永远没人再提交；
    /// - 认领 token + **租约未过期**：**抢不走**。⚠️ 这一支是本方法全部价值所在：
    ///   若谓词只认前缀不判租约，第二个请求会把第一个请求**正在提交中**的认领抢走，
    ///   两个都 POST ⇒ 重复计费 —— 正是本方法要防的那个失败模式换个形式重现。
    ///
    /// ## ⚠️⚠️ 前缀判定用 `starts_with(...)`，**不是** `LIKE '__claiming__:%'`
    /// （2026-10-13 review 第 2 轮 N-6）
    ///
    /// `LIKE` 里的 `_` 是**单字符通配符**（不是字面量）⇒ `'__claiming__:%'` 实际
    /// 匹配的是「**任意两字符** + `claiming` + **任意两字符** + `:` + 任意」，
    /// 比意图宽得多。此前之所以安全，是「CI 生成的真 job id 不含 `:`」这个
    /// **巧合**，不是设计 —— 一旦 CI 换了 id 格式（比如带命名空间 `ci:job:xxx`），
    /// 一个**真 job id** 就会被误判成认领 token ⇒ 租约过期后被无谓地重提
    /// （重复计费）。
    ///
    /// `starts_with(ci_job_id, '__claiming__:')`（PG 11+；本仓 PG 18）语义与
    /// service 侧 `id.starts_with(CLAIM_TOKEN_PREFIX)` 的 Rust 判据**逐字一致**，
    /// 顺带消掉「SQL 与 Rust 两处各写一遍前缀判定」的漂移面。
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
                   OR (starts_with(ci_job_id, $5) AND next_poll_at <= $6))
            "#,
            id,
            claim_token,
            lease_until,
            updated_by,
            // ⚠️ 前缀取自 service 侧常量：SQL 与 Rust 判据**同一份字面量**。
            //    这里刻意**不**再写一份 `'__claiming__:'`（第 1 轮就是绑
            //    `format!("{}%", …)`；第 2 轮 N-6 改成 `starts_with` 后若顺手把
            //    前缀内联成字面量，就又多了一个可漂移点）。
            super::service::CLAIM_TOKEN_PREFIX,
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
    ///
    /// ## ⚠️ 调用方**必须**判这个返回值（2026-10-13 review 第 2 轮 N-5 的 R2）
    ///
    /// 0 行 = 认领已经被接管 / 行已不是 PENDING。此时若调用方**仍无条件**接着
    /// `touch_poll(now + 2s)`，就会把**接管者刚写的租约戳**（`now + 30s`）**缩短**
    /// 成 `now + 2s` ⇒ 第三个请求 2s 后即可合法接管 ⇒ 认领租约形同虚设，
    /// 「租约不可抢」这条闸被一个善意的退避动作从下面捅穿。
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

#[cfg(test)]
mod tests {
    use super::ALL_METHODS;

    /// 扫 `impl PreviewRepo` 块，把全部 `pub async fn` 的名字抠出来。
    ///
    /// 解析规则（对本文件的书写方式足够，不引构建期依赖）：从 `impl PreviewRepo`
    /// 之后开始找 `pub async fn `，取其后第一个标识符。
    ///
    /// ⚠️ **只解析 `impl` 块内部**：本模块 `mod tests` 里为了说明解析规则而在
    /// doc 里写了同样那串字面量（`include_str!` 拿到的是**整个文件**），所以必须
    /// 先切到 `impl` 块的收尾大括号再扫 —— 否则测试代码会把自己当成方法
    /// （第 1 版就是这么红的，抠出两个空串名字）。
    fn methods_in_impl_source() -> Vec<String> {
        let src = include_str!("repo.rs");
        let body = src
            .split_once("impl PreviewRepo {")
            .expect("本文件必须有 impl PreviewRepo 块")
            .1;
        let body = &body[..body.find("\n}\n").expect("impl 块必须有收尾大括号")];
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(at) = rest.find("pub async fn ") {
            rest = &rest[at + "pub async fn ".len()..];
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .expect("函数名后必须有分隔符");
            out.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        out
    }

    /// [`ALL_METHODS`] 与 `impl PreviewRepo` 里的方法**逐条**一致。
    ///
    /// 2026-10-13 review 第 2 轮 N-1：此前「本表有 N 个静态方法」是纯手工维护的
    /// 散文，**连错两轮**（6 → 8 → 实际 9）。本条把它变成有编译期抓手的断言 ——
    /// 加了 `pub async fn` 忘了登记 `ALL_METHODS`（或反之）会当场红。
    #[test]
    fn all_methods_matches_impl() {
        let actual = methods_in_impl_source();
        assert_eq!(
            actual.len(),
            ALL_METHODS.len(),
            "`impl PreviewRepo` 有 {} 个 pub async fn，ALL_METHODS 只列了 {} 个\n\
             （新加方法忘了登记 ALL_METHODS？）\nimpl: {:?}\nALL_METHODS: {:?}",
            actual.len(),
            ALL_METHODS.len(),
            actual,
            ALL_METHODS,
        );
        for (got, want) in actual.iter().zip(ALL_METHODS.iter()) {
            assert_eq!(
                got,
                want,
                "impl 与 ALL_METHODS 的第 {} 条不一致",
                ALL_METHODS.len()
            );
        }
    }
}
