//! `files::part_file::preview` 的业务层：惰性状态机 + seed 推导 + URL 拼接。
//!
//! 本文件是 `GET /api/v2/files/objects/{file_id}/preview` 的全部业务逻辑，
//! 模块级取舍（安全 / CDN 契约 / 状态机）见 [`super::mod`] 的 doc。

use std::sync::Arc;

use axum::http::HeaderMap;
use chrono::NaiveDateTime;
use serde_json::{Value, json};
use sqlx::PgPool;
use tracing::warn;

use crate::auth::rbac::{CurrentUser, Role};
use crate::infra::clock::now_naive;
use crate::infra::py_backend::PyBackendClient;
use crate::modules::files::part_file::model::TPartFile;
use crate::modules::files::part_file::preview::dto::PreviewOut;
use crate::modules::files::part_file::preview::model::TFilePreview;
use crate::modules::files::part_file::preview::repo::{PreviewRepo, state};
use crate::shared::error::{AppError, code};

/// 认领占位 token 的前缀（打 python **之前**写进 `ci_job_id` 的那枚标记）。
///
/// ⚠️ **为什么需要一个占位 token**：`ci_job_id IS NULL` 是「尚未提交成功」的判据，
/// 而并发下两个请求都能读到 NULL。要在**打 python 之前**独占「我来提交」，就必须
/// 先把这一列写上点什么 —— 写占位 token 是最小改动（不改 schema、不加列）。
///
/// ⚠️ **前缀不可省、且必须可识别**：进程在「认领 → `mark_submitted`」之间挂掉时，
/// 行会停在「token 占位」态。下一次调用的 3d 分支要能认出「这是占位、不是真 job
/// id」并重新认领（而不是拿着 token 去 `describe` 打 python —— 那会拿到一个
/// python 侧不存在的 job id）。真 job id 由 CI 侧生成，不以本前缀开头。
pub const CLAIM_TOKEN_PREFIX: &str = "__claiming__:";

/// 认领**租约**时长（秒）：认领成功后 `next_poll_at` 被推到 `now + 本值`，
/// 在此之前**别的请求抢不走这枚 token**。
///
/// ⚠️ **必须显著大于 [`POLL_BACKOFF_SECONDS`]（2s）**：认领之后紧接着是一次
/// `POST /doc_jobs` 的**同步往返**，python 侧要落队列、要调 CI，耗时可能好几秒。
/// 若租约只有 2s，第二个请求会在第一次还在飞的时候把认领抢走 ⇒ 两个都提交 ⇒
/// **重复计费**（本方法存在的全部意义就是防这个）。
///
/// 取 **30s** 的取舍：够覆盖一次 CI 提交往返；又不至于让「认领者进程崩溃」之后
/// 这份预览长时间无人接管（租约一过，下一次调用的 3d 分支就能重提）。
///
/// ⚠️ 改这个值前先确认 `PyBackendClient::forward_ci_submit` 的超时
/// （`PythonBackendConfig::timeout_ms`，缺省 10s）仍 < 本值。
pub const CLAIM_LEASE_SECONDS: i64 = 30;

/// 轮询节流：首次登记后 / 提交后到下一次允许轮询的间隔（秒）。
///
/// 取 **2s** 的理由：CI 光栅化一页 PDF 的典型耗时是几百毫秒到数秒，2s 足以让
/// 「点了预览立刻再看」的用户在第二次请求就拿到结果；同时又不至于把 python 打成
/// 高频探针（前端轮询间隔通常 ≥1s）。
pub const POLL_BACKOFF_SECONDS: i64 = 2;

/// 轮询退避上限（秒）。
///
/// ⚠️ 当前实现只写 [`POLL_BACKOFF_SECONDS`]（**无递增退避**）：本表没有「已轮询
/// 次数」列，加递增要改 schema，而 CI 通常几秒内收敛，固定 2s 已经够。
/// 这个上限常量是给「将来真要加递增退避」预留的接缝 —— 那时退避序列写成
/// `min(POLL_BACKOFF_SECONDS * 2^n, POLL_BACKOFF_MAX_SECONDS)`。
///
/// 之所以现在就把它钉成一个常量而不是等需要时再加：让它**先在代码里存在**，
/// 将来加递增退避的人能一眼看到上限在哪，而不必自己拍一个（拍高了会拖慢收敛）。
pub const POLL_BACKOFF_MAX_SECONDS: i64 = 5;

/// `error_message` 列宽上限（varchar(500)）。
///
/// CI 失败原因可能被截断到 500 字符 —— 只回显给前端、不含栈，超长不影响排查
/// （日志侧另有 `tracing::warn!` 打全量）。
pub const ERROR_MESSAGE_MAX_CHARS: usize = 500;

/// `preview_seed` 的十六进制长度（32 位 = sha256 的前 16 字节 = 128 bit）。
///
/// 128 bit 对「同一份 PDF 被不同人反复上传」这个规模的碰撞概率已经可以忽略
/// （生日界约 2^64 份才有一半概率碰撞）。
pub const PREVIEW_SEED_HEX_LEN: usize = 32;

/// 预览任务的全部业务逻辑。字段仅 `snowflake`（发 `t_file_preview.id` 用）。
///
/// ⚠️ **本域零后台任务、零新增 cron**：状态推进完全由端点调用驱动。
pub struct PreviewService {
    snowflake: Arc<crate::infra::snowflake::SnowflakeIdGenerator>,
}

impl PreviewService {
    pub fn new(snowflake: Arc<crate::infra::snowflake::SnowflakeIdGenerator>) -> Self {
        Self { snowflake }
    }

    /// `GET /api/v2/files/objects/{file_id}/preview` 的全部逻辑。
    ///
    /// ## 分支判定条件（惰性状态机，逐条）
    ///
    /// | 条件 | 动作 | 外部依赖 |
    /// |---|---|---|
    /// | 该文件不可预览（kind 非 `DRAWING`/`ASSEMBLY_MASTER`，或 `file_type ≠ PDF`） | 21116 | 无 |
    /// | seed 未命中 | INSERT PENDING（`ON CONFLICT DO NOTHING`）→ **登记成功才**提交 CI → 返 PENDING | 提交 1 次 |
    /// | seed 未命中但 INSERT 冲突（并发，别的请求已登记） | 返 PENDING | **无** |
    /// | `PENDING` 且 `now < next_poll_at` | 返 PENDING（节流） | **无** |
    /// | `PENDING` 且到期、**无真 job id**（NULL 或认领占位 token） | **条件认领**（`claim_submit` 抢到才继续）→ 打 python → 返 PENDING | 提交 ≤1 次 |
    /// | 认领失败（并发，别人正在提交 / 行已非 PENDING） | 返 PENDING，**绝不自己打 python** | **无** |
    /// | `PENDING` 且到期且有真 job id | describe：Success → READY；**四种终局失败**（`Failed` / `Cancel` / `Pause` / `NotExist`）→ FAILED(21117)；其余 → 推进 `next_poll_at` 返 PENDING | 查询 1 次 |
    /// | `READY` | 直接返 `pages` | **无**（不打 python、不打 COS） |
    /// | `FAILED` 且重置 1 行 | 自愈重置为 PENDING 后按 PENDING 处理 | 见上 |
    /// | `FAILED` 但重置 **0 行**（并发，别人已推进） | 读回真实状态按它走，**绝不自己提交** | 见上 |
    ///
    /// ## ⚠️ 并发不重复提交 CI job 是**两道闸**，各管一段（review 第 1 轮 MAJOR-1）
    ///
    /// | 闸 | 管哪条路径 | 机制 |
    /// |---|---|---|
    /// | `uk_t_file_preview_seed` 唯一索引 | **首次登记** | `insert_pending` 的 `ON CONFLICT DO NOTHING`，`rows==1` 才是登记者 |
    /// | [`PreviewRepo::claim_submit`] 条件 UPDATE | **重试** 与 **自愈** | 「行已存在 + 无真 job id + 到期」，两个请求都能读到；认领抢到才允许打 python |
    ///
    /// 只靠唯一索引会漏掉后两条路径（详见该方法的 doc）。
    ///
    /// ## 权限（2026-10-13 PR2）
    ///
    /// 5 角色（M / C / I / CNC / **SHELF**），与 `GET /objects`（列表）、
    /// `GET /objects/{id}/content` 逐字相同。⚠️ 放开 `ShelfAccount` 是**对既有
    /// 取舍的有意反转**，理由与风险补偿见 [`super::mod`] 的「安全取舍反转」一节 ——
    /// 改这一行前务必先读那段。
    pub async fn get_preview(
        &self,
        pool: &PgPool,
        py_backend: &Arc<dyn PyBackendClient>,
        preview_prefix: &str,
        preview_base_url: &str,
        file: &TPartFile,
        current: &CurrentUser,
    ) -> Result<PreviewOut, AppError> {
        current.require_any_role(&[
            Role::Manager,
            Role::Clerk,
            Role::Inspector,
            Role::CncProgrammer,
            Role::ShelfAccount,
        ])?;

        // ── 0. 可预览性闸门（纯内存判断，先于任何 IO）──
        Self::assert_previewable(file)?;

        // ── 1. 派生内容寻址种子 ──
        let seed = Self::preview_seed(file);
        let now = now_naive();

        // 整个状态机在**一个连接**上跑：每次写完立刻读回最新状态（并发下别的请求
        // 可能已推进过状态），避免「读 A 状态 → 写 → 读 B 状态」的自造 TOCTOU。
        let mut conn = pool.acquire().await?;

        // ── 2. 取当前行；未命中则尝试登记 ──
        let mut row = match PreviewRepo::find_by_seed(&mut *conn, &seed).await? {
            Some(r) => Some(r),
            None => {
                let inserted = PreviewRepo::insert_pending(
                    &mut *conn,
                    self.snowflake.next_id(),
                    &seed,
                    &file.object_key,
                    // ⚠️ **置 `now()` 而不是「现在 + 退避」**：本次调用就是登记者，
                    // 它必须**紧接着**就提交 job（否则首次调用只会登记不提交，前端
                    // 要等两秒后的第二次调用才看见 job 被提交）。
                    // `now()` 同时也让「登记成功但进程在提交前挂了」能被下一次调用
                    // 的 3d 分支补上（自愈）。
                    now,
                    current.id,
                )
                .await?;
                if inserted {
                    // 本调用是登记者 → 读回自己刚插的那行，交给下面的统一状态机处理
                    PreviewRepo::find_by_seed(&mut *conn, &seed).await?
                } else {
                    // 撞唯一索引 = 并发，别的请求正在提交 → 直接返 PENDING，
                    // **绝不自己再提交一次**（重复提交 CI = 重复计费）
                    None
                }
            }
        };

        // ── 3. 统一状态机 ──
        let Some(row) = row.as_mut() else {
            // 并发分支：命中别人刚登记的 PENDING 且尚未提交，本调用不做任何推进。
            // 返 PENDING 让前端按 retry_after_ms 稍后再来（那时轮询条件已到期）。
            return Ok(PreviewOut::pending(Self::backoff_ms()));
        };

        // 3a. READY → **立即返回，零 IO**。
        //     ⚠️ 必须排在所有推进逻辑之前：`mark_ready` 会把 `next_poll_at` 清成
        //     NULL，而 `poll_due` 把 NULL 当作「到期」（PENDING 行的 fail-open）。
        //     若把 READY 交给下面的 PENDING 流程，一个本该零 IO 的请求会去打一次
        //     python describe —— 既慢、又可能把并发窗口里的状态写乱。
        if row.state == state::READY {
            return Ok(Self::ready_out(row, preview_base_url, preview_prefix));
        }

        // 3b. FAILED → 自愈重置为 PENDING 后重走下面的推进流程。
        //     ⚠️ **只判 FAILED、不碰 READY**（READY 已在 3a 返回）。
        //
        //     ⚠️⚠️ `reset_to_pending` 的**返回值必须判**（2026-10-13 review 第 1 轮
        //     MAJOR-1）：0 行不是错误，而是「并发下别人已把这行推进走了」。此前本处
        //     **丢弃**了行数、无条件把内存行改成 `PENDING / ci_job_id = None`，
        //     然后继续走 3d → **再提交一个 CI job**。CI 按页计费 ⇒ 两个并存 job、
        //     先写的那个变孤儿、钱两份都花了。症状与 B-1 同款迷惑：调用方拿到
        //     PENDING，看起来一切正常，只是账单翻倍。
        if row.state == state::FAILED {
            let reset = PreviewRepo::reset_to_pending(&mut *conn, row.id, current.id).await?;
            if reset == 0 {
                // 认输：按**别人推进后的真实状态**走，绝不自己提交。
                // 重新读回（可能已是 PENDING + 别人的新 job id，也可能是 READY）。
                let Some(fresh) = PreviewRepo::find_by_seed(&mut *conn, &seed).await? else {
                    // 行被软删了（本仓 preview 侧零 DELETE，理论上不会发生）：
                    // 退化成「不推进」，让前端下轮再来。
                    return Ok(Self::pending_out(row, &now));
                };
                *row = fresh;
                // 重新过 3a：可能已被别人推到 READY，那就该零 IO 直接返。
                if row.state == state::READY {
                    return Ok(Self::ready_out(row, preview_base_url, preview_prefix));
                }
                // 仍是 FAILED（别人又把它打失败了）⇒ 本调用不参与自愈，返 PENDING。
                if row.state == state::FAILED {
                    return Ok(Self::pending_out(row, &now));
                }
                // 否则是 PENDING ⇒ 继续走下面的 3c / 3d（3d 有条件认领，不会重复提交）
            } else {
                row.state = state::PENDING.to_string();
                row.ci_job_id = None;
                row.page_count = 0;
                row.error_message = None;
            }
        }

        // 3c. PENDING & 未到期 → 节流返回（不打 python）
        if !Self::poll_due(row, &now) {
            return Ok(Self::pending_out(row, &now));
        }

        // 3d. 尚无真 job id → **条件认领**后补提交
        //     （首次登记的正常延续 / 上次提交失败后的重试 / 自愈后的重新提交）
        //
        //     ⚠️ **认领先于打 python**（review 第 1 轮 MAJOR-1）：唯一索引只闸首次
        //     登记，这两条路径是「行已存在」，两个请求会同时读到「due + 无 job id」。
        //     「先查再打 python」是 TOCTOU ⇒ 两个 CI job（按页计费）。标准写法是
        //     用一条带谓词的 UPDATE 认领，只有抢到的那个才允许打 python。
        //
        //     ⚠️ 判据是「**没有真 job id**」而不是「`ci_job_id IS NULL`」：认领期间
        //     该列被写成 [`CLAIM_TOKEN_PREFIX`] 占位 token，必须一并认作「尚未提交」
        //     （进程在认领与 mark_submitted 之间挂掉时，下一次调用靠这条恢复）。
        let needs_submit = match row.ci_job_id.as_deref() {
            None => true,
            Some(id) => id.starts_with(CLAIM_TOKEN_PREFIX),
        };
        if needs_submit {
            let claim_token = format!("{CLAIM_TOKEN_PREFIX}{}", self.snowflake.next_id());
            // 租约**不是**节流戳：它只用来在「认领 → POST → mark_submitted」这段
            // 窗口内挡住别的请求（见 CLAIM_LEASE_SECONDS 的 doc）。
            let lease_until = now + chrono::Duration::seconds(CLAIM_LEASE_SECONDS);
            // ⚠️ `now` 一并传入：租约到期比较用**应用时钟**，不用 SQL 的 `now()`
            //    （DB 会话时区与应用时钟是两个东西，理由见 repo 的 doc）
            let claimed = PreviewRepo::claim_submit(
                &mut *conn,
                row.id,
                &claim_token,
                lease_until,
                now,
                current.id,
            )
            .await?;
            if !claimed {
                // 认领失败 = 别人正在提交（租约未过）/ 行已不是 PENDING
                // ⇒ **绝不自己打 python**。返 PENDING 让前端按 retry_after_ms 再来。
                return Ok(Self::pending_out(row, &now));
            }
            let next_poll = now + Self::backoff();
            match Self::submit(py_backend, preview_prefix, &seed, &file.object_key, current).await {
                Ok(job_id) => {
                    PreviewRepo::mark_submitted(&mut *conn, row.id, &job_id, next_poll, current.id)
                        .await?;
                    // ⚠️ **两处都要更新**：`pending_out` 读的是内存里这一行的
                    // `next_poll_at`，只改 `ci_job_id` 会让它仍停在「登记时的
                    // `now()`」⇒ 算出 `retry_after_ms = 0`，前端立刻重调，
                    // 退避形同虚设（变成忙轮询）。且此时传 `&now`（不是 `&next_poll`）
                    // 才是「距现在还有多久」的正确语义。
                    row.ci_job_id = Some(job_id);
                    row.next_poll_at = Some(next_poll);
                    return Ok(Self::pending_out(row, &now));
                }
                Err(e) => {
                    // 提交失败**不**把行打成 FAILED —— 那是 CI 的终态语义，
                    // 而这里只是「这次没送出去」。行保持 PENDING / ci_job_id 仍为空，
                    // 下次调用（过了 next_poll_at）会自动重试 ⇒ 自愈。
                    warn!(
                        seed = %seed,
                        error = %e,
                        "CI 预览任务提交失败，行保持 PENDING 待重试"
                    );
                    // ⚠️ **归还认领**（`ci_job_id` 清回 NULL），不能留着占位 token：
                    // 留着虽然也能被下一轮 3d 重新认领（谓词认 token），但该列上挂一个
                    // python 侧不存在的值，会误导任何将来直接读这一列的排障代码。
                    // 用独立方法而非改 `touch_poll`：`touch_poll` 还有一个调用点
                    // （3e 的 Running 分支）**必须保留真 job id**，两处语义相反。
                    PreviewRepo::release_claim(&mut *conn, row.id, &claim_token, current.id)
                        .await?;
                    // 节流：至少等一个 backoff 再重试，避免前端对着一个必然失败的
                    // 上游连打（错误原样抛给调用方 → 21118 / HTTP 502）。
                    PreviewRepo::touch_poll(&mut *conn, row.id, now + Self::backoff(), current.id)
                        .await?;
                    return Err(e);
                }
            }
        }

        // 3e. 有真 job id 且到期 → describe
        //     ⚠️ 走到这里时 `ci_job_id` 必非 None（3d 已把「无真 job id」return 掉），
        //     故这里用 `expect` 而不是 `unwrap_or_default()` —— 后者会把「状态机
        //     被改坏」静默变成「打 python 一个空 job id 的 URL」，故障现场难查。
        //     ⚠️⚠️ **改 3d 分支结构时必须同步改这里**：3d 现在多认了一种
        //     `CLAIM_TOKEN_PREFIX` 占位值，若 3d 的放行条件被放宽而本处没跟着把
        //     占位值滤掉，就会拿一个 python 侧不存在的 job id 去 describe。
        let job_id = row
            .ci_job_id
            .as_deref()
            .expect("3e 分支必已确保 ci_job_id 非空")
            .to_string();
        let describe = Self::describe(py_backend, &job_id, current).await?;
        match describe.status {
            CiStatus::Success => {
                PreviewRepo::mark_ready(&mut *conn, row.id, describe.page_count, current.id)
                    .await?;
                row.state = state::READY.to_string();
                row.page_count = describe.page_count;
                row.error_message = None;
                row.next_poll_at = None;
                Ok(Self::ready_out(row, preview_base_url, preview_prefix))
            }
            CiStatus::Failed | CiStatus::Canceled | CiStatus::Paused | CiStatus::NotExist => {
                // ⚠️ **四个变体逐个列出、刻意不写 `_`**：编译器会在本函数新增
                // `CiStatus` 变体时把这里变红，防止新词又悄悄落进兜底变无限轮询。
                //
                // 四者统一落 `FAILED` + 21117（含 `NotExist`），**不静默重提 job**
                // —— 决策理由与「为什么不会变成无限提交 job」的论证见
                // [`super::mod`] 的「CI 终局失败的落点」一节。
                let mut msg = describe.status.failure_reason().to_string();
                if let Some(reason) = describe.message.as_deref().filter(|m| !m.trim().is_empty()) {
                    msg.push('：');
                    msg.push_str(reason.trim());
                }
                let msg = Self::truncate_error(Some(&msg));
                warn!(
                    seed = %seed,
                    job_id = %job_id,
                    ci_state = %describe.status.as_str(),
                    "CI 预览任务落入终局失败态，行打 FAILED（下次调用自愈重提）"
                );
                PreviewRepo::mark_failed(&mut *conn, row.id, &msg, current.id).await?;
                Err(AppError::biz(
                    code::BIZ_PART_FILE_PREVIEW_FAILED,
                    format!("CI 预览渲染失败: {msg}"),
                ))
            }
            CiStatus::Running => {
                // 退避推进节流戳，返 PENDING（前端按 retry_after_ms 再来）
                let next_poll = now + Self::backoff();
                PreviewRepo::touch_poll(&mut *conn, row.id, next_poll, current.id).await?;
                row.next_poll_at = Some(next_poll);
                // ⚠️ **传 `&now` 而不是 `&next_poll`**（2026-10-13 review 第 1 轮修）：
                // 与 3d 分支同一纪律 —— `pending_out` 算的是 `next_poll_at - 传入时刻`
                // 即「距现在还有多久」，传 `&next_poll` 会让结果恒为 **0** ⇒ 前端
                // 拿 `retry_after_ms = 0` 立刻重调，退避形同虚设（变成**忙轮询**，
                // 每个请求都真打一次 python）。这正是「无限轮询」在**合法 Running
                // 路径**上的同款症状：越是非法终局被识别出来，这条路径越要真的节流。
                Ok(Self::pending_out(row, &now))
            }
        }
    }

    /// 可预览性闸门：**kind 是图纸类** 且 **file_type 是 PDF**。
    ///
    /// - kind：`DRAWING`（零件图纸）/ `ASSEMBLY_MASTER`（装配件整图）。
    ///   `3D_MODEL` / `G_CODE` / `SETUP_SHEET` / `CAD_2D` 一律拒 —— CI 的 PDF
    ///   光栅化管线对它们无意义（3D 预览是另一套 viewer 的事，不在本轮范围）。
    /// - file_type：取自 `policy::file_type_for_ext`，大小写已被上传路径归一
    ///   为大写（`"PDF"`）。历史脏数据（手写的小写）不额外兜底 —— 上传路径是
    ///   唯一生产点，写进去的就是大写。
    ///
    /// 命中任一条 ⇒ 21116（HTTP 400）。前端据此**不弹重试**、提示换文件。
    fn assert_previewable(file: &TPartFile) -> Result<(), AppError> {
        let kind_ok = matches!(file.kind.as_str(), "DRAWING" | "ASSEMBLY_MASTER");
        let type_ok = file.file_type.eq_ignore_ascii_case("PDF");
        if !kind_ok || !type_ok {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_PREVIEW_UNSUPPORTED,
                format!(
                    "文件不支持预览（kind={} file_type={}；仅支持 DRAWING / ASSEMBLY_MASTER 的 PDF）",
                    file.kind, file.file_type
                ),
            ));
        }
        Ok(())
    }

    /// 内容寻址种子：`content_sha256` 前 32 位 hex；为 NULL（历史行）时退回
    /// `sha256(object_key)` 前 32 位 hex。
    ///
    /// ⚠️⚠️ **两条路径的产物都必须是纯哈希，绝不带文件名**。`object_key` 的形状是
    /// `{sha16}_{原文件名}`，文件名是图号（如 `A00123_rev2.pdf`）、**可枚举**；
    /// 而预览目录是 public-read 的，把图号拼进路径等于公开图号清单。退回分支只
    /// 取 `sha256(object_key)` 的**摘要**，原始 object_key（含文件名）不进路径。
    ///
    /// ⚠️ 退回分支**不是**内容寻址（同一份内容挂不同文件名会算出不同种子 ⇒ 重复
    /// 跑 CI）。这是可接受的：它只对 `content_sha256 IS NULL` 的历史行走，而
    /// 新上传路径（`upload_file_for_owner` / `bind_uploaded_file`）必写 sha256。
    /// 真要根治得回填历史行的 sha256（要重读 COS 对象算哈希），属独立立项。
    pub fn preview_seed(file: &TPartFile) -> String {
        let source = match file.content_sha256.as_deref() {
            Some(sha) if !sha.is_empty() => sha.to_string(),
            _ => crate::modules::files::part_file::repo::hash_bytes(file.object_key.as_bytes()),
        };
        source.chars().take(PREVIEW_SEED_HEX_LEN).collect()
    }

    /// 预览对象在 COS 里的**相对前缀**（不含站点根、不含页文件名）：
    /// `"{prefix}/{seed}"`。
    ///
    /// ## 唯一真源（跨仓契约的交汇点，2026-10-13 review 第 1 轮）
    ///
    /// 这个字符串被**两处**消费，缺一不可：
    ///
    /// 1. [`Self::submit`] 的请求体 `dst_prefix` —— python 侧 CI 按
    ///    `{dst_prefix}/p{Page}.jpg` **写**对象；
    /// 2. [`Self::preview_urls`] —— 前端按 `{base}/{prefix}/{seed}/p{n}.jpg`
    ///    **读**对象。
    ///
    /// 两处各拼一遍必然漂移（拼错一个字符的症状是「CI 成功、URL 404」，因为两边
    /// 都在成功路径上）。故只此一处拼装。
    ///
    /// ## ⚠️ `prefix` 的 `trim_matches('/')` 是必需的，不是洁癖
    ///
    /// `config.rs` 的缺省值是 `previews`（**无**尾斜杠），而同结构体的
    /// `upload_prefix` / `tmp_prefix` 缺省都**带**尾斜杠 —— 三者形态不一致，
    /// 运维照着 `COS_UPLOAD_PREFIX=obj/` 的习惯把 `COS_PREVIEW_PREFIX` 配成
    /// `previews/` 是极自然的事。不 trim 就产出 `previews//<seed>/`：
    /// COS 的 key 里 `//` 是**实打实的两个字符**，不会像 URL 那样被规范化 ⇒
    /// CI 写到 `previews//x/p1.jpg`、前端去取 `previews/x/p1.jpg`，两边都
    /// 「看起来对」但 404。故这里对**首尾**都 trim（首部同理：配成
    /// `/previews` 会产出 `/previews/<seed>`，python 侧的
    /// `CiPreviewJobRequest._check_object_path` 会因「不得以 / 开头」直接
    /// 抛 422）。
    pub fn preview_object_prefix(prefix: &str, seed: &str) -> String {
        format!("{}/{}", prefix.trim_matches('/'), seed)
    }

    /// 预览图 URL 列表：`{base}/{prefix}/{seed}/p{n}.jpg`，`n ∈ [1, page_count]`。
    ///
    /// ⚠️ **页码从 1 起**：CI 产出的对象名是 `p1.jpg` / `p2.jpg` / …（人类可读的
    /// 1-based），不是 `p0.jpg`。这个约定必须与 python 侧 CI 配置一致。
    ///
    /// ⚠️ 中段走 [`Self::preview_object_prefix`]（与 `submit` 的 `dst_prefix`
    /// 同一真源）而不是就地拼 `prefix` —— 否则「写了哪个前缀」与「读哪个前缀」
    /// 会各自演化，`previews/` 那种尾斜杠配置就会一边 `//` 一边 `/`。
    ///
    /// `page_count <= 0` 返回空 vec（不该发生：`mark_ready` 只在 CI 报正页数时调，
    /// 而 service 在 `Success` 分支上把 0 页当 Running 处理，见 [`CiStatus`] 解析）。
    pub fn preview_urls(base_url: &str, prefix: &str, seed: &str, page_count: i64) -> Vec<String> {
        let mid = Self::preview_object_prefix(prefix, seed);
        (1..=page_count.max(0))
            .map(|n| format!("{}/{}/p{}.jpg", base_url.trim_end_matches('/'), mid, n))
            .collect()
    }

    /// 节流间隔（[`chrono::Duration`]）。
    fn backoff() -> chrono::Duration {
        chrono::Duration::seconds(POLL_BACKOFF_SECONDS.min(POLL_BACKOFF_MAX_SECONDS))
    }

    /// 返给前端的建议重试间隔（毫秒）。
    fn backoff_ms() -> u64 {
        (POLL_BACKOFF_SECONDS.min(POLL_BACKOFF_MAX_SECONDS) * 1000) as u64
    }

    /// 轮询是否到期：`next_poll_at` 为 NULL 视为**到期**。
    ///
    /// ⚠️ **只对 PENDING 行有意义**：READY 行在到达这里之前已被 3a 提前返回
    /// （`mark_ready` 会把 `next_poll_at` 清成 NULL，若不提前返回，NULL 会被
    /// 当成「到期」而白打一次 describe）。
    ///
    /// 对 PENDING 行 fail-open 取「到期」是刻意的：节流戳缺失时宁可多打一次 python
    /// （describe 是幂等的），也不要让预览卡在 PENDING 永不推进。
    fn poll_due(row: &TFilePreview, now: &NaiveDateTime) -> bool {
        match row.next_poll_at {
            Some(at) => now >= &at,
            None => true,
        }
    }

    /// PENDING 出参：`retry_after_ms` = 距离 `next_poll_at` 的毫秒数（>=0）。
    fn pending_out(row: &TFilePreview, now: &NaiveDateTime) -> PreviewOut {
        let retry_after_ms = row
            .next_poll_at
            .map(|at| (at - *now).num_milliseconds().max(0) as u64)
            .unwrap_or_else(Self::backoff_ms);
        PreviewOut::pending(retry_after_ms)
    }

    /// READY 出参：`pages` 长度恒等于 `page_count`（同一处循环产出，不可能不等长）。
    fn ready_out(row: &TFilePreview, preview_base_url: &str, preview_prefix: &str) -> PreviewOut {
        let page_count = row.page_count.max(0) as i64;
        PreviewOut::ready(
            page_count,
            Self::preview_urls(
                preview_base_url,
                preview_prefix,
                &row.preview_seed,
                page_count,
            ),
        )
    }

    /// `error_message` 截断到列宽（按 **字符**边界，不是字节 —— 避免把多字节字符
    /// 切成非法 UTF-8 而让 PG 拒写）。
    fn truncate_error(msg: Option<&str>) -> String {
        let raw = msg.unwrap_or("CI 未给出失败原因");
        if raw.chars().count() <= ERROR_MESSAGE_MAX_CHARS {
            return raw.to_string();
        }
        let mut out: String = raw.chars().take(ERROR_MESSAGE_MAX_CHARS - 3).collect();
        out.push_str("...");
        out
    }

    /// 转发 CI 任务提交，取回 `job_id`。
    ///
    /// ## ⚠️⚠️ 请求体**只有两个键**，与 python 侧 `CiPreviewJobRequest` 逐字对齐
    ///
    /// ```python
    /// class CiPreviewJobRequest(BaseModel):
    ///     src_object_key: str
    ///     dst_prefix: str = Field(min_length=1, max_length=512)   # 必填，无默认值
    /// ```
    ///
    /// ⚠️ **2026-10-13 review 第 1 轮 BLOCKER**：本函数此前发的是
    /// `{src_object_key, preview_prefix, preview_seed}` —— 后两个键 **python 侧根本
    /// 不存在**。pydantic v2 默认 `extra="ignore"` 会**静默丢掉**它们，而必填的
    /// `dst_prefix` 缺失 ⇒ FastAPI 直接返 **422** ⇒ 本函数 `parse_envelope` 立刻
    /// 21118/502。症状极具迷惑性：行永远停在 `PENDING / ci_job_id IS NULL`，
    /// 2s 后再试、再失败 —— **既烧不出图，也不报成「CI 失败」**（因为压根没到 CI）。
    ///
    /// ⚠️ 之所以此前 19 条集成测试全绿：所有 submit mock 都写成
    /// `.returning(|_b, _h| …)`，**body 参数整个被丢弃、零断言**。跨仓请求体契约
    /// 现在由 `tests/files/preview.rs::submit_request_body_matches_python_schema`
    /// 用 `withf` 钉死（断言键集合**恰好**两个 + `dst_prefix` 的逐字形状）。
    ///
    /// ## `dst_prefix` 必须与 `preview_urls` 逐字对齐
    ///
    /// CI 按 `{dst_prefix}/p{Page}.jpg` 落对象，而本端点 READY 时返的 URL 是
    /// `{base}/{prefix}/{seed}/p{n}.jpg`。两者由同一个
    /// [`Self::preview_object_prefix`] 产出（唯一真源），所以「写到哪里」与
    /// 「前端去哪里取」**不可能分叉**。
    async fn submit(
        py_backend: &Arc<dyn PyBackendClient>,
        preview_prefix: &str,
        seed: &str,
        src_object_key: &str,
        current: &CurrentUser,
    ) -> Result<String, AppError> {
        let body = json!({
            "src_object_key": src_object_key,
            "dst_prefix": Self::preview_object_prefix(preview_prefix, seed),
        });
        let resp = py_backend
            .forward_ci_submit(body, Self::fwd_headers(current))
            .await?;
        Self::parse_envelope(&resp.body, &resp.status, "submit")?
            .get("job_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                AppError::biz(
                    code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED,
                    "CI 提交响应缺少 data.job_id",
                )
            })
    }

    /// 转发 CI 任务查询，解析成 [`CiStatus`]。
    async fn describe(
        py_backend: &Arc<dyn PyBackendClient>,
        job_id: &str,
        current: &CurrentUser,
    ) -> Result<CiDescribe, AppError> {
        let resp = py_backend
            .forward_ci_describe(job_id, Self::fwd_headers(current))
            .await?;
        let data = Self::parse_envelope(&resp.body, &resp.status, "describe")?;
        // ⚠️ **状态字段名跨仓有两种写法，且 `state` 才是真源**（2026-10-13 review
        // 第 1 轮订正：此前本行是 `status` 优先、`state` 兜底，**顺序反了**）。
        // python 侧的响应模型 `CiPreviewJobStatus` 字段就叫 **`state`**
        // （`api/v1/ci_preview.py`），其模块 docstring 给的线上样例也是
        // `{"data":{"state":"Running", ...}}`；`status` 键在 python 侧**根本不存在**。
        // 两个键都读、`state` 优先 —— 键名读错的后果是空串 → 落 `Running` 兜底 →
        // **无限轮询**，正是本轮要消灭的形态，所以宁可多读一个键。
        // ⚠️ 两侧收敛到同一个键名后删掉 `status` 那一支（登记见 [`super::mod`]
        // 已知偏差 #8）。
        let status = data
            .get("state")
            .or_else(|| data.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let page_count = data
            .get("total_page_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let message = data
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
        let status = Self::parse_ci_status(status, page_count);
        Ok(CiDescribe {
            status,
            page_count: page_count.clamp(0, i32::MAX as i64) as i32,
            message,
        })
    }

    /// **跨仓 CI 终态词表的唯一判据**：python / CI 传来的原始词 → [`CiStatus`]。
    ///
    /// `pub` 是为了能被 `tests/files/preview.rs` 直接做表驱动断言（与既有的
    /// `pub fn preview_urls` 同款取舍：纯函数、无 IO、无副作用，值得被单测钉死）。
    ///
    /// ## 词表（2026-10-13 review 第 1 轮补齐）
    ///
    /// | 原始词（大小写不敏感） | → | 归属 |
    /// |---|---|---|
    /// | `success` 且 `total_page_count > 0` | `Success` | CI 官方 |
    /// | `success` 但 `total_page_count == 0` | `Running` | 刻意，见下 |
    /// | `failed` / `failure` | `Failed` | CI 官方 |
    /// | `cancel` / `canceled` / `cancelled` | `Canceled` | CI 官方 |
    /// | `pause` / `paused` | `Paused` | CI 官方（对本系统等同终局） |
    /// | `notexist` / `not_exist` / `not_exists` / `nonexist` | `NotExist` | ⚠️ **python 合成** |
    /// | 其余一切（`submitted` / `running` / `processing` / 空串 / 未知） | `Running` | 兜底 |
    ///
    /// ## 为什么 `Success` 且 0 页仍算 `Running`（**护栏，勿改**）
    ///
    /// CI 的 Success 快照**可能先于对象落盘**：报成功的那一刻 `total_page_count`
    /// 还是 0。此时若直接判 `Success`，`mark_ready(page_count=0)` 会让端点返回
    /// 一个 **`pages` 为空的 READY** —— 前端拿到「渲染成功」却一张图都渲染不出来，
    /// 且该行已是终态、**永远不会自愈**。判 `Running` 则下一轮就会重查。
    ///
    /// ## 兜底为什么留 `Running`（而不是猜失败）
    ///
    /// 未知词时继续等是 fail-safe 的：最坏是多轮询几次（节流 2s/次），而猜成终局
    /// 失败会让**正常预览直接报 502**。⚠️ 代价是这条兜底**不会自动识别跨仓新增
    /// 的词** —— python 侧改 `state` 取值时必须同步本函数（词表真源见
    /// [`super::mod`] 的「跨仓 CI 终态词表」一节）。
    pub fn parse_ci_status(raw_state: &str, page_count: i64) -> CiStatus {
        let s = raw_state.trim();
        // ⚠️ **大小写不敏感 + 先 trim**：python 发的是 CI 官方拼写（`Success` /
        // `NotExist` …），但 CI 与 python 历史上都出现过全小写 / 带空白的形态，
        // 两边对齐比猜「上游到底发哪一种」便宜。
        if s.eq_ignore_ascii_case("success") {
            return if page_count > 0 {
                CiStatus::Success
            } else {
                CiStatus::Running
            };
        }
        if s.eq_ignore_ascii_case("failed") || s.eq_ignore_ascii_case("failure") {
            return CiStatus::Failed;
        }
        // ↓ 以下三支是 2026-10-13 review 第 1 轮补的跨仓契约缺口：
        //   此前它们全落进下面的 `_ => Running` 兜底 ⇒ 每轮 `touch_poll` 再等 2s，
        //   **永远返回 PENDING** ⇒ 前端无限轮询直到自己超限。
        if s.eq_ignore_ascii_case("cancel")
            || s.eq_ignore_ascii_case("canceled")
            || s.eq_ignore_ascii_case("cancelled")
        {
            return CiStatus::Canceled;
        }
        if s.eq_ignore_ascii_case("pause") || s.eq_ignore_ascii_case("paused") {
            return CiStatus::Paused;
        }
        if s.eq_ignore_ascii_case("notexist")
            || s.eq_ignore_ascii_case("not_exist")
            || s.eq_ignore_ascii_case("not_exists")
            || s.eq_ignore_ascii_case("nonexist")
        {
            return CiStatus::NotExist;
        }
        CiStatus::Running
    }

    /// 解 python 信封 `{code, message, data}` 并取出 `data`。
    ///
    /// 非 2xx 或 `code != 0` ⇒ 21118（打 python / CI 失败）。⚠️ **不看 python 的
    /// 业务码是什么**：那套码属于送货单 / 打印语义，与预览无关，统一映射成 21118
    /// 才不会让前端拿到一个它不认识的码。
    fn parse_envelope(
        body: &axum::body::Bytes,
        status: &axum::http::StatusCode,
        what: &str,
    ) -> Result<Value, AppError> {
        if !status.is_success() {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED,
                format!("CI {what} 返回 HTTP {}", status.as_u16()),
            ));
        }
        let v: Value = serde_json::from_slice(body).map_err(|e| {
            AppError::biz(
                code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED,
                format!("CI {what} 响应不是合法 JSON: {e}"),
            )
        })?;
        let code_val = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        if code_val != 0 {
            let msg = v.get("message").and_then(Value::as_str).unwrap_or("");
            return Err(AppError::biz(
                code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED,
                format!("CI {what} 返回业务错误 {code_val}: {msg}"),
            ));
        }
        Ok(v.get("data").cloned().unwrap_or(Value::Null))
    }

    /// 转发头：注入 `X-Forwarded-User-Id`（与 `files::handler::forward_sts_tmp_keys`
    /// 同款纪律 —— python 端靠它取真实身份）。
    fn fwd_headers(current: &CurrentUser) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Ok(v) = current.id.to_string().parse() {
            h.insert("x-forwarded-user-id", v);
        }
        h
    }
}

/// CI 查询结果的**归一化**状态（本仓内部语义，**不是** CI 的原始词表）。
///
/// ⚠️ **词表的唯一真源是 [`PreviewService::parse_ci_status`]**，跨仓契约全文
/// （CI 官方六态 / python 合成态 / 大小写与别名 / 谁负责识别）见
/// [`super::mod`] 的「跨仓 CI 终态词表」一节。改任何一侧都要改另一侧。
///
/// `pub` 的唯一理由：它是 `pub fn parse_ci_status` 的返回类型，且集成测试要拿它
/// 做表驱动断言（同 `pub fn preview_urls` 的取舍）。**变体本身不构成对外 HTTP
/// 契约** —— 前端看到的只有 `PreviewOut.state`（`PENDING` / `READY`）与 502 信封。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    /// 终态成功，**且 `total_page_count > 0`**。
    Success,
    /// 终态失败（CI 官方 `Failed` / `Failure`）。
    Failed,
    /// 终态被取消（CI 官方 `Cancel` / `Canceled` / `Cancelled`）。
    Canceled,
    /// 队列暂停（CI 官方 `Pause` / `Paused`）。⚠️ CI 侧理论上可 Resume，
    /// 但**本系统两侧都没有任何一侧会去做 Resume**（零后台任务、纯惰性轮询），
    /// 故对本状态机而言它等同终局 —— 理由与残留风险见 [`super::mod`]。
    Paused,
    /// job 记录**丢失**（**`NotExist` 是 python 自己合成的值，CI 官方六态里
    /// 没有它**）。触发条件有三种：响应带 `NonExistJobIds` 节点 /
    /// `JobsDetail.Code ∈ {NotExistJobId, NoSuchJob, NotFound, …}` / CI 记录
    /// 超一个月被清理。
    NotExist,
    /// 还在跑：`Submitted` / `Running` / `Processing` / **报 0 页的 `Success`** /
    /// 空串 / 任何未知词。
    Running,
}

impl CiStatus {
    /// 落进 `t_file_preview.error_message` 与 21117 `message` 的中文原因短语。
    fn failure_reason(self) -> &'static str {
        match self {
            CiStatus::Failed => "CI 渲染失败",
            CiStatus::Canceled => "CI 任务被取消",
            CiStatus::Paused => "CI 任务被暂停（队列暂停，需在 CI 控制台人工 Resume）",
            CiStatus::NotExist => "CI 任务记录不存在（CI 侧已查不到该 job）",
            CiStatus::Success | CiStatus::Running => "非失败态",
        }
    }

    /// 日志 / 排障用的规范拼写（对齐 python 发来的原词，不是本枚举变体名）。
    fn as_str(self) -> &'static str {
        match self {
            CiStatus::Success => "Success",
            CiStatus::Failed => "Failed",
            CiStatus::Canceled => "Cancel",
            CiStatus::Paused => "Pause",
            CiStatus::NotExist => "NotExist",
            CiStatus::Running => "Running",
        }
    }
}

/// [`CiStatus`] + 页数 + 失败原因的具名三元组。
struct CiDescribe {
    status: CiStatus,
    page_count: i32,
    message: Option<String>,
}
