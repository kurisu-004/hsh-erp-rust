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
// 2026-10-13 新增：轮询节流上限（`PreviewService::backoff*` 的防御性 clamp）。
// 缺省 / env 名 / 启动校验见 `infra::config::preview_poll_backoff_config`。
use crate::infra::config::PREVIEW_POLL_BACKOFF_MAX_SECONDS;
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
/// ⚠️ **必须显著大于轮询节流间隔**（[`crate::infra::config::PREVIEW_POLL_BACKOFF_DEFAULT_SECONDS`]
/// 缺省 2s、可经 env `PREVIEW_POLL_BACKOFF_SECONDS` 配 1..=5，2026-10-13 起可配）：
/// 认领之后紧接着是一次 `POST /doc_jobs` 的**同步往返**，python 侧要落队列、要调
/// CI，耗时可能好几秒。若租约只有节流间隔那么短 —— 即便配到区间上限 5s ——
/// 第二个请求会在第一次还在飞的时候把认领抢走 ⇒ 两个都提交 ⇒ **重复计费**
/// （本方法存在的全部意义就是防这个）。30s 对 1..=5s 区间仍有 ≥6× 余量，
/// 「显著大于」这条约束在可配区间下依然成立。
///
/// 取 **30s** 的取舍：够覆盖一次 CI 提交往返；又不至于让「认领者进程崩溃」之后
/// 这份预览长时间无人接管（租约一过，下一次调用的 3d 分支就能重提）。
///
/// ⚠️ 改这个值前先确认 `PyBackendClient::forward_ci_submit` 的超时
/// （`PythonBackendConfig::timeout_ms`，缺省 10s）仍 < 本值。
///
/// ↩️ **反向指针（2026-10-13 review 第 2 轮 N-5）**：本约束在
/// `crate::infra::py_backend::PyBackendClient::forward_ci_submit` 的 doc 上有
/// 对称的一条。**任何一侧改动都必须同时看另一侧** —— 之前守卫是**单向**的
/// （只在这里写），读 `py_backend.rs` 的人看不到「这个超时还有个租约要配」。
///
/// ⚠️ 即便两者配错，**后果已被 [`PreviewRepo::mark_submitted`] 的
/// `ci_job_id = $5` 谓词封住**：最坏是「多跑一个 job」，而**不是**「静默覆盖
/// 别人的 job id」。本常量是**第一道**闸（少花钱），那条谓词是**第二道**闸
/// （不产生错数据）。
pub const CLAIM_LEASE_SECONDS: i64 = 30;

/// `error_message` 列宽上限（varchar(500)）。
///
/// CI 失败原因可能被截断到 500 字符 —— 只回显给前端、不含栈，超长不影响排查
/// （日志侧另有 `tracing::warn!` 打全量）。
///
/// ⚠️ **2026-10-14 起它也是「上游信封消息」的上限**，不只服务 `error_message` 列：
/// [`PreviewService::upstream_detail`] 摘 python 响应体的 `message`（或 body 片段）
/// 时复用同一个常量与同一个截断函数 —— **一个 500 字预算、两条使用路径**，避免
/// 出现「DB 侧截 500、日志侧不截」这种同源信息两个量纲的漂移。
pub const ERROR_MESSAGE_MAX_CHARS: usize = 500;

/// `preview_seed` 的十六进制长度（32 位 = sha256 前 16 字节 = 128 bit）。
///
/// 128 bit 对「同一份 PDF 被不同人反复上传」这个规模的碰撞概率已经可以忽略
/// （生日界约 2^64 份才有一半概率碰撞）。
///
/// ⚠️ 种子**不是**「材料哈希的前 32 位直取」——材料先带域前缀（`content:` /
/// `key:`）与配方版本（[`PREVIEW_RECIPE_VERSION`]）再整体哈希一次，见
/// [`PreviewService::preview_seed`] 的 doc。
pub const PREVIEW_SEED_HEX_LEN: usize = 32;

/// 渲染配方版本标签，参与 [`PreviewService::preview_seed`] 的哈希输入。
///
/// ## 为什么需要它
///
/// 种子是**内容寻址**的：同一份 PDF 渲出的图永远落在同一个目录，`READY` 行与
/// CDN / 浏览器缓存都以它为锚。代价是「同一份内容、换了渲染配方」（CI 镜像升级、
/// DPI / 质量调整）时**没有任何一代机制** —— 旧 `READY` 行会继续返旧图，
/// 浏览器也命中旧缓存。把配方版本掺进种子即可：改配方 ⇒ 种子变 ⇒ 新目录、
/// 新行、新 URL，旧的一代自然退场。
///
/// ## 纪律
///
/// - 当前值与 python CI 配方（ImageDpi=300 / Quality=90）配套；
/// - ⚠️ **每次改渲染配方必须递增本常量**（否则旧 READY 行与 CDN 缓存不会换代，
///   用户看到的仍是按旧配方渲的图）；
/// - ⚠️ **首上本改动、以及此后的每次递增，都会让所有已预览文件各重跑一次 CI**：
///   种子换代 ⇒ 新目录、新行、新 URL，而旧行与旧目录**不回收**（旧图仍永久公开
///   可读，见 `preview/mod.rs` 已知偏差 #5）。CI **按页计费**，这是上线 / 递增前
///   要有预期的一次性成本；
/// - 版本只作为哈希输入：**不进 URL、不出现在任何输出**（种子仍是 32 位 hex）。
pub const PREVIEW_RECIPE_VERSION: &str = "r2-dpi300q90";

/// 预览任务的全部业务逻辑。字段：`snowflake`（发 `t_file_preview.id` 用）+
/// 轮询节流间隔。
///
/// ⚠️ **本域零后台任务、零新增 cron**：状态推进完全由端点调用驱动。
pub struct PreviewService {
    snowflake: Arc<crate::infra::snowflake::SnowflakeIdGenerator>,
    /// 轮询节流间隔（秒），来自 `AppConfig::preview_poll_backoff_seconds`
    /// （env `PREVIEW_POLL_BACKOFF_SECONDS`，缺省 2、启动期已校验 1..=5）。
    ///
    /// ⚠️ `backoff*()` 里还会**再 clamp 一次**，那是防御性的：启动校验才是真闸，
    /// 但本值一旦绕过 `from_env`（测试直构 / 将来别的装配路径）配成 0 或负数，
    /// `next_poll_at` 会被推到当前或过去 ⇒ `retry_after_ms` 恒 0 ⇒ 忙轮询。
    poll_backoff_seconds: i64,
}

impl PreviewService {
    pub fn new(
        snowflake: Arc<crate::infra::snowflake::SnowflakeIdGenerator>,
        poll_backoff_seconds: i64,
    ) -> Self {
        Self {
            snowflake,
            poll_backoff_seconds,
        }
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
    ///
    /// ## `image_params`（2026-10-12 新增）
    ///
    /// 由 handler 按 `?size=` 档位选好的图片即时处理参数，只影响 READY 的
    /// `pages[]` 尾部 query，**不进任何落库 / 送 python 的数据**（`preview_seed`、
    /// `dst_prefix` 都与它无关）。
    pub async fn get_preview(
        &self,
        pool: &PgPool,
        py_backend: &Arc<dyn PyBackendClient>,
        preview_prefix: &str,
        preview_base_url: &str,
        image_params: &str,
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
                    // 要等一个节流间隔后的第二次调用才看见 job 被提交；间隔缺省 2s、
                    // 可经 env `PREVIEW_POLL_BACKOFF_SECONDS` 配 1..=5）。
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
            return Ok(PreviewOut::pending(self.backoff_ms()));
        };

        // 3a. READY → **立即返回，零 IO**。
        //     ⚠️ 必须排在所有推进逻辑之前：`mark_ready` 会把 `next_poll_at` 清成
        //     NULL，而 `poll_due` 把 NULL 当作「到期」（PENDING 行的 fail-open）。
        //     若把 READY 交给下面的 PENDING 流程，一个本该零 IO 的请求会去打一次
        //     python describe —— 既慢、又可能把并发窗口里的状态写乱。
        if row.state == state::READY {
            return Ok(Self::ready_out(
                row,
                preview_base_url,
                preview_prefix,
                image_params,
            ));
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
                    return Ok(self.pending_out(row, &now));
                };
                *row = fresh;
                // 重新过 3a：可能已被别人推到 READY，那就该零 IO 直接返。
                if row.state == state::READY {
                    return Ok(Self::ready_out(
                        row,
                        preview_base_url,
                        preview_prefix,
                        image_params,
                    ));
                }
                // 仍是 FAILED（别人又把它打失败了）⇒ 本调用不参与自愈，返 PENDING。
                if row.state == state::FAILED {
                    return Ok(self.pending_out(row, &now));
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
            return Ok(self.pending_out(row, &now));
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
                return Ok(self.pending_out(row, &now));
            }
            let next_poll = now + self.backoff();
            match Self::submit(py_backend, preview_prefix, &seed, &file.object_key, current).await {
                Ok(job_id) => {
                    // ⚠️ `claim_token` 必须一并传入：`mark_submitted` 的谓词靠它
                    //    校验「我才是认领者」（review 第 2 轮 N-5）。传错 / 传空会让
                    //    写入永远得 0 行 ⇒ job id 落不了库 ⇒ 预览永远卡在 PENDING。
                    PreviewRepo::mark_submitted(
                        &mut *conn,
                        row.id,
                        &job_id,
                        next_poll,
                        &claim_token,
                        current.id,
                    )
                    .await?;
                    // ⚠️ **两处都要更新**：`pending_out` 读的是内存里这一行的
                    // `next_poll_at`，只改 `ci_job_id` 会让它仍停在「登记时的
                    // `now()`」⇒ 算出 `retry_after_ms = 0`，前端立刻重调，
                    // 退避形同虚设（变成忙轮询）。且此时传 `&now`（不是 `&next_poll`）
                    // 才是「距现在还有多久」的正确语义。
                    row.ci_job_id = Some(job_id);
                    row.next_poll_at = Some(next_poll);
                    return Ok(self.pending_out(row, &now));
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
                    let released =
                        PreviewRepo::release_claim(&mut *conn, row.id, &claim_token, current.id)
                            .await?;
                    // ⚠️⚠️ **只在真的归还成功（1 行）时才推节流戳**
                    //（2026-10-13 review 第 2 轮 N-5 的 R2）：`release_claim` 得 0 行
                    // = 认领**已被接管** / 行已不是 PENDING。此时若仍无条件
                    // `touch_poll(now + 节流间隔)`，就会把**接管者刚写的租约戳**
                    // （now + 30s）**缩短**成 now + 节流间隔（缺省 2s，可配 1..=5）
                    // ⇒ 第三个请求一个节流间隔后即可合法接管 ⇒「租约不可抢」这条闸
                    // 被一个善意的退避动作从下面捅穿。
                    if released == 1 {
                        // 节流：至少等一个 backoff 再重试，避免前端对着一个必然失败的
                        // 上游连打（错误原样抛给调用方 → 21118 / HTTP 502）。
                        PreviewRepo::touch_poll(
                            &mut *conn,
                            row.id,
                            now + self.backoff(),
                            current.id,
                        )
                        .await?;
                    } else {
                        warn!(
                            seed = %seed,
                            "归还认领得 0 行（已被接管或行已非 PENDING），不推节流戳以免缩短接管者的租约"
                        );
                    }
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
                Ok(Self::ready_out(
                    row,
                    preview_base_url,
                    preview_prefix,
                    image_params,
                ))
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
                let next_poll = now + self.backoff();
                PreviewRepo::touch_poll(&mut *conn, row.id, next_poll, current.id).await?;
                row.next_poll_at = Some(next_poll);
                // ⚠️ **传 `&now` 而不是 `&next_poll`**（2026-10-13 review 第 1 轮修）：
                // 与 3d 分支同一纪律 —— `pending_out` 算的是 `next_poll_at - 传入时刻`
                // 即「距现在还有多久」，传 `&next_poll` 会让结果恒为 **0** ⇒ 前端
                // 拿 `retry_after_ms = 0` 立刻重调，退避形同虚设（变成**忙轮询**，
                // 每个请求都真打一次 python）。这正是「无限轮询」在**合法 Running
                // 路径**上的同款症状：越是非法终局被识别出来，这条路径越要真的节流。
                Ok(self.pending_out(row, &now))
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

    /// 内容寻址种子：材料带域前缀 + 配方版本，整体再哈希，取前
    /// [`PREVIEW_SEED_HEX_LEN`] 位 hex。
    ///
    /// ## 公式（2026-10-12 改，配方版本入 seed）
    ///
    /// ```text
    /// 材料 = content_sha256 非空 ? "content:{sha256}" : "key:{sha256(object_key)}"
    /// 种子 = hex(sha256("{材料}|recipe:{PREVIEW_RECIPE_VERSION}"))[..32]
    /// ```
    ///
    /// 两条来源都**显式带域前缀**（`content:` / `key:`）—— 不加前缀时，
    /// 「sha 恰好等于某个 object_key 的摘要」这种（理论上可构造的）输入会让两条
    /// 来源坍缩到同一材料，而它们语义不同、不该共享目录。`|recipe:` 段是
    /// [`PREVIEW_RECIPE_VERSION`] 的接入口（改配方 ⇒ 换代，理由见该常量 doc）。
    /// 先 `trim` 再判空：`content_sha256` 是历史列，可能有空白脏值。
    ///
    /// ## ⚠️ 两条路径的产物都必须是纯哈希，绝不带文件名
    ///
    /// `object_key` 的形状是 `{sha16}_{原文件名}`，文件名是图号（如
    /// `A00123_rev2.pdf`）、**可枚举**；而预览目录是 public-read 的，把图号拼进
    /// 路径等于公开图号清单。退回分支只取 `sha256(object_key)` 的**摘要**，原始
    /// object_key（含文件名）不进路径；两条来源又都被外层哈希一遍，不可反推。
    ///
    /// ⚠️ 退回分支**不是**内容寻址（同一份内容挂不同文件名会算出不同种子 ⇒ 重复
    /// 跑 CI）。这是可接受的：它只对 `content_sha256 IS NULL` 的历史行走，而
    /// 新上传路径（`upload_file_for_owner` / `bind_uploaded_file`）必写 sha256。
    /// 真要根治得回填历史行的 sha256（要重读 COS 对象算哈希），属独立立项。
    pub fn preview_seed(file: &TPartFile) -> String {
        // ⚠️ 材料与配方标签之间用 `|` 分隔：两段都是定长 / 无 `|` 的十六进制或
        // 固定串，拼接不产生歧义。
        let material = match file.content_sha256.as_deref().map(str::trim) {
            Some(sha) if !sha.is_empty() => format!("content:{sha}"),
            _ => format!(
                "key:{}",
                crate::modules::files::part_file::repo::hash_bytes(file.object_key.as_bytes())
            ),
        };
        let hashed = crate::modules::files::part_file::repo::hash_bytes(
            format!("{material}|recipe:{PREVIEW_RECIPE_VERSION}").as_bytes(),
        );
        hashed.chars().take(PREVIEW_SEED_HEX_LEN).collect()
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

    /// 预览图 URL 列表：`{base}/{prefix}/{seed}/p{n}.jpg`，`n ∈ [1, page_count]`；
    /// `image_params` 非空时给每条 URL 追加 query（2026-10-12 新增）。
    ///
    /// ⚠️ **页码从 1 起**：CI 产出的对象名是 `p1.jpg` / `p2.jpg` / …（人类可读的
    /// 1-based），不是 `p0.jpg`。这个约定必须与 python 侧 CI 配置一致。
    ///
    /// ⚠️ 中段走 [`Self::preview_object_prefix`]（与 `submit` 的 `dst_prefix`
    /// 同一真源）而不是就地拼 `prefix` —— 否则「写了哪个前缀」与「读哪个前缀」
    /// 会各自演化，`previews/` 那种尾斜杠配置就会一边 `//` 一边 `/`。
    ///
    /// ## `image_params` 的追加规则（`size` 档位的落点）
    ///
    /// - 先 `trim`：空 / 纯空白 ⇒ **与追加功能引入前逐字一致**的裸 URL；
    /// - 非空 ⇒ 追加 `?{params}`；URL 已含 `?` 时用 `&` 连接。
    ///
    /// ⚠️ **base 带 query 不受支持**：`preview_base_url` 的正常形态是不带 `?` 的
    /// 站点根（`config.rs` 的 `env_or_non_empty` 只判非空，不做语法校验）。上面的
    /// `&` 分支仅为**畸形防御** —— 避免拼出「两个 `?`」的畸形输出，**不是**
    /// 「base 带 query 可用」：那样对象路径会落进 base 的 query，浏览器请求的是
    /// 站点根而非图片。要支持这种形态得先改拼装模型。
    ///
    /// ⚠️ `image_params` 由调用方按 `PreviewSize` 档位选好（`Full` 档传空串），
    /// 本函数**不认识档位**、只认识「拼不拼」。
    ///
    /// `page_count <= 0` 返回空 vec（不该发生：`mark_ready` 只在 CI 报正页数时调，
    /// 而 service 在 `Success` 分支上把 0 页当 Running 处理，见 [`CiStatus`] 解析）。
    pub fn preview_urls(
        base_url: &str,
        prefix: &str,
        seed: &str,
        page_count: i64,
        image_params: &str,
    ) -> Vec<String> {
        let mid = Self::preview_object_prefix(prefix, seed);
        let params = image_params.trim();
        (1..=page_count.max(0))
            .map(|n| {
                let url = format!("{}/{}/p{}.jpg", base_url.trim_end_matches('/'), mid, n);
                if params.is_empty() {
                    url
                } else if url.contains('?') {
                    format!("{url}&{params}")
                } else {
                    format!("{url}?{params}")
                }
            })
            .collect()
    }

    /// 节流间隔（[`chrono::Duration`]）。
    ///
    /// ⚠️ `clamp(1, PREVIEW_POLL_BACKOFF_MAX_SECONDS)` 是**防御性**的（2026-10-13
    /// 轮询节流可配）：启动校验（`preview_poll_backoff_config`）才是真闸，这里只
    /// 兜住绕过 `from_env` 的装配路径 —— 允许 0 会让 `next_poll_at` 推到当前 /
    /// 过去，退避形同虚设（见字段 doc）。
    fn backoff(&self) -> chrono::Duration {
        chrono::Duration::seconds(
            self.poll_backoff_seconds
                .clamp(1, PREVIEW_POLL_BACKOFF_MAX_SECONDS),
        )
    }

    /// 返给前端的建议重试间隔（毫秒）。clamp 口径与 [`Self::backoff`] 相同
    /// （同一个区间常量，两处不得漂移）。
    fn backoff_ms(&self) -> u64 {
        (self
            .poll_backoff_seconds
            .clamp(1, PREVIEW_POLL_BACKOFF_MAX_SECONDS)
            * 1000) as u64
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
    ///
    /// ⚠️ 2026-10-13 轮询节流可配后由关联函数改 `&self`：`next_poll_at` 为 NULL 的
    /// 兜底分支要用 `self.backoff_ms()`（与 `backoff()` 同源）。
    fn pending_out(&self, row: &TFilePreview, now: &NaiveDateTime) -> PreviewOut {
        let retry_after_ms = row
            .next_poll_at
            .map(|at| (at - *now).num_milliseconds().max(0) as u64)
            .unwrap_or_else(|| self.backoff_ms());
        PreviewOut::pending(retry_after_ms)
    }

    /// READY 出参：`pages` 长度恒等于 `page_count`（同一处循环产出，不可能不等长）。
    ///
    /// `image_params` 是 `size` 档位选好的图片处理参数，原样透传给
    /// [`Self::preview_urls`]（空串 = `full` 档 / 未配置）。
    fn ready_out(
        row: &TFilePreview,
        preview_base_url: &str,
        preview_prefix: &str,
        image_params: &str,
    ) -> PreviewOut {
        let page_count = row.page_count.max(0) as i64;
        PreviewOut::ready(
            page_count,
            Self::preview_urls(
                preview_base_url,
                preview_prefix,
                &row.preview_seed,
                page_count,
                image_params,
            ),
        )
    }

    /// 截断到 [`ERROR_MESSAGE_MAX_CHARS`]（按 **字符**边界，不是字节 —— 避免把多字节字符
    /// 切成非法 UTF-8 而让 PG 拒写）。
    ///
    /// ⚠️ 两个调用点、两种载荷：① CI 终态失败的中文原因短语（落
    /// `t_file_preview.error_message`，列宽 varchar(500)）；② 上游响应的 `message`
    /// 或 body 片段（落 21118 的 `AppError.message`，见 [`Self::upstream_detail`]）。
    /// ② 之所以也要截：python 侧只保证自己的 body 可读、不保证长度，而本条 message
    /// 会**进 HTTP 响应信封**（`AppError::biz` ⇒ `into_response`），不能让它无界。
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
    /// 一个节流间隔（缺省 2s，可经 env `PREVIEW_POLL_BACKOFF_SECONDS` 配 1..=5）后
    /// 再试、再失败 —— **既烧不出图，也不报成「CI 失败」**（因为压根没到 CI）。
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
    /// 未知词时继续等是 fail-safe 的：最坏是多轮询几次（节流缺省 2s/次），而猜成终局
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
        //   此前它们全落进下面的 `_ => Running` 兜底 ⇒ 每轮 `touch_poll` 再等一个
        //   节流间隔（缺省 2s），**永远返回 PENDING** ⇒ 前端无限轮询直到自己超限。
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

    /// 从上游非 2xx 响应体里摘出可读原因，拼成 `": msg"` 或 `" body=…"` 后缀。
    ///
    /// ## ⚠️ 为什么需要它（2026-10-14 修的一个真实排障黑洞）
    ///
    /// python 侧把 CI 的真因放在**响应体的 `message` 字段**里（`core/ci.py` 的
    /// `_describe_ci_error` 会把 `code=… msg=… status=…` 全部拼进去）。而本模块
    /// 此前在非 2xx 分支**只写状态码、把 body 整个丢弃**，python 侧自己也不打日志
    /// ⇒ **两端同时丢弃** ⇒ 排障现场只剩一行
    /// `CI 预览任务提交失败 … error=[21118] CI submit 返回 HTTP 502`，
    /// 502 是「python 打 CI 失败」的统称，它**不区分**「桶未开通文档预览」/
    /// 「源对象不存在」/「网络不通」/「CI 参数被拒」四种完全不同的根因。
    /// 同一个函数的 `code != 0` 分支**带** `msg`、非 2xx 分支不带，本身也是不对称。
    ///
    /// ## 形态（与 python `_describe_ci_error` 同款取舍）
    ///
    /// | body | 后缀 |
    /// |---|---|
    /// | JSON 且 `message` 是非空串 | `": {message}"` |
    /// | 非 JSON / 无该键 / 空串，但 body 非空 | `" body={片段}"`（压平空白后截断） |
    /// | 空 body | `""` —— 与本函数改动前**逐字一致** |
    ///
    /// ⚠️ **第三行是刻意的不变式**：body 为空时不能凭空造一句话，那只会让日志
    /// 变长而不变好排查。有信息才加后缀。
    ///
    /// ⚠️ 片段要**压平空白**：body 可能是一整个 HTML 错误页（网关 502），逐字塞进
    /// 单行日志会把整页糊进去；压平后仍超长的由 [`Self::truncate_error`] 截断。
    fn upstream_detail(body: &axum::body::Bytes) -> String {
        // 刻意写成一条链而不是嵌套 `if let`：既避开 `clippy::collapsible_if`，
        // 也让「唯一的失败理由是 message 缺失/为空」这件事在一行里说完。
        let envelope_msg = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
            v.get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
        });
        if let Some(m) = envelope_msg {
            return format!(": {}", Self::truncate_error(Some(&m)));
        }
        let flat: String = String::from_utf8_lossy(body)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if flat.is_empty() {
            return String::new();
        }
        format!(" body={}", Self::truncate_error(Some(&flat)))
    }

    /// 解 python 信封 `{code, message, data}` 并取出 `data`。
    ///
    /// 非 2xx 或 `code != 0` ⇒ 21118（打 python / CI 失败）。⚠️ **不看 python 的
    /// 业务码是什么**：那套码属于送货单 / 打印语义，与预览无关，统一映射成 21118
    /// 才不会让前端拿到一个它不认识的码。
    ///
    /// ⚠️ **两条错误分支都要带出上游原因**（2026-10-14 修）：非 2xx 走
    /// [`Self::upstream_detail`] 摘 body 的 `message`，`code != 0` 直接带 `msg`
    /// —— 而 python 的 CI 失败是**抛 `BizError(http_status=502)`**，走的正是前者。
    /// 只修后者等于没修（这是本函数曾经的状态）。
    fn parse_envelope(
        body: &axum::body::Bytes,
        status: &axum::http::StatusCode,
        what: &str,
    ) -> Result<Value, AppError> {
        if !status.is_success() {
            return Err(AppError::biz(
                code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED,
                format!(
                    "CI {what} 返回 HTTP {}{}",
                    status.as_u16(),
                    Self::upstream_detail(body),
                ),
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

/// `parse_envelope` / `upstream_detail` 的单测。
///
/// ⚠️ **这组用例的存在理由是一个真实故障**（2026-10-14）：CI 预览提交持续 502，
/// 而 rust 侧日志只有 `error=[21118] CI submit 返回 HTTP 502` —— 非 2xx 分支把
/// body 整个丢弃，python 又不打日志，两端同时把真因扔了。故第一条用例钉的就是
/// 「python 把 CI 真因放在 body 的 `message` 里时，它必须出现在我们的错误消息里」。
///
/// ⚠️ 全部**不碰 DB、不发 HTTP**（纯函数），故可在无库/无网环境跑：
/// `SQLX_OFFLINE=true cargo test --lib preview`。
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::http::StatusCode;

    /// python 的统一信封（`R.fail` 的形状：`{code, message, data}`）。
    fn envelope(code: i64, message: &str) -> Bytes {
        Bytes::from(
            serde_json::to_vec(&json!({"code": code, "message": message, "data": null})).unwrap(),
        )
    }

    // ---------------------------------------------------------------
    // upstream_detail：body 的四种形态
    // ---------------------------------------------------------------

    /// ★ 本次故障的回归本体：python 把 CI 真因放进 `message`，我们必须摘出来。
    #[test]
    fn upstream_detail_prefers_envelope_message() {
        let body = envelope(
            20404,
            "CI 提交转码任务失败: code=AccessDenied msg=CI is not enabled for this bucket status=403",
        );
        let d = PreviewService::upstream_detail(&body);
        assert!(
            d.starts_with(": "),
            "拿到结构化 message 时后缀必须是 `: msg`，实际 {d:?}"
        );
        assert!(d.contains("AccessDenied"), "CI 错误码必须可见：{d}");
        assert!(d.contains("status=403"), "CI HTTP 状态必须可见：{d}");
    }

    /// body 不是 JSON（网关 502 的 HTML 页）⇒ 退回片段，且**必须压平空白**：
    /// 逐字塞进单行日志会把整页糊进去。
    #[test]
    fn upstream_detail_falls_back_to_flattened_body_snippet() {
        let body = Bytes::from_static(b"<html>\n  <body>\n    502 Bad Gateway\n  </body>\n</html>");
        let d = PreviewService::upstream_detail(&body);
        assert!(d.starts_with(" body="), "非 JSON 应走片段：{d:?}");
        assert!(d.contains("502 Bad Gateway"), "片段要含关键信息：{d}");
        assert!(
            !d.contains('\n'),
            "片段必须压平空白，否则单行日志被整页撑开：{d:?}"
        );
    }

    /// JSON 但 `message` 为空串 / 缺该键 ⇒ 仍退回片段（不能返回「无信息」的裸 502）。
    #[test]
    fn upstream_detail_falls_back_when_message_blank_or_absent() {
        for body in [
            envelope(20404, ""),
            envelope(20404, "   "),
            Bytes::from_static(br#"{"code":20404}"#),
        ] {
            let d = PreviewService::upstream_detail(&body);
            assert!(
                !d.is_empty(),
                "body 里还有 code 等信息，不该返回空后缀：{body:?}"
            );
            assert!(d.contains("20404"), "片段至少要带出信封的 code：{d}");
        }
    }

    /// ⚠️ **不变式**：空 body ⇒ 后缀为空串，消息与本函数改动前**逐字一致**。
    /// 不能凭空造一句话 —— 那只会让日志变长而不变好排查。
    #[test]
    fn upstream_detail_empty_body_is_no_suffix() {
        assert_eq!(PreviewService::upstream_detail(&Bytes::new()), "");
        assert_eq!(
            PreviewService::upstream_detail(&Bytes::from_static(b"   \n ")),
            ""
        );
    }

    /// 截断走**字符**边界（不是字节）：超长中文不能被切成非法 UTF-8
    /// （它会进响应信封，也进日志）。
    #[test]
    fn upstream_detail_truncates_multibyte_on_char_boundary() {
        let long = "错".repeat(ERROR_MESSAGE_MAX_CHARS + 200);
        let d = PreviewService::upstream_detail(&envelope(20404, &long));
        let payload = d.strip_prefix(": ").expect("应走 message 分支");
        assert_eq!(
            payload.chars().count(),
            ERROR_MESSAGE_MAX_CHARS,
            "截断后应恰好等于预算（{} + \"...\"）",
            ERROR_MESSAGE_MAX_CHARS
        );
        assert!(payload.ends_with("..."), "截断标记：{payload}");
        assert!(
            !payload.contains('\u{FFFD}'),
            "不得出现替换字符（说明按字节切了多字节字符）"
        );
    }

    // ---------------------------------------------------------------
    // parse_envelope：两条错误分支都不得再丢上游信息
    // ---------------------------------------------------------------

    /// 非 2xx（python 的 CI 失败就是 `BizError(http_status=502)`）⇒ 21118 + 真因。
    #[test]
    fn parse_envelope_non_2xx_keeps_upstream_reason() {
        let err = PreviewService::parse_envelope(
            &envelope(20404, "CI 提交转码任务失败: code=NoSuchKey status=404"),
            &StatusCode::BAD_GATEWAY,
            "submit",
        )
        .expect_err("非 2xx 必须报错");
        assert_eq!(err.code(), code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED);
        assert_eq!(err.http_status(), StatusCode::BAD_GATEWAY);
        let s = err.to_string();
        assert!(s.contains("HTTP 502"), "状态码仍在：{s}");
        assert!(s.contains("NoSuchKey"), "★ 上游真因必须出现在错误里：{s}");
    }

    /// ⚠️ 钉住另一条分支的现行为（它本来就带 `msg`）：修非 2xx 时**不能**把它改坏。
    #[test]
    fn parse_envelope_business_code_branch_still_carries_message() {
        let err = PreviewService::parse_envelope(
            &envelope(20404, "CI 返回业务错误原因"),
            &StatusCode::OK,
            "describe",
        )
        .expect_err("code != 0 必须报错");
        assert_eq!(err.code(), code::BIZ_PART_FILE_PREVIEW_FORWARD_FAILED);
        let s = err.to_string();
        assert!(s.contains("20404"), "{s}");
        assert!(s.contains("CI 返回业务错误原因"), "{s}");
    }

    /// 2xx + `code == 0` ⇒ 照旧只取 `data`（守住本次改动没碰成功路径）。
    #[test]
    fn parse_envelope_success_returns_data() {
        let v = PreviewService::parse_envelope(&envelope(0, "ok"), &StatusCode::OK, "submit")
            .expect("2xx + code 0 必须成功");
        assert!(v.is_null(), "data 为 null 时原样返回 null：{v}");
    }
}
