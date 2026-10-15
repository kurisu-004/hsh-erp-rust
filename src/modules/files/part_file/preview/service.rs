//! `files::part_file::preview` 的业务层：惰性状态机 + seed 推导 + URL 拼接。
//!
//! 本文件是 `GET /api/v2/files/objects/{file_id}/preview` 与批量入口
//! [`PreviewService::ensure_ready_for_parts`] 的全部业务逻辑，模块级取舍
//! （安全 / CDN 契约 / 状态机）见 [`super::mod`] 的 doc。
//!
//! ## ⚠️ 状态机只有**一份**，两条驱动路径共用「推进一格」
//!
//! [`PreviewService::get_preview`]（HTTP 端点，一次请求推进一格）与
//! [`PreviewService::ensure_ready_for_parts`]（批量入口，在预算内反复推进到收敛）
//! **共用 [`PreviewService::advance_one`]**：登记、两道并发闸、自愈重置、四种终局失败、
//! 0 页护栏全部在这一处。复制第二份状态机的代价不是「代码多」，而是**省钱契约会开始
//! 漂移**：某一侧漏掉「认领先于打 python」或漏判 `reset_to_pending` 的行数，症状是
//! CI job 重复提交（按页计费）且**两个调用方都看不出对方错了**。

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use chrono::NaiveDateTime;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::pool::PoolConnection;
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

/// 后台任务（非某个登录用户发起的请求）驱动预览链路时的**审计署名 id**。
///
/// ## 为什么要有一个「无用户」的取值
///
/// `t_file_preview.created_by` / `updated_by` 是 bigint，而批量入口
/// [`PreviewService::ensure_ready_for_parts`] 的调用方是**定时任务**（企业微信日报
/// cron：库里没有任何人点了「发送」）—— 它没有登录用户可署。这两列**只做审计、
/// 不进任何谓词**，所以「署名是谁」不影响状态机的任何一个判据。
///
/// 取 `0` 而不是 `NULL`：`create_part_file` 等写路径一律传 `i64`，而
/// `find_by_seed` / `claim_submit` 那些谓词从不引用这两列 ⇒ 让调用方传
/// `Option<i64>` 只会把「这个调用点没有用户」这件事散成每个调用点各写一次判断。
///
/// ⚠️ **跨仓义务**：批量路径同样会带 `X-Forwarded-User-Id: 0` 转发给 python
/// （见 [`PreviewService::fwd_headers`]），python 侧**不得**因查不到该用户而拒绝转发 ——
/// 否则日报的图纸链接会在生产恒挂不上。登记见 [`super::mod`] 的
/// 「需要 `backend-python` 同步的位置」表。
pub const SYSTEM_ACTOR_ID: i64 = 0;

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

        // 一次 HTTP 请求 = **至多一次** python 往返：推一格就返回，由前端按
        // `retry_after_ms` 再来（批量入口才会反复推进，见 `ensure_ready_for_parts`）。
        let seed = Self::preview_seed(file);
        Self::step_to_out(
            self.advance_one(pool, py_backend, preview_prefix, &seed, file, current.id)
                .await?,
            preview_base_url,
            preview_prefix,
            image_params,
        )
    }

    /// [`Step`] → 端点出参（**只**服务 HTTP 端点那条路径）。
    ///
    /// ⚠️ `FAILED` 在这里**必须**冒泡成 `21117`（本端点三态契约里失败走 502 信封），
    /// 而批量入口对同一个 `Step` 的处置是「跳过这一份、不影响整份日报」—— 失败**该**
    /// 影响谁由调用方决定，这是两条路径唯一的处置分歧。
    fn step_to_out(
        step: Step,
        preview_base_url: &str,
        preview_prefix: &str,
        image_params: &str,
    ) -> Result<PreviewOut, AppError> {
        match step {
            Step::Ready { seed, page_count } => Ok(Self::ready_out(
                &seed,
                page_count,
                preview_base_url,
                preview_prefix,
                image_params,
            )),
            Step::Pending { retry_after_ms } => Ok(PreviewOut::pending(retry_after_ms)),
            Step::Failed { reason } => Err(AppError::biz(
                code::BIZ_PART_FILE_PREVIEW_FAILED,
                format!("CI 预览渲染失败: {reason}"),
            )),
        }
    }

    /// ★ 批量入口：`part_id` 集合 → 每个 part 的**第 1 页**预览直链。
    ///
    /// 供「一次要在几十份清单里给出可点开的图纸链接」的调用方使用（企业微信日报的
    /// 第三节）。返回值里**缺键** = 这一份不挂链接，四种成因同一后果（见下），
    /// **绝不抛错**：一份日报因为一张图没转出来就不该发不出去。
    ///
    /// ## 缺键的四种成因（同一个渲染后果：名称渲成纯文字）
    ///
    /// 1. 该 part 没有 `DRAWING` 类的 PDF 图纸；
    /// 2. 预算耗尽（`budget` 内没等到 CI 收敛）；
    /// 3. CI 终局失败（行打 `FAILED`）；
    /// 4. 打 python 失败（上游 5xx / 信封错误）。
    ///
    /// ## ⚠️ 只取**第 1 页**，不是全部页
    ///
    /// 返回值是「一行一个 markdown 链接」的素材：日报每行只挂一个 URL，而企业微信
    /// markdown 链接渲染不了多图。多页图纸（整套装配件图）只给首页，是形态决定的，
    /// 不是「取不到后面的页」—— 要翻页请走 `GET /objects/{file_id}/preview`。
    ///
    /// ## `budget` 的两条用法
    ///
    /// - **真预算**（推送路径）：`DRAWING_CI_BUDGET` 那个量级，会提交 CI job 并轮询
    ///   到收敛或预算耗尽；
    /// - **`Duration::ZERO`**（只读路径，如管理端 `/preview`）：一次 python 往返都
    ///   不发生，只把**已经 READY** 的那些直链取出来。⇒ 「预览一次日报」不会烧掉
    ///   CI 配额、也不会让 HTTP 请求挂在那儿等几分钟。
    ///
    /// ## ⚠️ **准备段恒 2 条语句 + 推进段轮转**（为什么不按 part 逐个等）
    ///
    /// | 段 | 语句数 | 与 part 数的关系 |
    /// |---|---|---|
    /// | 准备 | `t_part_file` 按 `part_id` 批量 **1** 条 + `t_file_preview` 按 seed 批量 **1** 条 | **无关** |
    /// | 推进 | 每个未收敛者**每格**至多 1 次 python 往返 + 3~4 条状态迁移语句 | 与「未收敛数 × 轮数」成正比 |
    ///
    /// 已就绪的那些**零 IO** 返回、不进推进段。
    ///
    /// **轮转**而不是「逐个 part 等到收敛」：串行等会让排在后面的 part 在前面那个慢
    /// job 上把预算耗光，而各 part 的收敛时刻彼此独立（不同 CI job、不同 PDF）。轮转
    /// 的代价是「预算耗尽时每个 part 拿到的轮数相同」—— 这正是想要的结果：要么都挂上
    /// 链接，要么都不挂，而不是清单前半段有链接、后半段没有。
    ///
    /// ⚠️ 所以调用方**有义务**把入参控制在「本次真的要用」的量级：推进段是唯一随 part
    /// 数增长的语句来源，而它每一次都可能提交一个**按页计费**的 CI job。日报那条链路
    /// 因此只传「截断之后会被列出来的那些行」。
    ///
    /// ## ⚠️ **全程不持有 DB 连接**
    ///
    /// 每一格由 [`Self::advance_one`] 自己短借短还，python 往返期间**手里没有连接**
    /// —— 本函数最长会跑满 `budget`（分钟级），握着一条 `PoolConnection` 等 CI 会把
    /// 连接池占满、读端点跟着排队。
    ///
    /// ⚠️ `#[allow(too_many_arguments)]` 是**刻意**的：八个形参里有五个是
    /// **部署级配置**（`pool` / `py_backend` / `prefix` / `base_url` / `image_params`），
    /// 它们在本仓的惯例里都由调用方从 `AppState` 直接传（`handler::get_part_file_preview`
    /// 同款）。打成结构体只会在签名里造一层「看起来更整齐、实际每处调用都要多构造一个
    /// 东西」的壳，而这三个 `&str` 在函数体内再拆包时还得逐个注明谁是谁。
    #[allow(clippy::too_many_arguments)]
    pub async fn ensure_ready_for_parts(
        &self,
        pool: &PgPool,
        py_backend: &Arc<dyn PyBackendClient>,
        preview_prefix: &str,
        preview_base_url: &str,
        image_params: &str,
        part_ids: &[i64],
        budget: Duration,
    ) -> HashMap<i64, String> {
        // ① 每个 part 的最新一张 PDF 图纸（`t_part_file` 批量一条语句）。
        let files = match Self::latest_pdf_drawings(pool, part_ids).await {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "批量确保图纸预览：读 t_part_file 失败，本期不挂任何链接");
                return HashMap::new();
            }
        };
        if files.is_empty() {
            // 没有一份 PDF 图纸 ⇒ 一个字都不必打 python（连 `t_file_preview` 都不查）。
            return HashMap::new();
        }
        // ② 按 seed 批量查 `t_file_preview`（一条语句）。
        let urls = UrlParams {
            prefix: preview_prefix,
            base_url: preview_base_url,
            image_params,
        };
        let (ready, pending) = match self.split_by_state(pool, &files, urls).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "批量确保图纸预览：读 t_file_preview 失败，本期不挂任何链接");
                return HashMap::new();
            }
        };

        // ③ 轮转推进未收敛者，直到预算耗尽 / 全部收敛 / 全部终局失败。
        self.drive_pending(
            pool,
            py_backend,
            urls,
            pending,
            ready,
            tokio::time::Instant::now() + budget,
        )
        .await
    }

    /// 轮转推进：每一轮让每个未收敛者各推一格，直到预算耗尽或全部收敛 / 失败。
    ///
    /// ## 为什么是**轮转**而不是「逐个 part 等到收敛」
    ///
    /// 逐个串行等会让排在后面的 part 在前面那个慢 job 上把预算耗光，而各 part 的
    /// 收敛时刻彼此独立（不同 CI job、不同 PDF）。轮转的代价是「预算耗尽时每个 part
    /// 拿到的轮数相同」，这正是想要的结果：要么都挂上链接，要么都不挂 —— 而不是
    /// 清单前半段有链接、后半段没有。
    async fn drive_pending(
        &self,
        pool: &PgPool,
        py_backend: &Arc<dyn PyBackendClient>,
        urls: UrlParams<'_>,
        mut pending: Vec<DrawingTarget>,
        mut ready: HashMap<i64, String>,
        deadline: tokio::time::Instant,
    ) -> HashMap<i64, String> {
        while !pending.is_empty() {
            let mut still: Vec<DrawingTarget> = Vec::with_capacity(pending.len());
            // `drain(..)` 把值**移**出来：循环体里有 `.await`，借用 `pending` 的元素
            // 会跨 await 存活（编译器直接拒绝）。
            for t in pending.drain(..) {
                if tokio::time::Instant::now() >= deadline {
                    still.push(t);
                    continue;
                }
                match self
                    .advance_one(
                        pool,
                        py_backend,
                        urls.prefix,
                        &t.seed,
                        &t.file,
                        SYSTEM_ACTOR_ID,
                    )
                    .await
                {
                    Ok(Step::Ready { page_count, .. }) => {
                        if let Some(u) = urls.first_page(&t.seed, page_count) {
                            ready.insert(t.part_id, u);
                        }
                    }
                    Ok(Step::Pending { .. }) => still.push(t),
                    Ok(Step::Failed { reason }) => {
                        // 终局失败**不重试**：CI 按页计费，而这一格的失败已被状态机打
                        // 成 `FAILED`（终局），下一次调用（含明天这次日报）自愈重提。
                        warn!(seed = %t.seed, reason = %reason, "批量确保图纸预览：CI 终局失败，该份不带链接");
                    }
                    // ⚠️ 打 python 失败**不中断整批**：其余 part 的预览仍能就绪。行保持
                    // `PENDING` 且认领已归还 ⇒ 下一次调用会自动补提交。
                    Err(e) => {
                        warn!(seed = %t.seed, error = %e, "批量确保图纸预览：打 python 失败，该份不带链接");
                    }
                }
            }
            pending = still;
            if pending.is_empty() {
                break;
            }
            // 一格都没收敛 ⇒ 守一个节流间隔再进下一轮。⚠️ sleep 时长取
            // `min(节流, 距 deadline)`：一次收尾 sleep 越过 deadline 就等于整条推送
            // 超出长档超时（`middleware/timeout.rs` 的 660s）。
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remain.is_zero() {
                break;
            }
            tokio::time::sleep(std::cmp::min(remain, self.backoff_std())).await;
        }
        if !pending.is_empty() {
            warn!(
                pending = pending.len(),
                ready = ready.len(),
                "批量确保图纸预览：预算耗尽，未收敛的那几份不带链接"
            );
        }
        ready
    }

    /// 每个 part 的**最新一张** PDF 图纸（`part_id → TPartFile`）。
    ///
    /// ## ⚠️ 「最新」在内存里判，`DISTINCT ON` 不进 SQL
    ///
    /// 与任何「每 owner 取最新一行」的批量查询同款取舍：`DISTINCT ON` 能少传行，
    /// 但它把「同一 part 有 3 张图纸时取哪张」这条业务口径藏进一个 `ORDER BY`；
    /// 放在这里它就是一条有名字、有单测的规则（[`Self::latest_of`]），口径变了也不会
    /// 有人漏改 SQL。
    ///
    /// 排序 `ORDER BY part_id ASC, created_at ASC, id ASC` 只为让入参顺序确定、
    /// 便于复现与排障（判「最新」不依赖它，见上）。
    ///
    /// ⚠️ 入参为空集时**不发 SQL**（省一条语句；本域的通用纪律）。
    async fn latest_pdf_drawings(
        pool: &PgPool,
        part_ids: &[i64],
    ) -> Result<Vec<DrawingTarget>, sqlx::Error> {
        if part_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = pool.acquire().await?;
        let rows =
            crate::modules::files::part_file::repo::PartFileRepo::list_pdf_drawings_by_part_ids(
                &mut *conn, part_ids,
            )
            .await?;
        drop(conn);

        let mut by_part: HashMap<i64, TPartFile> = HashMap::with_capacity(rows.len());
        for f in rows {
            match by_part.entry(f.part_id) {
                Entry::Occupied(mut prev) => {
                    if Self::is_newer(&f, prev.get()) {
                        prev.insert(f);
                    }
                }
                Entry::Vacant(v) => {
                    v.insert(f);
                }
            }
        }
        Ok(by_part
            .into_iter()
            .map(|(part_id, file)| DrawingTarget {
                part_id,
                seed: Self::preview_seed(&file),
                file,
            })
            .collect())
    }

    /// 「`b` 比 `a` 新就换掉它」（判据 = `(created_at, id)` 的字典序）。
    ///
    /// ⚠️ `id` 是**必需**的 tiebreaker：同一秒上传多张图纸时，取哪张取决于 PG 的物理
    /// 行序 —— 少了它，同一个 part 两次生成日报会给出不同的图纸链接。
    fn is_newer(cand: &TPartFile, cur: &TPartFile) -> bool {
        (cand.created_at, cand.id) > (cur.created_at, cur.id)
    }

    /// 把批量目标按「已就绪 / 待推进」分成两堆（`t_file_preview` 按 seed 批量一条语句）。
    ///
    /// 分堆而不是「一条一条查」是这条链路 **O(1)** 的另一半：READY 的那些**零 IO**
    /// 返回、不进 python 往返循环。
    async fn split_by_state(
        &self,
        pool: &PgPool,
        targets: &[DrawingTarget],
        urls: UrlParams<'_>,
    ) -> Result<(HashMap<i64, String>, Vec<DrawingTarget>), sqlx::Error> {
        let seeds: Vec<String> = targets.iter().map(|t| t.seed.clone()).collect();
        let mut conn = pool.acquire().await?;
        let rows = PreviewRepo::find_by_seeds(&mut *conn, &seeds).await?;
        drop(conn);
        let by_seed: HashMap<&str, &TFilePreview> =
            rows.iter().map(|r| (r.preview_seed.as_str(), r)).collect();

        let mut ready: HashMap<i64, String> = HashMap::new();
        let mut pending: Vec<DrawingTarget> = Vec::new();
        for t in targets {
            match by_seed.get(t.seed.as_str()) {
                // ⚠️ 只认 `READY` 且 `page_count > 0`：0 页的 READY 是 CI 报成功时
                // 对象尚未落盘的窗口，状态机的 0 页护栏会把它当进行中处理，这里照同一
                // 判据把它划进「待推进」。
                Some(r) if r.state == state::READY && r.page_count > 0 => {
                    if let Some(u) = urls.first_page(&t.seed, r.page_count as i64) {
                        ready.insert(t.part_id, u);
                    }
                }
                _ => pending.push(t.clone()),
            }
        }
        Ok((ready, pending))
    }

    /// ★ **推进一格**：状态机唯一的实现，两条驱动路径（HTTP 端点 / 批量入口）共用。
    ///
    /// 「一格」= 从「读行；必要时登记」开始，到「至多一次 python 往返 + 相应落库」
    /// 为止。返回值即这一格的结论，**由调用方决定怎么处置**（HTTP 端点把
    /// `Failed` 变成 502，批量入口只是跳过这一份）。
    ///
    /// ## 状态分支（与 [`Self::get_preview`] 的 doc 同表，两处必须同步）
    ///
    /// | 条件 | 动作 | 外部依赖 |
    /// |---|---|---|
    /// | seed 未命中 | INSERT PENDING（`ON CONFLICT DO NOTHING`）→ **登记成功才**提交 CI | 提交 1 次 |
    /// | seed 未命中但 INSERT 冲突（并发，别的请求已登记） | 返 `Pending`（`retry_after_ms` = 一个节流间隔） | **无** |
    /// | `READY` | 返 `Ready`，**零 IO**（不登记、不打 python） | **无** |
    /// | `FAILED` | 自愈重置为 `PENDING` 后按 `PENDING` 处理；重置 **0 行** ⇒ 读回真实状态按它走 | 见下 |
    /// | `PENDING` 且 `now < next_poll_at` | 返 `Pending`（节流） | **无** |
    /// | `PENDING` 且到期、**无真 job id**（NULL 或认领占位 token） | **条件认领**（`claim_submit` 抢到才继续）→ 打 python | 提交 ≤1 次 |
    /// | 认领失败（并发，别人正在提交 / 行已非 PENDING） | 返 `Pending`，**绝不自己打 python** | **无** |
    /// | `PENDING` 且到期且有真 job id | describe：Success → `Ready`；**四种终局失败**（`Failed` / `Cancel` / `Pause` / `NotExist`）→ `Failed`；其余 → 推进 `next_poll_at` 返 `Pending` | 查询 1 次 |
    ///
    /// ## ⚠️ 并发不重复提交 CI job 是**两道闸**，各管一段
    ///
    /// | 闸 | 管哪条路径 | 机制 |
    /// |---|---|---|
    /// | `uk_t_file_preview_seed` 唯一索引 | **首次登记** | `insert_pending` 的 `ON CONFLICT DO NOTHING`，`rows==1` 才是登记者 |
    /// | [`PreviewRepo::claim_submit`] 条件 UPDATE | **重试** 与 **自愈** | 「行已存在 + 无真 job id + 到期」，两个请求都能读到；认领抢到才允许打 python |
    ///
    /// 只靠唯一索引会漏掉后两条路径（详见该方法的 doc）。本函数是它们**唯一**的实现
    /// 点，批量入口与 HTTP 端点都从这里过 ⇒ 加一条驱动路径不会漏掉任一闸。
    ///
    /// ## ⚠️ 连接纪律：每个 DB 步骤短借短还，**python 往返期间不持连接**
    ///
    /// 一次 `advance_one` 至多打一次 python（提交或 describe，各有
    /// `PythonBackendConfig::timeout_ms` 缺省 10s 的超时）。HTTP 端点路径上这点时间
    /// 无所谓，而批量入口要在预算内对几十份文件反复调本函数 ⇒ 「python 在飞时手里
    /// 握着一条连接」会随并发的日报数线性堆积，把连接池占满、读端点跟着排队。
    async fn advance_one(
        &self,
        pool: &PgPool,
        py_backend: &Arc<dyn PyBackendClient>,
        preview_prefix: &str,
        seed: &str,
        file: &TPartFile,
        actor_id: i64,
    ) -> Result<Step, AppError> {
        let now = now_naive();

        // ── 1. 取当前行；未命中则尝试登记 ──
        let mut conn = pool.acquire().await?;
        let mut row = match PreviewRepo::find_by_seed(&mut *conn, seed).await? {
            Some(r) => Some(r),
            None => {
                let inserted = PreviewRepo::insert_pending(
                    &mut *conn,
                    self.snowflake.next_id(),
                    seed,
                    &file.object_key,
                    // ⚠️ **置 `now()` 而不是「现在 + 退避」**：本次调用就是登记者，
                    // 它必须**紧接着**就提交 job（否则首次调用只会登记不提交，调用方要
                    // 等一个节流间隔后的第二次调用才看见 job 被提交；间隔缺省 2s、可经
                    // env `PREVIEW_POLL_BACKOFF_SECONDS` 配 1..=5）。
                    // `now()` 同时也让「登记成功但进程在提交前挂了」能被下一次调用
                    // 的「补提交」分支恢复（自愈）。
                    now,
                    actor_id,
                )
                .await?;
                if inserted {
                    // 本调用是登记者 → 读回自己刚插的那行，交给下面的统一状态机处理
                    PreviewRepo::find_by_seed(&mut *conn, seed).await?
                } else {
                    // 撞唯一索引 = 并发，别的请求正在提交 → 直接返 Pending，
                    // **绝不自己再提交一次**（重复提交 CI = 重复计费）
                    None
                }
            }
        };

        // 并发分支：命中别人刚登记的 PENDING 且尚未提交，本调用不做任何推进。
        // 返 Pending 让调用方稍后再来（那时轮询条件已到期）。
        let Some(row) = row.as_mut() else {
            drop(conn);
            return Ok(Step::Pending {
                retry_after_ms: self.backoff_ms(),
            });
        };

        // 2a. READY → **立即返回，零 IO**。
        //     ⚠️ 必须排在所有推进逻辑之前：`mark_ready` 会把 `next_poll_at` 清成
        //     NULL，而 `poll_due` 把 NULL 当作「到期」（PENDING 行的 fail-open）。
        //     若把 READY 交给下面的 PENDING 流程，一个本该零 IO 的请求会去打一次
        //     python describe —— 既慢、又可能把并发窗口里的状态写乱。
        if row.state == state::READY {
            let out = Step::Ready {
                seed: row.preview_seed.clone(),
                page_count: row.page_count as i64,
            };
            drop(conn);
            return Ok(out);
        }

        // 2b. FAILED → 自愈重置为 PENDING 后重走下面的推进流程。
        //     ⚠️ **只判 FAILED、不碰 READY**（READY 已在 2a 返回）。
        //
        //     ⚠️⚠️ `reset_to_pending` 的**返回值必须判**：0 行不是错误，而是「并发下
        //     别人已把这行推进走了」。把它丢掉并无条件把内存行改成
        //     `PENDING / ci_job_id = None` 会让本调用**再提交一个 CI job**，与赢家那个
        //     并存（按页计费 = 双份钱，先写的那个变孤儿）。
        if row.state == state::FAILED {
            let reset = PreviewRepo::reset_to_pending(&mut *conn, row.id, actor_id).await?;
            if reset == 0 {
                // 认输：按**别人推进后的真实状态**走，绝不自己提交。
                let Some(fresh) = PreviewRepo::find_by_seed(&mut *conn, seed).await? else {
                    // 行被软删了（本仓 preview 侧零 DELETE，理论上不会发生）：
                    // 退化成「不推进」，让调用方下轮再来。
                    return Ok(self.pending_step(row, &now));
                };
                *row = fresh;
                drop(conn);
                // 重新过 2a：可能已被别人推到 READY，那就该零 IO 直接返。
                if row.state == state::READY {
                    return Ok(Step::Ready {
                        seed: row.preview_seed.clone(),
                        page_count: row.page_count as i64,
                    });
                }
                // 仍是 FAILED（别人又把它打失败了）⇒ 本调用不参与自愈，返 Pending。
                if row.state == state::FAILED {
                    return Ok(self.pending_step(row, &now));
                }
                // 否则是 PENDING ⇒ 继续走下面的 2c / 2d（2d 有条件认领，不会重复提交）。
                // ⚠️ 重新借一条而不是复用上面那条：上面那条此刻仍被本作用域持有。
                let fresh_conn = pool.acquire().await?;
                return self
                    .advance_pending(
                        pool,
                        py_backend,
                        preview_prefix,
                        seed,
                        file,
                        actor_id,
                        fresh_conn,
                        row,
                        now,
                    )
                    .await;
            } else {
                row.state = state::PENDING.to_string();
                row.ci_job_id = None;
                row.page_count = 0;
                row.error_message = None;
            }
        }

        self.advance_pending(
            pool,
            py_backend,
            preview_prefix,
            seed,
            file,
            actor_id,
            conn,
            row,
            now,
        )
        .await
    }

    /// 推进「已是 `PENDING`」的行（`advance_one` 的后半段）。
    ///
    /// ## ⚠️ `conn` **按值持有**，`drop(conn)` 是「真还连接」的唯一写法
    ///
    /// 形参若取 `&mut PoolConnection`，函数里的 `drop(conn)` 只是丢掉一个引用、连接
    /// **不会**回池（编译器只会给 `unused_must_use` 之外的 `dropping_references` 警告，
    /// 而那条警告恰好被本函数最初的一版吞掉了）—— 于是「python 在飞时不占池位」这条
    /// 连接纪律在代码上看起来成立、实际不成立。故本形参取**所有权**：跨网络往返前
    /// `drop(conn)` 把连接真的还回池，之后再 `pool.acquire()` 借一条新的。
    ///
    /// 写成本方法（而不是内联进 `advance_one`）是为了让「重置 0 行 → 读回真实状态 →
    /// 可能已是 READY / FAILED」的收尾有地方落脚，又不必把整个状态机的缩进再套一层。
    #[allow(clippy::too_many_arguments)]
    async fn advance_pending(
        &self,
        pool: &PgPool,
        py_backend: &Arc<dyn PyBackendClient>,
        preview_prefix: &str,
        seed: &str,
        file: &TPartFile,
        actor_id: i64,
        mut conn: PoolConnection<sqlx::Postgres>,
        row: &mut TFilePreview,
        now: NaiveDateTime,
    ) -> Result<Step, AppError> {
        // 2c. PENDING & 未到期 → 节流返回（不打 python）
        if !Self::poll_due(row, &now) {
            return Ok(self.pending_step(row, &now));
        }

        // 2d. 尚无真 job id → **条件认领**后补提交
        //     （首次登记的正常延续 / 上次提交失败后的重试 / 自愈后的重新提交）
        //
        //     ⚠️ **认领先于打 python**：唯一索引只闸首次登记，这两条路径是「行已存在」，
        // 两个请求会同时读到「due + 无 job id」。「先查再打 python」是 TOCTOU ⇒ 两个
        // CI job（按页计费）。标准写法是用一条带谓词的 UPDATE 认领，只有抢到的那个才
        // 允许打 python。
        //
        //     ⚠️ 判据是「**没有真 job id**」而不是「`ci_job_id IS NULL`」：认领期间该列
        // 被写成 [`CLAIM_TOKEN_PREFIX`] 占位 token，必须一并认作「尚未提交」（进程在
        // 认领与 mark_submitted 之间挂掉时，下一次调用靠这条恢复）。
        let needs_submit = match row.ci_job_id.as_deref() {
            None => true,
            Some(id) => id.starts_with(CLAIM_TOKEN_PREFIX),
        };
        if needs_submit {
            let claim_token = format!("{CLAIM_TOKEN_PREFIX}{}", self.snowflake.next_id());
            // 租约**不是**节流戳：它只用来在「认领 → POST → mark_submitted」这段窗口内
            // 挡住别的请求（见 CLAIM_LEASE_SECONDS 的 doc）。
            let lease_until = now + chrono::Duration::seconds(CLAIM_LEASE_SECONDS);
            // ⚠️ `now` 一并传入：租约到期比较用**应用时钟**，不用 SQL 的 `now()`
            //    （DB 会话时区与应用时钟是两个东西，理由见 repo 的 doc）
            let claimed = PreviewRepo::claim_submit(
                &mut *conn,
                row.id,
                &claim_token,
                lease_until,
                now,
                actor_id,
            )
            .await?;
            if !claimed {
                // 认领失败 = 别人正在提交（租约未过）/ 行已不是 PENDING
                // ⇒ **绝不自己打 python**。返 Pending 让调用方按 retry_after_ms 再来。
                return Ok(self.pending_step(row, &now));
            }
            let next_poll = now + self.backoff();
            // ⚠️ 提交是**跨网络**的往返 ⇒ 先还连接再打，python 在飞时不占池位。
            drop(conn);
            let submitted =
                Self::submit(py_backend, preview_prefix, seed, &file.object_key, actor_id).await;
            let mut conn = pool.acquire().await?;
            let outcome = match submitted {
                Ok(job_id) => {
                    // ⚠️ `claim_token` 必须一并传入：`mark_submitted` 的谓词靠它校验
                    //    「我才是认领者」。传错 / 传空会让写入永远得 0 行 ⇒ job id 落不了
                    //    库 ⇒ 预览永远卡在 PENDING。
                    PreviewRepo::mark_submitted(
                        &mut *conn,
                        row.id,
                        &job_id,
                        next_poll,
                        &claim_token,
                        actor_id,
                    )
                    .await?;
                    // ⚠️ **两处都要更新**：`pending_step` 读的是内存里这一行的
                    // `next_poll_at`，只改 `ci_job_id` 会让它仍停在「登记时的 `now()`」
                    // ⇒ 算出 `retry_after_ms = 0`，调用方立刻重调，退避形同虚设（变成
                    // 忙轮询）。且此时传 `&now`（不是 `&next_poll`）才是「距现在还有
                    // 多久」的正确语义。
                    row.ci_job_id = Some(job_id);
                    row.next_poll_at = Some(next_poll);
                    Ok(self.pending_step(row, &now))
                }
                Err(e) => {
                    // 提交失败**不**把行打成 FAILED —— 那是 CI 的终态语义，而这里只是
                    // 「这次没送出去」。行保持 PENDING / ci_job_id 仍为空，下次调用（过了
                    // next_poll_at）会自动重试 ⇒ 自愈。
                    warn!(
                        seed = %seed,
                        error = %e,
                        "CI 预览任务提交失败，行保持 PENDING 待重试"
                    );
                    // ⚠️ **归还认领**（`ci_job_id` 清回 NULL），不能留着占位 token：
                    // 留着虽然也能被下一轮重新认领（谓词认 token），但该列上挂一个
                    // python 侧不存在的值，会误导任何将来直接读这一列的排障代码。
                    // 用独立方法而非改 `touch_poll`：`touch_poll` 还有一个调用点
                    // （2e 的 Running 分支）**必须保留真 job id**，两处语义相反。
                    let released =
                        PreviewRepo::release_claim(&mut *conn, row.id, &claim_token, actor_id)
                            .await?;
                    // ⚠️⚠️ **只在真的归还成功（1 行）时才推节流戳**：得 0 行 = 认领**已被
                    // 接管** / 行已不是 PENDING。此时若仍无条件
                    // `touch_poll(now + 节流间隔)`，就会把**接管者刚写的租约戳**
                    // （now + 30s）**缩短**成 now + 节流间隔（缺省 2s，可配 1..=5）⇒ 第三个
                    // 请求一个节流间隔后即可合法接管 ⇒「租约不可抢」这条闸被一个善意的
                    // 退避动作从下面捅穿。
                    if released == 1 {
                        // 节流：至少等一个 backoff 再重试，避免对着一个必然失败的上游连打
                        // （错误原样抛给调用方 → 21118 / HTTP 502）。
                        PreviewRepo::touch_poll(&mut *conn, row.id, now + self.backoff(), actor_id)
                            .await?;
                    } else {
                        warn!(
                            seed = %seed,
                            "归还认领得 0 行（已被接管或行已非 PENDING），不推节流戳以免缩短接管者的租约"
                        );
                    }
                    Err(e)
                }
            };
            return outcome;
        }

        // 2e. 有真 job id 且到期 → describe
        //     ⚠️ 走到这里时 `ci_job_id` 必非 None（「补提交」分支已把「无真 job id」
        //     return 掉），故这里用 `expect` 而不是 `unwrap_or_default()` —— 后者会把
        //     「状态机被改坏」静默变成「打 python 一个空 job id 的 URL」，故障现场难查。
        //     ⚠️⚠️ **改上面那个分支的结构时必须同步改这里**：它多认了一种
        //     `CLAIM_TOKEN_PREFIX` 占位值，若放行条件被放宽而本处没跟着把占位值滤掉，
        //     就会拿一个 python 侧不存在的 job id 去 describe。
        let job_id = row
            .ci_job_id
            .as_deref()
            .expect("2e 分支必已确保 ci_job_id 非空")
            .to_string();
        // ⚠️ 同上：describe 是跨网络往返，先还连接。
        drop(conn);
        let describe = Self::describe(py_backend, &job_id, actor_id).await;
        let mut conn = pool.acquire().await?;
        let describe = describe?;
        match describe.status {
            CiStatus::Success => {
                PreviewRepo::mark_ready(&mut *conn, row.id, describe.page_count, actor_id).await?;
                Ok(Step::Ready {
                    seed: row.preview_seed.clone(),
                    page_count: describe.page_count as i64,
                })
            }
            CiStatus::Failed | CiStatus::Canceled | CiStatus::Paused | CiStatus::NotExist => {
                // ⚠️ **四个变体逐个列出、刻意不写 `_`**：编译器会在本函数新增
                // `CiStatus` 变体时把这里变红，防止新词又悄悄落进兜底变无限轮询。
                //
                // 四者统一落 `FAILED`（含 `NotExist`），**不静默重提 job** —— 决策理由与
                // 「为什么不会变成无限提交 job」的论证见 [`super::mod`] 的「CI 终局失败的
                // 落点」一节。
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
                PreviewRepo::mark_failed(&mut *conn, row.id, &msg, actor_id).await?;
                Ok(Step::Failed { reason: msg })
            }
            CiStatus::Running => {
                // 退避推进节流戳，返 Pending（调用方按 retry_after_ms 再来）
                let next_poll = now + self.backoff();
                PreviewRepo::touch_poll(&mut *conn, row.id, next_poll, actor_id).await?;
                row.next_poll_at = Some(next_poll);
                // ⚠️ **传 `&now` 而不是 `&next_poll`**：与「补提交」分支同一纪律 ——
                // `pending_step` 算的是 `next_poll_at - 传入时刻` 即「距现在还有多久」，
                // 传 `&next_poll` 会让结果恒为 **0** ⇒ 调用方拿到 `retry_after_ms = 0`
                // 立刻重调，退避形同虚设（变成**忙轮询**，每次都真打一次 python）。
                Ok(self.pending_step(row, &now))
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

    /// [`Self::backoff`] 的 `std::time::Duration` 形态（批量推进循环要 `tokio::time::sleep`）。
    ///
    /// clamp 口径与 [`Self::backoff`] **同源**（同一区间常量），两处不得漂移。
    fn backoff_std(&self) -> Duration {
        Duration::from_secs(
            self.poll_backoff_seconds
                .clamp(1, PREVIEW_POLL_BACKOFF_MAX_SECONDS) as u64,
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
    fn pending_step(&self, row: &TFilePreview, now: &NaiveDateTime) -> Step {
        Step::Pending {
            retry_after_ms: row
                .next_poll_at
                .map(|at| (at - *now).num_milliseconds().max(0) as u64)
                .unwrap_or_else(|| self.backoff_ms()),
        }
    }

    /// READY 出参：`pages` 长度恒等于 `page_count`（同一处循环产出，不可能不等长）。
    ///
    /// `image_params` 是 `size` 档位选好的图片处理参数，原样透传给
    /// [`Self::preview_urls`]（空串 = `full` 档 / 未配置）。
    ///
    /// 形参是 `seed` + `page_count` 而不是 `&TFilePreview`：状态机在
    /// [`Self::advance_pending`] 里已经**把行还回内存**、连接也还了，拿到裸值比把整行
    /// 的生命周期拖到这里干净。
    fn ready_out(
        seed: &str,
        page_count: i64,
        preview_base_url: &str,
        preview_prefix: &str,
        image_params: &str,
    ) -> PreviewOut {
        PreviewOut::ready(
            page_count,
            Self::preview_urls(
                preview_base_url,
                preview_prefix,
                seed,
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
        actor_id: i64,
    ) -> Result<String, AppError> {
        let body = json!({
            "src_object_key": src_object_key,
            "dst_prefix": Self::preview_object_prefix(preview_prefix, seed),
        });
        let resp = py_backend
            .forward_ci_submit(body, Self::fwd_headers(actor_id))
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
        actor_id: i64,
    ) -> Result<CiDescribe, AppError> {
        let resp = py_backend
            .forward_ci_describe(job_id, Self::fwd_headers(actor_id))
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
    ///
    /// ⚠️ 形参是 `i64` 而不是 `&CurrentUser`：批量入口由**后台任务**驱动，那里没有
    /// 登录用户（它署名 [`SYSTEM_ACTOR_ID`]）。要让它为了「拿一个 id」而构造一个
    /// `CurrentUser` 假人，就得凭空编出角色 / 货架 scope —— 那几个字段本请求里
    /// 一个也不读，编出来只是让「这个请求是谁」看起来比实际更确定。
    fn fwd_headers(actor_id: i64) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Ok(v) = actor_id.to_string().parse() {
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

/// ★ **推进一格**的结论（[`PreviewService::advance_one`] 的出参）。
///
/// ## 为什么不让 `advance_one` 直接产 `PreviewOut`
///
/// 因为**两条驱动路径对「失败」的处置相反**：HTTP 端点要把终局失败变成 502 信封
/// （前端据此停自动重试），而批量入口只是「这一份不挂链接」、整份日报照发。让状态机
/// 自己产 wire 出参就得在这两条路径之间做取舍判断，而那正是调用方的权限。
enum Step {
    /// 已收敛到 `READY`（`seed` 是回读后的种子，`page_count > 0` 恒成立）。
    ///
    /// 返 seed 而不是让调用方自己留着 `file`：种子的 `char(64)` 空白填充只在
    /// `find_by_seed` 的读出口被 `trim_end()` 修过（见 [`PreviewRepo::find_by_seed`]），
    /// 由状态机把这个**已修好的值**交给调用方，就不必在每个调用点各记一次「读出来的
    /// seed 要 trim」这条纪律。
    Ready { seed: String, page_count: i64 },
    /// 仍在推进中；`retry_after_ms` = 距现在还差多久（**不是** `next_poll_at -
    /// next_poll_at` 那个恒 0 的值，见 `pending_step` 的注释）。
    Pending { retry_after_ms: u64 },
    /// 落入终局失败：行**已**被打成 `FAILED`，`reason` 是落进 `error_message` 的同一句。
    Failed { reason: String },
}

/// 批量入口的一个目标：「某个 part 的最新一张 PDF 图纸」+ 由它派生的种子。
///
/// `file` 是**整行**而不是几列窄投影：`advance_one` 的两条路径都要它的 `object_key`
/// 与 `content_sha256`（推 seed + 提交 CI），而窄投影会在「推 seed 的地方补一列」
/// 与「提交的地方补一列」之间来回漏。
#[derive(Debug, Clone)]
struct DrawingTarget {
    /// `t_part_file.part_id`（⚠️ polymorphic：散件行传 `t_part.id`、装配件行传
    /// `t_assembly.id` —— 同一条查询、同一列，见 [`PreviewService::latest_pdf_drawings`]）。
    part_id: i64,
    /// 由 [`PreviewService::preview_seed`] 派生（纯哈希，⚠️ **不带文件名**）。
    seed: String,
    file: TPartFile,
}

/// 拼直链所需的三个**部署级**参数（批量入口一路传下去）。
///
/// 打成结构体而不是三个形参：`first_page` 与 `advance_one` 都要用到它们，逐层
/// 拆三个字符串会让「哪个是 base、哪个是 prefix」在每个签名里重新猜一遍。
#[derive(Debug, Clone, Copy)]
struct UrlParams<'a> {
    prefix: &'a str,
    base_url: &'a str,
    image_params: &'a str,
}

impl UrlParams<'_> {
    /// 第 1 页的直链；`page_count <= 0` ⇒ `None`（**不返回空串**：空串会被调用方当成
    /// 一个合法链接塞进 markdown，渲染出一个点不动的空链接）。
    ///
    /// ⚠️ 路径成分只有 `{base}/{prefix}/{seed}/p1.jpg`（+ 可选 query），**绝不含
    /// 文件名** —— 图号可枚举而 `previews/` 是 public-read，理由与补偿见
    /// [`super::mod`] 的「⚠️⚠️ 安全条款：预览路径里绝不能带文件名」。本函数复用
    /// [`PreviewService::preview_urls`]（与 CI 写对象的 `dst_prefix` 同一真源），所以
    /// 「写到哪里」与「读哪里取」不可能分叉。
    fn first_page(&self, seed: &str, page_count: i64) -> Option<String> {
        PreviewService::preview_urls(
            self.base_url,
            self.prefix,
            seed,
            page_count,
            self.image_params,
        )
        .into_iter()
        .next()
    }
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
    use chrono::NaiveDate;

    /// python 的统一信封（`R.fail` 的形状：`{code, message, data}`）。
    fn envelope(code: i64, message: &str) -> Bytes {
        Bytes::from(
            serde_json::to_vec(&json!({"code": code, "message": message, "data": null})).unwrap(),
        )
    }

    // ---------------------------------------------------------------
    // 批量入口的两条纯逻辑（不碰 DB、不发 HTTP）
    // ---------------------------------------------------------------

    /// 造一条 `t_part_file` 行（纯内存 fixture，不落库）。
    fn file_row(id: i64, part_id: i64, filename: &str, sec: u32) -> TPartFile {
        TPartFile {
            id,
            part_id,
            kind: "DRAWING".into(),
            file_type: "PDF".into(),
            // ⚠️ 形状照生产：`{sha16}_{原文件名}`，而文件名是**图号**、可枚举。
            object_key: format!("0123456789abcdef_{filename}"),
            original_filename: filename.into(),
            file_size: 1024,
            content_type: "application/pdf".into(),
            upload_status: "READY".into(),
            content_sha256: None,
            version: 0,
            created_at: NaiveDate::from_ymd_opt(2026, 10, 23)
                .expect("固定日期合法")
                .and_hms_opt(9, 0, sec)
                .expect("时分秒合法"),
            created_by: Some(1),
            updated_at: NaiveDate::from_ymd_opt(2026, 10, 23)
                .expect("固定日期合法")
                .and_hms_opt(9, 0, sec)
                .expect("时分秒合法"),
            updated_by: Some(1),
            deleted_at: None,
            paired_file_id: None,
        }
    }

    /// ★ 守卫：批量入口取「每个 part 的**最新一张**」，同秒上传时按 `id` 定序。
    ///
    /// ⚠️ 少了 `id` 这个 tiebreaker，同一秒上传的两张图纸取哪张取决于 PG 的物理行序
    /// ⇒ 同一个工单两份日报给出不同链接，而两边都不会报错。
    #[test]
    fn newest_drawing_wins_and_id_breaks_created_at_ties() {
        let older = file_row(11, 1, "A00123.pdf", 10);
        let newer = file_row(12, 1, "A00124.pdf", 20);
        assert!(
            PreviewService::is_newer(&newer, &older),
            "更晚上传的那张必须胜出"
        );
        assert!(!PreviewService::is_newer(&older, &newer));

        // 同一秒：`id` 更大者胜出（两条 SQL 的 ORDER BY 都按 id 兜底排序）。
        let tie_low = file_row(21, 2, "A00201.pdf", 30);
        let tie_high = file_row(22, 2, "A00202.pdf", 30);
        assert!(PreviewService::is_newer(&tie_high, &tie_low));
        assert!(!PreviewService::is_newer(&tie_low, &tie_high));
    }

    /// ⚠️⚠️ 安全守卫：批量入口给出的直链**只有** `<prefix>/<seed>/p1.jpg`，
    /// **绝不带文件名**（图号可枚举而 `previews/` 是 public-read）。
    ///
    /// 同时钉住「只给第 1 页」这条形态决定：多页图纸在日报里只有一个链接位。
    #[test]
    fn batch_link_path_is_prefix_seed_and_first_page_only() {
        let f = file_row(31, 1, "A00123_rev2.pdf", 10);
        let seed = PreviewService::preview_seed(&f);
        let urls = UrlParams {
            prefix: "previews",
            base_url: "https://cos.example.com/",
            image_params: "eo-img.resize=l/2560",
        };
        let link = urls
            .first_page(&seed, 7)
            .expect("page_count > 0 必有第 1 页");

        assert_eq!(
            link,
            format!("https://cos.example.com/previews/{seed}/p1.jpg?eo-img.resize=l/2560")
        );
        assert!(!link.contains("A00123"), "★ 图号绝不能进路径：{link}");
        assert!(!link.contains("rev2"), "★ 版本号绝不能进路径：{link}");
        assert!(
            !link.contains(&f.object_key),
            "★ object_key 绝不能进路径：{link}"
        );
        assert!(
            !link.contains("p2") && !link.contains("p7"),
            "★ 只给第 1 页（形态决定），不得夹带其它页：{link}"
        );
        // 种子恒 32 位 hex ⇒ 路径成分长度固定，图号拼不进去。
        assert_eq!(seed.len(), PREVIEW_SEED_HEX_LEN);
    }

    /// `page_count <= 0` ⇒ **不给链接**（而不是给一条点不动的空串）。
    ///
    /// ⚠️ 空串会被调用方当成合法链接塞进 markdown，渲染出一个看起来能点、实际什么都不
    /// 是的链接 —— 那比「没有链接」更难被发现。
    #[test]
    fn zero_page_row_yields_no_link() {
        let urls = UrlParams {
            prefix: "previews",
            base_url: "https://cos.example.com",
            image_params: "",
        };
        assert!(urls.first_page("deadbeef", 0).is_none());
        assert!(urls.first_page("deadbeef", -1).is_none());
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
