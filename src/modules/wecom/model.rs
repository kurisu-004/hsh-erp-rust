//! wecom 域表行模型（`T*` = 域 owned 表的**全列** `FromRow` 投影）
//!
//! 仓内惯例：`pub struct T*` 全部 derive `sqlx::FromRow`、全列投影、零例外。
//! 本域 owned 的表只有一张 [`TWeComWebhookRow`] ← `t_wecom_webhook`（群机器人 webhook 实体）。
//!
//! ## `encrypted_url` 为什么在**模型**里而不是只在 repo 的私有 row 里
//!
//! 因为**全列投影就是这个字段存在的唯一理由**：`/test` 探针与 `resolve_target` 都必须
//! 把密文取回来解开成明文 URL 才能真发请求（出参侧的屏蔽在
//! [`crate::modules::wecom::dto::WebhookOut::from_row`] 做，**不是**在这里）。
//!
//! ## ⚠️ 与 `wx::wecom_client.rs` **同名不同物**
//!
//! 本域是「定时任务」菜单下的企业微信**群机器人 webhook**（裸 HTTP POST、不需任何
//! 凭据）；`src/modules/wx/wecom_client.rs` 是**小程序登录**客户端（`gettoken` /
//! `jscode2session`，需 corpid / corpsecret）。两者除「都是打企业微信的域名」外零交集，
//! 不要把本域的 webhook URL 当成那里的 access_token 看待，也不要在两边共用客户端。

use chrono::NaiveDateTime;

/// `t_wecom_webhook` 全列投影（群机器人 webhook 实体；软删 + OCC）。
///
/// ⚠️ **刻意不 derive `Debug`，手写一个屏蔽 `encrypted_url`**：derive 版会让任何一处
/// `{row:?}` / `dbg!(&row)` 把密文原文写进日志，而本仓对含凭据的类型一律走手写
/// `Debug`（`infra::crypto` 侧 `MailAccountSecret`、`scheduled_task::model::TMailAccount`
/// 同一手法）—— 不靠「调用方记得别打印」这条约定。
///
/// ⚠️ 更要紧的一层：`encrypted_url` 是**可逆**密文（`crypto::open` 能取回明文），明文里
/// 的 `key=` 就是企业微信的 bearer 凭证 ⇒ 密文与明文的泄漏后果几乎等价。
#[derive(Clone, sqlx::FromRow)]
pub struct TWeComWebhookRow {
    pub id: i64,
    pub version: i32,
    /// 自定义名称（管理端展示用，如「管理层群机器人」）。未软删行内唯一。
    pub name: String,
    /// webhook URL 密文（AES-256-GCM，`base64(nonce‖ct‖tag)`，AAD =
    /// `t_wecom_webhook.url.v1`）。
    ///
    /// ⚠️ **绝不进任何出参**；本结构只在本域 service / repo 与出参的掩码计算之间流转。
    pub encrypted_url: String,
    pub is_default: bool,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// 手写 `Debug`：屏蔽 `encrypted_url`（2026-10-18，与 `TMailAccount` 同款）。
///
/// ⚠️ 字段顺序与声明顺序一致，便于和 derive 版对照阅读。
impl std::fmt::Debug for TWeComWebhookRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TWeComWebhookRow")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("name", &self.name)
            // ⚠️ 永远输出 `***`
            .field("encrypted_url", &"***")
            .field("is_default", &self.is_default)
            .field("created_at", &self.created_at)
            .field("created_by", &self.created_by)
            .field("updated_at", &self.updated_at)
            .field("updated_by", &self.updated_by)
            .field("deleted_at", &self.deleted_at)
            .finish()
    }
}
