//! wx::login 子模块 service 层 —— 业务逻辑（ZST + 静态方法）
//!
//! 2026-10-11 自旧 `src/modules/wx/auth.rs` 平铺实现迁入（语义逐字保留，只搬位置
//! + 把「投影 iam 中间值 → wx VO」独立成 [`WxLoginService::project_login`]）。
//!
//! ## 本层负责两件事（都很薄，故不需要 trait）
//! 1. [`WxLoginService::exchange_wecom_identity`] —— **事务外**的外部 HTTP + 入参
//!    校验 + corpid 比对 + userid 归一化
//! 2. [`WxLoginService::project_login`] —— 把 iam 域的 `LoginResponse` **投影**成本
//!    子模块的 [`WxLoginOut`]（砍掉 `is_active` / `shelf_ids` / `menus`）
//!
//! ## ⚠️ 本层**不知道事务**
//! 仓库硬约束：事务边界在 handler（`pool.begin()` / `tx.commit()`），service 收
//! `&state` 而不是 `&mut PgConnection`。中间那段「查绑定 + 签 token」是纯 iam 域
//! service 编排，故留在 handler 而不是硬塞进本层。

use std::sync::Arc;

use crate::modules::iam::vo::LoginResponse;
use crate::shared::error::{AppError, code};
use crate::state::AppState;

use super::dto::WxLoginRequest;
use super::vo::{WxLoginOut, WxLoginUserOut};

/// 换到的企业微信身份（**已归一化**，`corp_id` 已与本地配置比对通过）。
///
/// 只在事务外产生，是「开事务」之前必须先拿齐的两项入参。
#[derive(Debug, Clone)]
pub struct WeComIdentity {
    /// 与本地 `config.wecom.corpid` **已比对一致**的企业 ID（非空）
    pub corp_id: String,
    /// 归一化后的企业微信 userid（trim + **转小写**；非空）
    pub wx_user_id: String,
}

/// `wx::login` service（ZST + 静态方法，与 `prod::process_design` 范本一致）。
pub struct WxLoginService;

impl WxLoginService {
    /// **事务外**一步：把小程序 code 换成「可比对的系统账号定位键」。
    ///
    /// 流程（与旧 `auth.rs` 的事务外段逐字同序）：
    /// 1. `req.validate()`（trim + 非空 + ≤512 字节）
    /// 2. `state.wecom.code_to_session(&code)` —— 外部 HTTP，**不持 PG 连接**
    /// 3. 比对 `sess.corp_id == state.config.wecom.corpid`（防跨企业串号；
    ///    不等 → **40107**，注意与「未绑定」同码：对外不区分这两者，避免泄露
    ///    「本企业存在这个 userid 但没绑」）
    /// 4. userid 转小写（企微 userid 不区分大小写；绑定表存小写），空 → 40106
    ///
    /// `session_key` 在 `HttpWeComClient` 内部反序列化瞬间即被丢弃，此处拿到的
    /// `WeComSession` 只有 `corp_id` + `user_id` 两个字段 —— 结构上就无处落库。
    pub async fn exchange_wecom_identity(
        state: &Arc<AppState>,
        req: &WxLoginRequest,
    ) -> Result<WeComIdentity, AppError> {
        // 1. 入参校验（trim + 非空 + ≤512 字节）
        let code = req.validate()?;

        // 2. 外部 HTTP（事务外）
        let sess = state.wecom.code_to_session(&code).await?;

        // 3. 防跨企业串号：配置未启用时 corpid 为空 → 已在第 2 步被
        //    `NoopWeComClient` 拒掉（40109），到这里 corpid 必非空。
        let expected_corp = state.config.wecom.corpid.trim();
        if expected_corp.is_empty() || sess.corp_id.trim() != expected_corp {
            // ⚠️ 日志只打返回长度，**不打 corpid 本身**（它是企业标识，不是 secret，
            // 但排查时长度足够区分「空返回」与「串号」）。
            tracing::warn!(
                returned_corp_len = sess.corp_id.len(),
                "wx-login: 企微返回 corpid 与本地配置不符，拒绝登录"
            );
            return Err(AppError::biz(
                code::BIZ_WX_NOT_BOUND,
                "该企业微信账号未绑定系统账号，请联系管理员",
            ));
        }

        // 4. userid 归一化（企微 userid 不区分大小写；绑定表存小写）
        let wx_user_id = sess.user_id.trim().to_lowercase();
        if wx_user_id.is_empty() {
            return Err(AppError::biz(
                code::BIZ_WX_LOGIN_FAILED,
                "企业微信登录失败：返回的 userid 为空",
            ));
        }

        Ok(WeComIdentity {
            corp_id: expected_corp.to_string(),
            wx_user_id,
        })
    }

    /// iam 域 `LoginResponse` → [`WxLoginOut`] 投影（**本子模块 VO 不复用 iam 结构**）。
    ///
    /// 只取 4 个用户字段：`id` / `username` / `full_name` / `roles`。iam 侧的
    /// `is_active` / `shelf_ids` / `menus` **一律不带**（理由见
    /// [`super::vo`] 模块 doc 的「为什么砍掉 4 个字段」表）。
    ///
    /// iam 域的 `LoginResponse` 在这里是**中间值**：`complete_login` 的返回类型
    /// 是它，我们只借它的 token 与 4 个用户字段，不把它当响应类型透出。
    pub fn project_login(resp: LoginResponse) -> WxLoginOut {
        WxLoginOut {
            token: resp.token,
            refresh_token: resp.refresh_token,
            user: WxLoginUserOut {
                id: resp.user.id,
                username: resp.user.username,
                full_name: resp.user.full_name,
                roles: resp.user.roles,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::iam::vo::CurrentUserOut;

    fn login_response() -> LoginResponse {
        LoginResponse {
            token: "T".into(),
            refresh_token: "R".into(),
            user: CurrentUserOut {
                id: 42,
                username: "u".into(),
                full_name: "n".into(),
                is_active: true,
                roles: vec!["MANAGER".into()],
                shelf_ids: vec!["7".into()],
                menus: Vec::new(),
            },
        }
    }

    /// 投影必须**只**搬 4 个用户字段，iam 侧那 3 个 Web 端字段不得泄漏进响应结构。
    #[test]
    fn project_login_drops_web_only_user_fields() {
        let out = WxLoginService::project_login(login_response());
        assert_eq!(out.token, "T");
        assert_eq!(out.refresh_token, "R");
        assert_eq!(out.user.id, 42);
        assert_eq!(out.user.username, "u");
        assert_eq!(out.user.full_name, "n");
        assert_eq!(out.user.roles, vec!["MANAGER".to_string()]);

        // 结构性断言：序列化后的 JSON 顶层只有 3 键、user 只有 4 键。
        let v: serde_json::Value = serde_json::to_value(&out).expect("serialize WxLoginOut");
        let obj = v.as_object().expect("object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["refresh_token", "token", "user"]);

        let user = obj
            .get("user")
            .and_then(|u| u.as_object())
            .expect("user obj");
        let mut ukeys: Vec<&str> = user.keys().map(String::as_str).collect();
        ukeys.sort_unstable();
        assert_eq!(ukeys, vec!["full_name", "id", "roles", "username"]);

        // 雪花 id 走 JSON string（防 JS 精度截断）
        assert_eq!(user.get("id").and_then(|i| i.as_str()), Some("42"));
    }
}
