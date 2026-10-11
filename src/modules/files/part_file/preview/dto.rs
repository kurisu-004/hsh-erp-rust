//! `files::part_file::preview` 的出参 VO。

use serde::Serialize;

/// `GET /api/v2/files/objects/{file_id}/preview` 的出参。
///
/// ## 三态契约（前端按 `state` 分流，不要按 HTTP 状态码）
///
/// | `state` | HTTP | `pages` | `error` | 前端应做 |
/// |---|---|---|---|---|
/// | `READY` | 200 | 长度 == `page_count`，URL 可直接 `<img src>` | `null` | 翻页展示 |
/// | `PENDING` | 200 | 空 | `null` | 等 `retry_after_ms` 毫秒后**重调本端点** |
/// | （失败） | **502** | — | — | 由信封里的 `code=21117` + `message` 呈现 |
///
/// ⚠️ **`PENDING` 走 HTTP 200 而不是 202**：本端点的信封约定是
/// `R { code, message, data }`，而 `PENDING` 是一个**正常中间态**（不是错误）。
/// 用 200 + `state` 字段表达，前端只需一个 `fetch` 循环；若用 202 会让通用
/// 请求拦截器把它当「非成功」抛异常。
///
/// ⚠️ **失败不返 `state="FAILED"` 而返 502 + 21117**：与全仓错误处理统一
/// （`AppError::into_response()` 装信封）。代价是 `FAILED` 这个值**只出现在
/// `t_file_preview.state` 里、不出现在响应里** —— 前端拿不到「上一次失败过」这个
/// 事实，它只会看到本次的 502。这是刻意的：失败是**要重试**的信号，不是要展示
/// 给用户的状态。
///
/// ## ⚠️⚠️ 前端**必须**对 21117 / 21118 停止自动重试（跨仓 / 跨端义务）
///
/// 上表「前端应做」一列只写了 `PENDING` 那一行，本节补失败那一行的**硬要求**：
///
/// - **21117**（CI 终局失败）：**不要**按 `retry_after_ms` 或任何固定间隔重调。
///   虽然下一轮调用会经 `reset_to_pending` 自愈重提（能成功），但**每一次重试都是
///   一个新的 CI job，按页计费**。正确做法是提示「渲染失败」并提供「重试」按钮
///   ——让**用户**决定要不要再花一次钱。
/// - **21118**（打 python 失败）：同样停自动重试。区别在于行仍是 `PENDING /
///   ci_job_id IS NULL`，所以**用户手动再点一次**就会补提交；而**自动**重试在
///   上游持续故障时等于无上限地烧 CI 配额。
///
/// ⚠️ 通用请求拦截器的 `retry` 逻辑若对本端点生效，会直接违反这条义务 —— 接入时
/// 需把本端点列入「失败不自动重试」白名单，并**降级到
/// `GET /objects/{id}/content`（原始 PDF）**。
#[derive(Debug, Clone, Serialize)]
pub struct PreviewOut {
    /// `READY` / `PENDING`。
    pub state: String,
    /// 总页数；`PENDING` 时为 0。
    pub page_count: i64,
    /// 各页 JPG 的绝对直链；`READY` 时长度恒等于 `page_count`，否则为空 vec。
    ///
    /// ⚠️ 这些 URL 是 **COS `previews/` 前缀下的公开读直链、永久不过期**，
    /// 拿到即可脱离本后端直接访问 —— 见 `super::mod` 的「安全取舍反转」一节。
    pub pages: Vec<String>,
    /// `PENDING` 时的建议重试间隔（毫秒）；`READY` 时为 0。
    ///
    /// 值 = 距 `next_poll_at` 的毫秒数（服务端算好，前端不必自己推算时间差）。
    pub retry_after_ms: u64,
    /// 失败原因；**当前恒为 `null`**（失败走 502 信封，见上表）。
    ///
    /// 保留该字段是为了将来引入「非致命降级」（例如某页 CI 没渲出来、整体仍可用）
    /// 时不必改响应形状 —— 那时它承载部分页面的告警，而整体 `state` 仍是 `READY`。
    pub error: Option<String>,
}

impl PreviewOut {
    /// `PENDING` 出参（页数 0、无页 URL）。
    pub fn pending(retry_after_ms: u64) -> Self {
        Self {
            state: crate::modules::files::part_file::preview::repo::state::PENDING.to_string(),
            page_count: 0,
            pages: Vec::new(),
            retry_after_ms,
            error: None,
        }
    }

    /// `READY` 出参（不重试、无错误）。
    pub fn ready(page_count: i64, pages: Vec<String>) -> Self {
        Self {
            state: crate::modules::files::part_file::preview::repo::state::READY.to_string(),
            page_count,
            pages,
            retry_after_ms: 0,
            error: None,
        }
    }
}
