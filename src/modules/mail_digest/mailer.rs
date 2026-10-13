//! mail_digest 域：lettre SMTP 发送（**唯一**直接调 lettre 的文件）
//!
//! ## 数据来源：2026-10-16 起是 `scheduled_task` 域的邮箱表
//!
//! 本文件的入参 [`ResolvedSender`] **不再来自 env**（`MAIL_SMTP_*` 已随
//! 「邮箱改为页面内配置」整轮删除）。它由
//! `mail_digest::service::resolve_sender` 按 `t_mail_digest_config.sender_account_id`
//! 查 `t_mail_account`、用 `infra::crypto` 解密授权码后组装而成。
//!
//! **`build_transport` / `tls_params` / `parse_mailbox` 的内部逻辑逐字未改** ——
//! 本轮只换了「数据从哪里来」，没有动「怎么发」。
//!
//! ## lettre 0.11 的确切 API（**实读 crate 源码核对**，不是凭记忆）
//!
//! ⚠️ 本表**按符号名 + crate 版本引用，不写源码行号**（2026-10-13 review 第 1 轮
//! m11）——行号随 patch 升级即漂，而本仓的 `Cargo.lock` 会随其它依赖一起升级，
//! 一旦漂了就没人知道这些结论是在哪个版本上核对的。
//!
//! | 用法 | lettre 0.11 中的符号 / 位置 |
//! |---|---|
//! | `lettre::AsyncSmtpTransport` | crate 根的重导出（源自 `transport::smtp`） |
//! | `lettre::Tokio1Executor` | crate 根的重导出（由 feature `tokio1` 启用） |
//! | `AsyncSmtpTransport::<Tokio1Executor>::relay(host) -> Result<AsyncSmtpTransportBuilder, Error>` | `transport::smtp::AsyncSmtpTransport::relay` |
//! | `.credentials(Credentials::new(String, String))` | builder 的 `AsyncSmtpTransportBuilder::credentials` + `transport::smtp::authentication::Credentials::new` |
//! | `.port(u16)` | builder 的 `AsyncSmtpTransportBuilder::port` |
//! | `.tls(Tls::Wrapper(TlsParameters))` / `Tls::Opportunistic(..)` / `Tls::None` | builder 的 `AsyncSmtpTransportBuilder::tls`；`Tls` 定义在 `transport::smtp::client::tls` |
//! | `.build::<Tokio1Executor>()` | `AsyncSmtpTransportBuilder::build` |
//! | `Message::builder().from(Mailbox).to(..).cc(..).subject(..).header(ContentType::TEXT_HTML).body(html)` | `message::MessageBuilder` 的 `builder` / `from` / `to` / `cc` / `subject` / `header` / `body` |
//! | `SinglePart::html(String)` | `lettre::message::SinglePart::html`（类型定义在 crate 的 `message::mimebody`，经 `message` 模块重导出；`charset` feature 在 0.11 已无条件生效）。⚠️ **没有 `message::single_part` 这个模块** —— 本行原先写的就是那个不存在的路径（review 第 2 轮 #8） |
//! | `AsyncTransport::send(&Message).await` | `transport::AsyncTransport` trait |
//!
//! ⚠️ **0.11 没有 `charset` feature**（0.10 有）：`cargo add lettre --features charset` 会直接
//! 报 `unrecognized feature`。charset 在 0.11 是无条件编译的，别再往 Cargo.toml 里加。
//!
//! ## TLS 三态怎么落到 lettre
//!
//! | [`MailTlsModeKind`] | lettre | 效果 | 本轮是否有生产方 |
//! |---|---|---|---|
//! | `WRAPPED` | `Tls::Wrapper(default_params)` | 连接建立即 TLS（隐式） | **有** —— 两个预设都是它（465） |
//! | `STARTTLS` | `Tls::Opportunistic(default_params)` | 明文连上后**尝试**升级，失败则**降级明文** | 无（预设不开放 587） |
//! | `NONE` | `Tls::None` | 明文 | 无 |
//!
//! ⚠️ `STARTTLS` 走的是 `Opportunistic` 而不是「必须升级」的语义 —— lettre 的 `Tls`
//! 枚举**没有** `Required` 之外的「STARTTLS 且失败即报错」档（`Tls::Required` 是指
//! 握手失败即报错，且仍以明文建连）。对一个内部 ERP 来说「opportunistic 降级」的
//! 风险是：中间人能让凭据明文过。要绝对避免只能选 `WRAPPED` —— 这也正是
//! `scheduled_task::dto::MailAccountPreset` 两家都固定 465 的原因。

use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::TlsParameters;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor, transport::smtp::client::Tls,
};

use crate::infra::config::MailTlsModeKind;
use crate::modules::mail_digest::dto::NormalizedConfig;
use crate::shared::error::{AppError, code};

/// 已解析的发信通道（**明文授权码只活在这个结构里**）。
///
/// 由 `mail_digest::service::resolve_sender` 从 `t_mail_account` + 解密结果组装。
/// 本结构**刻意不 derive `Debug`**：它含明文授权码，而本仓对含凭据的类型一律手写
/// `Debug` 屏蔽（`infra::config::WeComConfig` / `MailAccountSecret` 同一手法）。
/// 需要打日志时只打 `host` / `port` / `from_address` 三个非敏感字段。
#[derive(Clone)]
pub struct ResolvedSender {
    pub host: String,
    pub port: u16,
    pub tls_mode: MailTlsModeKind,
    /// SMTP 登录账号（= `t_mail_account.account`）。
    pub username: String,
    /// SMTP 授权码明文（`crypto::open` 的结果）。
    pub password: String,
    /// 发件人地址。
    ///
    /// ⚠️ **缺省回退到登录账号**（163 / QQ 的登录账号本身就是可发信地址）。
    /// `MAIL_FROM_ADDRESS` 那一对 env 随本轮改造删除，而「显式发件地址与登录账号
    /// 不同」的场景企业邮箱并不支持（SMTP 会拒 From 与认证主体不一致的信），
    /// 故不再提供这个自由度。
    pub from_address: String,
    /// 发件人显示名（= `t_mail_account.name`，即管理员给这个邮箱起的自定义名）。
    ///
    /// ⚠️ 旧 env `MAIL_FROM_NAME` 缺省是硬编码的「汇森 ERP」；改成「取管理员填的
    /// 名称」之后，管理员可以把发件人显示成「汇森告警邮箱」「财务日报」这类名字 ——
    /// 收件人一眼能看出这封信是谁发的，比一个全局常量有用。
    pub from_name: String,
}

/// 手写 `Debug`：屏蔽 `password`（不靠「调用方记得别打印」这条约定）。
impl std::fmt::Debug for ResolvedSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSender")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls_mode", &self.tls_mode)
            .field("username", &self.username)
            // ⚠️ 永远输出 `***`
            .field("password", &"***")
            .field("from_address", &self.from_address)
            .field("from_name", &self.from_name)
            .finish()
    }
}

/// 用 lettre 发一封 HTML 邮件。**失败一律返回 `Err(原因文本)`**，由调用方
/// (`service::deliver`) 包成 `21702 BIZ_MAIL_SEND_FAILED`（HTTP 502 —— 见
/// `status_from_code` 的注释：SMTP 是外部依赖，它挂了，本服务的入参校验其实全过了）。
///
/// ⚠️ **为什么返回 `String` 而不是 `AppError`**：同一段失败文本有两个去处 ——
/// `t_mail_digest_log.error_message`（审计留痕）与 HTTP 响应体（`message` 字段）。
/// 若在这里就包成 `AppError`，落库时拿到的是 `Display`（带 `[21702] ` 前缀），
/// 于是流水里会存下「[21702] [21702] …」这种重复前缀的文本。返回裸文本后，
/// 落库存裸文本、HTTP 层再加码，各得其所。
///
/// ⚠️ 文本里**不含任何凭据**：lettre 的错误只报连接/握手/应答，绝不回显密码；
/// 调用方也只打 `%e`（不打印整个 `ResolvedSender`）。
pub async fn send(
    sender: &ResolvedSender,
    cfg: &NormalizedConfig,
    subject: &str,
    html: &str,
) -> Result<(), String> {
    let transport = build_transport(sender)?;

    let from: lettre::message::Mailbox = format!("{} <{}>", sender.from_name, sender.from_address)
        .parse()
        .map_err(|e| {
            format!(
                "发件人地址无法解析（{} <{}>）：{e}",
                sender.from_name, sender.from_address
            )
        })?;

    let mut builder = Message::builder()
        .from(from)
        .subject(subject.to_string())
        .header(ContentType::TEXT_HTML);
    for to in &cfg.recipients {
        builder = builder.to(parse_mailbox(to).map_err(|e| e.to_string())?);
    }
    for cc in &cfg.cc_recipients {
        builder = builder.cc(parse_mailbox(cc).map_err(|e| e.to_string())?);
    }
    let message = builder
        .body(html.to_string())
        .map_err(|e| format!("构造邮件正文失败：{e}"))?;

    transport
        .send(message)
        .await
        .map(|_| ())
        // ⚠️ 只打 SMTP 服务器 / 端口 / 错误种类，**不打凭据**：lettre 的错误文本
        // 不含 password（认证失败是 `535`，不回显密码），且这里连
        // `ResolvedSender` 整个结构都没打（它的 Debug 才做屏蔽）。
        .map_err(|e| {
            format!(
                "SMTP 发送失败（host={} port={}）：{e}",
                sender.host, sender.port
            )
        })
}

/// 依 [`ResolvedSender::tls_mode`] 构造 transport。
fn build_transport(sender: &ResolvedSender) -> Result<AsyncSmtpTransport<Tokio1Executor>, String> {
    let builder = AsyncSmtpTransport::<Tokio1Executor>::relay(&sender.host)
        .map_err(|e| build_err(sender, e))?
        .port(sender.port)
        .credentials(Credentials::new(
            sender.username.clone(),
            sender.password.clone(),
        ));

    let builder = match sender.tls_mode {
        MailTlsModeKind::Wrapped => {
            // ⚠️ lettre 0.11 的 `TlsParameters` **没有** `Default()`
            // （只有 `new(domain)` / `new_rustls(domain)` 两个构造器，实读
            // `transport::smtp::client::tls` 的 `TlsParameters` 核实）。本仓用 rustls 栈
            // （`tokio1-rustls-tls`，见 Cargo.toml）⇒ 必须用 `new_rustls`。
            // 参数是 **SNI 域名**，用 `sender.host` —— 证书校验靠它，填错会握手失败。
            builder.tls(Tls::Wrapper(tls_params(&sender.host)?))
        }
        MailTlsModeKind::StartTls => builder.tls(Tls::Opportunistic(tls_params(&sender.host)?)),
        MailTlsModeKind::None => builder.tls(Tls::None),
    };

    Ok(builder.build())
}

/// 构造 rustls TLS 参数（SNI = SMTP 主机名）。
fn tls_params(host: &str) -> Result<TlsParameters, String> {
    TlsParameters::new_rustls(host.to_string())
        .map_err(|e| format!("TLS 参数构造失败（host={host}）：{e}"))
}

fn build_err(sender: &ResolvedSender, e: lettre::transport::smtp::Error) -> String {
    format!("SMTP transport 构造失败（host={}）：{e}", sender.host)
}

fn parse_mailbox(addr: &str) -> Result<lettre::message::Mailbox, AppError> {
    addr.parse().map_err(|e| {
        AppError::biz(
            code::BIZ_MAIL_RECIPIENT_INVALID,
            format!("lettre 拒绝了收件人地址 `{addr}`：{e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender(tls_mode: MailTlsModeKind) -> ResolvedSender {
        ResolvedSender {
            host: "127.0.0.1".to_string(),
            port: 1,
            tls_mode,
            username: "sender@example.com".to_string(),
            password: "dummy-secret".to_string(),
            from_address: "sender@example.com".to_string(),
            from_name: "测试发件人".to_string(),
        }
    }

    /// 三个 `MailTlsModeKind` 变体的对外字面量必须逐字不变。
    ///
    /// ⚠️ 2026-10-16 起 `MailTlsModeKind::parse`（env 字符串解析）已随
    /// `MAIL_SMTP_TLS_MODE` 整轮删除，所以这条用例守的是 `Display` 而不是
    /// `parse`：字面量仍会经 `GET /api/v2/mail/meta` 的 `tls_mode` 字段出参。
    #[test]
    fn tls_mode_literals_are_pinned() {
        assert_eq!(MailTlsModeKind::Wrapped.to_string(), "WRAPPED");
        assert_eq!(MailTlsModeKind::StartTls.to_string(), "STARTTLS");
        assert_eq!(MailTlsModeKind::None.to_string(), "NONE");
    }

    /// `build_transport` 对三条 TLS 路径都能构造出 transport（**不建连接**）。
    ///
    /// 覆盖的是「模式 → lettre 变体」的映射表本身可读、不 panic；真连 SMTP 需要
    /// 网络，不在单测里做。
    #[test]
    fn build_transport_maps_every_tls_mode() {
        for mode in [
            MailTlsModeKind::Wrapped,
            MailTlsModeKind::StartTls,
            MailTlsModeKind::None,
        ] {
            assert!(
                build_transport(&sender(mode)).is_ok(),
                "{mode:?} 应能构造出 transport"
            );
        }
    }

    /// `ResolvedSender` 的手写 `Debug` 必须屏蔽明文授权码。
    ///
    /// 这是「不靠约定靠类型」的回归闸：有人把本结构改回 `#[derive(Debug)]`
    /// 或删掉手写实现时，本测试立刻红。
    #[test]
    fn resolved_sender_debug_masks_password() {
        let dbg = format!("{:?}", sender(MailTlsModeKind::Wrapped));
        assert!(!dbg.contains("dummy-secret"), "Debug 泄漏授权码明文：{dbg}");
        assert!(dbg.contains("***"), "授权码应显示为 ***: {dbg}");
        // 排障需要的非敏感字段仍应可见
        assert!(dbg.contains("127.0.0.1"), "host 应可见：{dbg}");
    }

    /// lettre 会拒绝带 CR/LF 的地址 —— 我们的 service 层已经先拦一道（21703），
    /// 这里验证第二道防线确实存在（纵深防御，不是重复劳动）。
    #[test]
    fn lettre_rejects_header_injection_address() {
        assert!(parse_mailbox("a@b.com").is_ok());
        assert!(parse_mailbox("a@b.com\r\nBcc: victim@x.com").is_err());
    }
}
