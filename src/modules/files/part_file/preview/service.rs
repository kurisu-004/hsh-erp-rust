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
    /// | `PENDING` 且到期、`ci_job_id` 为空 | 当作未提交补一次提交 → 返 PENDING | 提交 ≤1 次 |
    /// | `PENDING` 且到期且有 job id | describe：Success → READY；Failed → FAILED(21117)；进行中 → 推进 `next_poll_at` 返 PENDING | 查询 1 次 |
    /// | `READY` | 直接返 `pages` | **无**（不打 python、不打 COS） |
    /// | `FAILED` | 重置为 PENDING 后按 PENDING 处理（自愈） | 见上 |
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
        if row.state == state::FAILED {
            PreviewRepo::reset_to_pending(&mut *conn, row.id, current.id).await?;
            row.state = state::PENDING.to_string();
            row.ci_job_id = None;
            row.page_count = 0;
            row.error_message = None;
        }

        // 3c. PENDING & 未到期 → 节流返回（不打 python）
        if !Self::poll_due(row, &now) {
            return Ok(Self::pending_out(row, &now));
        }

        // 3d. 尚无 job id → 补提交（首次登记的正常延续，或上次提交失败后的重试）
        if row.ci_job_id.is_none() {
            match Self::submit(py_backend, preview_prefix, &seed, &file.object_key, current).await {
                Ok(job_id) => {
                    let next_poll = now + Self::backoff();
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
                    // 节流：至少等一个 backoff 再重试，避免前端对着一个必然失败的
                    // 上游连打（错误原样抛给调用方 → 21118 / HTTP 502）。
                    PreviewRepo::touch_poll(&mut *conn, row.id, now + Self::backoff(), current.id)
                        .await?;
                    return Err(e);
                }
            }
        }

        // 3e. 有 job id 且到期 → describe
        //     ⚠️ 走到这里时 `ci_job_id` 必非 None（3d 已把 None 分支 return 掉），
        //     故这里用 `expect` 而不是 `unwrap_or_default()` —— 后者会把「状态机
        //     被改坏」静默变成「打 python 一个空 job id 的 URL」，故障现场难查。
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
            CiStatus::Failed => {
                let msg = Self::truncate_error(describe.message.as_deref());
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
                Ok(Self::pending_out(row, &next_poll))
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

    /// 预览图 URL 列表：`{base}/{prefix}/{seed}/p{n}.jpg`，`n ∈ [1, page_count]`。
    ///
    /// ⚠️ **页码从 1 起**：CI 产出的对象名是 `p1.jpg` / `p2.jpg` / …（人类可读的
    /// 1-based），不是 `p0.jpg`。这个约定必须与 python 侧 CI 配置一致。
    ///
    /// `page_count <= 0` 返回空 vec（不该发生：`mark_ready` 只在 CI 报正页数时调，
    /// 而 service 在 `Success` 分支上把 0 页当 Running 处理，见 [`CiStatus`] 解析）。
    pub fn preview_urls(base_url: &str, prefix: &str, seed: &str, page_count: i64) -> Vec<String> {
        (1..=page_count.max(0))
            .map(|n| {
                format!(
                    "{}/{}/{}/p{}.jpg",
                    base_url.trim_end_matches('/'),
                    prefix,
                    seed,
                    n
                )
            })
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
    async fn submit(
        py_backend: &Arc<dyn PyBackendClient>,
        preview_prefix: &str,
        seed: &str,
        src_object_key: &str,
        current: &CurrentUser,
    ) -> Result<String, AppError> {
        let body = json!({
            "src_object_key": src_object_key,
            "preview_prefix": preview_prefix,
            "preview_seed": seed,
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
        let status = data.get("status").and_then(Value::as_str).unwrap_or("");
        let page_count = data
            .get("total_page_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let message = data
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
        let status = match status {
            // ⚠️ **大小写不敏感**：python / CI 两侧对终态用词历史上不一致
            // （`Success` / `success` 都出现过），按小写归一后再判。
            s if s.eq_ignore_ascii_case("success") => {
                // 报 0 页视为「还没真跑完」（CI 的 Success 快照可能先于对象落盘），
                // 继续按进行中处理并等下一轮，避免返回一个空 pages 的 READY。
                if page_count > 0 {
                    CiStatus::Success
                } else {
                    CiStatus::Running
                }
            }
            s if s.eq_ignore_ascii_case("failed") || s.eq_ignore_ascii_case("failure") => {
                CiStatus::Failed
            }
            // 其余一切形态（Pending / Running / Processing / 空串 / 未知值）一律
            // 当「还在跑」：未知终态时继续等是 fail-safe 的（最坏是多轮询几次），
            // 猜成 Failed 会让正常预览报 502。
            _ => CiStatus::Running,
        };
        Ok(CiDescribe {
            status,
            page_count: page_count.clamp(0, i32::MAX as i64) as i32,
            message,
        })
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

/// CI 查询结果的三态。
enum CiStatus {
    Success,
    Failed,
    Running,
}

/// [`CiStatus`] + 页数 + 失败原因的具名三元组。
struct CiDescribe {
    status: CiStatus,
    page_count: i32,
    message: Option<String>,
}
