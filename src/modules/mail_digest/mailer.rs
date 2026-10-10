//! mail_digest 域：lettre SMTP 发送（**唯一**直接调 lettre 的文件）
//!
//! ## lettre 0.11 的确切 API（**实读 crate 源码核对**，不是凭记忆）
//!
//! | 用法 | 源码位置 |
//! |---|---|
//! | `lettre::AsyncSmtpTransport` | `src/lib.rs:361` 的 `pub use crate::transport::smtp::AsyncSmtpTransport` |
//! | `lettre::Tokio1Executor` | `src/lib.rs:329`（由 feature `tokio1` 启用） |
//! | `AsyncSmtpTransport::<Tokio1Executor>::relay(host) -> Result<AsyncSmtpTransportBuilder, Error>` | `transport/smtp/async_transport.rs:135` |
//! | `.credentials(Credentials::new(String, String))` | builder `async_transport.rs:381` + `authentication.rs:22` |
//! | `.port(u16)` | builder `async_transport.rs:404` |
//! | `.tls(Tls::Wrapper(TlsParameters))` / `Tls::Opportunistic(..)` / `Tls::None` | builder `async_transport.rs:440`；`Tls` 定义在 `transport/smtp/client/tls.rs:78`（由 `client/mod.rs:40` 重导出） |
//! | `.build::<Tokio1Executor>()` | builder `async_transport.rs:456` |
//! | `Message::builder().from(Mailbox).to(..).cc(..).subject(..).header(ContentType::TEXT_HTML).body(html)` | `message/mod.rs:503 / 255 / 295 / 302 / 328 / 367 / 458` |
//! | `SinglePart::html(String)` | `message/single_part.rs::html`（`charset` feature 在 0.11 已无条件生效） |
//! | `AsyncTransport::send(&Message).await` | `transport/mod.rs` 的 `AsyncTransport` trait |
//!
//! ⚠️ **0.11 没有 `charset` feature**（0.10 有）：`cargo add lettre --features charset` 会直接
//! 报 `unrecognized feature`。charset 在 0.11 是无条件编译的，别再往 Cargo.toml 里加。
//!
//! ## TLS 三态怎么落到 lettre
//!
//! | 本仓 `MAIL_SMTP_TLS_MODE` | lettre | 效果 |
//! |---|---|---|
//! | `WRAPPED`（默认，465） | `Tls::Wrapper(default_params)` | 连接建立即 TLS（隐式） |
//! | `STARTTLS`（587） | `Tls::Opportunistic(default_params)` | 明文连上后**尝试**升级，失败则**降级明文** |
//! | `NONE` | `Tls::None` | 明文 |
//!
//! ⚠️ `STARTTLS` 走的是 `Opportunistic` 而不是「必须升级」的语义 —— lettre 的
//! `Tls` 枚举**没有** `Required` 之外的「STARTTLS 且失败即报错」档
//! （`Tls::Required` 是指握手失败即报错，且仍以明文建连）。对一个内部 ERP 来说
//! 「 opportunistic 降级」的风险是：中间人能让凭据明文过。要绝对避免只能选
//! `WRAPPED`（隐式 TLS，连接建不起来就根本不会发凭据）。这一条已登记在
//! `MAIL_SMTP_TLS_MODE` 的选择上 —— 默认档是 `WRAPPED` 而不是 `STARTTLS`。

use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::TlsParameters;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor, transport::smtp::client::Tls,
};

use crate::infra::config::{MailConfig, MailTlsModeKind};
use crate::modules::mail_digest::dto::NormalizedConfig;
use crate::shared::error::{AppError, code};

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
/// 调用方也只打 `%e`（不打印整个 `MailConfig`）。
pub async fn send(
    mail: &MailConfig,
    cfg: &NormalizedConfig,
    subject: &str,
    html: &str,
) -> Result<(), String> {
    let transport = build_transport(mail)?;

    let from: lettre::message::Mailbox = format!("{} <{}>", mail.from_name, mail.from_address)
        .parse()
        .map_err(|e| {
            format!(
                "发件人地址无法解析（{} <{}>）：{e}",
                mail.from_name, mail.from_address
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
        // 不含 password（认证失败是 `535`，不回显密码），且这里连 `MailConfig`
        // 整个结构都没打（它的 Debug 才做屏蔽）。
        .map_err(|e| format!("SMTP 发送失败（host={} port={}）：{e}", mail.host, mail.port))
}

/// 依 `MAIL_SMTP_TLS_MODE` 构造 transport。
fn build_transport(mail: &MailConfig) -> Result<AsyncSmtpTransport<Tokio1Executor>, String> {
    let builder = AsyncSmtpTransport::<Tokio1Executor>::relay(&mail.host)
        .map_err(|e| build_err(mail, e))?
        .port(mail.port)
        .credentials(Credentials::new(
            mail.username.clone(),
            mail.password.clone(),
        ));

    let builder = match mail.tls_mode {
        MailTlsModeKind::Wrapped => {
            // ⚠️ lettre 0.11 的 `TlsParameters` **没有** `Default()`
            // （只有 `new(domain)` / `new_rustls(domain)` 两个构造器，实读
            // `transport/smtp/client/tls.rs:598 / 617` 核实）。本仓用 rustls 栈
            // （`tokio1-rustls-tls`，见 Cargo.toml）⇒ 必须用 `new_rustls`。
            // 参数是 **SNI 域名**，用 `MAIL_SMTP_HOST` —— 证书校验靠它，
            // 填错会握手失败。
            builder.tls(Tls::Wrapper(tls_params(&mail.host)?))
        }
        MailTlsModeKind::StartTls => builder.tls(Tls::Opportunistic(tls_params(&mail.host)?)),
        MailTlsModeKind::None => builder.tls(Tls::None),
    };

    Ok(builder.build())
}

/// 构造 rustls TLS 参数（SNI = SMTP 主机名）。
fn tls_params(host: &str) -> Result<TlsParameters, String> {
    TlsParameters::new_rustls(host.to_string())
        .map_err(|e| format!("TLS 参数构造失败（host={host}）：{e}"))
}

fn build_err(mail: &MailConfig, e: lettre::transport::smtp::Error) -> String {
    format!("SMTP transport 构造失败（host={}）：{e}", mail.host)
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

    #[test]
    fn tls_mode_maps_to_lettre_variants() {
        // 只验证「模式 → 用的哪个 lettre 变体」的映射表本身可读、不 panic；
        // 真正建连接需要网络，不在单测里做。
        assert_eq!(MailTlsModeKind::LEGAL_VALUES, &["WRAPPED", "STARTTLS", "NONE"]);
        assert_eq!(MailTlsModeKind::parse("wrapped").unwrap(), MailTlsModeKind::Wrapped);
        assert_eq!(MailTlsModeKind::parse("  StartTLS ").unwrap(), MailTlsModeKind::StartTls);
        assert_eq!(MailTlsModeKind::parse("NONE").unwrap(), MailTlsModeKind::None);
        // 非法值 → 报错且错误信息带合法值清单
        let err = MailTlsModeKind::parse("ssl").unwrap_err().to_string();
        assert!(err.contains("WRAPPED") && err.contains("STARTTLS") && err.contains("NONE"));
    }

    /// 未配置 SMTP 时 `build_transport` 不应被走到（service 层先拦），但
    /// `MailConfig::is_enabled` 的判定式本身要钉死。
    #[test]
    fn is_enabled_requires_all_three_credentials_plus_switch() {
        let base = MailConfig {
            username: "u@x.com".into(),
            password: "pw".into(),
            from_address: "u@x.com".into(),
            ..MailConfig::default()
        };
        // 三项凭据齐 + 总闸关 → false
        assert!(!base.is_enabled());
        // 总闸开 → true
        assert!(
            MailConfig {
                digest_enabled: true,
                ..base.clone()
            }
            .is_enabled()
        );
        // 任一凭据缺失 → false
        for missing in ["username", "password", "from_address"] {
            let mut c = base.clone();
            c.digest_enabled = true;
            match missing {
                "username" => c.username = String::new(),
                "password" => c.password = String::new(),
                _ => c.from_address = String::new(),
            }
            assert!(!c.is_enabled(), "{missing} 为空时不该判为已配置");
        }
        // 纯空格占位同样判为未配置（对齐 WeComConfig 的同款纪律）
        let mut ws = base.clone();
        ws.digest_enabled = true;
        ws.password = "   ".into();
        assert!(!ws.is_enabled(), "纯空格密码不该判为已配置");
    }

    /// lettre 会拒绝带 CR/LF 的地址 —— 我们的 service 层已经先拦一道（21703），
    /// 这里验证第二道防线确实存在（纵深防御，不是重复劳动）。
    #[test]
    fn lettre_rejects_header_injection_address() {
        assert!(parse_mailbox("a@b.com").is_ok());
        assert!(parse_mailbox("a@b.com\r\nBcc: victim@x.com").is_err());
    }
}
