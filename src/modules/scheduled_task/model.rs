//! scheduled_task 域表行模型（`T*` = 域 owned 表的**全列** `FromRow` 投影）
//!
//! 仓内惯例：`pub struct T*` 全部 derive `sqlx::FromRow`、全列投影、零例外。
//! 本域 owned 的表只有一张 [`TMailAccount`] ← `t_mail_account`（邮箱实体）。
//!
//! ## `encrypted_secret` 为什么在**模型**里而不是只在 repo 的私有 row 里
//!
//! 因为**全列投影就是这个字段存在的唯一理由**：它必须被完整读出来，否则
//! 「不覆写时沿用旧密文」这条路径无从实现（`update_account` 拿到的是新密文，
//! 要沿用旧的就得读回旧的）。
//!
//! 出参层的屏蔽**不在这里**做，而是在 [`crate::modules::scheduled_task::dto`]
//! 的 `MailAccountOut::from_row` —— VO 是手写的、没有 `#[serde(skip)]` 漏网的可能，
//! 且 `docs/api/mail.md` §2 的字段表与那个结构逐字对应。**任何新的出参都必须
//! 显式挑字段**，这是「密文不回显」这条硬约束的结构性保证。

use chrono::NaiveDateTime;

/// `t_mail_account` 全列投影（邮箱实体；软删 + OCC）。
///
/// ⚠️ **刻意不 derive `Debug`，手写一个屏蔽 `encrypted_secret`**：derive 版会让任何一处
/// `{row:?}` / `dbg!(&row)` 把密文原文写进日志，而本仓对含凭据的类型一律走手写
/// `Debug`（`infra::config::MailAccountSecret` / `WeComConfig`、`mail_digest::mailer
/// ::ResolvedSender` 同一手法）—— 不靠「调用方记得别打印」这条约定。
#[derive(Clone, sqlx::FromRow)]
pub struct TMailAccount {
    pub id: i64,
    pub version: i32,
    /// 自定义名称（管理端展示 + **发件人显示名**的来源，见 `mail_digest::mailer::ResolvedSender`）。
    pub name: String,
    /// `varchar(30)`：`NET_EASE_163` / `TENCENT_QQ`。
    ///
    /// ⚠️ **刻意存字符串不存 host/port**：host/port/tls_mode 由
    /// [`crate::modules::scheduled_task::dto::MailAccountPreset`] 推导。存了列就有
    /// 「两个预设同一台主机但端口被改成 25」这种组合，而 SMTP 配错的现场表现是
    /// 一个来自网络层的握手异常 —— 比「预设选错了」难查一个数量级。
    pub preset: String,
    /// SMTP 登录账号（163/QQ 都是完整邮箱地址）；同时是发件人地址的来源。
    pub account: String,
    /// 授权码密文（AES-256-GCM，`base64(nonce‖ct‖tag)`）。
    ///
    /// ⚠️ **绝不进任何出参**；本结构只在本域 service / repo 与
    /// `mail_digest` 的发信 resolver 之间流转。
    pub encrypted_secret: String,
    pub is_default: bool,
    pub created_at: NaiveDateTime,
    pub created_by: Option<i64>,
    pub updated_at: NaiveDateTime,
    pub updated_by: Option<i64>,
    pub deleted_at: Option<NaiveDateTime>,
}

/// 手写 `Debug`：屏蔽 `encrypted_secret`（2026-10-16，与 `MailAccountSecret` 同款）。
///
/// ⚠️ `encrypted_secret` 的值是 `base64(nonce‖ct‖tag)`，它**可被 base64 解码** ⇒ 打进
/// 日志等于泄漏授权码。字段顺序与声明顺序一致，便于和 derive 版对照阅读。
impl std::fmt::Debug for TMailAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TMailAccount")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("name", &self.name)
            .field("preset", &self.preset)
            .field("account", &self.account)
            // ⚠️ 永远输出 `***`
            .field("encrypted_secret", &"***")
            .field("is_default", &self.is_default)
            .field("created_at", &self.created_at)
            .field("created_by", &self.created_by)
            .field("updated_at", &self.updated_at)
            .field("updated_by", &self.updated_by)
            .field("deleted_at", &self.deleted_at)
            .finish()
    }
}
