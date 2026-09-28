//! 微信小程序 BFF / auth 域占位（2026-09-28 新增）
//!
//! **本 PR 不实现任何端点**——保留本文件仅为显式标注「未来 wx-login 在这里」，
//! 避免 reviewer 在 PR 描述里看不到「auth 占位」字样。
//!
//! ## 后续 PR：mini-program 登录
//!
//! 计划实现 `POST /api/v2/wx/iam/wx-login`（路径已对齐 wx-app 模块设计）：
//! - 入参：`{ code: String, encryptedData?: String, iv?: String }`
//! - 流程：
//!   1. `code2Session(code)` 调 `https://api.weixin.qq.com/sns/jscode2session`
//!      拿 `openid` + `session_key`（微信小程序官方接口）
//!   2. `openid → t_user` 反查（未来需新增 wx_openid 列；本字段非阻塞可分 PR 加）
//!   3. 复用 `iam::service::SessionService::complete_login` 签双 token + 写 Redis
//! - 错误码：未来加 `BIZ_WX_LOGIN_FAILED`（与现有 401xx 同段位）
//! - 鉴权：本端点挂在 v2_router nest 下，需要在 wx 域**单独**走公开路径
//!   （v2_router 中间件白名单加 `/wx/iam/wx-login`）——这是后续 PR 的范围
//!
//! 当前占位仅声明 router 占位 + 空 router：
//!
//! 见 `super::mod.rs::router()` 中 `auth::router()` merge 处。

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// 故意保留空 router（让 `super::router()` 的 merge 调用保持 0 端点状态）。
///
/// 后续 wx-login 端点会在这里添加（handlers/wx_login.rs 等）。
#[allow(dead_code)]
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
}
