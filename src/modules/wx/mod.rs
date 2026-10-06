//! 微信小程序 BFF 模块聚合（2026-09-28 新增）
//!
//! 路径：`/api/v2/wx/*`，挂在 `modules::v2_router` 的 `/wx` nest 下。
//!
//! ## 设计目标
//! - **瘦 DTO**：mini-program 卡片视图专用，剔除 `version` / 审计字段 / children
//!   嵌套，避免微信小程序首屏 >4KB 响应（实测单卡片列表 10 条 ~1.5KB JSON）
//! - **BFF 聚合**：单端点拉多维度数据，减少 mini-program HTTP 请求数
//!   （如 `/dashboard/home` 一次拉 me + 4 个计数 + 2 个今日事件）
//! - **复用 IAM 鉴权**：绝大多数端点均需 Bearer JWT，走 `v2_router` 末尾
//!   `authenticate_middleware` 统一处理。**唯一例外**是 `POST /wx/iam/wx-login`
//!   ——小程序还没有 token，需要匿名换取，故它是白名单里唯一的新增项
//!   （2026-09-29 加白名单，`auth/middleware.rs::is_public_path` +
//!   `middleware/idempotency.rs::is_public_idempotency_path` 两处必须同步改）
//!
//! ## 端点清单（2026-09-29 更新）
//! - `POST /wx/iam/wx-login` —— **公开** 企业微信小程序登录（详见 `auth.rs`）
//! - `GET  /wx/dashboard/home` —— 首页聚合（5 个 BFF 维度）
//! - `GET  /wx/parts/counts` —— 工单 4 tab 计数
//! - `GET  /wx/parts` —— 工单卡片分页
//! - `GET  /wx/parts/by-serial/{serial_no}` —— 扫码定位
//! - `GET  /wx/batches/counts?period=YYYY-MM` —— 批次 2 tab 计数
//! - `GET  /wx/batches?tab=in_progress|done&period=&page=&size=` —— 批次卡片分页
//! - `GET  /wx/worker/stats?period=YYYY-MM` —— 当月工人工作量
//!
//! 端点清单即契约（本节即全文契约载体）。
//!
//! ## 企业微信登录方案（2026-09-29 落地）
//! - 身份源 = 企业微信 **userid**（不是微信 openid）：小程序只在企业微信客户端内打开
//! - 走企业微信 `/cgi-bin/miniprogram/jscode2session`（自建应用 → 明文 userid）
//! - **仅预绑定**：`t_wx_identity` 命中才放行，未绑定直接 40107，不自动开户
//! - `session_key` 拿到即丢、不落库；不取手机号 / 头像昵称
//! - 外部 HTTP 调用在事务外；`complete_login` 在 commit 后写 Redis session
//!
//! ## 模块布局
//! - `dto.rs`     —— 入参 DTO（axum extractor 反序列化目标；2026-09-29 新增）
//! - `vo.rs`      —— 出参 VO（仅含 Serialize；不进 axum extractor）
//! - `repo.rs`    —— SQL 真源（ZST + 静态方法；不开 trait——薄聚合无业务规则）
//! - `wecom_client.rs` —— 企业微信 API 客户端（trait + Http + Noop，2026-09-29 新增）
//! - `dashboard.rs / parts.rs / batches.rs / worker.rs / auth.rs` —— 各端点 handler
//!
//! ## 事务分层
//! 除 `auth.rs::wx_login`（外部 HTTP 必须在事务外）外，全部端点 read-only：
//! `pool.acquire()` 不开事务，与 `part::handler::crud` 范式一致。

use std::sync::Arc;

use axum::Router;

use crate::shared::error::AppError;
use crate::state::AppState;

pub mod auth;
pub mod batches;
pub mod dashboard;
pub mod dto;
pub mod parts;
pub mod repo;
pub mod vo;
pub mod wecom_client;
pub mod worker;

/// 把可选 `period`（YYYY-MM）归一化：`None` → 当前月；`Some(s)` → 严格校验。
///
/// 设计：服务端 fallback 到当前月是为了让 mini-program 端不必每次拼 query 字符
/// 串；同时支持前端显式传 period（历史月份视图）。
///
/// 校验规则：
/// - 长度必须 7（`YYYY-MM`）
/// - 第 5 字节必须是 `-`
/// - 月份 ∈ `01..=12`
///
/// 2026-09-28 review #1 修复：从 batches / worker 抽到本模块共享，原地两副本
/// 删除。测试也一并合并到本模块（`#[cfg(test)] mod tests`），避免分散。
pub(crate) fn resolve_period(raw: Option<&str>) -> Result<String, AppError> {
    match raw {
        None => Ok(chrono::Local::now().format("%Y-%m").to_string()),
        Some(s) => {
            if s.len() != 7 || s.as_bytes()[4] != b'-' {
                return Err(AppError::validation(format!(
                    "period {s:?} 格式非法（要求 YYYY-MM）"
                )));
            }
            let month: u32 = s[5..7]
                .parse()
                .map_err(|_| AppError::validation(format!("period {s:?} 月份非法")))?;
            if !(1..=12).contains(&month) {
                return Err(AppError::validation(format!("period {s:?} 月份非法")));
            }
            Ok(s.to_string())
        }
    }
}

/// `/api/v2/wx/*` 入口 router 工厂。
///
/// 注册顺序敏感（axum 静态段优先于 catch-all）：
/// 1. `dashboard::router()` —— `/dashboard/*`
/// 2. `parts::router()`     —— `/parts/*`（含 `counts`/`by-serial/{serial_no}`/list）
/// 3. `batches::router()`   —— `/batches/*`
/// 4. `worker::router()`    —— `/worker/*`
/// 5. `auth::router()`      —— `/iam/wx-login`（2026-09-29 起为真实端点）
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .nest("/dashboard", dashboard::router())
        .nest("/parts", parts::router())
        .nest("/batches", batches::router())
        .nest("/worker", worker::router())
        // 2026-09-29：企业微信小程序登录（原为空 router 占位）
        .nest("/iam", auth::router())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_period_defaults_to_current_month() {
        let p = resolve_period(None).unwrap();
        assert_eq!(p.len(), 7);
        assert_eq!(p.as_bytes()[4], b'-');
    }

    #[test]
    fn resolve_period_accepts_valid() {
        assert_eq!(resolve_period(Some("2026-09")).unwrap(), "2026-09");
        assert_eq!(resolve_period(Some("2025-12")).unwrap(), "2025-12");
    }

    #[test]
    fn resolve_period_rejects_invalid() {
        assert!(resolve_period(Some("2026-9")).is_err()); // 月份 1 位
        assert!(resolve_period(Some("2026/09")).is_err()); // 分隔符错
        assert!(resolve_period(Some("2026-13")).is_err()); // 月份 13
        assert!(resolve_period(Some("2026-00")).is_err()); // 月份 0
        assert!(resolve_period(Some("26-09")).is_err()); // 年份 2 位
    }
}
