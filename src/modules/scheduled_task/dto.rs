//! scheduled_task 域 DTO：SMTP 预设枚举 + 入参请求体 + 出参 VO
//!
//! ## 为什么没有单独的 vo.rs
//!
//! 出参 VO（`MailAccountOut` / `MailMetaOut`）也放在本文件：本域只有一张表、
//! 三个出参结构，入参更少。拆一个 `vo.rs` 只会让读代码的人多一次「该 import
//! 哪个路径」的判断，而那正是 CLAUDE.md「`docs/api/` 目录约定」第 2 条要避免的事。
//!
//! ## 密文绝不回显：为什么靠「出参是手写挑字段」而不是 `#[serde(skip)]`
//!
//! [`MailAccountOut`] **不 derive 任何自动字段映射**，字段逐个从行里挑。
//! 于是「不小心把 `encrypted_secret` 加进出参」不是一个「忘了加 skip」的错误，
//! 而是一个必须显式写出来才能发生的动作 —— 配合 `docs/api/mail.md` §2 的字段表，
//! 密文不回显这条硬约束就有了结构性的保证（`model.rs` 顶部有同一说明）。

use serde::{Deserialize, Serialize};

use crate::infra::config::MailTlsModeKind;
use crate::modules::scheduled_task::model::TMailAccount;
use crate::shared::error::{AppError, code};

// ============================================================================
// SMTP 预设枚举（本域 **唯一** 的 SMTP 参数来源）
// ============================================================================

/// 邮箱服务商预设。
///
/// ⚠️ **只做两家，且两家都要授权码而非登录密码**（管理员十有八九会填成密码 ——
/// 见每个变体的 doc）。主机名 / 端口 / TLS 模式由本枚举推导，**不存 DB**
/// （理由见 `model.rs` 的 `preset` 字段 doc）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailAccountPreset {
    /// 网易 163 邮箱：`smtp.163.com:465`，隐式 TLS。
    NetEase163,
    /// QQ 邮箱：`smtp.qq.com:465`，隐式 TLS。
    TencentQQ,
}

impl MailAccountPreset {
    /// 全量预设（`GET /meta` 的 `presets[]` 与写侧校验共用这一份）。
    ///
    /// ⚠️ **前端不要硬编码这份列表**：选服务商时 host/port/tls_mode 都要跟着变，
    /// 前端存一份就等于复制了三处会在「加一家服务商」时漂掉的常量。
    pub const ALL: &'static [MailAccountPreset] =
        &[MailAccountPreset::NetEase163, MailAccountPreset::TencentQQ];

    /// DB / JSON 的字面量（与 `migrations/20261016000000_001_scheduled_task.sql`
    /// 的 `preset` 列取值域逐字对齐）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NetEase163 => "NET_EASE_163",
            Self::TencentQQ => "TENCENT_QQ",
        }
    }

    /// 中文标签（前端直接渲染）。
    pub fn label(self) -> &'static str {
        match self {
            Self::NetEase163 => "网易 163",
            Self::TencentQQ => "QQ 邮箱",
        }
    }

    /// SMTP 服务器主机名（lettre 的 `relay(host)` 与 TLS 的 **SNI 域名**）。
    pub fn host(self) -> &'static str {
        match self {
            Self::NetEase163 => "smtp.163.com",
            Self::TencentQQ => "smtp.qq.com",
        }
    }

    /// SMTP 端口。两家都是 465 = 隐式 TLS。
    pub fn port(self) -> u16 {
        match self {
            Self::NetEase163 | Self::TencentQQ => 465,
        }
    }

    /// 映射到既有 TLS 模式枚举。两家都是 `Wrapped`。
    ///
    /// ⚠️ 465 = 隐式 TLS（连接建立即 TLS）。本域**不开放** 587 / STARTTLS：
    /// lettre 的 `Tls::Opportunistic` 在服务端不支持时会**降级明文继续发**
    /// （凭据可能裸传），而预设的语义是「选一个能用的服务商」，不该让管理员
    /// 去挑一个协议档位。详见 `mail_digest::mailer` 的 TLS 三态表。
    pub fn tls_mode(self) -> MailTlsModeKind {
        match self {
            Self::NetEase163 | Self::TencentQQ => MailTlsModeKind::Wrapped,
        }
    }

    /// 由字面量解析；失败返 `21704` 并列出合法值。
    pub fn parse(raw: &str) -> Result<Self, AppError> {
        let trimmed = raw.trim();
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.as_str() == trimmed)
            .ok_or_else(|| {
                let legal: Vec<&str> = Self::ALL.iter().map(|p| p.as_str()).collect();
                AppError::biz(
                    code::BIZ_MAIL_CONFIG_INVALID,
                    format!(
                        "SMTP 预设取值非法：{trimmed}；合法值只有 {}",
                        legal.join(" / ")
                    ),
                )
            })
    }
}

// ============================================================================
// 入参请求体
// ============================================================================

/// 授权码长度上限（`encrypted_secret` 是 `text`，但无限长等于给 DB 塞垃圾）。
///
/// 163 / QQ 的授权码都是 16 位字母数字，200 已是极大裕度。
pub const SECRET_MAX_CHARS: usize = 200;

/// `POST /api/v2/cron/mail/accounts` 请求体。
///
/// ⚠️ **无 `version`**：这是新建，没有「别人先改过」这回事（CLAUDE.md §8 的 OCC
/// 只适用于已存在的行）。
///
/// ⚠️ **刻意不 derive `Debug`**：`secret` 是**明文**授权码（比 repo 两个结构体手上的密文
/// 更危险 —— 密文还要 base64 解一道），而本仓对含凭据的类型一律走「不实现 `Debug`」或
/// 「手写屏蔽版」（`infra::config::MailAccountSecret` / `WeComConfig`、
/// `mail_digest::mailer::ResolvedSender`、`repo::NewAccount` / `AccountPatch`、
/// `model::TMailAccount` 同一手法）—— 不靠「调用方记得别打印」这条约定。
/// 守卫：`tests::create_and_update_request_must_not_expose_plaintext_secret`.
#[derive(Clone, Deserialize)]
pub struct CreateAccountRequest {
    /// 自定义名称（同时是发件人显示名）。≤50 字（与列 `varchar(50)` 对齐）。
    pub name: String,
    /// 预设字面量，见 [`MailAccountPreset::ALL`]。
    pub preset: String,
    /// SMTP 登录账号（163/QQ 都是完整邮箱地址）。≤120 字。
    pub account: String,
    /// 授权码（非登录密码）。**必填非空**；解密要用它，不存在「没配也能建」。
    pub secret: String,
}

/// `POST /api/v2/cron/mail/accounts/{id}` 请求体：**全量覆写**。
///
/// ⚠️ `version` **必填、无 `#[serde(default)]`**（CLAUDE.md §8 硬约定）：
/// 缺失时 axum `Json` 提取器直接返 **HTTP 422 纯文本**，不是业务信封。
///
/// ⚠️ **同样刻意不 derive `Debug`**：`secret` 是**明文**授权码，与
/// [`CreateAccountRequest`] 同一理由（不靠「调用方记得别打印」这条约定）。
/// 守卫：`tests::create_and_update_request_must_not_expose_plaintext_secret`.
#[derive(Clone, Deserialize)]
pub struct UpdateAccountRequest {
    /// 乐观锁锚点。0 行命中 → `40901 VERSION_CONFLICT`。
    pub version: i32,
    pub name: String,
    pub preset: String,
    pub account: String,
    /// 授权码。**缺省或 trim 后为空 ⇒ 沿用旧密文**（不覆写、不报错；判定在 trim
    /// 之后做，故 `"   "` 也算「没填」，与新建路径「纯空白直接 21704」刻意不同 ——
    /// 理由见 `docs/api/mail.md` §2.4）。
    ///
    /// 之所以不是「必填」：管理员改个名称时前端拿不到授权码明文（服务端从不回显），
    /// 强制必填等于逼前端每次都把「不知道的东西」原样传回来。空串是前端唯一能
    /// 表达「我没改它」的方式。
    #[serde(default)]
    pub secret: Option<String>,
}

/// `POST /api/v2/cron/mail/accounts/{id}/delete` 请求体。
///
/// ⚠️ **只有 `version`**：本表带 `version` 列，故按 CLAUDE.md §8「所有对带
/// `version` 列的表的写端点，body 必须显式收客户端 `version` 且必填」，
/// 软删也不例外（这正是「读-再-比式隐式 OCC」被明令禁止的场景：两人同时打开
/// 同一个编辑页，一人改了名、一人删了行，后者会静默成功）。
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteAccountRequest {
    pub version: i32,
}

/// `POST /api/v2/cron/mail/accounts/{id}/set-default` 请求体。
///
/// 与 [`DeleteAccountRequest`] 同款：目标行的 `version` 必填。
#[derive(Debug, Clone, Deserialize)]
pub struct SetDefaultAccountRequest {
    pub version: i32,
}

// ============================================================================
// 出参
// ============================================================================

/// `GET /meta` 的一个预设选项。
///
/// ⚠️ **不含任何凭据**：host/port/tls_mode 是让管理员判断「选这家能不能连上」
/// 的公开信息，授权码一个字都不在这里。
#[derive(Debug, Clone, Serialize)]
pub struct PresetOption {
    pub value: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    /// `WRAPPED` / `STARTTLS` / `NONE`（`MailTlsModeKind` 的 Display 字面量）。
    pub tls_mode: String,
}

/// `GET /api/v2/cron/mail/meta` 响应体。
#[derive(Debug, Clone, Serialize)]
pub struct MailMetaOut {
    pub presets: Vec<PresetOption>,
}

/// `GET /api/v2/cron/mail/accounts` 的一个邮箱。
///
/// ⚠️ **没有 `encrypted_secret`、没有掩码字段、没有 `secret_configured`**：
/// 建邮箱时授权码必填 ⇒ 「是否配了授权码」恒为 true，是个死字段；而死字段会让人
/// 误以为「授权码可以是空的」，进而写出「先把邮箱建好、授权码以后再补」的流程。
/// 密文与掩码都不回显 —— 掩码回显（「abc***def」）同样是一条不需要存在的泄漏面。
#[derive(Debug, Clone, Serialize)]
pub struct MailAccountOut {
    #[serde(serialize_with = "crate::shared::types::serialize_i64")]
    pub id: i64,
    pub version: i32,
    /// 自定义名称（也是发件人显示名）。
    pub name: String,
    /// 预设字面量。
    pub preset: String,
    /// 预设中文标签（前端不必另存映射）。
    pub preset_label: String,
    /// SMTP 登录账号 = 发件人地址。
    pub account: String,
    /// 由预设推导（不存 DB）。
    pub host: String,
    /// 由预设推导（不存 DB）。
    pub port: u16,
    /// 由预设推导（不存 DB），字面量同 `MailTlsModeKind` 的 Display。
    pub tls_mode: String,
    pub is_default: bool,
    /// naive timestamp（Asia/Shanghai），格式 `YYYY-MM-DD HH:MM:SS`。
    pub created_at: String,
    pub updated_at: String,
}

impl MailAccountOut {
    /// 行 → 出参。**逐字段挑**，是「密文不回显」的执行点（见文件头 doc）。
    ///
    /// 遇到库里存了未知 `preset` 字面量（手工改库 / 将来加了预设但没同步）时
    /// **降级为该行自身的字面量 + 空 host/port**，不 panic 也不丢行 ——
    /// 列表页整体 500 会让人以为「邮箱全没了」，而实际上只是某个值不认识。
    pub fn from_row(row: &TMailAccount) -> Self {
        let preset = MailAccountPreset::parse(&row.preset).ok();
        Self {
            id: row.id,
            version: row.version,
            name: row.name.clone(),
            preset: row.preset.clone(),
            preset_label: preset.map(|p| p.label().to_string()).unwrap_or_default(),
            account: row.account.clone(),
            host: preset.map(|p| p.host().to_string()).unwrap_or_default(),
            port: preset.map(|p| p.port()).unwrap_or_default(),
            tls_mode: preset.map(|p| p.tls_mode().to_string()).unwrap_or_default(),
            is_default: row.is_default,
            created_at: format_ts(row.created_at),
            updated_at: format_ts(row.updated_at),
        }
    }
}

/// `GET /api/v2/cron/mail/accounts` 响应体。
#[derive(Debug, Clone, Serialize)]
pub struct MailAccountListOut {
    pub items: Vec<MailAccountOut>,
}

fn format_ts(ts: chrono::NaiveDateTime) -> String {
    ts.format("%Y-%m-%d %H:%M:%S").to_string()
}
