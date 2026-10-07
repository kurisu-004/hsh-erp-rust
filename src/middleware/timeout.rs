//! 2026-10-03 新增：按路径分档的请求级超时中间件（替换 `tower_http::timeout::TimeoutLayer`）。
//!
//! ## 为什么需要它
//! `/api/v2` 整个 nest 原本统一套 30s 的 `TimeoutLayer`。批量图纸打印（20 件/批，
//! 前端并发 3 批）合法耗时数分钟、且真正执行方是 Python 后端，30s 会把批量打印
//! **必然打断成 408**——功能直接不可用。本中间件按路径分两档：
//! 打印路径走 [`AppConfig::print_request_timeout_seconds`](crate::infra::config::AppConfig::print_request_timeout_seconds)（缺省 660s），
//! 其余路径仍走 [`AppConfig::request_timeout_seconds`](crate::infra::config::AppConfig::request_timeout_seconds)（缺省 30s，行为不变）。
//!
//! ## 为什么超时响应要带信封而不是空 body
//! 前端打印请求是 `responseType: 'blob'`，blob 错误分支（`frontend/src/api/http.ts`）
//! 会把错误 body 读成文本再 `JSON.parse`，解析成功才读出 `code` / `message` 呈现
//! 可读原因。空 body 直接 parse 失败 → 只能弹一句泛化的网络错误。本层用
//! [`AppError`] 的 `IntoResponse` 装标准 `{code, message, data}` 信封
//! （`code = code::REQUEST_TIMEOUT` = 40800），与全仓错误响应同形。
//!
//! ## 分档判定
//! 判定逻辑在 [`is_print_path`]，**只按路径、不按 method**（打印 4 端点里
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

/// 打印路径判定：命中则走长档超时（`print_request_timeout_seconds`）。
///
/// 命中的 2 条打印路径（`{id}` 为变段）：
/// - `GET /api/v2/parts/{id}/print-drawing`
/// - `POST /api/v2/parts/print-drawing-batch`（静态段，无 `{id}`）
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
/// 误判成打印路径 = 一个普通端点白拿 660s 超时（慢请求不再被兜底挡住）；
/// 误判成普通路径 = 批量打印被 30s 砍断（功能不可用）。所以一律用
/// 「首段 + 段数 + 尾段字面量」的组合匹配，**不用裸后缀匹配**：
/// `/delivery-notes/{id}/print-preview`、`/assemblies/{id}/print` 之类
/// 都不能命中。
pub fn is_print_path(path: &str) -> bool {
    let stripped = path.strip_prefix("/api/v2").unwrap_or(path);
    // 按 `/` 切段并丢掉空段（容忍尾斜杠）
    let segs: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        // `POST /parts/print-drawing-batch`（静态段，2 段）
        ["parts", "print-drawing-batch"] => true,
        // `GET /parts/{id}/print-drawing`（3 段，中间是变段 id）
        ["parts", id, "print-drawing"] if !id.is_empty() => true,
        _ => false,
    }
}

/// 按路径选择超时档位（**纯函数、零 IO**，便于单测直接喂字面量）。
///
/// 打印路径 → `print_seconds`；其余 → `default_seconds`。
/// 参数显式传入而不是收 `&AppConfig`：单测不构造 `AppConfig`（它要扫 JWT 公钥
/// 目录、连 `.env`，成本与不稳定性都高）。
pub fn select_timeout(path: &str, default_seconds: u64, print_seconds: u64) -> Duration {
    let seconds = if is_print_path(path) {
        print_seconds
    } else {
        default_seconds
    };
    Duration::from_secs(seconds)
}

/// axum 中间件：按 [`is_print_path`] 分档的请求级超时。
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
                "请求超时（按路径分档：打印路径长档、其余通用档）"
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
            assert!(is_print_path(p), "应命中打印长档：{p}");
        }
    }

    /// 集成测试形态（`test_app` 直接挂 `v2_router`，无 `/api/v2` 前缀）同样命中。
    #[test]
    fn print_paths_are_recognized_without_api_v2_prefix() {
        for p in ["/parts/1/print-drawing", "/parts/print-drawing-batch"] {
            assert!(is_print_path(p), "应命中打印长档（无前缀形态）：{p}");
        }
    }

    /// 相近的**非**打印路径一律不命中（拿不到 660s 长档）。
    ///
    /// 重点是前缀相似 / 后缀相近 / 段数相近的组合：判定的目的是「精确命中 4 条」，
    /// 任何漏进来都会让普通端点静默继承长超时。
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
            assert!(!is_print_path(p), "不应命中打印长档：{p}");
        }
    }

    /// 分档：打印路径拿打印档，其余拿通用档（30s 不变）。
    #[test]
    fn select_timeout_picks_long_tier_only_for_print_paths() {
        assert_eq!(
            select_timeout("/api/v2/parts/print-drawing-batch", 30, 660),
            Duration::from_secs(660)
        );
        assert_eq!(
            select_timeout("/api/v2/parts/1/print-drawing", 30, 660),
            Duration::from_secs(660)
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
