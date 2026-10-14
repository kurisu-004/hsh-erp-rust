//! 2026-10-03 新增：按路径分档的请求级超时中间件（替换 `tower_http::timeout::TimeoutLayer`）。
//!
//! ## 分档：按路径在两个超时档之间选
//!
//! `/api/v2` 整个 nest 套这一层：命中**长档白名单**的路径等
//! [`AppConfig::print_request_timeout_seconds`](crate::infra::config::AppConfig::print_request_timeout_seconds)（缺省 660s），
//! 其余路径仍走 [`AppConfig::request_timeout_seconds`](crate::infra::config::AppConfig::request_timeout_seconds)（缺省 30s，行为不变）。
//!
//! ⚠️ **超时是「把 handler 的 future 整个 drop 掉」**（`tokio::time::timeout` 到点直接
//! 放弃那个 future，不等它收尾）⇒ 一条路径若「在同一个请求内串着做完一串慢操作」，
//! 半路超时**已经产生的对外副作用会连同它本该落库的痕迹一起消失**，而调用方只看到
//! 408。这正是本表存在的理由：凡是这类路径都必须登记进来。
//!
//! ## 长档白名单（**只有 3 条**，判定见 [`is_long_timeout_path`]）
//!
//! | 路径 | 为什么它需要长档 |
//! |---|---|
//! | `GET /api/v2/parts/{id}/print-drawing`、<br>`POST /api/v2/parts/print-drawing-batch` | 批量图纸打印（20 件/批、前端并发 3 批）合法耗时数分钟，且**真正执行方是 Python 后端**（还必须在它自己的 600s 超时之内回来）。30s 会把批量打印**必然打断成 408** —— 功能直接不可用 |
//! | `POST /api/v2/cron/wecom-daily/send` | 在**同一个请求内**做「取数（恒 ≤ 6 条 SQL）+ 最多 `max_chunks`（上界 20）次**顺序 HTTP**」。单片超时 5s（`wecom::sender::HTTP_TIMEOUT_MS`）⇒ 最坏 ≈ 100s。30s 会把**已经推到群里的前 k 片**连同本该落库的那行流水一起 drop 掉 ⇒ `docs/api/wecom-daily.md` 的「任一片失败 ⇒ 整条 FAILED + `chunk_sent` 记录已成功数」这条不变量被静默破坏，且运维在 `GET /logs` 里**看不到任何痕迹** |
//!
//! ⚠️ **wecom 那条与打印两条共用同一档、没为它单开第三档**：`max_chunks` 上界 20 ×
//! 单片 5s = 100s，660s 有充足余量；新开一档要同时动 `infra/config.rs` +
//! `.env.example`，收益不抵。
//!
//! ⚠️ **`/cron/wecom-daily/preview` 刻意**不命中：它只渲染不投递，恒 ≤ 6 条 SQL，
//! 远小于 30s。
//!
//! ## 为什么超时响应要带信封而不是空 body
//! 前端打印请求是 `responseType: 'blob'`，blob 错误分支（`frontend/src/api/http.ts`）
//! 会把错误 body 读成文本再 `JSON.parse`，解析成功才读出 `code` / `message` 呈现
//! 可读原因。空 body 直接 parse 失败 → 只能弹一句泛化的网络错误。本层用
//! [`AppError`] 的 `IntoResponse` 装标准 `{code, message, data}` 信封
//! （`code = code::REQUEST_TIMEOUT` = 40800），与全仓错误响应同形。
//!
//! ## 分档判定
//! 判定逻辑在 [`is_long_timeout_path`]，**只按路径、不按 method**（白名单里
//! `print-drawing` 是 GET，其余是 POST，同一路径的档位与 method 无关）。判定必须是
//! 精确的前缀 + 段数 + 段值组合：任何「结尾是 `/print` 的路径」都不能拿到 660s，
//! 否则将来别的域加个短端点就会静默继承长超时。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::shared::error::{AppError, code};
use crate::state::AppState;

/// 长耗时路径判定：命中则走长档超时（`print_request_timeout_seconds`）。
///
/// 白名单 3 条：
/// - `GET /api/v2/parts/{id}/print-drawing`（`{id}` 为变段）
/// - `POST /api/v2/parts/print-drawing-batch`（静态段，无 `{id}`）
/// - `POST /api/v2/cron/wecom-daily/send`（静态段，3 段，**2026-10-19 新增**）
///
/// ⚠️ 2026-10-08：送货单的两条打印路径（`{id}/print` / `{id}/print-labels`）随
/// 打印链路下线一并移除，故本判定表不再有送货单分支。历史上它是 `["delivery-notes",
/// id, action]` 三段匹配；域平移到 `com/delivery/note` 后路径变成 5 段，本就不会
/// 命中 —— 与其留一段永不生效的分支，不如删干净。
///
/// ## 路径前缀为什么要 strip
/// 与 `crate::auth::middleware::is_public_path` 同一处理：生产是 `/api/v2` nest，
/// `req.uri().path()` 带完整前缀；但集成测试的 `test_app` 直接挂 `v2_router`
/// （不 nest `/api/v2`），路径是 `/iam/login` 这种裸路径。两种形态都要命中，
/// 因此先 strip 前缀再判定，`unwrap_or(path)` 兜住「没有前缀」的单测形态
/// （该文件已有同款先例）。
///
/// ## 判定精度（误判代价不对称）
/// 误判成长档 = 一个普通端点白拿 660s 超时（慢请求不再被兜底挡住）；
/// 误判成普通档 = 批量打印被 30s 砍断、企业微信日报被砍在半路（功能不可用 + 流水
/// 丢失）。所以一律用「首段 + 段数 + 尾段字面量」的组合匹配，**不用裸后缀匹配**：
/// `/delivery-notes/{id}/print-preview`、`/assemblies/{id}/print`、
/// `/cron/wecom/send`、`/cron/wecom-daily/send/extra` 之类都不能命中。
///
/// ⚠️ **本判定被 [`crate::middleware::idempotency`] 复用**（长耗时路径的响应体不该
/// 整份缓冲进 Redis 缓存）⇒ 加一条路径时**两处行为一起变**：那边也会跳过一个端点的
/// 幂等缓存。理由与代价见该文件里的同名闸门注释。
///
/// ## `cron` 这条与邮件的 `/cron/mail-digest/send` **不对称**是刻意的
/// 邮件是**一次** SMTP 往返（秒级，30s 有充足余量）；wecom 是 **N 次串行 HTTP**
/// （`max_chunks` 上界 20，单片 5s）。二者相差一个量级，把邮件也拉进长档只会让
/// 一个慢 SMTP 服务器长期占住请求位。
pub fn is_long_timeout_path(path: &str) -> bool {
    let stripped = path.strip_prefix("/api/v2").unwrap_or(path);
    // 按 `/` 切段并丢掉空段（容忍尾斜杠）
    let segs: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        // `POST /parts/print-drawing-batch`（静态段，2 段）
        ["parts", "print-drawing-batch"] => true,
        // `GET /parts/{id}/print-drawing`（3 段，中间是变段 id）
        ["parts", id, "print-drawing"] if !id.is_empty() => true,
        // `POST /cron/wecom-daily/send`（静态段，3 段）—— 只送不渲，故**不含**
        // `/cron/wecom-daily/preview`（只渲染、恒 ≤ 6 条 SQL，30s 绰绰有余）。
        ["cron", "wecom-daily", "send"] => true,
        _ => false,
    }
}

/// 按路径选择超时档位（**纯函数、零 IO**，便于单测直接喂字面量）。
///
/// 长档路径 → `print_seconds`；其余 → `default_seconds`。
/// 参数显式传入而不是收 `&AppConfig`：单测不构造 `AppConfig`（它要扫 JWT 公钥
/// 目录、连 `.env`，成本与不稳定性都高）。
pub fn select_timeout(path: &str, default_seconds: u64, print_seconds: u64) -> Duration {
    let seconds = if is_long_timeout_path(path) {
        print_seconds
    } else {
        default_seconds
    };
    Duration::from_secs(seconds)
}

/// axum 中间件：按 [`is_long_timeout_path`] 分档的请求级超时。
///
/// 超时后返回 `408` + 标准信封（`code = code::REQUEST_TIMEOUT`），不手拼 JSON。
///
/// ## `enabled=false` 的取舍
/// 中间件本身不读 `python_backend.enabled`：打印转发是否走真实 Python 后端由
/// handler 决定，超时档位只管「HTTP 层允许等多久」，两者正交。
pub async fn timeout_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let timeout = select_timeout(
        &path,
        state.config.request_timeout_seconds,
        state.config.print_request_timeout_seconds,
    );
    match tokio::time::timeout(timeout, next.run(req)).await {
        Ok(resp) => resp,
        Err(_elapsed) => {
            tracing::warn!(
                path = %path,
                timeout_secs = timeout.as_secs(),
                "请求超时（按路径分档：长耗时路径长档、其余通用档）"
            );
            AppError::biz_with_status(
                code::REQUEST_TIMEOUT,
                "请求超时",
                StatusCode::REQUEST_TIMEOUT,
            )
            .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2 条打印路径各自命中（带 `/api/v2` 前缀的生产形态）。
    #[test]
    fn print_paths_are_recognized() {
        for p in [
            "/api/v2/parts/1234567890/print-drawing",
            "/api/v2/parts/print-drawing-batch",
        ] {
            assert!(is_long_timeout_path(p), "应命中长档：{p}");
        }
    }

    /// 集成测试形态（`test_app` 直接挂 `v2_router`，无 `/api/v2` 前缀）同样命中。
    #[test]
    fn print_paths_are_recognized_without_api_v2_prefix() {
        for p in ["/parts/1/print-drawing", "/parts/print-drawing-batch"] {
            assert!(is_long_timeout_path(p), "应命中长档（无前缀形态）：{p}");
        }
    }

    /// ★ 企业微信日报 `/send` 命中长档（2026-10-19 新增的白名单条目）。
    ///
    /// **这条钉的是「零流水」事故**：本路径在**同一个请求内**串着做「取数 + 最多
    /// `max_chunks` 次顺序 HTTP」（单片 5s、上界 20 ⇒ 最坏 ≈ 100s）。拿 30s 通用档时，
    /// 超时把 handler 的 future 整个 drop 掉 ⇒ **已经推到群里的前 k 片连同流水一起
    /// 消失**，而 `GET /logs` 里查不到任何痕迹（违背 `docs/api/wecom-daily.md` 的
    /// 「任一片失败 ⇒ 整条 FAILED + `chunk_sent` 记录已成功数」不变量）。
    /// 集成侧同款断言见 `tests/wecom_daily/timeout_guard.rs`。
    #[test]
    fn wecom_daily_send_path_is_recognized() {
        for p in [
            "/api/v2/cron/wecom-daily/send",
            // 集成测试形态（`test_app` 直挂 `v2_router`，无 `/api/v2` 前缀）
            "/cron/wecom-daily/send",
            // 容忍尾斜杠（切段时丢空段）
            "/api/v2/cron/wecom-daily/send/",
        ] {
            assert!(
                is_long_timeout_path(p),
                "日报推送路径必须命中长档，否则超时会把已推送的分片变成零流水：{p}"
            );
        }
    }

    /// `/cron/wecom-daily/preview` **不**命中长档：它只渲染、不投递，恒 ≤ 6 条 SQL，
    /// 远小于 30s —— 白拿 660s 只会让一个真卡住的预览迟迟不被兜底挡住。
    #[test]
    fn wecom_daily_preview_path_stays_on_the_default_tier() {
        for p in [
            "/api/v2/cron/wecom-daily/preview",
            "/cron/wecom-daily/preview",
        ] {
            assert!(
                !is_long_timeout_path(p),
                "只渲染不投递的预览不该命中长档：{p}"
            );
        }
    }

    /// `cron/wecom-daily` 家族里**名字相近但不是长档**的路径一律不命中。
    ///
    /// 三种误判形态：尾段多一个字符（`sendx`）、父段少一段（`wecom/send`）、尾段后多
    /// 一段（`send/extra`）。任一漏进来都会让普通端点静默继承 660s。
    #[test]
    fn cron_wecom_daily_lookalikes_are_rejected() {
        for p in [
            "/api/v2/cron/wecom-daily/sendx",
            "/api/v2/cron/wecom/send",
            "/api/v2/cron/wecom-daily/send/extra",
            "/api/v2/cron/wecom-daily/sendall",
            // 邮件那条**刻意不对称**（单次 SMTP 往返，秒级）：见函数 doc
            "/api/v2/cron/mail-digest/send",
            "/api/v2/cron/wecom-daily",
        ] {
            assert!(!is_long_timeout_path(p), "不应命中长档：{p}");
        }
    }

    /// 相近的**非**打印路径一律不命中（拿不到 660s 长档）。
    ///
    /// 重点是前缀相似 / 后缀相似 / 段数相近的组合：判定的目的是「精确命中白名单里的
    /// 几条」，任何漏进来都会让普通端点静默继承长超时。
    #[test]
    fn lookalike_non_print_paths_are_rejected() {
        for p in [
            // 前缀相似、后缀不同
            "/api/v2/parts/1/print-drawing-preview",
            "/api/v2/parts/1/print",
            "/api/v2/parts/1/print-drawing-batch",
            // 别的域将来若加 /print，不得被裸后缀匹配捞进来
            "/api/v2/assemblies/1/print",
            "/api/v2/outsource-shipments/1/print",
            "/api/v2/whatever/print",
            "/print",
            // 段数不足（缺变段 id / 缺动作段）
            "/api/v2/parts/print",
            "/api/v2/parts/print-drawing",
            // 其它无关路径
            "/api/v2/parts/1/update",
            "/api/v2/parts/batch",
            "/api/v2/parts",
            "/api/v2/health",
            "/ws/dashboard",
            "/",
        ] {
            assert!(!is_long_timeout_path(p), "不应命中长档：{p}");
        }
    }

    /// 分档：长档路径拿长档（660s），其余拿通用档（30s 不变）。
    #[test]
    fn select_timeout_picks_long_tier_only_for_long_timeout_paths() {
        assert_eq!(
            select_timeout("/api/v2/parts/print-drawing-batch", 30, 660),
            Duration::from_secs(660)
        );
        assert_eq!(
            select_timeout("/api/v2/parts/1/print-drawing", 30, 660),
            Duration::from_secs(660)
        );
        // 企业微信日报推送：30s 通用档会砍掉 ≈100s 的最坏路径并丢掉那一行流水。
        assert_eq!(
            select_timeout("/api/v2/cron/wecom-daily/send", 30, 660),
            Duration::from_secs(660)
        );
        assert_eq!(
            select_timeout("/api/v2/cron/wecom-daily/preview", 30, 660),
            Duration::from_secs(30)
        );
        assert_eq!(
            select_timeout("/api/v2/parts/1", 30, 660),
            Duration::from_secs(30)
        );
        assert_eq!(
            select_timeout("/api/v2/parts/1/print-preview", 30, 660),
            Duration::from_secs(30)
        );
    }

    /// 超时响应带标准信封（408 + `code::REQUEST_TIMEOUT`）——前端 blob 错误分支要
    /// 靠这个信封 JSON.parse 出可读原因，空 body 会让它退化成泛化网络错误。
    #[tokio::test]
    async fn timeout_response_carries_envelope() {
        use axum::body::to_bytes;

        let err = AppError::biz_with_status(
            code::REQUEST_TIMEOUT,
            "请求超时",
            StatusCode::REQUEST_TIMEOUT,
        );
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);

        let bytes = to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("读取超时响应 body");
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).expect("超时响应必须是可解析的 JSON 信封");
        assert_eq!(body["code"], code::REQUEST_TIMEOUT);
        assert!(
            body["message"].as_str().unwrap().contains("请求超时"),
            "message 应含可读原因，实际：{body}"
        );
        assert!(body.get("data").is_some(), "信封必须含 data 字段：{body}");
    }
}
