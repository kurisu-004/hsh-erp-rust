//! `files::part_file::preview` 的出参 VO 与入参 query。

use serde::{Deserialize, Serialize};

use crate::shared::error::AppError;

/// `GET /api/v2/files/objects/{file_id}/preview` 的 query 参数。
///
/// ## ⚠️ `size` 刻意是 `Option<String>` 而不是枚举
///
/// 枚举放在**提取器**这一层会让 `?size=bogus` 走 `QueryRejection` ⇒ axum 返
/// **纯文本 400**（不进 `R<T>` 信封）。前端契约（`PreviewOut` 的 doc）是
/// 「一切错误走信封 + 业务码」，故解析放在 handler 里手动做（[`PreviewSize::parse`]），
/// 让非法值走 `AppError::validation` ⇒ 422 + 40001 信封。
///
/// ⚠️ **已知残余**：`?size=a&size=b` 这类**重复键**仍会在提取器层失败
/// （serde_urlencoded 对单值字段遇重复键报错）⇒ 纯文本 400。这是平台级行为，
/// 不实现 `RawQuery` 兜（多写一层仅为一个病态输入），已登记在
/// `preview/mod.rs` 的已知偏差节。
#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    /// 投放档位（`view` / `thumb` / `full`，缺省 `view`）。大小写敏感。
    pub size: Option<String>,
}

/// 预览图投放档位（2026-10-12 新增）。
///
/// 由 query `?size=` 选择，决定 `pages[]` 尾部拼哪一档图片即时处理参数
/// （参数值来自 `CosConfig` 的两个 `preview_image_*_params` 字段）：
///
/// | 档位 | 语义 | URL 参数 |
/// |---|---|---|
/// | `View` | 单张查看（缺省） | `preview_image_view_params` |
/// | `Thumb` | 列表缩略图 | `preview_image_thumb_params` |
/// | `Full` | 原始全尺寸 | **恒无**（不读配置） |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewSize {
    /// 单张查看（缺省档）。
    View,
    /// 列表缩略图。
    Thumb,
    /// 原始全尺寸（URL 不带任何 query）。
    Full,
}

impl PreviewSize {
    /// 解析 query 里的 `size` 原始值。
    ///
    /// 规则：先 `trim`；`None` / 空串 ⇒ [`Self::View`]；`"view"` / `"thumb"` /
    /// `"full"` **精确小写**匹配；其余 ⇒ 40001（HTTP 422）——
    /// **大小写敏感**是刻意的：`"VIEW"` 落进「非法」而不是被静默接受，
    /// 避免前端两种拼法并存后没人知道哪个是真契约。
    pub fn parse(raw: Option<&str>) -> Result<Self, AppError> {
        match raw.map(str::trim) {
            None | Some("") => Ok(Self::View),
            Some("view") => Ok(Self::View),
            Some("thumb") => Ok(Self::Thumb),
            Some("full") => Ok(Self::Full),
            Some(other) => Err(AppError::validation(format!(
                "size 非法（可选：view / thumb / full）：{other:?}"
            ))),
        }
    }

    /// 本档位对应的图片处理参数。
    ///
    /// `Full` 恒返 `""`（**不读两个入参**）—— 原始全尺寸直链不带 query 是本档的
    /// 定义，不是「配置为空」的巧合。
    pub fn image_params<'a>(self, view: &'a str, thumb: &'a str) -> &'a str {
        match self {
            Self::View => view,
            Self::Thumb => thumb,
            Self::Full => "",
        }
    }
}

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

/// 2026-10-12 新增：`size` 档位解析的纯函数单测（零 IO，`cargo test --lib preview` 可跑）。
#[cfg(test)]
mod tests {
    use super::PreviewSize;

    /// 三档命中 + 大小写敏感 + 非法值报 40001。
    #[test]
    fn parse_accepts_exactly_three_lowercase_tiers() {
        assert_eq!(PreviewSize::parse(Some("view")).unwrap(), PreviewSize::View);
        assert_eq!(
            PreviewSize::parse(Some("thumb")).unwrap(),
            PreviewSize::Thumb
        );
        assert_eq!(PreviewSize::parse(Some("full")).unwrap(), PreviewSize::Full);

        // ⚠️ 精确小写：大写是非法值（不是「宽容接受」）—— 见 parse 的 doc。
        for bad in ["VIEW", "Thumb", "bogus", "v"] {
            let err = PreviewSize::parse(Some(bad)).expect_err("非法档位必须报错");
            assert_eq!(err.code(), 40001, "{bad:?} 应走校验错误码");
            assert_eq!(
                err.http_status(),
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "{bad:?} 应为 HTTP 422"
            );
        }
    }

    /// `None` / 空串 / 纯空白 ⇒ 缺省 `View`（`trim` 后再判空）。
    #[test]
    fn parse_defaults_to_view_on_missing_or_blank() {
        assert_eq!(PreviewSize::parse(None).unwrap(), PreviewSize::View);
        assert_eq!(PreviewSize::parse(Some("")).unwrap(), PreviewSize::View);
        assert_eq!(PreviewSize::parse(Some("   ")).unwrap(), PreviewSize::View);
        assert_eq!(
            PreviewSize::parse(Some(" view ")).unwrap(),
            PreviewSize::View
        );
    }

    /// `Full` 档恒无参数（不读两个入参），`View` / `Thumb` 各取自己那一份。
    #[test]
    fn image_params_maps_tier_to_configured_param() {
        let view = "eo-img.resize=l/2560";
        let thumb = "eo-img.resize=w/480";
        assert_eq!(PreviewSize::View.image_params(view, thumb), view);
        assert_eq!(PreviewSize::Thumb.image_params(view, thumb), thumb);
        assert_eq!(
            PreviewSize::Full.image_params(view, thumb),
            "",
            "full 档不得带上任何参数（即使两项配置都非空）"
        );
    }
}
